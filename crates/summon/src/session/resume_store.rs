//! per-chat CLI resume 记录的**单一收口**。一个 chat 一条记录,键 = 该 chat 的 tmux 会话名
//! (owner 私聊裸名 `soul-<alias>`、其余 `soul-<alias>-<tag>`),路径
//! `runtime/chats/<session>.session`,与 `.meta` 同目录同键。格式复用
//! `summon_core::session_id::format_record`(`cli` / `id` / 可选 `cwd`)。tmux(Transcript,claude)
//! 与无头(Subprocess,cursor/codex)两路共用。
//!
//! **为什么键选 tmux 会话名而非 chat tag**:resume 记录不止 chat 线程写——CLI `attach`、task run、
//! 临时 worker 都经 `ensure_started` 落记录,而这些上下文**只有会话名、没有 chat tag**;且 owner
//! 私聊裸名记录被 daemon 与 `attach` 两路**共享**,只有会话名是两路都拿得到的稳定键。会话名与
//! `.meta` 同键,`glob runtime/chats/*` 即可配对,熵最低。
//!
//! **迁移(存量无损)**:历史上无头路把 id 单独存 `sessions/<tag>/session_id`(tag 键)。本模块保留它
//! 为**只读 fallback + 惰性迁移**(`read_id_migrating`):读时新路径优先,未命中才读旧路径,命中即
//! 写新路径 + 删旧;**写侧只写新路径**。现网 agent 重启不丢 resume。

use std::fs;
use std::path::{Path, PathBuf};

use summon_core::registry::Agent;
use summon_core::session_id::{parse_record, SessionRecord};

use crate::runtime::fsio::atomic_write;

/// 本 chat 最近结束的原生会话。与自动续聊入口分开，归档后普通消息才能冷启。
pub(crate) fn previous_path(agent: &Agent, session: &str) -> PathBuf {
    record_path(agent, session).with_extension("previous-session")
}

pub(crate) fn remember_chat_checked(agent: &Agent, session: &str, tag: &str) -> anyhow::Result<()> {
    use anyhow::Context as _;
    let record = match read_record_checked(agent, session)? {
        Some(record) => Some(record),
        None => match inspect_legacy(agent, tag) {
            ResumePresence::Missing => None,
            ResumePresence::Valid(record) => Some(record),
            _ => anyhow::bail!(summon_i18n::format!("r6063cd0486")),
        },
    };
    if let Some(record) = record {
        anyhow::ensure!(!record.fork_pending, summon_i18n::format!("r5fcc98c01d"));
        atomic_write(
            &previous_path(agent, session),
            summon_core::session_id::format_full_record(&record).as_bytes(),
        )
        .context(summon_i18n::text!("r6ef816f560"))?;
    }
    Ok(())
}

pub(crate) fn previous_checked(agent: &Agent, session: &str) -> anyhow::Result<SessionRecord> {
    match inspect_path(&previous_path(agent, session)) {
        ResumePresence::Valid(record) => {
            anyhow::ensure!(
                record.cli == agent.resume_cli_kind(),
                summon_i18n::format!("rf33a6c35ac")
            );
            anyhow::ensure!(!record.fork_pending, summon_i18n::format!("re401c9424d"));
            Ok(record)
        }
        ResumePresence::Missing => anyhow::bail!(summon_i18n::format!("r314fe757a0")),
        _ => anyhow::bail!(summon_i18n::format!("r176d68355d")),
    }
}

/// 同一 agent 的恢复提交串行，防止两个归档入口同时接回同一 SID。
static RESTORE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// 写入严格恢复入口，provider 接回失败时保留目标并报错。
pub(crate) fn restore_previous_checked(
    agent: &Agent,
    session: &str,
    record: &SessionRecord,
) -> anyhow::Result<()> {
    let _guard = RESTORE_LOCK
        .lock()
        .map_err(|_| anyhow::anyhow!(summon_i18n::format!("r4ed245bfbe")))?;
    let own = record_path(agent, session);
    if let Some(dir) = own.parent() {
        for entry in fs::read_dir(dir)? {
            let path = entry?.path();
            if path == own || path.extension().and_then(|ext| ext.to_str()) != Some("session") {
                continue;
            }
            match inspect_path(&path) {
                ResumePresence::Valid(other) => anyhow::ensure!(
                    other.cli != record.cli || other.id != record.id,
                    summon_i18n::format!("rec8d01116c")
                ),
                ResumePresence::Missing => {}
                _ => anyhow::bail!(summon_i18n::format!("rcad229522a")),
            }
        }
    }
    let mut record = record.clone();
    record.strict_resume = agent.capture_mode() == summon_core::registry::CaptureMode::Rpc;
    atomic_write(
        &record_path(agent, session),
        summon_core::session_id::format_full_record(&record).as_bytes(),
    )
}

/// fork 提交的暂存文件序号。**每次调用必须拿到唯一的暂存名** —— 见
/// [`write_fork_pending_checked`] 里的理由:共用一个名字会让并发者搬走对方的内容。
static FORK_STAGING_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Bridge 占用探测使用的无副作用视图。Corrupt/Unreadable 都必须 fail-closed，不能像正常 resume
/// 读取那样把它们折叠成 None 后误判目标群空闲。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ResumePresence {
    Missing,
    Valid(SessionRecord),
    Corrupt,
    Unreadable,
}

/// 记录文件路径:`<agent.dir>/runtime/chats/<session>.session`(与 `.meta` 同目录同键)。
pub(crate) fn record_path(agent: &Agent, session: &str) -> PathBuf {
    agent
        .state_dir()
        .join("runtime")
        .join("chats")
        .join(format!("{session}.session"))
}

pub(crate) fn takeover_marker_path(agent: &Agent, session: &str) -> PathBuf {
    record_path(agent, session).with_extension("takeover")
}

pub(crate) fn mark_takeover(
    agent: &Agent,
    session: &str,
    external: &summon_core::bridge::SessionKey,
) -> anyhow::Result<()> {
    let content = format!(
        "config_dir={}\nsession_id={}\n",
        external.config_dir, external.session_id
    );
    atomic_write(&takeover_marker_path(agent, session), content.as_bytes())
}

/// 消息协议正文已进入哪个原生会话:`<session>.protocol`,内容是会话的 `cli` / `id` 加正文原文。
/// 键跟着原生会话走:resume 记录换了会话(冷启、重置、恢复另一条、fork)就自然对不上。
pub(crate) fn protocol_marker_path(agent: &Agent, session: &str) -> PathBuf {
    record_path(agent, session).with_extension("protocol")
}

fn protocol_marker(record: &SessionRecord, protocol: &str) -> String {
    format!("cli={}\nid={}\n\n{protocol}", record.cli, record.id)
}

/// 当前 resume 记录指向的原生会话里是否已有这份协议正文。正文变了(升级改了规则、表情开关
/// 翻转)也算没有。
pub(crate) fn protocol_taught(agent: &Agent, session: &str, protocol: &str) -> bool {
    let Some(record) = read_record(agent, session) else {
        return false;
    };
    fs::read_to_string(protocol_marker_path(agent, session))
        .is_ok_and(|saved| saved == protocol_marker(&record, protocol))
}

/// 记下当前 resume 记录指向的原生会话已收到这份协议正文。没有记录时不写:说不出是哪个会话。
pub(crate) fn mark_protocol_taught(
    agent: &Agent,
    session: &str,
    protocol: &str,
) -> anyhow::Result<()> {
    let Some(record) = read_record(agent, session) else {
        return Ok(());
    };
    atomic_write(
        &protocol_marker_path(agent, session),
        protocol_marker(&record, protocol).as_bytes(),
    )
}

/// 旧无头存储路径:`<base>/sessions/<tag>/session_id`(迁移期只读 fallback,不再写)。
fn legacy_headless_path(base: &Path, tag: &str) -> PathBuf {
    base.join("sessions").join(tag).join("session_id")
}

fn inspect_path(path: &Path) -> ResumePresence {
    match fs::read_to_string(path) {
        Ok(content) => parse_record(&content)
            .map(ResumePresence::Valid)
            .unwrap_or(ResumePresence::Corrupt),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => ResumePresence::Missing,
        Err(_) => ResumePresence::Unreadable,
    }
}

/// 只读检查新 resume 记录；不迁移、不删除损坏文件、不写任何目录。
pub(crate) fn inspect_record(agent: &Agent, session: &str) -> ResumePresence {
    inspect_path(&record_path(agent, session))
}

/// 只读检查旧无头记录；占用判定必须同时覆盖迁移窗口，但不能触发惰性迁移副作用。
pub(crate) fn inspect_legacy(agent: &Agent, tag: &str) -> ResumePresence {
    inspect_path(&legacy_headless_path(&agent.state_dir(), tag))
}

/// 读完整记录(`cwd` 供转录路径定位)。无 / 损坏 → None。
pub(crate) fn read_record(agent: &Agent, session: &str) -> Option<SessionRecord> {
    let s = fs::read_to_string(record_path(agent, session)).ok()?;
    parse_record(&s)
}

