//! 跨群 fork 的落盘侧:把一次通过判据的 fork 计划写进目标 chat,让它下一轮按 fork 形态起。
//!
//! 判据在 `summon_core::fork`(纯件,可穷举单测),鉴权在 `can_operate`(双向,各自的群各判一次),
//! 这里只做两件事:**把盘上事实读成判据的入参**,以及**按顺序落盘**。
//!
//! 源群卡片与直达命令由 `fork_entry` 复验两端权限、成员和源会话；短码兑换由 chat worker
//! 复验邀请发起人及源会话。两种入口共享这里的目标检查与提交点。

use std::fs;
use std::path::PathBuf;

use anyhow::{Context as _, Result};
use summon_core::fork::{ForkPlan, ForkSourceState, ForkTargetState};
use summon_core::registry::Agent;
use summon_core::session_id::ForkOrigin;

#[cfg(test)]
use crate::runtime::fsio::atomic_write;
use crate::session::resume_store::{self, ResumePresence};

/// 一封 fork 邀请函:源群的 operator 说过「这个会话的上下文可以带走」。
///
/// **不指定目标群** —— 发起那一刻目标可能还没建出来。所以它按短码落盘,凭码兑现;
/// 而「谁能兑」由 `issuer_open_id` 限定成发起人本人。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ForkInvite {
    /// 源 chat 的 tag。
    pub source_tag: String,
    /// 源会话此刻的 sid。**记下来是为了在兑现时复验** —— 源群这十分钟里可能 reset 过,
    /// 那时它的历史已经不是发邀请时那份了,不能照旧 fork。
    pub source_sid: String,
    /// 源群的名字,只用于回执渲染。
    pub source_name: String,
    /// 发出邀请的时刻(unix 秒)。
    pub issued_at: i64,
    /// 发起人的 open_id。**只有他本人能兑现这封邀请。**
    ///
    /// 邀请不定向到任何群(那是「fork 到还不存在的群」这个能力的前提),所以「谁能兑」是唯一
    /// 剩下的边界。少了它,短码就是一张通行证:源群任何成员看到回执即可把整段上下文接进自己
    /// 新建的群,而 operate-gate 形同虚设 —— 那道闸是本仓破坏性动作的统一鉴权,fork 的破坏性
    /// 按设计自评「至少等同 Dismiss」(不可逆)。
    pub issuer_open_id: String,
}

impl ForkInvite {
    #[cfg(test)]
    fn render(&self) -> String {
        format!(
            "source_tag={}\nsource_sid={}\nsource_name={}\nissued_at={}\nissuer_open_id={}\n",
            self.source_tag,
            self.source_sid,
            summon_core::chat_meta::sanitize_value(&self.source_name),
            self.issued_at,
            self.issuer_open_id
        )
    }

    fn parse(raw: &str) -> Option<Self> {
        let mut source_tag = None;
        let mut source_sid = None;
        let mut source_name = None;
        let mut issued_at = None;
        let mut issuer_open_id = None;
        for line in raw.lines() {
            let Some((key, value)) = line.trim().split_once('=') else {
                continue;
            };
            let value = value.trim();
            if value.is_empty() {
                continue;
            }
            match key.trim() {
                "source_tag" => source_tag = Some(value.to_string()),
                "source_sid" => source_sid = Some(value.to_string()),
                "source_name" => source_name = Some(value.to_string()),
                "issued_at" => issued_at = value.parse::<i64>().ok(),
                "issuer_open_id" => issuer_open_id = Some(value.to_string()),
                _ => {}
            }
        }
        Some(Self {
            source_tag: source_tag?,
            source_sid: source_sid?,
            source_name: source_name.unwrap_or_default(),
            issued_at: issued_at?,
            // 发起人缺失即视为损坏:没有它就没有兑现边界,而「谁都能兑」是这个功能不能有的形态。
            issuer_open_id: issuer_open_id?,
        })
    }

    /// 邀请函在 `now` 这一刻还算不算数。
    ///
    /// **未来时间戳一律算过期**:那说明落盘时的时钟与现在的时钟对不上(改过系统时间、跨机复制过
    /// 状态目录),这种邀请函的 TTL 无从判断 —— 与其给它一个说不清的有效期,不如当它无效。
    pub(crate) fn is_fresh(&self, now: i64) -> bool {
        let age = now - self.issued_at;
        (0..=summon_core::fork::INVITE_TTL_SECS).contains(&age)
    }
}

/// 邀请函的路径由 `resume_store` 持有 —— reset 要清它,而 reset 的统一入口在那边。
/// 两处各写一份路径拼接,清理侧就会漏掉(C5/C10 那次漏清的形状)。
fn invite_path(agent: &Agent, code: &str) -> PathBuf {
    resume_store::fork_invite_path(agent, code)
}

