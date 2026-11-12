//! 台账的汇总:按时间分桶数各类结果,按维度分组算轮数、耗时分位与失败,以及额度曲线。token 与费用
//! 由 [`crate::pricing`] 按同样的分组计,轮外用量([`OutsideUsage`])算进 token 与费用,不算进轮数
//! 与耗时。
//!
//! 纯计算。分桶边界按调用方给的时区取(本地的整点与零点),所以「今天」「近 7 天」与人看到的
//! 日历对得上;夏令时那天的桶长短不一,按真实边界算,不假设每桶等长。零点恰好被夏令时跳过的时区
//! (如古巴),那一天从当天第一个存在的时刻算起。

use chrono::{DateTime, Duration, NaiveDate, NaiveTime, TimeZone};
use serde::{Deserialize, Serialize};

use crate::turn_ledger::{ModelUsage, OutsideUsage, TurnEnd, TurnQuota, TurnRecord};

/// 额度曲线最多留多少个点:够画一条看得出起伏的线,又不让 30 天的数据把响应撑大。
pub const MAX_QUOTA_POINTS: usize = 120;

/// 各类结果的轮数。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EndCounts {
    pub done: u32,
    pub empty: u32,
    pub silent_reset: u32,
    pub failed: u32,
    pub interrupted: u32,
}

impl EndCounts {
    pub fn add(&mut self, end: TurnEnd) {
        let slot = match end {
            TurnEnd::Done => &mut self.done,
            TurnEnd::Empty => &mut self.empty,
            TurnEnd::SilentReset => &mut self.silent_reset,
            TurnEnd::Failed => &mut self.failed,
            TurnEnd::Interrupted => &mut self.interrupted,
        };
        *slot += 1;
    }

    pub fn total(&self) -> u32 {
        self.done + self.empty + self.silent_reset + self.failed + self.interrupted
    }
}

/// 一组轮的统计。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupStats {
    pub turns: u32,
    pub failed: u32,
    pub interrupted: u32,
    /// 耗时合计(秒)。
    pub seconds: u64,
    pub avg_secs: u64,
    /// 90 分位耗时(最近秩法:排序后第 ⌈0.9·n⌉ 个)。
    pub p90_secs: u64,
}

/// 额度曲线上的一个点。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuotaPoint {
    pub at_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub five_hour_left: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub long_left: Option<u8>,
}

/// 一个 agent 在时间段里看到的额度:最近一次的读数与按时间排的曲线。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuotaSeries {
    /// 最近一次读数所在那一轮的结束时刻。
    pub at_ms: i64,
    pub latest: TurnQuota,
    pub points: Vec<QuotaPoint>,
}

/// 一轮跑了多少秒(四舍五入,时钟倒退的按 0)。
pub fn turn_secs(record: &TurnRecord) -> u64 {
    let ms = record
        .ended_at_ms
        .saturating_sub(record.started_at_ms)
        .max(0);
    u64::try_from((ms + 500) / 1000).unwrap_or(0)
}

/// 一组轮的统计。
pub fn stats<'a>(records: impl IntoIterator<Item = &'a TurnRecord>) -> GroupStats {
    let mut out = GroupStats::default();
    let mut secs = Vec::new();
    for record in records {
        out.turns += 1;
        match record.end {
            TurnEnd::Failed => out.failed += 1,
            TurnEnd::Interrupted => out.interrupted += 1,
            _ => {}
        }
        secs.push(turn_secs(record));
    }
    if secs.is_empty() {
        return out;
    }
    secs.sort_unstable();
    out.seconds = secs.iter().sum();
    let n = secs.len() as u64;
    out.avg_secs = (out.seconds + n / 2) / n;
    let rank = (secs.len() * 9).div_ceil(10);
    out.p90_secs = secs[rank.max(1) - 1];
    out
}

