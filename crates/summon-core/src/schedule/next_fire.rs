//! 定时器接下来几次的触发时刻(纯),控制台列表与编辑节律时的预览用。
//!
//! 与 [`super::due_plan`] 同一套口径:去重靠 `last_fired`,错过的周期只补一次、在下一个 tick
//! 立刻补上(所以已过期未触发的那一次记为 `now`),补完之后从补的那一刻起算下一个周期 ——
//! scheduler 落盘的 `last_fired` 就是实际触发的时刻。停用的定时器没有下次触发。

use super::{cron, parse_rfc3339, parse_rfc3339_loose, Kind, Schedule};
use chrono::{DateTime, Duration, Local};

/// 从 `now` 起接下来最多 `n` 次触发时刻。
pub fn next_fires(schedule: &Schedule, now: DateTime<Local>, n: usize) -> Vec<DateTime<Local>> {
    if !schedule.enabled || n == 0 {
        return Vec::new();
    }
    let last = schedule.last_fired.as_deref().and_then(parse_rfc3339);
    let created = parse_rfc3339_loose(&schedule.created_at).unwrap_or(now);
    match schedule.kind {
        Kind::Once => {
            if last.is_some() {
                return Vec::new();
            }
            parse_rfc3339(&schedule.spec)
                .map(|when| vec![when.max(now)])
                .unwrap_or_default()
        }
        Kind::Interval => {
            let Some(step) = schedule
                .spec
                .parse::<i64>()
                .ok()
                .filter(|secs| *secs > 0)
                .and_then(Duration::try_seconds)
            else {
                return Vec::new();
            };
            let Some(first) = last.unwrap_or(created).checked_add_signed(step) else {
                return Vec::new();
            };
            successors(first.max(now), n, |t| t.checked_add_signed(step))
        }
        Kind::Cron => {
            let Ok(expr) = cron::parse_cron(&schedule.spec) else {
                return Vec::new();
            };
            // `next_after` 在一年内找不到匹配时原样返回起点:当作「没有下一次」。
            let next = |t: DateTime<Local>| Some(cron::next_after(&expr, t)).filter(|n| *n > t);
            let from = last.unwrap_or(created);
            let Some(slot) = next(from) else {
                return Vec::new();
            };
            successors(slot.max(now), n, next)
        }
    }
}