/// 源群 operator 发起:落一封邀请函,键是它的短码。
///
/// **不指定目标群。** 那是「fork 到还不存在的群」的前提:发起那一刻目标可能还没建出来,
/// 连 tag 都不存在。代价是「谁能兑」成了唯一的边界,由 `ForkInvite::issuer_open_id` 承担。
#[cfg(test)]
pub(crate) fn issue_invite(agent: &Agent, code: &str, invite: &ForkInvite) -> Result<()> {
    atomic_write(&invite_path(agent, code), invite.render().as_bytes())
}

/// 凭短码读那封邀请。读不到 / 解析不出 / 已过期 → None。
///
/// **不校验目标群** —— 邀请本来就不定向。兑现资格由调用方比对 `issuer_open_id`:
/// 只有发起人本人能兑,否则短码就是一张通行证 —— 源群任何成员看到回执即可把整段上下文接进
/// 自己新建的群,而 operate-gate(本仓破坏性动作的统一鉴权)形同虚设。
pub(crate) fn read_invite(agent: &Agent, code: &str, now: i64) -> Option<ForkInvite> {
    let raw = fs::read_to_string(invite_path(agent, code)).ok()?;
    ForkInvite::parse(&raw).filter(|invite| invite.is_fresh(now))
}

/// 兑现成功后销掉那封邀请函。**一封邀请只能兑现一次** —— 留着它,同一段码在 TTL 内还能再 fork
/// 一遍,而发起人那次表态只针对当时那一次。
pub(crate) fn consume_invite(agent: &Agent, code: &str) {
    // 删不掉只是留下一封会自己过期的邀请,不该中断已经成功的 fork。
    let _ = fs::remove_file(invite_path(agent, code));
}

/// 读源 chat 的会话状态,喂给 `fork::evaluate_fork`。
///
/// Corrupt / Unreadable **不折成 Missing**:那会让「读不准」变成「没有历史」,fork 于是静默
/// 降级成冷启,目标群拿到空会话却以为带着上下文。
pub(crate) fn source_state(agent: &Agent, session: &str) -> ForkSourceState {
    match resume_store::inspect_record(agent, session) {
        ResumePresence::Missing => ForkSourceState::Missing,
        ResumePresence::Corrupt | ResumePresence::Unreadable => ForkSourceState::Unreadable,
        ResumePresence::Valid(record) => ForkSourceState::Ready {
            cli: record.cli,
            sid: record.id,
            strict_resume: record.strict_resume,
            fork_pending: record.fork_pending,
        },
    }
}

/// 源会话的工作目录 —— fork 出来的会话必须落在**同一个目录**。
///
/// 副本里满是对源目录的引用(读过的文件、跑过的命令、相对路径),换个目录接上去,那些引用全部
/// 指向不存在的路径。而对 claude 来说还更硬:转录文件路径由 cwd 推导,目录不对就**定位不到
/// 自己的转录**,整条会话的输出都看不见。
///
/// `.session` 的 cwd 优先(它是「当前生效目录」,`/cd` 会更新它),回退 per-chat `.cwd` 覆盖。
/// 两处都没有 → None:宁可拒绝 fork,也不拿一个猜的目录去起会话。
pub(crate) fn source_work_dir(agent: &Agent, session: &str) -> Option<String> {
    resume_store::read_record(agent, session)
        .and_then(|record| record.cwd)
        .or_else(|| super::chat_meta::read_chat_cwd(agent, session))
        .filter(|cwd| cwd.starts_with('/'))
}

/// 按 chat tag 找出该 chat 的 `.meta`:返回(会话名, meta)。
///
/// **会话名取自文件名,不由 chat_id 重算** —— 重算要猜 chat_type,猜错就落到另一条记录上;
/// 而 owner 私聊的裸名(`soul-<alias>`)压根算不出来(见 [`target_session_name`])。
/// 文件名是 daemon 自己写下的那个键,是唯一权威。
///
/// 同 tag 有多份 `.meta` 时取 mtime 最新的,与 `collect_digests` 同一条消歧规则。
pub(crate) fn find_chat_by_tag(
    agent: &Agent,
    tag: &str,
) -> Option<(String, summon_core::chat_meta::ChatMeta)> {
    let dir = agent.state_dir().join("runtime").join("chats");
    let mut best: Option<(
        std::time::SystemTime,
        String,
        summon_core::chat_meta::ChatMeta,
    )> = None;
    for entry in fs::read_dir(dir).ok()?.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("meta") {
            continue;
        }
        let Ok(content) = fs::read_to_string(&path) else {
            continue;
        };
        let meta = summon_core::chat_meta::ChatMeta::parse(&content);
        if meta.tag.as_deref() != Some(tag) {
            continue;
        }
        let Some(session) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let mtime = entry
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(std::time::UNIX_EPOCH);
        if best
            .as_ref()
            .is_none_or(|(best_mtime, _, _)| mtime > *best_mtime)
        {
            best = Some((mtime, session.to_string(), meta));
        }
    }
    best.map(|(_, session, meta)| (session, meta))
}