/// 按 `key` 分组:轮数多的在前,一样多的按 key 排。
pub fn group_by<K: Ord + Clone>(
    records: &[TurnRecord],
    key: impl Fn(&TurnRecord) -> K,
) -> Vec<(K, Vec<&TurnRecord>)> {
    let mut groups: std::collections::BTreeMap<K, Vec<&TurnRecord>> = Default::default();
    for record in records {
        groups.entry(key(record)).or_default().push(record);
    }
    let mut out: Vec<(K, Vec<&TurnRecord>)> = groups.into_iter().collect();
    out.sort_by(|(ka, a), (kb, b)| b.len().cmp(&a.len()).then_with(|| ka.cmp(kb)));
    out
}

/// 一组:组里的轮与组里的轮外用量。
pub struct Group<'a, K> {
    pub key: K,
    pub records: Vec<&'a TurnRecord>,
    pub outside: Vec<&'a OutsideUsage>,
}

impl<K> Group<'_, K> {
    /// 这一组的全部 token:轮里的与轮外的。
    pub fn usage(&self) -> impl Iterator<Item = &ModelUsage> {
        self.records
            .iter()
            .flat_map(|r| &r.usage)
            .chain(self.outside.iter().flat_map(|o| &o.usage))
    }
}

/// 轮与轮外用量一起按 key 分组:轮数多的在前,一样多的按 key 排。只有轮外用量的组也在,轮数为 0。
pub fn group_all<'a, K: Ord + Clone>(
    records: &'a [TurnRecord],
    outside: &'a [OutsideUsage],
    key: impl Fn(&TurnRecord) -> K,
    outside_key: impl Fn(&OutsideUsage) -> K,
) -> Vec<Group<'a, K>> {
    let mut groups: std::collections::BTreeMap<K, Group<'a, K>> = Default::default();
    for record in records {
        let k = key(record);
        groups
            .entry(k.clone())
            .or_insert_with(|| Group {
                key: k,
                records: Vec::new(),
                outside: Vec::new(),
            })
            .records
            .push(record);
    }
    for row in outside {
        let k = outside_key(row);
        groups
            .entry(k.clone())
            .or_insert_with(|| Group {
                key: k,
                records: Vec::new(),
                outside: Vec::new(),
            })
            .outside
            .push(row);
    }
    let mut out: Vec<Group<'a, K>> = groups.into_values().collect();
    out.sort_by(|a, b| {
        b.records
            .len()
            .cmp(&a.records.len())
            .then_with(|| a.key.cmp(&b.key))
    });
    out
}

/// 各桶里开始的轮外用量,分桶同 [`bucket_records`]。
pub fn bucket_outside<'a>(
    outside: &'a [OutsideUsage],
    starts: &[i64],
    until_ms: i64,
) -> Vec<Vec<&'a OutsideUsage>> {
    let mut out = vec![Vec::new(); starts.len()];
    for row in outside {
        let at = row.from_ms;
        if starts.first().is_none_or(|first| at < *first) || at >= until_ms {
            continue;
        }
        let index = starts.partition_point(|start| *start <= at) - 1;
        out[index].push(row);
    }
    out
}

/// 各桶里结束的轮。`starts` 升序,第 i 个桶是 `[starts[i], starts[i+1])`,最后一个到 `until_ms`
/// 为止;落在第一个桶之前或 `until_ms` 及之后的不算。
pub fn bucket_records<'a>(
    records: &'a [TurnRecord],
    starts: &[i64],
    until_ms: i64,
) -> Vec<Vec<&'a TurnRecord>> {
    let mut out = vec![Vec::new(); starts.len()];
    for record in records {
        let at = record.ended_at_ms;
        if starts.first().is_none_or(|first| at < *first) || at >= until_ms {
            continue;
        }
        let index = starts.partition_point(|start| *start <= at) - 1;
        out[index].push(record);
    }
    out
}

/// 各桶里结束的轮按结果计数,分桶同 [`bucket_records`]。
pub fn bucket_counts(records: &[TurnRecord], starts: &[i64], until_ms: i64) -> Vec<EndCounts> {
    bucket_records(records, starts, until_ms)
        .into_iter()
        .map(|rs| {
            let mut counts = EndCounts::default();
            for r in rs {
                counts.add(r.end);
            }
            counts
        })
        .collect()
}

