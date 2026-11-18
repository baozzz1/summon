//! 逐轮台账的写入与读取(格式见 `summon_core::turn_ledger`)。
//!
//! 一轮的事实散在几处:触发消息、开始时间与终态在 chat 线程手里;工具步数、实际用的模型、额度与
//! feed 总结在捕获层的收尾汇合点上;token 随每次模型请求陆续到来(codex 推送的、cursor 按轮报的、
//! claude 转录里读到的,见 `runtime::claude_usage`)。各方按 (agent, 会话) 往旁路表里登记([`note`]),
//! chat 线程在终态确定时取走、拼成一行写盘([`finish`])。一个会话同一时刻只跑一轮,这张表按会话
//! 记不会串轮;每轮起跑时清掉上一轮没取走的(见 [`begin`])。
//!
//! 不在任何一轮里的 claude 请求写成轮外用量([`append_outside`]),与台账同样按月存、同样读。
//!
//! 台账是旁路:登记与写盘失败都只记日志,不影响这一轮的收尾、回复与销账。

use std::collections::HashMap;
use std::io::{Read as _, Seek as _, SeekFrom, Write as _};
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex, PoisonError};

use summon_core::agent_msg::{RequestUsage, Usage};
use summon_core::feishu::TurnBadge;
use summon_core::turn_ledger::{
    self, ModelUsage, OutsideUsage, TurnCtx, TurnEnd, TurnOrigin, TurnQuota, TurnRecord, UsagePiece,
};

use crate::capture::TurnOutcome;
use crate::observe::log::log;

/// 捕获层登记的这一轮的事实。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct TurnFacts {
    pub(crate) turn: Option<String>,
    pub(crate) tool_steps: u32,
    pub(crate) cli: Option<String>,
    pub(crate) model: Option<String>,
    pub(crate) effort: Option<String>,
    pub(crate) ctx: Option<TurnCtx>,
    pub(crate) quota: Option<TurnQuota>,
    /// 这一轮从什么时刻算起([`begin`] 时记下):claude 的请求按时刻归属,落在它之后的算这一轮。
    pub(crate) started_at_ms: Option<i64>,
    /// 这一轮见到的模型请求(codex 推送的,含子线程),按请求去重。
    pub(crate) requests: std::collections::BTreeMap<String, RequestUsage>,
    /// claude 转录里归到这一轮的用量(主会话与子 agent 的),带各自的时刻。
    pub(crate) claude: Vec<(i64, UsagePiece)>,
    /// CLI 按轮上报的一份合计(cursor `-p` 的 `result.usage`),没有逐次请求时才用。
    pub(crate) reported: Option<Usage>,
    pub(crate) error: Option<String>,
    pub(crate) feed: Option<String>,
}

impl TurnFacts {
    /// 轮末角标:CLI、模型、档位、上下文与额度,与卡片上显示的是同一份。
    pub(crate) fn absorb_badge(&mut self, badge: &TurnBadge) {
        self.cli = (!badge.cli.is_empty()).then(|| badge.cli.clone());
        self.model = badge.model.clone();
        self.effort = badge.effort.clone();
        let ctx = TurnCtx {
            used: badge.ctx.used_tokens,
            window: badge.ctx.window_tokens,
            pct: badge.ctx.reported_pct,
        };
        self.ctx = (ctx != TurnCtx::default()).then_some(ctx);
        let quota = TurnQuota {
            five_hour_left: badge.quota_5h_left_pct,
            five_hour_resets_at: badge.quota_5h_resets_at,
            long_left: badge.quota_left_pct,
            long_resets_at: badge.quota_resets_at,
        };
        self.quota = (quota != TurnQuota::default()).then_some(quota);
    }

    /// 捕获层交回来的结果:失败类别与回复末尾的 feed 总结。
    pub(crate) fn absorb_outcome(
        &mut self,
        out: &anyhow::Result<TurnOutcome>,
        last_reply: Option<&str>,
    ) {
        match out {
            Ok(outcome) => {
                self.error = if let Some(kind) = outcome.api_error_kind {
                    Some(format!("{kind:?}"))
                } else if outcome.api_error.is_some() {
                    Some("api_error".into())
                } else if outcome.timed_out {
                    Some("timed_out".into())
                } else {
                    None
                };
                let reply = outcome.reply.as_deref().or(last_reply).unwrap_or("");
                self.feed = summon_core::feed_tag::split_feed_tag(reply).summary;
            }
            Err(_) => self.error = Some("capture".into()),
        }
    }

