//! 最小 5 段 cron(分 时 日 月 周,本地时区)。支持 `*`/`a-b`/`a,b`/`*/n`/数字;周 0-6(0=周日)。
//! 不支持秒段/`@reboot`/`L`/`#`(YAGNI)。`next_after` 用分钟步进 + 366 天封顶(简单、可测、够用)。

use chrono::{DateTime, Datelike, Duration, Local, Timelike};

/// 一个 cron 字段:允许的取值集合(已展开)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CronExpr {
    minute: Vec<u32>,
    hour: Vec<u32>,
    dom: Vec<u32>,
    month: Vec<u32>,
    dow: Vec<u32>,
}

pub fn parse_cron(s: &str) -> Result<CronExpr, String> {
    let f: Vec<&str> = s.split_whitespace().collect();
    if f.len() != 5 {
        return Err(summon_i18n::format!("rf7a376dcce", f.len()));
    }
    Ok(CronExpr {
        minute: field(f[0], 0, 59)?,
        hour: field(f[1], 0, 23)?,
        dom: field(f[2], 1, 31)?,
        month: field(f[3], 1, 12)?,
        dow: field(f[4], 0, 6)?,
    })
}

/// 把一个字段(`*`/`a-b`/`a,b`/`*/n`/数字)展开成允许值集合。
fn field(spec: &str, lo: u32, hi: u32) -> Result<Vec<u32>, String> {
    let mut out = Vec::new();
    for part in spec.split(',') {
        let (range, step) = match part.split_once('/') {
            Some((r, st)) => (
                r,
                st.parse::<u32>()
                    .map_err(|_| summon_i18n::format!("r8404faa07c"))?,
            ),
            None => (part, 1),
        };
        if step == 0 {
            return Err(summon_i18n::format!("r6dcdd1459a"));
        }
        let (a, b) = if range == "*" {
            (lo, hi)
        } else if let Some((x, y)) = range.split_once('-') {
            (
                x.parse().map_err(|_| summon_i18n::format!("rfcdb9a683c"))?,
                y.parse().map_err(|_| summon_i18n::format!("rfcdb9a683c"))?,
            )
        } else {
            let v: u32 = range
                .parse()
                .map_err(|_| summon_i18n::format!("r739ee7abb5"))?;
            (v, v)
        };
        if a < lo || b > hi || a > b {
            return Err(summon_i18n::format!("rfee400ae81"));
        }
        out.extend((a..=b).step_by(step as usize));
    }
    out.sort_unstable();
    out.dedup();
    Ok(out)
}

/// 严格大于 `from` 的下一个匹配时刻(秒/纳秒归零)。分钟步进,366 天找不到则回退 `from`(防死循环)。
pub fn next_after(e: &CronExpr, from: DateTime<Local>) -> DateTime<Local> {
    let mut t = (from + Duration::minutes(1))
        .with_second(0)
        .and_then(|t| t.with_nanosecond(0))
        .unwrap_or(from);
    let cap = from + Duration::days(366);
    while t <= cap {
        let dow = t.weekday().num_days_from_sunday();
        if e.minute.contains(&t.minute())
            && e.hour.contains(&t.hour())
            && e.dom.contains(&t.day())
            && e.month.contains(&t.month())
            && e.dow.contains(&dow)
        {
            return t;
        }
        t += Duration::minutes(1);
    }
    from
}

/// 星期中文名(索引 = `num_days_from_sunday`,0=周日)。
const DOW_LABEL_KEYS: [&str; 7] = [
    "r85217f7aff",
    "r51a75f4634",
    "r084b42f6e9",
    "ra4c3313deb",
    "r754a9d5828",
    "rc9b87f516a",
    "rde07b53838",
];

/// 「纯周期」cron 的解构:`(周集合, 时, 分)`。不满足则 `None`。
///
/// 判据是「日/月为全集 + 时刻唯一」——只有这样才存在「每周几的几点」这个说法。卡片能生成的全部
/// cron 都落在这一类里,所以人话渲染与编辑预填都以它为唯一入口:**同一个判据,两处不会漂移**。
pub fn plain_weekly(spec: &str) -> Option<(Vec<u32>, u32, u32)> {
    let e = parse_cron(spec).ok()?;
    // 时刻必须唯一,否则「几点」这句话本身就不成立。
    let (&[hour], &[minute]) = (e.hour.as_slice(), e.minute.as_slice()) else {
        return None;
    };
    // 日/月必须是全集:一旦限定了「几号」或「几月」,就不再是纯周期节律。
    if e.dom.len() != 31 || e.month.len() != 12 {
        return None;
    }
    Some((e.dow, hour, minute))
}

