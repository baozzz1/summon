use serde::{Deserialize, Serialize};

/// 一条 @ 提及(sidecar 从 msg.mentions 抽取)。群消息门控用 open_id 比对本 bot。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Mention {
    #[serde(default)]
    pub open_id: String,
    #[serde(default)]
    pub key: String,
    #[serde(default)]
    pub name: String,
    /// 被 @ 的是人(`user`)还是机器人(`bot`),即飞书的 `mentioned_type`;事件没给则为空。
    #[serde(default)]
    pub kind: String,
}

/// 用户随消息发来的一个附件(图片/文件/语音/视频/富文本内嵌图)。sidecar 抽 file_key + 类型,
/// daemon 据此从飞书下载到本地、把路径喂给 agent 查看。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Resource {
    /// 飞书资源 key(image_key 或 file_key),下载用。
    #[serde(default)]
    pub file_key: String,
    /// 类型:image|file|audio|media。决定下载 type 参数与本地命名。
    #[serde(default)]
    pub kind: String,
    /// 原始文件名(file/media 有;image/audio 常为空)。
    #[serde(default)]
    pub name: String,
}

/// mentions 里是否 @ 了 me(open_id 比对;空 open_id 跳过)。纯函数,便于单测。
pub fn mentions_contain(mentions: &[Mention], me: &str) -> bool {
    !me.is_empty()
        && mentions
            .iter()
            .any(|m| !m.open_id.is_empty() && m.open_id == me)
}

/// 把正文里的 @ 占位 token(飞书的 `@_user_N`)渲染成可读形式 `@姓名`。
/// 人 / bot 统一只显示名字,不带 open_id —— agent 回 @ 由出站层 `render_outbound_mentions`
/// 查 peers/identity_cache 解析 open_id,prompt 正文里塞 raw id 既吵又没用(per-app 不稳定)。
/// 姓名缺则退化显示 open_id;两者皆空则删掉该占位。其余文本不动,最后 collapse 空格 + trim。
/// 纯函数,便于单测。
pub fn render_mentions(text: &str, mentions: &[Mention]) -> String {
    let mut out = text.to_string();
    for m in mentions {
        if m.key.is_empty() {
            continue;
        }
        let label = match (m.name.trim(), m.open_id.trim()) {
            ("", "") => String::new(),
            ("", oid) => format!("@{oid}"),
            (name, _) => format!("@{name}"),
        };
        out = out.replace(&m.key, &label);
    }
    collapse_spaces(&out)
}

/// 把连续多个空白(空格/tab/换行)压成单个空格,再 trim。
/// `strip_mention_by_open_id` 把占位替换成空串后会留双空格,用这个收干净。
pub fn collapse_spaces(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut prev_space = false;
    for c in text.chars() {
        if c.is_whitespace() {
            if !prev_space {
                out.push(' ');
                prev_space = true;
            }
        } else {
            out.push(c);
            prev_space = false;
        }
    }
    out.trim().to_string()
}

/// 判断指定 open_id 的 mention 是否出现在正文开头(去前导空白后)。
/// 用于位置感知剥离:开头的 @bot = 「对本 bot 说话」(应剥离,让斜杠命令等前缀解析正常);
/// 非开头的 @bot = 「提及本 bot」(保留为信息,agent 据此判断意图)。纯函数,便于单测。
pub fn is_leading_mention(text: &str, mentions: &[Mention], open_id: &str) -> bool {
    if open_id.is_empty() {
        return false;
    }
    let trimmed = text.trim_start();
    mentions
        .iter()
        .any(|m| !m.key.is_empty() && m.open_id == open_id && mention_key_starts(trimmed, &m.key))
}

pub(crate) fn mention_key_starts(text: &str, key: &str) -> bool {
    text.strip_prefix(key).is_some_and(|rest| {
        rest.chars()
            .next()
            .is_none_or(|next| !next.is_ascii_alphanumeric() && next != '_')
    })
}

/// 仅当指定 open_id 的 mention 出现在正文开头时才抹掉它(位置感知剥离)。
/// 返回 (处理后的正文, 是否真的剥离了)。其余 @ 保留给 `render_mentions` 渲染。
/// 剥离后自动 collapse 空格,不留双空格垃圾。
pub fn strip_leading_mention_by_open_id(
    text: &str,
    mentions: &[Mention],
    open_id: &str,
) -> (String, bool) {
    if open_id.is_empty() {
        return (collapse_spaces(text), false);
    }
    let trimmed = text.trim_start();
    let stripped = mentions
        .iter()
        .find(|m| !m.key.is_empty() && m.open_id == open_id && mention_key_starts(trimmed, &m.key))
        .map(|m| {
            // 只去掉开头那一个 mention key,保留后面可能的同名 mention
            let after_key = &trimmed[m.key.len()..];
            collapse_spaces(after_key)
        });
    match stripped {
        Some(s) => (s, true),
        None => (collapse_spaces(text), false),
    }
}

// 仅测试使用:生产路径未调用(2026-08-20 死代码盘点确认)。不进 release 二进制。
#[cfg(test)]
/// 抹掉指定 open_id 对应的全部 @ 占位 token(不区分位置),其余 @ 保留给 `render_mentions` 渲染。
/// 用于命令解析等不关心位置的场景;热路径上的消息正文请用 `strip_leading_mention_by_open_id`
/// 做位置感知剥离。剥离后自动 collapse 空格。
pub(crate) fn strip_mention_by_open_id(text: &str, mentions: &[Mention], open_id: &str) -> String {
    let mut s = text.to_string();
    if open_id.is_empty() {
        return collapse_spaces(&s);
    }
    for m in mentions {
        if !m.key.is_empty() && m.open_id == open_id {
            s = s.replace(&m.key, "");
        }
    }
    collapse_spaces(&s)
}

/// 抹掉所有 @ 占位 token(群里 @bot 等)并 collapse 空格后的纯正文。summon 命令识别与 provider
/// 斜杠候选分类都基于它，保证「群里 @bot /cmd」与私聊「/cmd」走同一判定。纯函数,便于单测。
pub fn strip_mentions_text(text: &str, mentions: &[Mention]) -> String {
    let mut s = text.to_string();
    for m in mentions {
        if !m.key.is_empty() {
            s = s.replace(&m.key, "");
        }
    }
    collapse_spaces(&s)
}

/// `/session` 的子命令。**为什么挂在 `/session` 下而不另开顶层命令**：summon 与 provider 共用
/// 斜杠命名空间；新增顶层词会永久挡住 provider 将来采用同名命令。挂子命令零新增顶层冲突，且与
/// 面板上的两个按钮一一对应。未知 provider 候选现在会安全拒绝，不再自动透传。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionCommand {
    /// `/session` —— 唤出会话控制面板。
    Panel,
    /// `/session clean` —— 归档续聊(= 面板「退出(归档续聊)」):留 resume 记录,下次接着聊。
    Clean,
    /// `/session reset` —— 彻底重置(= 面板「退出(唤醒时新建session)」):清 resume 记录,下次全新会话。
    /// 这也是 `/bridge` 被「该群已有可恢复会话」挡住时的解法。
    Reset,
    /// `/session quota [on|off|开|关]` —— 额度显示开关的文字入口(= 面板「额度显示」按钮)。
    /// `None` = 翻转当前态;`Some(bool)` = 显式指定。误触后果只是显示项翻转,可见且可逆,
    /// 与 clean/reset 的破坏性不同——但仍与面板按钮同一把 operate 闸(改持久状态 + 敏感面展示)。
    Quota(Option<bool>),
}

/// 解析 `/session [clean|reset]`。**未知子命令一律退回 `Panel`**——绝不能把打错的参数猜成破坏性
/// 动作(`/session rest` 不该清掉上下文);面板本身就带这两个按钮,退回面板既安全又是答案。
///
/// 判**首 token** 而非整体匹配:用户自然会写 `/session 看看`,整体匹配会把它漏给下游当普通消息,
/// bot 只能用自然语言瞎回(`/introduce` 就是这么炸的,见 `introduce_command`)。首 token 判定同样
/// 不会被正文里顺带提到的 `/session` 误触发。
pub fn session_command(text: &str, mentions: &[Mention]) -> Option<SessionCommand> {
    let stripped = strip_mentions_text(text, mentions);
    let mut tokens = stripped.split_whitespace();
    if !tokens.next()?.eq_ignore_ascii_case("/session") {
        return None;
    }
    Some(match tokens.next() {
        Some(sub) if sub.eq_ignore_ascii_case("reset") => SessionCommand::Reset,
        Some(sub) if sub.eq_ignore_ascii_case("clean") => SessionCommand::Clean,
        Some(sub) if sub.eq_ignore_ascii_case("quota") => {
            // 第三 token:on/off(兼容中文 开/关)显式指定;缺失或不认识 → 翻转当前态。
            // 「不认识就翻转」在这里是安全的:后果只是显示项可见地翻一下,与 reset 那条
            // 「打错不得猜成破坏动作」的约束不冲突。
            SessionCommand::Quota(tokens.next().and_then(|v| {
                if v.eq_ignore_ascii_case("on") || v == summon_i18n::source!("r39eae64cfc") {
                    Some(true)
                } else if v.eq_ignore_ascii_case("off") || v == summon_i18n::source!("r5d0ae622f6")
                {
                    Some(false)
                } else {
                    None
                }
            }))
        }
        _ => SessionCommand::Panel,
    })
}