    /// 主轮终态带的那份合计。
    pub(crate) fn absorb_usage(&mut self, usage: &Usage) {
        self.reported = Some(usage.clone());
    }

    /// 一次模型请求;同一次请求再见到时合并(见 [`RequestUsage::merge`])。
    pub(crate) fn absorb_request(&mut self, usage: &RequestUsage) {
        match self.requests.get_mut(&usage.id) {
            Some(seen) => seen.merge(usage),
            None => {
                self.requests.insert(usage.id.clone(), usage.clone());
            }
        }
    }

    /// 写进运行记录的 token:有逐次请求就按请求合并;只有按轮合计时记成一组(按轮的合计分不出
    /// 单次请求的输入量,不分档)。按轮合计来自 cursor,它的输入与 claude 同口径、不含缓存。
    fn usage(&self) -> Vec<ModelUsage> {
        let fallback_model = self.model.as_deref();
        let pieces: Vec<UsagePiece> = self
            .requests
            .values()
            .map(UsagePiece::from)
            .chain(self.claude.iter().map(|(_, p)| p.clone()))
            .collect();
        if !pieces.is_empty() {
            return turn_ledger::merge_pieces(&pieces, fallback_model);
        }
        let Some(r) = &self.reported else {
            return Vec::new();
        };
        let group = ModelUsage {
            model: fallback_model.unwrap_or_default().to_string(),
            requests: 1,
            input: r.input_tokens,
            output: r.output_tokens,
            cache_read: r.cache_read,
            cache_write_5m: r.cache_write,
            ..Default::default()
        };
        if group.tokens() == 0 {
            return Vec::new();
        }
        vec![group]
    }
}

type Key = (String, String);

static FACTS: LazyLock<Mutex<HashMap<Key, TurnFacts>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn facts() -> std::sync::MutexGuard<'static, HashMap<Key, TurnFacts>> {
    // 表里只有登记值,持锁时 panic 不会留下半改的结构,中毒后照常用。
    FACTS.lock().unwrap_or_else(PoisonError::into_inner)
}

/// 一轮起跑:清掉这个会话上一轮没被取走的登记,记下这一轮从 `started_at_ms` 算起。上一轮没写成
/// 运行记录(没收口的轮不记)时,归给它的 claude 用量交还调用方,改记成轮外用量。
pub(crate) fn begin(agent: &str, tag: &str, started_at_ms: i64) -> Vec<(i64, UsagePiece)> {
    let key = (agent.to_string(), tag.to_string());
    let mut table = facts();
    let stale = table.remove(&key).map(|f| f.claude).unwrap_or_default();
    table.insert(
        key,
        TurnFacts {
            started_at_ms: Some(started_at_ms),
            ..Default::default()
        },
    );
    stale
}

/// 这一轮不再收 claude 用量:不写运行记录就收尾的轮(停机接管、转人工恢复、没跑起来),或跑得太久
/// 的轮。已归给它的 claude 用量交还调用方改记成轮外用量;其余登记留着,照常写行时不受影响。
pub(crate) fn release_claude(agent: &str, tag: &str) -> Vec<(i64, UsagePiece)> {
    let mut table = facts();
    let Some(facts) = table.get_mut(&(agent.to_string(), tag.to_string())) else {
        return Vec::new();
    };
    facts.started_at_ms = None;
    std::mem::take(&mut facts.claude)
}

/// 这个会话正在跑的那一轮从什么时刻算起;没有在跑的轮就没有。
pub(crate) fn running_since(agent: &str, tag: &str) -> Option<i64> {
    facts()
        .get(&(agent.to_string(), tag.to_string()))
        .and_then(|f| f.started_at_ms)
}

/// 读取游标提交前,既不能有仍在跑的轮,也不能有写盘失败后待交还的请求。
pub(crate) fn claude_settled(agent: &str, tag: &str) -> bool {
    facts()
        .get(&(agent.to_string(), tag.to_string()))
        .is_none_or(|f| f.started_at_ms.is_none() && f.claude.is_empty())
}

