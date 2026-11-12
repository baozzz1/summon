//! 洞察:人在 agent 环路里的等待 —— 回复落地到同一会话里同一个人发下一条消息隔了多久、长间隔
//! 期间人是不是在别的会话里、下一条消息是哪一类、每天发出多少消息,以及按规则得出的结论。
//!
//! 纯计算,只读运行记录([`TurnRecord`])的结构化字段。下一条消息的分类只看触发消息开头
//! (`prompt_head`),不读回复正文;结果里只有计数、时长与分类,不带任何消息文字,也不带发消息
//! 的人。结论只给 key 与数值参数,句子由界面按语言渲染。
//!
//! 口径:
//! - 一条「间隔」是同一 (agent, tag) 里按开始时刻相邻的两轮 prev → next:两轮都是真人发起、
//!   发起人相同,prev 正常完成(`Done`);gap = next 开始 − prev 结束。不到 [`QUEUED_MS`] 的是轮
//!   还没结束就发出的排队消息,单独计数,不算间隔。
//! - 分布一律用当日口径:prev 结束在本地 [[`DAY_START_HOUR`], [`DAY_END_HOUR`]) 点之间,且 next
//!   在同一个本地自然日里开始 —— 过夜的间隔说明的是作息,不是等待。
//! - 间隔按 prev 的结束时刻归入时间段;轮数、每日输入量按开始时刻归入。
//! - 「别处」指其它 (agent, tag),不按发起人过滤:同一个人在不同 agent 下的 open_id 不同,对不上。

use std::collections::{BTreeMap, BTreeSet, HashMap};

use chrono::{Datelike, NaiveDate, NaiveDateTime, TimeZone, Timelike};
use serde::Serialize;

use crate::turn_ledger::{TurnEnd, TurnOrigin, TurnRecord};

/// 当日口径的起止小时(本地时间,左闭右开)。
pub const DAY_START_HOUR: u32 = 9;
pub const DAY_END_HOUR: u32 = 22;
/// 下一条消息在回复落地后不到这么久(毫秒)就开始了:轮还没结束时已经发出、排着队,不算间隔。
pub const QUEUED_MS: i64 = 2_000;
/// 长间隔的下限(秒),也是「15 分钟以上」的起点。
pub const LONG_GAP_SECS: f64 = 900.0;
/// 间隔里所有会话都没有人发消息的最长一段至少占间隔的这么多,算「这段时间没在用 agent」。
pub const SILENT_SHARE: f64 = 0.8;
/// 六档分桶的上界(秒),最后一档没有上界。
const BUCKET_EDGES: [f64; 5] = [60.0, 300.0, 900.0, 3_600.0, 14_400.0];
/// 长间隔三档的下界(秒)。
const LONG_EDGES: [f64; 3] = [LONG_GAP_SECS, 3_600.0, 14_400.0];
/// 催问进展至多这么多个字;放行与短指令至多这么多个字。
const POLL_MAX_CHARS: usize = 24;
const SHORT_MAX_CHARS: usize = 12;

/// 当日口径的间隔少于这么多条时样本不足,只给这一条结论。
pub const MIN_SAMPLE: u32 = 30;
/// 15 分钟以上的间隔占总时长至少这么多:时间集中在少数长间隔上。
pub const TAIL_SECS_SHARE: f64 = 0.7;
/// 长间隔至少这么多条,且其中期间别处有消息的至少占这么多:长间隔多半是在别的会话里忙。
pub const BUSY_MIN_LONG_GAPS: u32 = 20;
pub const BUSY_SHARE: f64 = 0.6;
/// 放行不到这么多、新指令与短指令合计至少这么多:下一条消息多半是新的指令,而不是放行。
pub const APPROVE_MAX_SHARE: f64 = 0.1;
pub const INSTRUCTION_MIN_SHARE: f64 = 0.8;
/// 催问进展至少这么多条。
pub const MIN_POLLS: u32 = 5;
/// 有消息的工作日至少这么多天,才给每日输入量的结论。
pub const MIN_WORKDAYS: u32 = 3;
/// 间隔合计至少是 agent 一轮耗时合计的这么多倍。
pub const HUMAN_AGENT_RATIO: f64 = 1.5;
/// 上一时段的中位数变化至少这么多(相对上一时段)。
pub const TREND_CHANGE: f64 = 0.25;

/// 一段时间:`[since_ms, until_ms)`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Window {
    pub since_ms: i64,
    pub until_ms: i64,
}

impl Window {
    fn contains(&self, ms: i64) -> bool {
        ms >= self.since_ms && ms < self.until_ms
    }
}

/// 汇总什么。
pub struct Query<'a, Tz: TimeZone> {
    /// 本地时间按这个时区算(当日口径、自然日、星期)。
    pub tz: &'a Tz,
    pub window: Window,
    /// 用来对比的上一时段;没有就不对比。
    pub previous: Option<Window>,
    /// 只看这个 agent 的会话;「别处」仍然看全部 agent。
    pub agent: Option<&'a str>,
}

/// 一组时长(秒)的分位数:线性插值(见 `pct`);没有样本时全是 0。
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize)]
pub struct Quantiles {
    pub n: u32,
    pub p50_secs: f64,
    pub p75_secs: f64,
    pub p90_secs: f64,
    pub total_secs: f64,
}

/// 时长分档。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Span {
    #[serde(rename = "lt_1m")]
    Lt1m,
    #[serde(rename = "m1_5")]
    M1To5,
    #[serde(rename = "m5_15")]
    M5To15,
    #[serde(rename = "m15_60")]
    M15To60,
    #[serde(rename = "h1_4")]
    H1To4,
    #[serde(rename = "gte_4h")]
    Gte4h,
}

const SPANS: [Span; 6] = [
    Span::Lt1m,
    Span::M1To5,
    Span::M5To15,
    Span::M15To60,
    Span::H1To4,
    Span::Gte4h,
];
const LONG_SPANS: [Span; 3] = [Span::M15To60, Span::H1To4, Span::Gte4h];

/// 一档间隔:条数与时长合计。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Bucket {
    pub key: Span,
    pub count: u32,
    pub secs: f64,
}

/// 一档长间隔期间的别处活动。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Elsewhere {
    pub key: Span,
    pub count: u32,
    pub secs: f64,
    /// 期间在其它会话里有真人发起的轮的条数。
    pub active_elsewhere: u32,
    /// 期间其它会话里真人发起的轮数的中位数。
    pub elsewhere_messages_p50: f64,
}

/// 下一条消息的类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NextKind {
    /// 较长的新指令。
    Instruction,
    /// 短指令。
    Short,
    /// 放行:继续 / 可以 / 选一个选项。
    Approve,
    /// 催问进展。
    Poll,
}

const NEXT_KINDS: [NextKind; 4] = [
    NextKind::Instruction,
    NextKind::Short,
    NextKind::Approve,
    NextKind::Poll,
];

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct NextRow {
    pub key: NextKind,
    pub count: u32,
    pub p50_secs: f64,
    pub p90_secs: f64,
}