/// 安全边界读取：Missing 与坏/不可读严格区分。会决定是否启动新 provider 的路径必须用它，
/// 否则一条损坏的 strict handoff 记录会被折成“没有历史”，随后 cold start 覆盖唯一 owner。
pub(crate) fn read_record_checked(
    agent: &Agent,
    session: &str,
) -> anyhow::Result<Option<SessionRecord>> {
    let path = record_path(agent, session);
    match inspect_path(&path) {
        ResumePresence::Missing => Ok(None),
        ResumePresence::Valid(record) => Ok(Some(record)),
        ResumePresence::Corrupt => {
            anyhow::bail!(summon_i18n::format!("rc7f8ed2dc6", path.display()))
        }
        ResumePresence::Unreadable => {
            anyhow::bail!(summon_i18n::format!("r3a2fd5b344", path.display()))
        }
    }
}

/// 读 resume id;仅当记录 cli 与 `kind` 一致才返回(换了 cli 的旧 id 作废,防把 claude UUID 喂给
/// codex resume 串台)。无记录 / 损坏 / kind 不符 → None。
pub(crate) fn read_id(agent: &Agent, session: &str, kind: &str) -> Option<String> {
    let rec = read_record(agent, session)?;
    (rec.cli == kind).then_some(rec.id)
}

/// 读 resume id,带旧无头存储 fallback + 惰性迁移(无头路专用)。新路径**有记录即权威**(不回落旧
/// 存储,避免 capture 模式切换后旧 id 复活);新路径无记录才读旧 `sessions/<tag>/session_id`,命中就
/// **忠实迁移**(保留原 cli / cwd)到新路径 + 删旧,再按 `kind` guard 决定返回值。迁移 best-effort:
/// 写 / 删失败也照常返回 id(下轮再迁),绝不因迁移失败丢 resume。
pub(crate) fn read_id_migrating(
    agent: &Agent,
    session: &str,
    tag: &str,
    kind: &str,
) -> Option<String> {
    match inspect_record(agent, session) {
        ResumePresence::Valid(record) => return (record.cli == kind).then_some(record.id),
        ResumePresence::Corrupt | ResumePresence::Unreadable => return None,
        ResumePresence::Missing => {}
    }
    let legacy = legacy_headless_path(&agent.state_dir(), tag);
    let rec = parse_record(&fs::read_to_string(&legacy).ok()?)?;
    // 忠实迁移:按原 cli / cwd / created_at 写新路径,删旧文件(含空目录)。
    write(
        agent,
        session,
        &rec.cli,
        &rec.id,
        rec.cwd.as_deref(),
        rec.created_at.as_deref(),
    );
    remove_legacy(&legacy);
    (rec.cli == kind).then_some(rec.id)
}

/// 写 / 覆盖记录。`cwd` 只落**绝对路径**(相对值 `format_record` 内部丢弃,防读取侧 munge 出垃圾
/// 转录路径,C1)。`created_at` 是本会话首次冷启时间;resume 路径不调用本函数(走 `update_cwd`
/// 保留原值),只有冷启才写。失败静默忽略(最坏退化成下次冷启)。
pub(crate) fn write(
    agent: &Agent,
    session: &str,
    kind: &str,
    id: &str,
    cwd: Option<&str>,
    created_at: Option<&str>,
) {
    let _ = write_checked(agent, session, kind, id, cwd, created_at);
}

/// 与 [`write`] 相同,额外落 `primer_slot`(见 `SessionRecord::primer_slot`)。
///
/// 只有 RPC 池的会话落盘那一处需要它。不把参数加进 [`write`] 是因为那边有二十多个调用点,
/// 而槽位只对 codex 有意义 —— 让 claude / cursor 的记录凭空多一个判据,反而更容易读错。
#[allow(clippy::too_many_arguments)] // 与 `write_checked_inner` 同一条:记录本身就是这么多字段
pub(crate) fn write_with_primer_slot(
    agent: &Agent,
    session: &str,
    kind: &str,
    id: &str,
    cwd: Option<&str>,
    created_at: Option<&str>,
    primer_slot: Option<&str>,
    redeeming_fork: bool,
) {
    let _ = write_checked_inner(
        agent,
        session,
        kind,
        id,
        cwd,
        created_at,
        primer_slot,
        redeeming_fork,
    );
}

pub(crate) fn write_checked(
    agent: &Agent,
    session: &str,
    kind: &str,
    id: &str,
    cwd: Option<&str>,
    created_at: Option<&str>,
) -> anyhow::Result<()> {
    write_checked_inner(agent, session, kind, id, cwd, created_at, None, false)
}

#[allow(clippy::too_many_arguments)] // 记录本身就是这么多字段;拆结构体只会把同一份数据换个地方摊开
fn write_checked_inner(
    agent: &Agent,
    session: &str,
    kind: &str,
    id: &str,
    cwd: Option<&str>,
    created_at: Option<&str>,
    primer_slot: Option<&str>,
    // `redeeming_fork`:这次落盘是不是「fork 兑现」——只有真正执行了 fork 的那条路径给 true。
    // 见下面血缘继承那段。
    redeeming_fork: bool,
) -> anyhow::Result<()> {
    let path = record_path(agent, session);
    // fork 血缘要跨越这次覆盖:副本的历史来处是这条会话的**既有事实**,不随「又落了一次盘」消失。
    // 而 `fork_pending` 不继承 —— 它是「下一轮走 fork 启动」的判据,写盘的这一轮就是那一轮,
    // 由 provider 侧成功后经 `clear_fork_pending_checked` 清。继承它会让每轮重造副本。
    let mut inherited_origin = None;
    match inspect_path(&path) {
        ResumePresence::Valid(existing)
            if existing.strict_resume && (existing.cli != kind || existing.id != id) =>
        {
            anyhow::bail!(summon_i18n::format!(
                "rfe8771c620",
                existing.cli,
                existing.id
            ))
        }
        ResumePresence::Corrupt => {
            anyhow::bail!(summon_i18n::format!("r001f702fd9", path.display()))
        }
        ResumePresence::Unreadable => {
            anyhow::bail!(summon_i18n::format!("r42bb91cf8d", path.display()))
        }
        ResumePresence::Valid(existing) => {
            // 血缘跨这次覆盖继承,有两种情形:
            // ① 同一条会话又落了一次盘 —— 来处是既有事实,不随重复落盘消失。
            // ② 调用方**显式声明这次是 fork 兑现**(`redeeming_fork`)。codex 的新 thread id 由
            //    `thread/fork` 返回、与占位 sid 不同,按「同一条会话」判会在这一刻把血缘丢掉,
            //    而这恰恰是最需要记住来处的那次写入。
            //
            // ② 为什么要调用方声明,而不在这里按 `existing.fork_pending` 推断:待 fork 的记录也可能
            // 被一次**冷启**顶掉(fork 起不来、用户改了配置、走到别的分支),推断会把那次冷启也算成
            // 兑现,于是新 sid 声称自己有源群历史,而它里面一个字节都没有。血缘说谎比没有血缘更糟 ——
            // 排障的人会拿它去源群找一段从未被复制过来的对话。只有真正执行了 fork 的那条路径知道
            // 这件事,所以判据由它给。
            if (existing.cli == kind && existing.id == id) || redeeming_fork {
                inherited_origin = existing.forked_from;
            }
        }
        ResumePresence::Missing => {}
    }
    let record = SessionRecord {
        cli: kind.to_string(),
        id: id.to_string(),
        cwd: cwd.filter(|w| w.starts_with('/')).map(str::to_string),
        created_at: created_at
            .filter(|t| !t.trim().is_empty())
            .map(str::to_string),
        strict_resume: false,
        primer_slot: primer_slot
            .filter(|s| !s.trim().is_empty())
            .map(str::to_string),
        forked_from: inherited_origin,
        fork_pending: false,
    };
    atomic_write(
        &path,
        summon_core::session_id::format_full_record(&record).as_bytes(),
    )
}

/// 把 source 记录改成 strict-resume，作为 RPC handoff 的 durable intent。标记直接进入
/// `.session` 内容，随后与记录一起原子 move：崩溃在 move 前，source 仍只会 exact resume；
/// 崩溃在 move 后，target 只会 exact resume。另起 marker 文件无法得到这个单提交点性质。
pub(crate) fn mark_strict_checked(
    agent: &Agent,
    session: &str,
    expected_cli: &str,
    expected_sid: &str,
) -> anyhow::Result<SessionRecord> {
    use anyhow::{ensure, Context as _};

    let path = record_path(agent, session);
    let metadata =
        fs::symlink_metadata(&path).with_context(|| summon_i18n::format!("rfb13b25afa"))?;
    ensure!(
        metadata.file_type().is_file(),
        summon_i18n::format!("r8bf5a78675")
    );
    let content = fs::read_to_string(&path).with_context(|| summon_i18n::format!("rd8cd267ab8"))?;
    let mut record = parse_record(&content).with_context(|| summon_i18n::format!("rb4fd2648e8"))?;
    ensure!(
        record.cli == expected_cli && record.id == expected_sid,
        summon_i18n::format!("r6d0621aa9d")
    );
    record.strict_resume = true;
    let rendered = summon_core::session_id::format_full_record(&record);
    atomic_write(&path, rendered.as_bytes()).context(summon_i18n::text!("rcb445d37cb"))?;
    Ok(record)
}

