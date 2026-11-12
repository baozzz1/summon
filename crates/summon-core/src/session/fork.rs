//! 跨群 fork 的**纯判据**:一次 fork 请求合不合法、副本能不能生出来、驳回时说什么。
//!
//! 这里一个字节都不碰文件系统、不起进程、不发消息 —— 判据与副作用分开,是为了让「什么情况下
//! 拒绝 fork」这件事能被单测穷举。真正的落盘与 provider 启动在 `summon` crate。
//!
//! **fork 是 copy 不是 move**:源会话读完即止,一个字节都不动。这是它与 Bridge handoff
//! (`daemon/bridge.rs` 的 `rekey_idle_exact` / `transfer_exact`)的根本差别 —— 后者搬走所有权,
//! 所以要 detach 源 worker、证明静默、失败回滚;fork 全都不需要。

use crate::driver::CliDriver;

/// fork 请求被驳回的原因。每一条都对应一条不变量,而不是「顺手加的校验」。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForkRefusal {
    /// 源会话没有 resume 记录 —— 没有历史可复制。
    SourceHasNoSession,
    /// 源记录损坏或读不出来。fail-closed:读不准就不 fork,而不是当成「没历史」去冷启。
    SourceUnreadable,
    /// 源与目标是同一个 chat。fork 到自己身上没有意义,而且会撞目标必须空闲那条闸。
    SameChat,
    /// 目标 chat 已有会话。覆盖等于把别人正在聊的上下文销毁掉,不可逆。
    TargetBusy { cli: String, sid: String },
    /// 目标 chat 从没跟这个 bot 说过话(没有 `.meta`)—— 证明不了 bot 在那个群、发得出消息。
    TargetUnknown,
    /// 该 provider 没有 fork 原语。
    ProviderCannotFork { cli: String },
    /// 源会话正处于 strict resume(Bridge handoff 的 exact owner)。
    SourceIsStrictResume,
    /// 源会话自己的 fork 副本还没生出来 —— 从一个空壳再 fork 只会得到另一个空壳。
    SourceForkStillPending,
    /// 目标是私聊。只支持群 —— 见 `ForkRefusal::message` 里的理由。
    TargetIsNotAGroup,
}

impl ForkRefusal {
    /// 给用户看的驳回理由。**必须说清下一步能做什么** —— 只说「不行」会让人反复重试同一条命令。
    pub fn message(&self) -> String {
        match self {
            Self::SourceHasNoSession => summon_i18n::text!("r9b8f9f5216").to_string(),
            Self::SourceUnreadable => summon_i18n::text!("r7bf22b8536").to_string(),
            Self::SameChat => summon_i18n::text!("redafc63eb0").to_string(),
            Self::TargetBusy { cli, sid } => summon_i18n::format!("r1d3098b457"),
            Self::TargetUnknown => summon_i18n::text!("r3f1c4d13f7").to_string(),
            Self::ProviderCannotFork { cli } => summon_i18n::format!("re8ec758ef9"),
            Self::SourceIsStrictResume => summon_i18n::text!("r55fafd7fbe").to_string(),
            Self::SourceForkStillPending => summon_i18n::text!("rdf837e0ec7").to_string(),
            Self::TargetIsNotAGroup => summon_i18n::text!("rc53dee3de9").to_string(),
        }
    }
}

/// provider 能不能 fork 出一份带新 id 的历史副本。
///
/// - claude:`--resume <父> --fork-session` 配预铸 `--session-id`。真机已验(2026-09-03):
///   新 sid 被钉死、子转录完整继承父历史、**父文件字节与 mtime 未变**。
/// - codex / traex:`thread/fork`,同协议同构。
/// - cursor:**没有**。ACP 只有 `session/new` / `session/load`,历史在 `store.db`(SQLite),
///   复制得连 workspace hash 映射一起动。这里返回 false 让上层 fail-closed —— 绝不降级成
///   「带一段摘要冷启」,那会让人以为 fork 成功了,然后对着一个其实没有上下文的会话继续聊。
/// - Other:未知 CLI,没有可依赖的 fork 语义。
pub fn provider_can_fork(driver: CliDriver) -> bool {
    match driver {
        CliDriver::Claude | CliDriver::Codex | CliDriver::Traex => true,
        CliDriver::Cursor | CliDriver::Other => false,
    }
}

