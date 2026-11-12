//! per-chat 盘上元数据(A1 拆分):chats/<session>.meta 与 .cwd 的读写、effective_work_dir。

use super::*;
use crate::runtime::fsio::atomic_write;

pub(super) fn chat_meta_path(agent: &Agent, session: &str) -> PathBuf {
    agent
        .state_dir()
        .join(summon_core::chat_meta::meta_rel_path(session))
}

/// 把 chat 的持久身份落 `.meta`。`creator` = 该会话「群主近似」(首个 inbound 的 sender open_id),
/// 群没了通知谁用 —— 只在首写(`.meta` 还没 creator 行)时记一次,后续续写沿用旧值,
/// 避免被后说话的人覆盖成非群主。向后兼容:旧 `.meta` 无 creator 行 → 读出 None,走回退链。
pub(crate) fn write_chat_meta(
    agent: &Agent,
    session: &str,
    chat_id: &str,
    chat_type: &str,
    chat_name: Option<&str>,
    last_sender: &str,
    creator: Option<&str>,
) {
    let _ = write_chat_meta_checked(
        agent,
        session,
        chat_id,
        chat_type,
        chat_name,
        last_sender,
        creator,
    );
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn write_chat_meta_checked(
    agent: &Agent,
    session: &str,
    chat_id: &str,
    chat_type: &str,
    chat_name: Option<&str>,
    last_sender: &str,
    creator: Option<&str>,
) -> anyhow::Result<()> {
    let path = chat_meta_path(agent, session);
    // schema/渲染收口 summon-core::chat_meta(A4),这里只剩文件 IO。
    let meta = summon_core::chat_meta::ChatMeta {
        chat_id: Some(chat_id.to_string()),
        chat_type: Some(chat_type.to_string()),
        chat_name: chat_name.map(str::to_string),
        tag: Some(chat_tag(chat_type, chat_id)),
        last_sender: Some(last_sender.to_string()),
        updated_at: Some(chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string()),
        creator: creator.map(str::to_string),
    };
    // 有新入站 = 该 chat 重新开始对话 = 上一次退出的标记该让位,否则同一 chat 第二次 kill
    // 会被旧标记静默吞掉。`.meta` 全量重写是两种 capture 模式、两条入站路径(chat_thread /
    // bridge)的唯一交汇点,挂这里一处覆盖全部,不会漏。
    crate::session::exit_mark::clear(agent, chat_type, chat_id);
    let rendered = meta.render();
    atomic_write(&path, rendered.as_bytes())
}

/// 读 `.meta` 里已记的 creator(群主近似)。文件缺 / 无 creator 行 → None(向后兼容)。
/// 从 `.meta` 的 `updated_at` 恢复该 chat 的**真实**最后活动时刻。
///
/// **为什么不能用 `SystemTime::now()` 当计时器起点**(owner 2026-08-13 实测):chat handle 是懒建的,
/// daemon 每重启一次就全部重建一遍,起点统一被推到「此刻」。后果有两个,第二个更严重:
///
/// 1. 所有 agent × 所有 chat 的空闲计时在重启那一刻**一起清零**,此后永久同相 —— 55min 后集体
///    推卡。真机:13:51:44 部署,14:46:46-48 三秒内四张空闲卡砸进两个群。
/// 2. **空闲会话每次重启白白续命一个 ttl**。一台每小时重启一次的机器,`idle_ttl_secs`(提醒)与
///    `idle_retire_secs`(自动退出)永远等不到 —— 它们等的是一个每次重启都被推倒重来的钟,
///    功能静默失效。
///
/// `.meta` 的 `updated_at` 每条入站消息重写一次,是这个时刻在盘上唯一的、跨重启存活的记录。
/// 它是**下界**:agent 自己的活动、定时轮不写它,所以恢复出来的起点只会比真实活动**早**,
/// 方向上偏向「早一点提醒」而不是「永远不提醒」。解析不出就回退 `now()`(与改动前逐字节一致)。
pub(super) fn read_chat_last_activity(
    agent: &Agent,
    session: &str,
) -> Option<std::time::SystemTime> {
    let raw = read_chat_meta(agent, session)?.updated_at?;
    let naive = chrono::NaiveDateTime::parse_from_str(raw.trim(), "%Y-%m-%d %H:%M:%S").ok()?;
    // `.meta` 写的是本地时间(见 write_chat_meta),按本地时区还原。夏令时切换那一小时可能有两个
    // 合法解释,`single()` 在这种歧义下返回 None → 回退 now(),不猜。
    let secs = chrono::TimeZone::from_local_datetime(&chrono::Local, &naive)
        .single()?
        .timestamp();
    (secs >= 0).then(|| std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs as u64))
}

/// 空闲提醒卡的 **durable 去重账本**:`threads/<tag>/idle_prompt.marker`。
/// 每 chat 一个文件,身份 = chat tag(即路径本身);判定只读内容里的 `attempted_at`
/// (与恢复出的 last_activity 比较,见 [`read_idle_prompt_marker_since`]),
/// `period_start` 只是排查用的诊断信息,**不参与判定**。同一段空闲期重写只是覆盖凭证,
/// 语义天然按空闲期幂等。
///
/// 为什么不能从 `outbound.ndjson` 归档推导(终审阻塞,2026-08-13):归档是 best-effort
/// 观测日志 —— 发送成功 → append 失败/崩溃,文件完整可读却没有卡记录,重启后会被误判
/// 「没发过」而重发;磁盘短暂写满可同时命中多个 chat;且归档会轮转出 `.1`,尾窗回看还要
/// 追加一层轮转语义。账本必须**先写后发**(见 [`write_idle_prompt_marker`]),
/// at-most-once 才不依赖「发送方事后如实记账」这个证不了的假设。
pub(super) fn idle_prompt_marker_path(agent: &Agent, chat_type: &str, chat_id: &str) -> PathBuf {
    idle_prompt_marker_path_at_tag(agent, &chat_tag(chat_type, chat_id))
}

/// tag 直达变体:升级迁移按 `threads/*` 目录名(即 tag)补写 marker 时,手里只有 tag、
/// 没有(也不需要)chat_id —— tag 是 chat_id 的单向散列,反推不回去。
fn idle_prompt_marker_path_at_tag(agent: &Agent, tag: &str) -> PathBuf {
    agent
        .state_dir()
        .join("threads")
        .join(tag)
        .join("idle_prompt.marker")
}

/// 发空闲提醒卡前**必须先**调用本函数把 marker 落盘(tmp → fsync → rename,走仓内唯一
/// 原子写原语),写成功才允许联网发送;写失败调用方**不发**(fail-closed:漏一张、下轮
/// 扫描重试,绝不「先发再记」)。marker 落盘那一刻,本段空闲期的发送机会即已用掉 ——
/// 之后发送层报什么(成功/失败/超时)都不改变账本,因为那份报告不可信(超时实为已送达、
/// 成功却没来得及记录,都发生过)。
///
/// `attempted_at` = 此刻(重启恢复的**唯一**比较基准,见 [`read_idle_prompt_marker_since`]);
/// `period_start` = 发卡时刻的 `last_activity`,仅诊断用、不参与判定。
pub(super) fn write_idle_prompt_marker(
    agent: &Agent,
    chat_type: &str,
    chat_id: &str,
    period_start: std::time::SystemTime,
    attempted_at: std::time::SystemTime,
) -> anyhow::Result<()> {
    write_idle_prompt_marker_at_tag(
        agent,
        &chat_tag(chat_type, chat_id),
        period_start,
        attempted_at,
    )
}

fn write_idle_prompt_marker_at_tag(
    agent: &Agent,
    tag: &str,
    period_start: std::time::SystemTime,
    attempted_at: std::time::SystemTime,
) -> anyhow::Result<()> {
    let entry = serde_json::json!({
        "period_start": chrono::DateTime::<chrono::Local>::from(period_start).to_rfc3339(),
        "attempted_at": chrono::DateTime::<chrono::Local>::from(attempted_at).to_rfc3339(),
    });
    atomic_write(
        &idle_prompt_marker_path_at_tag(agent, tag),
        entry.to_string().as_bytes(),
    )
}

/// 从 durable marker 恢复「**本段空闲期**是否已(尝试)发过空闲提醒卡」。与
/// [`read_chat_last_activity`] 同构:空闲时钟跨重启恢复了(b81bdd8),这个布尔只活在内存的话,
/// daemon 一重启所有已到点的会话都会被再发一张卡(owner 2026-08-13 两次部署实测)。
///
/// 判定:marker 的 `attempted_at` 落在恢复出的 `last_activity` 之后 = 本段空闲期已尝试过
/// → 不再发。用户再说话会重写 `.meta` 的 `updated_at`,`last_activity` 越过 `attempted_at`,
/// 判定自然翻回「未提醒」。比较基准用 `attempted_at` 而非 `period_start`:后者与 `.meta`
/// 时刻间有秒级写入漂移,而 `attempted_at` 与两侧都隔着至少一个 idle_ttl,漂移不可能翻转判定。
///
/// 方向:
/// - **marker 缺失 → false(可发)**。「没有 marker = 本机制确实没尝试过」由两条一起保证:
///   先写后发的不变量(新机制下发过必有 marker),加上升级迁移
///   ([`ensure_idle_prompt_marker_migration`]:老版本发过卡但没有 marker 的会话,在任何
///   扫描发生前已被按「已提醒」补写)。缺了后者,部署新版本自身的重启就会给所有已提醒、
///   仍空闲、被 boot 恢复的旧会话再发一张 —— 恰是本机制要修的事故的复演。
/// - **marker 在但读不出/解析不出/时间坏 → true(不发,fail-closed)**:证不了「没发过」
///   就当发过,漏一张无害,重发才是 bug。
pub(super) fn read_idle_prompt_marker_since(
    agent: &Agent,
    chat_type: &str,
    chat_id: &str,
    last_activity: std::time::SystemTime,
) -> bool {
    let path = idle_prompt_marker_path(agent, chat_type, chat_id);
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return false,
        Err(_) => return true,
    };
    match serde_json::from_str::<serde_json::Value>(&raw)
        .ok()
        .and_then(|v| {
            v.get("attempted_at")?
                .as_str()
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                .map(std::time::SystemTime::from)
        }) {
        Some(attempted_at) => attempted_at >= last_activity,
        // 撕裂/坏 schema/时间读不出:确有一次尝试的痕迹但读不清 → 不猜,当发过。
        None => true,
    }
}

/// 新活动到来 = 新一段空闲期,盘上「已提醒」凭证作废。删除失败方向安全(重启后误判
/// 「已提醒」→ 漏一张卡,不会重发),best-effort 忽略。
pub(super) fn clear_idle_prompt_marker(agent: &Agent, chat_type: &str, chat_id: &str) {
    let _ = std::fs::remove_file(idle_prompt_marker_path(agent, chat_type, chat_id));
}

