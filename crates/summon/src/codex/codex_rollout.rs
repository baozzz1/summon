//! `$CODEX_HOME/sessions` rollout 轮询 watcher。
//!
//! rollout 是外部 Codex CLI 自动发现不可绕过的兼容层，不是公开 API。因此这里把全部内部
//! schema 依赖收口到 `summon_core::codex_quota`，本模块只负责文件增量、半行与轮询。

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, SystemTime};

use chrono::{DateTime, Days, Local, NaiveDate};
use summon_core::codex_quota::{parse_codex_rollout_line, CodexRateLimitSnapshot};

/// Codex 把 rollout 放在 `sessions/YYYY/MM/DD/` 下,目录名只能证明会话「什么时候开始」,
/// 不能证明最后一次 append 的时间——同一个 rollout 被 resume 续写会让 mtime 远晚于目录日期
/// (本机 614 个 rollout 实测最大偏差 11 天)。所以按目录名剪枝必须在 lookback 之外再退让
/// 这么多天,否则会漏掉长时间续写的会话。代价是剪枝率下降,换的是不改变收集语义。
const DIR_DATE_GRACE_DAYS: u64 = 30;

#[derive(Debug, Clone)]
pub struct CodexRolloutWatcherConfig {
    pub sessions_dir: PathBuf,
    /// 首次/重启回看窗口。运行中已跟踪文件不因越过窗口而丢 cursor。
    pub lookback: Duration,
    pub poll_interval: Duration,
    /// 防止异常目录树一次扫描无限载入。取 mtime 最新的 N 个文件。
    pub max_files_per_poll: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CodexRolloutQuotaObservation {
    pub rollout_path: PathBuf,
    pub line_offset: u64,
    pub snapshot: CodexRateLimitSnapshot,
    /// rollout 行的 `timestamp`(Unix 秒)。
    pub observed_at: Option<i64>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum CodexRolloutWatcherEvent {
    RateLimits(CodexRolloutQuotaObservation),
    Error { path: PathBuf, message: String },
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CodexRolloutPollStats {
    pub files_seen: usize,
    pub files_read: usize,
    pub observations: usize,
    pub errors: usize,
    /// 实际 read_dir 进去的目录数。日期剪枝生效时远小于整棵 sessions 树。
    pub dirs_scanned: usize,
}

#[derive(Debug, Default)]
struct RolloutCursor {
    /// 已从文件读进内存的绝对字节位置；半行也包含在内。
    read_offset: u64,
    /// 尚未遇到 `\n` 的尾巴。
    pending: Vec<u8>,
    /// pending 首字节在文件中的绝对位置。
    pending_offset: u64,
}

pub struct CodexRolloutWatcher {
    config: CodexRolloutWatcherConfig,
    cursors: HashMap<PathBuf, RolloutCursor>,
}

impl CodexRolloutWatcher {
    pub fn new(config: CodexRolloutWatcherConfig) -> Self {
        Self {
            config,
            cursors: HashMap::new(),
        }
    }

    #[cfg(test)]
    pub fn tracked_files(&self) -> usize {
        self.cursors.len()
    }

    /// 扫描一次并把事件投给 callback。单文件损坏不会让其它 rollout 饥饿；错误显式回调。
    pub fn poll_once(
        &mut self,
        mut callback: impl FnMut(CodexRolloutWatcherEvent),
    ) -> CodexRolloutPollStats {
        self.poll_once_at(SystemTime::now(), &mut callback)
    }

    /// 可注入时钟的同义入口，测试/宿主需要确定性 lookback 时使用。
    pub fn poll_once_at(
        &mut self,
        now: SystemTime,
        mut callback: impl FnMut(CodexRolloutWatcherEvent),
    ) -> CodexRolloutPollStats {
        let mut stats = CodexRolloutPollStats::default();
        let candidates = match collect_candidates(
            &self.config.sessions_dir,
            now,
            self.config.lookback,
            self.config.max_files_per_poll,
        ) {
            Ok((files, dirs_scanned)) => {
                stats.dirs_scanned = dirs_scanned;
                files
            }
            Err(error) => {
                stats.errors = 1;
                callback(CodexRolloutWatcherEvent::Error {
                    path: self.config.sessions_dir.clone(),
                    message: error.to_string(),
                });
                return stats;
            }
        };
        stats.files_seen = candidates.len();
        let live: HashSet<PathBuf> = candidates.iter().map(|(_, p)| p.clone()).collect();
        // cursor 数量同 lookback/max_files 一起有界。旧文件日后再次 append 会从头重建；
        // 上层按 (path, line_offset) 去重，不能以保留无界 cursor 换取进程内去重。
        self.cursors.retain(|path, _| live.contains(path));

        for (_, path) in candidates {
            let cursor = self.cursors.entry(path.clone()).or_default();
            match poll_file(&path, cursor, &mut callback) {
                Ok(observations) => {
                    stats.files_read += 1;
                    stats.observations += observations;
                }
                Err(error) => {
                    stats.errors += 1;
                    callback(CodexRolloutWatcherEvent::Error {
                        path,
                        message: error.to_string(),
                    });
                }
            }
        }
        stats
    }

    /// 起独立轮询线程。channel 同时传 observation 与显式错误；receiver 被丢弃后线程自行退出。
    pub fn spawn_channel(
        mut self,
        stop: Arc<AtomicBool>,
    ) -> (Receiver<CodexRolloutWatcherEvent>, thread::JoinHandle<()>) {
        let (tx, rx) = mpsc::channel();
        let poll_interval = self.config.poll_interval;
        let handle = thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                let mut disconnected = false;
                self.poll_once(|event| {
                    if tx.send(event).is_err() {
                        disconnected = true;
                    }
                });
                if disconnected {
                    break;
                }
                sleep_interruptibly(poll_interval, &stop);
            }
        });
        (rx, handle)
    }
}

