//! `summon usage`:按天 / agent / 群 / CLI 汇总运行记录与轮外用量的 token 与费用。只读盘上的运行记录、
//! 轮外用量与价格表,不经 daemon、不联网;分组与计价与控制台用量页是同一份代码
//! (`runtime::dashboard::api::usage::Report`),两边的数逐项一致。

use std::collections::HashMap;
use std::path::Path;

use chrono::{Local, NaiveDate};
use serde::Serialize;
use summon_core::pricing::{PriceTable, Spend};
use summon_core::registry::{Agent, Registry};
use summon_core::turn_usage;

use super::table::{Cell, Row, Table};
use crate::runtime::dashboard::api::types::{PriceSource, UsageDay, UsageRow};
use crate::runtime::dashboard::api::usage::{load_rows, Report};

/// 按什么汇总。
#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageBy {
    Day,
    Agent,
    Chat,
    Cli,
}

#[derive(clap::Args, Debug)]
pub struct UsageArgs {
    #[arg(help = summon_i18n::text!("r9b34207437"))]
    /// 起始日期(本机日历,含这一天);默认今天,只给 --to 时同 --to
    #[arg(long, value_name = "YYYY-MM-DD")]
    pub from: Option<NaiveDate>,
    #[arg(help = summon_i18n::text!("r7a21a7c0a0"))]
    /// 截止日期(含这一天);默认今天
    #[arg(long, value_name = "YYYY-MM-DD")]
    pub to: Option<NaiveDate>,
    #[arg(help = summon_i18n::text!("r9922797705"))]
    /// 汇总维度:day(按天)、agent、chat(按群聊 / 私聊)、cli
    #[arg(long, value_enum, default_value = "day")]
    pub by: UsageBy,
    #[arg(help = summon_i18n::text!("rcdd71bbffb"))]
    /// 输出 JSON(给脚本与 agent 读)
    #[arg(long)]
    pub json: bool,
}

#[derive(Serialize)]
struct Totals {
    turns: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    spend: Option<Spend>,
    /// 其中轮外用量的那部分。
    #[serde(skip_serializing_if = "Option::is_none")]
    outside: Option<Spend>,
}

#[derive(Serialize)]
#[serde(untagged)]
enum Rows {
    Days(Vec<UsageDay>),
    Groups(Vec<UsageRow>),
}

/// `--json` 的输出:日期区间、价格表、合计与各组。各组的结构与控制台 `usage.summary` 的同名字段相同。
#[derive(Serialize)]
struct UsageReport {
    /// 日期区间(本机日历,`YYYY-MM-DD`,含两端)。
    from: String,
    to: String,
    by: UsageBy,
    #[serde(skip_serializing_if = "Option::is_none")]
    prices: Option<PriceSource>,
    totals: Totals,
    rows: Rows,
}

fn build(
    run_dir: &Path,
    agents: &HashMap<String, Agent>,
    from: NaiveDate,
    to: NaiveDate,
    by: UsageBy,
) -> UsageReport {
    let (starts, until) = turn_usage::days_between(&Local, from, to);
    let since = starts.first().copied().unwrap_or(until);
    let (records, outside) = load_rows(run_dir, since, until);
    let table = crate::runtime::prices::load(run_dir);
    let prices: Option<&PriceTable> = table.as_deref();
    let report = Report {
        records: &records,
        outside: &outside,
        prices,
        agents,
    };
    let rows = match by {
        UsageBy::Day => Rows::Days(report.by_day(&starts, until)),
        UsageBy::Agent => Rows::Groups(report.by_agent()),
        UsageBy::Chat => Rows::Groups(report.by_chat()),
        UsageBy::Cli => Rows::Groups(report.by_cli()),
    };
    UsageReport {
        from: from.to_string(),
        to: to.to_string(),
        by,
        prices: prices.map(|t| PriceSource {
            source: t.source.clone(),
            fetched_at: crate::runtime::dashboard::api::sessions::iso_ms(t.fetched_at_ms)
                .unwrap_or_default(),
        }),
        totals: Totals {
            turns: u32::try_from(records.len()).unwrap_or(u32::MAX),
            spend: report.spend(),
            outside: report.outside_spend(),
        },
        rows,
    }
}

pub(crate) fn run(agents_dir: &Path, args: UsageArgs) -> anyhow::Result<()> {
    let today = Local::now().date_naive();
    let to = args.to.unwrap_or(today);
    let from = args
        .from
        .unwrap_or(if args.to.is_some() { to } else { today });
    if from > to {
        anyhow::bail!(summon_i18n::format!("rf00be05e72"));
    }
    let agents: HashMap<String, Agent> = Registry::load_tolerant(agents_dir)
        .map(|(reg, _)| reg.agents)
        .unwrap_or_default()
        .into_iter()
        .map(|a| (a.name.clone(), a))
        .collect();
    let report = build(
        &crate::runtime::paths::run_dir(),
        &agents,
        from,
        to,
        args.by,
    );
    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print!("{}", render(&report));
    }
    Ok(())
}

