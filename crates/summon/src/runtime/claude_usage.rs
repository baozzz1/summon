//! claude 会话的用量记账:读会话目录里的转录文件(主会话 `<sid>.jsonl`、子 agent 的
//! `<sid>/subagents/*.jsonl`、workflow 扇出的子 agent 的
//! `<sid>/subagents/workflows/<wf>/agent-*.jsonl`),每次模型请求按它发生的时刻归属。
//!
//! - 时刻落在本 chat 正在跑的那一轮里(轮开始之后、轮还没结束)→ 算进这一轮的运行记录,不管是主会话
//!   还是哪个子 agent、子 agent 是哪一轮派出的;轮的起点就是运行记录那一行的 `started_at_ms`(重启后
//!   接管的轮是这一代接手的时刻);
//! - 其余(两轮之间还在跑的后台子 agent、不经 summon 唤起的轮、没写成运行记录的轮)→ 这个会话的轮外
//!   用量(`runtime::turn_ledger::append_outside`)。
//!
//! claude 的请求用量只从转录读,事件流不带 claude 的请求用量,两条路不会重复计。
//!
//! daemon 后台每 [`SCAN_EVERY`] 扫一遍全部 claude 会话,只读新增的字节;一轮开始与结束时 chat 线程
//! 再同步读一次到文件末尾([`begin_turn`]、[`settle_turn`]),轮末的请求不会漏到下一轮;不写运行记录
//! 就收尾的轮由 [`abandon_turn`] 交还。
//!
//! 每个 chat 一份游标(`threads/<tag>/usage-cursor.json`),只在读到的用量都已落盘(运行记录或轮外
//! 用量)之后才存:重启后从游标重读,按时刻重新归属,不丢不重。两次写之间崩溃会让那一批重复计。
//! 第一次见到一个会话(没有游标)时,本代开始记账之后才冷启的会话从冷启时刻算起,更早就有的会话从
//! [`FIRST_SIGHT_SLACK_MS`] 之前算起,不补历史。
//!
//! 记账是旁路:读不到、写不进都只记日志,不影响一轮的收尾、回复与销账。

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex, PoisonError};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};
use summon_core::claude_usage::{self, FileCursor};
use summon_core::registry::{Agent, Registry};
use summon_core::turn_ledger::{OutsideUsage, UsagePiece};

use crate::observe::log::log;
use crate::runtime::turn_ledger;

/// 后台扫描的间隔。
const SCAN_EVERY: Duration = Duration::from_secs(30);
/// 轮外用量攒多久落一次盘:后台子 agent 往往一阵一阵地跑,不必每次请求写一行。
const FLUSH_AFTER_MS: i64 = 10 * 60 * 1000;
/// 第一次见到一个会话时往回算多久:刚才那几分钟里的请求照计,更早的是上线之前的历史,不补。
const FIRST_SIGHT_SLACK_MS: i64 = 5 * 60 * 1000;
/// 新会话从它冷启时刻往前多少算起(冷启时刻只记到秒)。
const NEW_SESSION_SLACK_MS: i64 = 60 * 1000;
/// 游标里记几个离开过的会话:换回其中之一时从离开那一刻接着算。更早离开的按没见过处理:换回时早于
/// 「上次存盘」与「换回前 5 分钟」两者较晚者的请求不再计,其中也包括它离开之后写下的。
const LEFT_KEPT: usize = 16;
/// 多久没动过的会话不必开始记账(没有游标时)。
const IDLE_SKIP_MS: i64 = 24 * 3600 * 1000;
/// 一轮跑了这么久还没收尾,就不再往里记:多半是收尾的路径没有交还(见 [`abandon_turn`]),之后的
/// 用量记成轮外用量,游标照常推进。
const STALE_TURN_MS: i64 = 6 * 3600 * 1000;
/// 一次最多读多少字节:新见到的大转录分块读,不整份读进内存。
const READ_CHUNK: usize = 4 << 20;

/// 本代开始记账的时刻:在它之后冷启的会话,第一次见到时从冷启时刻算起。
static SINCE: LazyLock<i64> = LazyLock::new(now_ms);

/// 一个 chat 的游标:读的是哪个会话、从什么时刻算起,各文件读到哪儿。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct Cursor {
    /// 原生会话 id;空 = 还没见过。
    session: String,
    /// 这个会话的主转录:换会话、会话被拆时先把它读完。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    main: Option<PathBuf>,
    /// 新出现的文件里,早于它的请求不计。
    floor_ms: i64,
    /// 上次存盘的时刻。
    #[serde(default)]
    committed_at_ms: i64,
    /// 键是相对于会话所在目录的路径:`<sid>.jsonl`、`<sid>/subagents/<名字>.jsonl`、
    /// `<sid>/subagents/workflows/<wf>/<名字>.jsonl`。
    #[serde(default)]
    files: BTreeMap<String, FileCursor>,
    /// 离开过的会话读到了哪一刻(换走时读完它的那一刻),键是会话 id。
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    left: BTreeMap<String, i64>,
}

/// 未提交的读取进度。轮外用量存在时,它的缓冲起点与待提交游标属于同一个阶段。
#[derive(Default)]
enum Pending {
    #[default]
    Committed,
    Cursor,
    Outside {
        buffered_at_ms: i64,
        pieces: Vec<(i64, UsagePiece)>,
    },
}

impl Pending {
    fn read(&mut self) {
        if matches!(self, Self::Committed) {
            *self = Self::Cursor;
        }
    }

    fn outside(&self) -> &[(i64, UsagePiece)] {
        match self {
            Self::Outside { pieces, .. } => pieces,
            _ => &[],
        }
    }
}

/// 一个 chat 的记账状态。
struct Account {
    agent: String,
    chat_id: String,
    tag: String,
    cursor_path: PathBuf,
    cursor: Cursor,
    /// 本代开始记账的时刻(见 [`SINCE`])。
    since_ms: i64,
    pending: Pending,
}

impl Account {
    fn load(agent: &str, chat_id: &str, tag: &str, cursor_path: PathBuf) -> Self {
        let cursor = std::fs::read(&cursor_path)
            .ok()
            .and_then(|raw| serde_json::from_slice(&raw).ok())
            .unwrap_or_default();
        Self {
            agent: agent.to_string(),
            chat_id: chat_id.to_string(),
            tag: tag.to_string(),
            cursor_path,
            cursor,
            since_ms: *SINCE,
            pending: Pending::Committed,
        }
    }

    /// 一个会话从什么时刻算起:冷启在本代开始记账之后的,从冷启时刻算;更早就有的(或不知道冷启
    /// 时刻的),从 `fallback_ms` 算,不补那之前的历史。
    fn floor_for(&self, born_ms: Option<i64>, fallback_ms: i64) -> i64 {
        match born_ms.map(|b| b - NEW_SESSION_SLACK_MS) {
            Some(b) if b >= self.since_ms - FIRST_SIGHT_SLACK_MS => b,
            _ => fallback_ms,
        }
    }

    /// 换到另一个会话时从什么时刻算起。离开过的会话(`/resume` 换回来的)从离开时读到的那一刻接着
    /// 算。没见过的会话里,冷启晚于上次存盘的是这个 chat 的新会话,从冷启时刻算;其余(冷启更早的、
    /// 不知道冷启时刻的 Bridge 召回会话)按第一次见到处理:往回看 [`FIRST_SIGHT_SLACK_MS`],且不早于
    /// 上次存盘 —— 存盘之前的请求要么已经记过,要么不归这个 chat。
    fn switch_floor(&mut self, sid: &str, born_ms: Option<i64>, now_ms: i64) -> i64 {
        if let Some(left) = self.cursor.left.remove(sid) {
            return left;
        }
        let committed = self.cursor.committed_at_ms;
        match born_ms {
            Some(born) if born >= committed => born - NEW_SESSION_SLACK_MS,
            _ => committed.max(now_ms - FIRST_SIGHT_SLACK_MS),
        }
    }

    /// 记下离开的会话读到了哪一刻,只留最近 [`LEFT_KEPT`] 个。
    fn leave(&mut self, sid: String, now_ms: i64) {
        self.cursor.left.insert(sid, now_ms);
        while self.cursor.left.len() > LEFT_KEPT {
            let Some(oldest) = self
                .cursor
                .left
                .iter()
                .min_by_key(|(_, at)| **at)
                .map(|(sid, _)| sid.clone())
            else {
                break;
            };
            self.cursor.left.remove(&oldest);
        }
    }

    /// 读到各文件末尾。`sid` / `main` 是此刻的会话与主转录;与游标里的不同就是换了会话:先把旧会话
    /// 读完,新会话的起算时刻见 [`Self::switch_floor`]。
    fn read(
        &mut self,
        sid: &str,
        main: &Path,
        born_ms: Option<i64>,
        now_ms: i64,
    ) -> Vec<(i64, UsagePiece)> {
        let mut out = Vec::new();
        if self.cursor.session != sid {
            if self.cursor.session.is_empty() {
                self.cursor.floor_ms = self.floor_for(born_ms, now_ms - FIRST_SIGHT_SLACK_MS);
            } else {
                out.extend(self.read_old());
                let previous = std::mem::take(&mut self.cursor.session);
                self.leave(previous, now_ms);
                self.cursor.floor_ms = self.switch_floor(sid, born_ms, now_ms);
                self.cursor.files.clear();
            }
            self.cursor.session = sid.to_string();
            self.pending.read();
        }
        if self.cursor.main.as_deref() != Some(main) {
            self.cursor.main = Some(main.to_path_buf());
            self.pending.read();
        }
        out.extend(self.read_session(main));
        out
    }

