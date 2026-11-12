//! `insights.summary`:人在 agent 环路里的等待 —— 回复落地到下一条消息的间隔分布、长间隔期间的
//! 别处活动、下一条消息的类型、每日输入量与按规则得出的结论。只有计数与时长,不含消息内容与
//! 发消息的人,进得来的人都能看(与用量页相同)。
//!
//! 口径与汇总在 [`summon_core::insights`] 里;`summon insights` 用的是同一份 [`build`],两边的数
//! 逐项一致。

use std::path::Path;

use chrono::{DateTime, Duration, Local, NaiveDate};
use serde::Deserialize;
use serde_json::Value;
use summon_core::insights::{self, Query, Window};
use summon_core::turn_usage;

use super::types::{
    AnalysisPrimary, AnalysisResult, AnalysisRetryFailure, AnalysisStatus, AnalysisView,
    InsightsView, UsageRange,
};
use super::usage::range_window;
use super::{internal, outcome, params, reply, rfc3339, ApiEnv};
use crate::runtime::dashboard::server::{ApiCtx, ApiError, ErrorCode};
use crate::runtime::insights_analysis::{self as analysis, Plan, PrimaryAgent, State};
use crate::runtime::turn_ledger;
use summon_core::dashboard::authz::Target;
use summon_core::i18n::Language;

/// 往时间段之前多读这么久的运行记录:成对要看到与时间段里第一轮相邻的那一轮。
const LOOKBEHIND_MS: i64 = 24 * 3600 * 1000;

#[derive(Deserialize)]
struct SummaryParams {
    range: UsageRange,
    from: Option<String>,
    to: Option<String>,
    hours: Option<u32>,
    agent: Option<String>,
}

/// `from`..=`to`(本机日历)这几天的洞察,截至此刻。`all` 从最早一条运行记录那天起,不做对比;
/// 其余与紧邻的、天数相同的上一时段对比。运行记录读到此刻为止:过夜的间隔要看到下一条消息。
pub(crate) fn build(
    run_dir: &Path,
    now: &DateTime<Local>,
    range: UsageRange,
    from: NaiveDate,
    to: NaiveDate,
    agent: Option<&str>,
) -> InsightsView {
    let (starts, end) = turn_usage::days_between(&now.timezone(), from, to);
    let window = Window {
        since_ms: starts.first().copied().unwrap_or(now.timestamp_millis()),
        until_ms: end.min(now.timestamp_millis() + 1),
    };
    build_window(run_dir, now, range, window, agent)
}

fn build_window(
    run_dir: &Path,
    now: &DateTime<Local>,
    range: UsageRange,
    mut window: Window,
    agent: Option<&str>,
) -> InsightsView {
    let tz = now.timezone();
    let now_ms = now.timestamp_millis();
    let date_of = |ms| {
        DateTime::from_timestamp_millis(ms)
            .unwrap()
            .with_timezone(&tz)
            .date_naive()
    };
    let mut from = date_of(window.since_ms);
    let to = date_of(window.until_ms - 1);
    let window_of = |from, to| {
        let (starts, end) = turn_usage::days_between(&tz, from, to);
        Window {
            since_ms: starts[0],
            until_ms: end.min(now_ms + 1),
        }
    };
    let dir = turn_ledger::dir_in(run_dir);
    let read =
        |since_ms: i64| turn_ledger::read_range(&dir, None, since_ms - LOOKBEHIND_MS, now_ms + 1);
    let (records, previous) = if range == UsageRange::All {
        let records = read(window.since_ms);
        // 与用量页一样,按全部 agent 里最早的一条定起点;一条都没有就是今天。
        let first = records
            .iter()
            .map(|r| r.started_at_ms)
            .min()
            .unwrap_or(now_ms);
        from = DateTime::from_timestamp_millis(first)
            .map_or(to, |t| t.with_timezone(&tz).date_naive())
            .min(to);
        window = window_of(from, to);
        (records, None)
    } else if range == UsageRange::Relative {
        let previous = Window {
            since_ms: window.since_ms - (window.until_ms - window.since_ms),
            until_ms: window.since_ms,
        };
        (read(previous.since_ms), Some(previous))
    } else {
        let days = (to - from).num_days() + 1;
        let previous = window_of(from - Duration::days(days), from - Duration::days(1));
        (read(previous.since_ms), Some(previous))
    };
    InsightsView {
        range,
        from: if range == UsageRange::Relative {
            super::sessions::iso_ms(window.since_ms).unwrap()
        } else {
            from.to_string()
        },
        to: if range == UsageRange::Relative {
            super::sessions::iso_ms(window.until_ms).unwrap()
        } else {
            to.to_string()
        },
        insights: insights::summarize(
            &records,
            &Query {
                tz: &tz,
                window,
                previous,
                agent,
            },
        ),
    }
}