/// token 数,与控制台一致:1,000 以下照写,往上用 K / M / B,保留一位小数并省略 .0。
fn count(n: u64) -> String {
    let short = |scale: u64, unit: &str| {
        let step = scale / 10;
        let tenths = n / step + u64::from(n % step >= step / 2);
        if tenths.is_multiple_of(10) {
            format!("{}{unit}", tenths / 10)
        } else {
            format!("{}.{}{unit}", tenths / 10, tenths % 10)
        }
    };
    match n {
        0..=999 => n.to_string(),
        1_000..=999_999 => short(1_000, "K"),
        1_000_000..=999_999_999 => short(1_000_000, "M"),
        _ => short(1_000_000_000, "B"),
    }
}

fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// 金额,写法与控制台一致:不小于 $1 两位小数,小于 $1 三位有效数字,0 写 `$0`。
fn usd(v: f64) -> String {
    if v <= 0.0 {
        return "$0".into();
    }
    if v >= 1.0 {
        let cents = (v * 100.0).round() as u64;
        return format!("${}.{:02}", thousands(cents / 100), cents % 100);
    }
    let decimals = (2 - v.log10().floor() as i32).max(0) as usize;
    let text = format!("{v:.decimals$}");
    let text = text.trim_end_matches('0').trim_end_matches('.');
    format!("${text}")
}

fn tokens_of(s: &Spend) -> u64 {
    s.tokens.input + s.tokens.output + s.tokens.cache_read + s.tokens.cache_write
}

/// 一笔金额,口径与控制台一致:有未计价的部分时标「起」(不含未计价的部分),全部未计价写「未计价」。
fn amount(s: &Spend, cost: f64) -> String {
    match &s.unpriced {
        Some(u) if u.tokens >= tokens_of(s) => summon_i18n::text!("re9e69996fd").into(),
        Some(_) => summon_i18n::format!("r946cf30bdb", usd(cost)),
        None => usd(cost),
    }
}

/// 一项:金额 / token 数。
fn part(s: &Spend, cost: f64, tokens: u64) -> String {
    format!("{} / {}", amount(s, cost), count(tokens))
}

fn cells(label: String, turns: u32, spend: Option<&Spend>) -> Vec<Cell> {
    let mut out = vec![Cell::plain(label), Cell::plain(turns.to_string())];
    match spend {
        Some(s) => {
            out.push(Cell::plain(part(s, s.cost.total, tokens_of(s))));
            out.push(Cell::plain(part(s, s.cost.input, s.tokens.input)));
            out.push(Cell::plain(part(s, s.cost.cache_read, s.tokens.cache_read)));
            out.push(Cell::plain(part(
                s,
                s.cost.cache_write,
                s.tokens.cache_write,
            )));
            out.push(Cell::plain(part(s, s.cost.output, s.tokens.output)));
        }
        None => out.extend((0..5).map(|_| Cell::plain("—"))),
    }
    out
}