    /// 游标里记着的那个会话读到末尾(换会话、会话被拆之后用)。
    fn read_old(&mut self) -> Vec<(i64, UsagePiece)> {
        match self.cursor.main.clone() {
            Some(old) => self.read_session(&old),
            None => Vec::new(),
        }
    }

    /// 一个会话的主转录与它的子 agent 转录,各自从游标读到末尾。
    fn read_session(&mut self, main: &Path) -> Vec<(i64, UsagePiece)> {
        let Some(base) = main.parent() else {
            return Vec::new();
        };
        let mut files = vec![main.to_path_buf()];
        let subagents = main.with_extension("").join("subagents");
        files.extend(transcripts_in(&subagents, |_| true));
        // 同一个 workflow 目录里的 journal.jsonl 是编排日志,不是模型请求。
        if let Ok(entries) = std::fs::read_dir(subagents.join("workflows")) {
            let mut runs: Vec<PathBuf> = entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.is_dir())
                .collect();
            runs.sort();
            for run in runs {
                files.extend(transcripts_in(&run, |name| name.starts_with("agent-")));
            }
        }
        let mut out = Vec::new();
        for path in files {
            let Ok(key) = path
                .strip_prefix(base)
                .map(|k| k.to_string_lossy().into_owned())
            else {
                continue;
            };
            out.extend(self.read_file(&key, &path));
        }
        out
    }

    fn read_file(&mut self, key: &str, path: &Path) -> Vec<(i64, UsagePiece)> {
        use std::io::{Read as _, Seek as _, SeekFrom};
        let Ok(meta) = std::fs::metadata(path) else {
            return Vec::new();
        };
        let len = meta.len();
        let floor = self.cursor.floor_ms;
        let cursor = match self.cursor.files.get_mut(key) {
            Some(c) => c,
            None => {
                // 新见到的文件:最后一次改动在起算时刻之前,里面就不会有要计的请求,直接记到末尾。
                let modified_ms = meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map_or(i64::MAX, |d| {
                        i64::try_from(d.as_millis()).unwrap_or(i64::MAX)
                    });
                let offset = if modified_ms < floor { len } else { 0 };
                self.pending.read();
                self.cursor
                    .files
                    .entry(key.to_string())
                    .or_insert(FileCursor {
                        offset,
                        ..Default::default()
                    })
            }
        };
        if len < cursor.offset {
            // 转录只追加;变短说明文件被换掉了,从头按起算时刻重读。
            *cursor = FileCursor::default();
            self.pending.read();
        }
        let mut out = Vec::new();
        let mut chunk = READ_CHUNK;
        while cursor.offset < len {
            let want = usize::try_from(len - cursor.offset).map_or(chunk, |left| left.min(chunk));
            let mut bytes = Vec::with_capacity(want);
            let read = std::fs::File::open(path).and_then(|mut f| {
                f.seek(SeekFrom::Start(cursor.offset))?;
                f.take(want as u64).read_to_end(&mut bytes)
            });
            if let Err(error) = read {
                log(&summon_i18n::format!(
                    "r82b2e98b18",
                    self.agent,
                    path.display()
                ));
                break;
            }
            let before = cursor.offset;
            out.extend(claude_usage::feed(cursor, &bytes, floor));
            if cursor.offset != before {
                self.pending.read();
                chunk = READ_CHUNK;
            } else if bytes.len() == want && cursor.offset + (want as u64) < len {
                // 一行比这一块还长:放大再读这一行。
                chunk = chunk.saturating_mul(2);
            } else {
                // 末尾那一行还没写完,留到下次。
                break;
            }
        }
        out
    }

    /// 跑得太久的轮不再收用量,已归给它的改记成轮外用量(见 [`STALE_TURN_MS`])。
    fn release_if_stale(&mut self, now_ms: i64) {
        let stale = turn_ledger::running_since(&self.agent, &self.tag)
            .is_some_and(|start| now_ms - start >= STALE_TURN_MS);
        if stale {
            let back = turn_ledger::release_claude(&self.agent, &self.tag);
            self.keep(back, now_ms);
        }
    }

    fn keep(&mut self, pieces: Vec<(i64, UsagePiece)>, now_ms: i64) {
        if pieces.is_empty() {
            return;
        }
        match &mut self.pending {
            Pending::Outside {
                pieces: buffered, ..
            } => buffered.extend(pieces),
            _ => {
                self.pending = Pending::Outside {
                    buffered_at_ms: now_ms,
                    pieces,
                }
            }
        }
    }

    /// 归属:落在正在跑的那一轮里的记进它(`until` 是轮末结算时的结束时刻),其余进缓冲。
    fn absorb(&mut self, pieces: Vec<(i64, UsagePiece)>, until_ms: Option<i64>, now_ms: i64) {
        self.release_if_stale(now_ms);
        for (at, piece) in pieces {
            if let Err(piece) =
                turn_ledger::note_claude(&self.agent, &self.tag, at, piece, until_ms)
            {
                self.keep(vec![(at, piece)], now_ms);
            }
        }
    }

    /// 缓冲里的轮外用量按天写成行。写成的那几天从缓冲里拿掉,写不进的留着下次再写;全写完返回真。
    fn flush(&mut self, dir: &Path) -> bool {
        if !matches!(self.pending, Pending::Outside { .. }) {
            return true;
        }
        let Pending::Outside {
            buffered_at_ms,
            pieces,
        } = std::mem::replace(&mut self.pending, Pending::Cursor)
        else {
            unreachable!();
        };
        let mut days: BTreeMap<Option<chrono::NaiveDate>, Vec<(i64, UsagePiece)>> = BTreeMap::new();
        for (at, piece) in pieces {
            let day = chrono::DateTime::from_timestamp_millis(at)
                .map(|t| t.with_timezone(&chrono::Local).date_naive());
            days.entry(day).or_default().push((at, piece));
        }
        for (_, group) in days {
            let rows = summon_core::turn_ledger::split_by_day(&chrono::Local, &group);
            let written = rows.into_iter().try_for_each(|(from_ms, to_ms, usage)| {
                turn_ledger::append_outside(
                    dir,
                    &OutsideUsage {
                        agent: self.agent.clone(),
                        chat_id: self.chat_id.clone(),
                        tag: self.tag.clone(),
                        cli: "claude".into(),
                        from_ms,
                        to_ms,
                        usage,
                    },
                )
            });
            if let Err(error) = written {
                log(&summon_i18n::format!(
                    "rc6161ff8b9",
                    self.agent,
                    self.chat_id
                ));
                self.keep(group, buffered_at_ms);
            }
        }
        self.pending.outside().is_empty()
    }

    /// 只有轮外缓冲和轮内登记都已结清才存游标;失败时内存中的提交时刻也保持不变。
    fn commit(&mut self, now_ms: i64) -> bool {
        if !self.pending.outside().is_empty()
            || !turn_ledger::claude_settled(&self.agent, &self.tag)
        {
            return false;
        }
        let mut committed = self.cursor.clone();
        committed.committed_at_ms = now_ms;
        let written = serde_json::to_vec(&committed)
            .map_err(anyhow::Error::from)
            .and_then(|raw| {
                if let Some(dir) = self.cursor_path.parent() {
                    std::fs::create_dir_all(dir)?;
                }
                crate::runtime::fsio::atomic_write(&self.cursor_path, &raw)
            });
        match written {
            Ok(()) => {
                self.cursor = committed;
                self.pending = Pending::Committed;
                true
            }
            Err(error) => {
                log(&summon_i18n::format!(
                    "rb339c5f1a5",
                    self.agent,
                    self.chat_id
                ));
                self.pending.read();
                false
            }
        }
    }

    /// 缓冲写完了才存游标;写不进时游标不动,下次连同缓冲一起再来。
    fn flush_and_commit(&mut self, dir: &Path, now_ms: i64) {
        if self.flush(dir) {
            self.commit(now_ms);
        }
    }

    /// 一轮开始:先把此刻之前读到的记成轮外用量(上一轮没写成运行记录时,归给它的也交还过来),
    /// 再登记这一轮。
    fn on_turn_begin(&mut self, pieces: Vec<(i64, UsagePiece)>, started_ms: i64, dir: &Path) {
        self.absorb(pieces, None, started_ms);
        let stale = turn_ledger::release_claude(&self.agent, &self.tag);
        self.keep(stale, started_ms);
        self.flush_and_commit(dir, started_ms);
        turn_ledger::begin(&self.agent, &self.tag, started_ms);
    }

    /// 一轮结束:结束时刻之前读到的归这一轮,`write(结束时刻)` 写运行记录;之后才到的记成轮外用量。
    fn on_turn_end(
        &mut self,
        pieces: Vec<(i64, UsagePiece)>,
        ended_ms: i64,
        dir: &Path,
        write: impl FnOnce(i64),
    ) {
        self.absorb(pieces, Some(ended_ms), ended_ms);
        write(ended_ms);
        let unwritten = turn_ledger::release_claude(&self.agent, &self.tag);
        self.keep(unwritten, ended_ms);
        self.flush_and_commit(dir, ended_ms);
    }

    /// 一轮没写运行记录就收尾:归给它的与此刻读到的都记成轮外用量。
    fn on_turn_abandoned(&mut self, pieces: Vec<(i64, UsagePiece)>, now_ms: i64, dir: &Path) {
        let back = turn_ledger::release_claude(&self.agent, &self.tag);
        self.keep(back, now_ms);
        self.absorb(pieces, None, now_ms);
        self.flush_and_commit(dir, now_ms);
    }

    /// 这个 chat 盘上已经没有会话记录了(重置、归档之后还没起新会话,或群没了):把记着的那个会话
    /// 读完、写掉。游标目录还在就存游标;不在(群没了,状态已回收)就不再建回来。全写完返回真。
    fn retire(&mut self, dir: &Path, now_ms: i64) -> bool {
        if turn_ledger::running_since(&self.agent, &self.tag).is_some() {
            return false;
        }
        let unwritten = turn_ledger::release_claude(&self.agent, &self.tag);
        self.keep(unwritten, now_ms);
        let pieces = self.read_old();
        self.absorb(pieces, None, now_ms);
        if !self.flush(dir) {
            return false;
        }
        if self.cursor_path.parent().is_some_and(Path::exists) {
            return self.commit(now_ms);
        }
        true
    }

    /// 扫描时的落盘策略:没有在跑的轮时,缓冲攒够 [`FLUSH_AFTER_MS`] 才写;缓冲空了就存游标。
    /// 有在跑的轮时什么都不存:归给它的用量还没写进运行记录。
    fn settle_idle(&mut self, dir: &Path, now_ms: i64, force: bool) {
        if turn_ledger::running_since(&self.agent, &self.tag).is_some() {
            return;
        }
        let due = matches!(&self.pending, Pending::Outside { buffered_at_ms, .. }
            if force || now_ms - buffered_at_ms >= FLUSH_AFTER_MS);
        if due && !self.flush(dir) {
            return;
        }
        if matches!(self.pending, Pending::Cursor) {
            self.commit(now_ms);
        }
    }
}