fn sleep_interruptibly(duration: Duration, stop: &AtomicBool) {
    let quantum = Duration::from_millis(100);
    let mut slept = Duration::ZERO;
    while slept < duration && !stop.load(Ordering::Relaxed) {
        let remaining = duration.saturating_sub(slept);
        let step = remaining.min(quantum);
        thread::sleep(step);
        slept += step;
    }
}

fn poll_file(
    path: &Path,
    cursor: &mut RolloutCursor,
    callback: &mut impl FnMut(CodexRolloutWatcherEvent),
) -> std::io::Result<usize> {
    let mut file = File::open(path)?;
    let len = file.metadata()?.len();
    if len < cursor.read_offset {
        // truncate/替换：旧半行不可信，从头重建。
        *cursor = RolloutCursor::default();
    }
    if len == cursor.read_offset {
        return Ok(0);
    }

    file.seek(SeekFrom::Start(cursor.read_offset))?;
    let mut fresh = Vec::new();
    file.read_to_end(&mut fresh)?;
    if fresh.is_empty() {
        return Ok(0);
    }
    if cursor.pending.is_empty() {
        cursor.pending_offset = cursor.read_offset;
    }
    cursor.read_offset = cursor
        .read_offset
        .saturating_add(u64::try_from(fresh.len()).unwrap_or(u64::MAX));
    cursor.pending.extend_from_slice(&fresh);

    let mut observations = 0;
    while let Some(newline) = cursor.pending.iter().position(|b| *b == b'\n') {
        let line_offset = cursor.pending_offset;
        let mut bytes: Vec<u8> = cursor.pending.drain(..=newline).collect();
        cursor.pending_offset = cursor
            .pending_offset
            .saturating_add(u64::try_from(bytes.len()).unwrap_or(u64::MAX));
        bytes.pop(); // `\n`
        if bytes.last() == Some(&b'\r') {
            bytes.pop();
        }
        if bytes.is_empty() {
            continue;
        }
        let line = String::from_utf8_lossy(&bytes);
        let Some(record) = parse_codex_rollout_line(&line) else {
            continue;
        };
        observations += 1;
        callback(CodexRolloutWatcherEvent::RateLimits(
            CodexRolloutQuotaObservation {
                rollout_path: path.to_path_buf(),
                line_offset,
                snapshot: record.snapshot,
                observed_at: record.observed_at,
            },
        ));
    }
    Ok(observations)
}