/// 一笔 claude 用量归给正在跑的那一轮:时刻在这一轮开始之后(且不晚于 `until`,轮末结算时给)。
/// 归不进去就原样交还,由调用方记成轮外用量。
pub(crate) fn note_claude(
    agent: &str,
    tag: &str,
    at_ms: i64,
    piece: UsagePiece,
    until_ms: Option<i64>,
) -> Result<(), UsagePiece> {
    let mut table = facts();
    let Some(facts) = table.get_mut(&(agent.to_string(), tag.to_string())) else {
        return Err(piece);
    };
    match facts.started_at_ms {
        Some(start) if at_ms >= start && until_ms.is_none_or(|end| at_ms <= end) => {
            facts.claude.push((at_ms, piece));
            Ok(())
        }
        _ => Err(piece),
    }
}

/// 往这个会话这一轮的登记里补事实。
pub(crate) fn note(agent: &str, tag: &str, update: impl FnOnce(&mut TurnFacts)) {
    update(
        facts()
            .entry((agent.to_string(), tag.to_string()))
            .or_default(),
    );
}

fn take(agent: &str, tag: &str) -> TurnFacts {
    facts()
        .remove(&(agent.to_string(), tag.to_string()))
        .unwrap_or_default()
}

/// chat 线程手里的那一半。
pub(crate) struct TurnBasics {
    pub(crate) agent: String,
    pub(crate) chat_id: String,
    pub(crate) tag: String,
    pub(crate) origin: TurnOrigin,
    pub(crate) schedule: Option<String>,
    pub(crate) requester: Option<String>,
    pub(crate) message_id: String,
    pub(crate) prompt_head: String,
    pub(crate) started_at_ms: i64,
    pub(crate) end: TurnEnd,
    pub(crate) reattached: bool,
}

/// 台账的目录:`~/.summon/run/turns/`。测试进程写到临时目录:测试里会跑完整的一轮,不能往真实的
/// 家目录里记账。
pub(crate) fn dir() -> PathBuf {
    if cfg!(test) {
        std::env::temp_dir().join("summon-test-turns")
    } else {
        dir_in(&crate::runtime::paths::run_dir())
    }
}

/// `run` 目录下的台账目录。写(daemon)与读(控制台)都经这里拼,两边落在同一处。
pub(crate) fn dir_in(run_dir: &Path) -> PathBuf {
    run_dir.join("turns")
}

/// 终态确定:取走捕获层的登记,拼成一行写进这个 agent 当月的文件。
pub(crate) fn finish(basics: TurnBasics, ended_at_ms: i64) {
    let agent = basics.agent.clone();
    let message_id = basics.message_id.clone();
    if let Err(error) = finish_at(&dir(), basics, ended_at_ms) {
        log(&summon_i18n::format!("r2706904cf3"));
    }
}

/// 写盘失败的 claude 请求仍由登记表持有,交给累计器按未写成运行记录的轮处理。
/// append 出错不证明没有写入任何字节;这里保留原始请求,不提供跨文件的恰好一次保证。
fn finish_at(dir: &Path, basics: TurnBasics, ended_at_ms: i64) -> anyhow::Result<()> {
    let facts = take(&basics.agent, &basics.tag);
    let claude = facts.claude.clone();
    let record = assemble(basics, facts, ended_at_ms);
    append(dir, &record).inspect_err(|_| {
        if !claude.is_empty() {
            note(&record.agent, &record.tag, |f| {
                f.started_at_ms = None;
                f.claude.extend(claude);
            });
        }
    })
}

/// 同 [`finish`],写进指定目录(回放对账用)。
#[cfg(test)]
pub(crate) fn finish_into(dir: &Path, basics: TurnBasics, ended_at_ms: i64) {
    finish_at(dir, basics, ended_at_ms).unwrap();
}

fn assemble(basics: TurnBasics, facts: TurnFacts, ended_at_ms: i64) -> TurnRecord {
    let usage = facts.usage();
    TurnRecord {
        agent: basics.agent,
        chat_id: basics.chat_id,
        tag: basics.tag,
        turn: facts.turn,
        origin: basics.origin,
        schedule: basics.schedule,
        requester: basics.requester,
        message_id: basics.message_id,
        prompt_head: basics.prompt_head,
        started_at_ms: basics.started_at_ms,
        ended_at_ms,
        // 失败类别只对失败的轮有意义:被打断的轮也可能带着超时之类的登记,不留。
        error: (basics.end == TurnEnd::Failed)
            .then_some(facts.error)
            .flatten(),
        end: basics.end,
        reattached: basics.reattached,
        tool_steps: facts.tool_steps,
        cli: facts.cli,
        model: facts.model,
        effort: facts.effort,
        ctx: facts.ctx,
        usage,
        quota: facts.quota,
        feed: facts.feed,
    }
}