/// 「空闲卡账本迁移已完成」的**类型证明**:唯一构造点是
/// [`ensure_idle_prompt_marker_migration`] 成功返回(字段对 daemon 其它模块私有,
/// tests 也只能走真迁移拿证明)。`run_idle_scan` 以它为必填参数 ——「迁移未完成不进扫描」
/// 由类型系统钉死,不靠「先调迁移再调扫描」的调用顺序默契。
pub(crate) struct IdleMarkersMigrated(());

/// 迁移完成标记(版本化):存在 = 该 agent 目录下升级前已有的 chat 已**全部**补写过
/// marker,后续启动直接放行。部分失败不写此文件,下次启动从缺的地方继续(补写幂等)。
fn idle_marker_migration_done_path(agent: &Agent) -> PathBuf {
    agent
        .state_dir()
        .join("runtime")
        .join("idle-marker-migration-v1.done")
}

/// 升级迁移(终审阻塞,2026-08-13):durable marker 机制首次启动时,老版本发过卡的会话
/// 盘上没有 marker,而部署新版本本身就重启 daemon —— boot 恢复旧会话后,首轮空闲扫描会把
/// 所有「已提醒、仍空闲」的旧会话再发一张,完整复演 owner 报告的事故。所以必须在**任何
/// 空闲扫描之前**,为升级前已有的每个 chat 的当段空闲期补写 marker(按「已提醒」处理;
/// 老版本到底发没发证不了,fail-closed 的方向取「漏一张」不取「重发」)。新入站仍经
/// `note_activity` 作废 marker,下一段空闲期照常发卡 —— 迁移只压当段。
///
/// **覆盖面的证成**(不靠「个位数 fleet」这类规模论):空闲卡只发给内存 `chats` 里的
/// handle;不带 `note_activity` 的恢复路径只有 boot 的两个扫描 —— pending 扫描的产出
/// 全部来自 `threads/*` 目录名(tag),mirror 扫描的产出全部来自 `runtime/chats/*.meta`
/// 中 chat_id+tag 齐全的条目;这两个来源在此全量枚举,故任何可被 boot 恢复的 chat 必已
/// 补写。其余 spawn 点(Inbound/replacement/孤儿重投/定时/哨兵)全部随手 `note_activity`,
/// 属于新活动开启的新空闲期,不在迁移范围。枚举本身失败(目录在但读不了)→ 覆盖面证不成,
/// 直接失败,绝不带着不完整的枚举宣布完成。
///
/// 提交协议:逐 tag 补写(已有 marker 跳过 —— 幂等,可跨启动续跑);**全部**成功才写
/// done 标记;任一失败不宣布完成 —— 本次启动拿不到 [`IdleMarkersMigrated`] 证明,
/// 整个进程生命周期不进空闲扫描(fail-closed),下次启动继续。
pub(super) fn ensure_idle_prompt_marker_migration(
    agent: &Agent,
) -> anyhow::Result<IdleMarkersMigrated> {
    use anyhow::Context as _;
    let done = idle_marker_migration_done_path(agent);
    // try_exists 语义:IO 错**不是**迁移状态。`exists()` 会把 metadata 错误吞成 false ——
    // 读不了标记就重跑迁移,把已被新活动作废的 marker 重新压上,静默违反「done 之后迁移
    // 退场」。证不了状态就 Err:本次启动拿不到证明、不进扫描,下次启动再证。
    if done
        .try_exists()
        .with_context(|| summon_i18n::format!("r10918358c4", done.display()))?
    {
        return Ok(IdleMarkersMigrated(()));
    }
    let mut tags = std::collections::BTreeSet::new();
    // 来源①:threads/* 目录名即 tag(boot pending 扫描可能产出的全集)。判目录用
    // DirEntry::file_type 而不是 Path::is_dir —— 后者把 metadata 错误吞成 false,
    // 一次瞬时 IO 错 = 静默漏一个 tag 却照常宣布完成,正是「枚举失败即 Err」要堵的洞。
    match std::fs::read_dir(agent.state_dir().join("threads")) {
        Ok(entries) => {
            let projected = entries.map(|e| -> std::io::Result<Vec<String>> {
                let e = e?;
                if !e.file_type()?.is_dir() {
                    return Ok(Vec::new());
                }
                Ok(e.file_name()
                    .to_str()
                    .map(str::to_string)
                    .into_iter()
                    .collect())
            });
            collect_migration_tags(projected, &mut tags)
                .map_err(|e| anyhow::anyhow!(summon_i18n::format!("r66ef71a297")))?;
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => anyhow::bail!(summon_i18n::format!("r66ef71a297")),
    }
    // 来源②:runtime/chats/*.meta(boot mirror 扫描可能产出的全集)。缺 chat_id 的条目
    // 任何 boot 路径都恢复不出 handle(mirror 要求 chat_id+tag 齐全并复核一致;pending 由
    // 来源①覆盖),不构成发卡面,跳过即安全。tag 取「meta 记录值 ∪ 按 chat_id 重算值」的
    // 并集 —— 宁多写一个无主 marker,不漏一个可恢复的 chat。
    match std::fs::read_dir(agent.state_dir().join("runtime").join("chats")) {
        Ok(entries) => {
            collect_migration_tags(entries.map(|e| meta_entry_tags(&e?.path())), &mut tags)
                .map_err(|e| anyhow::anyhow!(summon_i18n::format!("r167bf1e2ef")))?;
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => anyhow::bail!(summon_i18n::format!("r167bf1e2ef")),
    }
    let now = std::time::SystemTime::now();
    let mut wrote = 0usize;
    let mut preexisting = 0usize;
    let mut failures: Vec<String> = Vec::new();
    for tag in &tags {
        // 已有 marker 的不覆盖:可能是上次未完成迁移写的(幂等续跑),也可能读不清
        // (判定侧本就 fail-closed 当「已提醒」)。只补缺,不动存量。
        if idle_prompt_marker_path_at_tag(agent, tag).exists() {
            preexisting += 1;
            continue;
        }
        match write_idle_prompt_marker_at_tag(agent, tag, now, now) {
            Ok(()) => wrote += 1,
            // 单个失败不早退:把能写的都写上,让下次续跑的缺口最小。
            Err(e) => failures.push(format!("{tag}: {e:#}")),
        }
    }
    if !failures.is_empty() {
        anyhow::bail!(summon_i18n::format!(
            "r12f60074cc",
            failures.len(),
            wrote,
            preexisting,
            failures.join("; ")
        ));
    }
    let stamp = serde_json::json!({
        "version": 1,
        "completed_at": chrono::DateTime::<chrono::Local>::from(now).to_rfc3339(),
        "migrated": wrote,
        "preexisting": preexisting,
    });
    atomic_write(&done, stamp.to_string().as_bytes()).context(summon_i18n::text!("r1fc4e778ff"))?;
    if wrote > 0 {
        log(&summon_i18n::format!("r42f1a527c9", agent.name));
    }
    Ok(IdleMarkersMigrated(()))
}

/// 迁移枚举的严格收集核(纯函数,IO 全在调用方的投影闭包里):逐项消费可失败的枚举流,
/// 任一项 Err **立即上抛**,绝不带着不完整的枚举继续。这是「枚举失败即 Err」在逐项粒度
/// 上的落点 —— `ReadDir` 是 `Iterator<Item = io::Result<DirEntry>>`,`flatten()` 会把
/// 逐项 IO 错静默吞成「跳过该项」:漏掉的 tag 不补 marker 却照常写 done,重启恢复该
/// chat 后首轮扫描重发空闲卡,且 done 已落盘**永不自愈**。可注入普通迭代器,回归测试
/// 据此钉死「中途一个 Err → 不产证明、不写 done」(把这里改回 flatten 必红)。
fn collect_migration_tags(
    entries: impl Iterator<Item = std::io::Result<Vec<String>>>,
    tags: &mut std::collections::BTreeSet<String>,
) -> std::io::Result<()> {
    for entry in entries {
        tags.extend(entry?);
    }
    Ok(())
}

/// 来源②单个目录条目 → 它贡献的 tag 集(非 `.meta` 条目贡献空集)。readdir 与 read 之间
/// 被删(NotFound)= 该 chat 已不可恢复,跳过安全;读不了但文件在 → 带路径上抛(mirror
/// 扫描此刻跳过、下一刻可能读得出 —— 覆盖面证不成)。
fn meta_entry_tags(p: &std::path::Path) -> std::io::Result<Vec<String>> {
    if p.extension().and_then(|s| s.to_str()) != Some("meta") {
        return Ok(Vec::new());
    }
    let content = match std::fs::read_to_string(p) {
        Ok(content) => content,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(std::io::Error::new(
                e.kind(),
                summon_i18n::format!("r6063106994", p.display()),
            ))
        }
    };
    let m = summon_core::chat_meta::ChatMeta::parse(&content);
    let mut out = Vec::new();
    if let Some(tag) = m.tag {
        out.push(tag);
    }
    if let Some(chat_id) = m.chat_id {
        let chat_type = m.chat_type.unwrap_or_else(|| "group".to_string());
        out.push(chat_tag(&chat_type, &chat_id));
    }
    Ok(out)
}

pub(super) fn read_chat_meta_creator(agent: &Agent, session: &str) -> Option<String> {
    read_chat_meta(agent, session)?.creator
}

/// 群解散 / bot 被移除时必须在删 `.meta` 前同时取出 creator 与群名；集中读取避免两处解析
/// 漂移。历史文件缺字段仍由 core schema 自然降级为 None。
pub(crate) fn read_chat_meta(
    agent: &Agent,
    session: &str,
) -> Option<summon_core::chat_meta::ChatMeta> {
    let content = std::fs::read_to_string(chat_meta_path(agent, session)).ok()?;
    Some(summon_core::chat_meta::ChatMeta::parse(&content))
}

/// 本会话(per-chat)工作目录覆盖文件:`runtime/chats/<session>.cwd`,单行存已展开的路径。
/// **与 `.meta` 分开放**——`.meta` 每条入站全量重写,塞一起会被覆盖;独立文件只由 `/cd` / 面板改,
/// 不进热路径重写,无需「读旧值再回写」的保护逻辑。键名(session)与 `.meta` 一致 = tmux 会话名。
pub(super) fn chat_cwd_path(agent: &Agent, session: &str) -> PathBuf {
    agent
        .state_dir()
        .join("runtime")
        .join("chats")
        .join(format!("{session}.cwd"))
}

/// `.thinking` 是兼容迁移输入；当前设置统一写入 `.cot`。
pub(super) fn chat_thinking_path(agent: &Agent, session: &str) -> PathBuf {
    agent
        .state_dir()
        .join("runtime")
        .join("chats")
        .join(format!("{session}.thinking"))
}

/// 本会话(per-chat)分级开关覆盖文件。每行一个 `<key>=on|off|inherit`,key 取自
/// `summon_core::settings::SETTINGS`。**全部开关共用这一个文件**:唯一的原子提交点,
/// 加开关不必新增读写函数与清理路径。
pub(super) fn chat_settings_path(agent: &Agent, session: &str) -> PathBuf {
    agent
        .state_dir()
        .join("runtime")
        .join("chats")
        .join(format!("{session}.settings"))
}

/// `.cot` 是兼容迁移输入;当前设置统一写入 `.settings`。
pub(super) fn chat_cot_path(agent: &Agent, session: &str) -> PathBuf {
    agent
        .state_dir()
        .join("runtime")
        .join("chats")
        .join(format!("{session}.cot"))
}

/// 一个 chat 在 agent 目录内拥有的全部持久化路径。会话名键与 chat-tag 键的两套布局在这里
/// 集中列出，群解散/bot 被移除时不再由各写入者各自猜测该删什么。
pub(super) fn chat_disk_paths(agent: &Agent, session: &str, tag: &str) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if !session.is_empty() {
        paths.extend([
            chat_meta_path(agent, session),
            chat_cwd_path(agent, session),
            chat_thinking_path(agent, session),
            chat_cot_path(agent, session),
            chat_timer_reset_path(agent, session),
            chat_settings_path(agent, session),
            crate::session::resume_store::record_path(agent, session),
            crate::session::resume_store::previous_path(agent, session),
            crate::session::resume_store::takeover_marker_path(agent, session),
            crate::session::session::system_prompt_path(agent, session),
        ]);
    }
    paths.extend([
        agent.state_dir().join("sessions").join(tag),
        agent.state_dir().join("threads").join(tag),
        agent.state_dir().join("inbound").join(tag),
    ]);
    paths
}

