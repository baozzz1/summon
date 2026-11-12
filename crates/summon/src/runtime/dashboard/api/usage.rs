//! `usage.summary`:台账的汇总 —— 时间段内的轮数、耗时与结果分布,token 与按公开价格表算的费用,
//! 按 agent / 群 / CLI / 天的表,以及各 agent 轮末看到的额度。不含对话内容,进得来的人都能看。
//!
//! 分组与计价在 [`Report`] 里,`summon usage` 用的是同一份,两边的数逐项一致。

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;

use serde::Deserialize;
use serde_json::Value;
use summon_core::pricing::{self, PriceTable, Spend};
use summon_core::registry::Agent;
use summon_core::turn_ledger::{ModelUsage, OutsideUsage, TurnRecord};
use summon_core::turn_usage;

use super::chats::chat_names;
use super::sessions::{iso_ms, live_counts};
use super::types::{
    BucketUnit, PriceSource, UsageBucket, UsageDay, UsageQuota, UsageRange, UsageRow, UsageScope,
    UsageTotals, UsageView,
};
use super::{params, reply, Loaded};
use crate::runtime::dashboard::server::{ApiCtx, ApiError, ErrorCode};
use crate::runtime::turn_ledger;

#[derive(Deserialize)]
struct SummaryParams {
    range: UsageRange,
    from: Option<String>,
    to: Option<String>,
    hours: Option<u32>,
    agent: Option<String>,
    chat_id: Option<String>,
}

impl SummaryParams {
    fn matches(&self, agent: &str, chat: &str) -> bool {
        self.agent.as_deref().is_none_or(|a| a == agent)
            && self.chat_id.as_deref().is_none_or(|c| c == chat)
    }
}

/// 时间段对应的日期区间(本机日历,含两端)。`custom` 要给合法的 `from`/`to` 且不晚于今天,
/// 其余几种不许带 `from`/`to`;`all` 从 1970-01-01 起,由调用方按最早的记录收窄。
/// 洞察页与用量页共用这一份校验。
pub(crate) fn range_dates(
    range: UsageRange,
    from: Option<&str>,
    to: Option<&str>,
    now: &chrono::DateTime<chrono::Local>,
) -> Result<(chrono::NaiveDate, chrono::NaiveDate), ApiError> {
    use chrono::{Duration, NaiveDate};
    let today = now.date_naive();
    let invalid = || ApiError::new(ErrorCode::Invalid, summon_i18n::text!("r43a724ef96"));
    if range == UsageRange::Custom {
        let parse = |s: Option<&str>| -> Result<NaiveDate, ApiError> {
            let s = s.ok_or_else(invalid)?;
            let date = NaiveDate::parse_from_str(s, "%Y-%m-%d").map_err(|_| invalid())?;
            if date.to_string() != s {
                return Err(invalid());
            }
            Ok(date)
        };
        let from = parse(from)?;
        let to = parse(to)?;
        if from > to || from < NaiveDate::from_ymd_opt(1970, 1, 1).unwrap() || to > today {
            return Err(invalid());
        }
        return Ok((from, to));
    }
    if from.is_some() || to.is_some() {
        return Err(invalid());
    }
    let days = match range {
        UsageRange::Today => 1,
        UsageRange::Week => 7,
        UsageRange::Month => 30,
        UsageRange::Quarter => 90,
        UsageRange::All => return Ok((NaiveDate::from_ymd_opt(1970, 1, 1).unwrap(), today)),
        UsageRange::Custom => unreachable!(),
        UsageRange::Relative => return Err(invalid()),
    };
    Ok((today - Duration::days(days - 1), today))
}

/// 相对范围按固定小时数从查询时刻倒推;日历范围沿用本机日期边界。
pub(crate) fn range_window(
    range: UsageRange,
    from: Option<&str>,
    to: Option<&str>,
    hours: Option<u32>,
    now: &chrono::DateTime<chrono::Local>,
) -> Result<summon_core::insights::Window, ApiError> {
    use summon_core::insights::Window;
    let invalid = || {
        ApiError::new(
            ErrorCode::Invalid,
            summon_i18n::text!("range.relative.invalid"),
        )
    };
    if range == UsageRange::Relative {
        if from.is_some() || to.is_some() {
            return Err(invalid());
        }
        let hours = hours.filter(|h| *h > 0).ok_or_else(invalid)?;
        let until_ms = now.timestamp_millis();
        let since_ms = until_ms - i64::from(hours) * 3_600_000;
        if since_ms < 0 {
            return Err(invalid());
        }
        return Ok(Window { since_ms, until_ms });
    }
    if hours.is_some() {
        return Err(invalid());
    }
    let (from, to) = range_dates(range, from, to, now)?;
    let (starts, end) = turn_usage::days_between(&now.timezone(), from, to);
    Ok(Window {
        since_ms: starts.first().copied().unwrap_or(now.timestamp_millis()),
        until_ms: end.min(now.timestamp_millis() + 1),
    })
}

/// 没有轮末角标的轮(如换代续跑完的那一轮)不知道用的哪个 CLI,归在这一栏。
const UNKNOWN_CLI: &str = "r4d8c1c5b42";