/// `first` 起逐个取后继,最多 `n` 个;后继不存在就停。
fn successors(
    first: DateTime<Local>,
    n: usize,
    next: impl Fn(DateTime<Local>) -> Option<DateTime<Local>>,
) -> Vec<DateTime<Local>> {
    std::iter::successors(Some(first), |t| next(*t))
        .take(n)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schedule::{ScheduleContext, ScheduleOutput};
    use chrono::TimeZone;

    fn at(h: u32, m: u32) -> DateTime<Local> {
        Local.with_ymd_and_hms(2026, 10, 20, h, m, 0).unwrap()
    }

    fn schedule(kind: Kind, spec: &str, last_fired: Option<DateTime<Local>>) -> Schedule {
        Schedule {
            id: "SCH-1".into(),
            kind,
            spec: spec.into(),
            agent: "a".into(),
            chat_id: "oc_x".into(),
            chat_type: "group".into(),
            prompt: "p".into(),
            created_by: "ou".into(),
            created_at: at(8, 0).to_rfc3339(),
            output: ScheduleOutput::Progress,
            context: ScheduleContext::Resume,
            enabled: true,
            last_fired: last_fired.map(|t| t.to_rfc3339()),
            revision: 0,
        }
    }

    #[test]
    fn once_in_future_fires_at_its_time_only() {
        let s = schedule(Kind::Once, &at(15, 0).to_rfc3339(), None);
        assert_eq!(next_fires(&s, at(10, 0), 3), [at(15, 0)]);
    }

    #[test]
    fn once_overdue_fires_now_and_fired_once_never_again() {
        let s = schedule(Kind::Once, &at(9, 0).to_rfc3339(), None);
        assert_eq!(
            next_fires(&s, at(10, 0), 3),
            [at(10, 0)],
            "过期未触发:下一个 tick 补上"
        );
        let fired = schedule(Kind::Once, &at(9, 0).to_rfc3339(), Some(at(9, 0)));
        assert!(next_fires(&fired, at(10, 0), 3).is_empty());
    }

    #[test]
    fn interval_counts_from_last_fired_or_created_at() {
        // 从未触发:以 created_at(08:00)起算,每 2h。
        let s = schedule(Kind::Interval, "7200", None);
        assert_eq!(
            next_fires(&s, at(9, 0), 3),
            [at(10, 0), at(12, 0), at(14, 0)]
        );
        // 上次 09:30 触发。
        let s = schedule(Kind::Interval, "3600", Some(at(9, 30)));
        assert_eq!(
            next_fires(&s, at(10, 0), 3),
            [at(10, 30), at(11, 30), at(12, 30)]
        );
        // 错过了好几个周期:只补一次(now),之后从补的那一刻起算。
        let s = schedule(Kind::Interval, "600", Some(at(8, 0)));
        assert_eq!(
            next_fires(&s, at(10, 0), 3),
            [at(10, 0), at(10, 10), at(10, 20)]
        );
    }

    #[test]
    fn cron_rhythms_list_upcoming_slots() {
        let now = at(10, 0); // 2026-10-20 是周二
                             // daily 09:30:今天已过 → 明天起。
        let s = schedule(Kind::Cron, "30 9 * * *", Some(at(9, 30)));
        let day = |d: u32, h: u32, m: u32| Local.with_ymd_and_hms(2026, 10, d, h, m, 0).unwrap();
        assert_eq!(
            next_fires(&s, now, 3),
            [day(21, 9, 30), day(22, 9, 30), day(23, 9, 30)]
        );
        // weekday 09:30:周五之后跳过周末。
        let s = schedule(Kind::Cron, "30 9 * * 1-5", Some(day(22, 9, 30)));
        assert_eq!(
            next_fires(&s, day(22, 10, 0), 3),
            [day(23, 9, 30), day(26, 9, 30), day(27, 9, 30)]
        );
        // weekly mon,fri 10:00。
        let s = schedule(Kind::Cron, "0 10 * * 1,5", Some(at(9, 0)));
        assert_eq!(
            next_fires(&s, now, 3),
            [day(23, 10, 0), day(26, 10, 0), day(30, 10, 0)]
        );
        // 任意 cron:每 15 分钟。
        let s = schedule(Kind::Cron, "*/15 * * * *", Some(at(10, 0)));
        assert_eq!(
            next_fires(&s, at(10, 5), 3),
            [at(10, 15), at(10, 30), at(10, 45)]
        );
        // 上个槽错过了(daemon 停着):下一个 tick 立刻补,之后按槽。
        let s = schedule(Kind::Cron, "0 9 * * *", Some(day(19, 9, 0)));
        assert_eq!(next_fires(&s, now, 2), [now, day(21, 9, 0)]);
        // 永远不会到的日期(2 月 31 日):不死循环,也不编一个时刻出来。
        let s = schedule(Kind::Cron, "0 0 31 2 *", None);
        assert!(next_fires(&s, now, 3).is_empty());
    }

    #[test]
    fn disabled_or_broken_schedules_have_no_next_fire() {
        let mut s = schedule(Kind::Interval, "3600", None);
        s.enabled = false;
        assert!(next_fires(&s, at(10, 0), 3).is_empty());
        for (kind, spec) in [
            (Kind::Interval, "0"),
            (Kind::Interval, "abc"),
            (Kind::Cron, "61 * * * *"),
            (Kind::Once, "明天"),
        ] {
            let s = schedule(kind, spec, None);
            assert!(next_fires(&s, at(10, 0), 3).is_empty(), "{kind:?} {spec}");
        }
        let s = schedule(Kind::Interval, "3600", None);
        assert!(next_fires(&s, at(10, 0), 0).is_empty());
    }
}