/// 源会话此刻的样子(调用方从 `.session` 读出来后填进来)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForkSourceState {
    /// 有一条可用记录。
    Ready {
        cli: String,
        sid: String,
        strict_resume: bool,
        fork_pending: bool,
    },
    /// 没有记录 —— 这个 chat 一轮都没跑过。
    Missing,
    /// 记录损坏 / 读不出来。
    Unreadable,
}

/// 目标会话此刻的样子。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForkTargetState {
    /// 空闲:没有 resume 记录,且 `.meta` 证明 bot 在那个群待过。
    Free,
    /// 已有会话。
    Occupied { cli: String, sid: String },
    /// 没有 `.meta` —— bot 没在那个群露过面。
    NeverSeen,
    /// 目标是私聊,不是群。
    ///
    /// **判据放进纯件而不是留在调用点**:owner 私聊用裸名 `soul-<alias>`,而「这个私聊是不是
    /// owner 的」只有 dispatcher 算得出(依赖运行期状态)。调用点自己拼会话名就绕过了这道拒绝,
    /// 而那种绕过用源码 grep 抓不住 —— 只有让它成为 `evaluate_fork` 的一个返回值,
    /// 「有没有走这道闸」才是可以被行为测试证伪的事实。
    NotAGroup,
}

/// 发起侧判据:**本会话的上下文能不能被复制**。
///
/// 与 [`evaluate_fork`] 的分工:那条是兑现时用的、要判目标;这条在发起时用,而那一刻目标群可能
/// 还没建出来 —— 所以与目标相关的判据(必须是群、必须空闲、必须跟 bot 说过话)全部推迟到兑现。
///
/// 这个推迟有代价,如实记着:用户可能拿着一段注定兑现失败的短码去建群,到那边才第一次看到
/// 拒绝理由。换来的是「fork 到还不存在的群」这个能力 —— 没有它,那件事根本做不到。
pub fn evaluate_fork_source(
    source_tag: &str,
    driver: CliDriver,
    source: &ForkSourceState,
) -> Result<ForkPlan, ForkRefusal> {
    if !provider_can_fork(driver) {
        return Err(ForkRefusal::ProviderCannotFork {
            cli: driver_name(driver).to_string(),
        });
    }
    let (cli, sid) = match source {
        ForkSourceState::Missing => return Err(ForkRefusal::SourceHasNoSession),
        ForkSourceState::Unreadable => return Err(ForkRefusal::SourceUnreadable),
        ForkSourceState::Ready {
            strict_resume: true,
            ..
        } => return Err(ForkRefusal::SourceIsStrictResume),
        ForkSourceState::Ready {
            fork_pending: true, ..
        } => return Err(ForkRefusal::SourceForkStillPending),
        ForkSourceState::Ready { cli, sid, .. } => (cli.clone(), sid.clone()),
    };
    Ok(ForkPlan {
        source_tag: source_tag.to_string(),
        // 目标在发起时未知,兑现那一刻才填。
        target_tag: String::new(),
        cli,
        source_sid: sid,
    })
}

/// 带了目标参数时的指路。
///
/// 旧形态要在发起时指定目标群;现在不需要了 —— 无参 `/fork` 就发码,fork 到哪由「码被贴进哪个
/// 群」回答。这条提示存在的理由是别让人以为命令没生效。
pub fn no_target_needed_hint() -> String {
    summon_i18n::text!("rfe59f75f5e").to_string()
}