/// 额度的最近一次读数至少往回找这么久,不随所选时间段变:「今天」还没跑过的 agent 也看得到额度
/// 还剩多少。走势只画所选时间段里的。
const QUOTA_LOOKBACK_MS: i64 = 7 * 24 * 3600 * 1000;

/// 一段时间里的运行记录与轮外用量:轮数与耗时只算轮,token 与费用两样都算。
pub(crate) struct Report<'a> {
    pub(crate) records: &'a [TurnRecord],
    pub(crate) outside: &'a [OutsideUsage],
    pub(crate) prices: Option<&'a PriceTable>,
    /// 显示名与群名从这里取;不在里面的 agent 显示别名、群显示 chat id。
    pub(crate) agents: &'a HashMap<String, Agent>,
}

/// `[since_ms, until_ms)` 里结束的轮与开始的轮外用量。
pub(crate) fn load_rows(
    run_dir: &Path,
    since_ms: i64,
    until_ms: i64,
) -> (Vec<TurnRecord>, Vec<OutsideUsage>) {
    (
        turn_ledger::read_range(&turn_ledger::dir_in(run_dir), None, since_ms, until_ms),
        turn_ledger::read_outside(
            &turn_ledger::outside_dir_in(run_dir),
            None,
            since_ms,
            until_ms,
        ),
    )
}

impl Report<'_> {
    fn display(&self, alias: &str) -> String {
        self.agents
            .get(alias)
            .map(|a| a.display.clone())
            .filter(|d| !d.is_empty())
            .unwrap_or_else(|| alias.to_string())
    }

    fn rows<K: Ord + Clone>(
        &self,
        key: impl Fn(&TurnRecord) -> K,
        outside_key: impl Fn(&OutsideUsage) -> K,
        row: impl Fn(&turn_usage::Group<'_, K>) -> UsageRow,
    ) -> Vec<UsageRow> {
        turn_usage::group_all(self.records, self.outside, key, outside_key)
            .into_iter()
            .map(|g| UsageRow {
                stats: turn_usage::stats(g.records.iter().copied()),
                spend: pricing::spend(self.prices, g.usage()),
                ..row(&g)
            })
            .collect()
    }

    /// 全部的 token 与费用。
    pub(crate) fn spend(&self) -> Option<Spend> {
        pricing::spend(
            self.prices,
            self.records
                .iter()
                .flat_map(|r| &r.usage)
                .chain(self.outside.iter().flat_map(|o| &o.usage)),
        )
    }

    /// 其中轮外用量的那部分。
    pub(crate) fn outside_spend(&self) -> Option<Spend> {
        pricing::spend(self.prices, self.outside.iter().flat_map(|o| &o.usage))
    }

    pub(crate) fn by_agent(&self) -> Vec<UsageRow> {
        self.rows(
            |r| r.agent.clone(),
            |o| o.agent.clone(),
            |g| UsageRow {
                key: g.key.clone(),
                label: self.display(&g.key),
                agent: Some(g.key.clone()),
                ..UsageRow::default()
            },
        )
    }

    pub(crate) fn by_chat(&self) -> Vec<UsageRow> {
        let names: HashMap<&str, HashMap<String, String>> = self
            .agents
            .iter()
            .map(|(alias, agent)| (alias.as_str(), chat_names(agent)))
            .collect();
        self.rows(
            |r| (r.agent.clone(), r.chat_id.clone()),
            |o| (o.agent.clone(), o.chat_id.clone()),
            |g| {
                let (alias, chat_id) = &g.key;
                let label = names
                    .get(alias.as_str())
                    .and_then(|n| n.get(chat_id).cloned())
                    .unwrap_or_else(|| chat_id.clone());
                let tag = g
                    .records
                    .first()
                    .map(|r| r.tag.clone())
                    .or_else(|| g.outside.first().map(|o| o.tag.clone()));
                UsageRow {
                    key: format!("{alias}/{chat_id}"),
                    label,
                    agent: Some(alias.clone()),
                    chat_id: Some(chat_id.clone()),
                    tag,
                    ..UsageRow::default()
                }
            },
        )
    }

    pub(crate) fn by_cli(&self) -> Vec<UsageRow> {
        self.rows(
            |r| {
                r.cli
                    .clone()
                    .unwrap_or_else(|| ::summon_core::i18n::label(UNKNOWN_CLI).to_string())
            },
            |o| o.cli.clone(),
            |g| UsageRow {
                key: g.key.clone(),
                label: g.key.clone(),
                ..UsageRow::default()
            },
        )
    }

    /// 同一轮可涉及多个模型,各模型下只计一次轮数;费用只取该模型的请求。
    pub(crate) fn by_model(&self) -> Vec<UsageRow> {
        #[derive(Default)]
        struct ModelGroup<'a> {
            records: Vec<&'a TurnRecord>,
            usage: Vec<&'a ModelUsage>,
        }
        let mut groups: BTreeMap<&str, ModelGroup<'_>> = BTreeMap::new();
        for record in self.records {
            let mut models = BTreeSet::new();
            for usage in &record.usage {
                groups.entry(&usage.model).or_default().usage.push(usage);
                models.insert(usage.model.as_str());
            }
            if models.is_empty() {
                models.insert(record.model.as_deref().unwrap_or_default());
            }
            for model in models {
                groups.entry(model).or_default().records.push(record);
            }
        }
        for outside in self.outside {
            for usage in &outside.usage {
                groups.entry(&usage.model).or_default().usage.push(usage);
            }
        }
        let mut rows: Vec<UsageRow> = groups
            .into_iter()
            .map(|(model, group)| UsageRow {
                key: model.to_string(),
                label: if model.is_empty() {
                    summon_i18n::text!("r0a9156766e").into()
                } else {
                    model.into()
                },
                stats: turn_usage::stats(group.records),
                spend: pricing::spend(self.prices, group.usage),
                ..UsageRow::default()
            })
            .collect();
        rows.sort_by(|a, b| {
            b.stats
                .turns
                .cmp(&a.stats.turns)
                .then_with(|| a.key.cmp(&b.key))
        });
        rows
    }

    /// 按天:`day_starts` 升序,最后一天到 `until_ms` 为止;只留有轮或有轮外用量的日子,新的在前。
    pub(crate) fn by_day(&self, day_starts: &[i64], until_ms: i64) -> Vec<UsageDay> {
        let records = turn_usage::bucket_records(self.records, day_starts, until_ms);
        let outside = turn_usage::bucket_outside(self.outside, day_starts, until_ms);
        let mut days: Vec<UsageDay> = day_starts
            .iter()
            .zip(records.into_iter().zip(outside))
            .filter(|(_, (rs, os))| !rs.is_empty() || !os.is_empty())
            .map(|(start, (rs, os))| UsageDay {
                start: iso_ms(*start).unwrap_or_default(),
                turns: u32::try_from(rs.len()).unwrap_or(u32::MAX),
                spend: pricing::spend(
                    self.prices,
                    rs.iter()
                        .flat_map(|r| &r.usage)
                        .chain(os.iter().flat_map(|o| &o.usage)),
                ),
            })
            .collect();
        days.reverse();
        days
    }
}