/// 一个本地自然日的输入量。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Day {
    /// `YYYY-MM-DD`。
    pub date: String,
    /// ISO 星期:1 = 周一 … 7 = 周日。
    pub weekday: u32,
    /// 真人发起的轮数。
    pub messages: u32,
    /// 这些轮涉及的 (agent, tag) 数。
    pub chats: u32,
}

/// 有消息的工作日(周一到周五)的输入量。
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize)]
pub struct Workdays {
    pub days: u32,
    pub messages_p50: f64,
    pub messages_p90: f64,
    pub chats_p50: f64,
    pub chats_p90: f64,
}

/// 紧邻的上一时段的间隔。
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize)]
pub struct Previous {
    pub gaps: Quantiles,
    /// 15 分钟以上的间隔占条数、占时长的比例(0–1)。
    pub over_15m_count_share: f64,
    pub over_15m_secs_share: f64,
}

/// 结论的种类,规则见 [`findings`]。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FindingKey {
    SampleSmall,
    TailConcentration,
    BusyElsewhere,
    MostlyInstructions,
    Polls,
    Load,
    HumanVsAgent,
    Trend,
}

/// 结论里的一个参数:比例为 0–1,时长为秒。
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum Param {
    Number(f64),
    Text(&'static str),
}

impl From<f64> for Param {
    fn from(v: f64) -> Self {
        Param::Number(v)
    }
}

impl From<u32> for Param {
    fn from(v: u32) -> Self {
        Param::Number(f64::from(v))
    }
}

impl From<&'static str> for Param {
    fn from(v: &'static str) -> Self {
        Param::Text(v)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Finding {
    pub key: FindingKey,
    pub params: BTreeMap<&'static str, Param>,
}

/// 一段时间的洞察。
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Insights {
    /// 时间段里开始的轮(任何来源)。
    pub turns: u32,
    /// 其中真人发起的。
    pub human_turns: u32,
    /// 真人发起的轮涉及的 agent 数与 (agent, tag) 数。
    pub agents: u32,
    pub chats: u32,
    pub day_start_hour: u32,
    pub day_end_hour: u32,
    /// 当日口径的间隔。
    pub gaps: Quantiles,
    /// 不限当日口径的间隔条数(含过夜)。
    pub gaps_including_overnight: u32,
    /// 排队消息条数(不算间隔)。
    pub queued: u32,
    /// 六档,固定顺序。
    pub buckets: Vec<Bucket>,
    /// agent 一轮耗时:真人发起、正常完成的轮。
    pub agent_turns: Quantiles,
    /// 长间隔三档,固定顺序。
    pub elsewhere: Vec<Elsewhere>,
    pub long_gaps: u32,
    /// 长间隔里所有会话都没有人发消息的最长一段至少占 [`SILENT_SHARE`] 的条数。
    pub long_gaps_silent: u32,
    /// 四类,固定顺序。
    pub next_messages: Vec<NextRow>,
    /// 时间段里每个本地自然日,早的在前。
    pub days: Vec<Day>,
    pub workdays: Workdays,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous: Option<Previous>,
    pub findings: Vec<Finding>,
}

/// 线性插值分位数:下标 k = (n−1)·p,取 `sorted[⌊k⌋]` 与下一个样本之间按 k 的小数部分插值;
/// `sorted` 须已升序。
fn pct(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let k = (sorted.len() - 1) as f64 * p;
    let lo = k as usize;
    let hi = (lo + 1).min(sorted.len() - 1);
    sorted[lo] + (sorted[hi] - sorted[lo]) * (k - lo as f64)
}

fn sorted(mut xs: Vec<f64>) -> Vec<f64> {
    xs.sort_by(f64::total_cmp);
    xs
}

fn count(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

fn quantiles(xs: Vec<f64>) -> Quantiles {
    let xs = sorted(xs);
    Quantiles {
        n: count(xs.len()),
        p50_secs: pct(&xs, 0.5),
        p75_secs: pct(&xs, 0.75),
        p90_secs: pct(&xs, 0.9),
        total_secs: total(xs.iter().copied()),
    }
}

/// 合计。`f64` 的 `sum` 从 -0.0 起算,空集合得 -0.0、序列化成 `-0.0`;这里从 0.0 起算。
fn total(xs: impl IntoIterator<Item = f64>) -> f64 {
    let mut out = 0.0;
    for x in xs {
        out += x;
    }
    out
}

fn share(part: f64, whole: f64) -> f64 {
    if whole > 0.0 {
        part / whole
    } else {
        0.0
    }
}

fn local<Tz: TimeZone>(tz: &Tz, ms: i64) -> Option<NaiveDateTime> {
    tz.timestamp_millis_opt(ms)
        .single()
        .map(|t| t.naive_local())
}

/// 一个会话:(agent, tag)。
type ChatKey<'a> = (&'a str, &'a str);

/// 一对相邻的轮。
struct Pair<'a> {
    prev: &'a TurnRecord,
    next: &'a TurnRecord,
}

impl Pair<'_> {
    fn gap_ms(&self) -> i64 {
        self.next.started_at_ms - self.prev.ended_at_ms
    }

    fn secs(&self) -> f64 {
        self.gap_ms() as f64 / 1000.0
    }

    fn queued(&self) -> bool {
        self.gap_ms() < QUEUED_MS
    }

    /// 当日口径:prev 结束在本地 [9, 22) 点,next 与它同一个本地自然日开始。
    fn same_day<Tz: TimeZone>(&self, tz: &Tz) -> bool {
        let (Some(end), Some(next)) = (
            local(tz, self.prev.ended_at_ms),
            local(tz, self.next.started_at_ms),
        ) else {
            return false;
        };
        (DAY_START_HOUR..DAY_END_HOUR).contains(&end.hour()) && end.date() == next.date()
    }
}

/// 同一 (agent, tag) 里按开始时刻相邻、成对规则成立的全部 prev → next(含排队消息)。
fn pairs<'a>(records: &'a [TurnRecord], agent: Option<&str>) -> Vec<Pair<'a>> {
    let mut chats: HashMap<ChatKey<'_>, Vec<&TurnRecord>> = HashMap::new();
    for r in records
        .iter()
        .filter(|r| agent.is_none_or(|a| a == r.agent))
    {
        chats
            .entry((r.agent.as_str(), r.tag.as_str()))
            .or_default()
            .push(r);
    }
    let mut out = Vec::new();
    for rs in chats.values_mut() {
        rs.sort_by_key(|r| (r.started_at_ms, r.ended_at_ms));
        for w in rs.windows(2) {
            let (prev, next) = (w[0], w[1]);
            if prev.origin == TurnOrigin::Human
                && prev.end == TurnEnd::Done
                && next.origin == TurnOrigin::Human
                && prev.requester.is_some()
                && prev.requester == next.requester
            {
                out.push(Pair { prev, next });
            }
        }
    }
    out
}