/// 一天里说得出口的时刻数上限。超过就不渲成一长串 —— 「每天 00:00、02:00、…、22:00」
/// 比原表达式 `0 */2 * * *` 更难读,那正是本模块「说不清就别猜」要避免的。
const MAX_TIMES_IN_WORDS: usize = 4;

/// 「每周几 + 一天里的哪几个时刻」的解构:`(周集合, 时集合, 分)`。
///
/// 与 [`plain_weekly`] 的区别只有一处:**允许多个整点**。`0 10,20 * * *`(每天两次)是很自然的
/// 写法,却因为 `plain_weekly` 要求「时刻唯一」而只能回退成裸 cron —— owner 2026-08-10 实测
/// 看到的就是 `cron 0 10,20 * * *`。分钟仍要求唯一:`0,30 10,20 * * *` 展开是四个时刻,
/// 与其塞进一行,不如老实回显表达式。
///
/// **`plain_weekly` 的语义不动**:编辑卡预填要的是「唯一时刻」(控件只有一个时间选择器),
/// 放宽它会让预填拿到错的值。两个函数各服务一个用途。
fn plain_weekly_times(spec: &str) -> Option<(Vec<u32>, Vec<u32>, u32)> {
    let e = parse_cron(spec).ok()?;
    let &[minute] = e.minute.as_slice() else {
        return None;
    };
    if e.hour.is_empty() || e.hour.len() > MAX_TIMES_IN_WORDS {
        return None;
    }
    // 日/月必须是全集:一旦限定了「几号」或「几月」,就不再是纯周期节律。
    if e.dom.len() != 31 || e.month.len() != 12 {
        return None;
    }
    Some((e.dow, e.hour, minute))
}

/// 把 cron 表达式说成人话;说不清就返回 `None`,由调用方原样回显表达式。
///
/// why 故意不猜:`*/15 9-18 * * *` 渲染成一句似是而非的中文,比直接给出原表达式更容易误导。
pub fn human(spec: &str) -> Option<String> {
    let (dow, hours, minute) = plain_weekly_times(spec)?;
    let hhmm = hours
        .iter()
        .map(|h| format!("{h:02}:{minute:02}"))
        .collect::<Vec<_>>()
        .join("、");
    Some(match dow.as_slice() {
        d if d.len() == 7 => summon_i18n::format!("r993f659488"),
        [1, 2, 3, 4, 5] => summon_i18n::format!("r8c2f795f92"),
        [0, 6] => summon_i18n::format!("r5ef46caca6"),
        d => {
            let names: Vec<&str> = d
                .iter()
                .map(|&i| crate::i18n::label(DOW_LABEL_KEYS[i as usize]))
                .collect();
            summon_i18n::format!("r979fcd5d99", names.join("、"))
        }
    })
}