/// 这一天在本地日历上的起点:通常是零点;零点落在夏令时跳过的那段里时,取当天第一个存在的时刻。
fn start_of_day<Tz: TimeZone>(tz: &Tz, date: NaiveDate) -> Option<DateTime<Tz>> {
    (0..=16)
        .map(|quarter| NaiveTime::MIN + Duration::minutes(15 * quarter))
        .find_map(|time| tz.from_local_datetime(&date.and_time(time)).earliest())
}

/// 今天从零点到此刻所在那个小时,每个整点一个桶起点(毫秒)。
pub fn hour_starts<Tz: TimeZone>(now: &DateTime<Tz>) -> Vec<i64> {
    let mut out = Vec::new();
    let mut at = start_of_day(&now.timezone(), now.date_naive()).unwrap_or_else(|| now.clone());
    while at <= *now {
        out.push(at.timestamp_millis());
        at += Duration::hours(1);
    }
    out
}

/// `from` 到 `to`(含)每天零点一个起点(毫秒),最早的在前;另给 `to` 次日零点作为截止。
/// `to` 早于 `from` 时没有日子。
pub fn days_between<Tz: TimeZone>(tz: &Tz, from: NaiveDate, to: NaiveDate) -> (Vec<i64>, i64) {
    let starts: Vec<i64> = from
        .iter_days()
        .take_while(|d| *d <= to)
        .filter_map(|d| start_of_day(tz, d).map(|at| at.timestamp_millis()))
        .collect();
    let until = to
        .succ_opt()
        .and_then(|next| start_of_day(tz, next))
        .map_or(i64::MAX, |at| at.timestamp_millis());
    (starts, until)
}

/// 截至今天的最近 `days` 天,每天零点一个桶起点(毫秒),最早的在前。
pub fn day_starts<Tz: TimeZone>(now: &DateTime<Tz>, days: u32) -> Vec<i64> {
    let today = now.date_naive();
    let tz = now.timezone();
    (0..days)
        .rev()
        .filter_map(|back| {
            start_of_day(&tz, today - Duration::days(i64::from(back)))
                .map(|at| at.timestamp_millis())
        })
        .collect()
}