/// 解析 `/quota [on|off|开|关]` —— `/session quota` 的短别名(owner 2026-08-19:
/// 「/session quota on/off 太长了」)。语义与 [`session_command`] 的 `Quota` 分支逐字节一致:
/// 外层 `None` = 不是本命令;`Some(None)` = 裸 `/quota` 翻转当前态;
/// `Some(Some(bool))` = 显式开/关;不认识的参数落回翻转(后果只是显示项可见地翻一下)。
///
/// 与挂在 `/session` 下的子命令不同,这是**顶层词**:会永久挡住 provider 将来采用同名
/// `/quota` 命令(设计约束见 `SessionCommand` 的注释)。owner 知情并指定要这个短词。
pub fn quota_command(text: &str, mentions: &[Mention]) -> Option<Option<bool>> {
    let stripped = strip_mentions_text(text, mentions);
    let mut tokens = stripped.split_whitespace();
    if !tokens.next()?.eq_ignore_ascii_case("/quota") {
        return None;
    }
    Some(tokens.next().and_then(|v| {
        if v.eq_ignore_ascii_case("on") || v == summon_i18n::source!("r39eae64cfc") {
            Some(true)
        } else if v.eq_ignore_ascii_case("off") || v == summon_i18n::source!("r5d0ae622f6") {
            Some(false)
        } else {
            None
        }
    }))
}

/// 解析 `/model <id>`。模型切换是 summon 的持久配置动作，不透传给无头 CLI：没有既有会话时
/// 透传只会把它当普通 prompt，既不能改下次启动参数，也可能由 CLI 直接报错。
/// 返回空串表示用户只发了 `/model`，调用方据此回用法提示。
pub fn model_command(text: &str, mentions: &[Mention]) -> Option<String> {
    let stripped = strip_mentions_text(text, mentions);
    let mut parts = stripped.splitn(2, char::is_whitespace);
    if !parts.next()?.eq_ignore_ascii_case("/model") {
        return None;
    }
    Some(parts.next().unwrap_or("").trim().to_string())
}

/// 解析 `/effort [参数]`。与 `/model` 同理由 summon 处理:本群档位要落进 summon 的本群配置,
/// 透传给 CLI 只会改它自己的会话(claude 还会顺带改写用户级默认)。返回参数原文(可能为空)。
pub fn effort_command(text: &str, mentions: &[Mention]) -> Option<String> {
    let stripped = strip_mentions_text(text, mentions);
    let mut parts = stripped.splitn(2, char::is_whitespace);
    if !parts.next()?.eq_ignore_ascii_case("/effort") {
        return None;
    }
    Some(parts.next().unwrap_or("").trim().to_string())
}

/// Codex / Cursor Fast 的文字控制命令。Fast 会改变额度消耗，故只接受显式 `on` / `off` 两态；
/// 裸命令、未知参数或多余参数都返回 [`FastCommand::Invalid`]，交给调用方回用法提示，绝不猜测。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FastCommand {
    On,
    Off,
    Invalid,
}

/// 解析群聊 `@Bot /fast on|off` 或私聊 `/fast on|off`。mention 由统一入口剥离，命令和参数
/// 大小写不敏感；只有首 token 精确等于 `/fast` 才命中，避免正文里顺带提及被误触发。
/// provider 自带 `/fast` 的 CLI 返回 None，整条交给 slash 透传。
pub fn fast_command(
    text: &str,
    mentions: &[Mention],
    cli: crate::driver::CliDriver,
) -> Option<FastCommand> {
    let stripped = strip_mentions_text(text, mentions);
    let mut tokens = stripped.split_whitespace();
    let head = tokens.next()?;
    if !head.eq_ignore_ascii_case("/fast") || summon_owned_command_head(head, cli).is_none() {
        return None;
    }
    Some(match (tokens.next(), tokens.next()) {
        (Some(mode), None) if mode.eq_ignore_ascii_case("on") => FastCommand::On,
        (Some(mode), None) if mode.eq_ignore_ascii_case("off") => FastCommand::Off,
        _ => FastCommand::Invalid,
    })
}

/// Cursor ACP 会话模式的文字控制命令。id 来自 server 动态 catalog；语法仍只接受一个显式
/// token，裸命令、非法 id 或多余参数都返回 Invalid，不能误透传给 agent 当普通 prompt。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModeCommand {
    Set(crate::driver::CursorSessionMode),
    Invalid,
}

/// 解析群聊 `@Bot /mode <id>` 或私聊 `/mode <id>`。命令词大小写不敏感，mode id 保持
/// server 公布的原样（协议 id 可能区分大小写）。
pub fn mode_command(text: &str, mentions: &[Mention]) -> Option<ModeCommand> {
    let stripped = strip_mentions_text(text, mentions);
    let mut tokens = stripped.split_whitespace();
    if !tokens.next()?.eq_ignore_ascii_case("/mode") {
        return None;
    }
    Some(match (tokens.next(), tokens.next()) {
        (Some(mode), None) => mode
            .parse()
            .map(ModeCommand::Set)
            .unwrap_or(ModeCommand::Invalid),
        _ => ModeCommand::Invalid,
    })
}

/// 解析 `/fleet [alias]` —— 管家 bot 的 fleet 观测命令。
///
/// `Some(None)` = 总览;`Some(Some(alias))` = 下钻某个 agent。`/fleet` 后面只能跟 agent 名,
/// 任何子命令都会和 `/fleet <alias>` 撞语义(有人把 agent 起成那个名字时,同一条输入有两种读法)。
pub fn fleet_command(text: &str, mentions: &[Mention]) -> Option<Option<String>> {
    let stripped = strip_mentions_text(text, mentions);
    let mut tokens = stripped.split_whitespace();
    if !tokens.next()?.eq_ignore_ascii_case(fleet_command_head()) {
        return None;
    }
    Some(tokens.next().map(str::to_string))
}

/// 抹掉 @ 占位后首 token 是否等于 `expected`(忽略大小写)。summon 自有命令的统一判定:
/// 命令词必须在开头(不被正文顺带提及误触发),但允许后面跟参数或说明文字。
fn first_token_is(text: &str, mentions: &[Mention], expected: &str) -> bool {
    strip_mentions_text(text, mentions)
        .split_whitespace()
        .next()
        .is_some_and(|head| head.eq_ignore_ascii_case(expected))
}

/// 外部 Claude bridge 控制命令。命中后必须在 daemon 创建 ChatHandle 前处理，绝不能透传给 CLI。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BridgeCommand {
    /// 群里精确 `@Bot /bridge`。
    Start,
    /// 私聊 `/bridge <一次性短码>`。
    Redeem { code: String },
    /// 私聊裸 `/bridge`，返回用法提示。
    Help,
    /// 私聊 `/bridge page <N>`，分页找回已绑定 Mirror 的控制卡。
    RecoveryPage { page: usize },
    /// 明确以 `/bridge` 开头但形态不合法；仍需拦截，不能交给 agent。
    Invalid,
}

/// `mentionless` = 本条群消息经一人一 Bot 验证免 @ 放行,此时不带任何 @ 也算在对本 bot 说话。
pub fn bridge_command(
    text: &str,
    mentions: &[Mention],
    is_group: bool,
    mentionless: bool,
) -> Option<BridgeCommand> {
    let command = strip_mentions_text(text, mentions);
    let mut parts = command.split_whitespace();
    let head = parts.next()?;
    if !head.eq_ignore_ascii_case("/bridge") {
        return None;
    }
    let args = parts.collect::<Vec<_>>();
    if is_group {
        let addressed = mentions.len() == 1 || (mentionless && mentions.is_empty());
        return if addressed && args.is_empty() {
            Some(BridgeCommand::Start)
        } else {
            Some(BridgeCommand::Invalid)
        };
    }
    match args.as_slice() {
        [] => Some(BridgeCommand::Help),
        [page, raw]
            if page.eq_ignore_ascii_case("page")
                && raw.parse::<usize>().is_ok_and(|page| page > 0) =>
        {
            Some(BridgeCommand::RecoveryPage {
                page: raw.parse().expect(summon_i18n::text!("r6849e7246e")),
            })
        }
        [code] => Some(BridgeCommand::Redeem {
            code: (*code).to_string(),
        }),
        _ => Some(BridgeCommand::Invalid),
    }
}

/// 识别「/introduce」指令:抹掉 @ 占位后**首 token** 等于 `/introduce`(忽略大小写)即命中,
/// 返回命令词之后的说明文字(trim 后,可能为空串)。
///
/// 跟 `/cd` 同级。此前用「整体等于」判定,`/introduce @botA @botB 互相认识下` 这种
/// 自然写法会漏给 `slash_command_text` 透传,CLI 报 `from unknown skill` 并卡满 stall 窗口。
///
/// 返回说明文字而非 bool:用户交代的分工(「A 开发、B 审核」)是发这条命令的主要意图,
/// 只认出命令、把正文丢掉等于只完成了一半——它会被存进 `PeersFile::note` 并注入双方上下文。
pub fn introduce_command(text: &str, mentions: &[Mention]) -> Option<String> {
    let s = strip_mentions_text(text, mentions);
    let mut parts = s.splitn(2, char::is_whitespace);
    let head = parts.next()?;
    if !head.eq_ignore_ascii_case("/introduce") {
        return None;
    }
    Some(parts.next().unwrap_or("").trim().to_string())
}

/// `/unintroduce` 的作用域。**两态分开而不是用 bool**:调用方 match 一次即可,
/// 且新增作用域时编译器会逼所有分支表态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnintroduceScope {
    /// 解除**被 @ 的这些 bot 之间**的相互认识。
    ///
    /// 与 `/introduce` 完全对称:群消息投给每个被 @ 的 bot,各自从自己的 peers 里移除
    /// 「被 @ 的其余人」。A 移除 B/C、B 移除 A/C、C 移除 A/B —— 两两解除是这个机制的
    /// 自然结果,不需要谁去做全局编排。没被 @ 的旧相识原样保留。
    Mentioned,
    /// 清空本群**全部** introduce 关系。
    ///
    /// **必须显式写 `--all`**:`/introduce` 打错顶多多认识一个 bot,而这条打错会静默清空
    /// 整群关系,之后所有 bot 互相 @ 都被 talk 闸拒、日志里只留一行「拒绝非白名单」——
    /// 那种失败极难被发现。所以不给「无参数即全清」这种省事写法。
    All,
}