pub(crate) fn summary(ctx: &ApiCtx, p: Value) -> Result<Value, ApiError> {
    let env = ctx.env()?;
    let query: SummaryParams = params(p)?;
    let range = query.range;
    let now = ctx.now;
    let window = range_window(
        range,
        query.from.as_deref(),
        query.to.as_deref(),
        query.hours,
        &now,
    )?;
    let now_ms = now.timestamp_millis();
    let date_of = |ms| {
        chrono::DateTime::from_timestamp_millis(ms)
            .unwrap()
            .with_timezone(&now.timezone())
            .date_naive()
    };
    let mut from = date_of(window.since_ms);
    let to = date_of(window.until_ms - 1);
    let mut day_starts = turn_usage::days_between(&now.timezone(), from, to).0;
    let until = window.until_ms;
    let mut since = window.since_ms;
    let dir = turn_ledger::dir_in(&env.run_dir());
    let (mut records, mut outside) = load_rows(&env.run_dir(), since, until);
    if range == UsageRange::All {
        let first = records
            .iter()
            .map(|r| r.ended_at_ms)
            .chain(outside.iter().map(|r| r.from_ms))
            .min()
            .unwrap_or(now_ms);
        from = chrono::DateTime::from_timestamp_millis(first)
            .unwrap_or(now.to_utc())
            .with_timezone(&now.timezone())
            .date_naive();
        day_starts = turn_usage::days_between(&now.timezone(), from, to).0;
        since = day_starts.first().copied().unwrap_or(now_ms);
    }
    let (unit, starts) = if range == UsageRange::Today {
        (BucketUnit::Hour, turn_usage::hour_starts(&now))
    } else if range == UsageRange::Relative && query.hours.is_some_and(|h| h <= 48) {
        use chrono::Timelike;
        let at = chrono::DateTime::from_timestamp_millis(since)
            .unwrap()
            .with_timezone(&now.timezone());
        let hour = since
            - i64::from(at.minute() * 60 + at.second()) * 1000
            - i64::from(at.timestamp_subsec_millis());
        (BucketUnit::Hour, (hour..until).step_by(3_600_000).collect())
    } else {
        (BucketUnit::Day, day_starts.clone())
    };
    let table = crate::runtime::prices::load(&env.run_dir());
    let prices = table.as_deref();

    let agents: HashMap<String, Agent> = env
        .scan()
        .into_iter()
        .filter_map(|loaded| match loaded {
            Loaded::Ok(agent) => Some((agent.name.clone(), *agent)),
            Loaded::Broken { .. } => None,
        })
        .collect();
    let scopes = Report {
        records: &records,
        outside: &outside,
        prices,
        agents: &agents,
    }
    .by_chat()
    .into_iter()
    .map(|row| {
        let agent = row.agent.unwrap_or_default();
        let agent_label = agents
            .get(&agent)
            .map(|a| a.display.clone())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| agent.clone());
        UsageScope {
            agent,
            agent_label,
            chat_id: row.chat_id.unwrap_or_default(),
            chat_label: row.label,
        }
    })
    .collect();
    records.retain(|r| query.matches(&r.agent, &r.chat_id));
    outside.retain(|r| query.matches(&r.agent, &r.chat_id));
    let report = Report {
        records: &records,
        outside: &outside,
        prices,
        agents: &agents,
    };
    let display = |alias: &str| report.display(alias);

    let total = turn_usage::stats(&records);
    let live = if query.agent.is_none() && query.chat_id.is_none() {
        live_counts(env, ctx)
    } else {
        None
    };
    let buckets = starts
        .iter()
        .zip(turn_usage::bucket_counts(&records, &starts, until))
        .map(|(start, counts)| UsageBucket {
            start: iso_ms(*start).unwrap_or_default(),
            counts,
        })
        .collect();
    let by_day = report.by_day(&day_starts, until);

    // 额度按 agent 列:配了自己账号的 agent 各是一份,共用默认账号的几个 agent 读数相同。
    let quota_since = since.min(now_ms - QUOTA_LOOKBACK_MS);
    let lookback = if quota_since < since {
        turn_ledger::read_range(&dir, None, quota_since, until)
    } else {
        records.clone()
    };
    let mut per_agent: BTreeMap<&str, Vec<&TurnRecord>> = BTreeMap::new();
    for record in lookback
        .iter()
        .filter(|r| query.matches(&r.agent, &r.chat_id))
    {
        per_agent.entry(&record.agent).or_default().push(record);
    }
    let quota = per_agent
        .into_iter()
        .filter_map(|(alias, rs)| {
            let latest = turn_usage::quota_series(rs.iter().copied())?;
            let trend =
                turn_usage::quota_series(rs.iter().copied().filter(|r| r.ended_at_ms >= since));
            Some(UsageQuota {
                agent: alias.to_string(),
                label: display(alias),
                // 台账从新到旧:第一条带 CLI 的就是最近用的那个。
                cli: rs.iter().find_map(|r| r.cli.clone()),
                at: iso_ms(latest.at_ms).unwrap_or_default(),
                latest: latest.latest,
                points: trend.map(|s| s.points).unwrap_or_default(),
            })
        })
        .collect();

    reply(&UsageView {
        range,
        scopes,
        today: now.date_naive().to_string(),
        unit,
        since: iso_ms(since).unwrap_or_default(),
        until: iso_ms(if range == UsageRange::Relative {
            until
        } else {
            until - 1
        })
        .unwrap_or_default(),
        totals: UsageTotals {
            turns: total.turns,
            seconds: total.seconds,
            failed: total.failed,
            interrupted: total.interrupted,
            spend: report.spend(),
            outside: report.outside_spend(),
            working: live.map(|(working, _)| working),
            waiting: live.map(|(_, waiting)| waiting),
        },
        buckets,
        by_agent: report.by_agent(),
        by_chat: report.by_chat(),
        by_cli: report.by_cli(),
        by_model: report.by_model(),
        days: by_day,
        quota,
        prices: prices.map(|t| PriceSource {
            source: t.source.clone(),
            fetched_at: iso_ms(t.fetched_at_ms).unwrap_or_default(),
        }),
    })
}