/// 按月存、只追加的一类行:运行记录与轮外用量。
trait Row: serde::Serialize + Clone + Sized + 'static {
    fn agent(&self) -> &str;
    /// 这一行落在哪一刻:决定写进哪个月的文件,读取时按它筛选与排序。
    fn at_ms(&self) -> i64;
    fn parse(text: &str) -> Vec<Self>;
    fn cache() -> &'static Mutex<HashMap<PathBuf, Parsed<Self>>>;
}

impl Row for TurnRecord {
    fn agent(&self) -> &str {
        &self.agent
    }
    fn at_ms(&self) -> i64 {
        self.ended_at_ms
    }
    fn parse(text: &str) -> Vec<Self> {
        turn_ledger::parse_lines(text)
    }
    fn cache() -> &'static Mutex<HashMap<PathBuf, Parsed<Self>>> {
        &READ_CACHE
    }
}

impl Row for OutsideUsage {
    fn agent(&self) -> &str {
        &self.agent
    }
    fn at_ms(&self) -> i64 {
        self.from_ms
    }
    fn parse(text: &str) -> Vec<Self> {
        turn_ledger::parse_outside_lines(text)
    }
    fn cache() -> &'static Mutex<HashMap<PathBuf, Parsed<Self>>> {
        &OUTSIDE_CACHE
    }
}

/// 同一进程里写同一个文件的只有 daemon:一把锁让每行整行落盘。
static WRITE: Mutex<()> = Mutex::new(());

fn append<T: Row>(dir: &Path, row: &T) -> anyhow::Result<()> {
    let agent_dir = dir.join(row.agent());
    std::fs::create_dir_all(&agent_dir)?;
    let mut line = serde_json::to_vec(row)?;
    line.push(b'\n');
    let _guard = WRITE.lock().unwrap_or_else(PoisonError::into_inner);
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(agent_dir.join(turn_ledger::file_name(row.at_ms())))?;
    file.write_all(&line)?;
    Ok(())
}

/// 轮外用量的目录:`~/.summon/run/usage/`。测试进程写到临时目录(同 [`dir`])。
pub(crate) fn outside_dir() -> PathBuf {
    if cfg!(test) {
        std::env::temp_dir().join("summon-test-usage")
    } else {
        outside_dir_in(&crate::runtime::paths::run_dir())
    }
}

/// `run` 目录下的轮外用量目录。
pub(crate) fn outside_dir_in(run_dir: &Path) -> PathBuf {
    run_dir.join("usage")
}

/// 写一段轮外用量。
pub(crate) fn append_outside(dir: &Path, row: &OutsideUsage) -> anyhow::Result<()> {
    append(dir, row)
}

/// daemon 启动时调:删掉整月都在保留期之前的文件。失败只记日志。
pub(crate) fn prune(dir: &Path, now_ms: i64) {
    let Ok(agents) = std::fs::read_dir(dir) else {
        return;
    };
    for agent in agents.flatten() {
        let Ok(files) = std::fs::read_dir(agent.path()) else {
            continue;
        };
        for file in files.flatten() {
            let name = file.file_name().to_string_lossy().into_owned();
            if turn_ledger::expired(&name, now_ms, turn_ledger::KEEP_DAYS) {
                if let Err(error) = std::fs::remove_file(file.path()) {
                    log(&summon_i18n::format!("r36d8f500b6"));
                }
            }
        }
    }
}

/// 读过的文件:解析到哪个字节、解析出了哪些行。文件只追加,变长时只解析新增的尾部。
struct Parsed<T> {
    offset: u64,
    rows: Vec<T>,
}

impl<T> Default for Parsed<T> {
    fn default() -> Self {
        Self {
            offset: 0,
            rows: Vec::new(),
        }
    }
}