/// 返回 (候选文件, 实际进入的目录数)。目录数只用于观测剪枝效果。
fn collect_candidates(
    root: &Path,
    now: SystemTime,
    lookback: Duration,
    max_files: usize,
) -> std::io::Result<(Vec<(SystemTime, PathBuf)>, usize)> {
    if !root.exists() {
        return Ok((Vec::new(), 0));
    }
    let cutoff = prune_cutoff_date(now, lookback);
    let mut files = Vec::new();
    let mut dirs_scanned = 0;
    collect_recursive(
        root,
        0,
        DirDateLevel::Root,
        cutoff,
        &mut files,
        &mut dirs_scanned,
    )?;
    files.retain(|(modified, _)| {
        now.duration_since(*modified)
            .map(|age| age <= lookback)
            .unwrap_or(true) // future mtime/clock rollback：保守纳入
    });
    files.sort_by_key(|(modified, _)| std::cmp::Reverse(*modified));
    files.truncate(max_files);
    Ok((files, dirs_scanned))
}

/// 早于该本地日期的 `YYYY/MM/DD` 子树整棵跳过。返回 `None` 表示算不出边界(时钟离谱 /
/// lookback 大到日期减不动),此时一律不剪枝。
fn prune_cutoff_date(now: SystemTime, lookback: Duration) -> Option<NaiveDate> {
    // 目录名是本地日期,mtime 窗口是绝对时间;多减一天吸收时区与「当天不足 24h」的误差。
    let days = (lookback.as_secs() / 86_400)
        .checked_add(1)?
        .checked_add(DIR_DATE_GRACE_DAYS)?;
    let today: DateTime<Local> = now.into();
    today.date_naive().checked_sub_days(Days::new(days))
}

/// 目录在 `sessions/YYYY/MM/DD` 里的位置。用它把「该不该进这个目录」变成纯函数。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DirDateLevel {
    /// sessions 根,子目录应为 `YYYY`。
    Root,
    /// `YYYY`,子目录应为 `MM`。
    Year(i32),
    /// `YYYY/MM`,子目录应为 `DD`。
    YearMonth(i32, u32),
    /// 结构对不上,或已在日目录之下:放弃日期推断,后代一律放行。
    Opaque,
}

/// 纯函数:父目录层级 + 子目录名 + 剪枝边界 → `None` 表示整棵子树都早于 `cutoff` 可跳过,
/// `Some(level)` 表示进去并带上子目录的层级。**任何解析失败都保守放行**,宁可多扫不可漏文件。
fn enter_dir(parent: DirDateLevel, name: &str, cutoff: NaiveDate) -> Option<DirDateLevel> {
    match parent {
        DirDateLevel::Opaque => Some(DirDateLevel::Opaque),
        DirDateLevel::Root => {
            let Some(year) = parse_fixed_digits(name, 4).and_then(|y| i32::try_from(y).ok()) else {
                return Some(DirDateLevel::Opaque);
            };
            // 比「次年 1/1」省掉「本年最后一天」的换算,判据等价。
            match year
                .checked_add(1)
                .and_then(|y| NaiveDate::from_ymd_opt(y, 1, 1))
            {
                Some(boundary) if boundary <= cutoff => None,
                Some(_) => Some(DirDateLevel::Year(year)),
                None => Some(DirDateLevel::Opaque),
            }
        }
        DirDateLevel::Year(year) => {
            let Some(month @ 1..=12) = parse_fixed_digits(name, 2) else {
                return Some(DirDateLevel::Opaque);
            };
            let boundary = if month == 12 {
                year.checked_add(1)
                    .and_then(|y| NaiveDate::from_ymd_opt(y, 1, 1))
            } else {
                NaiveDate::from_ymd_opt(year, month + 1, 1)
            };
            match boundary {
                Some(boundary) if boundary <= cutoff => None,
                Some(_) => Some(DirDateLevel::YearMonth(year, month)),
                None => Some(DirDateLevel::Opaque),
            }
        }
        DirDateLevel::YearMonth(year, month) => {
            let day = parse_fixed_digits(name, 2)
                .and_then(|day| NaiveDate::from_ymd_opt(year, month, day));
            match day {
                Some(day) if day < cutoff => None,
                _ => Some(DirDateLevel::Opaque),
            }
        }
    }
}

/// 严格定长纯数字。宽松解析(`7` / ` 07` / `07x`)会让剪枝建立在猜测上,一律交给保守分支。
fn parse_fixed_digits(name: &str, len: usize) -> Option<u32> {
    if name.len() != len || !name.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    name.parse().ok()
}