#[cfg(test)]
mod tests {
    use super::super::testkit::*;
    use super::*;
    use serde_json::json;
    use summon_core::turn_ledger::{ModelUsage, OutsideUsage, TurnEnd, TurnOrigin, TurnQuota};

    fn record(agent: &str, chat_id: &str, ended_at_ms: i64, secs: i64, end: TurnEnd) -> TurnRecord {
        TurnRecord {
            agent: agent.into(),
            chat_id: chat_id.into(),
            tag: summon_core::registry::chat_tag("group", chat_id),
            turn: None,
            origin: TurnOrigin::Human,
            schedule: None,
            requester: None,
            message_id: "om_x".into(),
            prompt_head: "不该出现在汇总里".into(),
            started_at_ms: ended_at_ms - secs * 1000,
            ended_at_ms,
            end,
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

    fn write(kit: &Kit, records: &[TurnRecord]) {
        for r in records {
            let dir = kit.home().join("run/turns").join(&r.agent);
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join(summon_core::turn_ledger::file_name(r.ended_at_ms));
            let mut text = std::fs::read_to_string(&path).unwrap_or_default();
            text.push_str(&serde_json::to_string(r).unwrap());
            text.push('\n');
            std::fs::write(path, text).unwrap();
        }
    }

    #[test]
    fn relative_ranges_filter_exact_boundaries_and_refresh_from_now() {
        let kit = Kit::new();
        let mut ctx = kit.owner();
        ctx.now = chrono::DateTime::parse_from_rfc3339("2026-11-04T00:17:29.123+08:00")
            .unwrap()
            .with_timezone(&chrono::Local);
        let end = ctx.now.timestamp_millis();
        let start = end - 24 * 3_600_000;
        write(
            &kit,
            &[start - 1, start, end - 1, end, end + 1]
                .map(|at| record("starlight", GROUP, at, 1, TurnEnd::Done)),
        );
        write_outside(
            &kit,
            &[start - 1, start, end - 1, end].map(|at| outside_row(GROUP, at, 1)),
        );
        let query = json!({"range":"relative", "hours":24});
        let out = kit.call(&ctx, summary, query.clone()).unwrap();
        assert_eq!(out["totals"]["turns"], 2);
        assert_eq!(out["totals"]["outside"]["tokens"]["output"], 2);
        assert_eq!(
            chrono::DateTime::parse_from_rfc3339(out["since"].as_str().unwrap())
                .unwrap()
                .timestamp_millis(),
            start
        );
        assert_eq!(
            chrono::DateTime::parse_from_rfc3339(out["until"].as_str().unwrap())
                .unwrap()
                .timestamp_millis(),
            end
        );
        assert_eq!(out["unit"], "hour");
        assert_eq!(out["buckets"].as_array().unwrap().len(), 25);
        assert_eq!(
            out["buckets"]
                .as_array()
                .unwrap()
                .iter()
                .map(|b| b["done"].as_u64().unwrap())
                .sum::<u64>(),
            2
        );
        ctx.now += chrono::Duration::hours(1);
        let refreshed = kit.call(&ctx, summary, query).unwrap();
        assert_eq!(refreshed["totals"]["turns"], 3);
        assert_ne!(refreshed["since"], out["since"]);
        let week = range_window(UsageRange::Relative, None, None, Some(168), &ctx.now).unwrap();
        assert_eq!(week.until_ms - week.since_ms, 7 * 24 * 3_600_000);
    }

    #[test]
    fn relative_queries_reject_invalid_or_mixed_durations() {
        let kit = Kit::new();
        let ctx = kit.owner();
        for query in [
            json!({"range":"relative"}),
            json!({"range":"relative","hours":0}),
            json!({"range":"relative","hours":-1}),
            json!({"range":"relative","hours":1.5}),
            json!({"range":"relative","hours":4294967295_u32}),
            json!({"range":"relative","hours":24,"from":"2026-11-01"}),
            json!({"range":"relative","hours":24,"to":"2026-11-01"}),
            json!({"range":"7d","hours":24}),
        ] {
            assert_eq!(
                kit.call(&ctx, summary, query.clone()).unwrap_err().code,
                ErrorCode::Invalid,
                "{query}"
            );
            assert_eq!(
                kit.call(&ctx, super::super::insights::summary, query.clone())
                    .unwrap_err()
                    .code,
                ErrorCode::Invalid,
                "{query}"
            );
        }
    }

    #[test]
    fn custom_dates_include_both_days_and_filter_turns_and_outside_together() {
        use chrono::TimeZone;
        let kit = Kit::new();
        let mut ctx = kit.owner();
        ctx.now = chrono::Local
            .with_ymd_and_hms(2026, 11, 4, 14, 0, 0)
            .unwrap();
        let start = chrono::Local
            .with_ymd_and_hms(2026, 10, 31, 0, 0, 0)
            .unwrap()
            .timestamp_millis();
        let end = chrono::Local
            .with_ymd_and_hms(2026, 11, 2, 0, 0, 0)
            .unwrap()
            .timestamp_millis();
        let mut rows = Vec::new();
        for (agent, chat, at, tokens) in [
            ("starlight", GROUP, start - 1, 1),
            ("starlight", GROUP, start, 10),
            ("starlight", GROUP, end - 1, 20),
            ("starlight", GROUP, end, 2),
            ("orbit", GROUP, start, 30),
            ("starlight", "oc_other", start, 40),
        ] {
            let mut r = record(agent, chat, at, 1, TurnEnd::Done);
            r.usage = usage(tokens);
            rows.push(r);
        }
        write(&kit, &rows);
        write_outside(
            &kit,
            &[
                outside_row(GROUP, start - 1, 1),
                outside_row(GROUP, start, 5),
                outside_row(GROUP, end - 1, 7),
                outside_row(GROUP, end, 2),
                outside_row("oc_other", start, 50),
            ],
        );
        crate::runtime::prices::refresh(
            &kit.home().join("run"),
            ctx.now.timestamp_millis(),
            || {
                Ok(
                    r#"{"claude-test-1":{"input_cost_per_token":0,"output_cost_per_token":0.01}}"#
                        .into(),
                )
            },
        )
        .unwrap();
        let out = kit.call(&ctx, summary, json!({"range":"custom", "from":"2026-10-31", "to":"2026-11-01", "agent":"starlight", "chat_id":GROUP})).unwrap();
        assert_eq!(out["totals"]["turns"], 2);
        assert_eq!(out["totals"]["spend"]["tokens"]["output"], 42);
        assert_eq!(out["totals"]["outside"]["tokens"]["output"], 12);
        assert!((out["totals"]["spend"]["cost"]["total"].as_f64().unwrap() - 0.42).abs() < 1e-12);
        assert_eq!(out["buckets"].as_array().unwrap().len(), 2);
        assert!(out["until"]
            .as_str()
            .unwrap()
            .starts_with("2026-11-01T23:59:59"));
        for key in ["by_agent", "by_chat", "by_cli", "by_model"] {
            assert_eq!(out[key].as_array().unwrap().len(), 1, "{key}");
            assert_eq!(out[key][0]["spend"]["tokens"]["output"], 42, "{key}");
        }
        assert_eq!(out["scopes"].as_array().unwrap().len(), 3);
        assert_eq!(out["days"][0]["spend"]["tokens"]["output"], 27);
        let group = kit
            .call(
                &ctx,
                summary,
                json!({"range":"custom", "from":"2026-10-31", "to":"2026-11-01", "chat_id":GROUP}),
            )
            .unwrap();
        assert_eq!(group["totals"]["spend"]["tokens"]["output"], 72);
        let empty = kit
            .call(&ctx, summary, json!({"range":"7d", "agent":"missing"}))
            .unwrap();
        assert_eq!(empty["totals"]["turns"], 0);
        assert!(empty["totals"].get("spend").is_none());
    }

    #[test]
    fn ranges_validate_dates_and_all_includes_the_oldest_outside_record() {
        use chrono::TimeZone;
        let kit = Kit::new();
        let mut ctx = kit.owner();
        ctx.now = chrono::Local
            .with_ymd_and_hms(2026, 11, 4, 14, 0, 0)
            .unwrap();
        for p in [
            json!({"range":"custom"}),
            json!({"range":"custom", "from":"2026-11-02", "to":"2026-11-01"}),
            json!({"range":"custom", "from":"2026-02-30", "to":"2026-11-01"}),
            json!({"range":"custom", "from":"2026-01-01", "to":"2026-11-05"}),
            json!({"range":"custom", "from":"1969-12-31", "to":"2026-11-01"}),
            json!({"range":"7d", "from":"2026-11-01"}),
        ] {
            assert_eq!(
                kit.call(&ctx, summary, p).unwrap_err().code,
                ErrorCode::Invalid
            );
        }
        for (from, to, count) in [
            ("2025-12-31", "2026-01-01", 2),
            ("2024-02-28", "2024-03-01", 3),
            ("2026-11-01", "2026-11-01", 1),
        ] {
            let out = kit
                .call(
                    &ctx,
                    summary,
                    json!({"range":"custom", "from":from, "to":to}),
                )
                .unwrap();
            assert_eq!(out["buckets"].as_array().unwrap().len(), count);
        }
        let out = kit.call(&ctx, summary, json!({"range":"90d"})).unwrap();
        assert_eq!(out["buckets"].as_array().unwrap().len(), 90);
        let at = chrono::Local
            .with_ymd_and_hms(2026, 7, 1, 12, 0, 0)
            .unwrap()
            .timestamp_millis();
        write_outside(&kit, &[outside_row(GROUP, at, 17)]);
        let all = kit.call(&ctx, summary, json!({"range":"all"})).unwrap();
        assert!(all["since"].as_str().unwrap().starts_with("2026-07-01"));
        assert_eq!(all["totals"]["spend"]["tokens"]["output"], 17);
    }

    #[test]
    fn summary_counts_the_range_and_groups_by_agent_chat_and_cli() {
        let kit = Kit::new();
        let agent = kit.add_agent("starlight", &agent_yaml("starlight", "星光", "claude", ""));
        kit.add_chat(&agent, "group", GROUP, "示例群");
        // 钉在一天的中午:离零点远,几分钟前的轮都落在今天这个桶里。
        let mut ctx = kit.owner();
        ctx.now =
            chrono::TimeZone::with_ymd_and_hms(&chrono::Local, 2026, 10, 20, 14, 32, 0).unwrap();
        let now = ctx.now.timestamp_millis();
        let mut codex = record("orbit", "oc_other", now - 60_000, 30, TurnEnd::Failed);
        codex.cli = Some("codex".into());
        codex.usage = vec![ModelUsage {
            model: "gpt-test-1".into(),
            requests: 3,
            input: 1_000,
            output: 200,
            cache_read: 4_000,
            ..Default::default()
        }];
        let mut quota = record("starlight", GROUP, now - 30_000, 10, TurnEnd::Done);
        quota.quota = Some(TurnQuota {
            five_hour_left: Some(56),
            long_left: Some(83),
            ..Default::default()
        });
        let mut earlier = record(
            "vega",
            "oc_vega",
            now - 3 * 24 * 3600 * 1000,
            5,
            TurnEnd::Done,
        );
        earlier.quota = Some(TurnQuota {
            five_hour_left: Some(40),
            ..Default::default()
        });
        write(
            &kit,
            &[
                quota,
                earlier,
                record("starlight", GROUP, now - 120_000, 20, TurnEnd::Interrupted),
                codex,
                // 31 天前:哪个时间段都不算
                record(
                    "starlight",
                    GROUP,
                    now - 31 * 24 * 3600 * 1000,
                    5,
                    TurnEnd::Done,
                ),
            ],
        );
        let out = kit.call(&ctx, summary, json!({"range": "30d"})).unwrap();
        assert_eq!(out["unit"], "day");
        assert_eq!(out["buckets"].as_array().unwrap().len(), 30);
        assert_eq!(out["totals"]["turns"], 4);
        assert_eq!(out["totals"]["failed"], 1);
        assert_eq!(out["totals"]["interrupted"], 1);
        assert_eq!(out["totals"]["seconds"], 65);
        let last = out["buckets"].as_array().unwrap().last().unwrap().clone();
        assert_eq!(last["done"], 1);
        assert_eq!(last["failed"], 1);
        assert_eq!(out["by_agent"][0]["key"], "starlight");
        assert_eq!(out["by_agent"][0]["label"], "星光");
        assert_eq!(out["by_agent"][0]["turns"], 2);
        assert_eq!(out["by_chat"][0]["label"], "示例群");
        assert_eq!(out["by_chat"][0]["chat_id"], GROUP);
        let codex_row = out["by_cli"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["key"] == "codex")
            .unwrap()
            .clone();
        assert_eq!(codex_row["spend"]["tokens"]["input"], 1_000);
        // 还没有价格表:token 照计,费用全部未计价
        assert!(out.get("prices").is_none());
        assert_eq!(codex_row["spend"]["cost"]["total"], 0.0);
        assert_eq!(codex_row["spend"]["unpriced"]["tokens"], 5_200);
        assert_eq!(
            codex_row["spend"]["unpriced"]["models"],
            json!(["gpt-test-1"])
        );
        // 没报 token 的分组不带 spend
        assert!(out["by_agent"][0].get("spend").is_none());
        // 按天:有轮结束的那两天,新的在前
        let days = out["days"].as_array().unwrap();
        assert_eq!(days.len(), 2);
        assert_eq!(days[0]["turns"], 3);
        assert_eq!(days[0]["spend"]["tokens"]["output"], 200);
        assert_eq!(days[1]["turns"], 1);
        assert!(days[1].get("spend").is_none());
        assert_eq!(out["quota"].as_array().unwrap().len(), 2);
        assert_eq!(out["quota"][0]["agent"], "starlight");
        assert_eq!(out["quota"][0]["five_hour_left"], 56);
        assert!(!out.to_string().contains("不该出现在汇总里"));

        let run = kit.home().join("run");
        crate::runtime::prices::refresh(&run, now - 1_000, || {
            Ok(
                r#"{"gpt-test-1": {"input_cost_per_token": 2e-06, "output_cost_per_token": 1e-05,
                "cache_read_input_token_cost": 1e-07}}"#
                    .into(),
            )
        })
        .unwrap();
        let today = kit.call(&ctx, summary, json!({"range": "today"})).unwrap();
        assert_eq!(today["unit"], "hour");
        assert_eq!(today["totals"]["turns"], 3);
        assert_eq!(today["days"].as_array().unwrap().len(), 1);
        let cost = &today["totals"]["spend"]["cost"];
        let close = |v: &Value, want: f64| (v.as_f64().unwrap() - want).abs() < 1e-12;
        assert!(close(&cost["input"], 1_000.0 * 2e-6));
        assert!(close(&cost["output"], 200.0 * 1e-5));
        assert!(close(&cost["cache_read"], 4_000.0 * 1e-7));
        assert!(close(&cost["total"], 0.002 + 0.002 + 0.0004));
        assert!(today["totals"]["spend"].get("unpriced").is_none());
        assert!(today["prices"]["source"]
            .as_str()
            .unwrap()
            .contains("litellm"));
        let vega = today["quota"]
            .as_array()
            .unwrap()
            .iter()
            .find(|q| q["agent"] == "vega")
            .unwrap()
            .clone();
        assert_eq!(vega["five_hour_left"], 40);
        assert_eq!(vega["points"], json!([]));
        let bad = kit.call(&ctx, summary, json!({"range": "1y"})).unwrap_err();
        assert_eq!(
            bad.code,
            crate::runtime::dashboard::server::ErrorCode::Invalid
        );
    }