/// 解析 `/unintroduce`:抹掉 @ 占位后首 token 为 `/unintroduce`(忽略大小写)才认。
///
/// 返回作用域而非 bool —— 见 [`UnintroduceScope`]。`--all` 之外的尾随文字一律忽略:
/// 解除不像 introduce 那样需要承载分工说明,多余的话没有存储去处。
pub fn unintroduce_command(text: &str, mentions: &[Mention]) -> Option<UnintroduceScope> {
    let s = strip_mentions_text(text, mentions);
    let mut parts = s.splitn(2, char::is_whitespace);
    let head = parts.next()?;
    if !head.eq_ignore_ascii_case("/unintroduce") {
        return None;
    }
    let rest = parts.next().unwrap_or("").trim();
    if rest
        .split_whitespace()
        .any(|t| t.eq_ignore_ascii_case("--all"))
    {
        return Some(UnintroduceScope::All);
    }
    Some(UnintroduceScope::Mentioned)
}

/// 是否一条「/summon」指令:抹掉 @ 占位后**首 token** 等于 `/summon`(忽略大小写)。
/// `/summon <agent>` 是 Claude 侧的一个 skill,不是飞书侧用户命令(summon 2026-08-08 起冷启
/// 也不再注入它作为 seed,见 `driver::default_seed`)。用户在飞书里误发会被 `slash_command_text`
/// 透传给 CLI,CLI 把它当 skill 执行 → 不输出 end_turn → `run_turn` 傻等 STALL_WINDOW(240s)
/// 超时,期间新消息全部排队不处理。这里拦截并给友好提示,绝不透传。
pub fn is_summon_command(text: &str, mentions: &[Mention]) -> bool {
    first_token_is(text, mentions, "/summon")
}

/// provider 斜杠候选分类:抹掉 @ 后正文若是**命令样式**(首 token 为 `/` + 字母开头、且 token
/// 内无第二个 `/`)即返回整段正文，供 daemon 保留命令语法并进入独立安全策略。分类本身不授权
/// 执行：当前 daemon 只允许 operate-gate 后、driver 声明的 exact compact；`/clear` 与未知命令
/// 都会拒绝。判定排除了 `/Users/foo` 这类路径(首 token 含第二个 `/`)和 `/123`(非字母起头)。
/// 已识别命令(如 `/session`)由各自分支先行处理、`continue`,不会走到这里。
///
/// **防御性排除**:在 `cli` 上归 summon 的命令(见 `summon_owned_command_head`)即使漏过了
/// 自己的拦截分支也不从这里透传;provider 原生的同名命令照常作为 slash 候选返回。
/// 注意这只保证「不被 CLI 当命令执行」——返回 None 后调用方仍会把它当普通消息处理(套「来自 X」
/// 信封,首字符不是 `/`,CLI 不会当命令)。正常路径应由 `summon_owned_command` 兜底给用法提示,
/// 别让它一路滑到 agent 面前。
pub fn slash_command_text(
    text: &str,
    mentions: &[Mention],
    cli: crate::driver::CliDriver,
) -> Option<String> {
    let s = strip_mentions_text(text, mentions);
    let first = s.split_whitespace().next()?;
    let rest = first.strip_prefix('/')?;
    let starts_alpha = rest.chars().next().is_some_and(|c| c.is_ascii_alphabetic());
    if !starts_alpha || rest.contains('/') {
        return None;
    }
    // summon 自有命令不走透传:由 daemon 各分支处理,CLI 不认识、当命令执行会报错并卡满 stall。
    if summon_owned_command_head(first, cli).is_some() {
        return None;
    }
    Some(s)
}

/// 飞书聊天框中由 summon 自己处理的公开命令。解析防透传与 `/help` 共用这份目录，新增命令时
/// 只改一处，不会出现命令已经可用、帮助却漏列的漂移。
struct ChatCommandSpec {
    head: &'static str,
    usage: &'static str,
    description: &'static str,
    scope: ChatCommandScope,
}

#[derive(Clone, Copy)]
enum ChatCommandScope {
    Shared,
    Agent,
    CodexOrCursor,
    Cursor,
    Steward,
}

impl ChatCommandScope {
    fn supports(self, cli: crate::driver::CliDriver) -> bool {
        match self {
            Self::Shared => true,
            Self::Agent => true,
            Self::CodexOrCursor => matches!(
                cli,
                crate::driver::CliDriver::Codex | crate::driver::CliDriver::Cursor
            ),
            Self::Cursor => cli == crate::driver::CliDriver::Cursor,
            Self::Steward => false,
        }
    }
}

const PUBLIC_CHAT_COMMANDS: &[ChatCommandSpec] = &[
    ChatCommandSpec {
        head: "/mentionless",
        usage: "/mentionless [on|off|permissions]",
        description: "rea2495e5d1",
        scope: ChatCommandScope::Agent,
    },
    ChatCommandSpec {
        head: "/help",
        usage: "/help",
        description: "r55fa79f06e",
        scope: ChatCommandScope::Shared,
    },
    ChatCommandSpec {
        head: "/session",
        usage: "/session [clean|reset|quota [on|off]]",
        description: "r8b664b3945",
        scope: ChatCommandScope::Agent,
    },
    ChatCommandSpec {
        head: "/quota",
        usage: "/quota [on|off]",
        description: "r516140c999",
        scope: ChatCommandScope::Agent,
    },
    ChatCommandSpec {
        head: "/model",
        usage: "/model [<id>|default <id>|inherit]",
        description: "r0be5afe98c",
        scope: ChatCommandScope::Agent,
    },
    ChatCommandSpec {
        head: "/effort",
        usage: "rfc7ccf8dae",
        description: "r5a34cef60c",
        scope: ChatCommandScope::Agent,
    },
    ChatCommandSpec {
        head: "/fast",
        usage: "/fast on|off",
        description: "r82feb7e5c3",
        scope: ChatCommandScope::CodexOrCursor,
    },
    ChatCommandSpec {
        head: "/mode",
        usage: "/mode <id>",
        description: "rb66fb1cc84",
        scope: ChatCommandScope::Cursor,
    },
    ChatCommandSpec {
        head: "/cd",
        usage: "r0d912a4692",
        description: "r432b5f215b",
        scope: ChatCommandScope::Agent,
    },
    ChatCommandSpec {
        head: "/pwd",
        usage: "/pwd",
        description: "r1481f3eee5",
        scope: ChatCommandScope::Agent,
    },
    ChatCommandSpec {
        head: "/schedules",
        usage: "/schedules",
        description: "r61cb44e7cc",
        scope: ChatCommandScope::Agent,
    },
    ChatCommandSpec {
        head: "/remind",
        usage: "rd3cb9557ef",
        description: "rd174a60e07",
        scope: ChatCommandScope::Agent,
    },
    ChatCommandSpec {
        head: "/bridge",
        usage: "r2fce44c44d",
        description: "r9fe62364a6",
        scope: ChatCommandScope::Agent,
    },
    ChatCommandSpec {
        head: "/introduce",
        usage: "r761e100905",
        description: "r674275911d",
        scope: ChatCommandScope::Agent,
    },
    ChatCommandSpec {
        head: "/unintroduce",
        usage: "r6d5c13d865",
        description: "r1384bc32df",
        scope: ChatCommandScope::Agent,
    },
    ChatCommandSpec {
        head: "/fleet",
        usage: "/fleet [agent]",
        description: "r54d3ad7ea7",
        scope: ChatCommandScope::Steward,
    },
];

/// `/summon` 只用于内部冷启，仍属于 summon 自有词，避免误透传给 provider。
const INTERNAL_CHAT_COMMANDS: &[&str] = &["/summon"];

/// 与 summon 命令同名、但在该 CLI 上归 provider 原生处理的命令:summon 不接管,整条交给
/// slash 透传注入会话。Claude Code 自带 `/fast [on|off]`:它把 `fastMode` 写进用户级
/// `~/.claude/settings.json`(本机共用这份配置的 Claude 会话都受影响、按更高费率计费),
/// 当前模型不支持时顺带切模型;裸 `/fast` 弹选择面板。
fn provider_native_command(cli: crate::driver::CliDriver, head: &str) -> bool {
    cli == crate::driver::CliDriver::Claude && head.eq_ignore_ascii_case("/fast")
}

/// 首 token 命中的 summon 自有命令词(忽略大小写),否则 None。provider 原生同名命令不算。
fn summon_owned_command_head(head: &str, cli: crate::driver::CliDriver) -> Option<&'static str> {
    if provider_native_command(cli, head) {
        return None;
    }
    PUBLIC_CHAT_COMMANDS
        .iter()
        .find(|command| command.head.eq_ignore_ascii_case(head))
        .map(|command| command.head)
        .or_else(|| {
            INTERNAL_CHAT_COMMANDS
                .iter()
                .find(|command| command.eq_ignore_ascii_case(head))
                .copied()
        })
}

/// 抹掉 @ 占位后首 token 命中的 summon 自有命令词。**兜底用**:dispatcher 里各命令的专门分支
/// 都已 `continue`,还能走到这里说明该命令的形态没被它自己的解析器认出(比如参数写错)。
///
/// 此时既不能透传 CLI(报错卡轮),也不该静默当普通消息喂给 agent——那会让 bot 对着
/// `/session 看看` 用自然语言瞎回。调用方据此回一句用法提示,让用户知道命令没被识别。
pub fn summon_owned_command(
    text: &str,
    mentions: &[Mention],
    cli: crate::driver::CliDriver,
) -> Option<&'static str> {
    let s = strip_mentions_text(text, mentions);
    summon_owned_command_head(s.split_whitespace().next()?, cli)
}

/// `/help` 只接受无参精确形态，避免把未来的子命令悄悄吞掉。
pub fn help_command(text: &str, mentions: &[Mention]) -> bool {
    strip_mentions_text(text, mentions)
        .trim()
        .eq_ignore_ascii_case("/help")
}