/// exact pool reuse / provider resume 成功后清 strict 位。identity 漂移时拒绝改写；调用方可以
/// 选择保留 strict（安全但下次仍会复验），不能为“清标记成功”覆盖后来者。
pub(crate) fn clear_strict_checked(
    agent: &Agent,
    session: &str,
    expected_cli: &str,
    expected_sid: &str,
) -> anyhow::Result<()> {
    use anyhow::{ensure, Context as _};

    let path = record_path(agent, session);
    let content = fs::read_to_string(&path).with_context(|| summon_i18n::format!("r54bfcd3f22"))?;
    let record = parse_record(&content).with_context(|| summon_i18n::format!("r19265e9eb6"))?;
    ensure!(
        record.cli == expected_cli && record.id == expected_sid,
        summon_i18n::format!("r00891f08c5")
    );
    if !record.strict_resume {
        return Ok(());
    }
    let mut cleared = record;
    cleared.strict_resume = false;
    let rendered = summon_core::session_id::format_full_record(&cleared);
    atomic_write(&path, rendered.as_bytes()).context(summon_i18n::text!("r075aa10b3c"))
}

/// 跨群 fork 的**唯一提交点**:把目标 chat 的 `.session` 写成「带血缘、待 fork」的记录。
///
/// 目标记录必须严格不存在 —— 覆盖等于把别人正在聊的会话销毁掉,那是不可逆的信息丢失。判据放在
/// 这里而不是只在调用方,是因为源群发邀请到目标群兑现之间有十分钟,期间目标群完全可能开起会话。
///
/// `new_sid` 是目标会话的新 sid,与源 sid 相等即拒绝:共用一个 sid 就是双 writer,撞全仓最强的
/// 那条不变量。
pub(crate) fn write_fork_pending_checked(
    agent: &Agent,
    session: &str,
    kind: &str,
    new_sid: &str,
    cwd: Option<&str>,
    created_at: Option<&str>,
    origin: &summon_core::session_id::ForkOrigin,
) -> anyhow::Result<()> {
    use anyhow::{ensure, Context as _};

    ensure!(
        !new_sid.trim().is_empty(),
        summon_i18n::format!("rf82d4f8511")
    );
    ensure!(
        new_sid != origin.sid,
        summon_i18n::format!("r290238922f", origin.sid)
    );
    let path = record_path(agent, session);
    match inspect_path(&path) {
        ResumePresence::Missing => {}
        ResumePresence::Valid(existing) => anyhow::bail!(summon_i18n::format!(
            "rc9d38724ad",
            existing.cli,
            existing.id
        )),
        ResumePresence::Corrupt => {
            anyhow::bail!(summon_i18n::format!("rd7d9d33a9c", path.display()))
        }
        ResumePresence::Unreadable => {
            anyhow::bail!(summon_i18n::format!("r66fbbeff56", path.display()))
        }
    }
    let content =
        summon_core::session_id::format_fork_pending_record(kind, new_sid, cwd, created_at, origin);
    // **提交必须是 no-replace,不能用 `atomic_write`。** 上面那次 `inspect_path` 只是「此刻还
    // 空着」,而不同 chat 各跑一个线程、真并行:两个源群同时 fork 到同一个目标群时,两条命令
    // 都能通过检查,后写的那条会覆盖前一条 —— 目标群的 `.session` 指向 B 的历史,而 A 那边
    // 刚回执说 fork 成功了。检查与提交之间的窗口只能靠 rename 本身的原子性关掉。
    //
    // 与 `transfer_exact` 共用同一个原语,理由也同一条:不支持该原语的平台直接拒绝,
    // 绝不退化成会覆盖 target 的 `fs::rename`。
    //
    // **暂存文件必须每次调用唯一。** 共用一个固定名字时,并发者会互相覆盖对方的暂存内容:
    // A 写完暂存、B 覆盖它、A 的 rename 成功 —— A 收到 Ok、回执说自己 fork 成功了,而落盘的
    // 是 **B 的** sid 与血缘。目标群于是接上了另一个源群的历史,两边都不知道。
    // no-replace 只保证「至多一个 rename 成功」,保证不了「成功那个搬的是自己的内容」。
    let staging = path.with_extension(format!(
        "session.fork-staging.{}.{}",
        std::process::id(),
        FORK_STAGING_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    atomic_write(&staging, content.as_bytes()).context(summon_i18n::text!("rbe1f705716"))?;
    match rename_no_replace(&staging, &path) {
        Ok(()) => Ok(()),
        Err(error) => {
            // 暂存文件留着只会碍事:它不是任何路径的判据,但会占着盘。
            let _ = fs::remove_file(&staging);
            Err(error).context(summon_i18n::text!("ra19ed67ddb"))
        }
    }
}

/// fork 的第一轮成功落下副本后清掉 `fork_pending`,`forked_from` 血缘保留。
///
/// identity 漂移即拒绝改写(与 `clear_strict_checked` 同一条纪律:不能为「清标记成功」覆盖
/// 后来者)。这一位不清掉,每轮都会拿新 sid 再从父会话 fork 一次,每轮造一份新副本。
pub(crate) fn clear_fork_pending_checked(
    agent: &Agent,
    session: &str,
    expected_cli: &str,
    expected_sid: &str,
) -> anyhow::Result<()> {
    use anyhow::{ensure, Context as _};

    let path = record_path(agent, session);
    let content = fs::read_to_string(&path).with_context(|| summon_i18n::format!("r85162705a2"))?;
    let mut record = parse_record(&content).with_context(|| summon_i18n::format!("r855c696ab2"))?;
    ensure!(
        record.cli == expected_cli && record.id == expected_sid,
        summon_i18n::format!("r4d380df97d")
    );
    if !record.fork_pending {
        return Ok(());
    }
    record.fork_pending = false;
    let rendered = summon_core::session_id::format_full_record(&record);
    atomic_write(&path, rendered.as_bytes()).context(summon_i18n::text!("rbd65b99907"))
}

/// 把一条 `.session` 的所有权从 `source_session` 原子转给 `target_session`。
///
/// 这不是普通 rename：Bridge handoff 的 source/target 都可能被陈旧卡片或并发恢复改变，故提交前
/// 必须逐项证明：source 是本 store 内的普通文件、内容可解析且仍是调用方看到的 channel+sid，
/// target 严格不存在。真正提交使用平台原生的 no-replace rename；不支持该原语的平台直接拒绝，
/// 绝不退化成会覆盖 target 的 `std::fs::rename`。
///
/// 成功后返回被移动的完整记录（含 cwd/created_at），调用方无需从可能继续变化的路径重新读取。
pub(crate) fn transfer_exact(
    agent: &Agent,
    source_session: &str,
    target_session: &str,
    expected_cli: &str,
    expected_sid: &str,
) -> anyhow::Result<SessionRecord> {
    use anyhow::{ensure, Context as _};
    use std::io::Read as _;
    use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _};
    use std::path::Component;

    ensure!(
        !expected_cli.trim().is_empty() && !expected_sid.trim().is_empty(),
        summon_i18n::format!("red63a626eb")
    );
    ensure!(
        source_session != target_session,
        summon_i18n::format!("r0121d792b8")
    );

    let store = agent.state_dir().join("runtime").join("chats");
    let checked_record_path = |session: &str| -> anyhow::Result<PathBuf> {
        let mut components = Path::new(session).components();
        ensure!(
            matches!(components.next(), Some(Component::Normal(_))) && components.next().is_none(),
            summon_i18n::format!("r864a23fe98")
        );
        let path = record_path(agent, session);
        ensure!(
            path.parent() == Some(store.as_path()),
            summon_i18n::format!("rb5dd7d24d5")
        );
        Ok(path)
    };
    let source = checked_record_path(source_session)?;
    let target = checked_record_path(target_session)?;

    // O_NOFOLLOW 把「普通文件」约束放在 open 本身：不能先 stat 再被换成 symlink 后跟出去读。
    let mut options = fs::OpenOptions::new();
    options
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_CLOEXEC);
    let mut file = options
        .open(&source)
        .with_context(|| summon_i18n::format!("rde806c11e7"))?;
    let opened = file
        .metadata()
        .with_context(|| summon_i18n::format!("r1c986607f0"))?;
    ensure!(
        opened.file_type().is_file(),
        summon_i18n::format!("rfa4b32d906")
    );

    let mut content = String::new();
    file.read_to_string(&mut content)
        .with_context(|| summon_i18n::format!("r4f0908c547"))?;
    let record = parse_record(&content).with_context(|| summon_i18n::format!("ra9c8340805"))?;
    ensure!(
        record.cli == expected_cli && record.id == expected_sid,
        summon_i18n::format!("rff1f7ac8c7", record.cli, record.id)
    );
    ensure!(record.strict_resume, summon_i18n::format!("rb1cd41363d"));

    // 再核一次路径仍指向刚才打开的 inode。正常写端是 tmp+rename；若验证期间被换代，这里拒绝
    // 移动陈旧路径。后续真正的 target 不覆盖保证仍由内核原子提供，不能用这类 precheck 代替。
    let current =
        fs::symlink_metadata(&source).with_context(|| summon_i18n::format!("r4f1ef08d11"))?;
    ensure!(
        current.file_type().is_file()
            && current.dev() == opened.dev()
            && current.ino() == opened.ino(),
        summon_i18n::format!("r0a2d6e56ed")
    );
    match fs::symlink_metadata(&target) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Ok(_) => anyhow::bail!(summon_i18n::format!("rab10ae97da")),
        Err(error) => return Err(error).with_context(|| summon_i18n::format!("rb8b50a5697")),
    }

    rename_no_replace(&source, &target)?;
    // 与 atomic_write 一致：rename 已提交后目录 fsync 只增强断电耐久性；不支持目录 fsync 的
    // 文件系统不能让调用方收到一个“失败”后误以为 source 仍拥有记录。
    if let Some(parent) = target.parent() {
        let _ = fs::File::open(parent).and_then(|dir| dir.sync_all());
    }
    Ok(record)
}