static READ_CACHE: LazyLock<Mutex<HashMap<PathBuf, Parsed<TurnRecord>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static OUTSIDE_CACHE: LazyLock<Mutex<HashMap<PathBuf, Parsed<OutsideUsage>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// 一个文件里落在 `[since_ms, until_ms)` 的行(带缓存)。文件比上次读到的短(被删了重建)就从头读。
fn read_file<T: Row>(path: &Path, since_ms: i64, until_ms: i64) -> Vec<T> {
    let mut cache = T::cache().lock().unwrap_or_else(PoisonError::into_inner);
    let Ok(mut file) = std::fs::File::open(path) else {
        cache.remove(path);
        return Vec::new();
    };
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    let entry = cache.entry(path.to_path_buf()).or_default();
    if len < entry.offset {
        *entry = Parsed::default();
    }
    if len > entry.offset && file.seek(SeekFrom::Start(entry.offset)).is_ok() {
        let mut tail = Vec::new();
        if file.take(len - entry.offset).read_to_end(&mut tail).is_ok() {
            // 只吃完整的行:正在写的那一行留到下次。
            let complete = tail.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
            let text = String::from_utf8_lossy(&tail[..complete]);
            entry.rows.extend(T::parse(&text));
            entry.offset += complete as u64;
        }
    }
    entry
        .rows
        .iter()
        .filter(|r| r.at_ms() >= since_ms && r.at_ms() < until_ms)
        .cloned()
        .collect()
}

fn read_rows<T: Row>(dir: &Path, agent: Option<&str>, since_ms: i64, until_ms: i64) -> Vec<T> {
    let agents: Vec<PathBuf> = match agent {
        Some(alias) => vec![dir.join(alias)],
        None => std::fs::read_dir(dir)
            .map(|it| it.flatten().map(|e| e.path()).collect())
            .unwrap_or_default(),
    };
    let months = months_between(since_ms, until_ms);
    let mut out: Vec<T> = agents
        .iter()
        .flat_map(|agent_dir| months.iter().map(move |m| agent_dir.join(m)))
        .flat_map(|path| read_file::<T>(&path, since_ms, until_ms))
        .collect();
    out.sort_by_key(|r| std::cmp::Reverse(r.at_ms()));
    // 过了保留期的月份不会再被查到:缓存里的一并放掉,常驻的 daemon 不会越攒越多。
    let now_ms = chrono::Local::now().timestamp_millis();
    T::cache()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .retain(|path, _| {
            path.file_name()
                .and_then(|n| n.to_str())
                .is_none_or(|n| !turn_ledger::expired(n, now_ms, turn_ledger::KEEP_DAYS))
        });
    out
}

/// `[since_ms, until_ms)` 里结束的轮,按结束时间从新到旧。`agent` 为 `None` 时读全部 agent。
pub(crate) fn read_range(
    dir: &Path,
    agent: Option<&str>,
    since_ms: i64,
    until_ms: i64,
) -> Vec<TurnRecord> {
    read_rows(dir, agent, since_ms, until_ms)
}

/// `[since_ms, until_ms)` 里开始的轮外用量,从新到旧。
pub(crate) fn read_outside(
    dir: &Path,
    agent: Option<&str>,
    since_ms: i64,
    until_ms: i64,
) -> Vec<OutsideUsage> {
    read_rows(dir, agent, since_ms, until_ms)
}