/// 卡片「每周」多选 → cron 的周字段。空集调用方须先拒。
pub fn dow_field(days: &[u32]) -> String {
    days.iter()
        .map(|d| d.to_string())
        .collect::<Vec<_>>()
        .join(",")
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Local, TimeZone};

    fn at(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> chrono::DateTime<Local> {
        Local.with_ymd_and_hms(y, mo, d, h, mi, 0).unwrap()
    }

    /// owner 2026-08-10 实测:`/schedules` 上「每天 10 点、20 点」只显示成裸表达式
    /// `cron 0 10,20 * * *`。一天跑两次是很自然的写法,不该说不出人话。
    #[test]
    fn human_speaks_multiple_times_a_day() {
        assert_eq!(human("0 10,20 * * *").as_deref(), Some("每天 10:00、20:00"));
        assert_eq!(
            human("30 9,13,18 * * 1-5").as_deref(),
            Some("每个工作日 09:30、13:30、18:30")
        );
        // 展开等价的写法给同一句话。
        assert_eq!(human("0 10-11 * * *"), human("0 10,11 * * *"));
    }

    /// 时刻太多就别硬渲:「每天 00:00、02:00、…、22:00」比原表达式更难读,
    /// 回退成裸 cron 反而诚实。分钟不唯一同理(展开是二维的,一行说不清)。
    #[test]
    fn human_declines_when_a_list_would_be_worse_than_the_expression() {
        assert!(human("0 */2 * * *").is_none(), "12 个时刻不该渲成一长串");
        assert!(
            human("0,30 10,20 * * *").is_none(),
            "四个时刻的二维展开不渲"
        );
        assert!(human("0 9 1 * *").is_none(), "限定了几号就不是纯周期节律");
    }

    /// **`plain_weekly` 的语义不许被放宽**:编辑卡预填只有一个时间选择器,
    /// 拿到多时刻就会填错。人话渲染走它自己那条路。
    #[test]
    fn plain_weekly_still_requires_a_single_time() {
        assert!(plain_weekly("0 10,20 * * *").is_none());
        assert_eq!(
            plain_weekly("0 10 * * *").map(|(_, h, m)| (h, m)),
            Some((10, 0))
        );
    }

    #[test]
    fn human_covers_card_generated_shapes() {
        // 卡片能生成的四种形态都要说得出人话。
        assert_eq!(human("0 9 * * *").as_deref(), Some("每天 09:00"));
        assert_eq!(human("0 9 * * 1-5").as_deref(), Some("每个工作日 09:00"));
        assert_eq!(human("30 10 * * 1,3").as_deref(), Some("每周一、三 10:30"));
        assert_eq!(human("0 8 * * 0,6").as_deref(), Some("每个周末 08:00"));
        // 展开后等价的写法必须给出同一句话(1-5 与 1,2,3,4,5 是同一个集合)。
        assert_eq!(human("0 9 * * 1,2,3,4,5"), human("0 9 * * 1-5"));
        assert_eq!(human("0 9 * * 0-6"), human("0 9 * * *"));
    }

    #[test]
    fn human_declines_rather_than_guesses() {
        // 多值时刻 / 限定几号 / 限定月份:说不清就返回 None,由调用方回显原表达式。
        assert_eq!(human("*/15 9 * * *"), None);
        assert_eq!(human("0 9-18 * * *"), None);
        assert_eq!(human("0 9 1 * *"), None);
        assert_eq!(human("0 9 * 6 *"), None);
        assert_eq!(human("garbage"), None);
    }

    #[test]
    fn dow_field_joins_sorted_days() {
        assert_eq!(dow_field(&[1, 3]), "1,3");
        assert_eq!(dow_field(&[0]), "0");
    }

    #[test]
    fn parse_rejects_wrong_field_count() {
        assert!(parse_cron("0 9 * *").is_err());
        assert!(parse_cron("0 9 * * 1-5 7").is_err());
        assert!(parse_cron("0 9 * * 1-5").is_ok());
    }

    #[test]
    fn next_after_daily_9am_weekdays() {
        let e = parse_cron("0 9 * * 1-5").unwrap();
        let sat = at(2026, 6, 13, 12, 0);
        let n = next_after(&e, sat);
        assert_eq!(n, at(2026, 6, 15, 9, 0));
        assert_eq!(
            next_after(&e, at(2026, 6, 15, 8, 30)),
            at(2026, 6, 15, 9, 0)
        );
        assert_eq!(next_after(&e, at(2026, 6, 15, 9, 0)), at(2026, 6, 16, 9, 0));
    }

    #[test]
    fn step_and_list_and_range() {
        let e = parse_cron("*/15 * * * *").unwrap();
        assert_eq!(
            next_after(&e, at(2026, 6, 14, 10, 7)),
            at(2026, 6, 14, 10, 15)
        );
        let e2 = parse_cron("0 0,12 * * *").unwrap();
        assert_eq!(
            next_after(&e2, at(2026, 6, 14, 6, 0)),
            at(2026, 6, 14, 12, 0)
        );
    }

    #[test]
    fn sunday_is_zero() {
        let e = parse_cron("0 8 * * 0").unwrap();
        assert_eq!(next_after(&e, at(2026, 6, 15, 0, 0)), at(2026, 6, 21, 8, 0));
    }
}