pub(crate) fn summary(ctx: &ApiCtx, p: Value) -> Result<Value, ApiError> {
    let env = ctx.env()?;
    let query: SummaryParams = params(p)?;
    let window = range_window(
        query.range,
        query.from.as_deref(),
        query.to.as_deref(),
        query.hours,
        &ctx.now,
    )?;
    reply(&build_window(
        &env.run_dir(),
        &ctx.now,
        query.range,
        window,
        query.agent.as_deref(),
    ))
}

#[derive(Deserialize)]
struct AnalysisParams {
    range: UsageRange,
    from: Option<String>,
    to: Option<String>,
    hours: Option<u32>,
    agent: Option<String>,
    lang: Language,
    #[serde(default)]
    force: bool,
}

/// 这段时间的汇总、主 agent 与要跑的那一次。汇总与 `insights.summary` 是同一份。
fn analysis_plan(
    env: &ApiEnv,
    ctx: &ApiCtx,
    q: &AnalysisParams,
) -> Result<(Option<PrimaryAgent>, Plan), ApiError> {
    let window = range_window(
        q.range,
        q.from.as_deref(),
        q.to.as_deref(),
        q.hours,
        &ctx.now,
    )?;
    let view = build_window(
        &env.run_dir(),
        &ctx.now,
        q.range,
        window,
        q.agent.as_deref(),
    );
    let summary_json = serde_json::to_string(&view).map_err(internal)?;
    // config.yaml 读不出时按内置默认:主 agent 自动选择。
    let configured = env.read_config()?.parsed.primary_agent;
    let primary = analysis::primary(
        configured.as_deref(),
        env.agents(),
        &|alias| env.enabled(alias),
        &env.run_dir(),
        ctx.now.timestamp_millis(),
    );
    let plan = analysis::plan(
        primary.as_ref(),
        analysis::Request {
            from: if q.range == UsageRange::Relative {
                format!("past:{}h", q.hours.unwrap())
            } else {
                view.from
            },
            to: if q.range == UsageRange::Relative {
                "now".into()
            } else {
                view.to
            },
            agent_filter: q.agent.clone(),
            lang: q.lang,
            summary_json,
        },
    );
    Ok((primary, plan))
}

fn rfc3339_ms(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .map(|t| rfc3339(t.with_timezone(&Local)))
        .unwrap_or_default()
}

fn analysis_view(
    env: &ApiEnv,
    primary: Option<&PrimaryAgent>,
    plan: &Plan,
    allowed: bool,
) -> AnalysisView {
    let primary_view = primary.map(|p| AnalysisPrimary {
        agent: p.agent.name.clone(),
        label: Some(p.agent.display.clone())
            .filter(|d| !d.is_empty())
            .unwrap_or_else(|| p.agent.name.clone()),
        source: p.source,
        cli: p.agent.cli_kind().to_string(),
        model: crate::runtime::chat_model::effective(&p.agent, None)
            .model()
            .map(str::to_string),
    });
    let mut view = AnalysisView {
        primary: primary_view,
        status: AnalysisStatus::None,
        result: None,
        reason: None,
        started_at: None,
        can_start: false,
    };
    let job = match plan {
        Plan::Unsupported(reason) => {
            view.status = AnalysisStatus::Unsupported;
            view.reason = Some(reason.clone());
            return view;
        }
        Plan::Ready(job) => job,
    };
    view.can_start = allowed;
    match env.analysis.state(&job.slot) {
        State::None => {}
        State::Running { since_ms } => {
            view.status = AnalysisStatus::Running;
            view.started_at = Some(rfc3339_ms(since_ms));
        }
        State::Finished(record) if record.ok => {
            let record = *record;
            view.status = AnalysisStatus::Done;
            view.result = Some(AnalysisResult {
                text: record.text.unwrap_or_default(),
                truncated: record.truncated,
                generated_at: rfc3339_ms(record.generated_at_ms),
                agent: record.agent,
                cli: record.cli,
                model: record.model,
                elapsed_ms: record.elapsed_ms,
                prompt_version: record.key.prompt_version,
                usage: record.usage,
                stale: record.key != job.key,
                retry_failure: record.last_failure.map(|f| AnalysisRetryFailure {
                    reason: f.reason,
                    at: rfc3339_ms(f.at_ms),
                }),
            });
        }
        State::Finished(record) => {
            view.status = AnalysisStatus::Failed;
            view.reason = record.error;
        }
    }
    view
}

/// `insights.analysis.get`:这段时间的分析此刻是什么状态。可见范围与洞察页相同。
pub(crate) fn analysis_get(ctx: &ApiCtx, p: Value) -> Result<Value, ApiError> {
    let env = ctx.env()?;
    let q: AnalysisParams = params(p)?;
    let (primary, plan) = analysis_plan(env, ctx, &q)?;
    let allowed = env.decide(ctx, &Target::Machine).is_allowed();
    reply(&analysis_view(env, primary.as_ref(), &plan, allowed))
}