#[cfg(target_os = "macos")]
fn rename_no_replace(source: &Path, target: &Path) -> anyhow::Result<()> {
    use anyhow::Context as _;
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt as _;

    let source_c =
        CString::new(source.as_os_str().as_bytes()).context(summon_i18n::text!("rf9fed3a690"))?;
    let target_c =
        CString::new(target.as_os_str().as_bytes()).context(summon_i18n::text!("r57cc359f6e"))?;
    // SAFETY:两个 CString 在调用期间存活且以 NUL 结尾；RENAME_EXCL 由 Darwin 保证目标存在时
    // 原子失败，不会覆盖已有所有者。
    let result = unsafe {
        nix::libc::renamex_np(source_c.as_ptr(), target_c.as_ptr(), nix::libc::RENAME_EXCL)
    };
    if result == 0 {
        return Ok(());
    }
    Err(std::io::Error::last_os_error())
        .with_context(|| format!("renamex_np(RENAME_EXCL) {source:?} → {target:?}"))
}

#[cfg(target_os = "linux")]
fn rename_no_replace(source: &Path, target: &Path) -> anyhow::Result<()> {
    use anyhow::Context as _;
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt as _;

    let source_c =
        CString::new(source.as_os_str().as_bytes()).context(summon_i18n::text!("rf9fed3a690"))?;
    let target_c =
        CString::new(target.as_os_str().as_bytes()).context(summon_i18n::text!("r57cc359f6e"))?;
    // SAFETY:两个 CString 在调用期间存活且以 NUL 结尾；RENAME_NOREPLACE 由 Linux VFS 保证
    // 目标存在时原子失败。内核/文件系统不支持时错误原样上抛，绝不降级覆盖。
    let result = unsafe {
        nix::libc::renameat2(
            nix::libc::AT_FDCWD,
            source_c.as_ptr(),
            nix::libc::AT_FDCWD,
            target_c.as_ptr(),
            nix::libc::RENAME_NOREPLACE,
        )
    };
    if result == 0 {
        return Ok(());
    }
    Err(std::io::Error::last_os_error())
        .with_context(|| format!("renameat2(RENAME_NOREPLACE) {source:?} → {target:?}"))
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn rename_no_replace(_source: &Path, _target: &Path) -> anyhow::Result<()> {
    anyhow::bail!(summon_i18n::format!("ra9c4c891a8"))
}

/// 更新已有记录的 `cwd`(`/cd` 改目录、resume 成功后调用)。记录不存在 / 解析失败 → 静默 no-op
///(不影响主流程)。**保留 `created_at` 不变**:resume 不改会话起始时间。
pub(crate) fn update_cwd(agent: &Agent, session: &str, new_cwd: &str) {
    let Some(mut rec) = read_record(agent, session) else {
        return;
    };
    let path = record_path(agent, session);
    rec.cwd = Some(new_cwd.to_string());
    let content = summon_core::session_id::format_full_record(&rec);
    let _ = atomic_write(&path, content.as_bytes());
}

/// 删除记录。返回**是否真删了文件**(供如实回执:不能对不存在文件谎报「已清除」,C17)。
pub fn clear(agent: &Agent, session: &str) -> bool {
    let record = fs::remove_file(record_path(agent, session)).is_ok();
    let marker = fs::remove_file(takeover_marker_path(agent, session)).is_ok();
    // 协议标记只是记录的附属:记录没了它就对不上任何会话,删不掉也不影响判据,不计入回执。
    let _ = fs::remove_file(protocol_marker_path(agent, session));
    record || marker
}

/// 一封待兑现的 fork 邀请函的路径(键 = **目标** chat 的 tag)。
///
/// 定义在这里而不是 `daemon::fork`,是因为 reset 要清它,而 reset 的统一入口在本模块 ——
/// 判据与清理分居两处,清理侧就会漏掉新加的文件(这类漏清正是 C5/C10 那次的形状)。
/// 一封待兑现的 fork 邀请函的路径,**键 = 短码**。
///
/// 邀请不定向到任何群 —— 那是「fork 到还不存在的群」这个能力的前提:发起那一刻目标群可能还没
/// 建出来,连 tag 都不存在。所以键只能是短码本身,兑现侧凭码直接查到它。
///
/// 定义在这里而不是 `daemon::fork`,是因为 reset 要清它,而 reset 的统一入口在本模块 ——
/// 判据与清理分居两处,清理侧就会漏掉新加的文件(这类漏清正是 C5/C10 那次的形状)。
pub(crate) fn fork_invite_path(agent: &Agent, code: &str) -> PathBuf {
    agent
        .state_dir()
        .join("runtime")
        .join("chats")
        .join(format!("{code}.fork-invite"))
}

/// 清掉**本 chat 作为源**发出去的全部未兑现邀请。reset 用它。
///
/// 邀请函按短码命名(它不定向到任何群),所以这里只能逐个读、按 `source_tag` 认。
///
/// **为什么 reset 要清它**:reset 之后源会话的 sid 变了,那些邀请背书的历史已经不存在;留着
/// 它们只会让兑现方拿到一句「源会话换过了」的报错。更要紧的是别让一段旧码在 TTL 内还能兑 ——
/// 兑现侧虽有 sid 复验兜底,但清掉是更直接的答案。
fn remove_fork_invites_issued_by(agent: &Agent, source_tag: &str) {
    let dir = agent.state_dir().join("runtime").join("chats");
    let Ok(entries) = fs::read_dir(&dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !name.ends_with(".fork-invite") {
            continue;
        }
        let Ok(raw) = fs::read_to_string(entry.path()) else {
            continue;
        };
        // 只认自己发出去的那些。读不出来的不动 —— 那可能是别的 chat 的邀请。
        if raw
            .lines()
            .any(|line| line.trim() == format!("source_tag={source_tag}"))
        {
            let _ = fs::remove_file(entry.path());
        }
    }
}

/// **reset 统一清理入口**:清一个 chat 的全部 resume 记录——新路径 `runtime/chats/<session>.session`
/// 外加迁移期旧无头 `sessions/<tag>/session_id`,无条件双清(删不存在 = no-op,幂等)。兜住迁移窗口
/// 与「中途换过 CLI / capture 模式」的历史残留。返回是否清掉了任意一处。所有 reset 路径
/// (do_teardown reset、面板退出新建、agents kill --reset、offline archive、down --reset)都走此入口
/// (arch-review C5/C10:旧实现漏清无头路,reset 后照样 resume 旧上下文)。
pub fn clear_chat(agent: &Agent, session: &str, tag: &str) -> bool {
    let new_cleared = clear(agent, session);
    let legacy_cleared = remove_legacy(&legacy_headless_path(&agent.state_dir(), tag));
    // 待兑现的 fork 邀请函一并清掉。留着它,本群 reset 之后在 TTL 内再发一次 `/fork` 就能凭
    // **同一封**邀请再兑现一遍,而源群那次表态只针对当时那一次 —— 「一封邀请只能兑现一次」
    // 这条不变量必须在 reset 这条路上也成立,否则 reset 反而成了重放它的办法。
    //
    // 不计入返回值:返回值答的是「有没有清掉 resume 记录」(回执据此说「已清除」),
    // 一封没人兑现的邀请函不该让一个本来没有会话的 chat 报成「清掉了会话」。
    remove_fork_invites_issued_by(agent, tag);
    new_cleared || legacy_cleared
}

/// 枚举旧无头存储里所有存有 resume id 的 chat tag(`down --reset` 无 `--chat` 时清残留用)。
/// `sessions/` 不存在 / 读不动 → 空;只认真正含 `session_id` 文件的 tag 目录(跳过空目录残骸)。
pub(crate) fn legacy_headless_tags(base: &Path) -> Vec<String> {
    let Ok(entries) = fs::read_dir(base.join("sessions")) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|e| {
            let tag_dir = e.path();
            if !tag_dir.join("session_id").is_file() {
                return None;
            }
            tag_dir
                .file_name()
                .and_then(|s| s.to_str())
                .map(String::from)
        })
        .collect()
}

/// `.session` 缺同名 `.meta` 的不可归属记录。它不再全局阻断 Bridge，但必须在 status/doctor
/// 精确暴露，交给 owner 逐条 reset，避免永久积累成看不见的可恢复上下文。
pub(crate) fn orphan_records(agent: &Agent) -> Vec<String> {
    let dir = agent.state_dir().join("runtime").join("chats");
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut records = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            let session = path
                .file_name()
                .and_then(|name| name.to_str())
                .and_then(|name| name.strip_suffix(".session"))?;
            (!path.with_extension("meta").is_file()).then(|| session.to_string())
        })
        .collect::<Vec<_>>();
    records.sort();
    records
}

/// 清旧无头某 tag 的记录(`down --reset` 迁移期残留清理;供 CLI 按 tag 精确清)。返回是否真删了。
pub(crate) fn clear_legacy_headless(base: &Path, tag: &str) -> bool {
    remove_legacy(&legacy_headless_path(base, tag))
}