/// 一个 agent 的额度:带额度读数的轮按时间排成曲线(点多时均匀抽稀,最后一个点总留着),
/// 最近一次读数单列。一轮都没有读数就没有。
pub fn quota_series<'a>(records: impl IntoIterator<Item = &'a TurnRecord>) -> Option<QuotaSeries> {
    let mut seen: Vec<(&TurnRecord, &TurnQuota)> = records
        .into_iter()
        .filter_map(|r| r.quota.as_ref().map(|q| (r, q)))
        .filter(|(_, q)| q.five_hour_left.is_some() || q.long_left.is_some())
        .collect();
    seen.sort_by_key(|(r, _)| r.ended_at_ms);
    let (last, latest) = *seen.last()?;
    let step = seen.len().div_ceil(MAX_QUOTA_POINTS);
    let mut points: Vec<QuotaPoint> = seen
        .iter()
        .enumerate()
        .filter(|(i, _)| i % step == 0 || *i == seen.len() - 1)
        .map(|(_, (r, q))| QuotaPoint {
            at_ms: r.ended_at_ms,
            five_hour_left: q.five_hour_left,
            long_left: q.long_left,
        })
        .collect();
    points.dedup_by_key(|p| p.at_ms);
    Some(QuotaSeries {
        at_ms: last.ended_at_ms,
        latest: latest.clone(),
        points,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::turn_ledger::TurnOrigin;
    use chrono::FixedOffset;

    fn record(ended_at_ms: i64, secs: i64, end: TurnEnd) -> TurnRecord {
        TurnRecord {
            agent: "starlight".into(),
            chat_id: "oc_x".into(),
            tag: "g-x".into(),
            turn: None,
            origin: TurnOrigin::Human,
            schedule: None,
            requester: None,
            message_id: "om_x".into(),
            prompt_head: String::new(),
            started_at_ms: ended_at_ms - secs * 1000,
            ended_at_ms,
            end,
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

    #[test]
    fn stats_count_outcomes_and_take_the_nearest_rank_p90() {
        let records: Vec<TurnRecord> = (1..=10)
            .map(|i| {
                let end = match i {
                    3 => TurnEnd::Failed,
                    7 => TurnEnd::Interrupted,
                    _ => TurnEnd::Done,
                };
                record(1_000_000 + i * 10_000, i * 10, end)
            })
            .collect();
        let s = stats(&records);
        assert_eq!(s.turns, 10);
        assert_eq!(s.failed, 1);
        assert_eq!(s.interrupted, 1);
        assert_eq!(s.seconds, 550);
        assert_eq!(s.avg_secs, 55);
        assert_eq!(s.p90_secs, 90);
        assert_eq!(stats(&records[..1]).p90_secs, 10);
        // 最近秩:⌈0.9 × 11⌉ = 10,取第 10 小的那个。
        let eleven: Vec<TurnRecord> = (1..=11)
            .map(|i| record(1_000_000 + i * 10_000, i * 10, TurnEnd::Done))
            .collect();
        assert_eq!(stats(&eleven).p90_secs, 100);
        assert_eq!(stats(std::iter::empty()), GroupStats::default());
    }

    #[test]
    fn groups_put_the_busiest_first() {
        let mut records = vec![record(1, 1, TurnEnd::Done); 3];
        records[0].agent = "b".into();
        records[1].agent = "a".into();
        records[2].agent = "b".into();
        let groups = group_by(&records, |r| r.agent.clone());
        assert_eq!(groups[0].0, "b");
        assert_eq!(groups[0].1.len(), 2);
        assert_eq!(groups[1].0, "a");
    }

    #[test]
    fn buckets_take_turns_by_their_end_time() {
        let starts = [100, 200, 300];
        let records = [
            record(99, 1, TurnEnd::Done),
            record(100, 1, TurnEnd::Done),
            record(199, 1, TurnEnd::Failed),
            record(300, 1, TurnEnd::Interrupted),
            record(399, 1, TurnEnd::Empty),
            record(400, 1, TurnEnd::Done),
        ];
        let counts = bucket_counts(&records, &starts, 400);
        let grouped = bucket_records(&records, &starts, 400);
        assert_eq!(grouped.iter().map(Vec::len).collect::<Vec<_>>(), [2, 0, 2]);
        assert_eq!(grouped[2][0].ended_at_ms, 300);
        assert_eq!(counts[0].done, 1);
        assert_eq!(counts[0].failed, 1);
        assert_eq!(counts[1].total(), 0);
        assert_eq!(counts[2].interrupted, 1);
        assert_eq!(counts[2].empty, 1);
        assert_eq!(counts.iter().map(EndCounts::total).sum::<u32>(), 4);
    }

    #[test]
    fn hour_and_day_starts_follow_the_local_calendar() {
        let tz = FixedOffset::east_opt(8 * 3600).unwrap();
        let now = tz.with_ymd_and_hms(2026, 10, 20, 14, 32, 0).unwrap();
        let hours = hour_starts(&now);
        assert_eq!(hours.len(), 15);
        let midnight = tz.with_ymd_and_hms(2026, 10, 20, 0, 0, 0).unwrap();
        assert_eq!(hours[0], midnight.timestamp_millis());
        assert_eq!(hours[14] - hours[0], 14 * 3600 * 1000);
        let days = day_starts(&now, 7);
        assert_eq!(days.len(), 7);
        assert_eq!(days[6], midnight.timestamp_millis());
        assert_eq!(
            days[0],
            tz.with_ymd_and_hms(2026, 10, 14, 0, 0, 0)
                .unwrap()
                .timestamp_millis()
        );
    }

    #[test]
    fn calendar_starts_follow_daylight_saving_switches() {
        use chrono_tz::America::{Havana, New_York};
        // 纽约回拨那天有 25 个小时:零点到中午是 14 个整点桶,天桶照常 7 个。
        let fall = New_York.with_ymd_and_hms(2026, 11, 1, 12, 0, 0).unwrap();
        assert_eq!(hour_starts(&fall).len(), 14);
        assert_eq!(day_starts(&fall, 7).len(), 7);
        // 古巴的夏令时在零点跳:那天没有零点,从 1:00 算起;天桶也不能少。
        let spring = Havana.with_ymd_and_hms(2026, 3, 8, 12, 0, 0).unwrap();
        let hours = hour_starts(&spring);
        assert_eq!(hours.len(), 12);
        assert_eq!(
            hours[0],
            Havana
                .with_ymd_and_hms(2026, 3, 8, 1, 0, 0)
                .unwrap()
                .timestamp_millis()
        );
        let days = day_starts(&Havana.with_ymd_and_hms(2026, 3, 10, 12, 0, 0).unwrap(), 7);
        assert_eq!(days.len(), 7);
        assert!(days.windows(2).all(|w| w[0] < w[1]));
    }

    #[test]
    fn quota_series_keeps_the_latest_reading_and_thins_long_curves() {
        assert!(quota_series(&[record(1, 1, TurnEnd::Done)]).is_none());
        let records: Vec<TurnRecord> = (0..300)
            .map(|i| {
                let mut r = record(1_000 + i, 1, TurnEnd::Done);
                r.quota = Some(TurnQuota {
                    five_hour_left: Some(u8::try_from(100 - i / 3).unwrap()),
                    long_left: Some(90),
                    ..Default::default()
                });
                r
            })
            .collect();
        let series = quota_series(&records).unwrap();
        assert_eq!(series.at_ms, 1_299);
        assert_eq!(series.latest.five_hour_left, Some(1));
        assert!(series.points.len() <= MAX_QUOTA_POINTS + 1);
        assert_eq!(series.points.last().unwrap().at_ms, 1_299);
        assert_eq!(series.points[0].at_ms, 1_000);
    }

    fn outside(chat_id: &str, from_ms: i64, output: u64) -> OutsideUsage {
        OutsideUsage {
            agent: "starlight".into(),
            chat_id: chat_id.into(),
            tag: "g-x".into(),
            cli: "claude".into(),
            from_ms,
            to_ms: from_ms + 10,
            usage: vec![ModelUsage {
                model: "claude-test-1".into(),
                requests: 1,
                output,
                ..Default::default()
            }],
        }
    }

    #[test]
    fn outside_usage_joins_groups_without_adding_turns() {
        let mut a = record(1_000, 5, TurnEnd::Done);
        a.usage = vec![ModelUsage {
            model: "claude-test-1".into(),
            requests: 1,
            output: 3,
            ..Default::default()
        }];
        let records = vec![a];
        let rows = vec![outside("oc_x", 500, 7), outside("oc_y", 600, 11)];
        let groups = group_all(
            &records,
            &rows,
            |r| r.chat_id.clone(),
            |o| o.chat_id.clone(),
        );
        let summary: Vec<(&str, usize, u64)> = groups
            .iter()
            .map(|g| {
                (
                    g.key.as_str(),
                    g.records.len(),
                    g.usage().map(|u| u.output).sum(),
                )
            })
            .collect();
        assert_eq!(summary, [("oc_x", 1, 10), ("oc_y", 0, 11)]);
        let buckets = bucket_outside(&rows, &[0, 550], 700);
        assert_eq!(buckets.iter().map(Vec::len).collect::<Vec<_>>(), [1, 1]);
        assert!(bucket_outside(&rows, &[550], 600).iter().all(Vec::is_empty));
    }

    #[test]
    fn days_between_covers_both_ends_and_stops_at_the_next_midnight() {
        let tz = chrono::FixedOffset::east_opt(8 * 3600).unwrap();
        let day = |d| NaiveDate::from_ymd_opt(2026, 10, d).unwrap();
        let midnight = |d| {
            tz.with_ymd_and_hms(2026, 10, d, 0, 0, 0)
                .unwrap()
                .timestamp_millis()
        };
        let (starts, until) = days_between(&tz, day(20), day(22));
        assert_eq!(starts, [midnight(20), midnight(21), midnight(22)]);
        assert_eq!(until, midnight(23));
        assert!(days_between(&tz, day(22), day(20)).0.is_empty());
    }
}