/// 窗口里(按 prev 结束时刻)不是排队消息、符合当日口径的间隔。
fn day_gaps<'p, 'a, Tz: TimeZone>(
    pairs: &'p [Pair<'a>],
    window: Window,
    tz: &Tz,
) -> Vec<&'p Pair<'a>> {
    pairs
        .iter()
        .filter(|p| window.contains(p.prev.ended_at_ms) && !p.queued() && p.same_day(tz))
        .collect()
}

fn span_of(secs: f64) -> Span {
    let index = BUCKET_EDGES
        .iter()
        .take_while(|edge| secs >= **edge)
        .count();
    SPANS[index]
}

fn previous_of(gaps: &[&Pair<'_>]) -> Previous {
    let secs: Vec<f64> = gaps.iter().map(|p| p.secs()).collect();
    let long: Vec<f64> = secs
        .iter()
        .copied()
        .filter(|s| *s >= LONG_GAP_SECS)
        .collect();
    let q = quantiles(secs);
    Previous {
        gaps: q,
        over_15m_count_share: share(long.len() as f64, f64::from(q.n)),
        over_15m_secs_share: share(total(long), q.total_secs),
    }
}

/// 真人发起的轮的开始时刻与所在会话,按时刻排好:看长间隔期间别处有没有人发消息。
struct HumanStarts<'a> {
    starts: Vec<(i64, ChatKey<'a>)>,
}

impl<'a> HumanStarts<'a> {
    fn new(records: &'a [TurnRecord]) -> Self {
        let mut starts: Vec<(i64, ChatKey<'_>)> = records
            .iter()
            .filter(|r| r.origin == TurnOrigin::Human)
            .map(|r| (r.started_at_ms, (r.agent.as_str(), r.tag.as_str())))
            .collect();
        starts.sort_unstable();
        Self { starts }
    }

    /// `[from, to]`(含两端)里开始的。
    fn between(&self, from: i64, to: i64) -> &[(i64, ChatKey<'a>)] {
        let lo = self.starts.partition_point(|(t, _)| *t < from);
        let hi = self.starts.partition_point(|(t, _)| *t <= to);
        &self.starts[lo..hi.max(lo)]
    }
}

/// 汇总一段时间。`records` 要覆盖窗口与上一时段,并往前多带一些(成对要看到相邻的那一轮);
/// 往后要带到此刻(过夜的间隔要看到下一条消息)。
pub fn summarize<Tz: TimeZone>(records: &[TurnRecord], q: &Query<'_, Tz>) -> Insights {
    let tz = q.tz;
    let window = q.window;
    let mine = |r: &&TurnRecord| q.agent.is_none_or(|a| a == r.agent);
    let in_window: Vec<&TurnRecord> = records
        .iter()
        .filter(mine)
        .filter(|r| window.contains(r.started_at_ms))
        .collect();
    let human: Vec<&TurnRecord> = in_window
        .iter()
        .copied()
        .filter(|r| r.origin == TurnOrigin::Human)
        .collect();

    let all_pairs = pairs(records, q.agent);
    let anchored: Vec<&Pair<'_>> = all_pairs
        .iter()
        .filter(|p| window.contains(p.prev.ended_at_ms))
        .collect();
    let queued = anchored.iter().filter(|p| p.queued()).count();
    let gaps = day_gaps(&all_pairs, window, tz);
    let secs: Vec<f64> = gaps.iter().map(|p| p.secs()).collect();

    let mut buckets: Vec<Bucket> = SPANS
        .iter()
        .map(|key| Bucket {
            key: *key,
            count: 0,
            secs: 0.0,
        })
        .collect();
    for s in &secs {
        let slot = &mut buckets[SPANS.iter().position(|k| *k == span_of(*s)).unwrap_or(0)];
        slot.count += 1;
        slot.secs += s;
    }

    // 长间隔期间的别处活动:看全部 agent 的真人消息,不只是所选 agent 的。
    let starts = HumanStarts::new(records);
    let mut tiers: Vec<(Vec<f64>, Vec<f64>, u32)> = vec![(Vec::new(), Vec::new(), 0); 3];
    let mut long_gaps = 0;
    let mut silent = 0;
    for p in gaps.iter().filter(|p| p.secs() >= LONG_GAP_SECS) {
        let (from, to) = (p.prev.ended_at_ms, p.next.started_at_ms);
        let me = (p.prev.agent.as_str(), p.prev.tag.as_str());
        let during = starts.between(from, to);
        let elsewhere = during.iter().filter(|(_, k)| *k != me).count();
        let tier = LONG_EDGES.iter().take_while(|e| p.secs() >= **e).count() - 1;
        let slot = &mut tiers[tier];
        slot.0.push(p.secs());
        slot.1.push(elsewhere as f64);
        slot.2 += u32::from(elsewhere > 0);
        long_gaps += 1;
        let points: Vec<i64> = std::iter::once(from)
            .chain(during.iter().map(|(t, _)| *t))
            .chain(std::iter::once(to))
            .collect();
        let away = points.windows(2).map(|w| w[1] - w[0]).max().unwrap_or(0);
        silent += u32::from(away as f64 >= SILENT_SHARE * p.gap_ms() as f64);
    }
    let elsewhere = LONG_SPANS
        .iter()
        .zip(tiers)
        .map(|(key, (secs, counts, active))| Elsewhere {
            key: *key,
            count: count(secs.len()),
            secs: total(secs),
            active_elsewhere: active,
            elsewhere_messages_p50: pct(&sorted(counts), 0.5),
        })
        .collect();

    let next_messages = NEXT_KINDS
        .iter()
        .map(|kind| {
            let xs = sorted(
                gaps.iter()
                    .filter(|p| classify_next(&p.next.prompt_head) == *kind)
                    .map(|p| p.secs())
                    .collect(),
            );
            NextRow {
                key: *kind,
                count: count(xs.len()),
                p50_secs: pct(&xs, 0.5),
                p90_secs: pct(&xs, 0.9),
            }
        })
        .collect();

    let agent_turns = quantiles(
        human
            .iter()
            .filter(|r| r.end == TurnEnd::Done)
            .map(|r| (r.ended_at_ms - r.started_at_ms).max(0) as f64 / 1000.0)
            .collect(),
    );

    let days = days(tz, window, &human);
    let workdays = workdays(&days);

    let previous = q
        .previous
        .map(|w| previous_of(&day_gaps(&all_pairs, w, tz)));

    let mut out = Insights {
        turns: count(in_window.len()),
        human_turns: count(human.len()),
        agents: count(
            human
                .iter()
                .map(|r| r.agent.as_str())
                .collect::<BTreeSet<_>>()
                .len(),
        ),
        chats: count(
            human
                .iter()
                .map(|r| (r.agent.as_str(), r.tag.as_str()))
                .collect::<BTreeSet<_>>()
                .len(),
        ),
        day_start_hour: DAY_START_HOUR,
        day_end_hour: DAY_END_HOUR,
        gaps: quantiles(secs),
        gaps_including_overnight: count(anchored.len() - queued),
        queued: count(queued),
        buckets,
        agent_turns,
        elsewhere,
        long_gaps,
        long_gaps_silent: silent,
        next_messages,
        days,
        workdays,
        previous,
        findings: Vec::new(),
    };
    out.findings = findings(&out);
    out
}

/// 窗口里每个本地自然日(早的在前)真人发起的轮数与涉及的会话数。
fn days<Tz: TimeZone>(tz: &Tz, window: Window, human: &[&TurnRecord]) -> Vec<Day> {
    let (Some(first), Some(last)) = (
        local(tz, window.since_ms),
        local(tz, window.until_ms.saturating_sub(1)),
    ) else {
        return Vec::new();
    };
    let mut per_day: BTreeMap<NaiveDate, (u32, BTreeSet<ChatKey<'_>>)> = BTreeMap::new();
    for r in human {
        if let Some(at) = local(tz, r.started_at_ms) {
            let slot = per_day.entry(at.date()).or_default();
            slot.0 += 1;
            slot.1.insert((r.agent.as_str(), r.tag.as_str()));
        }
    }
    first
        .date()
        .iter_days()
        .take_while(|d| *d <= last.date())
        .map(|d| {
            let (messages, chats) = per_day
                .get(&d)
                .map(|(m, c)| (*m, count(c.len())))
                .unwrap_or_default();
            Day {
                date: d.to_string(),
                weekday: d.weekday().number_from_monday(),
                messages,
                chats,
            }
        })
        .collect()
}

/// 有消息的工作日:没有消息的工作日(休假)不拉低中位数。
fn workdays(days: &[Day]) -> Workdays {
    let work: Vec<&Day> = days
        .iter()
        .filter(|d| d.weekday <= 5 && d.messages > 0)
        .collect();
    let messages = sorted(work.iter().map(|d| f64::from(d.messages)).collect());
    let chats = sorted(work.iter().map(|d| f64::from(d.chats)).collect());
    Workdays {
        days: count(work.len()),
        messages_p50: pct(&messages, 0.5),
        messages_p90: pct(&messages, 0.9),
        chats_p50: pct(&chats, 0.5),
        chats_p90: pct(&chats, 0.9),
    }
}

/// 按规则给结论:每条规则的阈值见同名常量。样本不足时只给 `sample_small`。
pub fn findings(s: &Insights) -> Vec<Finding> {
    let finding = |key, params: Vec<(&'static str, Param)>| Finding {
        key,
        params: params.into_iter().collect(),
    };
    if s.gaps.n < MIN_SAMPLE {
        return vec![finding(
            FindingKey::SampleSmall,
            vec![("n", s.gaps.n.into())],
        )];
    }
    let mut out = Vec::new();
    let n = f64::from(s.gaps.n);
    let long = s.buckets.iter().filter(|b| LONG_SPANS.contains(&b.key));
    let (long_count, long_secs) = long.fold((0u32, 0.0), |(c, t), b| (c + b.count, t + b.secs));
    let secs_share = share(long_secs, s.gaps.total_secs);
    if secs_share >= TAIL_SECS_SHARE {
        out.push(finding(
            FindingKey::TailConcentration,
            vec![
                ("count_share", share(f64::from(long_count), n).into()),
                ("secs_share", secs_share.into()),
            ],
        ));
    }
    let active: u32 = s.elsewhere.iter().map(|e| e.active_elsewhere).sum();
    let busy = share(f64::from(active), f64::from(s.long_gaps));
    if s.long_gaps >= BUSY_MIN_LONG_GAPS && busy >= BUSY_SHARE {
        out.push(finding(
            FindingKey::BusyElsewhere,
            vec![
                ("share", busy.into()),
                (
                    "silent_share",
                    share(f64::from(s.long_gaps_silent), f64::from(s.long_gaps)).into(),
                ),
            ],
        ));
    }
    let next = |kind| s.next_messages.iter().find(|r| r.key == kind);
    let next_count = |kind| next(kind).map_or(0, |r| r.count);
    let approve = share(f64::from(next_count(NextKind::Approve)), n);
    let instructions = share(
        f64::from(next_count(NextKind::Instruction) + next_count(NextKind::Short)),
        n,
    );
    if approve < APPROVE_MAX_SHARE && instructions >= INSTRUCTION_MIN_SHARE {
        out.push(finding(
            FindingKey::MostlyInstructions,
            vec![
                ("approve_share", approve.into()),
                (
                    "approve_p50_secs",
                    next(NextKind::Approve).map_or(0.0, |r| r.p50_secs).into(),
                ),
            ],
        ));
    }
    if let Some(poll) = next(NextKind::Poll).filter(|r| r.count >= MIN_POLLS) {
        out.push(finding(
            FindingKey::Polls,
            vec![
                ("count", poll.count.into()),
                ("p50_secs", poll.p50_secs.into()),
            ],
        ));
    }
    if s.workdays.days >= MIN_WORKDAYS {
        out.push(finding(
            FindingKey::Load,
            vec![
                ("messages_p50", s.workdays.messages_p50.into()),
                ("chats_p50", s.workdays.chats_p50.into()),
            ],
        ));
    }
    if s.agent_turns.total_secs > 0.0 {
        let ratio = s.gaps.total_secs / s.agent_turns.total_secs;
        if ratio >= HUMAN_AGENT_RATIO {
            out.push(finding(
                FindingKey::HumanVsAgent,
                vec![("ratio", ratio.into())],
            ));
        }
    }
    if let Some(prev) = s
        .previous
        .filter(|p| p.gaps.n >= MIN_SAMPLE && p.gaps.p50_secs > 0.0)
    {
        let change = (s.gaps.p50_secs - prev.gaps.p50_secs) / prev.gaps.p50_secs;
        if change.abs() >= TREND_CHANGE {
            out.push(finding(
                FindingKey::Trend,
                vec![
                    (
                        "direction",
                        (if change > 0.0 { "up" } else { "down" }).into(),
                    ),
                    ("p50_secs", s.gaps.p50_secs.into()),
                    ("previous_p50_secs", prev.gaps.p50_secs.into()),
                ],
            ));
        }
    }
    out
}

// ── 下一条消息的分类 ────────────────────────────────────────────────────

/// 催问进展的说法:出现在消息里任何位置都算。英文按词边界比,不分大小写。
const POLL_PHRASES: &[&str] = &[
    // 中文
    "进展",
    "怎么样了",
    "好了吗",
    "好了没",
    "完成了吗",
    "跑完了吗",
    "结果呢",
    "状态如何",
    "到哪了",
    "还在跑",
    // 英文
    "any update",
    "any updates",
    "any progress",
    "any news",
    "progress?",
    "status?",
    "status update",
    "how's it going",
    "how is it going",
    "how's it looking",
    "done yet",
    "finished yet",
    "ready yet",
    "is it done",
    "are you done",
    "still running",
    "where are we",
    "eta",
];

/// 放行的说法:消息以它开头,且后面紧跟的是结尾、空白、标点或语气词(「好的吧」「ok, go」);
/// 「上线一下」「going」这类只是开头几个字相同,不算。
const APPROVE_WORDS: &[&str] = &[
    // 中文
    "继续",
    "可以",
    "认可",
    "好",
    "好的",
    "同意",
    "是",
    "是的",
    "行",
    "嗯",
    "确认",
    "开始",
    "合",
    "合入",
    "发",
    "发吧",
    "发布",
    "上",
    "上线",
    "按你说的",
    "按推荐",
    "做吧",
    "干吧",
    "搞",
    "没问题",
    // 英文
    "continue",
    "go",
    "go ahead",
    "ok",
    "okay",
    "yes",
    "y",
    "yep",
    "yeah",
    "sure",
    "lgtm",
    "approve",
    "approved",
    "agree",
    "confirm",
    "confirmed",
    "done",
    "proceed",
    "ship it",
    "merge",
    "merge it",
    "do it",
    "sounds good",
    "fine",
];

/// 选项式的放行(「b」「2。」):整条消息只有这个选项,后面至多跟标点或语气词。
const APPROVE_OPTIONS: &[&str] = &["a", "b", "c", "d", "1", "2", "3", "4"];

/// 放行词后面可以紧跟的字。
fn is_tail(c: char) -> bool {
    c.is_whitespace() || ",，。.!！?？~～、;；:：…".contains(c) || "的吧啊了呀哈啦嘛嗯".contains(c)
}

/// 英文说法两端要落在词边界上(「eta」不匹配「beta」);中文没有词边界,直接比。
fn contains_phrase(text: &str, phrase: &str) -> bool {
    let word = |c: Option<char>| c.is_some_and(|c| c.is_ascii_alphanumeric());
    let edge_start = phrase
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphanumeric());
    let edge_end = phrase
        .chars()
        .last()
        .is_some_and(|c| c.is_ascii_alphanumeric());
    text.match_indices(phrase).any(|(i, _)| {
        let before = text[..i].chars().next_back();
        let after = text[i + phrase.len()..].chars().next();
        !(edge_start && word(before)) && !(edge_end && word(after))
    })
}

fn approves(text: &str) -> bool {
    let words = APPROVE_WORDS.iter().any(|w| {
        text.strip_prefix(w)
            .is_some_and(|rest| rest.chars().next().is_none_or(is_tail))
    });
    words
        || APPROVE_OPTIONS.iter().any(|o| {
            text.strip_prefix(o)
                .is_some_and(|rest| rest.chars().all(is_tail))
        })
}

/// 下一条消息是哪一类:先去掉开头的 `@某人`,再按「催问进展 → 放行 → 短指令 → 较长的新指令」
/// 依次判断。
pub fn classify_next(head: &str) -> NextKind {
    let mut text = head.trim();
    if text.starts_with('@') {
        text = text
            .split_once(char::is_whitespace)
            .map_or("", |(_, rest)| rest)
            .trim_start();
    }
    let text = text.to_lowercase().replace('’', "'");
    let chars = text.chars().count();
    if chars <= POLL_MAX_CHARS && POLL_PHRASES.iter().any(|p| contains_phrase(&text, p)) {
        NextKind::Poll
    } else if chars <= SHORT_MAX_CHARS && approves(&text) {
        NextKind::Approve
    } else if chars <= SHORT_MAX_CHARS {
        NextKind::Short
    } else {
        NextKind::Instruction
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::FixedOffset;

    const ME: &str = "ou_00000000000000000000000000000001";
    const PEER: &str = "ou_00000000000000000000000000000002";

    fn tz() -> FixedOffset {
        FixedOffset::east_opt(8 * 3600).unwrap()
    }

    /// 2026-11 月某天本地时刻的毫秒数(11-09 是周一)。
    fn at(day: u32, h: u32, m: u32, s: u32) -> i64 {
        tz().with_ymd_and_hms(2026, 11, day, h, m, s)
            .unwrap()
            .timestamp_millis()
    }

    fn turn(tag: &str, origin: TurnOrigin, requester: &str, start: i64, end: i64) -> TurnRecord {
        TurnRecord {
            agent: "starlight".into(),
            chat_id: format!("oc_{tag}"),
            tag: tag.into(),
            turn: None,
            origin,
            schedule: None,
            requester: (!requester.is_empty()).then(|| requester.to_string()),
            message_id: "om_x".into(),
            prompt_head: "请把这个模块的单元测试补齐并跑一遍".into(),
            started_at_ms: start,
            ended_at_ms: end,
            end: TurnEnd::Done,
            error: None,
            reattached: false,
            tool_steps: 0,
            cli: None,
            model: None,
            effort: None,
            ctx: None,
            usage: Vec::new(),
            quota: None,
            feed: None,
        }
    }

    fn human(tag: &str, start: i64, end: i64) -> TurnRecord {
        turn(tag, TurnOrigin::Human, ME, start, end)
    }

    /// 一个会话里的一对:prev 在 `end` 结束,next 在 `gap_ms` 之后开始。
    fn pair(tag: &str, end: i64, gap_ms: i64) -> [TurnRecord; 2] {
        [
            human(tag, end - 30_000, end),
            human(tag, end + gap_ms, end + gap_ms + 10_000),
        ]
    }

    fn whole_month() -> Window {
        Window {
            since_ms: at(1, 0, 0, 0),
            until_ms: at(30, 0, 0, 0),
        }
    }

    fn run(records: &[TurnRecord]) -> Insights {
        run_in(records, whole_month(), None, None)
    }

    fn run_in(
        records: &[TurnRecord],
        window: Window,
        previous: Option<Window>,
        agent: Option<&str>,
    ) -> Insights {
        summarize(
            records,
            &Query {
                tz: &tz(),
                window,
                previous,
                agent,
            },
        )
    }

    #[test]
    fn pairs_need_adjacent_human_turns_from_the_same_requester_after_a_done_turn() {
        let base = at(10, 10, 0, 0);
        let mut failed = human("c", base + 1_000_000, base + 1_010_000);
        failed.end = TurnEnd::Failed;
        let records = vec![
            // a:相邻、同一人、prev 完成 —— 一条 60 秒的间隔
            human("a", base, base + 10_000),
            human("a", base + 70_000, base + 80_000),
            // 另一个会话插在中间,不打断 a 的相邻关系
            human("x", base + 20_000, base + 30_000),
            // b:发起人不同
            human("b", base, base + 10_000),
            turn("b", TurnOrigin::Human, PEER, base + 100_000, base + 110_000),
            // c:prev 失败
            failed,
            human("c", base + 1_100_000, base + 1_110_000),
            // d:中间夹了一轮定时任务,两对都不成立
            human("d", base, base + 10_000),
            turn("d", TurnOrigin::Schedule, "", base + 50_000, base + 60_000),
            human("d", base + 200_000, base + 210_000),
            // e:没有发起人的两轮不算同一个人
            turn("e", TurnOrigin::Human, "", base, base + 10_000),
            turn("e", TurnOrigin::Human, "", base + 100_000, base + 110_000),
        ];
        let s = run(&records);
        assert_eq!(s.gaps.n, 1);
        assert_eq!(s.gaps.total_secs, 60.0);
        assert_eq!(s.gaps_including_overnight, 1);
        assert_eq!(s.queued, 0);
        // 另一个 agent 下同名会话不会与它相邻
        let mut other = human("a", base + 40_000, base + 50_000);
        other.agent = "orbit".into();
        let mut with_other = records.clone();
        with_other.push(other);
        assert_eq!(run(&with_other).gaps.n, 1);
        assert_eq!(
            run_in(&with_other, whole_month(), None, Some("orbit"))
                .gaps
                .n,
            0
        );
    }

    #[test]
    fn messages_sent_within_two_seconds_are_queued_not_gaps() {
        let end = at(10, 10, 0, 0);
        let mut records = Vec::new();
        records.extend(pair("a", end, 1_999));
        records.extend(pair("b", end, 2_000));
        records.extend(pair("c", end, -5_000));
        let s = run(&records);
        assert_eq!(s.queued, 2);
        assert_eq!(s.gaps.n, 1);
        assert_eq!(s.gaps.total_secs, 2.0);
        assert_eq!(s.gaps_including_overnight, 1);
    }

    #[test]
    fn the_day_window_is_nine_to_twenty_two_on_the_same_local_day() {
        let mut records = Vec::new();
        records.extend(pair("before", at(10, 8, 59, 59), 60_000));
        records.extend(pair("start", at(10, 9, 0, 0), 60_000));
        records.extend(pair("last", at(10, 21, 59, 59), 60_000));
        records.extend(pair("end", at(10, 22, 0, 0), 60_000));
        // 21:30 回复,次日 09:00 才回:过夜
        records.extend(pair(
            "overnight",
            at(10, 21, 30, 0),
            11 * 3600 * 1000 + 1_800_000,
        ));
        // 21:59:30 回复,过了零点才回:不同一天
        records.extend(pair("midnight", at(10, 21, 59, 30), 3 * 3_600_000));
        let s = run(&records);
        assert_eq!(s.gaps.n, 2);
        assert_eq!(s.gaps_including_overnight, 6);
        assert_eq!((s.day_start_hour, s.day_end_hour), (9, 22));
    }

    #[test]
    fn buckets_split_at_their_lower_edges() {
        let mut records = Vec::new();
        for (i, secs) in [59, 60, 299, 300, 899, 900, 3_599, 3_600, 14_399, 14_400]
            .into_iter()
            .enumerate()
        {
            records.extend(pair(&format!("c{i}"), at(10, 9, 0, 0), secs * 1000));
        }
        let s = run(&records);
        let counts: Vec<(Span, u32)> = s.buckets.iter().map(|b| (b.key, b.count)).collect();
        assert_eq!(
            counts,
            [
                (Span::Lt1m, 1),
                (Span::M1To5, 2),
                (Span::M5To15, 2),
                (Span::M15To60, 2),
                (Span::H1To4, 2),
                (Span::Gte4h, 1),
            ]
        );
        assert_eq!(s.buckets[3].secs, 900.0 + 3_599.0);
        assert_eq!(
            s.buckets.iter().map(|b| b.secs).sum::<f64>(),
            s.gaps.total_secs
        );
        // 空的时间段也给齐六档、三档、四类
        let empty = run(&[]);
        assert_eq!(empty.buckets.len(), 6);
        assert_eq!(empty.elsewhere.len(), 3);
        assert_eq!(empty.next_messages.len(), 4);
        assert_eq!(empty.gaps, Quantiles::default());
    }

    #[test]
    fn quantiles_interpolate_linearly_between_ranks() {
        // 期望值按 k = (n−1)·p 手算:[1, 2] 的 p75 是 k = 0.75 → 1 + 0.75 × 1 = 1.75;
        // 升序 [1, 1.5, 2.6, 3, 4, 9] 的 p50 是 k = 2.5 → 2.6 + 0.5 × 0.4 = 2.8,p90 是 k = 4.5 →
        // 4 + 0.5 × 5 = 6.5;1² … 11² 的 p75 是 k = 7.5 → 64 + 0.5 × 17 = 72.5
        let q = |xs: &[f64]| quantiles(xs.to_vec());
        let one = q(&[5.0]);
        assert_eq!((one.p50_secs, one.p75_secs, one.p90_secs), (5.0, 5.0, 5.0));
        let two = q(&[2.0, 1.0]);
        assert_eq!((two.p50_secs, two.p75_secs, two.p90_secs), (1.5, 1.75, 1.9));
        let six = q(&[3.0, 1.0, 4.0, 1.5, 9.0, 2.6]);
        assert_eq!((six.p50_secs, six.p75_secs, six.p90_secs), (2.8, 3.75, 6.5));
        assert_eq!(six.n, 6);
        assert!((six.total_secs - 21.1).abs() < 1e-9);
        let squares: Vec<f64> = (1..12).map(|i| f64::from(i * i)).collect();
        let sq = q(&squares);
        assert_eq!((sq.p50_secs, sq.p75_secs, sq.p90_secs), (36.0, 72.5, 100.0));
    }

    /// agent 单轮耗时只算真人发起、正常完成的轮:中断与失败的轮哪怕拖了很久也不算。
    #[test]
    fn agent_turns_count_only_human_turns_that_completed() {
        let base = at(10, 10, 0, 0);
        let hour = 3_600_000;
        let done = human("a", base, base + 60_000);
        let mut interrupted = human("b", base, base + 5 * hour);
        interrupted.end = TurnEnd::Interrupted;
        let mut failed = human("c", base, base + 5 * hour);
        failed.end = TurnEnd::Failed;
        let s = run(&[done, interrupted, failed]);
        assert_eq!(s.agent_turns.n, 1);
        assert_eq!(s.agent_turns.total_secs, 60.0);
    }

    #[test]
    fn long_gaps_count_activity_elsewhere_and_mostly_silent_stretches() {
        let end = at(10, 10, 0, 0);
        let min = 60_000;
        let mut records = Vec::new();
        // 20 分钟的间隔:期间另一个会话里来了两条消息(第 2、4 分钟),最长空白 16 分钟 = 80%
        records.extend(pair("a", end, 20 * min));
        records.push(human("x", end + 2 * min, end + 3 * min));
        records.push(human("y", end + 4 * min, end + 5 * min));
        // 2 小时的间隔:别处一条也没有(定时任务不算人的消息)
        records.extend(pair("b", at(11, 10, 0, 0), 120 * min));
        records.push(turn(
            "z",
            TurnOrigin::Schedule,
            "",
            at(11, 10, 30, 0),
            at(11, 10, 31, 0),
        ));
        // 5 小时的间隔:别处每小时一条,最长空白 1 小时 = 20%
        records.extend(pair("c", at(12, 10, 0, 0), 300 * min));
        for h in 1..5 {
            records.push(human(
                &format!("w{h}"),
                at(12, 10 + h, 0, 0),
                at(12, 10 + h, 1, 0),
            ));
        }
        let s = run(&records);
        assert_eq!(s.long_gaps, 3);
        let rows: Vec<(Span, u32, u32, f64)> = s
            .elsewhere
            .iter()
            .map(|e| (e.key, e.count, e.active_elsewhere, e.elsewhere_messages_p50))
            .collect();
        assert_eq!(
            rows,
            [
                (Span::M15To60, 1, 1, 2.0),
                (Span::H1To4, 1, 0, 0.0),
                (Span::Gte4h, 1, 1, 4.0),
            ]
        );
        assert_eq!(s.elsewhere[1].secs, 7_200.0);
        assert_eq!(s.long_gaps_silent, 2);
        // 只看 a 所在的 agent 时,别处仍然看全部 agent
        let mut moved = records.clone();
        for r in moved.iter_mut().filter(|r| r.tag == "x" || r.tag == "y") {
            r.agent = "orbit".into();
        }
        let s = run_in(&moved, whole_month(), None, Some("starlight"));
        assert_eq!(s.elsewhere[0].active_elsewhere, 1);
        assert_eq!(s.elsewhere[0].elsewhere_messages_p50, 2.0);
    }

    #[test]
    fn next_messages_are_classified_in_chinese_and_english() {
        use NextKind::*;
        for (head, want) in [
            ("进展如何?", Poll),
            ("@星光 怎么样了", Poll),
            ("测试跑完了吗", Poll),
            ("Any update?", Poll),
            ("how's it going", Poll),
            ("How’s it going?", Poll),
            ("ETA?", Poll),
            ("继续", Approve),
            ("@starlight 好的吧", Approve),
            ("可以,合入", Approve),
            ("嗯嗯", Approve),
            ("b", Approve),
            ("2。", Approve),
            ("OK", Approve),
            ("Go ahead.", Approve),
            ("lgtm!", Approve),
            ("ship it", Approve),
            ("yes", Approve),
            ("@bot y", Approve),
            ("上线一下", Short),
            ("3个问题看一下", Short),
            ("a quick fix", Short),
            ("going home", Short),
            ("beta 分支", Short),
            ("fix the bug", Short),
            ("@星光", Short),
            ("", Short),
            ("把登录页的按钮颜色改成主题色并补截图", Instruction),
            ("Please refactor the parser module", Instruction),
            ("继续,另外把文档也同步更新一下谢谢", Instruction),
        ] {
            assert_eq!(classify_next(head), want, "{head}");
        }
        // 催问说法在 24 个字以内才算
        let long_poll = format!("{}进展", "字".repeat(23));
        assert_eq!(classify_next(&long_poll), Instruction);
        assert_eq!(classify_next(&format!("{}进展", "字".repeat(22))), Poll);
    }

    #[test]
    fn next_rows_follow_the_classification_of_the_next_message() {
        let mut records = Vec::new();
        for (i, (head, secs)) in [("继续", 30), ("好的", 90), ("进展?", 600), ("改一下", 45)]
            .into_iter()
            .enumerate()
        {
            let mut p = pair(&format!("c{i}"), at(10, 10, 0, 0), secs * 1000);
            p[1].prompt_head = head.into();
            records.extend(p);
        }
        let s = run(&records);
        let row = |k| s.next_messages.iter().find(|r| r.key == k).unwrap().clone();
        assert_eq!(row(NextKind::Approve).count, 2);
        assert_eq!(row(NextKind::Approve).p50_secs, 60.0);
        assert_eq!(row(NextKind::Poll).p90_secs, 600.0);
        assert_eq!(row(NextKind::Short).count, 1);
        assert_eq!(row(NextKind::Instruction).count, 0);
        let json = serde_json::to_string(&s).unwrap();
        assert!(!json.contains("继续") && !json.contains(ME), "{json}");
    }

    #[test]
    fn days_count_human_turns_and_chats_and_workdays_skip_idle_days() {
        // 11-06 周五 … 11-10 周二;11-09 周一一条消息都没有
        let window = Window {
            since_ms: at(6, 0, 0, 0),
            until_ms: at(11, 0, 0, 0),
        };
        let mut records = vec![
            human("a", at(6, 10, 0, 0), at(6, 10, 1, 0)),
            human("a", at(6, 11, 0, 0), at(6, 11, 1, 0)),
            human("b", at(6, 12, 0, 0), at(6, 12, 1, 0)),
            human("a", at(7, 10, 0, 0), at(7, 10, 1, 0)),
            human("a", at(10, 10, 0, 0), at(10, 10, 1, 0)),
            turn(
                "a",
                TurnOrigin::Schedule,
                "",
                at(10, 9, 0, 0),
                at(10, 9, 1, 0),
            ),
            // 窗口之外
            human("a", at(5, 23, 59, 59), at(6, 0, 1, 0)),
            human("a", at(11, 0, 0, 0), at(11, 0, 1, 0)),
        ];
        records.push(human("c", at(10, 23, 0, 0), at(10, 23, 1, 0)));
        let s = run_in(&records, window, None, None);
        let days: Vec<(&str, u32, u32, u32)> = s
            .days
            .iter()
            .map(|d| (d.date.as_str(), d.weekday, d.messages, d.chats))
            .collect();
        assert_eq!(
            days,
            [
                ("2026-11-06", 5, 3, 2),
                ("2026-11-07", 6, 1, 1),
                ("2026-11-08", 7, 0, 0),
                ("2026-11-09", 1, 0, 0),
                ("2026-11-10", 2, 2, 2),
            ]
        );
        assert_eq!(s.workdays.days, 2);
        assert_eq!(s.workdays.messages_p50, 2.5);
        assert_eq!(s.workdays.messages_p90, 2.9);
        assert_eq!(s.workdays.chats_p50, 2.0);
        assert_eq!((s.turns, s.human_turns, s.agents, s.chats), (7, 6, 1, 3));
    }

    #[test]
    fn the_previous_window_reports_its_own_gaps_and_long_shares() {
        let window = Window {
            since_ms: at(10, 0, 0, 0),
            until_ms: at(11, 0, 0, 0),
        };
        let previous = Window {
            since_ms: at(9, 0, 0, 0),
            until_ms: at(10, 0, 0, 0),
        };
        let mut records = Vec::new();
        records.extend(pair("a", at(9, 10, 0, 0), 60_000));
        records.extend(pair("b", at(9, 11, 0, 0), 300_000));
        records.extend(pair("c", at(9, 12, 0, 0), 1_200_000));
        records.extend(pair("d", at(9, 13, 0, 0), 3_000_000));
        records.extend(pair("e", at(10, 10, 0, 0), 120_000));
        let s = run_in(&records, window, Some(previous), None);
        assert_eq!(s.gaps.n, 1);
        let prev = s.previous.unwrap();
        assert_eq!(prev.gaps.n, 4);
        assert_eq!(prev.gaps.p50_secs, 750.0);
        assert_eq!(prev.over_15m_count_share, 0.5);
        assert_eq!(prev.over_15m_secs_share, 4_200.0 / 4_560.0);
        assert!(run_in(&records, window, None, None).previous.is_none());
        let empty = run_in(&[], window, Some(previous), None).previous.unwrap();
        assert_eq!(empty, Previous::default());
    }

    /// 一份刚好过样本线、什么结论都不触发的汇总:各条规则的测试在它上面改一处。
    fn baseline() -> Insights {
        let mut s = run(&[]);
        s.gaps = Quantiles {
            n: MIN_SAMPLE,
            p50_secs: 100.0,
            p75_secs: 200.0,
            p90_secs: 400.0,
            total_secs: 10_000.0,
        };
        s.buckets[0].count = MIN_SAMPLE - 5;
        s.buckets[0].secs = 5_000.0;
        s.buckets[3].count = 5;
        s.buckets[3].secs = 5_000.0;
        s.next_messages[0].count = 20;
        s.next_messages[2].count = 10;
        s.agent_turns.total_secs = 10_000.0;
        s
    }

    fn keys(s: &Insights) -> Vec<FindingKey> {
        findings(s).into_iter().map(|f| f.key).collect()
    }

    #[test]
    fn a_small_sample_gives_only_that_finding() {
        let mut s = baseline();
        assert_eq!(keys(&s), []);
        s.gaps.n = MIN_SAMPLE - 1;
        s.workdays.days = 10;
        let f = findings(&s);
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].key, FindingKey::SampleSmall);
        assert_eq!(f[0].params["n"], Param::Number(29.0));
    }

    #[test]
    fn each_finding_triggers_exactly_at_its_threshold() {
        // 长间隔占时长:70% 触发,低于不触发
        let mut s = baseline();
        s.buckets[0].secs = 3_000.0;
        s.buckets[3].secs = 7_000.0;
        let f = findings(&s);
        assert_eq!(f[0].key, FindingKey::TailConcentration);
        assert_eq!(f[0].params["secs_share"], Param::Number(0.7));
        assert_eq!(f[0].params["count_share"], Param::Number(5.0 / 30.0));
        s.buckets[3].secs = 6_999.0;
        assert!(!keys(&s).contains(&FindingKey::TailConcentration));

        // 别处有消息:20 条长间隔、12 条别处有消息(60%)触发;19 条或 11 条都不触发
        let mut s = baseline();
        s.long_gaps = 20;
        s.long_gaps_silent = 5;
        s.elsewhere[0].active_elsewhere = 12;
        let f = findings(&s);
        assert_eq!(f[0].key, FindingKey::BusyElsewhere);
        assert_eq!(f[0].params["share"], Param::Number(0.6));
        assert_eq!(f[0].params["silent_share"], Param::Number(0.25));
        s.elsewhere[0].active_elsewhere = 11;
        assert!(!keys(&s).contains(&FindingKey::BusyElsewhere));
        s.long_gaps = 19;
        s.elsewhere[0].active_elsewhere = 19;
        assert!(!keys(&s).contains(&FindingKey::BusyElsewhere));

        // 多半是新指令:放行 2 条(<10%)、新指令 + 短指令 24 条(80%)
        let mut s = baseline();
        s.next_messages[0].count = 20;
        s.next_messages[1].count = 4;
        s.next_messages[2].count = 2;
        s.next_messages[2].p50_secs = 42.0;
        let f = findings(&s);
        assert_eq!(f[0].key, FindingKey::MostlyInstructions);
        assert_eq!(f[0].params["approve_p50_secs"], Param::Number(42.0));
        s.next_messages[1].count = 3;
        assert!(!keys(&s).contains(&FindingKey::MostlyInstructions));
        s.next_messages[1].count = 4;
        s.next_messages[2].count = 3;
        assert!(!keys(&s).contains(&FindingKey::MostlyInstructions));

        // 催问:5 条触发,4 条不触发
        let mut s = baseline();
        s.next_messages[3].count = 5;
        s.next_messages[3].p50_secs = 900.0;
        let f = findings(&s);
        assert_eq!(f[0].key, FindingKey::Polls);
        assert_eq!(f[0].params["count"], Param::Number(5.0));
        s.next_messages[3].count = 4;
        assert_eq!(keys(&s), []);

        // 输入量:3 个工作日触发
        let mut s = baseline();
        s.workdays = Workdays {
            days: 3,
            messages_p50: 40.0,
            messages_p90: 60.0,
            chats_p50: 6.0,
            chats_p90: 9.0,
        };
        let f = findings(&s);
        assert_eq!(f[0].key, FindingKey::Load);
        assert_eq!(f[0].params["chats_p50"], Param::Number(6.0));
        s.workdays.days = 2;
        assert_eq!(keys(&s), []);

        // 人与 agent:间隔合计是 agent 耗时的 1.5 倍触发;agent 耗时为 0 不触发
        let mut s = baseline();
        s.gaps.total_secs = 15_000.0;
        let f = findings(&s);
        assert_eq!(f[0].key, FindingKey::HumanVsAgent);
        assert_eq!(f[0].params["ratio"], Param::Number(1.5));
        s.agent_turns.total_secs = 10_001.0;
        assert_eq!(keys(&s), []);
        s.agent_turns.total_secs = 0.0;
        assert_eq!(keys(&s), []);

        // 趋势:上一时段 30 条、中位数变化 25% 触发
        let mut s = baseline();
        let mut prev = Previous::default();
        prev.gaps.n = MIN_SAMPLE;
        prev.gaps.p50_secs = 80.0;
        s.previous = Some(prev);
        let f = findings(&s);
        assert_eq!(f[0].key, FindingKey::Trend);
        assert_eq!(f[0].params["direction"], Param::Text("up"));
        assert_eq!(f[0].params["previous_p50_secs"], Param::Number(80.0));
        prev.gaps.p50_secs = 140.0;
        s.previous = Some(prev);
        assert_eq!(findings(&s)[0].params["direction"], Param::Text("down"));
        prev.gaps.p50_secs = 90.0;
        s.previous = Some(prev);
        assert_eq!(keys(&s), []);
        prev.gaps.p50_secs = 80.0;
        prev.gaps.n = MIN_SAMPLE - 1;
        s.previous = Some(prev);
        assert_eq!(keys(&s), []);
    }

    #[test]
    fn the_summary_serializes_with_the_contract_keys() {
        let v = serde_json::to_value(run(&[])).unwrap();
        let keys: Vec<&str> = v["buckets"]
            .as_array()
            .unwrap()
            .iter()
            .map(|b| b["key"].as_str().unwrap())
            .collect();
        assert_eq!(keys, ["lt_1m", "m1_5", "m5_15", "m15_60", "h1_4", "gte_4h"]);
        assert_eq!(v["next_messages"][2]["key"], "approve");
        assert_eq!(v["gaps"]["total_secs"].to_string(), "0.0");
        assert_eq!(v["elsewhere"][2]["secs"].to_string(), "0.0");
        assert_eq!(v["findings"][0]["key"], "sample_small");
        assert_eq!(v["findings"][0]["params"]["n"], 0.0);
        assert!(v.get("previous").is_none());
    }
}