/// 一次 fork 请求过不过。**判据顺序即报错优先级**,挑最能指导下一步的那条先报。
///
/// 注意这里**不做鉴权**:双向 operate-gate 由调用方在 `can_operate` 那个单点判,本函数只答
/// 「这次 fork 技术上成不成立」。两件事混在一起会让鉴权判据散开。
pub fn evaluate_fork(
    source_tag: &str,
    target_tag: &str,
    driver: CliDriver,
    source: &ForkSourceState,
    target: &ForkTargetState,
) -> Result<ForkPlan, ForkRefusal> {
    if source_tag == target_tag {
        return Err(ForkRefusal::SameChat);
    }
    // provider 能力先判:它与源/目标状态无关,而且是最不可能被用户绕过的一条 —— 先说清
    // 「这个 CLI 压根不支持」,比让人先去 reset 目标群再被拒有用。
    if !provider_can_fork(driver) {
        return Err(ForkRefusal::ProviderCannotFork {
            cli: driver_name(driver).to_string(),
        });
    }
    let (cli, sid) = match source {
        ForkSourceState::Missing => return Err(ForkRefusal::SourceHasNoSession),
        ForkSourceState::Unreadable => return Err(ForkRefusal::SourceUnreadable),
        ForkSourceState::Ready {
            strict_resume: true,
            ..
        } => return Err(ForkRefusal::SourceIsStrictResume),
        ForkSourceState::Ready {
            fork_pending: true, ..
        } => return Err(ForkRefusal::SourceForkStillPending),
        ForkSourceState::Ready { cli, sid, .. } => (cli.clone(), sid.clone()),
    };
    match target {
        ForkTargetState::NotAGroup => return Err(ForkRefusal::TargetIsNotAGroup),
        ForkTargetState::NeverSeen => return Err(ForkRefusal::TargetUnknown),
        ForkTargetState::Occupied {
            cli: tcli,
            sid: tsid,
        } => {
            return Err(ForkRefusal::TargetBusy {
                cli: tcli.clone(),
                sid: tsid.clone(),
            })
        }
        ForkTargetState::Free => {}
    }
    Ok(ForkPlan {
        source_tag: source_tag.to_string(),
        target_tag: target_tag.to_string(),
        cli,
        source_sid: sid,
    })
}

/// 通过判据的 fork 计划。落盘所需的一切都在里面,调用方不必再回头读可能已经变了的盘。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForkPlan {
    pub source_tag: String,
    pub target_tag: String,
    /// resume 记录里的 `cli` 值(通道名,不是 CLI 名 —— cursor RPC 记的是 `cursor-acp`)。
    /// 目标记录必须沿用同一个值,否则读取侧的 kind guard 会把它判成「换过 CLI 的作废 id」。
    pub cli: String,
    pub source_sid: String,
}

fn driver_name(driver: CliDriver) -> &'static str {
    match driver {
        CliDriver::Claude => "claude",
        CliDriver::Cursor => "cursor",
        CliDriver::Codex => "codex",
        CliDriver::Traex => "traex",
        CliDriver::Other => summon_i18n::text!("rba5582ede4"),
    }
}

/// 兑现一封邀请函之前的最后一道判据:**源会话还是发邀请时那一条吗?**
///
/// 源群在这十分钟里可能 reset 过。那时它的 sid 已经换了,历史也不是邀请函背书的那一份 ——
/// 照旧 fork 会把一段别的上下文接进目标群,而回执说的是「已接上「甲群」的上下文副本」。
///
/// **抽成纯函数是为了让这条判据可证伪。** 它此前写在编排层的一个 `if` 里,而那层依赖飞书与
/// tmux、进不了单测,只能靠源码 grep 守 —— 而 grep 挡不住 `&& false` 这种语义中和:
/// 字符串还在,守卫照绿,判据已经没了。
pub fn source_still_matches_invite(
    invited_sid: &str,
    current_sid: &str,
    source_name: &str,
) -> Result<(), String> {
    if invited_sid == current_sid {
        return Ok(());
    }
    Err(summon_i18n::format!("ra9f07aeff5"))
}

/// 一封邀请函的**短码**:目标群凭它接受,不必打源群的名字。
///
/// 由 `(源 tag, 目标 tag, 发出时刻)` 派生 —— 同一对群先后两次发起会得到不同短码,所以「上一封
/// 的短码」不会误兑现这一封。
///
/// **只需在「指向本群的未过期邀请」这个集合里唯一**,不追求全局唯一:那个集合通常只有一两封,
/// 4 个 hex 已经够;真撞上了由调用方拒绝并让源群重发,**绝不静默挑一封** —— 猜错就是接上了
/// 另一个群的上下文。
///
/// 它**不是凭据**。短码只在指向本群的邀请里查得到,所以它泄漏出去也换不来别的群的上下文;
/// 授权仍然全靠双向 operate-gate。
pub fn invite_code(source_tag: &str, target_tag: &str, issued_at: i64) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in source_tag
        .bytes()
        .chain(target_tag.bytes())
        .chain(issued_at.to_le_bytes())
    {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{:04x}", (h & 0xffff) as u16)
}