fn collect_recursive(
    dir: &Path,
    depth: usize,
    level: DirDateLevel,
    cutoff: Option<NaiveDate>,
    out: &mut Vec<(SystemTime, PathBuf)>,
    dirs_scanned: &mut usize,
) -> std::io::Result<()> {
    if depth > 6 {
        return Ok(());
    }
    *dirs_scanned += 1;
    for entry in std::fs::read_dir(dir)? {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => continue,
        };
        let path = entry.path();
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(_) => continue,
        };
        if file_type.is_dir() {
            let name = entry.file_name();
            // cutoff 缺失或目录名非 UTF-8 时无从判断,保守进入。
            let child = match (cutoff, name.to_str()) {
                (Some(cutoff), Some(name)) => match enter_dir(level, name, cutoff) {
                    Some(child) => child,
                    None => continue,
                },
                _ => DirDateLevel::Opaque,
            };
            // 不跟 symlink，避免逃出 CODEX_HOME 或目录环。
            collect_recursive(&path, depth + 1, child, cutoff, out, dirs_scanned)?;
            continue;
        }
        if !file_type.is_file() || !is_rollout(&path) {
            continue;
        }
        if let Ok(modified) = entry.metadata().and_then(|meta| meta.modified()) {
            out.push((modified, path));
        }
    }
    Ok(())
}