/// 飞书聊天框的 summon 本地命令清单。provider 原生命令由 daemon 按当前 CLI/通道追加。
pub fn chat_help_text(cli: crate::driver::CliDriver) -> String {
    let mut text = String::from(summon_i18n::text!("rec904d6f86"));
    for command in PUBLIC_CHAT_COMMANDS
        .iter()
        .filter(|command| command.scope.supports(cli))
    {
        if command.head == command.usage {
            text.push_str(&format!(
                "\n- `{}`：{}",
                command.head,
                crate::i18n::label(command.description)
            ));
        } else {
            text.push_str(&format!(
                "\n- `{}` · `{}`：{}",
                command.head,
                crate::i18n::label(command.usage),
                crate::i18n::label(command.description)
            ));
        }
    }
    text
}

pub fn steward_chat_help_text() -> String {
    let mut text = String::from(summon_i18n::text!("rb1b8caa294"));
    for command in PUBLIC_CHAT_COMMANDS.iter().filter(|command| {
        matches!(
            command.scope,
            ChatCommandScope::Shared | ChatCommandScope::Steward
        )
    }) {
        text.push_str(&format!(
            "\n- `{}`：{}",
            crate::i18n::label(command.usage),
            crate::i18n::label(command.description)
        ));
    }
    text
}

fn fleet_command_head() -> &'static str {
    PUBLIC_CHAT_COMMANDS
        .iter()
        .find(|command| matches!(command.scope, ChatCommandScope::Steward))
        .map(|command| command.head)
        .expect(summon_i18n::text!("rc4c942d09d"))
}

/// 识别 `/cd <路径>` 切换本会话工作目录的命令:抹掉 @ 后首 token 整体等于 `/cd`(忽略大小写)
/// 才命中,返回剩余正文(trim 后,可能为空串)。空串交给分派侧给「用法」提示。`~` 不在这里展开
/// (纯函数不碰文件系统/环境),展开与存在性校验留给 IO 侧。必须在 `slash_command_text` 透传之前
/// 拦截——否则 `/cd /x` 会被当成命令样式原样转发给 CLI。
pub fn cd_command(text: &str, mentions: &[Mention]) -> Option<String> {
    let s = strip_mentions_text(text, mentions);
    let mut parts = s.splitn(2, char::is_whitespace);
    let head = parts.next()?;
    if !head.eq_ignore_ascii_case("/cd") {
        return None;
    }
    Some(parts.next().unwrap_or("").trim().to_string())
}

/// `/pwd` 是 summon 的只读工作目录视图，不透传 provider，也不要求会话已启动。
pub fn pwd_command(text: &str, mentions: &[Mention]) -> bool {
    strip_mentions_text(text, mentions)
        .trim()
        .eq_ignore_ascii_case("/pwd")
}

/// 抹 @bot 后首 token 是否为 `/schedules`(列表,只读命令,不喂 agent)。
/// 首 token 而非整体匹配,理由同 `session_command`:`/schedules 明天的` 不该漏成普通消息。
pub fn is_schedule_command(text: &str, mentions: &[Mention]) -> bool {
    first_token_is(text, mentions, "/schedules")
}

/// `/remind <when…> <prompt>` 解析结果。
pub struct Remind {
    pub kind: crate::schedule::Kind,
    pub spec: String,
    pub prompt: String,
}

/// 裸 `/remind`(命令词后没有任何参数)。这是**卡片式创建的入口**:不带参数时不再回一句语法错,
/// 而是发一张起点卡。带参数的老写法照旧走 [`parse_remind`],两条路径共存。
pub fn bare_remind_command(text: &str, mentions: &[Mention]) -> bool {
    let body = strip_mentions_text(text, mentions);
    match body.strip_prefix("/remind") {
        Some(rest) => rest.trim().is_empty(),
        None => false,
    }
}

/// 解析 `/remind …`;非 /remind 返回 Ok(None);语法错返回 Err(友好串)。`now` 注入便于测试。
///
/// 先抹 @ 占位再解析,与其余命令一致:群里 `@Bot /remind in 10m 喝水` 的原文是
/// `@_user_1 /remind …`,不抹占位会 `strip_prefix` 失败、整条降级成普通消息,群里的 `/remind`
/// 就永远进不了定时器。
pub fn parse_remind(
    text: &str,
    mentions: &[Mention],
    now: chrono::DateTime<chrono::Local>,
) -> Result<Option<Remind>, String> {
    let body = strip_mentions_text(text, mentions);
    let Some(rest) = body.strip_prefix("/remind") else {
        return Ok(None);
    };
    let toks: Vec<&str> = rest.split_whitespace().collect();
    let w = crate::schedule::parse_when(&toks, now)?;
    let prompt = toks[w.consumed..].join(" ");
    if prompt.trim().is_empty() {
        return Err(summon_i18n::text!("rbff06d03f2").to_string());
    }
    Ok(Some(Remind {
        kind: w.kind,
        spec: w.spec,
        prompt,
    }))
}

/// sidecar(Go)→ daemon 的入站事件;events.sock 上一行一个 JSON。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Event {
    /// 云文档评论通知；daemon 持久化后才向 sidecar 确认。
    #[serde(rename = "document_comment")]
    DocumentComment {
        app_id: String,
        event_id: String,
        file_token: String,
        file_type: String,
        comment_id: String,
        reply_id: String,
        sender: String,
        recipient: String,
    },
    /// sidecar 进程级协议握手。卡片回调只在协议版本匹配后接收；老
    /// sidecar 缺字段会解析为 0，供 daemon fail-closed 拒绝。
    Hello {
        app_id: String,
        #[serde(default)]
        protocol: u32,
    },
    /// 收到用户消息
    Message {
        app_id: String,
        chat_id: String,
        #[serde(default)]
        chat_type: Option<String>,
        #[serde(default)]
        thread_id: Option<String>,
        #[serde(default)]
        root_id: Option<String>,
        sender: String,
        /// 发信人 union_id(同开发者下跨 app 稳定);白名单优先用它命中,空则退回 open_id。
        #[serde(default)]
        sender_union_id: String,
        /// 发信人类型:`"user"` = 真人,`"app"` = bot,空 = 老 sidecar 没带(未知)。
        /// agent 判断「这条是不是另一个 bot 发的」全靠它;缺了它,多 bot 互 @ 时防回音没有依据。
        #[serde(default)]
        sender_type: String,
        #[serde(default)]
        sender_name: String,
        text: String,
        message_id: String,
        /// 被引用/被回复消息的 message_id(飞书「引用回复」时由 webhook 带回,普通新消息为空)。
        /// daemon 据此拉回原消息、把它当「引用上下文」注入,优先帮 agent 判断用户意图。
        #[serde(default)]
        parent_id: Option<String>,
        /// 群消息的 @ 列表(p2p 通常为空);daemon 用它门控"是否 @ 了本 bot"。
        #[serde(default)]
        mentions: Vec<Mention>,
        /// 随消息发来的附件(图片/文件等);daemon 下载到本地后把路径喂给 agent。
        #[serde(default)]
        resources: Vec<Resource>,
        #[serde(default)]
        ts: i64,
        /// 飞书消息创建时间(毫秒,sidecar 从 msg.create_time 透传)。入站去重+时效闸用:抖动后被
        /// 补投的旧事件据此判陈旧。老 sidecar 不带 → `#[serde(default)]` = 0(未知,跳过时效只去重)。
        #[serde(default)]
        create_time_ms: i64,
    },
    /// 卡片按钮回调(card.action.trigger,经长连接;sidecar 已抽取 action 字段)
    #[serde(rename = "card_action")]
    CardAction {
        app_id: String,
        chat_id: String,
        message_id: String,
        #[serde(default)]
        operator_open_id: String,
        #[serde(default)]
        event_id: String,
        /// 被点按钮的完整 value(idle 卡片 {"action":..};选项卡片 {"kind":"ask","ask":..,"opt":..})。
        #[serde(default)]
        value: serde_json::Value,
    },
    /// 群被解散(`im.chat.disbanded_v1`)。bot 已不在群,只能 p2p 通知群主。
    #[serde(rename = "chat_disbanded")]
    ChatDisbanded {
        app_id: String,
        chat_id: String,
        /// 解散群的操作者(≈群主/管理员)open_id;群主回退源之一。
        #[serde(default)]
        operator_open_id: String,
    },
    /// bot 被移出某 chat(`im.chat.member.bot.deleted_v1`)。同样 p2p 通知操作者。
    #[serde(rename = "bot_removed")]
    BotRemoved {
        app_id: String,
        chat_id: String,
        /// 移除 bot 的操作者 open_id;群主回退源之一。
        #[serde(default)]
        operator_open_id: String,
    },
    /// 用户撤回消息(`im.message.recalled_v1`)。chat 线程在消费 Task 前检查此标记,已撤回则跳过。
    #[serde(rename = "message_recalled")]
    MessageRecalled {
        app_id: String,
        message_id: String,
        chat_id: String,
    },
    /// sidecar报错
    Error { app_id: String, detail: String },
}

impl Event {
    pub fn from_line(line: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(line.trim())
    }