/// 从目标群那条 `/fork <目标>` 的参数里认短码。
///
/// 只认 `#` 开头 + 全 hex 的形态,与序号(`/fork 3`)和名字片段明确分开。给别的东西返回 None,
/// 调用方据此报「请用邀请回执里那个短码」——**目标群那条只认短码**:它要指认的是「哪一封邀请」,
/// 而群名指认的是「哪个群」,后者在这一侧既多余(邀请自带来源)又容易打错。
pub fn parse_invite_code(arg: &str) -> Option<String> {
    let code = arg.trim().strip_prefix('#')?.trim();
    let lowered = code.to_ascii_lowercase();
    (!lowered.is_empty() && lowered.chars().all(|c| c.is_ascii_hexdigit())).then_some(lowered)
}

/// 邀请函的有效期(秒)。短是刻意的:它授权的是「把这个群的对话带去别处」,不该长期挂着 ——
/// 一周前那句 `/fork` 不能还在替今天的目标群背书。
///
/// 十分钟够两个群的人一来一回,不够让一条被遗忘的邀请在下个话题里生效。
pub const INVITE_TTL_SECS: i64 = 600;

/// 源群 operator 发起后给源群的回执。
///
/// 必须说清**下一步在哪做** —— 双向授权的另一半在目标群,发起人以为「发完就完了」的话,
/// 那封邀请会静静过期,而没有任何人知道为什么没生效。
///
/// **不提目标群的名字。** 这条回执发在源群里、群里所有人可见,而「这个 bot 还在另一个叫 X 的群」
/// 对源群的其他人既不需要也不该知道。发起人自己清楚他要发去哪(序号是他刚打的),短码也已经把
/// 「哪一封邀请」指认清楚了 —— 群名在这一侧是纯多余的暴露面。
///
/// 目标群那侧的 [`fork_receipt`] 相反,**必须**说出源群名:那个群即将接上一整段别处的对话,
/// 不告诉它来源等于让那边的人对着一段来历不明的上下文说话。两侧的知情前提不同,所以不对称。
pub fn invite_receipt(code: &str) -> String {
    summon_i18n::format!("r1b23c5e84e", INVITE_TTL_SECS / 60)
}

/// 短码查无此项(打错、已过期、或已经兑现过一次)。
pub fn code_not_found() -> String {
    summon_i18n::text!("r6be61fff8f").to_string()
}

/// 短码存在,但兑现的人不是发起人。
///
/// 这是非定向短码唯一的边界:邀请不指定目标群(那是「fork 到还不存在的群」的前提),所以
/// 「谁能兑」必须收紧到发起人本人。少了它,源群任何成员看到回执即可把整段上下文接进自己
/// 新建的群 —— 而 operate-gate 是本仓破坏性动作的统一鉴权,不能被一段可转发的字符串绕过。
pub fn code_belongs_to_someone_else() -> String {
    summon_i18n::text!("r7444174ee9").to_string()
}