/// 目录里文件名满足 `keep` 的转录(`.jsonl`),按路径排序。
fn transcripts_in(dir: &Path, keep: impl Fn(&str) -> bool) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut found: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
        .filter(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(&keep))
        .collect();
    found.sort();
    found
}

/// 盘上的一个 claude 会话。
#[derive(Clone)]
struct ChatSession {
    chat_id: String,
    tag: String,
    session_name: String,
    sid: String,
    born_ms: Option<i64>,
}

/// 一个 agent 的会话清单与读它时目录的 mtime。
type SessionList = (SystemTime, Vec<ChatSession>);

/// 按目录 mtime 缓存的会话清单:`.session` 与 `.meta` 都经原子改名落盘,目录 mtime 随之变。
static SESSIONS: LazyLock<Mutex<HashMap<PathBuf, SessionList>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// 这个 agent 盘上记着的 claude 会话(`runtime/chats/<会话名>.session` 配同名 `.meta`)。
fn chat_sessions(agent: &Agent) -> Vec<ChatSession> {
    let dir = agent.state_dir().join("runtime").join("chats");
    let Ok(mtime) = std::fs::metadata(&dir).and_then(|m| m.modified()) else {
        return Vec::new();
    };
    if let Some((at, cached)) = SESSIONS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(&dir)
    {
        if *at == mtime {
            return cached.clone();
        }
    }
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let found: Vec<ChatSession> = entries
        .flatten()
        .filter_map(|e| {
            let path = e.path();
            let session_name = path
                .file_name()?
                .to_str()?
                .strip_suffix(".session")?
                .to_string();
            let record =
                summon_core::session_id::parse_record(&std::fs::read_to_string(&path).ok()?)?;
            if record.cli != "claude" {
                return None;
            }
            let meta = summon_core::chat_meta::ChatMeta::parse(
                &std::fs::read_to_string(path.with_extension("meta")).ok()?,
            );
            let born_ms = record.created_at.as_deref().and_then(local_time_ms);
            Some(ChatSession {
                chat_id: meta.chat_id?,
                tag: meta.tag?,
                session_name,
                sid: record.id,
                born_ms,
            })
        })
        .collect();
    SESSIONS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(dir, (mtime, found.clone()));
    found
}

fn local_time_ms(text: &str) -> Option<i64> {
    use chrono::TimeZone;
    let naive = chrono::NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M:%S").ok()?;
    Some(
        chrono::Local
            .from_local_datetime(&naive)
            .earliest()?
            .timestamp_millis(),
    )
}

type Key = (String, String);

static ACCOUNTS: LazyLock<Mutex<HashMap<Key, Arc<Mutex<Account>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn account(agent: &Agent, chat: &ChatSession) -> Arc<Mutex<Account>> {
    let key = (agent.name.clone(), chat.tag.clone());
    let mut table = ACCOUNTS.lock().unwrap_or_else(PoisonError::into_inner);
    Arc::clone(table.entry(key).or_insert_with(|| {
        let path = agent
            .state_dir()
            .join("threads")
            .join(&chat.tag)
            .join("usage-cursor.json");
        Arc::new(Mutex::new(Account::load(
            &agent.name,
            &chat.chat_id,
            &chat.tag,
            path,
        )))
    }))
}

fn existing_account(agent: &str, tag: &str) -> Option<Arc<Mutex<Account>>> {
    ACCOUNTS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(&(agent.to_string(), tag.to_string()))
        .cloned()
}

fn has_account(agent: &str, tag: &str) -> bool {
    ACCOUNTS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .contains_key(&(agent.to_string(), tag.to_string()))
}

fn lock(account: &Mutex<Account>) -> std::sync::MutexGuard<'_, Account> {
    // 记账状态只有游标与缓冲,持锁时 panic 不会留下半改的结构,中毒后照常用。
    account.lock().unwrap_or_else(PoisonError::into_inner)
}

/// 解析出的路径还不是文件(会话刚冷启、claude 还没写出第一行)时,隔多久再解析一次:预测的路径
/// 每次都 stat,文件一出现就能读到;重新解析只为预测落空(转录其实在别处)的情形。
const RESOLVE_AGAIN_MS: i64 = 60 * 1000;
/// 解析出错的会话多久再试:按 SID 搜索要列一遍全部项目目录。
const RESOLVE_RETRY_MS: i64 = 10 * 60 * 1000;

/// 一个会话的转录路径:解析出的路径与解析的时刻,或解析出错的时刻。
type Resolved = Result<(PathBuf, i64), i64>;

/// 解析过的转录路径,按 (agent, 会话 id)。锁里只存取,解析在锁外做。
static RESOLVED: LazyLock<Mutex<HashMap<(String, String), Resolved>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// 这个会话的主转录:解析出的路径缓存起来,每次只 stat;还不是文件时没有。
fn cached_transcript(
    key: (String, String),
    now_ms: i64,
    resolve: impl FnOnce() -> anyhow::Result<PathBuf>,
) -> Option<PathBuf> {
    let cached = RESOLVED
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(&key)
        .cloned();
    let path = match cached {
        Some(Ok((path, _))) if path.is_file() => return Some(path),
        Some(Ok((_, at))) if now_ms - at < RESOLVE_AGAIN_MS => return None,
        Some(Err(at)) if now_ms - at < RESOLVE_RETRY_MS => return None,
        _ => resolve(),
    };
    let entry = match &path {
        Ok(p) => Ok((p.clone(), now_ms)),
        Err(_) => Err(now_ms),
    };
    RESOLVED
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(key, entry);
    path.ok().filter(|p| p.is_file())
}

fn transcript(agent: &Agent, chat: &ChatSession, now_ms: i64) -> Option<PathBuf> {
    cached_transcript((agent.name.clone(), chat.sid.clone()), now_ms, || {
        crate::capture::resolve_transcript_path(
            agent,
            &chat.session_name,
            Some(&chat.chat_id),
            None,
            &chat.sid,
        )
    })
}

/// 读一个会话到文件末尾。
fn read_chat(
    agent: &Agent,
    chat: &ChatSession,
    acc: &mut Account,
    now_ms: i64,
) -> Vec<(i64, UsagePiece)> {
    match transcript(agent, chat, now_ms) {
        Some(main) => acc.read(&chat.sid, &main, chat.born_ms, now_ms),
        None => Vec::new(),
    }
}

fn now_ms() -> i64 {
    chrono::Local::now().timestamp_millis()
}

fn find_chat(agent: &Agent, tag: &str) -> Option<ChatSession> {
    if agent.cli_kind() != "claude" {
        return None;
    }
    chat_sessions(agent).into_iter().find(|c| c.tag == tag)
}