    #[test]
    fn models_count_each_turn_once_and_split_usage_including_outside() {
        let mut mixed = record("starlight", GROUP, 100_000, 30, TurnEnd::Failed);
        mixed.model = Some("configured-model".into());
        mixed.usage = vec![
            ModelUsage {
                model: "model-a".into(),
                output: 10,
                ..Default::default()
            },
            ModelUsage {
                model: "model-a".into(),
                band: 200,
                output: 20,
                ..Default::default()
            },
            ModelUsage {
                model: "model-b".into(),
                output: 40,
                ..Default::default()
            },
            ModelUsage {
                model: String::new(),
                output: 5,
                ..Default::default()
            },
        ];
        let mut fallback = record("starlight", GROUP, 110_000, 10, TurnEnd::Done);
        fallback.model = Some("model-a".into());
        let unknown = record("starlight", GROUP, 120_000, 20, TurnEnd::Done);
        let records = [mixed, fallback, unknown];
        let mut outside = outside_row(GROUP, 130_000, 7);
        outside.usage[0].model = "model-b".into();
        let mut only_outside = outside.clone();
        only_outside.usage[0].model = "model-c".into();
        let outside = [outside, only_outside];
        let agents = HashMap::new();
        let prices = PriceTable::from_litellm(r#"{"model-a":{"input_cost_per_token":0.0,"output_cost_per_token":0.01},"model-b":{"input_cost_per_token":0.0,"output_cost_per_token":0.02}}"#, "test", 0).unwrap();
        let report = Report {
            records: &records,
            outside: &outside,
            prices: Some(&prices),
            agents: &agents,
        };
        let rows = report.by_model();
        let find = |key: &str| rows.iter().find(|r| r.key == key).unwrap();
        assert_eq!(rows.len(), 4);
        let a = find("model-a");
        assert_eq!(a.stats.turns, 2);
        assert_eq!(a.stats.failed, 1);
        assert_eq!(a.stats.avg_secs, 20);
        assert_eq!(a.stats.p90_secs, 30);
        assert_eq!(a.spend.as_ref().unwrap().tokens.output, 30);
        assert!((a.spend.as_ref().unwrap().cost.total - 0.3).abs() < 1e-12);
        let b = find("model-b");
        assert_eq!(b.stats.turns, 1);
        assert_eq!(b.spend.as_ref().unwrap().tokens.output, 47);
        assert!((b.spend.as_ref().unwrap().cost.total - 0.94).abs() < 1e-12);
        assert_eq!(find("model-c").stats.turns, 0);
        assert_eq!(find("").label, "未知模型");
        assert_eq!(find("").stats.turns, 2);
        assert_eq!(
            find("")
                .spend
                .as_ref()
                .unwrap()
                .unpriced
                .as_ref()
                .unwrap()
                .tokens,
            5
        );
        assert_eq!(
            rows.iter()
                .map(|r| r.spend.as_ref().map_or(0, |s| s.tokens.output))
                .sum::<u64>(),
            report.spend().unwrap().tokens.output
        );
        assert!(Report {
            records: &[],
            outside: &[],
            prices: None,
            agents: &agents
        }
        .by_model()
        .is_empty());
    }

    fn usage(output: u64) -> Vec<ModelUsage> {
        vec![ModelUsage {
            model: "claude-test-1".into(),
            requests: 1,
            output,
            ..Default::default()
        }]
    }

    fn write_outside(kit: &Kit, rows: &[OutsideUsage]) {
        let dir = turn_ledger::outside_dir_in(&kit.home().join("run"));
        for row in rows {
            turn_ledger::append_outside(&dir, row).unwrap();
        }
    }

    fn outside_row(chat_id: &str, from_ms: i64, output: u64) -> OutsideUsage {
        OutsideUsage {
            agent: "starlight".into(),
            chat_id: chat_id.into(),
            tag: summon_core::registry::chat_tag("group", chat_id),
            cli: "claude".into(),
            from_ms,
            to_ms: from_ms + 5_000,
            usage: usage(output),
        }
    }

    #[test]
    fn outside_usage_counts_in_tokens_and_cost_but_not_in_turns() {
        let kit = Kit::new();
        let agent = kit.add_agent("starlight", &agent_yaml("starlight", "星光", "claude", ""));
        kit.add_chat(&agent, "group", GROUP, "示例群");
        let mut ctx = kit.owner();
        ctx.now =
            chrono::TimeZone::with_ymd_and_hms(&chrono::Local, 2026, 10, 20, 14, 32, 0).unwrap();
        let now = ctx.now.timestamp_millis();
        let mut turn = record("starlight", GROUP, now - 60_000, 30, TurnEnd::Done);
        turn.usage = usage(100);
        write(&kit, &[turn]);
        write_outside(
            &kit,
            &[
                outside_row(GROUP, now - 30_000, 40),
                outside_row("oc_only_outside", now - 20_000, 7),
                // 前一天的:落在前一天
                outside_row(GROUP, now - 24 * 3600 * 1000, 3),
            ],
        );
        let out = kit.call(&ctx, summary, json!({"range": "7d"})).unwrap();
        assert_eq!(out["totals"]["turns"], 1);
        assert_eq!(out["totals"]["spend"]["tokens"]["output"], 150);
        assert_eq!(out["totals"]["outside"]["tokens"]["output"], 50);
        let chats: Vec<(String, u64, u64)> = out["by_chat"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| {
                (
                    r["label"].as_str().unwrap().to_string(),
                    r["turns"].as_u64().unwrap(),
                    r["spend"]["tokens"]["output"].as_u64().unwrap(),
                )
            })
            .collect();
        assert_eq!(
            chats,
            [
                ("示例群".to_string(), 1, 143),
                ("oc_only_outside".to_string(), 0, 7)
            ]
        );
        assert_eq!(out["by_cli"][0]["spend"]["tokens"]["output"], 150);
        let days = out["days"].as_array().unwrap();
        assert_eq!(days.len(), 2);
        assert_eq!(
            (
                days[0]["turns"].as_u64(),
                days[0]["spend"]["tokens"]["output"].as_u64()
            ),
            (Some(1), Some(147))
        );
        assert_eq!(
            (
                days[1]["turns"].as_u64(),
                days[1]["spend"]["tokens"]["output"].as_u64()
            ),
            (Some(0), Some(3))
        );
    }
}