/// 群已确定不可达后的本地状态清理。文件缺失视作幂等成功；单项失败只记录并继续，避免一个坏路径
/// 阻塞其余状态回收。返回实际删除的路径数，便于测试与日志核验。
pub(super) fn purge_agent_chat_state(
    agent: &Agent,
    chat_id: &str,
    session: &str,
    tag: &str,
) -> usize {
    // 历史版本可能给同一 chat 留下裸名/带 tag 多份 meta；先完整扫描，再统一去重删除。
    let mut sessions: Vec<String> = chat_sessions_on_disk(agent, chat_id)
        .into_iter()
        .map(|(session, _, _)| session)
        .collect();
    if !session.is_empty() && !sessions.iter().any(|known| known == session) {
        sessions.push(session.to_string());
    }
    if sessions.is_empty() {
        sessions.push(String::new());
    }
    let mut paths: Vec<PathBuf> = sessions
        .iter()
        .flat_map(|session| chat_disk_paths(agent, session, tag))
        .collect();
    paths.sort();
    paths.dedup();

    let mut removed = 0;
    for path in paths {
        let result = if path.is_dir() {
            std::fs::remove_dir_all(&path)
        } else {
            std::fs::remove_file(&path)
        };
        match result {
            Ok(()) => removed += 1,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => log(&summon_i18n::format!(
                "rd077900367",
                agent.name,
                path.display()
            )),
        }
    }
    removed
}

/// 读本会话工作目录覆盖。文件缺 / 空 → None(回退 agent 级 working_dir)。
pub(super) fn read_chat_cwd(agent: &Agent, session: &str) -> Option<String> {
    let s = std::fs::read_to_string(chat_cwd_path(agent, session)).ok()?;
    let v = s.trim();
    (!v.is_empty()).then(|| v.to_string())
}

/// 写本会话工作目录覆盖(传入已展开的路径)。副作用失败仅 log 忽略,不中断会话。
pub(super) fn write_chat_cwd(agent: &Agent, session: &str, cwd: &str) {
    if let Err(e) = write_chat_cwd_checked(agent, session, cwd) {
        log(&summon_i18n::format!("r1a0ccccbf4", agent.name));
    }
}

/// 需要“先写前置元数据、最后以 `.session` 为提交点”的召回路径使用 checked 版本。
pub(crate) fn write_chat_cwd_checked(
    agent: &Agent,
    session: &str,
    cwd: &str,
) -> anyhow::Result<()> {
    let path = chat_cwd_path(agent, session);
    let cwd = summon_core::work_dir::normalize_work_dir(cwd);
    let content = format!("{}\n", summon_core::chat_meta::sanitize_value(&cwd));
    atomic_write(&path, content.as_bytes())
}

use summon_core::settings::{ChatOverrides, ResolvedSettings, SettingKey};

/// 旧 `.timer_reset` 路径。只作为迁移输入与清理目标保留。
pub(super) fn chat_timer_reset_path(agent: &Agent, session: &str) -> PathBuf {
    agent
        .state_dir()
        .join("runtime")
        .join("chats")
        .join(format!("{session}.timer_reset"))
}

/// 轮外路径(面板渲染、空闲扫描)的一站式解析:读 per-chat 覆盖 + 传入的已解析 messaging。
///
/// **不在这里 `resolve_messaging`**:调用点往往已经算过一份,重算既浪费也可能拿到与调用点
/// 不同的代际,让同一次渲染里两个值来自两份配置。
pub(super) fn resolve_settings(
    agent: &Agent,
    session: &str,
    messaging: &Messaging,
) -> ResolvedSettings {
    ResolvedSettings::resolve(messaging, &read_chat_settings(agent, session))
}

/// per-chat 设置文件的写锁。`atomic_write` 只保证**单次写**不撕裂,拦不住「两个线程各读一份、
/// 各改一个键、后写的覆盖先写的」。写者都在 daemon 进程内(chat 线程的 `/set`、dispatcher 线程的
/// 面板按钮、chat 线程启动时的迁移),所以一把进程内的锁就够;写设置是低频动作,不必按 session 分锁。
static CHAT_SETTINGS_WRITE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// 改**一个**键并落盘,返回落盘后的完整状态供调用方更新内存态。
///
/// read-modify-write 收在这里一处并加锁:多个写者改的是同一个文件,谁拿旧基底做全量写,
/// 谁就会把对方刚落的值静默抹掉。
///
/// 读不动现有文件时**拒绝写**,而不是当作「无覆盖」继续 —— 后者会把用户已设的其它开关清零。
pub(super) fn set_chat_setting(
    agent: &Agent,
    session: &str,
    id: SettingKey,
    value: Option<bool>,
) -> anyhow::Result<ChatOverrides> {
    // 锁中毒说明另一个写者 panic 在临界区里,盘上内容未知 —— 取回内层继续,下面的 checked 读
    // 会独立地把损坏挡掉,不必因为一次别处的 panic 就永久拒绝所有写入。
    let _guard = CHAT_SETTINGS_WRITE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut next = read_chat_settings_checked(agent, session)?;
    match value {
        Some(v) => next.set(id, v),
        None => next.clear(id),
    }
    write_chat_settings(agent, session, next)?;
    Ok(next)
}

/// 控制台用:锁内先确认文件仍是读取时那一版(`unchanged` 看锁内读到的原始字节,文件不在为
/// `None`),再一次改多个键。与 [`set_chat_setting`] 同一把锁、同一个渲染,写出来的文件逐字相同。
/// 版本对不上返回 `Ok(None)`,文件不动。
pub(super) fn update_chat_settings_if(
    agent: &Agent,
    session: &str,
    changes: &[(SettingKey, Option<bool>)],
    unchanged: impl FnOnce(Option<&[u8]>) -> bool,
) -> anyhow::Result<Option<ChatOverrides>> {
    let _guard = CHAT_SETTINGS_WRITE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let current = match std::fs::read(chat_settings_path(agent, session)) {
        Ok(bytes) => Some(bytes),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    if !unchanged(current.as_deref()) {
        return Ok(None);
    }
    let mut next = read_chat_settings_checked(agent, session)?;
    for (id, value) in changes {
        match value {
            Some(v) => next.set(*id, *v),
            None => next.clear(*id),
        }
    }
    write_chat_settings(agent, session, next)?;
    Ok(Some(next))
}

/// 读旧 `.thinking`(单值 `on`/`off`)。只作为迁移输入。
fn read_legacy_thinking(agent: &Agent, session: &str) -> Option<bool> {
    parse_switch(&std::fs::read_to_string(chat_thinking_path(agent, session)).ok()?)
}

/// 读旧 `.timer_reset`(单值 `on`/`off`)。只作为迁移输入。
fn read_legacy_timer_reset(agent: &Agent, session: &str) -> Option<bool> {
    parse_switch(&std::fs::read_to_string(chat_timer_reset_path(agent, session)).ok()?)
}

fn parse_switch(value: &str) -> Option<bool> {
    match value.trim() {
        "on" => Some(true),
        "off" => Some(false),
        _ => None,
    }
}

/// 读旧 `.cot`(多行 `key=on|off|inherit`,亦兼容更早的单值形态=visible)。只作为迁移输入。
/// 读不动就返回错误,由调用方决定是保留旧文件重试还是按未覆盖处理 —— **不在这里吞掉**:
/// 吞掉等于把用户已经关掉的东西悄悄开回来。
fn read_legacy_cot(agent: &Agent, session: &str) -> anyhow::Result<Option<ChatOverrides>> {
    let path = chat_cot_path(agent, session);
    let content = match std::fs::read_to_string(&path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let mut out = ChatOverrides::default();
    let trimmed = content.trim();
    if trimmed.is_empty() {
        anyhow::bail!(summon_i18n::format!("raaa80308d7"));
    }
    if let Some(value) = parse_switch(trimmed) {
        out.set(SettingKey::CotVisible, value);
        return Ok(Some(out));
    }
    let mut seen = std::collections::HashSet::new();
    for line in content
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        let (key, value) = line
            .split_once('=')
            .ok_or_else(|| anyhow::anyhow!(summon_i18n::format!("rdfad9ba6db")))?;
        let key = key.trim();
        let id = match key {
            "visible" => SettingKey::CotVisible,
            "thinking" => SettingKey::CotThinking,
            "tools" => SettingKey::CotToolDetails,
            other => anyhow::bail!(summon_i18n::format!("rca6824b6f2")),
        };
        if !seen.insert(key) {
            anyhow::bail!(summon_i18n::format!("ra84bd52608"));
        }
        match value.trim() {
            "on" => out.set(id, true),
            "off" => out.set(id, false),
            "inherit" => out.clear(id),
            other => anyhow::bail!(summon_i18n::format!("r37c9661bb3")),
        }
    }
    Ok(Some(out))
}

/// 读本会话的分级开关覆盖。文件缺 = 全继承;文件读不动/格式非法 → 错误上抛。
pub(super) fn read_chat_settings_checked(
    agent: &Agent,
    session: &str,
) -> anyhow::Result<ChatOverrides> {
    let path = chat_settings_path(agent, session);
    match std::fs::read_to_string(&path) {
        Ok(content) => Ok(ChatOverrides::parse(&content)?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(ChatOverrides::default()),
        Err(error) => Err(error.into()),
    }
}

/// 轮首刷新用:认不出的 key 跳过(留日志),其余照常生效;没有文件按未覆盖处理;读文件出错
/// (权限、IO)返回 `None`,调用方沿用内存里那份 —— 把读失败当成「未覆盖」会清零已设的开关。
pub(super) fn read_chat_settings_for_turn(agent: &Agent, session: &str) -> Option<ChatOverrides> {
    let text = match std::fs::read_to_string(chat_settings_path(agent, session)) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Some(ChatOverrides::default())
        }
        Err(error) => {
            log(&summon_i18n::format!("r2d8a87ddf8", agent.name));
            return None;
        }
    };
    let (value, unknown) = ChatOverrides::parse_lenient(&text);
    if !unknown.is_empty() {
        log(&summon_i18n::format!(
            "rb945706f56",
            agent.name,
            unknown.join(", ")
        ));
    }
    Some(value)
}