/// 删旧无头 `sessions/<tag>/session_id`(含随手 rmdir 空目录:该目录通常只放过 session_id)。
/// 返回是否真删了文件;文件本不存在返回 false(幂等,不算失败)。
fn remove_legacy(path: &Path) -> bool {
    let removed = fs::remove_file(path).is_ok();
    if removed {
        if let Some(dir) = path.parent() {
            let _ = fs::remove_dir(dir); // 空则删;非空(未来放了别的)则 remove_dir 失败,忽略即可
        }
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;
    use summon_core::registry::{chat_tag, Agent};

    fn agent_at(dir: &Path) -> Agent {
        // agent.dir 必须是 tempdir 的子目录:state_dir() 从 dir.parent() 推导 agents_dir,
        // 用根会让 state_dir 落到系统临时目录的共享路径,并发测试互相串 resume 记录。
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

    #[test]
    fn protocol_marker_follows_the_native_session_and_the_exact_text() {
        let tmp = tempfile::tempdir().unwrap();
        let agent = agent_at(tmp.path());
        let session = "soul-x-p_ou_1";
        assert!(!protocol_taught(&agent, session, "规则"));
        mark_protocol_taught(&agent, session, "规则").unwrap();
        assert!(
            !protocol_marker_path(&agent, session).exists(),
            "没有 resume 记录时说不出是哪个会话,不写"
        );

        write(&agent, session, "codex", "sid-a", None, None);
        mark_protocol_taught(&agent, session, "规则").unwrap();
        assert!(protocol_taught(&agent, session, "规则"));
        assert!(!protocol_taught(&agent, session, "改过的规则"));
        update_cwd(&agent, session, "/tmp");
        assert!(protocol_taught(&agent, session, "规则"), "改目录不换会话");

        write(&agent, session, "codex", "sid-b", None, None);
        assert!(
            !protocol_taught(&agent, session, "规则"),
            "换了原生会话就要重教"
        );
        write(&agent, session, "codex", "sid-a", None, None);
        assert!(protocol_taught(&agent, session, "规则"));
        write(&agent, session, "cursor-acp", "sid-a", None, None);
        assert!(
            !protocol_taught(&agent, session, "规则"),
            "同 id 不同 cli 不是同一个会话"
        );

        assert!(clear(&agent, session));
        assert!(!protocol_marker_path(&agent, session).exists());
    }

    #[test]
    fn archive_and_quit_keep_the_exact_previous_session_across_a_new_session() {
        for (cli, capture) in [
            ("claude", "transcript"),
            ("codex", "rpc"),
            ("cursor", "rpc"),
            ("cursor", "cli"),
        ] {
            for reset in [false, true] {
                let dir = tempfile::tempdir().unwrap();
                let mut agent = agent_at(dir.path());
                agent.runtime = Some(serde_json::from_value(serde_json::json!({"cli": cli, "capture": if cli == "claude" { None } else { Some(capture) }})).unwrap());
                let session = "soul-x-g123";
                let channel = agent.resume_cli_kind();
                let record = SessionRecord {
                    cli: channel.clone(),
                    id: "sid-previous".into(),
                    cwd: Some(dir.path().to_string_lossy().into_owned()),
                    created_at: Some("2026-10-13 10:00:00".into()),
                    primer_slot: Some("developer".into()),
                    ..Default::default()
                };
                atomic_write(
                    &record_path(&agent, session),
                    summon_core::session_id::format_full_record(&record).as_bytes(),
                )
                .unwrap();
                remember_chat_checked(&agent, session, "g123").unwrap();
                if reset {
                    clear_chat(&agent, session, "g123");
                }
                assert_eq!(read_record(&agent, session).is_none(), reset);
                // 新会话使用另一个 SID，显式恢复仍选择上轮的完整记录。
                write_checked(
                    &agent,
                    session,
                    &channel,
                    "sid-new",
                    Some(dir.path().to_str().unwrap()),
                    None,
                )
                .unwrap();
                let previous = previous_checked(&agent, session).unwrap();
                assert_eq!(previous, record);
                restore_previous_checked(&agent, session, &previous).unwrap();
                let restored = read_record_checked(&agent, session).unwrap().unwrap();
                assert_eq!(restored.id, "sid-previous");
                assert_eq!(restored.cwd, record.cwd);
                assert_eq!(restored.created_at, record.created_at);
                assert_eq!(restored.primer_slot, record.primer_slot);
                assert_eq!(restored.strict_resume, capture == "rpc");
            }
        }
    }

    #[test]
    fn previous_session_is_chat_local_and_missing_teardown_keeps_the_saved_target() {
        let dir = tempfile::tempdir().unwrap();
        let agent = agent_at(dir.path());
        write_checked(&agent, "soul-x-ga", "claude", "sid-a", None, None).unwrap();
        remember_chat_checked(&agent, "soul-x-ga", "ga").unwrap();
        clear_chat(&agent, "soul-x-ga", "ga");
        remember_chat_checked(&agent, "soul-x-ga", "ga").unwrap();
        assert_eq!(previous_checked(&agent, "soul-x-ga").unwrap().id, "sid-a");
        assert!(previous_checked(&agent, "soul-x-gb").is_err());
        fs::write(previous_path(&agent, "soul-x-ga"), "corrupt").unwrap();
        assert!(previous_checked(&agent, "soul-x-ga").is_err());
        assert!(read_record(&agent, "soul-x-ga").is_none());
    }

    #[test]
    fn archive_snapshot_failure_preserves_the_active_resume_record() {
        let dir = tempfile::tempdir().unwrap();
        let agent = agent_at(dir.path());
        write_checked(&agent, "soul-x-ga", "claude", "sid-a", None, None).unwrap();
        fs::create_dir(previous_path(&agent, "soul-x-ga")).unwrap();
        assert!(remember_chat_checked(&agent, "soul-x-ga", "ga").is_err());
        assert_eq!(
            read_id(&agent, "soul-x-ga", "claude").as_deref(),
            Some("sid-a")
        );
    }

    #[test]
    fn explicit_restore_refuses_a_session_bound_to_another_chat() {
        let dir = tempfile::tempdir().unwrap();
        let agent = agent_at(dir.path());
        write_checked(&agent, "soul-x-ga", "claude", "sid-shared", None, None).unwrap();
        remember_chat_checked(&agent, "soul-x-ga", "ga").unwrap();
        let previous = previous_checked(&agent, "soul-x-ga").unwrap();
        clear_chat(&agent, "soul-x-ga", "ga");
        write_checked(&agent, "soul-x-gb", "claude", "sid-shared", None, None).unwrap();
        assert!(restore_previous_checked(&agent, "soul-x-ga", &previous).is_err());
        assert!(read_record(&agent, "soul-x-ga").is_none());
        assert_eq!(
            read_id(&agent, "soul-x-gb", "claude").as_deref(),
            Some("sid-shared")
        );
    }

    #[test]
    fn previous_session_rejects_a_different_provider_channel() {
        let dir = tempfile::tempdir().unwrap();
        let agent = agent_at(dir.path());
        write_checked(&agent, "soul-x-ga", "codex", "sid-a", None, None).unwrap();
        remember_chat_checked(&agent, "soul-x-ga", "ga").unwrap();
        assert!(previous_checked(&agent, "soul-x-ga").is_err());
    }

    /// 直接往旧无头存储写一条记录(模拟存量),用于迁移 fallback 测试。
    fn seed_legacy(agent: &Agent, tag: &str, kind: &str, id: &str) {
        let p = legacy_headless_path(&agent.state_dir(), tag);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(
            p,
            summon_core::session_id::format_record(kind, id, None, None),
        )
        .unwrap();
    }

    #[test]
    fn write_read_roundtrip_and_kind_guard() {
        let tmp = tempfile::tempdir().unwrap();
        let a = agent_at(tmp.path());
        assert!(read_id(&a, "soul-x-g01", "claude").is_none());
        write(&a, "soul-x-g01", "claude", "uuid-1", Some("/abs/wd"), None);
        assert_eq!(
            read_id(&a, "soul-x-g01", "claude").as_deref(),
            Some("uuid-1")
        );
        // 带 cwd 完整读回
        assert_eq!(
            read_record(&a, "soul-x-g01").unwrap().cwd.as_deref(),
            Some("/abs/wd")
        );
        // 换 cli kind → 旧 id 作废
        assert!(read_id(&a, "soul-x-g01", "codex").is_none());
    }

    #[test]
    fn inspect_distinguishes_missing_valid_and_corrupt_without_mutation() {
        let tmp = tempfile::tempdir().unwrap();
        let agent = agent_at(tmp.path());
        let session = "soul-x-g01";
        assert_eq!(inspect_record(&agent, session), ResumePresence::Missing);

        write(&agent, session, "claude", "uuid-1", Some("/work"), None);
        assert!(matches!(
            inspect_record(&agent, session),
            ResumePresence::Valid(SessionRecord { id, .. }) if id == "uuid-1"
        ));

        let path = record_path(&agent, session);
        fs::write(&path, "half-written").unwrap();
        assert_eq!(inspect_record(&agent, session), ResumePresence::Corrupt);
        assert_eq!(fs::read_to_string(&path).unwrap(), "half-written");
    }

    #[test]
    fn inspect_legacy_is_read_only() {
        let tmp = tempfile::tempdir().unwrap();
        let agent = agent_at(tmp.path());
        seed_legacy(&agent, "g-old", "cursor", "legacy-id");
        assert!(matches!(
            inspect_legacy(&agent, "g-old"),
            ResumePresence::Valid(SessionRecord { id, .. }) if id == "legacy-id"
        ));
        assert!(legacy_headless_path(&agent.state_dir(), "g-old").exists());
        assert!(!record_path(&agent, "soul-x-g-old").exists());
    }

    #[test]
    fn transfer_exact_moves_record_without_losing_fields() {
        let tmp = tempfile::tempdir().unwrap();
        let agent = agent_at(tmp.path());
        let source = "soul-x-gsource";
        let target = "soul-x-gtarget";
        write(
            &agent,
            source,
            "cursor-acp",
            "session-1",
            Some("/absolute/worktree"),
            Some("2026-08-11 12:34:56"),
        );
        let marked = mark_strict_checked(&agent, source, "cursor-acp", "session-1").unwrap();
        assert!(marked.strict_resume);
        let expected = read_record(&agent, source).unwrap();

        let moved = transfer_exact(&agent, source, target, "cursor-acp", "session-1").unwrap();

        assert_eq!(moved, expected, "返回值必须是被转移的完整原记录");
        assert_eq!(read_record(&agent, target), Some(expected));
        assert!(
            !record_path(&agent, source).exists(),
            "source 必须失去所有权"
        );
        assert!(
            read_record(&agent, target).unwrap().strict_resume,
            "strict intent 必须与唯一 owner 一起穿过提交点"
        );
    }

    #[test]
    fn mark_strict_rejects_identity_drift_without_rewriting_source() {
        let tmp = tempfile::tempdir().unwrap();
        let agent = agent_at(tmp.path());
        let source = "soul-x-gsource";
        write(&agent, source, "codex", "thread-1", Some("/work"), None);
        let before = fs::read(record_path(&agent, source)).unwrap();

        assert!(mark_strict_checked(&agent, source, "codex", "other").is_err());
        assert_eq!(fs::read(record_path(&agent, source)).unwrap(), before);
        assert!(!read_record(&agent, source).unwrap().strict_resume);
    }

    #[test]
    fn strict_owner_cannot_be_overwritten_by_an_exec_channel_or_new_sid() {
        let tmp = tempfile::tempdir().unwrap();
        let agent = agent_at(tmp.path());
        let session = "soul-x-gsource";
        write(
            &agent,
            session,
            "cursor-acp",
            "session-1",
            Some("/work"),
            None,
        );
        mark_strict_checked(&agent, session, "cursor-acp", "session-1").unwrap();

        assert!(
            write_checked(&agent, session, "cursor", "session-2", Some("/work"), None,).is_err()
        );

        let record = read_record(&agent, session).unwrap();
        assert_eq!(record.cli, "cursor-acp");
        assert_eq!(record.id, "session-1");
        assert!(record.strict_resume);
    }

    #[test]
    fn transfer_exact_rejects_a_source_without_strict_intent() {
        let tmp = tempfile::tempdir().unwrap();
        let agent = agent_at(tmp.path());
        let (source, target) = ("soul-x-gsource", "soul-x-gtarget");
        write(&agent, source, "codex", "thread-1", Some("/work"), None);

        assert!(transfer_exact(&agent, source, target, "codex", "thread-1").is_err());

        assert_eq!(
            read_id(&agent, source, "codex").as_deref(),
            Some("thread-1")
        );
        assert!(!record_path(&agent, target).exists());
    }

    #[test]
    fn transfer_exact_never_overwrites_an_existing_target() {
        let tmp = tempfile::tempdir().unwrap();
        let agent = agent_at(tmp.path());
        let (source, target) = ("soul-x-gsource", "soul-x-gtarget");
        write(&agent, source, "codex", "source-id", Some("/source"), None);
        write(&agent, target, "codex", "target-id", Some("/target"), None);
        let source_before = fs::read(record_path(&agent, source)).unwrap();
        let target_before = fs::read(record_path(&agent, target)).unwrap();

        assert!(transfer_exact(&agent, source, target, "codex", "source-id").is_err());

        assert_eq!(
            fs::read(record_path(&agent, source)).unwrap(),
            source_before
        );
        assert_eq!(
            fs::read(record_path(&agent, target)).unwrap(),
            target_before
        );
    }

    #[test]
    fn transfer_exact_rejects_a_corrupt_source_without_creating_target() {
        let tmp = tempfile::tempdir().unwrap();
        let agent = agent_at(tmp.path());
        let (source, target) = ("soul-x-gsource", "soul-x-gtarget");
        let source_path = record_path(&agent, source);
        fs::create_dir_all(source_path.parent().unwrap()).unwrap();
        fs::write(&source_path, "half-written").unwrap();

        assert!(transfer_exact(&agent, source, target, "codex", "sid").is_err());

        assert_eq!(fs::read_to_string(source_path).unwrap(), "half-written");
        assert!(!record_path(&agent, target).exists());
    }

    #[test]
    fn transfer_exact_rejects_channel_or_sid_drift() {
        let tmp = tempfile::tempdir().unwrap();
        let agent = agent_at(tmp.path());
        let (source, target) = ("soul-x-gsource", "soul-x-gtarget");
        write(&agent, source, "cursor-acp", "session-1", None, None);

        assert!(transfer_exact(&agent, source, target, "cursor", "session-1").is_err());
        assert!(transfer_exact(&agent, source, target, "cursor-acp", "session-2").is_err());

        assert_eq!(
            read_id(&agent, source, "cursor-acp").as_deref(),
            Some("session-1")
        );
        assert!(!record_path(&agent, target).exists());
    }

    #[test]
    fn transfer_exact_rejects_a_non_regular_source() {
        let tmp = tempfile::tempdir().unwrap();
        let agent = agent_at(tmp.path());
        let (source, target) = ("soul-x-gsource", "soul-x-gtarget");
        fs::create_dir_all(record_path(&agent, source)).unwrap();

        assert!(transfer_exact(&agent, source, target, "codex", "sid").is_err());
        assert!(record_path(&agent, source).is_dir());
        assert!(!record_path(&agent, target).exists());
    }

    #[test]
    fn transfer_exact_rejects_paths_outside_the_session_store() {
        let tmp = tempfile::tempdir().unwrap();
        let agent = agent_at(tmp.path());
        let source = "../source";
        let target = "../target";
        write(&agent, source, "codex", "sid", None, None);
        let source_path = record_path(&agent, source);

        assert!(transfer_exact(&agent, source, target, "codex", "sid").is_err());
        assert!(source_path.exists(), "越界 source 不得被移动");
        assert!(!record_path(&agent, target).exists());
    }

    #[test]
    fn dual_read_prefers_new_path_over_legacy() {
        // 双读 fallback:新旧两处都有时,新路径权威(不被旧存储污染)。
        let tmp = tempfile::tempdir().unwrap();
        let a = agent_at(tmp.path());
        let tag = "gdeadbeef";
        let session = "soul-x-gdeadbeef";
        seed_legacy(&a, tag, "cursor", "legacy-id");
        write(&a, session, "cursor", "new-id", None, None);
        assert_eq!(
            read_id_migrating(&a, session, tag, "cursor").as_deref(),
            Some("new-id"),
            "新路径有记录时必须优先于旧无头存储"
        );
        // 旧存储未被迁移动作触碰(新路径已权威,不回落)。
        assert!(legacy_headless_path(&a.state_dir(), tag).is_file());
    }

    #[test]
    fn lazy_migration_moves_legacy_into_new_path_then_deletes_old() {
        // 惰性迁移:新路径空、旧无头有 → 读时迁移到新路径 + 删旧,resume 无损。
        let tmp = tempfile::tempdir().unwrap();
        let a = agent_at(tmp.path());
        let tag = "gfeed";
        let session = "soul-x-gfeed";
        seed_legacy(&a, tag, "cursor", "chat-77");
        // 首次读:走 fallback,拿到 id
        assert_eq!(
            read_id_migrating(&a, session, tag, "cursor").as_deref(),
            Some("chat-77")
        );
        // 迁移副作用:新路径已落记录、旧文件已删
        assert_eq!(read_id(&a, session, "cursor").as_deref(), Some("chat-77"));
        assert!(
            !legacy_headless_path(&a.state_dir(), tag).exists(),
            "迁移后旧无头文件应被删除"
        );
        // 再次读:直接命中新路径(不再依赖旧存储)
        assert_eq!(
            read_id_migrating(&a, session, tag, "cursor").as_deref(),
            Some("chat-77")
        );
    }

    #[test]
    fn lazy_migration_is_faithful_to_original_kind() {
        // 忠实迁移:旧记录 cli=cursor,用 codex 读 → 迁移保留 cursor 记录,但返回 None(kind guard)。
        let tmp = tempfile::tempdir().unwrap();
        let a = agent_at(tmp.path());
        let (tag, session) = ("gk", "soul-x-gk");
        seed_legacy(&a, tag, "cursor", "cur-id");
        assert!(read_id_migrating(&a, session, tag, "codex").is_none());
        // 迁移仍发生,且保留原 cli(切回 cursor 读得到)
        assert_eq!(read_id(&a, session, "cursor").as_deref(), Some("cur-id"));
        assert!(!legacy_headless_path(&a.state_dir(), tag).exists());
    }

    #[test]
    fn clear_chat_wipes_both_new_and_legacy() {
        // 清理双清:reset 统一入口必须同清新路径 + 旧无头,漏一处 = C5/C10 bug 复发。
        let tmp = tempfile::tempdir().unwrap();
        let a = agent_at(tmp.path());
        let (tag, session) = ("gboth", "soul-x-gboth");
        write(&a, session, "claude", "uuid-new", None, None);
        seed_legacy(&a, tag, "cursor", "id-legacy");
        assert!(clear_chat(&a, session, tag), "清掉任一处应返回 true");
        assert!(read_record(&a, session).is_none(), "新路径未清");
        assert!(
            !legacy_headless_path(&a.state_dir(), tag).exists(),
            "旧无头路未清(C5/C10 核心 bug)"
        );
        // 幂等:全清后再调 → false
        assert!(!clear_chat(&a, session, tag));
    }

    #[test]
    fn owner_bare_name_tag_mapping_round_trips() {
        // 裸名映射 round-trip:owner 私聊会话名是裸名 `soul-x`(不含 tag),tag 是 `p<16hex>`。
        // 存储按会话名键,reset 按 (会话名, tag) 双清——两者由 registry 基础设施一致派生,不能错位。
        let tmp = tempfile::tempdir().unwrap();
        let a = agent_at(tmp.path());
        let (chat_type, chat_id) = ("p2p", "oc_owner_priv");
        let tag = chat_tag(chat_type, chat_id); // p<16hex>
        let session = a.tmux_name_for_chat(chat_type, chat_id, true); // 裸名 soul-x
        assert_eq!(session, "soul-x", "owner 私聊应为裸名");
        assert!(
            tag.starts_with('p') && tag.len() == 17,
            "tag = p+16hex: {tag}"
        );
        // claude Transcript owner 私聊:按裸名键写、读(含 cwd)
        write(&a, &session, "claude", "uuid-owner", Some("/w"), None);
        assert_eq!(
            read_id(&a, &session, "claude").as_deref(),
            Some("uuid-owner")
        );
        // 存量无头 id 落在 tag 路径(裸名 session 无法反推 tag,故迁移必须显式带 tag)
        let (tag2, session2) = (
            chat_tag("p2p", "oc_h"),
            a.tmux_name_for_chat("p2p", "oc_h", false),
        );
        seed_legacy(&a, &tag2, "cursor", "h-id");
        assert_eq!(
            read_id_migrating(&a, &session2, &tag2, "cursor").as_deref(),
            Some("h-id")
        );
        // reset 裸名会话:双清按 (裸名, tag) 生效
        assert!(clear_chat(&a, &session, &tag));
        assert!(read_record(&a, &session).is_none());
    }

    #[test]
    fn legacy_tags_enumeration_lists_only_records() {
        let tmp = tempfile::tempdir().unwrap();
        let a = agent_at(tmp.path());
        let base = a.state_dir();
        assert!(legacy_headless_tags(&base).is_empty());
        seed_legacy(&a, "gaaa", "claude", "s1");
        seed_legacy(&a, "pbbb", "cursor", "s2");
        let mut tags = legacy_headless_tags(&base);
        tags.sort();
        assert_eq!(tags, vec!["gaaa".to_string(), "pbbb".to_string()]);
        assert!(clear_legacy_headless(&base, "gaaa"));
        assert!(!clear_legacy_headless(&base, "gaaa")); // 幂等
    }

    fn origin() -> summon_core::session_id::ForkOrigin {
        summon_core::session_id::ForkOrigin::new("gsrc", "src-sid", "2026-09-03 11:52:30").unwrap()
    }

    #[test]
    fn a_fork_commit_lands_the_new_sid_with_its_lineage() {
        let tmp = tempfile::tempdir().unwrap();
        let a = agent_at(tmp.path());
        write_fork_pending_checked(
            &a,
            "soul-x-gdst",
            "claude",
            "new-sid",
            Some("/repo"),
            None,
            &origin(),
        )
        .expect("空目标该写得进去");
        let rec = read_record(&a, "soul-x-gdst").expect("记录该在");
        assert_eq!(rec.id, "new-sid");
        assert!(rec.fork_pending);
        assert_eq!(rec.forked_from, Some(origin()));
    }

    #[test]
    fn a_fork_commit_refuses_to_overwrite_an_existing_session() {
        // 覆盖等于把目标群正在聊的上下文销毁掉,不可逆。判据必须在**落盘这一刻**再验一次:
        // 源群发邀请到目标群兑现之间有十分钟,那段时间里目标群完全可能自己开起会话。
        let tmp = tempfile::tempdir().unwrap();
        let a = agent_at(tmp.path());
        write(&a, "soul-x-gdst", "claude", "busy-sid", Some("/repo"), None);
        let err = write_fork_pending_checked(
            &a,
            "soul-x-gdst",
            "claude",
            "new-sid",
            Some("/repo"),
            None,
            &origin(),
        )
        .expect_err("占用的目标必须拒");
        assert!(err.to_string().contains("busy-sid"), "{err}");
        // 拒绝之后目标记录必须**原样不动**。
        assert_eq!(read_record(&a, "soul-x-gdst").unwrap().id, "busy-sid");
    }

    #[test]
    fn a_fork_commit_refuses_to_reuse_the_source_sid() {
        // 两个 chat 共用一个 sid 就是双 writer,撞全仓最强的那条不变量。
        let tmp = tempfile::tempdir().unwrap();
        let a = agent_at(tmp.path());
        let err = write_fork_pending_checked(
            &a,
            "soul-x-gdst",
            "claude",
            "src-sid",
            Some("/repo"),
            None,
            &origin(),
        )
        .expect_err("复用源 sid 必须拒");
        assert!(err.to_string().contains("双 writer"), "{err}");
        assert!(
            read_record(&a, "soul-x-gdst").is_none(),
            "拒绝时不该留下记录"
        );
    }

    #[test]
    fn clearing_fork_pending_keeps_the_lineage() {
        let tmp = tempfile::tempdir().unwrap();
        let a = agent_at(tmp.path());
        write_fork_pending_checked(
            &a,
            "soul-x-gdst",
            "claude",
            "new-sid",
            Some("/repo"),
            None,
            &origin(),
        )
        .unwrap();
        clear_fork_pending_checked(&a, "soul-x-gdst", "claude", "new-sid").unwrap();
        let rec = read_record(&a, "soul-x-gdst").unwrap();
        assert!(!rec.fork_pending, "副本已生成,判据必须清掉");
        assert_eq!(
            rec.forked_from,
            Some(origin()),
            "血缘是既有事实,不随清判据消失"
        );
        assert_eq!(
            rec.cwd.as_deref(),
            Some("/repo"),
            "清判据不该顺带丢别的字段"
        );
    }

    #[test]
    fn clearing_fork_pending_refuses_when_the_identity_drifted() {
        // 与 clear_strict_checked 同一条纪律:不能为「清标记成功」覆盖后来者。
        let tmp = tempfile::tempdir().unwrap();
        let a = agent_at(tmp.path());
        write_fork_pending_checked(
            &a,
            "soul-x-gdst",
            "claude",
            "new-sid",
            Some("/repo"),
            None,
            &origin(),
        )
        .unwrap();
        assert!(clear_fork_pending_checked(&a, "soul-x-gdst", "claude", "other-sid").is_err());
        assert!(
            read_record(&a, "soul-x-gdst").unwrap().fork_pending,
            "identity 不符时不许改写记录"
        );
    }

    /// 一次**冷启**顶掉待 fork 的记录时,血缘必须跟着丢。
    ///
    /// 冷启出来的 sid 里一个字节源群历史都没有。让它继承血缘 = 血缘在说谎,而说谎的血缘比没有
    /// 血缘更糟:排障的人会拿它去源群找一段从未被复制过来的对话。所以「这次是不是 fork 兑现」
    /// 由执行方显式声明,不在写盘处按 `fork_pending` 推断 —— 推断分不出这两种情形。
    #[test]
    fn a_cold_start_over_a_pending_fork_drops_the_lineage() {
        let tmp = tempfile::tempdir().unwrap();
        let a = agent_at(tmp.path());
        write_fork_pending_checked(
            &a,
            "soul-x-gdst",
            "claude",
            "placeholder",
            Some("/repo"),
            None,
            &origin(),
        )
        .unwrap();
        // 同一个 cli 的普通冷启(不声明兑现)。
        write(
            &a,
            "soul-x-gdst",
            "claude",
            "cold-fresh-sid",
            Some("/repo"),
            None,
        );
        let rec = read_record(&a, "soul-x-gdst").unwrap();
        assert_eq!(rec.id, "cold-fresh-sid");
        assert!(!rec.fork_pending);
        assert_eq!(
            rec.forked_from, None,
            "冷启出来的 sid 没有源群历史,不该挂着血缘声称自己有"
        );
    }

    #[test]
    fn a_pending_fork_keeps_its_lineage_when_the_provider_returns_a_different_sid() {
        // codex 的新 thread id 由 `thread/fork` 返回,与占位 sid 不同。这一次落盘就是 fork 的
        // 兑现 —— 按「同一条会话才继承」判会在这一刻把血缘丢掉,而这恰恰是最该记住来处的写入。
        let tmp = tempfile::tempdir().unwrap();
        let a = agent_at(tmp.path());
        write_fork_pending_checked(
            &a,
            "soul-x-gdst",
            "codex",
            "placeholder",
            Some("/repo"),
            None,
            &origin(),
        )
        .unwrap();
        write_with_primer_slot(
            &a,
            "soul-x-gdst",
            "codex",
            "th-real",
            Some("/repo"),
            None,
            Some(summon_core::session_id::PRIMER_SLOT_DEVELOPER),
            // 这次是 fork 兑现。
            true,
        );
        let rec = read_record(&a, "soul-x-gdst").unwrap();
        assert_eq!(rec.id, "th-real");
        assert_eq!(rec.forked_from, Some(origin()), "血缘要跨 sid 替换存活");
        assert!(!rec.fork_pending, "落盘即兑现,判据不该被继承下去");
    }

    #[test]
    fn changing_the_work_dir_does_not_drop_the_fork_verdict() {
        // 目标群在 fork 落盘后、第一轮之前被 `/cd` 一下,判据就没了 —— fork 静默降级成冷启,
        // 目标群拿到空会话却以为带着上下文。这是加 `format_full_record` 的直接原因。
        let tmp = tempfile::tempdir().unwrap();
        let a = agent_at(tmp.path());
        write_fork_pending_checked(
            &a,
            "soul-x-gdst",
            "claude",
            "new-sid",
            Some("/repo"),
            None,
            &origin(),
        )
        .unwrap();
        update_cwd(&a, "soul-x-gdst", "/elsewhere");
        let rec = read_record(&a, "soul-x-gdst").unwrap();
        assert_eq!(rec.cwd.as_deref(), Some("/elsewhere"));
        assert!(rec.fork_pending, "改 cwd 不该清掉 fork 判据");
        assert_eq!(rec.forked_from, Some(origin()));
    }

    /// **真并发**:N 个线程同时提交 fork 到同一个目标,恰好一个成功。
    ///
    /// **必须真并发。** 串行调用测不到提交的原子性:第二次在函数开头的 `inspect_path` 就被挡住,
    /// 根本走不到 rename 那一步 —— 那样测的是那道已存在的检查,而不是提交本身。
    ///
    /// 让线程在同一个屏障上起跑,尽量把 check 与 write 之间的窗口撞开。撞不上时这条退化成
    /// 「至少不会两个都成功」,所以配套的 `fork_commit_uses_a_no_replace_rename` 从源码侧
    /// 兜住「别把提交换回覆盖式写」。
    #[test]
    fn only_one_of_many_racing_forks_can_win_the_same_target() {
        use std::sync::{Arc, Barrier};

        let tmp = tempfile::tempdir().unwrap();
        let a = Arc::new(agent_at(tmp.path()));
        const RACERS: usize = 8;
        let barrier = Arc::new(Barrier::new(RACERS));
        let mut handles = Vec::new();
        for i in 0..RACERS {
            let agent = Arc::clone(&a);
            let barrier = Arc::clone(&barrier);
            handles.push(std::thread::spawn(move || {
                let origin = summon_core::session_id::ForkOrigin::new(
                    &format!("gsrc{i}"),
                    &format!("src-{i}"),
                    "2026-09-03 11:00:00",
                )
                .unwrap();
                barrier.wait();
                write_fork_pending_checked(
                    &agent,
                    "soul-x-gdst",
                    "claude",
                    &format!("new-{i}"),
                    Some("/repo"),
                    None,
                    &origin,
                )
                .map(|()| i)
            }));
        }
        let winners: Vec<usize> = handles
            .into_iter()
            .filter_map(|h| h.join().expect("线程不该 panic").ok())
            .collect();
        assert_eq!(
            winners.len(),
            1,
            "恰好一个该赢 —— 多于一个说明后来者覆盖了先到者(双 writer),实得 {winners:?}"
        );
        // 落盘的必须是那个赢家,而不是任何被拒绝者的内容。
        let rec = read_record(&a, "soul-x-gdst").expect("记录该在");
        assert_eq!(rec.id, format!("new-{}", winners[0]));
    }

    /// fork 提交**必须用 no-replace rename**,不能退回 `atomic_write`。
    ///
    /// 上面那条并发测试撞不上窗口时会退化成恒真,所以这道闸从源码侧钉:`atomic_write` 是覆盖式
    /// rename,把提交换回它,「检查与提交之间的窗口」就重新打开 —— 撞的是「一个 provider session
    /// 至多一个 writer」,全仓最强的那条不变量。改动本身不会让任何行为测试变红,所以守在这里。
    #[test]
    fn fork_commit_uses_a_no_replace_rename() {
        let code = include_str!("resume_store.rs");
        let start = code
            .find("pub(crate) fn write_fork_pending_checked(")
            .expect("函数必须在");
        let body = &code[start..];
        let end = body
            .find("\n/// fork 的第一轮成功落下副本后")
            .expect("它后面应当紧跟 clear_fork_pending_checked 的 doc");
        let body = &body[..end];
        assert!(
            body.contains("rename_no_replace(&staging, &path)"),
            "提交必须走 no-replace rename"
        );
        assert!(
            !body.contains("atomic_write(&path,"),
            "绝不能直接 atomic_write 到目标路径 —— 那是覆盖式提交,竞态窗口又开了"
        );
    }

    #[test]
    fn a_second_concurrent_fork_cannot_overwrite_the_first_commit() {
        // 不同 chat 各跑一个线程、**真并行**。两个源群同时 fork 到同一个目标群时,两条命令都能
        // 通过「此刻还空着」这个检查 —— 提交必须靠 rename 的原子性挡住后来者,否则目标群的
        // `.session` 指向 B 的历史,而 A 那边刚回执说 fork 成功了。
        let tmp = tempfile::tempdir().unwrap();
        let a = agent_at(tmp.path());
        let first =
            summon_core::session_id::ForkOrigin::new("gsrc-a", "sid-a", "2026-09-03 11:00:00")
                .unwrap();
        let second =
            summon_core::session_id::ForkOrigin::new("gsrc-b", "sid-b", "2026-09-03 11:00:01")
                .unwrap();
        write_fork_pending_checked(
            &a,
            "soul-x-gdst",
            "claude",
            "new-a",
            Some("/repo"),
            None,
            &first,
        )
        .expect("第一次该成功");
        assert!(
            write_fork_pending_checked(
                &a,
                "soul-x-gdst",
                "claude",
                "new-b",
                Some("/repo"),
                None,
                &second,
            )
            .is_err(),
            "第二次必须失败 —— 目标已经被占住了"
        );
        let rec = read_record(&a, "soul-x-gdst").unwrap();
        assert_eq!(rec.id, "new-a", "先提交的那条必须存活");
        assert_eq!(rec.forked_from, Some(first));
    }

    #[test]
    fn a_failed_fork_commit_leaves_no_staging_file_behind() {
        // 暂存文件不是任何路径的判据,但留着会让下次同名写入撞上。
        let tmp = tempfile::tempdir().unwrap();
        let a = agent_at(tmp.path());
        write(&a, "soul-x-gdst", "claude", "busy", Some("/repo"), None);
        let _ = write_fork_pending_checked(
            &a,
            "soul-x-gdst",
            "claude",
            "new-sid",
            Some("/repo"),
            None,
            &origin(),
        );
        let dir = a.state_dir().join("runtime").join("chats");
        let leftovers: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains("fork-staging"))
            .collect();
        assert!(leftovers.is_empty(), "残留了暂存文件:{leftovers:?}");
    }

    #[test]
    fn a_reset_clears_the_invites_this_chat_issued() {
        // reset 之后本会话的 sid 变了,那些邀请背书的历史已经不存在。留着它们只会让兑现方
        // 在别的群拿到一句「源会话换过了」的报错;更要紧的是别让一段旧码在 TTL 内还能兑。
        let tmp = tempfile::tempdir().unwrap();
        let a = agent_at(tmp.path());
        let mine = fork_invite_path(&a, "7f3a");
        let other = fork_invite_path(&a, "b2c1");
        fs::create_dir_all(mine.parent().unwrap()).unwrap();
        fs::write(
            &mine,
            "source_tag=gsrc\nsource_sid=s\nissued_at=1\nissuer_open_id=ou_a\n",
        )
        .unwrap();
        // 另一个 chat 发出的邀请不该被顺手清掉。
        fs::write(
            &other,
            "source_tag=gelse\nsource_sid=s\nissued_at=1\nissuer_open_id=ou_a\n",
        )
        .unwrap();
        clear_chat(&a, "soul-x-gsrc", "gsrc");
        assert!(!mine.exists(), "本会话发出的邀请必须清掉");
        assert!(other.exists(), "别的会话发出的邀请不该被清");
    }

    #[test]
    fn clearing_a_chat_without_a_session_does_not_claim_it_cleared_one() {
        // 返回值答的是「有没有清掉 resume 记录」(回执据此说「已清除」)。一封没人兑现的邀请函
        // 不该让一个本来没有会话的 chat 报成「清掉了会话」。
        let tmp = tempfile::tempdir().unwrap();
        let a = agent_at(tmp.path());
        let invite = fork_invite_path(&a, "7f3a");
        fs::create_dir_all(invite.parent().unwrap()).unwrap();
        fs::write(
            &invite,
            "source_tag=gdst\nsource_sid=s\nissued_at=1\nissuer_open_id=ou_a\n",
        )
        .unwrap();
        assert!(!clear_chat(&a, "soul-x-gdst", "gdst"));
    }

    #[test]
    fn a_reset_clears_a_pending_fork_along_with_the_record() {
        // fork 的撤销就是 reset(设计上没有独立的「取消 fork」入口)。它必须真的清干净,
        // 否则下一轮还会按 fork 起,把用户已经放弃的那份副本又造出来。
        let tmp = tempfile::tempdir().unwrap();
        let a = agent_at(tmp.path());
        write_fork_pending_checked(
            &a,
            "soul-x-gdst",
            "claude",
            "new-sid",
            Some("/repo"),
            None,
            &origin(),
        )
        .unwrap();
        assert!(clear_chat(&a, "soul-x-gdst", "gdst"));
        assert!(read_record(&a, "soul-x-gdst").is_none());
    }
}