/// 目标 chat 的 tmux 会话名。**只支持群** —— 返回 None 即拒绝。
///
/// owner 私聊用裸名 `soul-<alias>`(`tmux_name_for_chat` 的唯一退化情形),而「这个私聊是不是
/// owner 的」只有 dispatcher 算得出:它依赖显式 owner 配置,以及无 owner 时「哪个 p2p chat 先
/// 说话」这个**运行期**状态。chat 线程手上没有这些,猜错就会把 fork 落到裸名记录上 —— 那条
/// 记录被 daemon 与 `attach` 两路共享,覆盖它等于顶掉 owner 自己的会话。
///
/// 群的名字恒为 `soul-<alias>-<tag>`,不依赖任何运行期状态,所以只放行群。
pub(crate) fn target_session_name(agent: &Agent, chat_type: &str, chat_id: &str) -> Option<String> {
    if !matches!(chat_type, "group" | "topic_group") {
        return None;
    }
    Some(agent.tmux_name_for_chat(chat_type, chat_id, false))
}

/// 读目标 chat 的状态。
///
/// `has_meta` 由调用方给:它证明「bot 在那个群露过面、发得出消息」,而这一点只有列出该 agent
/// 全部会话的那一侧知道(目标 tag 是从会话索引里解析出来的)。
pub(crate) fn target_state(agent: &Agent, session: &str, has_meta: bool) -> ForkTargetState {
    if !has_meta {
        return ForkTargetState::NeverSeen;
    }
    match resume_store::inspect_record(agent, session) {
        ResumePresence::Missing => ForkTargetState::Free,
        ResumePresence::Valid(record) => ForkTargetState::Occupied {
            cli: record.cli,
            sid: record.id,
        },
        // 损坏 / 不可读一律当占用:清不掉的记录会在下一轮把 fork 记录覆盖或让启动失败,
        // 而「看不清目标群有什么」时假定它空着,正是覆盖别人会话的那条路。
        ResumePresence::Corrupt | ResumePresence::Unreadable => ForkTargetState::Occupied {
            cli: "?".to_string(),
            sid: summon_i18n::text!("re46e751182").to_string(),
        },
    }
}

/// fork 要落进的那个 chat:身份三件 + 工作目录。
///
/// 收成一个结构体而不是六个平铺参数,是因为它们要么一起对、要么一起错 —— 拿 A 群的 chat_id
/// 配 B 群的会话名会把记录落到错误的键上,而平铺的参数列表让这种错配在调用点看不出来。
pub(crate) struct ForkTarget<'a> {
    /// 目标 chat 的 tmux 会话名(= `.session` / `.meta` 的键)。
    pub session: &'a str,
    pub chat_id: &'a str,
    pub chat_type: &'a str,
    pub chat_name: Option<&'a str>,
    /// 工作目录:必须与源会话同一个(见 [`source_work_dir`])。
    pub cwd: &'a str,
}

/// 目标 chat 的会话镜像水位文件(`threads/<tag>/mirror_offset`)。
///
/// fork 提交时必须清掉它 —— 见 [`commit_fork`] 里的理由。
fn mirror_offset_path(agent: &Agent, tag: &str) -> PathBuf {
    agent
        .state_dir()
        .join("threads")
        .join(tag)
        .join("mirror_offset")
}

/// 把 fork 落到目标 chat。**这是整条链路唯一的提交点。**
///
/// 落盘顺序 `cwd → meta → session`,与 `persist_headless_recall_binding` 逐字节同一条纪律:
/// `.session` 最后写,因为它是「这个 chat 有会话了」的判据。反过来先写它,并发入站会在还没
/// 落 cwd 的目标 chat 上启动 provider —— 那会跑在错误的工作目录里。
///
/// **落第一个字节之前先证明目标空闲。** 顺序纪律只保证「没提交成功就没有 `.session`」,保证不了
/// 「没提交成功就什么都没写」—— 而 `.cwd` 与 `.meta` 在它之前,且都是全量覆盖。目标群有活会话时
/// fork 会被拒,但那两个文件已经改了:per-chat 工作目录被换成源群的目录(它在 `work_dir_plan`
/// 里优先级最高),于是**别人那条会话下次重启就跑在源群目录里**,对 claude 更是连转录都定位不到。
/// 一次注定失败的 fork 不该留下任何痕迹,所以判据在这里重复一遍。
///
/// 同一目标的 Fork 提交通过文件锁串行处理，锁内预检确保失败请求不会改写 cwd/meta。
/// `write_fork_pending_checked` 的 no-replace rename 同时保护其它会话写入路径。
///
/// `new_sid` 的来处按 provider 分:claude 由这里预铸(它的 sid 本来就是 summon 指定的),
/// codex 的真 thread id 要等 `thread/fork` 返回,所以这里落的是占位值、由 RPC 池落盘时替换
/// (血缘跨那次替换存活,判据是「上一条记录待 fork」)。
pub(crate) fn commit_fork(
    agent: &Agent,
    plan: &ForkPlan,
    target: &ForkTarget<'_>,
    new_sid: &str,
    at: &str,
) -> Result<ForkOrigin> {
    super::group_target::with_record_lock(agent, "fork-targets", target.session, |_| {
        commit_fork_locked(agent, plan, target, new_sid, at)
    })
}