/// 读取用的宽容版:认不出的 key 跳过(留日志),读不到文件按未覆盖处理。
///
/// **只用在读了也不改盘的路径**(渲染、扫描、`/set` 列表);写路径必须走 checked 版 ——
/// 那里认不出就得拒绝写,否则全量覆盖会把认不出的那些键从文件里抹掉。
pub(super) fn read_chat_settings(agent: &Agent, session: &str) -> ChatOverrides {
    let text = match std::fs::read_to_string(chat_settings_path(agent, session)) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return ChatOverrides::default()
        }
        Err(error) => {
            log(&summon_i18n::format!("r95abbc9a6a", agent.name));
            return ChatOverrides::default();
        }
    };
    let (value, unknown) = ChatOverrides::parse_lenient(&text);
    if !unknown.is_empty() {
        log(&summon_i18n::format!(
            "r721e8ae8c5",
            agent.name,
            unknown.join(", ")
        ));
    }
    value
}

/// **全量**原子提交:`value` 是文件的完整目标状态,不是增量。
///
/// 因此调用方必须先读一份**新鲜**的现状作基底 —— 拿内存里的旧副本来写,会把这期间别的
/// 写者落的值一并抹掉。改单个键请用 [`set_chat_setting`],它把这件事做在函数里,
/// 让「用陈旧基底」这种用法根本表达不出来;本函数只留给迁移(那里的语义本来就是全量重写)。
fn write_chat_settings(agent: &Agent, session: &str, value: ChatOverrides) -> anyhow::Result<()> {
    atomic_write(
        &chat_settings_path(agent, session),
        value.render().as_bytes(),
    )
}

/// 把三份旧的 per-chat 设置文件(`.cot` / `.thinking` / `.timer_reset`)并进 `.settings`。
///
/// 顺序是**先写新、后删旧**:任一步失败都保留旧文件,下次进程起来继续 —— 反过来会在
/// 「删了旧、没写成新」的窗口里把用户的设置清零,而那是不可恢复的。
/// 已经有 `.settings` 时,旧文件只补它没有的键(新文件是权威,迁移不覆盖它)。
pub(super) fn migrate_chat_settings(agent: &Agent, session: &str) -> anyhow::Result<ChatOverrides> {
    // 迁移也是同一个文件上的 read-modify-write,是第三个写者(前两个是 `/set` 与面板按钮),
    // 所以同样要持锁 —— 它恰好跑在 chat 线程创建那一刻,而上一代 daemon 留下的会话卡此刻
    // 仍然点得动。
    let _guard = CHAT_SETTINGS_WRITE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut value = read_chat_settings_checked(agent, session)?;
    let legacy_cot = read_legacy_cot(agent, session)?;
    let legacy_thinking = read_legacy_thinking(agent, session);
    let legacy_timer = read_legacy_timer_reset(agent, session);
    let cot_path = chat_cot_path(agent, session);
    let thinking_path = chat_thinking_path(agent, session);
    let timer_path = chat_timer_reset_path(agent, session);
    // `.timer_reset` 与 `.thinking` 存在但内容非法时,`read_legacy_*` 返回 None。此时不能
    // 把文件删掉当作迁移完成 —— 内容非法说明有人手改过,留着让人看见比悄悄抹掉强。
    let thinking_legal = !thinking_path.exists() || legacy_thinking.is_some();
    let timer_legal = !timer_path.exists() || legacy_timer.is_some();
    if legacy_cot.is_none() && legacy_thinking.is_none() && legacy_timer.is_none() {
        return Ok(value);
    }
    if let Some(cot) = legacy_cot {
        for id in [
            SettingKey::CotVisible,
            SettingKey::CotThinking,
            SettingKey::CotToolDetails,
        ] {
            if value.get(id).is_none() {
                if let Some(v) = cot.get(id) {
                    value.set(id, v);
                }
            }
        }
    }
    if value.get(SettingKey::CotThinking).is_none() {
        if let Some(v) = legacy_thinking {
            value.set(SettingKey::CotThinking, v);
        }
    }
    if value.get(SettingKey::TimerIdleRetireReset).is_none() {
        if let Some(v) = legacy_timer {
            value.set(SettingKey::TimerIdleRetireReset, v);
        }
    }
    // `value` 已是「读新鲜现状 + 补旧文件缺键」的结果,全量写是它本来的语义。
    write_chat_settings(agent, session, value)?;
    for (path, removable) in [
        (cot_path, true),
        (thinking_path, thinking_legal),
        (timer_path, timer_legal),
    ] {
        if removable && path.exists() {
            std::fs::remove_file(&path)?;
        }
    }
    Ok(value)
}

/// 本会话实际生效的工作目录。三级优先级(高→低):
/// 1. per-chat `.cwd` 文件(`/cd` 设的,运行时可变)
/// 2. `chat_working_dirs` 配置映射(按 chat_id 绑定,持久但可被 /cd 覆盖)
/// 3. agent 级 `runtime.working_dir`(默认)
///
/// `chat_id` 用于查第 2 级映射;传 None 则跳过第 2 级。
///
/// **纯配置视图,不校验目录是否存在** —— 面板展示 / 表单回填要如实回显用户配了什么,转录推导
/// 也自带 SID 搜兜底。**起进程一律走 `start_work_dir`**(带存在性回退 + 明确失败)。
pub(crate) fn effective_work_dir(
    agent: &Agent,
    session: &str,
    chat_id: Option<&str>,
) -> Option<String> {
    effective_work_dir_with_source(agent, session, chat_id).map(|(path, _)| path)
}

/// 同 [`effective_work_dir`],但额外带回**命中的来源层**(面板灰字标「哪来的」用)。
/// 与 `effective_work_dir` 共用同一条优先级链,单点定义避免两处漂移。
pub(crate) fn effective_work_dir_with_source(
    agent: &Agent,
    session: &str,
    chat_id: Option<&str>,
) -> Option<(String, summon_core::work_dir::WorkDirSource)> {
    use summon_core::work_dir::WorkDirSource;
    // 1. per-chat .cwd(/cd 设的,最高优先级)
    if let Some(wd) = read_chat_cwd(agent, session) {
        return Some((wd, WorkDirSource::ChatOverride));
    }
    // 2. chat_working_dirs 配置映射(按 chat_id 持久绑定)
    if let Some(cid) = chat_id {
        if let Some(wd) = agent.chat_working_dir(cid) {
            return Some((wd, WorkDirSource::ChatMapping));
        }
    }
    // 3. agent 级 working_dir(默认)
    agent
        .working_dir()
        .map(|wd| (wd, WorkDirSource::AgentDefault))
}

/// 工作目录候选链(显式 → per-chat 覆盖 → chat 映射 → agent 默认),逐候选探存在性后交
/// `plan_work_dir` 判定。**起进程与转录定位共用这一条链** —— 两处对「claude 到底在哪个目录
/// 跑」的理解必须一致:2026-08-18 首次唤起必超时事故,就是起进程侧校验存在性、转录定位侧
/// 不校验,陈旧 `.cwd` 下两边算出不同目录,watcher 死盯永不出现的路径。
pub(crate) fn work_dir_plan(
    agent: &Agent,
    session: &str,
    chat_id: Option<&str>,
    explicit: Option<&str>,
) -> summon_core::work_dir::WorkDirPlan {
    use summon_core::work_dir::{plan_work_dir, WorkDirCandidate, WorkDirSource};

    let mut candidates = Vec::new();
    let mut push = |source: WorkDirSource, path: Option<String>| {
        if let Some(path) = path.filter(|p| !p.trim().is_empty()) {
            let exists = std::path::Path::new(&path).is_dir();
            candidates.push(WorkDirCandidate::new(source, path, exists));
        }
    };
    push(WorkDirSource::Explicit, explicit.map(str::to_string));
    push(WorkDirSource::ChatOverride, read_chat_cwd(agent, session));
    push(
        WorkDirSource::ChatMapping,
        chat_id.and_then(|cid| agent.chat_working_dir(cid)),
    );
    push(WorkDirSource::AgentDefault, agent.working_dir());

    plan_work_dir(candidates)
}

/// 本会话**起进程时真正该用**的工作目录。与 `effective_work_dir`(纯配置优先级,给面板展示 /
/// 转录推导用)的区别是这里**逐级校验目录真实存在**:
///
/// - 陈旧覆盖(`/cd` 到的目录后来被删)自动降级到下一级,并打 warning 让 owner 能发现;
/// - 配了目录但一级都不存在 → **Err**,由调用方明确失败。绝不能返回 `None` ——
///   调用方的 `None` 兜底是「本进程 cwd」,而 `tmux new-session -c <不存在>` 更是静默落到 `$HOME`,
///   两条路都会把爆炸半径从一个沙箱目录放大到整个家目录(真机实测)。
/// - `Ok(None)` 只表示「压根没配工作目录」,调用方沿用既有兜底,行为不变。
///
/// `explicit` 是调用方显式指定的目录(task/worker 继承父任务目录、bridge 恢复外部会话 cwd),
/// 排在链首。
pub(crate) fn start_work_dir(
    agent: &Agent,
    session: &str,
    chat_id: Option<&str>,
    explicit: Option<&str>,
) -> anyhow::Result<Option<String>> {
    use summon_core::work_dir::WorkDirPlan;

    match work_dir_plan(agent, session, chat_id, explicit) {
        WorkDirPlan::Use {
            path,
            source,
            stale,
        } => {
            for skipped in &stale {
                log(&summon_i18n::format!(
                    "r7530b65603",
                    agent.name,
                    skipped.source.label(),
                    skipped.path,
                    source.label(),
                    chat_id.unwrap_or("-"),
                ));
            }
            Ok(Some(path))
        }
        WorkDirPlan::AllMissing { stale } => {
            let detail = stale
                .iter()
                .map(|c| format!("{}={}", c.source.label(), c.path))
                .collect::<Vec<_>>()
                .join(";");
            anyhow::bail!(summon_i18n::format!("rd80168715a"))
        }
        WorkDirPlan::Unconfigured => Ok(None),
    }
}