    /// 序列化为一行(带结尾换行),写 socket 用。
    pub fn to_line(&self) -> String {
        let mut s = serde_json::to_string(self).expect("event serialize");
        s.push('\n');
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::driver::CliDriver;

    #[test]
    fn message_roundtrip() {
        let ev = Event::Message {
            app_id: "cli_abc".into(),
            chat_id: "oc_1".into(),
            chat_type: None,
            thread_id: None,
            root_id: None,
            sender: "ou_1".into(),
            sender_union_id: "on_1".into(),
            sender_type: String::new(),
            sender_name: String::new(),
            text: "hi".into(),
            message_id: "om_1".into(),
            parent_id: None,
            mentions: vec![],
            resources: vec![],
            ts: 0,
            create_time_ms: 1_717_000_000_000,
        };
        let line = ev.to_line();
        assert!(line.ends_with('\n'));
        assert_eq!(Event::from_line(&line).unwrap(), ev);
    }

    #[test]
    fn parses_chat_disbanded_and_bot_removed() {
        let ev = Event::from_line(
            r#"{"type":"chat_disbanded","app_id":"a","chat_id":"oc_1","operator_open_id":"ou_owner"}"#,
        )
        .unwrap();
        assert_eq!(
            ev,
            Event::ChatDisbanded {
                app_id: "a".into(),
                chat_id: "oc_1".into(),
                operator_open_id: "ou_owner".into(),
            }
        );
        // operator 缺省 → 空串(向后兼容)。
        let ev =
            Event::from_line(r#"{"type":"bot_removed","app_id":"a","chat_id":"oc_2"}"#).unwrap();
        assert_eq!(
            ev,
            Event::BotRemoved {
                app_id: "a".into(),
                chat_id: "oc_2".into(),
                operator_open_id: String::new(),
            }
        );
    }

    #[test]
    fn legacy_message_still_decodes_after_new_variants() {
        // 新变体不破坏旧 message 行反序列化(回归)。
        let ev = Event::from_line(
            r#"{"type":"message","app_id":"a","chat_id":"c","sender":"s","text":"hi","message_id":"m"}"#,
        )
        .unwrap();
        assert!(matches!(ev, Event::Message { .. }));
    }

    #[test]
    fn parses_hello_and_tag() {
        let ev = Event::from_line(r#"{"type":"hello","app_id":"cli_x","protocol":2}"#).unwrap();
        assert_eq!(
            ev,
            Event::Hello {
                app_id: "cli_x".into(),
                protocol: 2,
            }
        );
    }

    #[test]
    fn legacy_hello_decodes_as_protocol_zero_for_fail_closed_gate() {
        let ev = Event::from_line(r#"{"type":"hello","app_id":"cli_x"}"#).unwrap();
        assert_eq!(
            ev,
            Event::Hello {
                app_id: "cli_x".into(),
                protocol: 0,
            }
        );
    }

    #[test]
    fn message_carries_chat_type_and_ignores_unknown_fields() {
        // Go 会多发 mentions 字段;内部 tag 枚举默认忽略未知字段,不应报错
        let ev = Event::from_line(
            r#"{"type":"message","app_id":"a","chat_id":"c","chat_type":"group","sender":"s","text":"hi","message_id":"m","mentions":[{"open_id":"ou_1","key":"@_user_1","name":"张三"}]}"#,
        )
        .unwrap();
        match ev {
            Event::Message {
                chat_type,
                mentions,
                ..
            } => {
                assert_eq!(chat_type.as_deref(), Some("group"));
                // mentions 现在是已知字段,应被解析出来(不再当未知字段丢弃)
                assert_eq!(mentions.len(), 1);
                assert_eq!(mentions[0].open_id, "ou_1");
            }
            _ => panic!("expected message"),
        }
    }

    #[test]
    fn message_parses_resources_and_defaults_empty() {
        // 带附件:解析出 resources;无该字段(老 sidecar):默认空,不报错
        let ev = Event::from_line(
            r#"{"type":"message","app_id":"a","chat_id":"c","sender":"s","text":"","message_id":"m","resources":[{"file_key":"img_1","kind":"image"},{"file_key":"f_1","kind":"file","name":"a.pdf"}]}"#,
        )
        .unwrap();
        match ev {
            Event::Message { resources, .. } => {
                assert_eq!(resources.len(), 2);
                assert_eq!(resources[0].kind, "image");
                assert_eq!(resources[1].name, "a.pdf");
            }
            _ => panic!("expected message"),
        }
        let bare = Event::from_line(
            r#"{"type":"message","app_id":"a","chat_id":"c","sender":"s","text":"hi","message_id":"m"}"#,
        )
        .unwrap();
        match bare {
            Event::Message { resources, .. } => assert!(resources.is_empty()),
            _ => panic!("expected message"),
        }
    }

    #[test]
    fn message_parses_parent_id_and_defaults_none() {
        // 引用回复:webhook 带 parent_id → 解析出来
        let ev = Event::from_line(
            r#"{"type":"message","app_id":"a","chat_id":"c","sender":"s","text":"看下","message_id":"m","parent_id":"om_quoted","thread_id":"omt_topic","root_id":"om_root"}"#,
        )
        .unwrap();
        match ev {
            Event::Message {
                parent_id,
                thread_id,
                root_id,
                ..
            } => {
                assert_eq!(parent_id.as_deref(), Some("om_quoted"));
                assert_eq!(thread_id.as_deref(), Some("omt_topic"));
                assert_eq!(root_id.as_deref(), Some("om_root"));
            }
            _ => panic!("expected message"),
        }
        // 普通消息(老 sidecar 不带该字段):默认 None,不报错
        let bare = Event::from_line(
            r#"{"type":"message","app_id":"a","chat_id":"c","sender":"s","text":"hi","message_id":"m"}"#,
        )
        .unwrap();
        match bare {
            Event::Message {
                parent_id,
                thread_id,
                root_id,
                ..
            } => {
                assert!(parent_id.is_none());
                assert!(thread_id.is_none());
                assert!(root_id.is_none());
            }
            _ => panic!("expected message"),
        }
    }

    #[test]
    fn mentions_contain_matches_by_open_id() {
        let ms = vec![
            Mention {
                open_id: "ou_bot".into(),
                key: "@_user_1".into(),
                name: "蓝翼飞龙".into(),
                ..Default::default()
            },
            Mention {
                open_id: String::new(),
                key: "@_user_2".into(),
                name: "".into(),
                ..Default::default()
            },
        ];
        assert!(mentions_contain(&ms, "ou_bot"));
        assert!(!mentions_contain(&ms, "ou_other"));
        // 空 me / 空 open_id 不应误命中
        assert!(!mentions_contain(&ms, ""));
        assert!(!mentions_contain(&[], "ou_bot"));
    }

    #[test]
    fn bridge_command_is_exact_and_never_matches_normal_text() {
        let bot = vec![Mention {
            open_id: "ou_bot".into(),
            key: "@_user_1".into(),
            name: "bot".into(),
            ..Default::default()
        }];
        assert_eq!(
            bridge_command("@_user_1 /bridge", &bot, true, false),
            Some(BridgeCommand::Start)
        );
        assert_eq!(
            bridge_command("@_user_1 /bridge CODE", &bot, true, false),
            Some(BridgeCommand::Invalid)
        );
        assert_eq!(
            bridge_command("/bridge", &[], false, false),
            Some(BridgeCommand::Help)
        );
        assert_eq!(
            bridge_command("/bridge BRG-A1", &[], false, false),
            Some(BridgeCommand::Redeem {
                code: "BRG-A1".into()
            })
        );
        assert_eq!(
            bridge_command("/bridge page 2", &[], false, false),
            Some(BridgeCommand::RecoveryPage { page: 2 })
        );
        assert_eq!(
            bridge_command("/bridge page 0", &[], false, false),
            Some(BridgeCommand::Invalid)
        );
        // 两个未知参数 = Invalid(私聊只认单个短码/page N)。
        assert_eq!(
            bridge_command("/bridge A B", &[], false, false),
            Some(BridgeCommand::Invalid)
        );
        assert_eq!(
            bridge_command("解释 /bridge 是什么", &[], false, false),
            None
        );

        let mut two_mentions = bot.clone();
        two_mentions.push(Mention {
            open_id: "ou_other".into(),
            key: "@_user_2".into(),
            name: "other".into(),
            ..Default::default()
        });
        assert_eq!(
            bridge_command("@_user_1 @_user_2 /bridge", &two_mentions, true, false),
            Some(BridgeCommand::Invalid)
        );

        // 一人一 Bot 验证放行的群消息不带 @ 也算对本 bot 说话;未放行的仍要求 @。
        assert_eq!(
            bridge_command("/bridge", &[], true, true),
            Some(BridgeCommand::Start)
        );
        assert_eq!(
            bridge_command("/bridge", &[], true, false),
            Some(BridgeCommand::Invalid)
        );
        assert_eq!(
            bridge_command("/bridge CODE", &[], true, true),
            Some(BridgeCommand::Invalid)
        );
    }

    /// `--all` 必须显式:不给「无参数即全清」的省事写法。打错会静默清空整群关系,
    /// 之后所有 bot 互相 @ 都被 talk 闸拒、日志只留一行「拒绝非白名单」,极难发现。
    #[test]
    fn unintroduce_scope_requires_explicit_all_flag() {
        let m = vec![];
        assert_eq!(
            unintroduce_command("/unintroduce", &m),
            Some(UnintroduceScope::Mentioned),
            "无参数只解除被 @ 的,绝不当全清"
        );
        assert_eq!(
            unintroduce_command("/unintroduce --all", &m),
            Some(UnintroduceScope::All)
        );
        // 大小写不敏感,与 /introduce 一致
        assert_eq!(
            unintroduce_command("/UNINTRODUCE --ALL", &m),
            Some(UnintroduceScope::All)
        );
        // 不是这条命令
        assert_eq!(unintroduce_command("/introduce", &m), None);
        assert_eq!(unintroduce_command("聊天内容 /unintroduce", &m), None);
    }

    /// **必须登记进 SUMMON_OWNED_COMMANDS**:不登记就会被当 provider 命令透传给 CLI,
    /// CLI 不认识 → 不输出 end_turn → run_turn 傻等满 STALL_WINDOW,期间新消息全排队。
    #[test]
    fn unintroduce_is_never_passed_through_to_cli() {
        assert_eq!(
            slash_command_text("/unintroduce --all", &[], CliDriver::Codex),
            None
        );
        assert_eq!(
            slash_command_text("/unintroduce", &[], CliDriver::Codex),
            None
        );
    }

    #[test]
    fn introduce_command_matches_first_token_and_keeps_note() {
        let bot = vec![Mention {
            open_id: "ou_bot".into(),
            key: "@_user_1".into(),
            name: "蓝翼飞龙".into(),
            ..Default::default()
        }];
        // 群里 @bot + /introduce(占位抹掉后首 token 匹配),无说明文字 → 空串
        assert_eq!(
            introduce_command("@_user_1 /introduce", &bot).as_deref(),
            Some("")
        );
        assert_eq!(
            introduce_command("  @_user_1   /INTRODUCE  ", &bot).as_deref(),
            Some("")
        );
        // 私聊直接发(无 @)
        assert_eq!(introduce_command("/introduce", &[]).as_deref(), Some(""));
        // 说明文字必须原样保留:这是用户交代分工的唯一载体,丢了等于只修了一半
        assert_eq!(
            introduce_command("@_user_1 /introduce 星光开发、蓝翼飞龙审核", &bot).as_deref(),
            Some("星光开发、蓝翼飞龙审核")
        );
        // 真机原文形态(@ 占位在命令词之后,抹掉后说明文字紧跟)
        assert_eq!(
            introduce_command("/introduce @_user_1 互相介绍下, 星光现在是开发", &bot).as_deref(),
            Some("互相介绍下, 星光现在是开发")
        );
        // 正文顺带提到不误触发(首 token 不是 /introduce)
        assert_eq!(introduce_command("帮我看下 /introduce 是干嘛的", &[]), None);
        assert_eq!(introduce_command("introduce", &[]), None);
    }

    #[test]
    fn summon_command_matches_first_token() {
        let bot = vec![Mention {
            open_id: "ou_bot".into(),
            key: "@_user_1".into(),
            name: "蓝翼飞龙".into(),
            ..Default::default()
        }];
        // /summon 是冷启 seed 命令,用户误发必须拦截、不能透传 CLI
        assert!(is_summon_command("/summon blue-wing", &[]));
        assert!(is_summon_command("@_user_1 /summon starlight", &bot));
        assert!(is_summon_command("  /SUMMON  ", &[]));
        // 正文顺带提到不误触发
        assert!(!is_summon_command("帮我看下 /summon 是干嘛的", &[]));
        assert!(!is_summon_command("summon", &[]));
    }

    #[test]
    fn cd_command_extracts_path_arg() {
        let bot = vec![Mention {
            open_id: "ou_bot".into(),
            key: "@_user_1".into(),
            name: "蓝翼飞龙".into(),
            ..Default::default()
        }];
        // /cd <路径> → Some(路径)
        assert_eq!(cd_command("/cd /tmp", &[]), Some("/tmp".to_string()));
        // 群里 @bot + /cd:抹掉 @ 后识别;~ 原样保留(展开交给 IO 侧)
        assert_eq!(
            cd_command("@_user_1 /cd ~/work", &bot),
            Some("~/work".to_string())
        );
        // 多余空白 trim 掉
        assert_eq!(cd_command("/cd   /a/b/c", &[]), Some("/a/b/c".to_string()));
        // 裸 /cd(无参)→ Some("") 让分派侧给「用法」提示,而非静默忽略
        assert_eq!(cd_command("/cd", &[]), Some(String::new()));
        // 路径/相近词不误命中
        assert_eq!(cd_command("/cdfoo /tmp", &[]), None);
        assert_eq!(cd_command("/session", &[]), None);
        assert_eq!(cd_command("切到 /tmp 看看", &[]), None);
    }

    #[test]
    fn pwd_command_is_exact_and_never_passes_through() {
        let bot = vec![Mention {
            open_id: "ou_bot".into(),
            key: "@_user_1".into(),
            name: "蓝翼飞龙".into(),
            ..Default::default()
        }];
        assert!(pwd_command("/pwd", &[]));
        assert!(pwd_command("@_user_1 /PWD", &bot));
        assert!(!pwd_command("/pwd extra", &[]));
        assert!(!pwd_command("看下 /pwd", &[]));
        assert_eq!(slash_command_text("/pwd", &[], CliDriver::Codex), None);
    }

    #[test]
    fn help_command_is_exact_owned_and_never_passes_through() {
        let bot = vec![Mention {
            key: "@_user_1".into(),
            open_id: "ou_bot".into(),
            name: "Bot".into(),
            ..Default::default()
        }];
        assert!(help_command("/help", &[]));
        assert!(help_command("@_user_1 /HELP", &bot));
        assert!(!help_command("/help extra", &[]));
        assert!(!help_command("看看 /help", &[]));
        assert_eq!(slash_command_text("/help", &[], CliDriver::Codex), None);
        assert_eq!(
            summon_owned_command("/help", &[], CliDriver::Claude),
            Some("/help")
        );
    }

    #[test]
    fn chat_help_covers_every_public_summon_command() {
        let help = chat_help_text(CliDriver::Codex);
        assert!(help.starts_with("**Summon 管理**"));
        assert!(!help.contains("飞书聊天指令"));
        for command in [
            "/help",
            "/session",
            "/quota",
            "/model",
            "/fast",
            "/mode",
            "/cd",
            "/pwd",
            "/schedules",
            "/remind",
            "/bridge",
            "/introduce",
            "/unintroduce",
        ] {
            assert!(
                help.contains(&format!("`{command}")),
                "帮助遗漏 {command}: {help}"
            );
        }
        assert_eq!(
            summon_owned_command("/fleet", &[], CliDriver::Claude),
            Some("/fleet")
        );
        assert!(
            !help.contains("`/fleet`"),
            "普通 agent 不应展示管家专属命令"
        );
        assert!(!help.contains("`/summon`"), "内部冷启词不应伪装成用户命令");
        let claude = chat_help_text(CliDriver::Claude);
        assert!(!claude.contains("`/fast`"));
        assert!(!claude.contains("`/mode`"));
        let traex = chat_help_text(CliDriver::Traex);
        assert!(!traex.contains("`/fast`"));
        let cursor = chat_help_text(CliDriver::Cursor);
        assert!(cursor.contains("`/fast`"));
        assert!(cursor.contains("`/mode`"));
        let steward = steward_chat_help_text();
        assert!(steward.starts_with("**可用命令**\n\n**Summon 管理**"));
        assert!(steward.contains("`/help`"));
        assert!(steward.contains("`/fleet [agent]`"));
        assert!(!steward.contains("`/session`"));
        assert_eq!(
            fleet_command("/fleet coder", &[]),
            Some(Some("coder".into()))
        );
    }

    /// `/session reset` 是 `/bridge` 被「该群已有可恢复会话」挡住时的唯一解法(owner 实测撞上)。
    /// 挂在 `/session` 下而非另开顶层命令：summon 与 provider 共用斜杠命名空间，每多一个顶层
    /// summon 命令就永久占掉一个 provider 未来可能用的词。
    #[test]
    fn session_subcommands_map_to_the_two_panel_buttons() {
        use SessionCommand::*;
        let bot = vec![Mention {
            open_id: "ou_bot".into(),
            key: "@_user_1".into(),
            name: "Arcadia".into(),
            ..Default::default()
        }];
        assert_eq!(session_command("/session", &[]), Some(Panel));
        assert_eq!(session_command("@_user_1 /session", &bot), Some(Panel));
        assert_eq!(session_command("/session reset", &[]), Some(Reset));
        assert_eq!(
            session_command("@_user_1 /session reset", &bot),
            Some(Reset)
        );
        assert_eq!(session_command("/session clean", &[]), Some(Clean));
        assert_eq!(
            session_command("/SESSION RESET", &[]),
            Some(Reset),
            "大小写不敏感"
        );
        assert_eq!(
            session_command("  @_user_1   /SESSION  ", &bot),
            Some(Panel)
        );
        assert_eq!(session_command("session", &[]), None);
        // 首 token 而非整体匹配:`/session 看看` 不能漏成普通消息(整体匹配时 bot 只能瞎回,
        // `/introduce` 就是这么炸的);正文里顺带提到则不该误触发。
        assert_eq!(
            session_command("@_user_1 /session 再顺手干个活", &bot),
            Some(Panel)
        );
        assert_eq!(session_command("帮我看下 /session 是干嘛的", &[]), None);
        assert_eq!(session_command("帮我看下进度", &[]), None);
        assert_eq!(
            session_command("/sessions", &[]),
            None,
            "不能被相近词误命中"
        );

        // fail-safe:打错的子命令绝不能被猜成破坏性动作,一律退回面板(面板上就有那两个按钮)。
        assert_eq!(session_command("/session rest", &[]), Some(Panel));
        assert_eq!(session_command("/session 看看", &[]), Some(Panel));
        assert_eq!(session_command("/session resetx", &[]), Some(Panel));
    }

    /// `/session quota [on|off|开|关]`:面板「额度显示」按钮的文字入口。裸 quota = 翻转;
    /// on/off(兼容中文)显式指定;不认识的第三 token 落翻转——后果只是显示项可见地翻一下,
    /// 与「打错不得猜成破坏动作」不冲突。`quotax` 这类相近词仍退面板,不误命中。
    #[test]
    fn session_quota_parses_toggle_and_explicit_forms() {
        use SessionCommand::*;
        assert_eq!(session_command("/session quota", &[]), Some(Quota(None)));
        assert_eq!(
            session_command("/session quota on", &[]),
            Some(Quota(Some(true)))
        );
        assert_eq!(
            session_command("/session quota off", &[]),
            Some(Quota(Some(false)))
        );
        assert_eq!(
            session_command("/session quota 开", &[]),
            Some(Quota(Some(true)))
        );
        assert_eq!(
            session_command("/session quota 关", &[]),
            Some(Quota(Some(false)))
        );
        assert_eq!(
            session_command("/SESSION QUOTA ON", &[]),
            Some(Quota(Some(true))),
            "大小写不敏感"
        );
        assert_eq!(
            session_command("/session quota maybe", &[]),
            Some(Quota(None)),
            "不认识的第三 token 落翻转"
        );
        assert_eq!(
            session_command("/session quotax", &[]),
            Some(Panel),
            "相近词不误命中"
        );
    }

    /// `/quota [on|off|开|关]`:`/session quota` 的短别名,语义逐字节一致(裸=翻转、
    /// 显式 on/off、不认识落翻转);首 token 精确匹配,相近词与正文提及不误命中。
    #[test]
    fn quota_command_parses_short_alias() {
        // 不是本命令 → 外层 None
        assert_eq!(quota_command("/quotax", &[]), None, "相近词不误命中");
        assert_eq!(
            quota_command("帮我看看 /quota 是啥", &[]),
            None,
            "正文提及不误触发"
        );
        assert_eq!(
            quota_command("/session quota on", &[]),
            None,
            "长形式走 session_command"
        );
        // 裸命令 → 翻转
        assert_eq!(quota_command("/quota", &[]), Some(None));
        // 显式 on/off(兼容中文、大小写不敏感)
        assert_eq!(quota_command("/quota on", &[]), Some(Some(true)));
        assert_eq!(quota_command("/quota OFF", &[]), Some(Some(false)));
        assert_eq!(quota_command("/quota 开", &[]), Some(Some(true)));
        assert_eq!(quota_command("/quota 关", &[]), Some(Some(false)));
        // 不认识的参数落翻转;多余参数不影响
        assert_eq!(quota_command("/quota maybe", &[]), Some(None));
        assert_eq!(quota_command("/quota on extra", &[]), Some(Some(true)));
        // 群里 @bot 占位剥离后照常识别
        let bot = vec![Mention {
            open_id: "ou_bot".into(),
            key: "@_user_1".into(),
            name: "星光".into(),
            ..Default::default()
        }];
        assert_eq!(
            quota_command("@_user_1 /quota off", &bot),
            Some(Some(false))
        );
    }

    #[test]
    fn slash_command_candidate_strips_mention_and_detects_slash() {
        let bot = vec![Mention {
            open_id: "ou_bot".into(),
            key: "@_user_1".into(),
            name: "蓝翼飞龙".into(),
            ..Default::default()
        }];
        // 群里 @bot + /compact → 抹掉 @,返回纯命令供原样转发
        assert_eq!(
            slash_command_text("@_user_1 /compact", &bot, CliDriver::Codex),
            Some("/compact".to_string())
        );
        // 私聊直接发斜杠命令
        assert_eq!(
            slash_command_text("/clear", &[], CliDriver::Codex),
            Some("/clear".to_string())
        );
        // 未被 summon 接管的命令带参数仍完整分类；是否执行由 daemon 的独立策略裁决。
        assert_eq!(
            slash_command_text("/compact now", &[], CliDriver::Codex),
            Some("/compact now".to_string())
        );
        // 普通消息不透传
        assert_eq!(
            slash_command_text("帮我看下进度", &[], CliDriver::Codex),
            None
        );
        assert_eq!(
            slash_command_text("@_user_1 你好", &bot, CliDriver::Codex),
            None
        );
        // 路径不是命令样式(首 token 含第二个 `/`)
        assert_eq!(
            slash_command_text("/Users/me/foo.txt 看下", &[], CliDriver::Codex),
            None
        );
        assert_eq!(
            slash_command_text("/etc/hosts", &[], CliDriver::Codex),
            None
        );
        // 非字母起头不是命令
        assert_eq!(slash_command_text("/123", &[], CliDriver::Codex), None);
        assert_eq!(slash_command_text("/ 空格", &[], CliDriver::Codex), None);
        // summon 自有命令不从这里透传(防御性排除:即使专门拦截分支漏了也不会被 CLI 当命令执行)
        assert_eq!(
            slash_command_text("/summon blue-wing", &[], CliDriver::Codex),
            None
        );
        assert_eq!(
            slash_command_text("/introduce @botA @botB", &[], CliDriver::Codex),
            None
        );
        assert_eq!(slash_command_text("/session", &[], CliDriver::Codex), None);
        assert_eq!(slash_command_text("/cd /tmp", &[], CliDriver::Codex), None);
        assert_eq!(
            slash_command_text("/schedules", &[], CliDriver::Codex),
            None
        );
        assert_eq!(
            slash_command_text("/remind 10m 喝水", &[], CliDriver::Codex),
            None
        );
        assert_eq!(slash_command_text("/bridge", &[], CliDriver::Codex), None);
        assert_eq!(
            slash_command_text("/model opus", &[], CliDriver::Codex),
            None
        );
        assert_eq!(slash_command_text("/fast on", &[], CliDriver::Codex), None);
        assert_eq!(
            slash_command_text("/mode plan", &[], CliDriver::Codex),
            None
        );
    }

    #[test]
    fn model_command_is_persistent_control_not_slash_passthrough() {
        let bot = vec![Mention {
            open_id: "ou_bot".into(),
            key: "@_user_1".into(),
            name: "蓝翼飞龙".into(),
            ..Default::default()
        }];
        assert_eq!(
            model_command("@_user_1 /model claude-opus-5-thinking-xhigh", &bot),
            Some("claude-opus-5-thinking-xhigh".into())
        );
        assert_eq!(model_command("/model", &[]), Some(String::new()));
        assert_eq!(model_command("/models", &[]), None);
        assert_eq!(
            effort_command("/effort default high", &[]).as_deref(),
            Some("default high")
        );
        assert_eq!(effort_command("/effort", &[]).as_deref(), Some(""));
        assert_eq!(effort_command("/efforts", &[]), None);
    }

    /// `/fast on|off` 是 Codex / Cursor 的 summon 控制命令。参数必须完整、唯一；任何缺失或
    /// 多余参数都只返回 Invalid，由 dispatcher 回用法提示，绝不能猜测后改写额度档位。
    #[test]
    fn fast_command_parses_explicit_modes_and_rejects_ambiguous_forms() {
        let bot = vec![Mention {
            open_id: "ou_bot".into(),
            key: "@_user_1".into(),
            name: "蓝翼飞龙".into(),
            ..Default::default()
        }];
        for cli in [CliDriver::Codex, CliDriver::Cursor, CliDriver::Traex] {
            assert_eq!(fast_command("/fast on", &[], cli), Some(FastCommand::On));
            assert_eq!(
                fast_command("@_user_1 /FAST OFF", &bot, cli),
                Some(FastCommand::Off),
                "群里 @bot 后应剥离 mention，且命令大小写不敏感"
            );
            for invalid in ["/fast", "/fast maybe", "/fast on now"] {
                assert_eq!(
                    fast_command(invalid, &[], cli),
                    Some(FastCommand::Invalid),
                    "明确以 /fast 开头的错误形态必须被拦截"
                );
            }
            assert_eq!(fast_command("/faston", &[], cli), None);
            assert_eq!(fast_command("帮我 /fast on", &[], cli), None);
        }
    }

    /// Claude Code 自带 `/fast [on|off]`：任何形态都不归 summon，交给 slash 透传。
    #[test]
    fn fast_command_leaves_claude_native_fast_to_passthrough() {
        let bot = vec![Mention {
            open_id: "ou_bot".into(),
            key: "@_user_1".into(),
            name: "蓝翼飞龙".into(),
            ..Default::default()
        }];
        for text in [
            "/fast",
            "/fast on",
            "/fast off",
            "/fast maybe",
            "@_user_1 /FAST ON",
        ] {
            assert_eq!(fast_command(text, &bot, CliDriver::Claude), None, "{text}");
            assert_eq!(
                summon_owned_command(text, &bot, CliDriver::Claude),
                None,
                "兜底也不能把 Claude 原生 /fast 当 summon 命令回用法: {text}"
            );
        }
        assert_eq!(
            slash_command_text("@_user_1 /fast on", &bot, CliDriver::Claude),
            Some("/fast on".to_string()),
            "Claude 上 /fast 必须作为 provider slash 透传"
        );
        assert_eq!(
            slash_command_text("/fast", &[], CliDriver::Claude),
            Some("/fast".to_string())
        );
        // 出厂黑名单下由规划器放行,走通用执行器注入 TUI
        let line = slash_command_text("/fast on", &[], CliDriver::Claude).unwrap();
        assert_eq!(
            crate::driver::slash_pool::plan_slash(
                CliDriver::Claude,
                crate::config::registry::CaptureMode::Transcript,
                &line,
                &crate::config::builtin_messaging().slash_blacklist,
                &crate::driver::slash_pool::SlashToggles::default(),
            ),
            crate::driver::slash_pool::SlashPlan::Generic { line }
        );
        // Claude 上只有 /fast 归 provider,其余 summon 命令归 summon
        assert_eq!(
            slash_command_text("/model opus", &[], CliDriver::Claude),
            None
        );
        assert_eq!(
            summon_owned_command("/mode plan", &[], CliDriver::Claude),
            Some("/mode")
        );
    }

    #[test]
    fn mode_command_parses_mentioned_dynamic_id_and_rejects_ambiguous_forms() {
        let bot = vec![Mention {
            open_id: "ou_bot".into(),
            key: "@_user_1".into(),
            name: "蓝翼飞龙".into(),
            ..Default::default()
        }];
        assert_eq!(
            mode_command("@_user_1 /MODE plan", &bot),
            Some(ModeCommand::Set("plan".parse().unwrap()))
        );
        assert_eq!(
            mode_command("/mode review-v2", &[]),
            Some(ModeCommand::Set("review-v2".parse().unwrap()))
        );
        for invalid in ["/mode", "/mode ask now"] {
            assert_eq!(mode_command(invalid, &[]), Some(ModeCommand::Invalid));
        }
        assert_eq!(mode_command("/modes", &[]), None);
        assert_eq!(mode_command("帮我 /mode plan", &[]), None);
    }

    /// 兜底识别:命令词命中但形态没被专门解析器认出时,dispatcher 据此回用法提示,
    /// 而不是让 `/session 看看` 这种静默滑到 agent 面前被当闲聊。
    #[test]
    fn summon_owned_command_detects_head_only() {
        let bot = vec![Mention {
            open_id: "ou_bot".into(),
            key: "@_user_1".into(),
            name: "蓝翼飞龙".into(),
            ..Default::default()
        }];
        let cli = CliDriver::Codex;
        assert_eq!(
            summon_owned_command("/session 看看", &[], cli),
            Some("/session")
        );
        assert_eq!(
            summon_owned_command("@_user_1 /REMIND 乱写的", &bot, cli),
            Some("/remind")
        );
        assert_eq!(
            summon_owned_command("/bridge page x", &[], cli),
            Some("/bridge")
        );
        assert_eq!(summon_owned_command("/fast maybe", &[], cli), Some("/fast"));
        // 非 summon 命令(交给 CLI 透传)与普通正文都不命中
        assert_eq!(summon_owned_command("/compact", &[], cli), None);
        assert_eq!(summon_owned_command("看下 /session", &[], cli), None);
        assert_eq!(summon_owned_command("你好", &[], cli), None);
    }

    #[test]
    fn render_mentions_shows_name_only_no_open_id() {
        // @ 只显示名字,不带 open_id(per-app 不稳定,回 @ 由出站层解析)
        let ms = vec![
            Mention {
                open_id: "ou_shika".into(),
                key: "@_user_1".into(),
                name: "Starlight".into(),
                ..Default::default()
            },
            Mention {
                open_id: "ou_blue".into(),
                key: "@_user_2".into(),
                name: "blue-wing".into(),
                ..Default::default()
            },
        ];
        let text = "你们俩互相认识下，@_user_1是我的参谋， @_user_2 是干活好手";
        assert_eq!(
            render_mentions(text, &ms),
            "你们俩互相认识下，@Starlight是我的参谋， @blue-wing 是干活好手"
        );
    }

    #[test]
    fn strip_mention_by_open_id_removes_only_target_bot() {
        let ms = vec![
            Mention {
                open_id: "ou_bot".into(),
                key: "@_user_1".into(),
                name: "蓝翼飞龙".into(),
                ..Default::default()
            },
            Mention {
                open_id: "ou_human".into(),
                key: "@_user_2".into(),
                name: "老板".into(),
                ..Default::default()
            },
        ];
        let stripped = strip_mention_by_open_id("@_user_1 !立刻问 @_user_2", &ms, "ou_bot");
        assert_eq!(stripped, "!立刻问 @_user_2");
        // render_mentions 不再带 open_id
        assert_eq!(render_mentions(&stripped, &ms), "!立刻问 @老板");
    }

    #[test]
    fn render_mentions_degrades_and_skips_gracefully() {
        // 缺名只剩 open_id;缺 open_id 只剩名;都缺则删占位;无 key 跳过不动别处
        let ms = vec![
            Mention {
                open_id: "ou_x".into(),
                key: "@_user_1".into(),
                name: "".into(),
                ..Default::default()
            },
            Mention {
                open_id: "".into(),
                key: "@_user_2".into(),
                name: "无名氏".into(),
                ..Default::default()
            },
            Mention {
                open_id: "".into(),
                key: "@_user_3".into(),
                name: "".into(),
                ..Default::default()
            },
            Mention {
                open_id: "ou_ignored".into(),
                key: "".into(),
                name: "无 key".into(),
                ..Default::default()
            },
        ];
        assert_eq!(
            render_mentions("a@_user_1 b@_user_2 c@_user_3 d", &ms),
            "a@ou_x b@无名氏 c d"
        );
        // 无 mention 时 collapse + trim
        assert_eq!(render_mentions("  hi  ", &[]), "hi");
    }

    #[test]
    fn collapse_spaces_squashes_runs_and_trims() {
        assert_eq!(collapse_spaces("a  b   c"), "a b c");
        assert_eq!(collapse_spaces("  hi  there  "), "hi there");
        assert_eq!(collapse_spaces("\t\n  multi   space"), "multi space");
        assert_eq!(collapse_spaces(""), "");
    }

    #[test]
    fn is_leading_mention_detects_prefix_position() {
        let ms = vec![Mention {
            open_id: "ou_me".into(),
            key: "@_user_1".into(),
            name: "我".into(),
            ..Default::default()
        }];
        // 开头 → true
        assert!(is_leading_mention("@_user_1 帮我看看", &ms, "ou_me"));
        // 带前导空白也算开头
        assert!(is_leading_mention("  @_user_1 帮我", &ms, "ou_me"));
        // 非开头 → false
        assert!(!is_leading_mention("帮我 @_user_1 看看", &ms, "ou_me"));
        // open_id 不匹配 → false
        assert!(!is_leading_mention("@_user_1 帮我", &ms, "ou_other"));
        // 空 open_id → false
        assert!(!is_leading_mention("@_user_1 帮我", &ms, ""));
        assert!(
            !is_leading_mention("@_user_10 /quit", &ms, "ou_me"),
            "self key 只是另一个 mention 占位符的前缀时不能误判为定向到自己"
        );
    }

    #[test]
    fn strip_leading_mention_only_strips_prefix_self() {
        let ms = vec![
            Mention {
                open_id: "ou_me".into(),
                key: "@_user_1".into(),
                name: "我".into(),
                ..Default::default()
            },
            Mention {
                open_id: "ou_other".into(),
                key: "@_user_2".into(),
                name: "蓝翼飞龙".into(),
                ..Default::default()
            },
        ];
        // self 在开头 → 剥离 + 返回 true
        let (text, was) =
            strip_leading_mention_by_open_id("@_user_1 帮我 @_user_2 看看", &ms, "ou_me");
        assert!(was);
        assert_eq!(text, "帮我 @_user_2 看看");
        // self 不在开头 → 不剥离 + 返回 false
        let (text2, was2) =
            strip_leading_mention_by_open_id("帮我 @_user_1 看看 @_user_2", &ms, "ou_me");
        assert!(!was2);
        assert_eq!(text2, "帮我 @_user_1 看看 @_user_2");
        // 剥离后 collapse 双空格
        let (text3, was3) =
            strip_leading_mention_by_open_id("@_user_1  @_user_2 帮忙", &ms, "ou_me");
        assert!(was3);
        assert_eq!(text3, "@_user_2 帮忙");
        let (text4, was4) = strip_leading_mention_by_open_id("@_user_10 /quit", &ms, "ou_me");
        assert!(!was4);
        assert_eq!(text4, "@_user_10 /quit");
    }

    #[test]
    fn card_action_roundtrips() {
        let line = r#"{"type":"card_action","app_id":"a","chat_id":"c","message_id":"m","operator_open_id":"ou_1","event_id":"evt-1","value":{"kind":"ask","ask":"k1","opt":"2"}}"#;
        let ev = Event::from_line(line).unwrap();
        match ev {
            Event::CardAction {
                app_id,
                chat_id,
                message_id,
                operator_open_id,
                event_id,
                value,
            } => {
                assert_eq!(
                    (app_id.as_str(), chat_id.as_str(), message_id.as_str()),
                    ("a", "c", "m")
                );
                assert_eq!(
                    (operator_open_id.as_str(), event_id.as_str()),
                    ("ou_1", "evt-1")
                );
                assert_eq!(value["kind"], "ask");
                assert_eq!(value["ask"], "k1");
                assert_eq!(value["opt"], "2");
            }
            _ => panic!("expected card_action"),
        }
    }

    #[test]
    fn message_allows_missing_optional_fields() {
        let ev = Event::from_line(
            r#"{"type":"message","app_id":"a","chat_id":"c","sender":"s","text":"t","message_id":"m"}"#,
        )
        .unwrap();
        match ev {
            Event::Message {
                ts,
                thread_id,
                create_time_ms,
                ..
            } => {
                assert_eq!(ts, 0);
                assert!(thread_id.is_none());
                // 老 sidecar 不带 create_time_ms → 默认 0(未知),向后兼容。
                assert_eq!(create_time_ms, 0);
            }
            _ => panic!("expected message"),
        }
    }

    #[test]
    fn message_parses_create_time_ms() {
        let ev = Event::from_line(
            r#"{"type":"message","app_id":"a","chat_id":"c","sender":"s","text":"t","message_id":"m","create_time_ms":1717000000000}"#,
        )
        .unwrap();
        match ev {
            Event::Message { create_time_ms, .. } => assert_eq!(create_time_ms, 1_717_000_000_000),
            _ => panic!("expected message"),
        }
    }

    #[test]
    fn schedule_command_matching() {
        use chrono::{Local, TimeZone};
        let now = Local.with_ymd_and_hms(2026, 6, 14, 12, 0, 0).unwrap();
        assert!(is_schedule_command("/schedules", &[]));
        assert!(is_schedule_command("  /schedules  ", &[]));
        // 带正文仍命中(首 token 判定),不再漏成普通消息
        assert!(is_schedule_command("/schedules 明天的", &[]));
        assert!(!is_schedule_command("/schedule x", &[]));
        assert!(!is_schedule_command("看下 /schedules", &[]));
        // /remind 解析:吃掉 when token,余下为 prompt
        let r = parse_remind("/remind every 2h 巡检 CI", &[], now)
            .unwrap()
            .unwrap();
        assert_eq!(r.kind, crate::schedule::Kind::Interval);
        assert_eq!(r.spec, "7200");
        assert_eq!(r.prompt, "巡检 CI");
        assert!(parse_remind("/remind in 30m", &[], now).is_err()); // 缺 prompt
        assert!(parse_remind("/cd x", &[], now).unwrap().is_none()); // 非 /remind → Ok(None)

        // 群里 `@Bot /remind …`:必须先抹 @ 占位再解析,否则整条降级成普通消息,
        // 群里的 /remind 永远进不了定时器(与 /introduce 同类的漏判)。
        let bot = vec![Mention {
            open_id: "ou_bot".into(),
            key: "@_user_1".into(),
            name: "蓝翼飞龙".into(),
            ..Default::default()
        }];
        let grouped = parse_remind("@_user_1 /remind in 10m 喝水", &bot, now)
            .unwrap()
            .expect("群里 @bot 的 /remind 应能解析");
        assert_eq!(grouped.prompt, "喝水");
    }
}
