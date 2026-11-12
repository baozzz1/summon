//! `summon insights`:人在 agent 环路里的等待 —— 回复落地到下一条消息的间隔、长间隔期间的别处
//! 活动、下一条消息的类型与每日输入量。只读盘上的运行记录,不经 daemon、不联网;汇总与控制台
//! 洞察页是同一份代码(`runtime::dashboard::api::insights::build`),`--json` 输出与接口相同的结构。

use chrono::{DateTime, Duration, Local, NaiveDate};
use summon_core::insights::{NextKind, Span};

use crate::runtime::dashboard::api::insights::build;
use crate::runtime::dashboard::api::types::{InsightsView, UsageRange};
use crate::runtime::dashboard::api::usage::range_dates;

#[derive(clap::Args, Debug)]
pub struct InsightsArgs {
    #[arg(help = summon_i18n::text!("r13ae8a8f1d"))]
    /// 起始日期(本机日历,含这一天);两个日期都不给时取近 30 天(含今天),只给 --to 时同 --to
    #[arg(long, value_name = "YYYY-MM-DD")]
    pub from: Option<NaiveDate>,
    #[arg(help = summon_i18n::text!("r7a21a7c0a0"))]
    /// 截止日期(含这一天);默认今天
    #[arg(long, value_name = "YYYY-MM-DD")]
    pub to: Option<NaiveDate>,
    #[arg(help = summon_i18n::text!("r3eaf4823b4"))]
    /// 只看这个 agent 的会话(别处活动仍统计全部 agent)
    #[arg(long)]
    pub agent: Option<String>,
    #[arg(help = summon_i18n::text!("rcdd71bbffb"))]
    /// 输出 JSON(给脚本与 agent 读)
    #[arg(long)]
    pub json: bool,
}

/// 两个日期都不给时看的天数,与控制台洞察页的默认时间段(近 30 天)一致:单日的间隔几乎总是不到
/// 结论要求的样本数。
const DEFAULT_DAYS: i64 = 30;

/// 命令行日期 → 时间段与日期区间。都不给时就是页面上的「近 30 天」(`30d`,含今天),输出的
/// `range` 与上一时段对比也与页面相同;给了任一个就是自定义区间。
pub(crate) fn dates(
    from: Option<NaiveDate>,
    to: Option<NaiveDate>,
    today: NaiveDate,
) -> (UsageRange, NaiveDate, NaiveDate) {
    match (from, to) {
        (None, None) => (
            UsageRange::Month,
            today - Duration::days(DEFAULT_DAYS - 1),
            today,
        ),
        (from, Some(to)) => (UsageRange::Custom, from.unwrap_or(to), to),
        (Some(from), None) => (UsageRange::Custom, from, today),
    }
}

/// 日期区间的校验与洞察页接口(`range_dates` 的自定义区间)一致:起不晚于止、止不晚于今天、不早于
/// 1970-01-01。前两条用命令行自己的说法报出具体日期,其余交给接口那一份校验。
pub(crate) fn check(from: NaiveDate, to: NaiveDate, now: &DateTime<Local>) -> anyhow::Result<()> {
    if from > to {
        anyhow::bail!(summon_i18n::format!("rf00be05e72"));
    }
    let today = now.date_naive();
    if to > today {
        anyhow::bail!(summon_i18n::format!("r5e77089af0"));
    }
    range_dates(
        UsageRange::Custom,
        Some(&from.to_string()),
        Some(&to.to_string()),
        now,
    )
    .map(|_| ())
    .map_err(|e| anyhow::anyhow!(e.message))
}

pub(crate) fn run(args: InsightsArgs) -> anyhow::Result<()> {
    let now = Local::now();
    let (range, from, to) = dates(args.from, args.to, now.date_naive());
    check(from, to, &now)?;
    let view = build(
        &crate::runtime::paths::run_dir(),
        &now,
        range,
        from,
        to,
        args.agent.as_deref(),
    );
    if args.json {
        println!("{}", serde_json::to_string_pretty(&view)?);
    } else {
        print!("{}", render(&view));
    }
    Ok(())
}

/// 时长:90 秒以内写秒,90 分钟以内写分钟,再往上写小时(一位小数)。
fn dur(secs: f64) -> String {
    if secs < 90.0 {
        format!("{secs:.0}s")
    } else if secs < 5_400.0 {
        format!("{:.0}min", secs / 60.0)
    } else {
        format!("{:.1}h", secs / 3_600.0)
    }
}