/// fork 成功后给目标群的回执正文。
///
/// **截止点必须出现在这里**:副本只含 fork 那一刻源会话已落盘的部分,源群此刻正在跑的那一轮
/// 不在内。不说的话,目标群的人会以为自己拿到的是源群的全部,然后对着一段缺尾的上下文做判断。
pub fn fork_receipt(source_name: &str, at: &str, cwd: &str) -> String {
    summon_i18n::format!("r639ff85f1f")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ready(sid: &str) -> ForkSourceState {
        ForkSourceState::Ready {
            cli: "claude".into(),
            sid: sid.into(),
            strict_resume: false,
            fork_pending: false,
        }
    }

    #[test]
    fn a_healthy_request_yields_a_plan_carrying_the_source_sid() {
        let plan = evaluate_fork(
            "gsrc",
            "gdst",
            CliDriver::Claude,
            &ready("sid-1"),
            &ForkTargetState::Free,
        )
        .expect("这条请求该通过");
        assert_eq!(plan.source_sid, "sid-1");
        assert_eq!(plan.cli, "claude");
        assert_eq!(plan.target_tag, "gdst");
    }

    #[test]
    fn cursor_is_refused_instead_of_degraded() {
        // cursor 没有 fork 原语。降级成「带摘要冷启」会让人以为 fork 成功了 —— 那比报错糟得多。
        let refusal = evaluate_fork(
            "gsrc",
            "gdst",
            CliDriver::Cursor,
            &ready("sid-1"),
            &ForkTargetState::Free,
        )
        .expect_err("cursor 必须被拒");
        assert!(matches!(refusal, ForkRefusal::ProviderCannotFork { .. }));
        assert!(refusal.message().contains("claude"), "驳回要说清谁能 fork");
    }

    #[test]
    fn an_unknown_cli_is_refused_too() {
        assert!(matches!(
            evaluate_fork(
                "gsrc",
                "gdst",
                CliDriver::Other,
                &ready("sid-1"),
                &ForkTargetState::Free
            ),
            Err(ForkRefusal::ProviderCannotFork { .. })
        ));
    }

    #[test]
    fn codex_and_traex_can_fork_because_they_share_the_app_server_protocol() {
        for driver in [CliDriver::Codex, CliDriver::Traex] {
            assert!(provider_can_fork(driver), "driver={driver:?}");
        }
    }

    #[test]
    fn an_occupied_target_is_refused_and_the_message_names_the_way_out() {
        let refusal = evaluate_fork(
            "gsrc",
            "gdst",
            CliDriver::Claude,
            &ready("sid-1"),
            &ForkTargetState::Occupied {
                cli: "claude".into(),
                sid: "other".into(),
            },
        )
        .expect_err("占用的目标必须被拒");
        // 覆盖目标会话是不可逆的信息销毁,所以驳回必须指出 reset 这条出路。
        assert!(refusal.message().contains("reset"), "{}", refusal.message());
    }

    /// 私聊目标必须在**判据层**被拒,而不是靠调用点自觉。
    ///
    /// owner 私聊用裸名 `soul-<alias>`,那条记录与 `summon agents attach` 共享;而「这个私聊
    /// 是不是 owner 的」只有 dispatcher 算得出。调用点自己拼会话名就绕过了拒绝,且那种绕过
    /// 用源码 grep 抓不住 —— 所以它是 `evaluate_fork` 的一个返回值,可以被行为测试钉死。
    #[test]
    fn a_private_chat_can_never_be_a_fork_target() {
        assert_eq!(
            evaluate_fork(
                "gsrc",
                "pdst",
                CliDriver::Claude,
                &ready("sid-1"),
                &ForkTargetState::NotAGroup
            ),
            Err(ForkRefusal::TargetIsNotAGroup)
        );
        let message = ForkRefusal::TargetIsNotAGroup.message();
        assert!(message.contains("群"), "{message}");
        assert!(message.contains("attach"), "驳回要说清为什么:{message}");
    }

    #[test]
    fn a_target_that_never_talked_to_the_bot_is_refused() {
        assert_eq!(
            evaluate_fork(
                "gsrc",
                "gdst",
                CliDriver::Claude,
                &ready("sid-1"),
                &ForkTargetState::NeverSeen
            ),
            Err(ForkRefusal::TargetUnknown)
        );
    }

    #[test]
    fn an_unreadable_source_is_refused_rather_than_treated_as_empty() {
        // 把「读不出来」折成「没有历史」会让 fork 变成静默冷启,目标群拿到空会话却以为带着上下文。
        assert_eq!(
            evaluate_fork(
                "gsrc",
                "gdst",
                CliDriver::Claude,
                &ForkSourceState::Unreadable,
                &ForkTargetState::Free
            ),
            Err(ForkRefusal::SourceUnreadable)
        );
    }

    #[test]
    fn a_source_without_history_is_refused() {
        assert_eq!(
            evaluate_fork(
                "gsrc",
                "gdst",
                CliDriver::Claude,
                &ForkSourceState::Missing,
                &ForkTargetState::Free
            ),
            Err(ForkRefusal::SourceHasNoSession)
        );
    }

    #[test]
    fn a_strict_resume_source_is_left_alone() {
        // strict 位意味着 Bridge 正把这条会话当 exact owner。fork 只读源,但读的那一刻它可能
        // 正在被接管迁移,拿到的 sid 未必还是权威 —— 这种状态下不碰。
        let source = ForkSourceState::Ready {
            cli: "claude".into(),
            sid: "sid-1".into(),
            strict_resume: true,
            fork_pending: false,
        };
        assert_eq!(
            evaluate_fork(
                "gsrc",
                "gdst",
                CliDriver::Claude,
                &source,
                &ForkTargetState::Free
            ),
            Err(ForkRefusal::SourceIsStrictResume)
        );
    }

    #[test]
    fn forking_from_a_still_pending_fork_is_refused() {
        // 源自己的副本还没生出来,它的 sid 背后现在什么都没有。从它 fork 只会得到另一个空壳。
        let source = ForkSourceState::Ready {
            cli: "claude".into(),
            sid: "sid-1".into(),
            strict_resume: false,
            fork_pending: true,
        };
        assert_eq!(
            evaluate_fork(
                "gsrc",
                "gdst",
                CliDriver::Claude,
                &source,
                &ForkTargetState::Free
            ),
            Err(ForkRefusal::SourceForkStillPending)
        );
    }

    #[test]
    fn forking_a_chat_into_itself_is_refused() {
        assert_eq!(
            evaluate_fork(
                "gsame",
                "gsame",
                CliDriver::Claude,
                &ready("sid-1"),
                &ForkTargetState::Free
            ),
            Err(ForkRefusal::SameChat)
        );
    }

    #[test]
    fn the_plan_keeps_the_channel_name_verbatim() {
        // `cli` 是通道名不是 CLI 名(cursor RPC 记 `cursor-acp`)。目标记录换个值写,
        // 读取侧的 kind guard 会把它判成「换过 CLI 的作废 id」,resume 直接失效。
        let source = ForkSourceState::Ready {
            cli: "traex".into(),
            sid: "th-7".into(),
            strict_resume: false,
            fork_pending: false,
        };
        let plan = evaluate_fork(
            "gsrc",
            "gdst",
            CliDriver::Traex,
            &source,
            &ForkTargetState::Free,
        )
        .expect("traex 该通过");
        assert_eq!(plan.cli, "traex");
    }

    #[test]
    fn the_receipt_states_the_cutoff_and_that_the_source_is_untouched() {
        let text = fork_receipt("sample开发群", "2026-09-03 11:52:30", "/repo");
        assert!(text.contains("2026-09-03 11:52:30"), "{text}");
        assert!(text.contains("源群"), "{text}");
        // 工作目录被换掉了,而那是用户可能用 `/cd` 显式设过的东西 —— 不说等于偷偷改。
        assert!(text.contains("/repo"), "回执要说清工作目录切到了哪:{text}");
    }

    #[test]
    fn redeeming_requires_the_source_session_to_be_the_invited_one() {
        assert!(source_still_matches_invite("sid-1", "sid-1", "甲群").is_ok());
        let refusal =
            source_still_matches_invite("sid-1", "sid-2", "甲群").expect_err("源会话换过了必须拒");
        // 驳回要说清「换过了」以及两个 sid,否则用户不知道为什么被拒、也不知道下一步做什么。
        assert!(refusal.contains("甲群"), "{refusal}");
        assert!(
            refusal.contains("sid-1") && refusal.contains("sid-2"),
            "{refusal}"
        );
        assert!(refusal.contains("重新发起"), "{refusal}");
    }

    #[test]
    fn an_invite_code_is_four_hex_chars_and_round_trips_through_the_parser() {
        let code = invite_code("gsrc", "gdst", 1_000_000);
        assert_eq!(code.len(), 4, "短码应当是 4 个 hex:{code}");
        assert!(code.chars().all(|c| c.is_ascii_hexdigit()), "{code}");
        assert_eq!(
            parse_invite_code(&format!("#{code}")).as_deref(),
            Some(code.as_str())
        );
    }

    #[test]
    fn an_invite_code_changes_when_the_invite_is_reissued() {
        // 同一对群先后两次发起要得到不同短码 —— 否则「上一封的短码」会误兑现这一封,
        // 而那封背书的是另一个时刻的源会话。
        let first = invite_code("gsrc", "gdst", 1_000_000);
        let second = invite_code("gsrc", "gdst", 1_000_001);
        assert_ne!(first, second);
    }

    #[test]
    fn an_invite_code_is_specific_to_the_pair_of_chats() {
        let to_a = invite_code("gsrc", "gdstA", 1_000_000);
        let to_b = invite_code("gsrc", "gdstB", 1_000_000);
        assert_ne!(to_a, to_b, "指向不同目标的邀请不该共用短码");
        let from_a = invite_code("gsrcA", "gdst", 1_000_000);
        let from_b = invite_code("gsrcB", "gdst", 1_000_000);
        assert_ne!(from_a, from_b, "来自不同源的邀请不该共用短码");
    }

    #[test]
    fn the_refusals_tell_the_user_where_to_look_and_why() {
        let missing = code_not_found();
        assert!(missing.contains("过期"), "要说清可能的原因:{missing}");
        assert!(missing.contains("`/fork`"), "要给下一步:{missing}");
        // 非定向短码唯一的边界:只有发起人能兑。驳回要说清这一点,否则用户以为码坏了。
        let others = code_belongs_to_someone_else();
        assert!(others.contains("发起它的人"), "{others}");
    }
}