/// 一轮开始(`started_at_ms` 与这一轮运行记录的起点是同一个值):先把此刻之前的请求读完、记成
/// 轮外用量,再登记这一轮(见 `turn_ledger::begin`)。上一轮没写成运行记录时,归给它的 claude 用量
/// 也改记成轮外用量。
pub(crate) fn begin_turn(agent: &Agent, tag: &str, started_at_ms: i64) {
    LazyLock::force(&SINCE);
    let Some(chat) = find_chat(agent, tag) else {
        if let Some(account) = existing_account(&agent.name, tag) {
            let mut acc = lock(&account);
            let back = turn_ledger::release_claude(&agent.name, tag);
            acc.keep(back, started_at_ms);
            let _ = acc.retire(&turn_ledger::outside_dir(), started_at_ms);
            turn_ledger::begin(&agent.name, tag, started_at_ms);
        } else {
            turn_ledger::begin(&agent.name, tag, started_at_ms);
        }
        return;
    };
    let account = account(agent, &chat);
    let mut acc = lock(&account);
    let pieces = read_chat(agent, &chat, &mut acc, started_at_ms);
    acc.on_turn_begin(pieces, started_at_ms, &turn_ledger::outside_dir());
}

/// 一轮结束:读到文件末尾,结束时刻之前的归这一轮,然后由 `write(结束时刻)` 写运行记录;之后才到的
/// 记成轮外用量,游标存盘。
pub(crate) fn settle_turn(agent: &Agent, tag: &str, write: impl FnOnce(i64)) {
    let Some(chat) = find_chat(agent, tag) else {
        if let Some(account) = existing_account(&agent.name, tag) {
            let mut acc = lock(&account);
            let ended = now_ms();
            let pieces = acc.read_old();
            acc.absorb(pieces, Some(ended), ended);
            write(ended);
            let back = turn_ledger::release_claude(&agent.name, tag);
            acc.keep(back, ended);
            let _ = acc.retire(&turn_ledger::outside_dir(), ended);
        } else {
            write(now_ms());
        }
        return;
    };
    let account = account(agent, &chat);
    let mut acc = lock(&account);
    let ended = now_ms();
    let pieces = read_chat(agent, &chat, &mut acc, ended);
    acc.on_turn_end(pieces, ended, &turn_ledger::outside_dir(), write);
}

/// 一轮不写运行记录就收尾(停机接管、转人工恢复、没跑起来):不再往它里面记,归给它的 claude 用量
/// 与此刻读到的都记成轮外用量。
pub(crate) fn abandon_turn(agent: &Agent, tag: &str) {
    let Some(chat) = find_chat(agent, tag) else {
        if let Some(account) = existing_account(&agent.name, tag) {
            let mut acc = lock(&account);
            let now = now_ms();
            let back = turn_ledger::release_claude(&agent.name, tag);
            acc.keep(back, now);
            let _ = acc.retire(&turn_ledger::outside_dir(), now);
        } else {
            turn_ledger::release_claude(&agent.name, tag);
        }
        return;
    };
    let account = account(agent, &chat);
    let mut acc = lock(&account);
    let now = now_ms();
    let pieces = read_chat(agent, &chat, &mut acc, now);
    acc.on_turn_abandoned(pieces, now, &turn_ledger::outside_dir());
}

/// 扫一遍全部 claude 会话。盘上已经没有的会话(重置、归档之后还没起新会话,或群没了)把记着的
/// 那个会话读完、写掉:游标目录还在就存游标,不在(群没了,状态已回收)就只写用量、不再记账。
static SCAN_LOCK: Mutex<()> = Mutex::new(());

pub(crate) fn forget_agent(alias: &str) {
    let _scan = SCAN_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    ACCOUNTS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .retain(|(agent, _), _| agent != alias);
}

fn scan(agents_dir: &Path) {
    let _scan = SCAN_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    let agents = match Registry::load_tolerant(agents_dir) {
        Ok((reg, _)) => reg.agents,
        Err(error) => {
            log(&summon_i18n::format!("r064df70e96"));
            return;
        }
    };
    let outside = turn_ledger::outside_dir();
    let mut seen: HashSet<Key> = HashSet::new();
    for agent in agents
        .iter()
        .filter(|a| a.cli_kind() == "claude" && !super::agent_retirement::retired(&a.dir))
    {
        for chat in chat_sessions(agent) {
            let now = now_ms();
            seen.insert((agent.name.clone(), chat.tag.clone()));
            if !has_account(&agent.name, &chat.tag) && !recently_active(agent, &chat, now) {
                continue;
            }
            let account = account(agent, &chat);
            let mut acc = lock(&account);
            let pieces = read_chat(agent, &chat, &mut acc, now);
            acc.absorb(pieces, None, now);
            acc.settle_idle(&outside, now, false);
        }
    }
    let gone: Vec<(Key, Arc<Mutex<Account>>)> = ACCOUNTS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .iter()
        .filter(|(key, _)| !seen.contains(*key))
        .map(|(key, acc)| (key.clone(), Arc::clone(acc)))
        .collect();
    for (key, account) in gone {
        let mut acc = lock(&account);
        if turn_ledger::running_since(&acc.agent, &acc.tag).is_some()
            || !acc.retire(&outside, now_ms())
        {
            continue;
        }
        drop(acc);
        ACCOUNTS
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&key);
    }
}

/// 没有游标的会话,主转录最近一天里动过才开始记账。
fn recently_active(agent: &Agent, chat: &ChatSession, now_ms: i64) -> bool {
    transcript(agent, chat, now_ms)
        .and_then(|p| std::fs::metadata(p).ok())
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .is_some_and(|d| now_ms - i64::try_from(d.as_millis()).unwrap_or(0) < IDLE_SKIP_MS)
}

/// 后台扫描线程:每 [`SCAN_EVERY`] 一遍,`shutdown` 置位后退出。
pub(crate) fn spawn_scanner(
    agents_dir: PathBuf,
    shutdown: Arc<AtomicBool>,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    LazyLock::force(&SINCE);
    std::thread::Builder::new()
        .name("claude-usage".into())
        .spawn(move || {
            while !shutdown.load(Ordering::SeqCst) {
                scan(&agents_dir);
                let mut waited = Duration::ZERO;
                while waited < SCAN_EVERY && !shutdown.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_secs(1));
                    waited += Duration::from_secs(1);
                }
            }
        })
}