/// 覆盖 `[since_ms, until_ms)` 的那几个月的文件名。
fn months_between(since_ms: i64, until_ms: i64) -> Vec<String> {
    let mut names = Vec::new();
    let mut at = since_ms;
    let step = 24 * 3600 * 1000;
    while at < until_ms {
        let name = turn_ledger::file_name(at);
        if names.last() != Some(&name) {
            names.push(name);
        }
        at += step;
    }
    let last = turn_ledger::file_name(until_ms.saturating_sub(1));
    if names.last() != Some(&last) {
        names.push(last);
    }
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    fn basics(agent: &str, tag: &str, end: TurnEnd) -> TurnBasics {
        TurnBasics {
            agent: agent.into(),
            chat_id: "oc_00000000000000000000000000000001".into(),
            tag: tag.into(),
            origin: TurnOrigin::Human,
            schedule: None,
            requester: Some("ou_00000000000000000000000000000001".into()),
            message_id: "om_1".into(),
            prompt_head: "帮我看下".into(),
            started_at_ms: 1_000,
            end,
            reattached: false,
        }
    }

    #[test]
    fn tests_never_write_the_ledger_into_the_real_home() {
        assert!(dir().starts_with(std::env::temp_dir()));
        assert!(!dir().starts_with(crate::runtime::paths::summon_home()));
    }

    #[test]
    fn facts_noted_during_the_turn_land_in_its_record_once() {
        let (agent, tag) = ("ledger-test-a", "g1");
        begin(agent, tag, 0);
        note(agent, tag, |f| f.tool_steps = 4);
        note(agent, tag, |f| f.error = Some("timed_out".into()));
        let record = assemble(basics(agent, tag, TurnEnd::Failed), take(agent, tag), 9_000);
        assert_eq!(record.tool_steps, 4);
        assert_eq!(record.error.as_deref(), Some("timed_out"));
        // 取走之后不再串到下一轮
        assert_eq!(take(agent, tag), TurnFacts::default());
        // 非失败的轮不留失败类别
        note(agent, tag, |f| f.error = Some("timed_out".into()));
        let done = assemble(basics(agent, tag, TurnEnd::Done), take(agent, tag), 9_000);
        assert_eq!(done.error, None);
    }

    #[test]
    fn failed_turn_writes_keep_claude_requests_until_they_are_claimed_once() {
        let tmp = tempfile::tempdir().unwrap();
        let blocked = tmp.path().join("turns");
        std::fs::write(&blocked, "blocked").unwrap();
        let (agent, tag) = ("ledger-failed-claude", "g1");
        begin(agent, tag, 1_000);
        let piece = UsagePiece {
            output: 7,
            ..Default::default()
        };
        note_claude(agent, tag, 2_000, piece.clone(), None).unwrap();
        assert!(finish_at(&blocked, basics(agent, tag, TurnEnd::Done), 3_000).is_err());
        assert_eq!(running_since(agent, tag), None);
        assert!(!claude_settled(agent, tag));
        assert_eq!(
            note_claude(agent, tag, 4_000, piece, None)
                .unwrap_err()
                .output,
            7
        );
        let unwritten = release_claude(agent, tag);
        assert_eq!(unwritten.iter().map(|(_, p)| p.output).sum::<u64>(), 7);
        assert!(release_claude(agent, tag).is_empty());
        assert!(claude_settled(agent, tag));
    }

    fn request(id: &str, model: &str, output: u64) -> RequestUsage {
        RequestUsage {
            id: id.into(),
            model: Some(model.into()),
            input: 3,
            output,
            cache_read: 1_000,
            cache_write_5m: 0,
            cache_write_1h: 40,
            variant: None,
        }
    }

    #[test]
    fn a_request_seen_again_counts_once_with_its_final_numbers() {
        let mut facts = TurnFacts::default();
        // 同一次请求的用量又推了一次:各项取较大值
        facts.absorb_request(&request("msg_1", "claude-x", 10));
        facts.absorb_request(&request("msg_1", "claude-x", 250));
        facts.absorb_request(&request("msg_2", "claude-x", 5));
        // 子 agent 用的另一个模型单独一组
        facts.absorb_request(&request("msg_3", "claude-y", 7));
        // 有逐次请求时,按轮的合计不再计入
        facts.absorb_usage(&Usage {
            input_tokens: 999,
            ..Default::default()
        });
        let usage = facts.usage();
        assert_eq!(usage.len(), 2);
        assert_eq!(usage[0].model, "claude-x");
        assert_eq!(usage[0].requests, 2);
        assert_eq!(usage[0].output, 255);
        assert_eq!(usage[0].cache_write_1h, 80);
        assert_eq!(usage[1].model, "claude-y");
    }

    #[test]
    fn a_turn_total_without_requests_lands_as_one_group_under_the_turn_model() {
        let mut facts = TurnFacts {
            model: Some("claude-x[context=1m]".into()),
            ..Default::default()
        };
        assert!(facts.usage().is_empty());
        facts.absorb_usage(&Usage::default());
        assert!(facts.usage().is_empty());
        facts.absorb_usage(&Usage {
            input_tokens: 40,
            output_tokens: 10_720,
            cache_read: 7_158_122,
            cache_write: 419_852,
        });
        let usage = facts.usage();
        assert_eq!(usage.len(), 1);
        assert_eq!(usage[0].model, "claude-x[context=1m]");
        assert_eq!((usage[0].band, usage[0].requests), (0, 1));
        assert_eq!(usage[0].cache_write_5m, 419_852);
    }

    #[test]
    fn begin_drops_leftovers_of_an_unfinished_turn() {
        let (agent, tag) = ("ledger-test-b", "g1");
        note(agent, tag, |f| f.tool_steps = 9);
        assert!(begin(agent, tag, 5).is_empty());
        let facts = take(agent, tag);
        assert_eq!(facts.tool_steps, 0);
        assert_eq!(facts.started_at_ms, Some(5));
    }

    fn claude_piece(output: u64) -> UsagePiece {
        UsagePiece {
            model: Some("claude-x".into()),
            new_request: true,
            output,
            ..Default::default()
        }
    }

    #[test]
    fn claude_usage_goes_to_the_running_turn_only_inside_its_window() {
        let (agent, tag) = ("ledger-test-claude", "g1");
        // 没有在跑的轮:交还
        assert_eq!(running_since(agent, tag), None);
        assert!(note_claude(agent, tag, 100, claude_piece(1), None).is_err());
        assert!(begin(agent, tag, 1_000).is_empty());
        assert_eq!(running_since(agent, tag), Some(1_000));
        // 轮开始之前的请求不算这一轮
        assert!(note_claude(agent, tag, 999, claude_piece(2), None).is_err());
        assert!(note_claude(agent, tag, 1_000, claude_piece(3), None).is_ok());
        // 轮末结算时晚于结束时刻的也不算
        assert!(note_claude(agent, tag, 5_001, claude_piece(4), Some(5_000)).is_err());
        assert!(note_claude(agent, tag, 5_000, claude_piece(5), Some(5_000)).is_ok());
        // 这一轮没写成运行记录就开了下一轮:归给它的交还出来
        let stale = begin(agent, tag, 9_000);
        assert_eq!(
            stale
                .iter()
                .map(|(at, p)| (*at, p.output))
                .collect::<Vec<_>>(),
            [(1_000, 3), (5_000, 5)]
        );
        assert!(note_claude(agent, tag, 9_500, claude_piece(6), None).is_ok());
        let record = assemble(basics(agent, tag, TurnEnd::Done), take(agent, tag), 9_900);
        assert_eq!(record.usage.len(), 1);
        assert_eq!((record.usage[0].requests, record.usage[0].output), (1, 6));
    }

    #[test]
    fn a_released_turn_hands_back_its_claude_usage_and_takes_no_more() {
        let (agent, tag) = ("ledger-test-release", "g1");
        begin(agent, tag, 1_000);
        note(agent, tag, |f| f.tool_steps = 2);
        assert!(note_claude(agent, tag, 1_500, claude_piece(7), None).is_ok());
        let back = release_claude(agent, tag);
        assert_eq!(back.iter().map(|(_, p)| p.output).collect::<Vec<_>>(), [7]);
        assert_eq!(running_since(agent, tag), None);
        assert!(note_claude(agent, tag, 2_000, claude_piece(8), None).is_err());
        assert_eq!(take(agent, tag).tool_steps, 2);
        assert!(release_claude(agent, tag).is_empty());
    }

    #[test]
    fn outside_usage_is_appended_and_read_back_by_its_start() {
        let tmp = tempfile::tempdir().unwrap();
        let now = chrono::Local::now().timestamp_millis();
        let row = |from_ms: i64| OutsideUsage {
            agent: "ledger-e".into(),
            chat_id: "oc_00000000000000000000000000000001".into(),
            tag: "g1".into(),
            cli: "claude".into(),
            from_ms,
            to_ms: from_ms + 10,
            usage: Vec::new(),
        };
        append_outside(tmp.path(), &row(now - 2_000)).unwrap();
        append_outside(tmp.path(), &row(now - 1_000)).unwrap();
        let got = read_outside(tmp.path(), Some("ledger-e"), now - 1_500, now + 1);
        assert_eq!(got, [row(now - 1_000)]);
        assert_eq!(
            read_outside(tmp.path(), None, now - 5_000, now + 1).len(),
            2
        );
    }

    #[test]
    fn appended_records_are_read_back_incrementally_and_by_range() {
        let tmp = tempfile::tempdir().unwrap();
        let now = chrono::Local::now().timestamp_millis();
        let mut first = assemble(
            basics("ledger-c", "g1", TurnEnd::Done),
            TurnFacts::default(),
            now - 1_000,
        );
        first.message_id = "om_first".into();
        append(tmp.path(), &first).unwrap();
        let day = 24 * 3600 * 1000;
        assert_eq!(
            read_range(tmp.path(), Some("ledger-c"), now - day, now + 1).len(),
            1
        );
        let mut second = first.clone();
        second.message_id = "om_second".into();
        second.ended_at_ms = now - 500;
        append(tmp.path(), &second).unwrap();
        // 一行写了一半:不读进来
        let file = tmp
            .path()
            .join("ledger-c")
            .join(turn_ledger::file_name(now));
        std::fs::OpenOptions::new()
            .append(true)
            .open(&file)
            .unwrap()
            .write_all(b"{\"agent\":")
            .unwrap();
        let got = read_range(tmp.path(), None, now - day, now + 1);
        assert_eq!(
            got.iter()
                .map(|r| r.message_id.as_str())
                .collect::<Vec<_>>(),
            ["om_second", "om_first"]
        );
        assert!(read_range(tmp.path(), None, now - day, now - 800)
            .iter()
            .all(|r| r.message_id == "om_first"));
    }

    #[test]
    fn reading_drops_months_past_the_window_from_the_cache() {
        use chrono::TimeZone;
        let tmp = tempfile::tempdir().unwrap();
        let agent = tmp.path().join("ledger-evict");
        std::fs::create_dir_all(&agent).unwrap();
        let now = chrono::Local::now().timestamp_millis();
        let old = agent.join("2020-01.ndjson");
        let current = agent.join(turn_ledger::file_name(now));
        for path in [&old, &current] {
            std::fs::write(path, "").unwrap();
        }
        let jan_2020 = chrono::Local
            .with_ymd_and_hms(2020, 1, 15, 12, 0, 0)
            .unwrap()
            .timestamp_millis();
        read_range(tmp.path(), Some("ledger-evict"), jan_2020, jan_2020 + 1);
        read_range(tmp.path(), Some("ledger-evict"), now - 1, now + 1);
        let cache = READ_CACHE.lock().unwrap_or_else(PoisonError::into_inner);
        assert!(!cache.contains_key(&old));
        assert!(cache.contains_key(&current));
    }

    #[test]
    fn prune_removes_only_whole_months_past_the_window() {
        let tmp = tempfile::tempdir().unwrap();
        let agent = tmp.path().join("ledger-d");
        std::fs::create_dir_all(&agent).unwrap();
        for name in ["2020-01.ndjson", "notes.txt"] {
            std::fs::write(agent.join(name), "").unwrap();
        }
        let now = chrono::Local::now().timestamp_millis();
        let current = turn_ledger::file_name(now);
        std::fs::write(agent.join(&current), "").unwrap();
        prune(tmp.path(), now);
        assert!(!agent.join("2020-01.ndjson").exists());
        assert!(agent.join("notes.txt").exists());
        assert!(agent.join(current).exists());
    }

    #[test]
    fn badge_and_outcome_fill_the_facts() {
        let mut facts = TurnFacts::default();
        let badge = TurnBadge {
            cli: "claude".into(),
            effort: Some("high".into()),
            quota_5h_left_pct: Some(40),
            ..Default::default()
        };
        facts.absorb_badge(&badge);
        assert_eq!(facts.cli.as_deref(), Some("claude"));
        assert_eq!(
            facts.quota.as_ref().and_then(|q| q.five_hour_left),
            Some(40)
        );
        assert_eq!(facts.ctx, None);
        let outcome = TurnOutcome {
            reply: Some("修好了。<summon-feed>修好构建</summon-feed>".into()),
            timed_out: true,
            ..Default::default()
        };
        facts.absorb_outcome(&Ok(outcome), None);
        assert_eq!(facts.error.as_deref(), Some("timed_out"));
        assert_eq!(facts.feed.as_deref(), Some("修好构建"));
        facts.absorb_outcome(&Err(anyhow::anyhow!("boom")), None);
        assert_eq!(facts.error.as_deref(), Some("capture"));
    }
}