/// `insights.analysis.start`:让主 agent 分析这段时间。花的是这台机器的额度,发起与改整机设置
/// 同一道判权;槽里已有同键的成功结果不重跑(`force` 除外),过期了直接重跑;同槽在跑返回进行中,
/// 另一个槽在跑时回 busy。
pub(crate) fn analysis_start(ctx: &ApiCtx, p: Value) -> Result<Value, ApiError> {
    let env = ctx.env()?;
    let q: AnalysisParams = params(p)?;
    let result = (|| {
        env.require(ctx, &Target::Machine)?;
        let (primary, plan) = analysis_plan(env, ctx, &q)?;
        if let Plan::Ready(job) = &plan {
            env.analysis.start((**job).clone(), q.force).map_err(|_| {
                let busy = summon_core::i18n::with_language(q.lang, || {
                    summon_i18n::text!("rb6477a3aaa").to_string()
                });
                ApiError::new(ErrorCode::Busy, busy)
            })?;
        }
        Ok(analysis_view(env, primary.as_ref(), &plan, true))
    })();
    env.audit(ctx, "insights.analysis", &[], &outcome(&result));
    reply(&result?)
}

/// 契约样例用:给这组参数落一份已完成的分析(不起进程);`retry_failed` 时带上一次没成功的重新生成。
#[cfg(test)]
pub(crate) fn seed_analysis(
    ctx: &ApiCtx,
    p: Value,
    text: &str,
    generated_at_ms: i64,
    retry_failed: Option<&str>,
) {
    let env = ctx.env().unwrap();
    let q: AnalysisParams = params(p).unwrap();
    let (_, plan) = analysis_plan(env, ctx, &q).unwrap();
    let Plan::Ready(job) = plan else {
        panic!("样例的主 agent 应支持分析");
    };
    env.analysis.save(&analysis::Record {
        slot: job.slot.clone(),
        key: job.key.clone(),
        ok: true,
        text: Some(text.to_string()),
        truncated: false,
        error: None,
        generated_at_ms,
        agent: job.agent.name.clone(),
        cli: job.agent.cli_kind().to_string(),
        model: job.key.model.clone(),
        elapsed_ms: 41_000,
        usage: Some(summon_core::insights_analysis::AnalysisUsage {
            input: 1_200,
            output: 380,
            cache_read: 9_400,
            cache_write: 0,
        }),
        last_failure: retry_failed.map(|reason| analysis::RetryFailure {
            reason: reason.to_string(),
            at_ms: generated_at_ms,
        }),
    });
}

#[cfg(test)]
mod tests {
    use super::super::testkit::*;
    use super::*;
    use crate::runtime::dashboard::server::ErrorCode;
    use crate::runtime::dashboard::sessions::ChatKind;
    use chrono::TimeZone;
    use serde_json::json;
    use summon_core::turn_ledger::{TurnEnd, TurnOrigin, TurnRecord};

    const HEAD: &str = "不该出现在汇总里";

    fn at(month: u32, day: u32, h: u32, m: u32) -> i64 {
        Local
            .with_ymd_and_hms(2026, month, day, h, m, 0)
            .unwrap()
            .timestamp_millis()
    }