fn is_rollout(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with("rollout-") && name.ends_with(".jsonl"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    const META: &str = r#"{"timestamp":"t","type":"session_meta","payload":{"id":"thread-1","cwd":"/repo","source":"cli"}}"#;
    const RATE: &str = r#"{"timestamp":"2026-07-12T00:00:01Z","type":"event_msg","payload":{"type":"token_count","rate_limits":{"limit_id":"codex","primary":{"used_percent":100.0,"window_minutes":300,"resets_at":200},"secondary":null,"rate_limit_reached_type":null}}}"#;

    /// 目录名必须跟「今天」走:剪枝按目录日期算窗口,写死日期的用例会在某天之后突然被剪掉。
    fn rollout(tmp: &tempfile::TempDir) -> PathBuf {
        let today = Local::now().date_naive();
        let dir = tmp.path().join(today.format("%Y/%m/%d").to_string());
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("rollout-2026-07-12T00-00-00-thread.jsonl")
    }

    fn config(tmp: &tempfile::TempDir) -> CodexRolloutWatcherConfig {
        CodexRolloutWatcherConfig {
            sessions_dir: tmp.path().to_path_buf(),
            lookback: Duration::from_secs(3600),
            poll_interval: Duration::from_millis(10),
            max_files_per_poll: 16,
        }
    }

    #[test]
    fn keeps_partial_line_until_newline_arrives() {
        let tmp = tempfile::tempdir().unwrap();
        let path = rollout(&tmp);
        let split = RATE.len() / 2;
        std::fs::write(&path, format!("{META}\n{}", &RATE[..split])).unwrap();
        let mut watcher = CodexRolloutWatcher::new(config(&tmp));
        let mut events = Vec::new();
        let first = watcher.poll_once(|event| events.push(event));
        assert_eq!(first.observations, 0);
        assert!(events.is_empty());

        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        writeln!(file, "{}", &RATE[split..]).unwrap();
        let second = watcher.poll_once(|event| events.push(event));
        assert_eq!(second.observations, 1);
        let CodexRolloutWatcherEvent::RateLimits(observation) = &events[0] else {
            panic!("expected rate limits")
        };
        assert_eq!(observation.snapshot.limit_id, "codex");
        assert!(observation.line_offset > 0);
    }

    #[test]
    fn rate_limits_without_usage_error_emit_an_independent_quota_event() {
        let tmp = tempfile::tempdir().unwrap();
        let path = rollout(&tmp);
        std::fs::write(&path, format!("{META}\n{RATE}\n")).unwrap();
        let mut watcher = CodexRolloutWatcher::new(config(&tmp));
        let mut events = Vec::new();

        let stats = watcher.poll_once(|event| events.push(event));

        assert_eq!(stats.observations, 1);
        let [CodexRolloutWatcherEvent::RateLimits(observation)] = events.as_slice() else {
            panic!("expected one independent rate-limits event: {events:?}")
        };
        assert_eq!(observation.snapshot.limit_id, "codex");
        assert_eq!(observation.observed_at, Some(1783814401));
        assert_eq!(
            observation.line_offset,
            u64::try_from(META.len() + 1).unwrap()
        );
    }

    #[test]
    fn truncation_resets_offset_and_rereads_from_start() {
        let tmp = tempfile::tempdir().unwrap();
        let path = rollout(&tmp);
        std::fs::write(&path, format!("{META}\n{RATE}\n")).unwrap();
        let mut watcher = CodexRolloutWatcher::new(config(&tmp));
        let mut events = Vec::new();
        watcher.poll_once(|event| events.push(event));
        assert_eq!(events.len(), 1);
        assert!(matches!(
            &events[0],
            CodexRolloutWatcherEvent::RateLimits(o) if o.snapshot.limit_id == "codex"
        ));

        // 截断并让 watcher 观察到短文件，触发 offset 重建。
        std::fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&path)
            .unwrap();
        watcher.poll_once(|_| {});
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        writeln!(file, "{META}\n{RATE}").unwrap();
        events.clear();
        watcher.poll_once(|event| events.push(event));
        assert_eq!(events.len(), 1);
        assert!(matches!(
            &events[0],
            CodexRolloutWatcherEvent::RateLimits(o) if o.snapshot.limit_id == "codex"
        ));
    }

    #[test]
    fn finite_lookback_skips_old_rollout() {
        let tmp = tempfile::tempdir().unwrap();
        let path = rollout(&tmp);
        std::fs::write(&path, format!("{META}\n{RATE}\n")).unwrap();
        let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
        let mut cfg = config(&tmp);
        cfg.lookback = Duration::from_secs(60);
        let mut watcher = CodexRolloutWatcher::new(cfg);
        let stats = watcher.poll_once_at(modified + Duration::from_secs(61), |_| {});
        assert_eq!(stats.files_seen, 0);
        assert_eq!(watcher.tracked_files(), 0);
    }

    fn date(year: i32, month: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(year, month, day).unwrap()
    }

    #[test]
    fn prunes_year_month_and_day_dirs_before_cutoff() {
        let cutoff = date(2026, 7, 20);
        // 整年在窗口外 → 连年目录都不进。
        assert_eq!(enter_dir(DirDateLevel::Root, "2025", cutoff), None);
        assert_eq!(
            enter_dir(DirDateLevel::Root, "2026", cutoff),
            Some(DirDateLevel::Year(2026))
        );
        assert_eq!(enter_dir(DirDateLevel::Year(2026), "05", cutoff), None);
        // 6 月最后一天 6/30 仍早于 cutoff。
        assert_eq!(enter_dir(DirDateLevel::Year(2026), "06", cutoff), None);
        assert_eq!(
            enter_dir(DirDateLevel::Year(2026), "07", cutoff),
            Some(DirDateLevel::YearMonth(2026, 7))
        );
        assert_eq!(
            enter_dir(DirDateLevel::YearMonth(2026, 7), "19", cutoff),
            None
        );
        // 边界日等于 cutoff 要保留。
        assert_eq!(
            enter_dir(DirDateLevel::YearMonth(2026, 7), "20", cutoff),
            Some(DirDateLevel::Opaque)
        );
        assert_eq!(
            enter_dir(DirDateLevel::YearMonth(2026, 7), "31", cutoff),
            Some(DirDateLevel::Opaque)
        );
    }

    #[test]
    fn keeps_dirs_whose_names_do_not_parse_as_dates() {
        let cutoff = date(2026, 7, 20);
        for (level, name) in [
            (DirDateLevel::Root, "archive"),
            (DirDateLevel::Root, "26"),
            (DirDateLevel::Root, "2o26"),
            (DirDateLevel::Year(2026), "7"),
            (DirDateLevel::Year(2026), "00"),
            (DirDateLevel::Year(2026), "13"),
            (DirDateLevel::YearMonth(2026, 7), "00"),
            (DirDateLevel::YearMonth(2026, 2), "31"),
            (DirDateLevel::Opaque, "2025"),
        ] {
            assert_eq!(
                enter_dir(level, name, cutoff),
                Some(DirDateLevel::Opaque),
                "{level:?} + {name} 应保守放行"
            );
        }
    }

    /// 用 POSIX `touch` 把 mtime 对齐目录日期。std 无法设置 mtime,而本任务不允许引新依赖;
    /// 本仓只支持 unix,借外部命令可接受。
    fn set_mtime(path: &Path, day: NaiveDate) {
        let stamp = day.format("%Y%m%d1200").to_string();
        let status = std::process::Command::new("touch")
            .arg("-t")
            .arg(&stamp)
            .arg(path)
            .status()
            .unwrap();
        assert!(status.success(), "touch -t {stamp} 失败");
    }

    fn write_rollout(root: &Path, rel_dir: &str, name: &str, mtime: Option<NaiveDate>) -> PathBuf {
        let dir = root.join(rel_dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, format!("{META}\n")).unwrap();
        if let Some(day) = mtime {
            set_mtime(&path, day);
        }
        path
    }

    /// 剪枝前的参照实现:无条件走完整棵树。
    fn walk_unpruned(dir: &Path, out: &mut Vec<(SystemTime, PathBuf)>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if entry.file_type().unwrap().is_dir() {
                walk_unpruned(&path, out);
            } else if is_rollout(&path) {
                out.push((entry.metadata().unwrap().modified().unwrap(), path));
            }
        }
    }

    struct PruneFixture {
        tmp: tempfile::TempDir,
        entered_dirs: usize,
    }

    /// 构造一棵含「窗内 / 超 lookback 但在 grace 内 / 远超 grace / 非数字」四类目录的树,
    /// 并算出剪枝后应当进入的目录数。
    fn prune_fixture(lookback: Duration) -> PruneFixture {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let today = Local::now().date_naive();
        let stale = today
            .checked_sub_days(chrono::Days::new(lookback.as_secs() / 86_400 + 3))
            .unwrap();

        let mut entered: HashSet<PathBuf> = HashSet::new();
        entered.insert(root.to_path_buf());
        for day in [today, stale] {
            let rel = day.format("%Y/%m/%d").to_string();
            write_rollout(root, &rel, "rollout-x.jsonl", Some(day));
            entered.insert(root.join(day.format("%Y").to_string()));
            entered.insert(root.join(day.format("%Y/%m").to_string()));
            entered.insert(root.join(rel));
        }
        // 远超 grace:整个 2000 年目录都不该被 read_dir。
        write_rollout(
            root,
            "2000/01/01",
            "rollout-ancient.jsonl",
            Some(date(2000, 1, 1)),
        );
        // 目录名解析不出日期:保守放行,里面的新文件必须还能收到。
        write_rollout(root, "archive/misc", "rollout-weird.jsonl", None);
        entered.insert(root.join("archive"));
        entered.insert(root.join("archive/misc"));

        PruneFixture {
            entered_dirs: entered.len(),
            tmp,
        }
    }

    #[test]
    fn date_prune_collects_same_files_as_full_walk() {
        let lookback = Duration::from_secs(3 * 24 * 60 * 60);
        let fixture = prune_fixture(lookback);
        let root = fixture.tmp.path();
        let now = SystemTime::now();

        let mut reference = Vec::new();
        walk_unpruned(root, &mut reference);
        let expected: HashSet<PathBuf> = reference
            .into_iter()
            .filter(|(modified, _)| {
                now.duration_since(*modified)
                    .map(|age| age <= lookback)
                    .unwrap_or(true)
            })
            .map(|(_, path)| path)
            .collect();

        let (candidates, _) = collect_candidates(root, now, lookback, 64).unwrap();
        let actual: HashSet<PathBuf> = candidates.into_iter().map(|(_, path)| path).collect();
        assert_eq!(actual, expected);
        assert_eq!(actual.len(), 2, "今天的 rollout + 非数字目录里的新 rollout");
    }

    #[test]
    fn date_prune_skips_reading_out_of_window_dirs() {
        let lookback = Duration::from_secs(3 * 24 * 60 * 60);
        let fixture = prune_fixture(lookback);
        let (_, dirs_scanned) =
            collect_candidates(fixture.tmp.path(), SystemTime::now(), lookback, 64).unwrap();
        assert_eq!(dirs_scanned, fixture.entered_dirs);
    }

    #[test]
    fn cutoff_falls_back_to_no_pruning_on_absurd_lookback() {
        // lookback 大到日期减不动时必须退回「不剪枝」,不能反过来把整棵树剪光。
        assert_eq!(prune_cutoff_date(SystemTime::now(), Duration::MAX), None);
    }

    #[test]
    fn channel_thread_exits_when_receiver_is_dropped() {
        let tmp = tempfile::tempdir().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let watcher = CodexRolloutWatcher::new(config(&tmp));
        let (rx, handle) = watcher.spawn_channel(stop.clone());
        drop(rx);
        // 空目录不会 send，因此显式 stop；join 验证 100ms 分片停机而非睡完整 poll interval。
        stop.store(true, Ordering::Relaxed);
        handle.join().unwrap();
    }
}