/// 启动侧回退过陈旧配置候选时,给用户的提醒卡正文(旧路径失效、回退到哪、怎么 `/cd` 重设);
/// 没有陈旧候选 → `None`。**只读本仓配置 + 探目录存在性,不发消息** —— 发卡由调用方在有
/// frontend 的路径做(chat 线程启动点)。陈旧 `.cwd` 覆盖不自动清(owner 2026-08-18 明确):
/// 提醒的目的是让用户自己发 `/cd <新路径>` 换掉它。
pub(crate) fn stale_work_dir_notice(
    agent: &Agent,
    session: &str,
    chat_id: Option<&str>,
) -> Option<String> {
    use summon_core::work_dir::WorkDirPlan;

    match work_dir_plan(agent, session, chat_id, None) {
        WorkDirPlan::Use {
            path,
            source,
            stale,
        } if !stale.is_empty() => {
            summon_core::work_dir::stale_work_dir_notice(&stale, &path, source)
        }
        _ => None,
    }
}

/// 某 chat 在盘上的会话信息(`(session 名, chat_type, creator)`)。daemon 重启后内存 `chats` 空,
/// 收到 ChatGone 事件需从 `.meta` 重建 Session 收尾用。扫 `runtime/chats/*.meta` 找 `chat_id=` 命中者。
pub(super) fn chat_sessions_on_disk(
    agent: &Agent,
    chat_id: &str,
) -> Vec<(String, String, Option<String>)> {
    chat_sessions_on_disk_by(agent, chat_id, |session| {
        crate::session::tmux::Tmux::new(session.to_string()).has_session()
    })
}

/// 同 [`chat_sessions_on_disk`],会话活没活着由调用方判:一次要看很多个 chat 时(控制台的群列表)
/// 先取一份 tmux 会话清单,逐个判活只查这份清单。
pub(crate) fn chat_sessions_on_disk_by(
    agent: &Agent,
    chat_id: &str,
    is_live: impl Fn(&str) -> bool,
) -> Vec<(String, String, Option<String>)> {
    let dir = agent.state_dir().join("runtime").join("chats");
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut matches = Vec::new();
    for e in entries.flatten() {
        let p = e.path();
        let Some(session) = p
            .file_name()
            .and_then(|s| s.to_str())
            .and_then(|s| s.strip_suffix(".meta"))
        else {
            continue;
        };
        let Ok(content) = std::fs::read_to_string(&p) else {
            continue;
        };
        let m = summon_core::chat_meta::ChatMeta::parse(&content);
        if m.chat_id.as_deref() == Some(chat_id) {
            let live = is_live(session);
            let modified = e
                .metadata()
                .and_then(|metadata| metadata.modified())
                .ok()
                .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|duration| duration.as_millis())
                .unwrap_or(0);
            matches.push((
                live,
                m.updated_at.unwrap_or_default(),
                modified,
                session.to_string(),
                m.chat_type.unwrap_or_else(|| "group".to_string()),
                m.creator,
            ));
        }
    }
    // 历史版本可能给同一 chat 留下裸名/带 tag 两份 .meta。优先仍存活的 tmux，其次取最新记录；
    // 绝不依赖 read_dir 的未定义顺序去 resume/拆除随机会话。
    matches.sort_by(|a, b| (a.0, &a.1, a.2, &a.3).cmp(&(b.0, &b.1, b.2, &b.3)));
    matches
        .into_iter()
        .map(|(_, _, _, session, chat_type, creator)| (session, chat_type, creator))
        .collect()
}

pub(super) fn chat_session_on_disk(
    agent: &Agent,
    chat_id: &str,
) -> Option<(String, String, Option<String>)> {
    chat_sessions_on_disk(agent, chat_id).pop()
}

/// chat_id → 当前会话名:优先盘上 `.meta` 记录的(owner 裸名等历史命名),缺失按规则 derive
/// (无 sender → 非 owner)。ScheduledFire 投递与 schedules 面板开关共用同一份推导,
/// 保证 per-chat 覆盖文件键到真实会话。
pub(super) fn chat_session_name(agent: &Agent, chat_type: &str, chat_id: &str) -> String {
    chat_session_on_disk(agent, chat_id)
        .map(|(session, _, _)| session)
        .unwrap_or_else(|| agent.tmux_name_for_chat(chat_type, chat_id, false))
}

/// 同 [`chat_session_name`],会话活没活着按调用方给的 tmux 会话清单判。
pub(crate) fn chat_session_name_among(
    agent: &Agent,
    chat_type: &str,
    chat_id: &str,
    tmux: &[String],
) -> String {
    chat_sessions_on_disk_by(agent, chat_id, |session| tmux.iter().any(|t| t == session))
        .pop()
        .map(|(session, _, _)| session)
        .unwrap_or_else(|| agent.tmux_name_for_chat(chat_type, chat_id, false))
}

/// per-chat sidecar(`.timer_reset` 等)的键:**优先活 handle 的 `tmux_name`** —— idle 扫描
/// 读 sidecar 用的就是它,是 ground truth;无 handle(会话已退/daemon 重启)才回退
/// `chat_session_name`(读 `.meta`,缺失再 derive)。回退路径在 owner 私聊无 `.meta` 时可能
/// 与入站动态命名(is_owner_p2p)分叉,但那时本就没有活会话、sidecar 无人读,等会话建起来
/// `.meta` 落盘后键自然对齐。
pub(super) fn sidecar_session_key(
    live_name: Option<&str>,
    agent: &Agent,
    chat_type: &str,
    chat_id: &str,
) -> String {
    live_name
        .map(str::to_string)
        .unwrap_or_else(|| chat_session_name(agent, chat_type, chat_id))
}