    fn record(agent: &str, chat_id: &str, started_at_ms: i64) -> TurnRecord {
        TurnRecord {
            agent: agent.into(),
            chat_id: chat_id.into(),
            tag: summon_core::registry::chat_tag("group", chat_id),
            turn: None,
            origin: TurnOrigin::Human,
            schedule: None,
            requester: Some(ME_OPEN.into()),
            message_id: "om_x".into(),
            prompt_head: HEAD.into(),
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

    /// 11-04(周三)14:00 发请求。本群三轮(两条间隔:10 分钟、8 分钟),另一个 agent 前一天一条
    /// 30 分钟的间隔,上一时段(10-25)一条 4 分钟的间隔,更早的一条哪里都不算。
    fn sample(kit: &Kit) -> ApiCtx {
        let mut ctx = kit.owner();
        ctx.now = Local.with_ymd_and_hms(2026, 11, 4, 14, 0, 0).unwrap();
        write(
            kit,
            &[
                record("starlight", GROUP, at(11, 4, 10, 0)),
                record("starlight", GROUP, at(11, 4, 10, 11)),
                record("starlight", GROUP, at(11, 4, 10, 20)),
                record("orbit", "oc_other", at(11, 3, 10, 0)),
                record("orbit", "oc_other", at(11, 3, 10, 31)),
                record("starlight", GROUP, at(10, 25, 10, 0)),
                record("starlight", GROUP, at(10, 25, 10, 5)),
                record("starlight", "oc_old", at(10, 1, 10, 0)),
            ],
        );
        ctx
    }

    #[test]
    fn relative_ranges_compare_adjacent_equal_windows_and_keep_milliseconds() {
        let kit = Kit::new();
        let mut ctx = kit.owner();
        ctx.now =
            Local.with_ymd_and_hms(2026, 11, 4, 21, 17, 29).unwrap() + Duration::milliseconds(123);
        let end = ctx.now.timestamp_millis();
        let start = end - 5 * 3_600_000;
        let previous = start - 5 * 3_600_000;
        write(
            &kit,
            &[previous - 1, previous, start - 1, start, end - 1, end].map(|at| {
                let mut r = record("starlight", GROUP, at);
                r.ended_at_ms = at + 1;
                r
            }),
        );
        write(
            &kit,
            &[
                record("starlight", "oc_before", previous - 600_000),
                record("starlight", "oc_before", previous - 480_000),
            ],
        );
        let out = kit
            .call(&ctx, summary, json!({"range":"relative","hours":5}))
            .unwrap();
        assert_eq!(out["human_turns"], 2);
        assert_eq!(out["previous"]["gaps"]["n"], 1);
        assert_eq!(
            DateTime::parse_from_rfc3339(out["from"].as_str().unwrap())
                .unwrap()
                .timestamp_millis(),
            start
        );
        assert_eq!(
            DateTime::parse_from_rfc3339(out["to"].as_str().unwrap())
                .unwrap()
                .timestamp_millis(),
            end
        );
    }

    #[test]
    fn ranges_select_gaps_compare_the_previous_period_and_filter_by_agent() {
        let kit = Kit::new();
        let ctx = sample(&kit);
        let week = kit.call(&ctx, summary, json!({"range": "7d"})).unwrap();
        assert_eq!(
            (week["from"].as_str(), week["to"].as_str()),
            (Some("2026-10-29"), Some("2026-11-04"))
        );
        assert_eq!(week["gaps"]["n"], 3);
        assert_eq!(week["gaps"]["total_secs"], 600.0 + 480.0 + 1_800.0);
        assert_eq!(week["human_turns"], 5);
        assert_eq!(
            (week["agents"].as_u64(), week["chats"].as_u64()),
            (Some(2), Some(2))
        );
        assert_eq!(week["days"].as_array().unwrap().len(), 7);
        assert_eq!(week["days"][6]["messages"], 3);
        assert_eq!(week["previous"]["gaps"]["n"], 1);
        assert_eq!(week["previous"]["gaps"]["p50_secs"], 240.0);
        assert_eq!(week["day_start_hour"], 9);
        let text = week.to_string();
        assert!(!text.contains(HEAD) && !text.contains(ME_OPEN), "{text}");

        let mine = kit
            .call(&ctx, summary, json!({"range": "7d", "agent": "starlight"}))
            .unwrap();
        assert_eq!(mine["gaps"]["n"], 2);
        assert_eq!(mine["agents"], 1);
        let missing = kit
            .call(&ctx, summary, json!({"range": "7d", "agent": "missing"}))
            .unwrap();
        assert_eq!(
            (missing["turns"].as_u64(), missing["gaps"]["n"].as_u64()),
            (Some(0), Some(0))
        );

        // 今天:上一时段是昨天整天
        let today = kit.call(&ctx, summary, json!({"range": "today"})).unwrap();
        assert_eq!(today["gaps"]["n"], 2);
        assert_eq!(today["days"].as_array().unwrap().len(), 1);
        assert_eq!(today["previous"]["gaps"]["total_secs"], 1_800.0);

        // 全部:从最早一条记录那天起,不做对比
        let all = kit.call(&ctx, summary, json!({"range": "all"})).unwrap();
        assert_eq!(all["from"], "2026-10-01");
        assert_eq!(all["gaps"]["n"], 4);
        assert!(all.get("previous").is_none());

        let custom = kit
            .call(
                &ctx,
                summary,
                json!({"range": "custom", "from": "2026-10-25", "to": "2026-10-25"}),
            )
            .unwrap();
        assert_eq!(custom["gaps"]["n"], 1);
        assert_eq!(custom["previous"]["gaps"]["n"], 0);

        // 飞书签发的会话也能看(与用量页相同,不另设门槛)
        let mut feishu = kit.feishu(issuer("starlight", GROUP, ChatKind::Group));
        feishu.now = ctx.now;
        assert_eq!(
            kit.call(&feishu, summary, json!({"range": "7d"})).unwrap(),
            week
        );
    }

    #[test]
    fn ranges_validate_dates_like_the_usage_page() {
        let kit = Kit::new();
        let mut ctx = kit.owner();
        ctx.now = Local.with_ymd_and_hms(2026, 11, 4, 14, 0, 0).unwrap();
        for p in [
            json!({"range":"custom"}),
            json!({"range":"custom", "from":"2026-11-02", "to":"2026-11-01"}),
            json!({"range":"custom", "from":"2026-02-30", "to":"2026-11-01"}),
            json!({"range":"custom", "from":"2026-1-01", "to":"2026-11-01"}),
            json!({"range":"custom", "from":"2026-01-01", "to":"2026-11-05"}),
            json!({"range":"custom", "from":"1969-12-31", "to":"2026-11-01"}),
            json!({"range":"7d", "from":"2026-11-01"}),
            json!({"range":"1y"}),
        ] {
            assert_eq!(
                kit.call(&ctx, summary, p.clone()).unwrap_err().code,
                ErrorCode::Invalid,
                "{p}"
            );
        }
        for (from, to, days) in [
            ("2025-12-31", "2026-01-01", 2),
            ("2024-02-28", "2024-03-01", 3),
            ("2026-11-04", "2026-11-04", 1),
        ] {
            let out = kit
                .call(
                    &ctx,
                    summary,
                    json!({"range":"custom", "from":from, "to":to}),
                )
                .unwrap();
            assert_eq!(out["days"].as_array().unwrap().len(), days);
            assert_eq!(
                (out["from"].as_str(), out["to"].as_str()),
                (Some(from), Some(to))
            );
        }
        assert_eq!(
            kit.call(&ctx, summary, json!({"range":"90d"})).unwrap()["days"]
                .as_array()
                .unwrap()
                .len(),
            90
        );
        // 一条记录都没有时,「全部」就是今天
        let all = kit.call(&ctx, summary, json!({"range":"all"})).unwrap();
        assert_eq!(
            (all["from"].as_str(), all["to"].as_str()),
            (Some("2026-11-04"), Some("2026-11-04"))
        );
    }

    /// 提示词里用反引号写出的每个名字,都得是汇总 JSON 里真有的键或 `key` 取值。
    #[test]
    fn prompt_field_names_exist_in_the_summary() {
        fn walk(v: &Value, out: &mut std::collections::BTreeSet<String>) {
            match v {
                Value::Object(map) => {
                    for (k, v) in map {
                        out.insert(k.clone());
                        if k == "key" {
                            if let Some(s) = v.as_str() {
                                out.insert(s.to_string());
                            }
                        }
                        walk(v, out);
                    }
                }
                Value::Array(items) => items.iter().for_each(|v| walk(v, out)),
                _ => {}
            }
        }
        let kit = Kit::new();
        let ctx = sample(&kit);
        let view = kit.call(&ctx, summary, json!({"range": "7d"})).unwrap();
        assert!(view.get("previous").is_some() && !view["findings"].as_array().unwrap().is_empty());
        let mut known = std::collections::BTreeSet::new();
        walk(&view, &mut known);
        for lang in [Language::Chinese, Language::English] {
            let prompt = summon_core::insights_analysis::prompt(lang);
            for name in prompt.split('`').skip(1).step_by(2) {
                assert!(
                    known.contains(name),
                    "{lang:?} 提示词里的 `{name}` 不在汇总里"
                );
            }
        }
    }

    /// 假 claude:记下标准输入与调用次数,回一份合成的分析。
    const FAKE_CLAUDE: &str = r#"#!/bin/sh
root="${0%/*}"
cat > "$root/stdin"
n=$(cat "$root/count" 2>/dev/null || echo 0); echo $((n+1)) > "$root/count"
if [ -f "$root/hold" ]; then echo > "$root/started"; while [ ! -f "$root/release" ]; do sleep 0.05; done; fi
if [ -f "$root/fail" ]; then exit 1; fi
printf '%s\n' '{"type":"result","subtype":"success","is_error":false,"result":"合成的分析结论","usage":{"input_tokens":10,"output_tokens":5}}'
"#;

    struct Analysis {
        kit: Kit,
        bin: tempfile::TempDir,
    }

    impl Analysis {
        /// starlight 跑 claude(假的);orbit 的名单里没有「我」,飞书会话因此改不了整机设置。
        fn new() -> Self {
            let kit = Kit::new();
            let bin = tempfile::tempdir().unwrap();
            let script = bin.path().join("claude");
            crate::test_support::write_test_executable(&script, FAKE_CLAUDE);
            kit.add_agent(
                "starlight",
                &agent_yaml("starlight", "星光", "claude", "model: synthetic-model\n").replace(
                    "  working_dir: /tmp\n",
                    &format!("  working_dir: /tmp\n  cli_cmd: {}\n", script.display()),
                ),
            );
            kit.add_agent(
                "orbit",
                &agent_yaml("orbit", "轨道", "cursor", "").replace(ME_UNION, PEER_UNION),
            );
            Self { kit, bin }
        }

        fn config(&self, text: &str) {
            std::fs::write(self.kit.home().join("config.yaml"), text).unwrap();
        }

        fn runs(&self) -> u32 {
            std::fs::read_to_string(self.bin.path().join("count"))
                .map_or(0, |c| c.trim().parse().unwrap_or(0))
        }

        fn owner(&self) -> ApiCtx {
            let mut ctx = self.kit.owner();
            ctx.now = Local.with_ymd_and_hms(2026, 11, 4, 14, 0, 0).unwrap();
            ctx
        }

        fn feishu(&self) -> ApiCtx {
            let mut ctx = self.kit.feishu(issuer("starlight", GROUP, ChatKind::Group));
            ctx.now = Local.with_ymd_and_hms(2026, 11, 4, 14, 0, 0).unwrap();
            ctx
        }

        /// 等到不在跑。
        fn settle(&self, ctx: &ApiCtx, p: Value) -> Value {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            loop {
                let view = self.kit.call(ctx, analysis_get, p.clone()).unwrap();
                if view["status"] != "running" {
                    return view;
                }
                assert!(std::time::Instant::now() < deadline, "分析没在时限内结束");
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }
    }

    #[test]
    fn relative_analysis_keeps_the_slot_as_query_time_moves() {
        let a = Analysis::new();
        a.config("primary_agent: starlight\n");
        let mut ctx = a.owner();
        let query = json!({"range":"relative", "hours":24, "lang":"zh-CN"});
        a.kit.call(&ctx, analysis_start, query.clone()).unwrap();
        let first = a.settle(&ctx, query.clone());
        assert_eq!(first["status"], "done");
        ctx.now += Duration::hours(1);
        let moved = a.kit.call(&ctx, analysis_get, query.clone()).unwrap();
        assert_eq!(moved["status"], "done");
        assert_eq!(moved["result"]["text"], first["result"]["text"]);
        assert_eq!(moved["result"]["stale"], true);
        assert_eq!(a.runs(), 1);
        let other = a
            .kit
            .call(
                &ctx,
                analysis_get,
                json!({"range":"relative", "hours":48, "lang":"zh-CN"}),
            )
            .unwrap();
        assert_eq!(other["status"], "none");
        let invalid = a
            .kit
            .call(
                &ctx,
                analysis_get,
                json!({"range":"relative", "hours":0, "lang":"zh-CN"}),
            )
            .unwrap_err();
        assert_eq!(invalid.code, ErrorCode::Invalid);
    }

    #[test]
    fn starting_needs_a_machine_grant_while_reading_stays_open() {
        let a = Analysis::new();
        a.config("primary_agent: starlight\n");
        sample(&a.kit);
        let p = json!({"range": "7d", "lang": "zh-CN"});
        let feishu = a.feishu();
        let view = a.kit.call(&feishu, analysis_get, p.clone()).unwrap();
        assert_eq!(view["status"], "none");
        assert_eq!(view["can_start"], false);
        assert_eq!(view["primary"]["agent"], "starlight");
        assert_eq!(view["primary"]["source"], "configured");
        assert_eq!(view["primary"]["model"], "synthetic-model");
        assert_eq!(
            a.kit
                .call(&feishu, analysis_start, p.clone())
                .unwrap_err()
                .code,
            ErrorCode::Forbidden
        );
        assert_eq!(a.runs(), 0, "无权发起:不起进程");

        let owner = a.owner();
        assert_eq!(
            a.kit.call(&owner, analysis_get, p.clone()).unwrap()["can_start"],
            true
        );
        let started = a.kit.call(&owner, analysis_start, p.clone()).unwrap();
        assert!(matches!(
            started["status"].as_str(),
            Some("running" | "done")
        ));
        let done = a.settle(&owner, p.clone());
        assert_eq!(done["status"], "done", "{done}");
        assert_eq!(done["result"]["text"], "合成的分析结论");
        assert_eq!(done["result"]["usage"]["input"], 10);
        assert_eq!(done["result"]["agent"], "starlight");
        // 读取不受判权影响:飞书会话也看得到结果
        assert_eq!(
            a.kit.call(&feishu, analysis_get, p.clone()).unwrap()["result"],
            done["result"]
        );
        let audit: Vec<(String, String)> = a
            .kit
            .audit_lines()
            .iter()
            .map(|l| {
                (
                    l["file"].as_str().unwrap_or_default().to_string(),
                    l["result"].as_str().unwrap_or_default().to_string(),
                )
            })
            .collect();
        assert!(
            audit.contains(&("insights.analysis".into(), "forbidden".into())),
            "{audit:?}"
        );
        assert!(
            audit.contains(&("insights.analysis".into(), "ok".into())),
            "{audit:?}"
        );
    }

    #[test]
    fn the_model_sees_only_the_summary_and_nothing_else_is_kept() {
        let a = Analysis::new();
        a.config("primary_agent: starlight\n");
        sample(&a.kit);
        let owner = a.owner();
        let p = json!({"range": "7d", "lang": "en"});
        a.kit.call(&owner, analysis_start, p.clone()).unwrap();
        let done = a.settle(&owner, p.clone());
        assert_eq!(done["status"], "done", "{done}");
        let stdin = std::fs::read_to_string(a.bin.path().join("stdin")).unwrap();
        let prompt = summon_core::insights_analysis::prompt(Language::English).trim_end();
        let body = stdin
            .strip_prefix(prompt)
            .and_then(|rest| rest.strip_prefix("\n```json\n"))
            .and_then(|rest| rest.strip_suffix("\n```\n"))
            .unwrap_or_else(|| panic!("输入的形状不对:{stdin}"));
        let sent: Value = serde_json::from_str(body).unwrap();
        let summary = a.kit.call(&owner, summary, json!({"range": "7d"})).unwrap();
        assert_eq!(sent, summary, "交给模型的就是 insights.summary 的结果");
        let mut kept = vec![stdin, done.to_string()];
        for entry in std::fs::read_dir(a.kit.home().join("run/insights-analysis")).unwrap() {
            kept.push(std::fs::read_to_string(entry.unwrap().path()).unwrap());
        }
        for text in &kept {
            for secret in [HEAD, ME_OPEN, GROUP, "oc_other"] {
                assert!(!text.contains(secret), "{secret} 漏进了 {text}");
            }
        }
        // 落盘里连汇总也没有,只有它的指纹
        assert!(!kept[2].contains("human_turns"));
    }

    #[test]
    fn a_slot_keeps_its_result_marks_it_stale_and_reruns_only_when_needed() {
        let a = Analysis::new();
        a.config("primary_agent: starlight\n");
        sample(&a.kit);
        let owner = a.owner();
        let p = json!({"range": "7d", "lang": "zh-CN"});
        a.kit.call(&owner, analysis_start, p.clone()).unwrap();
        let done = a.settle(&owner, p.clone());
        assert_eq!(
            (done["status"].as_str(), done["result"]["stale"].as_bool()),
            (Some("done"), Some(false))
        );
        assert_eq!(
            a.kit.call(&owner, analysis_start, p.clone()).unwrap()["status"],
            "done"
        );
        assert_eq!(a.runs(), 1, "同键已有结果:不重跑");
        // 别的槽:时间段、筛选、语言
        for other in [
            json!({"range": "7d", "lang": "en"}),
            json!({"range": "7d", "lang": "zh-CN", "agent": "starlight"}),
            json!({"range": "30d", "lang": "zh-CN"}),
        ] {
            assert_eq!(
                a.kit.call(&owner, analysis_get, other.clone()).unwrap()["status"],
                "none",
                "{other}"
            );
        }
        // 新的运行记录让汇总变了:同一个槽,上一份照样看得到,标为过期
        write(&a.kit, &[record("starlight", GROUP, at(11, 4, 11, 0))]);
        let stale = a.kit.call(&owner, analysis_get, p.clone()).unwrap();
        assert_eq!(
            (stale["status"].as_str(), stale["result"]["stale"].as_bool()),
            (Some("done"), Some(true))
        );
        assert_eq!(stale["result"]["text"], done["result"]["text"]);
        // 过期了:不带 force 也重跑
        a.kit.call(&owner, analysis_start, p.clone()).unwrap();
        assert_eq!(a.settle(&owner, p.clone())["result"]["stale"], false);
        assert_eq!(a.runs(), 2);
        // 换了模型:同样过期
        let manifest = a.kit.agents_dir().join("starlight/agent.yaml");
        let text = std::fs::read_to_string(&manifest).unwrap();
        std::fs::write(&manifest, text.replace("synthetic-model", "another-model")).unwrap();
        assert_eq!(
            a.kit.call(&owner, analysis_get, p.clone()).unwrap()["result"]["stale"],
            true
        );
    }

    #[test]
    fn data_arriving_mid_run_keeps_running_and_then_shows_a_stale_result() {
        let a = Analysis::new();
        a.config("primary_agent: starlight\n");
        sample(&a.kit);
        std::fs::write(a.bin.path().join("hold"), "").unwrap();
        let owner = a.owner();
        let p = json!({"range": "7d", "lang": "zh-CN"});
        a.kit.call(&owner, analysis_start, p.clone()).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !a.bin.path().join("started").exists() {
            assert!(std::time::Instant::now() < deadline, "进程没起来");
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        write(&a.kit, &[record("starlight", GROUP, at(11, 4, 11, 0))]);
        let running = a.kit.call(&owner, analysis_get, p.clone()).unwrap();
        assert_eq!(running["status"], "running", "数据变了,槽仍在跑");
        assert!(running["started_at"].is_string());
        assert_eq!(
            a.kit.call(&owner, analysis_start, p.clone()).unwrap()["status"],
            "running",
            "同槽在跑:进行中,不是忙"
        );
        std::fs::write(a.bin.path().join("release"), "").unwrap();
        let done = a.settle(&owner, p.clone());
        assert_eq!(
            (done["status"].as_str(), done["result"]["stale"].as_bool()),
            (Some("done"), Some(true))
        );
        assert_eq!(a.runs(), 1);
    }

    #[test]
    fn a_failed_regeneration_keeps_the_previous_result_and_says_so() {
        let a = Analysis::new();
        a.config("primary_agent: starlight\n");
        sample(&a.kit);
        let owner = a.owner();
        let p = json!({"range": "7d", "lang": "en"});
        a.kit.call(&owner, analysis_start, p.clone()).unwrap();
        let first = a.settle(&owner, p.clone());
        let mut forced = p.clone();
        forced["force"] = json!(true);
        std::fs::write(a.bin.path().join("fail"), "").unwrap();
        a.kit.call(&owner, analysis_start, forced.clone()).unwrap();
        let kept = a.settle(&owner, p.clone());
        assert_eq!(kept["status"], "done");
        assert_eq!(kept["result"]["text"], first["result"]["text"]);
        assert_eq!(
            kept["result"]["generated_at"],
            first["result"]["generated_at"]
        );
        assert_eq!(
            kept["result"]["retry_failure"]["reason"],
            "claude exited abnormally"
        );
        assert!(kept["result"]["retry_failure"]["at"].is_string());
        std::fs::remove_file(a.bin.path().join("fail")).unwrap();
        a.kit.call(&owner, analysis_start, forced).unwrap();
        let again = a.settle(&owner, p);
        assert!(again["result"].get("retry_failure").is_none(), "{again}");
        assert_eq!(a.runs(), 3);
    }

    #[test]
    fn unsupported_primary_reports_why_and_bad_config_falls_back() {
        let a = Analysis::new();
        a.config("primary_agent: orbit\n");
        sample(&a.kit);
        let owner = a.owner();
        let p = json!({"range": "7d", "lang": "zh-CN"});
        let view = a.kit.call(&owner, analysis_start, p.clone()).unwrap();
        assert_eq!(view["status"], "unsupported");
        assert_eq!(view["can_start"], false);
        assert_eq!(view["primary"]["agent"], "orbit");
        assert!(
            view["reason"].as_str().unwrap().contains("cursor"),
            "{view}"
        );
        assert_eq!(a.runs(), 0);
        // 配置指向不在的 agent:回落到用量最多的那个
        a.config("primary_agent: missing\n");
        let mut used = record("starlight", GROUP, at(11, 4, 9, 0));
        used.usage = vec![summon_core::turn_ledger::ModelUsage {
            model: "m".into(),
            band: 0,
            variant: None,
            requests: 1,
            input: 0,
            output: 9,
            cache_read: 0,
            cache_write_5m: 0,
            cache_write_1h: 0,
        }];
        write(&a.kit, &[used]);
        let view = a.kit.call(&owner, analysis_get, p).unwrap();
        assert_eq!(
            (
                view["primary"]["agent"].as_str(),
                view["primary"]["source"].as_str()
            ),
            (Some("starlight"), Some("usage"))
        );
        assert_eq!(view["status"], "none");
    }

    #[test]
    fn primary_agent_setting_accepts_only_usable_agents() {
        let a = Analysis::new();
        let owner = a.owner();
        let patch = |value: Value| {
            let revision = crate::runtime::dashboard::writer::revision(
                std::fs::read(a.kit.home().join("config.yaml"))
                    .ok()
                    .as_deref(),
            );
            a.kit.call(
                &owner,
                super::super::config::patch,
                json!({"scope": {"level": "global"}, "revision": revision,
                       "changes": [{"key": "primary_agent", "value": value}]}),
            )
        };
        for bad in ["missing", "orbit-x"] {
            let error = patch(json!(bad)).unwrap_err();
            assert_eq!(
                (error.code, error.key.as_deref()),
                (ErrorCode::Invalid, Some("primary_agent"))
            );
        }
        patch(json!("orbit")).unwrap();
        assert!(a.kit.read("config.yaml").contains("primary_agent: orbit"));
        patch(Value::Null).unwrap();
        assert!(!a.kit.read("config.yaml").contains("primary_agent"));
    }
}