fn commit_fork_locked(
    agent: &Agent,
    plan: &ForkPlan,
    target: &ForkTarget<'_>,
    new_sid: &str,
    at: &str,
) -> Result<ForkOrigin> {
    let origin = ForkOrigin::new(&plan.source_tag, &plan.source_sid, at)
        .with_context(|| summon_i18n::format!("r6052606bb8", plan.source_tag, plan.source_sid))?;
    // Corrupt / Unreadable 一律当占用(fail-closed):看不清目标群有什么的时候假定它空着,
    // 正是覆盖别人会话的那条路。
    match resume_store::inspect_record(agent, target.session) {
        ResumePresence::Missing => {}
        ResumePresence::Valid(existing) => anyhow::bail!(summon_i18n::format!(
            "rc9d38724ad",
            existing.cli,
            existing.id
        )),
        ResumePresence::Corrupt => {
            anyhow::bail!(summon_i18n::format!("rc9dc78d8b3"))
        }
        ResumePresence::Unreadable => {
            anyhow::bail!(summon_i18n::format!("r81b4180226"))
        }
    }
    let source = find_chat_by_tag(agent, &plan.source_tag);
    crate::remote_tools::integration::fork_binding(
        agent,
        source.as_ref().map(|(session, _)| session.as_str()),
        target.session,
    )
    .context(summon_i18n::text!("rad929351c5"))?;
    super::chat_meta::write_chat_cwd_checked(agent, target.session, target.cwd)
        .context(summon_i18n::text!("r4996a8e08a"))?;
    // `.meta` 是**全量重写**,所以这两个字段必须先读回来 —— fork 只该改会话指向,不该顺手
    // 抹掉这个 chat 的既有事实。
    //
    // `creator`(谁把 bot 拉进这个群)与 fork 毫无关系,而它是群解散 / bot 被移除时那条通知的
    // **唯一收件人来源**(`chat_thread` 的 GoneTeardown 读它)。抹掉它,群散了就没人收到通知。
    // 有 `.meta` 的目标保留这些事实；由源群建群入口创建的目标可能还没有 `.meta`。
    //
    // `chat_name` 同理:调用方给的名字可能是退化值(拿不到群名时的兜底字符串),用它覆盖会把
    // 目标群在 `chats` 索引里显示成那个兜底名。盘上的原名优先。
    let existing = super::chat_meta::read_chat_meta(agent, target.session);
    let creator = existing.as_ref().and_then(|meta| meta.creator.clone());
    let chat_name = existing
        .as_ref()
        .and_then(|meta| meta.chat_name.clone())
        .filter(|name| !name.is_empty())
        .or_else(|| target.chat_name.map(str::to_string));
    super::chat_meta::write_chat_meta_checked(
        agent,
        target.session,
        target.chat_id,
        target.chat_type,
        chat_name.as_deref(),
        // fork 不是某个人「说了一句话」,`.meta` 的 last_sender 没有真实发言人可填。
        // 填 fork 而不是留空:排障时要能一眼看出这个 chat 的会话是 fork 来的。
        "fork",
        creator.as_deref(),
    )
    .context(summon_i18n::text!("r5128d16725"))?;
    resume_store::write_fork_pending_checked(
        agent,
        target.session,
        &plan.cli,
        new_sid,
        Some(target.cwd),
        Some(at),
        &origin,
    )
    .context(summon_i18n::text!("rcb5619e1ef"))?;
    // fork 不经 `handle_task` 的 admission,残留的 `closed` 会让刚 fork 好的会话显示成已关闭
    // 甚至误报泄漏 —— 与 `bind_group_to_canonical` 补 `mark_open` 同一个理由。
    // 标记写不下去只影响状态展示,fork 本身已经提交,不该因此报失败。
    let _ = crate::session::lifecycle::mark_open(agent, &plan.target_tag);
    // **清掉目标 tag 的会话镜像水位。**
    //
    // tag 是 `fnv1a_hex(chat_id)`,对一个 chat 是永久的;而 `clear_chat`(reset)不删这个文件。
    // 所以目标群此前用过 summon 又 reset 过时,盘上会留着一份属于**旧 sid** 的水位记录。
    // 副本继承了源会话的全部历史字节,水位判据一旦认下那条旧记录,那些字节就成了「待补发的
    // backlog」——源群整段对话会被倒进目标飞书群,而且撤不回来(第二红线)。
    //
    // 今天 sid 闸恰好挡得住(新铸 sid ≠ 旧 sid),但那是侥幸而非判据:安全性不该依赖
    // 「两个随机 id 不相等」。删掉它,让「目标群从 EOF 重新起步」成为落盘时的正向事实。
    let _ = fs::remove_file(mirror_offset_path(agent, &plan.target_tag));
    Ok(origin)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concurrent_forks_preserve_the_winning_session_and_work_directory() {
        let (agent, _dir) = crate::test_support::agent_dir("codex", "");
        let barrier = std::sync::Barrier::new(2);
        let outcomes = std::thread::scope(|threads| {
            let commit = |sid: &str, cwd: &str| {
                let plan = ForkPlan {
                    source_tag: format!("g{sid}"),
                    target_tag: "gtarget".into(),
                    cli: "codex".into(),
                    source_sid: format!("parent-{sid}"),
                };
                barrier.wait();
                commit_fork(
                    &agent,
                    &plan,
                    &ForkTarget {
                        session: "soul-test-gtarget",
                        chat_id: "oc_target",
                        chat_type: "group",
                        chat_name: Some("Target"),
                        cwd,
                    },
                    sid,
                    "2026-01-01 12:00:00",
                )
            };
            let a = threads.spawn(move || commit("first", "/first"));
            let b = threads.spawn(move || commit("second", "/second"));
            [a.join().unwrap().is_ok(), b.join().unwrap().is_ok()]
        });
        assert_eq!(outcomes.iter().filter(|ok| **ok).count(), 1);
        let record = resume_store::read_record(&agent, "soul-test-gtarget").unwrap();
        let expected = if outcomes[0] { "/first" } else { "/second" };
        assert_eq!(record.cwd.as_deref(), Some(expected));
        assert_eq!(
            super::super::chat_meta::read_chat_cwd(&agent, "soul-test-gtarget").as_deref(),
            Some(expected)
        );
    }

    fn agent_at(dir: &std::path::Path) -> Agent {
        let agent_dir = dir.join("agents").join("x");
        fs::create_dir_all(&agent_dir).unwrap();
        Agent {
            name: "x".to_string(),
            display: String::new(),
            status: "active".to_string(),
            role: Default::default(),
            model: None,
            effort: None,
            reasoning_summary: None,
            feishu: None,
            runtime: None,
            aliases: std::collections::HashMap::new(),
            messaging: None,
            dir: agent_dir,
            global_config_path: None,
            app_owner: Default::default(),
        }
    }

    fn invite() -> ForkInvite {
        ForkInvite {
            source_tag: "gsrc".into(),
            source_sid: "src-sid".into(),
            source_name: "sample开发群".into(),
            issued_at: 1_000_000,
            issuer_open_id: "ou_issuer".into(),
        }
    }

    fn code_of(inv: &ForkInvite) -> String {
        summon_core::fork::invite_code(&inv.source_tag, &inv.source_sid, inv.issued_at)
    }

    /// **发起人必须落盘。** 它是非定向短码唯一的兑现边界 —— 一封读不出发起人的邀请等于
    /// 「谁都能兑」,而那会让 operate-gate 形同虚设。所以缺它即视为损坏,不是「宽容降级」。
    #[test]
    fn an_invite_without_an_issuer_is_treated_as_corrupt() {
        let tmp = tempfile::tempdir().unwrap();
        let a = agent_at(tmp.path());
        let code = code_of(&invite());
        let path = crate::session::resume_store::fork_invite_path(&a, &code);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        // 有全部旧字段,只缺发起人。
        fs::write(
            &path,
            "source_tag=gsrc\nsource_sid=src-sid\nsource_name=X\nissued_at=1000000\n",
        )
        .unwrap();
        assert_eq!(read_invite(&a, &code, 1_000_060), None);
    }

    #[test]
    fn an_expired_invite_is_not_honoured() {
        let tmp = tempfile::tempdir().unwrap();
        let a = agent_at(tmp.path());
        let code = code_of(&invite());
        issue_invite(&a, &code, &invite()).unwrap();
        assert_eq!(
            read_invite(
                &a,
                &code,
                1_000_000 + summon_core::fork::INVITE_TTL_SECS + 1
            ),
            None
        );
    }

    #[test]
    fn an_invite_from_the_future_is_treated_as_expired() {
        assert!(!invite().is_fresh(999_000));
    }

    #[test]
    fn an_invite_is_consumed_so_it_cannot_be_redeemed_twice() {
        let tmp = tempfile::tempdir().unwrap();
        let a = agent_at(tmp.path());
        let code = code_of(&invite());
        issue_invite(&a, &code, &invite()).unwrap();
        consume_invite(&a, &code);
        assert_eq!(read_invite(&a, &code, 1_000_060), None);
    }

    /// 同一个源会话先后两次发起 → 两段不同的码,各自独立。
    ///
    /// 后一次不覆盖前一次:两段码指向的是同一个源会话的同一段历史,让先拿到码的人也能兑
    /// 没有坏处;而覆盖会让他手里那段码突然失效、却不知道为什么。
    #[test]
    fn reissuing_produces_an_independent_code() {
        let tmp = tempfile::tempdir().unwrap();
        let a = agent_at(tmp.path());
        let first = invite();
        let mut second = invite();
        second.issued_at = 1_000_005;
        let code_first = code_of(&first);
        let code_second = code_of(&second);
        assert_ne!(code_first, code_second);
        issue_invite(&a, &code_first, &first).unwrap();
        issue_invite(&a, &code_second, &second).unwrap();
        assert_eq!(read_invite(&a, &code_first, 1_000_060), Some(first));
        assert_eq!(read_invite(&a, &code_second, 1_000_060), Some(second));
    }

    /// 一段乱码查无此项 —— 不能因为「盘上恰好有别的邀请」就命中一封。
    #[test]
    fn an_unknown_code_finds_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let a = agent_at(tmp.path());
        issue_invite(&a, &code_of(&invite()), &invite()).unwrap();
        assert_eq!(read_invite(&a, "dead", 1_000_060), None);
    }

    #[test]
    fn a_corrupt_invite_is_ignored_rather_than_half_read() {
        let tmp = tempfile::tempdir().unwrap();
        let a = agent_at(tmp.path());
        let path = crate::session::resume_store::fork_invite_path(&a, "7f3a");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "source_tag=gsrc\n").unwrap();
        assert_eq!(read_invite(&a, "7f3a", 1_000_060), None);
    }

    #[test]
    fn a_source_that_cannot_be_read_is_never_reported_as_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let a = agent_at(tmp.path());
        let path = resume_store::record_path(&a, "soul-x-gsrc");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "这不是一条记录").unwrap();
        assert_eq!(
            source_state(&a, "soul-x-gsrc"),
            ForkSourceState::Unreadable,
            "读不准必须报 Unreadable —— 折成 Missing 会让 fork 静默降级成冷启"
        );
    }

    /// 写一份 `.meta`,键 = 会话名(daemon 真实的写法)。
    fn seed_meta(agent: &Agent, session: &str, chat_id: &str, chat_type: &str, name: &str) {
        crate::runtime::daemon::chat_meta::write_chat_meta_checked(
            agent,
            session,
            chat_id,
            chat_type,
            Some(name),
            "someone",
            None,
        )
        .unwrap();
    }

    #[test]
    fn a_chat_is_found_by_its_tag_and_the_session_name_comes_from_the_file_name() {
        // 会话名**取自文件名**,不由 chat_id 重算 —— 重算要猜 chat_type,而 owner 私聊的裸名
        // 压根算不出来。这条钉住「返回的是盘上那个键」。
        let tmp = tempfile::tempdir().unwrap();
        let a = agent_at(tmp.path());
        // 故意用一个**不等于** `tmux_name_for_chat` 结果的键(裸名形态),证明返回值来自文件名。
        seed_meta(&a, "soul-x", "oc_owner", "p2p", "owner私聊");
        let tag = summon_core::registry::chat_tag("p2p", "oc_owner");
        let (session, meta) = find_chat_by_tag(&a, &tag).expect("该找得到");
        assert_eq!(session, "soul-x", "会话名必须是盘上那个键,不是重算出来的");
        assert_eq!(meta.chat_id.as_deref(), Some("oc_owner"));
    }

    #[test]
    fn an_unknown_tag_finds_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let a = agent_at(tmp.path());
        seed_meta(&a, "soul-x-gaaa", "oc_a", "group", "甲群");
        assert!(find_chat_by_tag(&a, "gnope").is_none());
    }

    #[test]
    fn committing_a_fork_wipes_the_targets_stale_mirror_offset() {
        // 目标群此前用过 summon 又 reset 过时,`threads/<tag>/mirror_offset` 留在盘上(reset 不删它),
        // 而 tag 对一个 chat 是永久的。副本继承源会话的全部历史字节,水位判据一旦认下那条旧记录,
        // 源群整段对话就会被当成待补发的 backlog 倒进目标飞书群 —— 撤不回来。
        let tmp = tempfile::tempdir().unwrap();
        let a = agent_at(tmp.path());
        let offset = mirror_offset_path(&a, "gdst");
        fs::create_dir_all(offset.parent().unwrap()).unwrap();
        fs::write(&offset, "v3 epoch old-sid 4096 deadbeef terminal\n").unwrap();
        let plan = ForkPlan {
            source_tag: "gsrc".into(),
            target_tag: "gdst".into(),
            cli: "claude".into(),
            source_sid: "src-sid".into(),
        };
        commit_fork(
            &a,
            &plan,
            &ForkTarget {
                session: "soul-x-gdst",
                chat_id: "oc_dst",
                chat_type: "group",
                chat_name: Some("目标群"),
                cwd: "/repo",
            },
            "new-sid",
            "2026-09-03 11:52:30",
        )
        .expect("空目标该提交得进去");
        assert!(
            !offset.exists(),
            "提交后必须没有残留水位 —— 目标群要从 EOF 重新起步"
        );
    }

    #[test]
    fn only_a_group_can_be_a_fork_target() {
        // owner 私聊的裸名记录被 daemon 与 `attach` 两路共享,而「这个私聊是不是 owner 的」
        // 只有 dispatcher 算得出。猜错就是覆盖 owner 自己的会话,所以这里只放行群。
        let tmp = tempfile::tempdir().unwrap();
        let a = agent_at(tmp.path());
        assert!(target_session_name(&a, "p2p", "oc_x").is_none());
        for chat_type in ["group", "topic_group"] {
            let name = target_session_name(&a, chat_type, "oc_x").expect("群该给出会话名");
            assert!(name.starts_with("soul-x-"), "{name}");
        }
    }

    #[test]
    fn a_target_without_meta_is_never_seen_even_if_it_has_no_session() {
        let tmp = tempfile::tempdir().unwrap();
        let a = agent_at(tmp.path());
        assert_eq!(
            target_state(&a, "soul-x-gdst", false),
            ForkTargetState::NeverSeen
        );
    }

    #[test]
    fn a_target_with_an_unreadable_record_counts_as_occupied() {
        // 「看不清目标群有什么」时假定它空着,正是覆盖别人会话的那条路。
        let tmp = tempfile::tempdir().unwrap();
        let a = agent_at(tmp.path());
        let path = resume_store::record_path(&a, "soul-x-gdst");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "坏记录").unwrap();
        assert!(matches!(
            target_state(&a, "soul-x-gdst", true),
            ForkTargetState::Occupied { .. }
        ));
    }

    #[test]
    fn committing_a_fork_lands_all_three_pieces() {
        // 三件都得在:缺 cwd 会让转录定位不到,缺 meta 会让这个 chat 不在索引里,
        // 缺 session 就没有 fork 判据。**顺序**由 `commit_fork_writes_the_session_record_last`
        // 单独守 —— 存在性证明不了顺序。
        let tmp = tempfile::tempdir().unwrap();
        let a = agent_at(tmp.path());
        let plan = ForkPlan {
            source_tag: "gsrc".into(),
            target_tag: "gdst".into(),
            cli: "claude".into(),
            source_sid: "src-sid".into(),
        };
        let origin = commit_fork(
            &a,
            &plan,
            &ForkTarget {
                session: "soul-x-gdst",
                chat_id: "oc_dst",
                chat_type: "group",
                chat_name: Some("目标群"),
                cwd: "/repo",
            },
            "new-sid",
            "2026-09-03 11:52:30",
        )
        .expect("空目标该提交得进去");
        assert_eq!(origin.sid, "src-sid");
        let rec = resume_store::read_record(&a, "soul-x-gdst").expect("记录该在");
        assert!(rec.fork_pending);
        assert_eq!(rec.forked_from, Some(origin));
        assert_eq!(rec.cwd.as_deref(), Some("/repo"));
        assert!(crate::runtime::daemon::chat_meta::read_chat_cwd(&a, "soul-x-gdst").is_some());
        assert!(crate::runtime::daemon::chat_meta::read_chat_meta(&a, "soul-x-gdst").is_some());
    }

    /// **落盘顺序必须是 cwd → meta → session。**
    ///
    /// `.session` 是「这个 chat 有会话了」的判据,所以它必须最后写:反过来先写它,并发入站会在
    /// 还没落 cwd 的目标 chat 上启动 provider,provider 于是跑在错误的工作目录里 —— 对 claude
    /// 意味着转录定位不到,整条会话的输出全都看不见。`daemon/bridge.rs:1801` 是踩过这个坑的现场。
    ///
    /// 三件都写成功时**结果完全一样**,所以行为断言证明不了顺序(只能证明三者都存在)。
    /// 这里按源码顺序守,是目前唯一能证伪它的办法。
    #[test]
    fn commit_fork_writes_the_session_record_last() {
        let code = include_str!("fork.rs");
        let start = code
            .find("pub(crate) fn commit_fork(")
            .expect("commit_fork 必须在");
        let body = &code[start..];
        let end = body
            .find("\n#[cfg(test)]")
            .expect("commit_fork 之后应当是测试模块");
        let body = &body[..end];
        let cwd = body.find("write_chat_cwd_checked").expect("必须写目标 cwd");
        let meta = body
            .find("write_chat_meta_checked")
            .expect("必须写目标 meta");
        let session = body
            .find("write_fork_pending_checked")
            .expect("必须写目标 session");
        assert!(
            cwd < meta && meta < session,
            "落盘顺序必须是 cwd → meta → session(实得 cwd={cwd} meta={meta} session={session})"
        );
    }

    /// fork **不得抹掉目标 chat 的既有事实**:`creator` 与群名。
    ///
    /// `.meta` 是全量重写,而 `creator`(谁把 bot 拉进这个群)是群解散 / bot 被移除时那条通知的
    /// 唯一收件人来源。fork 与它毫无关系,抹掉它就是群散了没人收到通知 —— 而且要等到真散的
    /// 那天才发现。群名同理:调用方可能给的是拿不到名字时的兜底串,用它覆盖会让目标群在
    /// `chats` 索引里显示成那个兜底名。
    #[test]
    fn committing_a_fork_preserves_the_targets_creator_and_name() {
        let tmp = tempfile::tempdir().unwrap();
        let a = agent_at(tmp.path());
        // 目标群此前跟 bot 说过话(否则 `evaluate_fork` 早在 `TargetUnknown` 就拒了),
        // `.meta` 里有真实的 creator 与群名。
        crate::runtime::daemon::chat_meta::write_chat_meta_checked(
            &a,
            "soul-x-gdst",
            "oc_dst",
            "group",
            Some("真实群名"),
            "张三",
            Some("ou_creator"),
        )
        .unwrap();
        let plan = ForkPlan {
            source_tag: "gsrc".into(),
            target_tag: "gdst".into(),
            cli: "claude".into(),
            source_sid: "src-sid".into(),
        };
        commit_fork(
            &a,
            &plan,
            &ForkTarget {
                session: "soul-x-gdst",
                chat_id: "oc_dst",
                chat_type: "group",
                // 调用方给的是退化兜底值 —— 它不该盖掉盘上的真名。
                chat_name: Some("本会话"),
                cwd: "/repo",
            },
            "new-sid",
            "2026-09-03 11:52:30",
        )
        .expect("空目标该提交得进去");
        let meta = crate::runtime::daemon::chat_meta::read_chat_meta(&a, "soul-x-gdst")
            .expect("meta 该在");
        assert_eq!(
            meta.creator.as_deref(),
            Some("ou_creator"),
            "creator 是群解散通知的唯一收件人来源,fork 不该抹掉它"
        );
        assert_eq!(
            meta.chat_name.as_deref(),
            Some("真实群名"),
            "盘上的原名优先于调用方的兜底值"
        );
        assert_eq!(meta.last_sender.as_deref(), Some("fork"));
    }

    /// 被拒的 fork **一个字节都不许写**。
    ///
    /// 落盘顺序只保证「没提交成功就没有 `.session`」,保证不了「没提交成功就什么都没写」——
    /// `.cwd` 与 `.meta` 排在它之前且都是全量覆盖。目标群那条活会话的工作目录一旦被换成源群的
    /// 目录,它下次重启就跑在别人的目录里(`.cwd` 在 `work_dir_plan` 里优先级最高),
    /// 对 claude 更是连自己的转录都定位不到。
    #[test]
    fn committing_a_fork_refuses_an_occupied_target_without_touching_it() {
        let tmp = tempfile::tempdir().unwrap();
        let a = agent_at(tmp.path());
        // 目标群有自己的活会话:自己的工作目录、自己的最后发言人。
        resume_store::write(
            &a,
            "soul-x-gdst",
            "claude",
            "busy-sid",
            Some("/theirs"),
            None,
        );
        crate::runtime::daemon::chat_meta::write_chat_cwd_checked(&a, "soul-x-gdst", "/theirs")
            .unwrap();
        crate::runtime::daemon::chat_meta::write_chat_meta_checked(
            &a,
            "soul-x-gdst",
            "oc_dst",
            "group",
            Some("目标群"),
            "张三",
            Some("ou_creator"),
        )
        .unwrap();
        let plan = ForkPlan {
            source_tag: "gsrc".into(),
            target_tag: "gdst".into(),
            cli: "claude".into(),
            source_sid: "src-sid".into(),
        };
        assert!(commit_fork(
            &a,
            &plan,
            &ForkTarget {
                session: "soul-x-gdst",
                chat_id: "oc_dst",
                chat_type: "group",
                chat_name: Some("目标群"),
                cwd: "/repo",
            },
            "new-sid",
            "2026-09-03 11:52:30",
        )
        .is_err());
        assert_eq!(
            resume_store::read_record(&a, "soul-x-gdst").unwrap().id,
            "busy-sid",
            "拒绝之后目标会话必须原样不动"
        );
        // `.session` 没变还不够 —— cwd 与 meta 排在它之前,也必须一个字节都没动。
        assert_eq!(
            crate::runtime::daemon::chat_meta::read_chat_cwd(&a, "soul-x-gdst").as_deref(),
            Some("/theirs"),
            "目标群那条活会话的工作目录被改成源群的 —— 它下次重启就跑在别人的目录里"
        );
        assert_eq!(
            crate::runtime::daemon::chat_meta::read_chat_meta(&a, "soul-x-gdst")
                .and_then(|meta| meta.last_sender),
            Some("张三".to_string()),
            "被拒的 fork 不该把 last_sender 改成 fork"
        );
    }
}