/// A5:「设置本会话工作目录」的唯一实现——`/cd` 命令与面板 set_workdir 两个入口共用。
/// expand_tilde → is_dir 校验(失败发回执返回 None)→ write_chat_cwd + update .session cwd。
/// 返回 Some(折叠展示路径)表示成功,调用方据此注入 /cd 并给用户回执。
pub(super) fn apply_chat_workdir(
    agent: &Agent,
    fc: &FeishuClient,
    chat_type: &str,
    chat_id: &str,
    session: &str,
    raw_dir: &str,
) -> Option<String> {
    let expanded = summon_core::registry::expand_tilde(raw_dir);
    // 回执展示折叠 home(任务6:不暴露 /Users/<人>);落盘/日志用真实展开路径。
    let shown = summon_core::registry::collapse_tilde(&expanded);
    if !std::path::Path::new(&expanded).is_dir() {
        let _ = send_markdown_archived(
            fc,
            &agent.state_dir(),
            chat_type,
            chat_id,
            &summon_i18n::format!("rb15a201940"),
        );
        return None;
    }
    write_chat_cwd(agent, session, &expanded);
    // 同步把 .session.cwd 更新成新目录:`.session.cwd` 语义 = 会话「当前生效目录」(见
    // session_id.rs),`/cd` 后即以此为准。转录未必随 /cd 搬家也无碍——resolve_transcript_path
    // 优先级 #1 会先验「该 cwd 下转录存在」,不在则按 SID 搜自愈(C6),两侧语义一致。
    crate::session::resume_store::update_cwd(agent, session, &expanded);
    log(&summon_i18n::format!("rba4da354e2", agent.name));
    Some(shown)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 群列表一次取 tmux 清单判活:同一 chat 留了两份 `.meta` 时,清单里在的那份优先,与逐个问
    /// tmux 的 `chat_session_name` 同一优先序。
    #[test]
    fn session_name_among_prefers_the_listed_live_session() {
        let (agent, _tmp) = crate::test_support::agent_dir("claude", "");
        for session in ["soul-test-a", "soul-test-b"] {
            write_chat_meta(
                &agent,
                session,
                "oc_x",
                "group",
                Some("测试群"),
                "someone",
                None,
            );
        }
        for live in ["soul-test-a", "soul-test-b"] {
            let listed = vec![live.to_string()];
            assert_eq!(
                chat_session_name_among(&agent, "group", "oc_x", &listed),
                live
            );
        }
    }

    /// 空闲计时器的起点必须来自盘上的 `updated_at`,而不是「此刻」——否则 daemon 每重启一次,
    /// 所有 chat 的计时一起清零:既集体同相推卡,又让空闲会话无限续命(owner 2026-08-13 实测)。
    #[test]
    fn idle_clock_starts_from_disk_not_from_now() {
        let (agent, _tmp) = crate::test_support::agent_dir("claude", "");
        let session = "soul-test-g1234";
        write_chat_meta(
            &agent,
            session,
            "oc_x",
            "group",
            Some("测试群"),
            "someone",
            None,
        );

        let restored = read_chat_last_activity(&agent, session).expect("刚写的 meta 必须能读回");
        let drift = std::time::SystemTime::now()
            .duration_since(restored)
            .expect("刚写的时刻不该在未来");
        // `.meta` 只精确到秒,允许一点截断误差;关键是它落在「刚刚」而不是回退成别的值。
        assert!(drift < std::time::Duration::from_secs(5), "误差 {drift:?}");
    }

    /// 读不出就回退 `now()`(调用方语义),而不是拿一个瞎猜的时刻去决定「该不该提醒/该不该退出」。
    /// 三种读不出:没有 meta、meta 没有 updated_at、时间串坏掉。
    #[test]
    fn unreadable_timestamp_yields_none_so_caller_falls_back() {
        let (agent, _tmp) = crate::test_support::agent_dir("claude", "");
        assert!(
            read_chat_last_activity(&agent, "soul-never-written").is_none(),
            "没有 .meta 时不能编一个时刻出来"
        );

        let session = "soul-test-broken";
        let path = chat_meta_path(&agent, session);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "chat_id=oc_x\nchat_type=group\n").unwrap();
        assert!(
            read_chat_last_activity(&agent, session).is_none(),
            "缺 updated_at 行 → None"
        );

        std::fs::write(&path, "chat_id=oc_x\nupdated_at=昨天下午\n").unwrap();
        assert!(
            read_chat_last_activity(&agent, session).is_none(),
            "时间串坏掉 → None,不能 panic 也不能猜"
        );
    }

    /// durable marker 账本的核心判定:`attempted_at` 落在 last_activity 之后 = 本段空闲期
    /// 已尝试过 → 不重发;用户之后再说话(last_activity 越过 attempted_at)= 新一段空闲期
    /// → 照常可发。同一段空闲期重写 marker 只是覆盖同一身份的凭证(按空闲期幂等)。
    #[test]
    fn idle_prompt_marker_dedups_period_but_new_activity_resets() {
        let (agent, _tmp) = crate::test_support::agent_dir("claude", "");
        let now = chrono::Local::now();
        let activity = std::time::SystemTime::from(now - chrono::Duration::seconds(3600));
        write_idle_prompt_marker(
            &agent,
            "p2p",
            "oc_x",
            activity,
            std::time::SystemTime::from(now - chrono::Duration::seconds(1800)),
        )
        .expect("marker 原子写应成功");
        assert!(
            read_idle_prompt_marker_since(&agent, "p2p", "oc_x", activity),
            "attempted_at 在活动之后 → 本段空闲期已尝试过,重启后不得再发"
        );

        // 卡之后用户又说过话(activity 越过 attempted_at)→ 新一段空闲期,照常可发。
        let newer_activity = std::time::SystemTime::from(now - chrono::Duration::seconds(600));
        assert!(
            !read_idle_prompt_marker_since(&agent, "p2p", "oc_x", newer_activity),
            "新活动重置空闲期后,下一段空闲期要照常发卡"
        );

        // 活动收口作废 marker → 回到「未尝试」。
        clear_idle_prompt_marker(&agent, "p2p", "oc_x");
        assert!(
            !read_idle_prompt_marker_since(&agent, "p2p", "oc_x", activity),
            "marker 作废后不再压卡"
        );
    }

    /// marker 缺失 → 未尝试过(可发)。先写后发的不变量使「没有 marker」确凿证明本机制没
    /// 尝试过 —— 去重不因 fail-closed 而把从未提醒过的会话静默饿死。(老版本发过卡但没有
    /// marker 的升级窗口由 `ensure_idle_prompt_marker_migration` 在任何扫描前补写收口,
    /// 见迁移测试;稳态下「无 marker」只剩这一种含义。)
    #[test]
    fn missing_idle_prompt_marker_means_never_attempted() {
        let (agent, _tmp) = crate::test_support::agent_dir("claude", "");
        let now = chrono::Local::now();
        let activity = std::time::SystemTime::from(now - chrono::Duration::seconds(3600));
        assert!(
            !read_idle_prompt_marker_since(&agent, "p2p", "oc_x", activity),
            "无 marker = 本机制从未尝试过 → 照常发"
        );
    }

    /// fail-closed 面:marker **存在**但证不了内容的,一律当发过(不发)——坏 JSON、
    /// 缺 attempted_at、时间串坏掉。漏一张无害,重发才是 bug。
    #[test]
    fn unreadable_idle_prompt_marker_fails_closed_to_not_send() {
        let (agent, _tmp) = crate::test_support::agent_dir("claude", "");
        let now = chrono::Local::now();
        let activity = std::time::SystemTime::from(now - chrono::Duration::seconds(3600));
        let path = idle_prompt_marker_path(&agent, "p2p", "oc_x");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();

        for broken in [
            r#"{"period_start":"2026-"#,                       // 撕裂 JSON
            r#"{"period_start":"2026-08-13T00:00:00+08:00"}"#, // 缺 attempted_at
            r#"{"attempted_at":"昨天"}"#,                      // 时间串坏掉
        ] {
            std::fs::write(&path, broken).unwrap();
            assert!(
                read_idle_prompt_marker_since(&agent, "p2p", "oc_x", activity),
                "marker 在但读不清({broken})→ 证不了「没发过」→ 不发"
            );
        }

        // 路径在但不是可读文件(读 IO 错,非 NotFound)→ 同样 fail-closed 不发。
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert!(
            read_idle_prompt_marker_since(&agent, "p2p", "oc_x", activity),
            "marker 路径存在但读不到 → 不发"
        );
    }

    /// 升级迁移的提交协议(终审处方②):任一 marker 补写失败 → **不写** migration-complete
    /// 标记、返回 Err(调用方拿不到类型证明,进不了扫描);障碍解除后**再次启动**从缺的
    /// 地方继续补齐,补齐后才宣布完成。变异自验钩子:把「部分失败不写 done」的门去掉
    /// (失败也写 done / 返回 Ok),本测试第一段或第二段必红。
    #[test]
    fn idle_marker_migration_partial_failure_keeps_gate_closed_then_resumes() {
        let (agent, _tmp) = crate::test_support::agent_dir("claude", "");
        let now = chrono::Local::now() - chrono::Duration::seconds(7200);
        // 两个升级前已有的 chat:A 正常;B 的 threads/<tag> 被一个同名普通文件占住 →
        // marker 落不了盘(建父目录必败)。B 只能经 .meta(来源②)枚举出来。
        for (session, chat_id) in [("soul-mig-a", "oc_mig_a"), ("soul-mig-b", "oc_mig_b")] {
            let path = chat_meta_path(&agent, session);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(
                &path,
                format!(
                    "chat_id={chat_id}\nchat_type=p2p\ntag={}\nupdated_at={}\n",
                    chat_tag("p2p", chat_id),
                    now.format("%Y-%m-%d %H:%M:%S")
                ),
            )
            .unwrap();
        }
        let tag_b = chat_tag("p2p", "oc_mig_b");
        std::fs::create_dir_all(agent.state_dir().join("threads")).unwrap();
        std::fs::write(agent.state_dir().join("threads").join(&tag_b), b"occupied").unwrap();

        // 第一次启动:B 写不进 → 整体 Err、done 不落盘;A 的 marker 照写(缩小续跑缺口)。
        assert!(
            ensure_idle_prompt_marker_migration(&agent).is_err(),
            "有 chat 补写失败时不得宣布迁移完成"
        );
        assert!(
            !agent
                .state_dir()
                .join("runtime")
                .join("idle-marker-migration-v1.done")
                .exists(),
            "部分失败不得写 migration-complete 标记"
        );
        assert!(
            idle_prompt_marker_path(&agent, "p2p", "oc_mig_a").is_file(),
            "能写的先写上,下次续跑缺口最小"
        );

        // 障碍解除后「下一次启动」:从缺的地方继续,补齐 B 才写 done。
        std::fs::remove_file(agent.state_dir().join("threads").join(&tag_b)).unwrap();
        assert!(ensure_idle_prompt_marker_migration(&agent).is_ok());
        assert!(
            idle_prompt_marker_path(&agent, "p2p", "oc_mig_b").is_file(),
            "续跑必须补齐上次失败的 chat"
        );
        assert!(agent
            .state_dir()
            .join("runtime")
            .join("idle-marker-migration-v1.done")
            .is_file());

        // done 之后迁移退场:新活动作废 marker 后,再启动**不得**重写(迁移只压当段,
        // 不许每次重启都把当前空闲期压掉)。
        clear_idle_prompt_marker(&agent, "p2p", "oc_mig_a");
        assert!(ensure_idle_prompt_marker_migration(&agent).is_ok());
        assert!(
            !idle_prompt_marker_path(&agent, "p2p", "oc_mig_a").exists(),
            "migration-complete 后不得再补写 marker"
        );
    }

    /// 覆盖面的另一半:只在 `threads/*` 留下目录、没有 `.meta` 的 chat(来源①),迁移同样
    /// 补写;已有 marker 的不覆盖(幂等,不吃掉真实发送写下的凭证)。
    #[test]
    fn idle_marker_migration_covers_threads_dirs_and_keeps_existing_markers() {
        let (agent, _tmp) = crate::test_support::agent_dir("claude", "");
        let tag_only = "p0011223344556677";
        std::fs::create_dir_all(agent.state_dir().join("threads").join(tag_only)).unwrap();
        // 已有 marker 的 chat:内容不该被迁移覆盖。
        let now = chrono::Local::now();
        write_idle_prompt_marker(
            &agent,
            "p2p",
            "oc_kept",
            std::time::SystemTime::from(now - chrono::Duration::seconds(3600)),
            std::time::SystemTime::from(now - chrono::Duration::seconds(1800)),
        )
        .unwrap();
        let kept =
            std::fs::read_to_string(idle_prompt_marker_path(&agent, "p2p", "oc_kept")).unwrap();

        assert!(ensure_idle_prompt_marker_migration(&agent).is_ok());
        assert!(
            agent
                .state_dir()
                .join("threads")
                .join(tag_only)
                .join("idle_prompt.marker")
                .is_file(),
            "threads/* 目录名(tag)是 pending 扫描的全部产出,必须在覆盖面内"
        );
        assert_eq!(
            std::fs::read_to_string(idle_prompt_marker_path(&agent, "p2p", "oc_kept")).unwrap(),
            kept,
            "已有 marker 不得被迁移覆盖"
        );
    }

    /// 终审阻塞的回归(处方④):枚举流**中途一个 Err** → 收集必须整体 Err,绝不静默跳过。
    /// `ReadDir` 的逐项 IO 错就长这样 —— 旧实现 `entries.flatten()` 把它吞成「跳过该项」,
    /// 漏掉的 tag 不补 marker 却照常写 done:重启恢复该 chat 后首轮扫描重发空闲卡,且 done
    /// 已落盘永不自愈。变异自验:把 `collect_migration_tags` 改回 flatten,本测试必红
    /// (Err 被吞成 Ok)。
    #[test]
    fn migration_tag_collection_rejects_mid_stream_entry_error() {
        let mut tags = std::collections::BTreeSet::new();
        let entries = [
            Ok(vec!["t1".to_string()]),
            Err(std::io::Error::other("readdir 逐项瞬时 IO 错")),
            Ok(vec!["t3".to_string()]),
        ];
        assert!(
            collect_migration_tags(entries.into_iter(), &mut tags).is_err(),
            "中途一个 Err 必须整体失败 —— 静默漏项 = 漏 tag 仍写 done,永不自愈"
        );

        // 成功路径:全 Ok 才收全(收集本身不丢项)。
        let mut ok_tags = std::collections::BTreeSet::new();
        collect_migration_tags(
            [
                Ok(vec!["a".to_string()]),
                Ok(Vec::new()),
                Ok(vec!["b".to_string()]),
            ]
            .into_iter(),
            &mut ok_tags,
        )
        .expect("全 Ok 的枚举必须成功");
        assert_eq!(ok_tags.into_iter().collect::<Vec<_>>(), ["a", "b"]);
    }

    /// 处方④的端到端半边:枚举中途一项证不了(`.meta` 在但读不出,非 NotFound)→
    /// ensure **不产证明、不写 done**;障碍解除后下次启动补齐覆盖面才宣布完成。
    #[test]
    fn migration_enumeration_error_yields_no_proof_and_no_done() {
        let (agent, _tmp) = crate::test_support::agent_dir("claude", "");
        let session = "soul-enum-ok";
        let path = chat_meta_path(&agent, session);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            format!(
                "chat_id=oc_enum\nchat_type=p2p\ntag={}\n",
                chat_tag("p2p", "oc_enum")
            ),
        )
        .unwrap();
        // 「文件在但读不出」:用同名目录冒充 .meta,read_to_string 必败且非 NotFound。
        let broken = agent
            .state_dir()
            .join("runtime")
            .join("chats")
            .join("broken.meta");
        std::fs::create_dir(&broken).unwrap();

        assert!(
            ensure_idle_prompt_marker_migration(&agent).is_err(),
            "枚举不完整不得产出 IdleMarkersMigrated 证明"
        );
        assert!(
            !agent
                .state_dir()
                .join("runtime")
                .join("idle-marker-migration-v1.done")
                .exists(),
            "枚举失败不得写 done"
        );

        std::fs::remove_dir(&broken).unwrap();
        assert!(ensure_idle_prompt_marker_migration(&agent).is_ok());
        assert!(
            idle_prompt_marker_path(&agent, "p2p", "oc_enum").is_file(),
            "障碍解除后续跑必须补齐覆盖面"
        );
    }

    /// 终审阻塞的另一半(处方②):`threads/*` 判目录不得用 `Path::is_dir()` —— 它把
    /// metadata 错误吞成 false,静默漏 tag 却照常写 done(事故序列完整走通且永不自愈)。
    /// 真实构造(不 mock,循 lifecycle.rs 惯例):threads/ 改 0o444(可读不可搜),
    /// 目录名仍枚举得出,但 stat 其子项必败 —— 旧实现在此漏 tag 写 done;新实现要么经
    /// d_type 收到 tag 而 marker 写失败、要么 file_type 需 stat 时直接 Err,两条路都不写 done。
    #[test]
    #[cfg(unix)]
    fn threads_metadata_error_must_not_be_swallowed_into_done() {
        use std::os::unix::fs::PermissionsExt;
        let (agent, _tmp) = crate::test_support::agent_dir("claude", "");
        let threads = agent.state_dir().join("threads");
        let tag = "p00deadbeef0000";
        std::fs::create_dir_all(threads.join(tag)).unwrap();

        let restore = |mode: u32| {
            let mut perms = std::fs::metadata(&threads).unwrap().permissions();
            perms.set_mode(mode);
            std::fs::set_permissions(&threads, perms).unwrap();
        };
        restore(0o444);
        // root 绕过权限检查,构造不出该故障 → 跳过而不是误判(仓内既有惯例)。
        if threads.join(tag).metadata().is_ok() {
            restore(0o755);
            return;
        }
        let outcome = ensure_idle_prompt_marker_migration(&agent);
        let done_written = agent
            .state_dir()
            .join("runtime")
            .join("idle-marker-migration-v1.done")
            .exists();
        // 先恢复权限,保证断言失败时 TempDir 仍能清理。
        restore(0o755);

        assert!(outcome.is_err(), "metadata 证不了的枚举不得产出证明");
        assert!(
            !done_written,
            "静默漏 tag 还写 done = 事故永不自愈,必须堵死"
        );

        // 障碍解除后续跑补齐才宣布完成。
        assert!(ensure_idle_prompt_marker_migration(&agent).is_ok());
        assert!(threads.join(tag).join("idle_prompt.marker").is_file());
    }

    /// 处方③:done 存在性检查不得吞 IO 错。`exists()` 把 metadata 错吞成 false = 把
    /// 「读不了迁移状态」解释成「没迁移过」→ 重跑迁移,把已被新活动作废的 marker 重新
    /// 压上(静默违反「done 之后迁移退场」,该不变量已有测试钉死)。try_exists 语义:
    /// Err 就上抛,本次启动不进扫描。真实构造:done 换成自指 symlink,stat 必 ELOOP。
    #[test]
    #[cfg(unix)]
    fn done_probe_io_error_is_not_read_as_unmigrated() {
        let (agent, _tmp) = crate::test_support::agent_dir("claude", "");
        let tag = "p00feedface0000";
        std::fs::create_dir_all(agent.state_dir().join("threads").join(tag)).unwrap();
        assert!(
            ensure_idle_prompt_marker_migration(&agent).is_ok(),
            "先正常完成一次迁移"
        );
        let marker = agent
            .state_dir()
            .join("threads")
            .join(tag)
            .join("idle_prompt.marker");
        // 新活动作废 marker —— done 之后迁移必须退场,不得重写它。
        std::fs::remove_file(&marker).unwrap();
        let done = agent
            .state_dir()
            .join("runtime")
            .join("idle-marker-migration-v1.done");
        std::fs::remove_file(&done).unwrap();
        std::os::unix::fs::symlink("idle-marker-migration-v1.done", &done).unwrap();

        assert!(
            ensure_idle_prompt_marker_migration(&agent).is_err(),
            "done 读不出 ≠ 未迁移:必须 Err 不进扫描,不得重跑迁移"
        );
        assert!(
            !marker.exists(),
            "IO 错被吞成「未迁移」会重跑迁移,把已作废的 marker 重新压上"
        );
    }

    /// 回归(2026-09-08 现场):`/cd /path/to/dir/` 落盘时把尾斜杠一起写进 `.cwd`,
    /// 转录定位据此 munge 出多一个尾 `-` 的 Claude 工程目录名,冷启首轮死等到超时。
    /// 落盘即归一,盘上就只存在同一目录的一种写法。
    #[test]
    fn chat_cwd_is_normalized_on_disk() {
        let tmp = tempfile::tempdir().unwrap();
        let mut agent: Agent =
            serde_yaml::from_str("name: test\nruntime:\n  cli: claude\n").unwrap();
        agent.dir = tmp.path().join("agents").join(&agent.name);
        std::fs::create_dir_all(&agent.dir).unwrap();
        std::fs::create_dir_all(agent.state_dir()).unwrap();
        let session = "soul-test-cwd-norm";

        write_chat_cwd(&agent, session, "/Users/me/Desktop/My_App/");

        assert_eq!(
            read_chat_cwd(&agent, session).as_deref(),
            Some("/Users/me/Desktop/My_App"),
            "落盘的工作目录必须与 CLI 进程自报的 cwd 同形"
        );
        assert_eq!(
            summon_core::claude_log::claude_transcript_path("/Users/me/Desktop/My_App/", "sid"),
            summon_core::claude_log::claude_transcript_path(
                read_chat_cwd(&agent, session).as_deref().unwrap(),
                "sid"
            ),
            "落盘前后推导出的转录路径必须是同一个"
        );
    }

    #[test]
    fn stale_work_dir_notice_reports_gone_override_and_fallback() {
        // 陈旧 .cwd 覆盖(目录已删)→ 提醒卡正文须含失效旧路径、回退后的现用目录与 /cd 重设指引。
        let tmp = tempfile::tempdir().unwrap();
        let fallback = tmp.path().join("home");
        std::fs::create_dir_all(&fallback).unwrap();
        let fallback_s = fallback.to_string_lossy().into_owned();
        let mut agent: Agent = serde_yaml::from_str(&format!(
            "name: test\nruntime:\n  cli: claude\n  working_dir: {fallback_s}"
        ))
        .unwrap();
        agent.dir = tmp.path().join("agents").join(&agent.name);
        std::fs::create_dir_all(&agent.dir).unwrap();
        std::fs::create_dir_all(agent.state_dir()).unwrap();
        let session = "soul-test-stale";
        write_chat_cwd(&agent, session, "/nonexistent-stale-cwd");
        let notice = stale_work_dir_notice(&agent, session, None)
            .expect("陈旧覆盖必须产出提醒,不能静默回退");
        assert!(
            notice.contains("/nonexistent-stale-cwd"),
            "提醒须含失效旧路径: {notice}"
        );
        assert!(
            notice.contains(&fallback_s),
            "提醒须含回退后的现用目录: {notice}"
        );
        assert!(
            notice.contains("/cd"),
            "提醒须教用户怎么用 /cd 重设: {notice}"
        );
    }

    #[test]
    fn stale_work_dir_notice_none_when_all_config_dirs_exist() {
        // 覆盖目录真实存在 → 没有回退,不提醒。
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let home_s = home.to_string_lossy().into_owned();
        let mut agent: Agent = serde_yaml::from_str(&format!(
            "name: test\nruntime:\n  cli: claude\n  working_dir: {home_s}"
        ))
        .unwrap();
        agent.dir = tmp.path().join("agents").join(&agent.name);
        std::fs::create_dir_all(&agent.dir).unwrap();
        std::fs::create_dir_all(agent.state_dir()).unwrap();
        let session = "soul-test-fresh";
        write_chat_cwd(&agent, session, &home_s);
        assert_eq!(
            stale_work_dir_notice(&agent, session, None),
            None,
            "没有陈旧候选时不提醒"
        );
    }

    /// 建一个只有 state_dir 的临时 agent。
    fn tmp_agent(tmp: &tempfile::TempDir) -> Agent {
        let mut agent: Agent = serde_yaml::from_str("name: test\nruntime:\n  cli: claude").unwrap();
        agent.dir = tmp.path().join("agents").join(&agent.name);
        std::fs::create_dir_all(&agent.dir).unwrap();
        std::fs::create_dir_all(agent.state_dir()).unwrap();
        agent
    }

    #[test]
    fn chat_settings_round_trip_every_chat_scoped_key() {
        let tmp = tempfile::tempdir().unwrap();
        let agent = tmp_agent(&tmp);
        let session = "soul-test-settings";
        assert_eq!(
            read_chat_settings(&agent, session),
            ChatOverrides::default(),
            "文件缺 = 全继承"
        );

        let mut expected = ChatOverrides::default();
        for (i, decl) in summon_core::settings::chat_scoped().enumerate() {
            expected.set(decl.id, i % 2 == 0);
        }
        write_chat_settings(&agent, session, expected).unwrap();
        assert_eq!(read_chat_settings(&agent, session), expected);

        // `inherit` 必须能写回去,而不是只能在 on/off 之间来回。
        let mut cleared = expected;
        cleared.clear(SettingKey::CotVisible);
        write_chat_settings(&agent, session, cleared).unwrap();
        let read_back = read_chat_settings(&agent, session);
        assert_eq!(read_back.get(SettingKey::CotVisible), None);
        assert_eq!(read_back, cleared);
    }

    /// 写盘失败必须让状态原封不动 —— 半途提交会让盘上与内存各说各话。
    #[test]
    fn chat_settings_write_failure_does_not_commit_state() {
        let tmp = tempfile::tempdir().unwrap();
        let agent = tmp_agent(&tmp);
        let blocked = "soul-test-settings-blocked";
        let chats = agent.state_dir().join("runtime").join("chats");
        let _ = std::fs::remove_dir_all(&chats);
        std::fs::create_dir_all(chats.parent().unwrap()).unwrap();
        std::fs::write(&chats, b"not a directory").unwrap();

        let mut value = ChatOverrides::default();
        value.set(SettingKey::AgentReactions, false);
        assert!(write_chat_settings(&agent, blocked, value).is_err());
        assert_eq!(
            read_chat_settings(&agent, blocked),
            ChatOverrides::default()
        );
    }

    /// 已有 `.settings` 损坏时**拒绝写入**:拿一份读不出来的现状去做全量覆盖,
    /// 等于把用户已设的其它开关静默清零。
    #[test]
    fn setting_a_key_refuses_when_the_existing_file_is_corrupt() {
        let tmp = tempfile::tempdir().unwrap();
        let agent = tmp_agent(&tmp);
        let session = "soul-test-settings-corrupt";
        std::fs::create_dir_all(chat_settings_path(&agent, session).parent().unwrap()).unwrap();
        std::fs::write(chat_settings_path(&agent, session), b"cot.visible=maybe\n").unwrap();

        assert!(
            set_chat_setting(&agent, session, SettingKey::AgentReactions, Some(false)).is_err()
        );
        assert_eq!(
            std::fs::read(chat_settings_path(&agent, session)).unwrap(),
            b"cot.visible=maybe\n",
            "拒绝写入时原文件不得被动过"
        );
    }

    /// 三份旧文件一次并入,且旧文件在新文件写成功之后才删。
    #[test]
    fn legacy_files_migrate_once_into_unified_settings() {
        let tmp = tempfile::tempdir().unwrap();
        let agent = tmp_agent(&tmp);
        let session = "soul-test-migrate";
        std::fs::create_dir_all(chat_cot_path(&agent, session).parent().unwrap()).unwrap();
        // 更早的单值 `.cot` 形态 = visible。
        std::fs::write(chat_cot_path(&agent, session), b"off\n").unwrap();
        std::fs::write(chat_thinking_path(&agent, session), b"on\n").unwrap();
        std::fs::write(chat_timer_reset_path(&agent, session), b"on\n").unwrap();

        let migrated = migrate_chat_settings(&agent, session).unwrap();
        assert_eq!(migrated.get(SettingKey::CotVisible), Some(false));
        assert_eq!(migrated.get(SettingKey::CotThinking), Some(true));
        assert_eq!(migrated.get(SettingKey::TimerIdleRetireReset), Some(true));
        for legacy in [
            chat_cot_path(&agent, session),
            chat_thinking_path(&agent, session),
            chat_timer_reset_path(&agent, session),
        ] {
            assert!(!legacy.exists(), "迁移完成后旧文件必须已删除: {legacy:?}");
        }
        // 幂等:再跑一次读到同一份,且不再有旧文件可迁。
        assert_eq!(migrate_chat_settings(&agent, session).unwrap(), migrated);
        assert_eq!(read_chat_settings(&agent, session), migrated);
    }

    /// 已有 `.settings` 是权威:迁移只补它没有的键,不覆盖已设的值。
    #[test]
    fn migration_never_overwrites_values_already_in_new_file() {
        let tmp = tempfile::tempdir().unwrap();
        let agent = tmp_agent(&tmp);
        let session = "soul-test-migrate-precedence";
        std::fs::create_dir_all(chat_cot_path(&agent, session).parent().unwrap()).unwrap();
        let mut existing = ChatOverrides::default();
        existing.set(SettingKey::CotVisible, true);
        write_chat_settings(&agent, session, existing).unwrap();
        std::fs::write(chat_cot_path(&agent, session), b"visible=off\ntools=off\n").unwrap();

        let migrated = migrate_chat_settings(&agent, session).unwrap();
        assert_eq!(
            migrated.get(SettingKey::CotVisible),
            Some(true),
            "新文件已有的键不得被旧文件覆盖"
        );
        assert_eq!(
            migrated.get(SettingKey::CotToolDetails),
            Some(false),
            "新文件没有的键要从旧文件补上"
        );
    }

    /// 旧 `.cot` 损坏 → 整个迁移中止,三份输入原样留在盘上等下次。
    /// 半途删除会把用户已设的开关清零,而那不可恢复。
    #[test]
    fn migration_preserves_every_input_when_one_is_corrupt() {
        let tmp = tempfile::tempdir().unwrap();
        let agent = tmp_agent(&tmp);
        let session = "soul-test-corrupt-cot";
        std::fs::create_dir_all(agent.state_dir().join("runtime/chats")).unwrap();
        std::fs::write(chat_cot_path(&agent, session), b"visible=maybe\n").unwrap();
        std::fs::write(chat_thinking_path(&agent, session), b"on\n").unwrap();

        assert!(migrate_chat_settings(&agent, session).is_err());
        assert_eq!(
            std::fs::read(chat_cot_path(&agent, session)).unwrap(),
            b"visible=maybe\n"
        );
        assert!(chat_thinking_path(&agent, session).exists());
        assert!(!chat_settings_path(&agent, session).exists());
    }

    /// 旧 `.timer_reset` 内容非法:值不迁,但文件也**不删** —— 内容非法说明有人手改过,
    /// 留着让人看见比悄悄抹掉强。
    #[test]
    fn migration_keeps_illegible_legacy_file_instead_of_deleting_it() {
        let tmp = tempfile::tempdir().unwrap();
        let agent = tmp_agent(&tmp);
        let session = "soul-test-illegible";
        std::fs::create_dir_all(agent.state_dir().join("runtime/chats")).unwrap();
        std::fs::write(chat_thinking_path(&agent, session), b"on\n").unwrap();
        std::fs::write(chat_timer_reset_path(&agent, session), b"garbage\n").unwrap();

        let migrated = migrate_chat_settings(&agent, session).unwrap();
        assert_eq!(migrated.get(SettingKey::CotThinking), Some(true));
        assert_eq!(migrated.get(SettingKey::TimerIdleRetireReset), None);
        assert!(
            !chat_thinking_path(&agent, session).exists(),
            "可读的旧文件已并入,删掉"
        );
        assert!(
            chat_timer_reset_path(&agent, session).exists(),
            "读不懂的旧文件留在盘上"
        );
    }

    /// 两个写者改同一个文件时,后写的不得抹掉先写的。
    ///
    /// `/set` 走 chat 线程、面板按钮走 dispatcher 线程,而文件是全量覆盖写。这条测试模拟
    /// 「chat 线程手里的副本已经陈旧」的真实时序:面板刚落了一个值,`/set` 若拿旧副本做基底,
    /// 那个值就会无声无息地回到 inherit。
    #[test]
    fn a_second_writer_never_clobbers_what_the_first_one_just_committed() {
        let tmp = tempfile::tempdir().unwrap();
        let agent = tmp_agent(&tmp);
        let session = "soul-test-two-writers";

        // chat 线程启动时读到的副本:此刻文件还不存在,全继承。
        let stale = read_chat_settings(&agent, session);
        assert_eq!(stale, ChatOverrides::default());

        // 另一个写者(面板按钮)落盘一个值。
        set_chat_setting(
            &agent,
            session,
            SettingKey::TimerIdleRetireReset,
            Some(true),
        )
        .unwrap();

        // `/set` 改另一个键。它必须以**盘上现状**为基底,不是手里那份 `stale`。
        let after =
            set_chat_setting(&agent, session, SettingKey::AgentReactions, Some(false)).unwrap();

        assert_eq!(
            after.get(SettingKey::TimerIdleRetireReset),
            Some(true),
            "另一个写者刚落盘的值被覆盖掉了"
        );
        assert_eq!(after.get(SettingKey::AgentReactions), Some(false));
        assert_eq!(
            read_chat_settings(&agent, session),
            after,
            "返回值须与盘上一致"
        );
    }

    /// 「先写新、后删旧」这个顺序必须钉在源码上。
    ///
    /// 它在**正常路径上完全看不出区别** —— 两种顺序跑出来的最终状态一模一样,只有在写失败的
    /// 那一瞬才分叉:反过来的话旧文件已删、新文件没写成,用户的设置被清零且不可恢复。
    /// 用文件系统去构造那一瞬很难(能让写失败的手段通常同时让读或删也失败,于是测试在
    /// 更早的地方就返回了,顺序根本没被执行到),所以这里直接断言顺序本身。
    #[test]
    fn migration_writes_the_new_file_before_deleting_any_legacy_one() {
        let source = include_str!("chat_meta.rs");
        let body = source
            .split("pub(super) fn migrate_chat_settings(")
            .nth(1)
            .and_then(|tail| tail.split("\n/// ").next())
            .expect("迁移函数必须存在");
        let write = body
            .find("write_chat_settings(agent, session, value)?")
            .expect("必须写新文件");
        let remove = body.find("std::fs::remove_file(").expect("必须删旧文件");
        assert!(write < remove, "必须先写新文件,再删旧文件");
    }

    /// `.settings` 读不出来时整个迁移中止,旧文件一份不少地留在盘上等下次。
    #[test]
    fn migration_keeps_legacy_inputs_when_the_new_file_cannot_be_read() {
        let tmp = tempfile::tempdir().unwrap();
        let agent = tmp_agent(&tmp);
        let session = "soul-test-migrate-writefail";
        let chats = agent.state_dir().join("runtime").join("chats");
        std::fs::create_dir_all(&chats).unwrap();
        std::fs::write(chat_thinking_path(&agent, session), b"on\n").unwrap();
        std::fs::write(chat_timer_reset_path(&agent, session), b"on\n").unwrap();
        // 让 `.settings` 这个**路径**变成目录 → 原子写必失败,而旧文件读取不受影响。
        std::fs::create_dir_all(chat_settings_path(&agent, session)).unwrap();

        assert!(migrate_chat_settings(&agent, session).is_err());
        assert!(
            chat_thinking_path(&agent, session).exists(),
            "写新文件失败后旧文件必须还在"
        );
        assert!(chat_timer_reset_path(&agent, session).exists());
    }

    /// 四层合并:per-chat 显式值压过 agent 维度配置,`inherit` 则跟随。
    #[test]
    fn resolve_settings_prefers_chat_override_over_messaging() {
        let tmp = tempfile::tempdir().unwrap();
        let agent = tmp_agent(&tmp);
        let session = "soul-test-resolve";
        let mut messaging = summon_core::config::builtin_messaging();
        messaging.timer_idle_retire_reset = false;

        assert!(!resolve_settings(&agent, session, &messaging).timer_idle_retire_reset());
        set_chat_setting(
            &agent,
            session,
            SettingKey::TimerIdleRetireReset,
            Some(true),
        )
        .unwrap();
        assert!(resolve_settings(&agent, session, &messaging).timer_idle_retire_reset());

        // 覆盖存在时,改 agent 维度配置不再影响本会话。
        messaging.timer_idle_retire_reset = false;
        assert!(resolve_settings(&agent, session, &messaging).timer_idle_retire_reset());
    }

    #[test]
    fn sidecar_session_key_prefers_live_handle_name() {
        let tmp = tempfile::tempdir().unwrap();
        let mut agent: Agent = serde_yaml::from_str("name: test\nruntime:\n  cli: claude").unwrap();
        agent.dir = tmp.path().join("agents").join(&agent.name);
        std::fs::create_dir_all(&agent.dir).unwrap();
        std::fs::create_dir_all(agent.state_dir()).unwrap();

        // 有活 handle → 用它的 tmux_name(idle 扫描读的就是这个,ground truth),
        // 不管 .meta 里记了什么。
        let key = sidecar_session_key(Some("soul-live-handle"), &agent, "group", "oc_live");
        assert_eq!(key, "soul-live-handle");

        // 无 handle → 回退 chat_session_name(读 .meta,缺失再 derive)。
        let key = sidecar_session_key(None, &agent, "group", "oc_fallback");
        assert_eq!(
            key,
            chat_session_name(&agent, "group", "oc_fallback"),
            "无 handle 时回退 chat_session_name"
        );
    }
}