fn render(report: &UsageReport) -> String {
    let dim = match report.by {
        UsageBy::Day => summon_i18n::text!("r70d0c1b336"),
        UsageBy::Agent => "Agent",
        UsageBy::Chat => summon_i18n::text!("r35b49ee58a"),
        UsageBy::Cli => "CLI",
    };
    let mut out = if report.from == report.to {
        summon_i18n::format!("r6ba955d2e7", report.from)
    } else {
        summon_i18n::format!("ra7fc31270d", report.from, report.to)
    };
    match &report.prices {
        Some(p) => out.push_str(&summon_i18n::format!(
            "rc000cb4769",
            chrono::DateTime::parse_from_rfc3339(&p.fetched_at)
                .map(|t| t.with_timezone(&Local).format("%Y-%m-%d %H:%M").to_string())
                .unwrap_or_else(|_| p.fetched_at.clone())
        )),
        None => out.push_str(summon_i18n::text!("r8f8154d627")),
    }
    let mut table = Table::new(vec![
        dim,
        summon_i18n::text!("rc3e860113d"),
        summon_i18n::text!("re04daec4d4"),
        summon_i18n::text!("r8dcc1bf789"),
        summon_i18n::text!("r1ded6559f6"),
        summon_i18n::text!("r6078a681a3"),
        summon_i18n::text!("rfb04addb4c"),
    ])
    .cap(0, 24);
    match &report.rows {
        Rows::Days(days) => {
            for d in days {
                let label = d.start.get(..10).unwrap_or(&d.start).to_string();
                table.push(Row::new(cells(label, d.turns, d.spend.as_ref())));
            }
        }
        Rows::Groups(rows) => {
            for r in rows {
                let label = match (&report.by, &r.agent) {
                    (UsageBy::Chat, Some(agent)) => format!("{} · {agent}", r.label),
                    _ => r.label.clone(),
                };
                table.push(Row::new(cells(label, r.stats.turns, r.spend.as_ref())));
            }
        }
    }
    table.push(Row::new(cells(
        summon_i18n::text!("re009c148d1").into(),
        report.totals.turns,
        report.totals.spend.as_ref(),
    )));
    out.push_str(&table.render());
    if !out.ends_with('\n') {
        out.push('\n');
    }
    if let Some(outside) = &report.totals.outside {
        out.push_str(&summon_i18n::format!(
            "rd7f579ac84",
            part(outside, outside.cost.total, tokens_of(outside))
        ));
    }
    if let Some(unpriced) = report
        .totals
        .spend
        .as_ref()
        .and_then(|s| s.unpriced.as_ref())
    {
        out.push_str(&summon_i18n::format!(
            "r852ac5b610",
            count(unpriced.tokens),
            unpriced
                .models
                .iter()
                .map(|m| if m.is_empty() {
                    summon_i18n::text!("r0a9156766e")
                } else {
                    m.as_str()
                })
                .collect::<Vec<_>>()
                .join("、")
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::dashboard::api::testkit::*;
    use crate::runtime::turn_ledger;
    use serde_json::json;
    use summon_core::turn_ledger::{ModelUsage, OutsideUsage, TurnEnd, TurnOrigin, TurnRecord};

    fn record(chat_id: &str, ended_at_ms: i64, output: u64) -> TurnRecord {
        TurnRecord {
            agent: "starlight".into(),
            chat_id: chat_id.into(),
            tag: summon_core::registry::chat_tag("group", chat_id),
            turn: None,
            origin: TurnOrigin::Human,
            schedule: None,
            requester: None,
            message_id: "om_x".into(),
            prompt_head: "不该出现在汇总里".into(),
            started_at_ms: ended_at_ms - 10_000,
            ended_at_ms,
            end: TurnEnd::Done,
            error: None,
            reattached: false,
            tool_steps: 0,
            cli: Some("claude".into()),
            model: None,
            effort: None,
            ctx: None,
            usage: vec![ModelUsage {
                model: "claude-test-1".into(),
                requests: 1,
                input: 10,
                output,
                ..Default::default()
            }],
            quota: None,
            feed: None,
        }
    }

    /// CLI 与控制台用量页读同一份盘上数据,同一时间段的数逐项一致。
    #[test]
    fn the_cli_and_the_usage_page_agree_on_every_number() {
        let kit = Kit::new();
        let agent = kit.add_agent("starlight", &agent_yaml("starlight", "星光", "claude", ""));
        kit.add_chat(&agent, "group", GROUP, "示例群");
        let mut ctx = kit.owner();
        ctx.now =
            chrono::TimeZone::with_ymd_and_hms(&chrono::Local, 2026, 10, 20, 14, 32, 0).unwrap();
        let now = ctx.now.timestamp_millis();
        let run = kit.home().join("run");
        let agent_dir = turn_ledger::dir_in(&run).join("starlight");
        std::fs::create_dir_all(&agent_dir).unwrap();
        let lines: String = [
            record(GROUP, now - 60_000, 100),
            record("oc_other", now - 30_000, 5),
        ]
        .iter()
        .map(|r| serde_json::to_string(r).unwrap() + "\n")
        .collect();
        std::fs::write(
            agent_dir.join(summon_core::turn_ledger::file_name(now)),
            lines,
        )
        .unwrap();
        let outside = OutsideUsage {
            agent: "starlight".into(),
            chat_id: GROUP.into(),
            tag: summon_core::registry::chat_tag("group", GROUP),
            cli: "claude".into(),
            from_ms: now - 20_000,
            to_ms: now - 15_000,
            usage: vec![ModelUsage {
                model: "claude-test-1".into(),
                requests: 1,
                output: 40,
                ..Default::default()
            }],
        };
        turn_ledger::append_outside(&turn_ledger::outside_dir_in(&run), &outside).unwrap();
        crate::runtime::prices::refresh(&kit.home().join("run"), now, || {
            Ok(r#"{"claude-test-1": {"input_cost_per_token": 2e-06, "output_cost_per_token": 1e-05}}"#.into())
        })
        .unwrap();
        let page = kit
            .call(
                &ctx,
                crate::runtime::dashboard::api::usage::summary,
                json!({"range": "today"}),
            )
            .unwrap();
        let agents: HashMap<String, Agent> = [(agent.name.clone(), agent.clone())].into();
        let today = ctx.now.date_naive();
        let cli = build(
            &kit.home().join("run"),
            &agents,
            today,
            today,
            UsageBy::Chat,
        );
        let cli = serde_json::to_value(&cli).unwrap();
        assert_eq!(cli["totals"]["spend"], page["totals"]["spend"]);
        assert_eq!(cli["totals"]["outside"], page["totals"]["outside"]);
        assert_eq!(cli["rows"], page["by_chat"]);
        assert_eq!(cli["rows"][0]["label"], "示例群");
        assert_eq!(
            cli["rows"][0]["tag"],
            summon_core::registry::chat_tag("group", GROUP)
        );
        let days = build(&kit.home().join("run"), &agents, today, today, UsageBy::Day);
        assert_eq!(serde_json::to_value(&days).unwrap()["rows"], page["days"]);
        let text = render(&build(
            &kit.home().join("run"),
            &agents,
            today,
            today,
            UsageBy::Chat,
        ));
        assert!(text.contains("示例群 · starlight"), "{text}");
        assert!(text.contains("\n其中轮外用量"), "{text}");
        assert!(text.contains("拉取于 2026-"), "{text}");
        assert!(!text.contains("不该出现在汇总里"));
    }

    #[test]
    fn the_cli_names_insights_analysis_usage_like_the_usage_page() {
        let kit = Kit::new();
        let agent = kit.add_agent("starlight", &agent_yaml("starlight", "星光", "claude", ""));
        let now = Local::now().timestamp_millis();
        let run = kit.home().join("run");
        let row = OutsideUsage {
            agent: "starlight".into(),
            chat_id: String::new(),
            tag: crate::runtime::insights_analysis::SESSION_TAG.into(),
            cli: "claude".into(),
            from_ms: now - 1_000,
            to_ms: now,
            usage: vec![ModelUsage {
                model: "claude-test-1".into(),
                requests: 1,
                output: 9,
                ..Default::default()
            }],
        };
        turn_ledger::append_outside(&turn_ledger::outside_dir_in(&run), &row).unwrap();
        let agents: HashMap<String, Agent> = [(agent.name.clone(), agent.clone())].into();
        let today = Local::now().date_naive();
        let report = build(&run, &agents, today, today, UsageBy::Chat);
        let rows = serde_json::to_value(&report).unwrap()["rows"].clone();
        assert_eq!(rows[0]["label"], "洞察分析");
        assert_eq!(rows[0]["chat_id"], "");
        let text = render(&report);
        assert!(text.contains("洞察分析 · starlight"), "{text}");
    }

    #[test]
    fn counts_use_compact_english_units() {
        for (n, expected) in [
            (0, "0"),
            (999, "999"),
            (1_000, "1K"),
            (1_049, "1K"),
            (1_050, "1.1K"),
            (1_150, "1.2K"),
            (1_250, "1.3K"),
            (9_999, "10K"),
            (12_345, "12.3K"),
            (120_000, "120K"),
            (123_456, "123.5K"),
            (999_949, "999.9K"),
            (999_999, "1000K"),
            (1_000_000, "1M"),
            (1_250_000, "1.3M"),
            (12_345_678, "12.3M"),
            (420_000_000, "420M"),
            (999_999_999, "1000M"),
            (1_000_000_000, "1B"),
            (1_250_000_000, "1.3B"),
            (4_200_000_000, "4.2B"),
        ] {
            assert_eq!(count(n), expected, "count({n})");
        }
    }

    #[test]
    fn amounts_and_counts_are_written_like_the_console() {
        assert_eq!(usd(0.0), "$0");
        assert_eq!(usd(1234.567), "$1,234.57");
        assert_eq!(usd(0.5876), "$0.588");
        assert_eq!(usd(0.25), "$0.25");
        assert_eq!(usd(0.0012345), "$0.00123");
        let mut spend = Spend::default();
        spend.tokens.output = 10;
        spend.cost.output = 0.25;
        spend.cost.total = 0.25;
        assert_eq!(part(&spend, 0.25, 10), "$0.25 / 10");
        spend.unpriced = Some(summon_core::pricing::Unpriced {
            tokens: 5,
            models: vec!["x".into()],
        });
        spend.tokens.input = 5;
        assert_eq!(part(&spend, 0.25, 10), "$0.25 起 / 10");
        spend.unpriced = Some(summon_core::pricing::Unpriced {
            tokens: 15,
            models: vec!["x".into()],
        });
        assert_eq!(part(&spend, 0.0, 15), "未计价 / 15");
    }
}