/// 中位数这类可能带半个的数:整数照写,否则一位小数。
fn num(v: f64) -> String {
    if v.fract() == 0.0 {
        format!("{v:.0}")
    } else {
        format!("{v:.1}")
    }
}

fn percent(part: f64, whole: f64) -> String {
    if whole > 0.0 {
        format!("{:.0}", part / whole * 100.0)
    } else {
        "0".into()
    }
}

fn render(view: &InsightsView) -> String {
    let s = &view.insights;
    let mut out = summon_i18n::format!(
        "r22c696bc01",
        from = view.from,
        to = view.to,
        turns = s.human_turns,
        agents = s.agents,
        chats = s.chats
    );
    let g = &s.gaps;
    out.push_str(&summon_i18n::format!(
        "rcc8913b1ce",
        start = s.day_start_hour,
        end = s.day_end_hour,
        n = g.n,
        p50 = dur(g.p50_secs),
        p75 = dur(g.p75_secs),
        p90 = dur(g.p90_secs),
        total = dur(g.total_secs),
        overnight = s.gaps_including_overnight,
        queued = s.queued
    ));
    let a = &s.agent_turns;
    out.push_str(&summon_i18n::format!(
        "r78443cfb4c",
        n = a.n,
        p50 = dur(a.p50_secs),
        p90 = dur(a.p90_secs),
        total = dur(a.total_secs)
    ));
    let mut long_secs = 0.0;
    for b in &s.buckets {
        if matches!(b.key, Span::M15To60 | Span::H1To4 | Span::Gte4h) {
            long_secs += b.secs;
        }
    }
    out.push_str(&summon_i18n::format!(
        "r6f6c638586",
        n = s.long_gaps,
        count_share = percent(f64::from(s.long_gaps), f64::from(g.n)),
        secs_share = percent(long_secs, g.total_secs),
        active = s.elsewhere.iter().map(|e| e.active_elsewhere).sum::<u32>(),
        silent = s.long_gaps_silent
    ));
    let next = |kind| {
        s.next_messages
            .iter()
            .find(|r| r.key == kind)
            .map_or(0, |r| r.count)
    };
    out.push_str(&summon_i18n::format!(
        "ref829f37ba",
        instruction = next(NextKind::Instruction),
        short = next(NextKind::Short),
        approve = next(NextKind::Approve),
        poll = next(NextKind::Poll)
    ));
    let w = &s.workdays;
    out.push_str(&summon_i18n::format!(
        "re34915cef0",
        days = w.days,
        messages = num(w.messages_p50),
        chats = num(w.chats_p50)
    ));
    if let Some(prev) = &s.previous {
        out.push_str(&summon_i18n::format!(
            "rfbf4cf4e64",
            n = prev.gaps.n,
            p50 = dur(prev.gaps.p50_secs)
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::dashboard::api::testkit::*;
    use chrono::TimeZone;
    use serde_json::json;
    use summon_core::turn_ledger::{TurnEnd, TurnOrigin, TurnRecord};

    fn record(chat_id: &str, started_at_ms: i64, head: &str) -> TurnRecord {
        TurnRecord {
            agent: "starlight".into(),
            chat_id: chat_id.into(),
            tag: summon_core::registry::chat_tag("group", chat_id),
            turn: None,
            origin: TurnOrigin::Human,
            schedule: None,
            requester: Some(ME_OPEN.into()),
            message_id: "om_x".into(),
            prompt_head: head.into(),
            started_at_ms,
            ended_at_ms: started_at_ms + 60_000,
            end: TurnEnd::Done,
            error: None,
            reattached: false,
            tool_steps: 0,
            cli: Some("claude".into()),
            model: None,
            effort: None,
            ctx: None,
            usage: Vec::new(),
            quota: None,
            feed: None,
        }
    }

    /// CLI 与控制台洞察页读同一份盘上数据,同一时间段的数逐项一致。
    #[test]
    fn the_cli_and_the_insights_page_agree_on_every_number() {
        let kit = Kit::new();
        let mut ctx = kit.owner();
        ctx.now = Local.with_ymd_and_hms(2026, 11, 4, 14, 0, 0).unwrap();
        let at = |h, m| {
            Local
                .with_ymd_and_hms(2026, 11, 3, h, m, 0)
                .unwrap()
                .timestamp_millis()
        };
        let dir = crate::runtime::turn_ledger::dir_in(&kit.home().join("run")).join("starlight");
        std::fs::create_dir_all(&dir).unwrap();
        let lines: String = [
            record(GROUP, at(10, 0), "不该出现在汇总里"),
            record(GROUP, at(10, 11), "继续"),
            record(GROUP, at(11, 12), "把发布脚本的超时改成可配置"),
        ]
        .iter()
        .map(|r| serde_json::to_string(r).unwrap() + "\n")
        .collect();
        std::fs::write(
            dir.join(summon_core::turn_ledger::file_name(at(10, 0))),
            lines,
        )
        .unwrap();
        let page = kit
            .call(
                &ctx,
                crate::runtime::dashboard::api::insights::summary,
                json!({"range": "custom", "from": "2026-11-01", "to": "2026-11-04"}),
            )
            .unwrap();
        let from = NaiveDate::from_ymd_opt(2026, 11, 1).unwrap();
        let to = NaiveDate::from_ymd_opt(2026, 11, 4).unwrap();
        let view = build(
            &kit.home().join("run"),
            &ctx.now,
            UsageRange::Custom,
            from,
            to,
            None,
        );
        assert_eq!(serde_json::to_value(&view).unwrap(), page);
        assert_eq!(page["gaps"]["n"], 2);
        let text = render(&view);
        assert!(text.contains("2026-11-01 至 2026-11-04"), "{text}");
        assert!(text.contains("2 条,中位数 35min"), "{text}");
        assert!(text.contains("放行 1"), "{text}");
        assert!(text.contains("上一时段"), "{text}");
        assert!(!text.contains("不该出现在汇总里"));
    }

    /// 不给日期时与页面默认的「近 30 天」是同一段日期;给了任一个才是自定义区间。
    #[test]
    fn without_dates_the_cli_reads_the_same_30_days_as_the_page() {
        let now = Local.with_ymd_and_hms(2026, 11, 11, 14, 0, 0).unwrap();
        let today = now.date_naive();
        let page =
            crate::runtime::dashboard::api::usage::range_dates(UsageRange::Month, None, None, &now)
                .unwrap();
        assert_eq!(
            dates(None, None, today),
            (UsageRange::Month, page.0, page.1)
        );
        assert_eq!(page.0, NaiveDate::from_ymd_opt(2026, 10, 13).unwrap());
        let d = |day| NaiveDate::from_ymd_opt(2026, 11, day).unwrap();
        assert_eq!(
            dates(None, Some(d(5)), today),
            (UsageRange::Custom, d(5), d(5))
        );
        assert_eq!(
            dates(Some(d(1)), None, today),
            (UsageRange::Custom, d(1), today)
        );
        assert_eq!(
            dates(Some(d(1)), Some(d(3)), today),
            (UsageRange::Custom, d(1), d(3))
        );
    }

    /// 命令行与洞察页接口对同一段日期的取舍一致;晚于今天的 --to 报出那个日期。
    #[test]
    fn dates_are_checked_like_the_page() {
        let now = Local.with_ymd_and_hms(2026, 11, 11, 14, 0, 0).unwrap();
        let d = |m, day| NaiveDate::from_ymd_opt(2026, m, day).unwrap();
        let cases = [
            (d(11, 1), d(11, 11)),
            (d(11, 11), d(11, 11)),
            (d(11, 1), d(11, 12)),
            (d(11, 5), d(11, 3)),
            (NaiveDate::from_ymd_opt(1969, 12, 31).unwrap(), d(11, 1)),
        ];
        for (from, to) in cases {
            let page = range_dates(
                UsageRange::Custom,
                Some(&from.to_string()),
                Some(&to.to_string()),
                &now,
            )
            .is_ok();
            assert_eq!(check(from, to, &now).is_ok(), page, "{from} ~ {to}");
        }
        let err = check(d(11, 1), d(11, 12), &now).unwrap_err().to_string();
        assert!(
            err.contains("2026-11-12") && err.contains("2026-11-11"),
            "{err}"
        );
        // 只给晚于今天的 --from:--to 默认今天,落到「起晚于止」
        let (_, from, to) = dates(Some(d(11, 20)), None, now.date_naive());
        assert!(check(from, to, &now).is_err());
    }

    #[test]
    fn durations_and_counts_read_naturally() {
        assert_eq!(dur(0.0), "0s");
        assert_eq!(dur(89.4), "89s");
        assert_eq!(dur(90.0), "2min");
        assert_eq!(dur(5_399.0), "90min");
        assert_eq!(dur(5_400.0), "1.5h");
        assert_eq!(num(2.0), "2");
        assert_eq!(num(2.5), "2.5");
        assert_eq!(percent(1.0, 3.0), "33");
        assert_eq!(percent(1.0, 0.0), "0");
    }
}