/// 优雅停机:没有在跑的轮的会话,把缓冲里的轮外用量写掉、游标存盘。有在跑的轮的会话不动 ——
/// 归给它的用量随这一代一起没了,下一代从游标重读、按时刻重新归属。
pub(crate) fn flush_on_shutdown() {
    let accounts: Vec<Arc<Mutex<Account>>> = ACCOUNTS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .values()
        .cloned()
        .collect();
    let outside = turn_ledger::outside_dir();
    let now = now_ms();
    for account in accounts {
        lock(&account).settle_idle(&outside, now, true);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(id: &str, at_ms: i64, output: u64) -> String {
        let at = chrono::DateTime::from_timestamp_millis(at_ms)
            .unwrap()
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        format!(
            r#"{{"type":"assistant","timestamp":"{at}","message":{{"id":"{id}","model":"claude-test-1","content":[],"usage":{{"input_tokens":1,"cache_read_input_tokens":0,"output_tokens":{output}}}}}}}"#
        )
    }

    fn append(path: &Path, lines: &[String]) {
        use std::io::Write as _;
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        for l in lines {
            writeln!(f, "{l}").unwrap();
        }
    }

    struct Fixture {
        _tmp: tempfile::TempDir,
        main: PathBuf,
        sub: PathBuf,
        outside: PathBuf,
        cursor: PathBuf,
    }

    fn fixture() -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("projects").join("-work");
        Fixture {
            main: project.join("sid-1.jsonl"),
            sub: project
                .join("sid-1")
                .join("subagents")
                .join("agent-a1.jsonl"),
            outside: tmp.path().join("usage"),
            cursor: tmp
                .path()
                .join("threads")
                .join("g1")
                .join("usage-cursor.json"),
            _tmp: tmp,
        }
    }

    fn outputs(rows: &[OutsideUsage]) -> u64 {
        rows.iter().flat_map(|r| &r.usage).map(|u| u.output).sum()
    }

    #[test]
    fn requests_go_to_the_turn_they_happened_in_and_the_rest_are_outside_usage() {
        let f = fixture();
        let (agent, tag) = ("usage-test-a", "g1");
        let t0 = chrono::Local::now().timestamp_millis();
        let mut acc = Account::load(agent, "oc_1", tag, f.cursor.clone());
        // 第一次见到这个会话:起算时刻之前的不补
        append(&f.main, &[line("old", t0 - FIRST_SIGHT_SLACK_MS - 1, 999)]);
        let first = acc.read("sid-1", &f.main, None, t0);
        assert!(first.is_empty());
        // 两轮之间后台子 agent 的请求:轮外
        append(&f.sub, &[line("s1", t0 + 1_000, 7)]);
        let pieces = acc.read("sid-1", &f.main, None, t0 + 2_000);
        acc.absorb(pieces, None, t0 + 2_000);
        assert_eq!(acc.pending.outside().len(), 1);
        // 一轮开始:主会话与(早先派出、此刻续用的)子 agent 的请求都算这一轮
        assert!(turn_ledger::begin(agent, tag, t0 + 10_000).is_empty());
        acc.flush(&f.outside);
        append(&f.main, &[line("m1", t0 + 11_000, 5)]);
        append(
            &f.sub,
            &[line("s2", t0 + 12_000, 3), line("s2", t0 + 12_500, 30)],
        );
        let pieces = acc.read("sid-1", &f.main, None, t0 + 13_000);
        acc.absorb(pieces, Some(t0 + 20_000), t0 + 13_000);
        // 轮末之后才到的:轮外
        append(&f.sub, &[line("s3", t0 + 25_000, 11)]);
        let pieces = acc.read("sid-1", &f.main, None, t0 + 26_000);
        acc.absorb(pieces, Some(t0 + 20_000), t0 + 26_000);
        let facts = turn_ledger::begin(agent, tag, t0 + 30_000);
        let in_turn: u64 = facts.iter().map(|(_, p)| p.output).sum();
        assert_eq!(in_turn, 5 + 30);
        assert_eq!(facts.iter().filter(|(_, p)| p.new_request).count(), 2);
        acc.flush(&f.outside);
        let day = 24 * 3600 * 1000;
        let rows = turn_ledger::read_outside(&f.outside, Some(agent), t0 - day, t0 + day);
        assert_eq!(outputs(&rows), 7 + 11);
        turn_ledger::begin(agent, tag, 0);
    }

    #[test]
    fn the_cursor_is_saved_only_when_asked_and_a_restart_rereads_from_it() {
        let f = fixture();
        let (agent, tag) = ("usage-test-b", "g1");
        let t0 = chrono::Local::now().timestamp_millis();
        append(&f.main, &[line("a", t0, 1)]);
        let mut acc = Account::load(agent, "oc_1", tag, f.cursor.clone());
        assert!(!acc.read("sid-1", &f.main, None, t0).is_empty());
        acc.commit(t0);
        append(&f.main, &[line("b", t0 + 1_000, 2)]);
        // 读到了 b,但没落盘就停了
        assert_eq!(acc.read("sid-1", &f.main, None, t0 + 2_000).len(), 1);
        let mut again = Account::load(agent, "oc_1", tag, f.cursor.clone());
        let reread = again.read("sid-1", &f.main, None, t0 + 3_000);
        assert_eq!(
            reread.iter().map(|(_, p)| p.output).collect::<Vec<_>>(),
            [2]
        );
    }

    #[test]
    fn a_new_session_finishes_the_old_one_and_counts_from_its_own_start() {
        let f = fixture();
        let (agent, tag) = ("usage-test-c", "g1");
        let t0 = chrono::Local::now().timestamp_millis();
        append(&f.main, &[line("a", t0, 1)]);
        let mut acc = Account::load(agent, "oc_1", tag, f.cursor.clone());
        acc.read("sid-1", &f.main, None, t0);
        acc.commit(t0);
        // 旧会话最后又写了一次,随后换成了新会话(新会话的文件里带着冷启之前的历史)
        append(&f.main, &[line("b", t0 + 1_000, 2)]);
        let next = f.main.with_file_name("sid-2.jsonl");
        append(
            &next,
            &[line("x", t0 - 3_600_000, 50), line("y", t0 + 5_000, 4)],
        );
        let pieces = acc.read("sid-2", &next, Some(t0 + 4_000), t0 + 6_000);
        assert_eq!(
            pieces.iter().map(|(_, p)| p.output).collect::<Vec<_>>(),
            [2, 4]
        );
        assert_eq!(acc.cursor.session, "sid-2");
        assert_eq!(acc.cursor.files.len(), 1);
    }

    #[test]
    fn switching_back_to_an_earlier_session_does_not_recount_its_history() {
        let f = fixture();
        let (agent, tag) = ("usage-test-r", "g1");
        // 全部时刻都在过去:换回时新见到的文件按真实改动时间判断要不要读
        let t0 = chrono::Local::now().timestamp_millis() - 600_000;
        let born_1 = t0 - 3_600_000;
        append(&f.main, &[line("a", t0, 1)]);
        append(&f.sub, &[line("s", t0 + 500, 2)]);
        let mut acc = Account::load(agent, "oc_1", tag, f.cursor.clone());
        let first = acc.read("sid-1", &f.main, Some(born_1), t0 + 1_000);
        assert_eq!(first.len(), 2);
        acc.commit(t0 + 1_000);
        let next = f.main.with_file_name("sid-2.jsonl");
        append(&next, &[line("x", t0 + 5_000, 4)]);
        acc.read("sid-2", &next, Some(t0 + 4_000), t0 + 6_000);
        acc.commit(t0 + 6_000);
        // `/resume` 换回 sid-1:它的冷启时刻不变,历史不再计,换回之后的照计
        append(&f.main, &[line("c", t0 + 8_000, 8)]);
        let back = acc.read("sid-1", &f.main, Some(born_1), t0 + 9_000);
        assert_eq!(back.iter().map(|(_, p)| p.output).collect::<Vec<_>>(), [8]);
    }

    #[test]
    fn switched_sessions_continue_where_they_were_left_or_look_back_a_little() {
        let f = fixture();
        let mut acc = Account::load("usage-test-f", "oc_1", "g1", f.cursor.clone());
        acc.cursor.committed_at_ms = 1_000_000;
        // 离开过的会话:从离开那一刻接着算,不看冷启时刻,换回之后不再留着
        acc.leave("sid-old".into(), 5_000_000);
        assert_eq!(acc.switch_floor("sid-old", Some(10), 9_000_000), 5_000_000);
        assert!(acc.cursor.left.is_empty());
        // 没见过、冷启晚于上次存盘:新会话,从冷启前一点算
        assert_eq!(
            acc.switch_floor("sid-new", Some(2_000_000), 9_000_000),
            2_000_000 - NEW_SESSION_SLACK_MS
        );
        // 没见过、冷启更早:刚存过盘就从存盘时刻算,存盘已久只往回看一小段
        assert_eq!(acc.switch_floor("sid-x", Some(10), 1_000_500), 1_000_000);
        assert_eq!(
            acc.switch_floor("sid-x", Some(10), 9_000_000),
            9_000_000 - FIRST_SIGHT_SLACK_MS
        );
        // 不知道冷启时刻的同样处理,从没存过盘也不翻历史
        acc.cursor.committed_at_ms = 0;
        assert_eq!(
            acc.switch_floor("sid-x", None, 9_000_000),
            9_000_000 - FIRST_SIGHT_SLACK_MS
        );
        // 只留最近几个
        for i in 0..=LEFT_KEPT as i64 {
            acc.leave(format!("sid-{i}"), i);
        }
        assert_eq!(acc.cursor.left.len(), LEFT_KEPT);
        assert!(!acc.cursor.left.contains_key("sid-0"));
    }

    /// 换走时读出的尾巴还在缓冲里、游标没存盘,随后又换回来:换走前读过的不再计,换走之后才写的照计。
    #[test]
    fn switching_back_before_a_commit_neither_recounts_nor_drops() {
        let f = fixture();
        let (agent, tag) = ("usage-test-b", "g1");
        let t0 = chrono::Local::now().timestamp_millis() - 600_000;
        append(&f.main, &[line("a", t0, 1)]);
        let mut acc = Account::load(agent, "oc_1", tag, f.cursor.clone());
        assert_eq!(acc.read("sid-1", &f.main, None, t0 + 1_000).len(), 1);
        acc.commit(t0 + 1_000);
        append(&f.main, &[line("b", t0 + 2_000, 2)]);
        let next = f.main.with_file_name("sid-2.jsonl");
        append(&next, &[line("x", t0 + 3_500, 4)]);
        let away = acc.read("sid-2", &next, Some(t0 + 3_000), t0 + 4_000);
        assert_eq!(
            away.iter().map(|(_, p)| p.output).collect::<Vec<_>>(),
            [2, 4]
        );
        append(&f.main, &[line("m", t0 + 4_500, 16)]);
        append(&f.sub, &[line("c", t0 + 5_000, 8)]);
        let back = acc.read("sid-1", &f.main, None, t0 + 6_000);
        assert_eq!(
            back.iter().map(|(_, p)| p.output).collect::<Vec<_>>(),
            [16, 8]
        );
    }

    #[test]
    fn cursors_saved_without_left_sessions_still_load_and_round_trip() {
        let old = r#"{"session":"sid-1","floor_ms":5,"committed_at_ms":7,"files":{}}"#;
        let cursor: Cursor = serde_json::from_str(old).unwrap();
        assert!(cursor.left.is_empty());
        assert!(!serde_json::to_string(&cursor).unwrap().contains("left"));
        let mut cursor = cursor;
        cursor.left.insert("sid-0".into(), 3);
        let again: Cursor = serde_json::from_str(&serde_json::to_string(&cursor).unwrap()).unwrap();
        assert_eq!(again, cursor);
    }

    #[test]
    fn workflow_subagents_are_counted_and_their_journal_is_not() {
        let f = fixture();
        let (agent, tag) = ("usage-test-w", "g1");
        let t0 = chrono::Local::now().timestamp_millis();
        let mut acc = Account::load(agent, "oc_1", tag, f.cursor.clone());
        acc.read("sid-1", &f.main, None, t0);
        let run = f.sub.with_file_name("workflows").join("wf_1");
        append(&run.join("agent-w1.jsonl"), &[line("w1", t0 + 1_000, 3)]);
        append(&run.join("agent-w2.jsonl"), &[line("w2", t0 + 2_000, 5)]);
        append(&run.join("journal.jsonl"), &[line("j", t0 + 3_000, 100)]);
        let pieces = acc.read("sid-1", &f.main, None, t0 + 4_000);
        assert_eq!(
            pieces.iter().map(|(_, p)| p.output).collect::<Vec<_>>(),
            [3, 5]
        );
        assert!(acc
            .cursor
            .files
            .contains_key("sid-1/subagents/workflows/wf_1/agent-w1.jsonl"));
    }

    #[test]
    fn idle_chats_flush_after_a_while_and_running_turns_hold_everything() {
        let f = fixture();
        let (agent, tag) = ("usage-test-d", "g1");
        let t0 = chrono::Local::now().timestamp_millis();
        let mut acc = Account::load(agent, "oc_1", tag, f.cursor.clone());
        acc.read("sid-1", &f.main, None, t0);
        append(&f.sub, &[line("s1", t0 + 1_000, 7)]);
        let pieces = acc.read("sid-1", &f.main, None, t0 + 1_000);
        acc.absorb(pieces, None, t0 + 1_000);
        acc.settle_idle(&f.outside, t0 + 2_000, false);
        assert!(!acc.pending.outside().is_empty() && !f.cursor.exists());
        acc.settle_idle(&f.outside, t0 + 1_000 + FLUSH_AFTER_MS, false);
        assert!(acc.pending.outside().is_empty() && f.cursor.exists());
        // 有在跑的轮:一律不存
        turn_ledger::begin(agent, tag, t0 + 900_000);
        acc.keep(vec![(t0, UsagePiece::default())], t0);
        acc.settle_idle(&f.outside, t0 + 10 * FLUSH_AFTER_MS, true);
        assert_eq!(acc.pending.outside().len(), 1);
        assert!(!matches!(acc.pending, Pending::Committed));
        turn_ledger::begin(agent, tag, 0);
    }

    #[test]
    fn a_transcript_that_appears_after_its_session_record_is_read_once_it_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("sid-late.jsonl");
        let key = ("usage-test-late".to_string(), "sid-late".to_string());
        let t0 = chrono::Local::now().timestamp_millis();
        // 扫描赶在 claude 写出第一行之前:解析出的路径还不是文件
        assert_eq!(
            cached_transcript(key.clone(), t0, || Ok(path.clone())),
            None
        );
        std::fs::write(&path, "").unwrap();
        // 文件一出现就读得到,不等重新解析
        let again = cached_transcript(key.clone(), t0 + 1_000, || panic!("不该重新解析"));
        assert_eq!(again, Some(path));
        // 解析出错才隔一阵再试
        let bad = ("usage-test-late".to_string(), "sid-bad".to_string());
        assert_eq!(
            cached_transcript(bad.clone(), t0, || anyhow::bail!("找不到")),
            None
        );
        assert_eq!(
            cached_transcript(bad, t0 + 1_000, || panic!("不该重新解析")),
            None
        );
    }

    #[test]
    fn the_first_turn_of_a_session_seen_late_keeps_its_usage() {
        let f = fixture();
        let (agent, tag) = ("usage-test-e", "g1");
        let t0 = chrono::Local::now().timestamp_millis();
        let mut acc = Account::load(agent, "oc_1", tag, f.cursor.clone());
        acc.since_ms = t0 - 60_000;
        // 一轮开始时转录还没写出来,什么都读不到
        acc.on_turn_begin(Vec::new(), t0 + 1_000, &f.outside);
        append(&f.main, &[line("m1", t0 + 2_000, 5)]);
        append(&f.sub, &[line("s1", t0 + 3_000, 7)]);
        // 很久之后才第一次读到这个会话:它冷启于本代开始记账之后,从冷启时刻算起
        let late = t0 + 15 * 60_000;
        let pieces = acc.read("sid-1", &f.main, Some(t0), late);
        let turns = f.outside.with_file_name("turns");
        acc.on_turn_end(pieces, late, &f.outside, |ended| {
            turn_ledger::finish_into(&turns, basics(agent, tag, t0 + 1_000), ended);
        });
        let day = 24 * 3600 * 1000;
        let rows = turn_ledger::read_range(&turns, Some(agent), t0 - day, t0 + day);
        assert_eq!(rows[0].usage.iter().map(|u| u.output).sum::<u64>(), 12);
        assert_eq!(rows[0].started_at_ms, t0 + 1_000);
    }

    #[test]
    fn outside_usage_that_cannot_be_written_stays_buffered_and_the_cursor_stays_put() {
        let f = fixture();
        let (agent, tag) = ("usage-test-f", "g1");
        let t0 = chrono::Local::now().timestamp_millis();
        let mut acc = Account::load(agent, "oc_1", tag, f.cursor.clone());
        acc.read("sid-1", &f.main, None, t0);
        append(&f.sub, &[line("s1", t0 + 1_000, 7)]);
        let pieces = acc.read("sid-1", &f.main, None, t0 + 1_000);
        acc.absorb(pieces, None, t0 + 1_000);
        // 目录位置上是个文件:写不进
        std::fs::create_dir_all(f.outside.parent().unwrap()).unwrap();
        std::fs::write(&f.outside, "").unwrap();
        acc.settle_idle(&f.outside, t0 + FLUSH_AFTER_MS + 2_000, false);
        assert_eq!(acc.pending.outside().len(), 1);
        assert!(!f.cursor.exists());
        std::fs::remove_file(&f.outside).unwrap();
        acc.settle_idle(&f.outside, t0 + FLUSH_AFTER_MS + 3_000, false);
        assert!(acc.pending.outside().is_empty() && f.cursor.exists());
        let day = 24 * 3600 * 1000;
        let rows = turn_ledger::read_outside(&f.outside, Some(agent), t0 - day, t0 + day);
        assert_eq!(outputs(&rows), 7);
    }

    #[test]
    fn a_failed_turn_write_keeps_usage_and_cursor_until_outside_retry() {
        let f = fixture();
        let (agent, tag) = ("usage-test-turn-write", "g1");
        let t0 = chrono::Local::now().timestamp_millis();
        let mut acc = Account::load(agent, "oc_1", tag, f.cursor.clone());
        acc.read("sid-1", &f.main, None, t0);
        acc.on_turn_begin(Vec::new(), t0, &f.outside);
        let saved = std::fs::read(&f.cursor).unwrap();
        append(&f.main, &[line("m1", t0 + 1_000, 7)]);
        let pieces = acc.read("sid-1", &f.main, None, t0 + 1_000);
        let turns = f.outside.with_file_name("turns");
        std::fs::write(&turns, "blocked").unwrap();
        std::fs::write(&f.outside, "blocked").unwrap();
        acc.on_turn_end(pieces, t0 + 2_000, &f.outside, |ended| {
            let failed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                turn_ledger::finish_into(&turns, basics(agent, tag, t0), ended);
            }));
            assert!(failed.is_err());
        });
        assert_eq!(std::fs::read(&f.cursor).unwrap(), saved);
        std::fs::remove_file(&f.outside).unwrap();
        acc.settle_idle(&f.outside, t0 + 3_000, true);
        let rows = turn_ledger::read_outside(&f.outside, Some(agent), t0, t0 + 4_000);
        assert_eq!(outputs(&rows), 7);
        assert_ne!(std::fs::read(&f.cursor).unwrap(), saved);
    }

    #[test]
    fn a_partial_outside_write_retries_only_the_failed_month() {
        let f = fixture();
        let agent = "usage-test-partial-write";
        let (early, late) = (1_767_225_600_000, 1_769_904_000_000);
        let mut acc = Account::load(agent, "oc_1", "g1", f.cursor.clone());
        acc.keep(
            vec![
                (
                    early,
                    UsagePiece {
                        output: 3,
                        ..Default::default()
                    },
                ),
                (
                    late,
                    UsagePiece {
                        output: 7,
                        ..Default::default()
                    },
                ),
            ],
            late,
        );
        let blocked = f
            .outside
            .join(agent)
            .join(summon_core::turn_ledger::file_name(late));
        std::fs::create_dir_all(&blocked).unwrap();
        acc.settle_idle(&f.outside, late + FLUSH_AFTER_MS, false);
        let rows = turn_ledger::read_outside(&f.outside, Some(agent), early, late + 1);
        assert_eq!(outputs(&rows), 3);
        assert!(!f.cursor.exists());
        std::fs::remove_dir(&blocked).unwrap();
        acc.settle_idle(&f.outside, late + FLUSH_AFTER_MS + 1, false);
        let rows = turn_ledger::read_outside(&f.outside, Some(agent), early, late + 1);
        assert_eq!(outputs(&rows), 10);
        assert!(f.cursor.exists());
    }

    #[test]
    fn turn_boundaries_keep_the_account_after_its_session_record_is_removed() {
        let f = fixture();
        let name = format!(
            "usage-test-missing-{}",
            f._tmp.path().file_name().unwrap().to_string_lossy()
        );
        let mut agent: Agent = serde_json::from_value(serde_json::json!({ "name": name })).unwrap();
        agent.dir = f._tmp.path().join("agents").join(&name);
        let tag = "g1";
        let t0 = now_ms() - 10_000;
        let mut acc = Account::load(&name, "oc_1", tag, f.cursor.clone());
        acc.read("sid-1", &f.main, None, t0);
        assert!(acc.commit(t0));
        let shared = Arc::new(Mutex::new(acc));
        let key = (name.clone(), tag.to_string());
        ACCOUNTS
            .lock()
            .unwrap()
            .insert(key.clone(), Arc::clone(&shared));
        assert!(find_chat(&agent, tag).is_none());
        begin_turn(&agent, tag, t0 + 1_000);
        append(&f.main, &[line("m1", t0 + 2_000, 7)]);
        let turns = f.outside.with_file_name("turns");
        std::fs::write(&turns, "blocked").unwrap();
        settle_turn(&agent, tag, |ended| {
            assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                turn_ledger::finish_into(&turns, basics(&name, tag, t0 + 1_000), ended);
            }))
            .is_err());
        });
        let outside = turn_ledger::outside_dir();
        let rows = turn_ledger::read_outside(&outside, Some(&name), t0, now_ms() + 1);
        assert_eq!(outputs(&rows), 7);
        begin_turn(&agent, tag, t0 + 3_000);
        append(&f.sub, &[line("s1", t0 + 4_000, 3)]);
        abandon_turn(&agent, tag);
        let rows = turn_ledger::read_outside(&outside, Some(&name), t0, now_ms() + 1);
        assert_eq!(outputs(&rows), 10);
        assert_eq!(turn_ledger::running_since(&name, tag), None);
        assert!(matches!(lock(&shared).pending, Pending::Committed));
        std::fs::remove_dir_all(f.cursor.parent().unwrap()).unwrap();
        begin_turn(&agent, tag, t0 + 5_000);
        append(&f.sub, &[line("s2", t0 + 6_000, 2)]);
        abandon_turn(&agent, tag);
        assert!(!f.cursor.parent().unwrap().exists());
        let rows = turn_ledger::read_outside(&outside, Some(&name), t0, now_ms() + 1);
        assert_eq!(outputs(&rows), 12);
        ACCOUNTS.lock().unwrap().remove(&key);
        std::fs::remove_dir_all(outside.join(&name)).unwrap();
    }

    #[test]
    fn switching_sessions_and_abandoning_a_turn_preserves_a_failed_outside_batch() {
        let f = fixture();
        let (agent, tag) = ("usage-test-pending-switch", "g1");
        let t0 = chrono::Local::now().timestamp_millis();
        let mut acc = Account::load(agent, "oc_1", tag, f.cursor.clone());
        acc.read("sid-1", &f.main, None, t0);
        append(&f.sub, &[line("old", t0 + 1_000, 3)]);
        let pieces = acc.read("sid-1", &f.main, None, t0 + 1_000);
        acc.absorb(pieces, None, t0 + 1_000);
        std::fs::write(&f.outside, "blocked").unwrap();
        acc.on_turn_begin(Vec::new(), t0 + 2_000, &f.outside);
        assert!(!f.cursor.exists());
        let next = f.main.with_file_name("sid-2.jsonl");
        append(&next, &[line("new", t0 + 3_000, 7)]);
        let pieces = acc.read("sid-2", &next, Some(t0 + 2_000), t0 + 4_000);
        acc.absorb(pieces, None, t0 + 4_000);
        acc.on_turn_abandoned(Vec::new(), t0 + 5_000, &f.outside);
        assert_eq!(acc.pending.outside().len(), 2);
        assert!(
            matches!(acc.pending, Pending::Outside { buffered_at_ms, .. }
            if buffered_at_ms == t0 + 1_000)
        );
        assert!(!f.cursor.exists());
        std::fs::remove_file(&f.outside).unwrap();
        acc.settle_idle(&f.outside, t0 + 6_000, true);
        let rows = turn_ledger::read_outside(&f.outside, Some(agent), t0, t0 + 7_000);
        assert_eq!(outputs(&rows), 10);
        let saved: Cursor = serde_json::from_slice(&std::fs::read(&f.cursor).unwrap()).unwrap();
        assert_eq!(saved.session, "sid-2");
        assert!(saved.left.contains_key("sid-1"));
        assert!(matches!(acc.pending, Pending::Committed));
    }

    #[test]
    fn cursor_commit_refuses_buffered_or_running_usage() {
        let f = fixture();
        let (agent, tag) = ("usage-test-commit-gate", "g1");
        let t0 = chrono::Local::now().timestamp_millis();
        let mut acc = Account::load(agent, "oc_1", tag, f.cursor.clone());
        acc.read("sid-1", &f.main, None, t0);
        acc.keep(
            vec![(
                t0,
                UsagePiece {
                    output: 3,
                    ..Default::default()
                },
            )],
            t0,
        );
        assert!(!acc.commit(t0));
        assert!(!f.cursor.exists());
        assert!(acc.flush(&f.outside));
        turn_ledger::begin(agent, tag, t0);
        assert!(!acc.commit(t0));
        assert!(!acc.retire(&f.outside, t0));
        assert!(!f.cursor.exists());
        turn_ledger::release_claude(agent, tag);
        assert!(acc.commit(t0));
    }

    #[test]
    fn a_failed_cursor_commit_keeps_its_previous_time_and_retries_retirement() {
        let f = fixture();
        let (agent, tag) = ("usage-test-cursor-write", "g1");
        let t0 = chrono::Local::now().timestamp_millis();
        let mut acc = Account::load(agent, "oc_1", tag, f.cursor.clone());
        acc.read("sid-1", &f.main, None, t0);
        assert!(acc.commit(t0));
        append(&f.sub, &[line("s1", t0 + 1_000, 7)]);
        // 原子写入的目标被目录占住,用量可写,游标不能替换。
        std::fs::remove_file(&f.cursor).unwrap();
        std::fs::create_dir(&f.cursor).unwrap();
        assert!(!acc.retire(&f.outside, t0 + 2_000));
        assert_eq!(acc.cursor.committed_at_ms, t0);
        assert!(matches!(acc.pending, Pending::Cursor));
        std::fs::remove_dir(&f.cursor).unwrap();
        assert!(acc.retire(&f.outside, t0 + 3_000));
        assert_eq!(acc.cursor.committed_at_ms, t0 + 3_000);
        let rows = turn_ledger::read_outside(&f.outside, Some(agent), t0, t0 + 4_000);
        assert_eq!(outputs(&rows), 7);
    }

    #[test]
    fn a_turn_that_ends_without_a_record_or_runs_too_long_hands_its_usage_back() {
        let f = fixture();
        let (agent, tag) = ("usage-test-g", "g1");
        let t0 = chrono::Local::now().timestamp_millis();
        let mut acc = Account::load(agent, "oc_1", tag, f.cursor.clone());
        acc.read("sid-1", &f.main, None, t0);
        acc.on_turn_begin(Vec::new(), t0, &f.outside);
        append(&f.sub, &[line("s1", t0 + 1_000, 7)]);
        let pieces = acc.read("sid-1", &f.main, None, t0 + 1_000);
        acc.absorb(pieces, None, t0 + 1_000);
        assert!(acc.pending.outside().is_empty());
        // 停机接管 / 转人工恢复:不写运行记录,归给它的改记成轮外用量,之后的也不再进它
        append(&f.sub, &[line("s2", t0 + 2_000, 3)]);
        let pieces = acc.read("sid-1", &f.main, None, t0 + 2_000);
        acc.on_turn_abandoned(pieces, t0 + 2_000, &f.outside);
        assert_eq!(turn_ledger::running_since(agent, tag), None);
        let day = 24 * 3600 * 1000;
        let rows = turn_ledger::read_outside(&f.outside, Some(agent), t0 - day, t0 + day);
        assert_eq!(outputs(&rows), 10);
        assert!(f.cursor.exists());
        // 跑了太久还没收尾的轮:同样交还
        acc.on_turn_begin(Vec::new(), t0 + 3_000, &f.outside);
        append(&f.sub, &[line("s3", t0 + 4_000, 2)]);
        let pieces = acc.read("sid-1", &f.main, None, t0 + 4_000);
        acc.absorb(pieces, None, t0 + 4_000);
        append(&f.sub, &[line("s4", t0 + 5_000, 1)]);
        let pieces = acc.read("sid-1", &f.main, None, t0 + 5_000);
        acc.absorb(pieces, None, t0 + 3_000 + STALE_TURN_MS);
        assert_eq!(turn_ledger::running_since(agent, tag), None);
        assert_eq!(
            acc.pending
                .outside()
                .iter()
                .map(|(_, p)| p.output)
                .sum::<u64>(),
            3
        );
    }

    #[test]
    fn a_chat_whose_session_record_is_gone_is_read_to_the_end_and_its_state_is_not_rebuilt() {
        let f = fixture();
        let (agent, tag) = ("usage-test-h", "g1");
        let t0 = chrono::Local::now().timestamp_millis();
        let mut acc = Account::load(agent, "oc_1", tag, f.cursor.clone());
        acc.read("sid-1", &f.main, None, t0);
        acc.commit(t0);
        // 重置之后:游标目录还在,读完、存游标
        append(&f.sub, &[line("s1", t0 + 1_000, 7)]);
        assert!(acc.retire(&f.outside, t0 + 2_000));
        let saved: Cursor = serde_json::from_slice(&std::fs::read(&f.cursor).unwrap()).unwrap();
        assert!(saved.files.values().any(|c| c.offset > 0));
        // 群没了、状态已回收:用量照写,游标不再建回来
        std::fs::remove_dir_all(f.cursor.parent().unwrap()).unwrap();
        append(&f.sub, &[line("s2", t0 + 3_000, 4)]);
        assert!(acc.retire(&f.outside, t0 + 4_000));
        assert!(!f.cursor.parent().unwrap().exists());
        let day = 24 * 3600 * 1000;
        let rows = turn_ledger::read_outside(&f.outside, Some(agent), t0 - day, t0 + day);
        assert_eq!(outputs(&rows), 11);
    }

    fn basics(agent: &str, tag: &str, started_at_ms: i64) -> turn_ledger::TurnBasics {
        turn_ledger::TurnBasics {
            agent: agent.into(),
            chat_id: "oc_1".into(),
            tag: tag.into(),
            origin: summon_core::turn_ledger::TurnOrigin::Human,
            schedule: None,
            requester: None,
            message_id: "om_1".into(),
            prompt_head: String::new(),
            started_at_ms,
            end: summon_core::turn_ledger::TurnEnd::Done,
            reattached: false,
        }
    }

    /// 整天回放对账:按真实时间线把脱敏后的转录一行一行「写」出来,轮次的起止与 30 秒一次的扫描照
    /// daemon 的次序驱动累计器,运行记录与轮外用量写进假 home,再由独立脚本 `recon.py` 对账。
    #[test]
    #[ignore = "整天回放对账:需要 SUMMON_USAGE_REPLAY_HOME 指向脱敏后的假 home,及 _FROM / _TO(毫秒)"]
    fn replay_real_transcripts_through_the_accumulator() {
        use std::io::Write as _;
        let env = |k: &str| std::env::var(k).unwrap();
        let home = PathBuf::from(env("SUMMON_USAGE_REPLAY_HOME"));
        let from: i64 = env("SUMMON_USAGE_REPLAY_FROM").parse().unwrap();
        let to: i64 = env("SUMMON_USAGE_REPLAY_TO").parse().unwrap();

        #[derive(Deserialize)]
        struct Window {
            agent: String,
            chat_id: String,
            tag: String,
            started_at_ms: i64,
            ended_at_ms: i64,
        }
        let windows: Vec<Window> = std::fs::read_to_string(home.join("windows.ndjson"))
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();

        // 各 chat 的会话切换:events.jsonl 里 session_id 变了的时刻。
        let mut chats: BTreeMap<(String, String), Vec<(i64, String)>> = BTreeMap::new();
        for ev in glob_paths(&home.join(".summon/state"), 3, "events.jsonl") {
            let tag = ev
                .parent()
                .unwrap()
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned();
            let agent = ev
                .ancestors()
                .nth(3)
                .unwrap()
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned();
            let mut seen: Vec<(i64, String)> = std::fs::read_to_string(&ev)
                .unwrap()
                .lines()
                .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
                .filter_map(|v| Some((v["ts"].as_i64()?, v["session_id"].as_str()?.to_string())))
                .collect();
            seen.sort();
            let mut switches: Vec<(i64, String)> = Vec::new();
            for (ts, sid) in seen {
                if switches.last().is_none_or(|(_, s)| *s != sid) {
                    switches.push((ts, sid));
                }
            }
            chats.insert((agent, tag), switches);
        }
        let chat_id_of: HashMap<(String, String), String> = windows
            .iter()
            .map(|w| ((w.agent.clone(), w.tag.clone()), w.chat_id.clone()))
            .collect();

        // 源文件:每行带时刻;回放目录里按同样的相对路径慢慢写出来。
        struct Source {
            lines: Vec<(i64, String)>,
            written: usize,
            dst: PathBuf,
        }
        let src_root = home.join(".claude/projects");
        let dst_root = home.join("replay-projects");
        let _ = std::fs::remove_dir_all(&dst_root);
        let mut main_of: HashMap<String, PathBuf> = HashMap::new();
        let mut sources: Vec<Source> = Vec::new();
        for path in glob_paths(&src_root, 4, ".jsonl") {
            let rel = path.strip_prefix(&src_root).unwrap().to_path_buf();
            let dst = dst_root.join(&rel);
            if rel.components().count() == 2 {
                let sid = rel.file_stem().unwrap().to_string_lossy().into_owned();
                main_of.insert(sid, dst.clone());
            }
            let lines = std::fs::read_to_string(&path)
                .unwrap()
                .lines()
                .filter_map(|l| {
                    let v: serde_json::Value = serde_json::from_str(l).ok()?;
                    let at = chrono::DateTime::parse_from_rfc3339(v["timestamp"].as_str()?)
                        .ok()?
                        .timestamp_millis();
                    Some((at, l.to_string()))
                })
                .collect();
            std::fs::create_dir_all(dst.parent().unwrap()).unwrap();
            std::fs::write(&dst, "").unwrap();
            sources.push(Source {
                lines,
                written: 0,
                dst,
            });
        }

        // 时间线:(时刻, 种类, chat, 第几轮);同一时刻轮结束 < 轮开始 < 扫描。
        type Event = (i64, u8, (String, String), Option<usize>);
        let mut events: Vec<Event> = Vec::new();
        for (i, w) in windows.iter().enumerate() {
            let key = (w.agent.clone(), w.tag.clone());
            events.push((w.started_at_ms, 1, key.clone(), Some(i)));
            events.push((w.ended_at_ms, 0, key, Some(i)));
        }
        for key in chats.keys() {
            let mut t = from;
            while t <= to {
                events.push((t, 2, key.clone(), None));
                t += SCAN_EVERY.as_millis() as i64;
            }
        }
        events.sort();

        let turns_dir = home.join(".summon/run/turns");
        let outside_dir = home.join(".summon/run/usage");
        let _ = std::fs::remove_dir_all(&turns_dir);
        let _ = std::fs::remove_dir_all(&outside_dir);
        let cursors = home.join("replay-cursors");
        let _ = std::fs::remove_dir_all(&cursors);
        let mut accounts: BTreeMap<(String, String), Account> = BTreeMap::new();
        for (at, kind, key, window) in events {
            for src in &mut sources {
                if src.lines.get(src.written).is_none_or(|(ts, _)| *ts > at) {
                    continue;
                }
                let mut f = std::fs::OpenOptions::new()
                    .append(true)
                    .open(&src.dst)
                    .unwrap();
                while let Some((ts, line)) = src.lines.get(src.written) {
                    if *ts > at {
                        break;
                    }
                    writeln!(f, "{line}").unwrap();
                    src.written += 1;
                }
            }
            let Some((born, sid)) = chats
                .get(&key)
                .and_then(|s| s.iter().rev().find(|(t, _)| *t <= at).cloned())
            else {
                continue;
            };
            let Some(main) = main_of.get(&sid) else {
                continue;
            };
            let chat_id = chat_id_of
                .get(&key)
                .cloned()
                .unwrap_or_else(|| key.1.clone());
            let acc = accounts.entry(key.clone()).or_insert_with(|| {
                Account::load(&key.0, &chat_id, &key.1, cursors.join(&key.0).join(&key.1))
            });
            let pieces = acc.read(&sid, main, Some(born), at);
            match (kind, window) {
                (1, Some(_)) => acc.on_turn_begin(pieces, at, &outside_dir),
                (0, Some(i)) => {
                    let w = &windows[i];
                    let basics = turn_ledger::TurnBasics {
                        agent: w.agent.clone(),
                        chat_id: w.chat_id.clone(),
                        tag: w.tag.clone(),
                        origin: summon_core::turn_ledger::TurnOrigin::Human,
                        schedule: None,
                        requester: None,
                        message_id: format!("replay-{i}"),
                        prompt_head: String::new(),
                        started_at_ms: w.started_at_ms,
                        end: summon_core::turn_ledger::TurnEnd::Done,
                        reattached: false,
                    };
                    turn_ledger::note(&w.agent, &w.tag, |f| f.cli = Some("claude".into()));
                    acc.on_turn_end(pieces, at, &outside_dir, |ended| {
                        turn_ledger::finish_into(&turns_dir, basics, ended);
                    });
                }
                _ => {
                    acc.absorb(pieces, None, at);
                    acc.settle_idle(&outside_dir, at, false);
                }
            }
        }
        for acc in accounts.values_mut() {
            acc.settle_idle(&outside_dir, to, true);
        }
    }

    /// `root` 下深度不超过 `depth` 的、文件名以 `suffix` 结尾的文件。
    fn glob_paths(root: &Path, depth: usize, suffix: &str) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let Ok(entries) = std::fs::read_dir(root) else {
            return out;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() && depth > 0 {
                out.extend(glob_paths(&p, depth - 1, suffix));
            } else if p.to_string_lossy().ends_with(suffix) {
                out.push(p);
            }
        }
        out.sort();
        out
    }
}
