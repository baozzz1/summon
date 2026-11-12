//! Menu Bar 只读费用快照：正常请求的本地台账与已有价格表。

use std::io::{BufRead, BufReader};
use std::path::Path;

use anyhow::Context as _;

use chrono::{Local, NaiveDate};
use serde::{de::DeserializeOwned, Serialize};
use summon_core::pricing::{self, Spend};

use crate::runtime::turn_ledger;
use summon_core::turn_ledger::{OutsideUsage, TurnRecord};

const DAY_MS: i64 = 24 * 3600 * 1000;

#[derive(Debug, Serialize)]
struct Window {
    key: &'static str,
    spend: Option<Spend>,
    missing_usage_turns: usize,
}

#[derive(Debug, Serialize)]
struct Snapshot {
    as_of_ms: i64,
    retention_days: i64,
    prices_as_of_ms: Option<i64>,
    windows: Vec<Window>,
}

/// 只读取完整落盘的行；读失败与损坏记录向上传递，避免把未知费用显示为零。
fn read_local<T: DeserializeOwned>(dir: &Path) -> anyhow::Result<Vec<T>> {
    let agents = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let mut rows = Vec::new();
    for agent in agents {
        let agent = agent?;
        if !agent.file_type()?.is_dir() {
            continue;
        }
        for file in std::fs::read_dir(agent.path())? {
            let file = file?;
            let name = file.file_name();
            let Some(stem) = name.to_str().and_then(|n| n.strip_suffix(".ndjson")) else {
                continue;
            };
            if NaiveDate::parse_from_str(&format!("{stem}-01"), "%Y-%m-%d").is_err() {
                continue;
            }
            let mut reader = BufReader::new(std::fs::File::open(file.path())?);
            let mut line = Vec::new();
            loop {
                line.clear();
                if reader.read_until(b'\n', &mut line)? == 0 {
                    break;
                }
                // 追加过程中的尾行留给下一次快照。
                if line.last() != Some(&b'\n') {
                    break;
                }
                if line.iter().all(u8::is_ascii_whitespace) {
                    continue;
                }
                rows.push(
                    serde_json::from_slice(&line).context(summon_i18n::text!("r6bab5c0287"))?,
                );
            }
        }
    }
    Ok(rows)
}

fn snapshot(run_dir: &Path, now_ms: i64) -> anyhow::Result<Snapshot> {
    let records: Vec<TurnRecord> = read_local(&turn_ledger::dir_in(run_dir))?;
    let outside: Vec<OutsideUsage> = read_local(&turn_ledger::outside_dir_in(run_dir))?;
    let prices = crate::runtime::prices::load(run_dir);
    let windows = [
        ("24h", Some(1)),
        ("7d", Some(7)),
        ("30d", Some(30)),
        ("all", None),
    ]
    .into_iter()
    .map(|(key, days)| {
        let since = days.map_or(i64::MIN, |n| now_ms.saturating_sub(n * DAY_MS));
        let records = records
            .iter()
            .filter(|r| r.ended_at_ms >= since && r.ended_at_ms <= now_ms);
        Window {
            key,
            missing_usage_turns: records.clone().filter(|r| r.usage.is_empty()).count(),
            spend: pricing::spend(
                prices.as_deref(),
                records.flat_map(|r| &r.usage).chain(
                    outside
                        .iter()
                        .filter(|r| r.from_ms >= since && r.from_ms <= now_ms)
                        .flat_map(|r| &r.usage),
                ),
            ),
        }
    })
    .collect();
    Ok(Snapshot {
        as_of_ms: now_ms,
        retention_days: summon_core::turn_ledger::KEEP_DAYS,
        prices_as_of_ms: prices.as_ref().map(|p| p.fetched_at_ms),
        windows,
    })
}

pub(crate) fn run() -> anyhow::Result<()> {
    let snapshot = snapshot(
        &crate::runtime::paths::run_dir(),
        Local::now().timestamp_millis(),
    )?;
    println!("{}", serde_json::to_string(&snapshot)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use serde_json::json;
    use summon_core::turn_ledger::{ModelUsage, OutsideUsage, TurnRecord};

    fn now() -> i64 {
        Local
            .with_ymd_and_hms(2026, 11, 2, 12, 0, 0)
            .unwrap()
            .timestamp_millis()
    }

    fn add_turn(run: &Path, at: i64, model: Option<&str>) {
        let usage = model
            .map(|m| {
                vec![ModelUsage {
                    model: m.into(),
                    output: 100,
                    ..Default::default()
                }]
            })
            .unwrap_or_default();
        let record: TurnRecord = serde_json::from_value(json!({
            "agent":"starlight","chat_id":"oc_example","tag":"example",
            "origin":"human","message_id":"om_example","prompt_head":"private content",
            "started_at_ms":at-100,"ended_at_ms":at,"end":"done","usage":usage
        }))
        .unwrap();
        let dir = turn_ledger::dir_in(run).join("starlight");
        std::fs::create_dir_all(&dir).unwrap();
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join(summon_core::turn_ledger::file_name(at)))
            .unwrap();
        writeln!(file, "{}", serde_json::to_string(&record).unwrap()).unwrap();
    }

    fn price(run: &Path) {
        let table = pricing::PriceTable::from_litellm(
            r#"{"test-model":{"input_cost_per_token":0.001,"output_cost_per_token":0.01}}"#,
            "local-fixture",
            100,
        )
        .unwrap();
        let path = crate::runtime::prices::path_in(run);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, serde_json::to_vec(&table).unwrap()).unwrap();
    }

    #[test]
    fn rolling_windows_include_boundaries_and_outside_usage() {
        let dir = tempfile::tempdir().unwrap();
        let run = dir.path();
        let now = now();
        for at in [
            now,
            now - DAY_MS,
            now - DAY_MS - 1,
            now - 7 * DAY_MS,
            now - 7 * DAY_MS - 1,
            now - 30 * DAY_MS,
            now - 30 * DAY_MS - 1,
            now - 120 * DAY_MS,
            now + 1,
        ] {
            add_turn(run, at, Some("test-model"));
        }
        turn_ledger::append_outside(
            &turn_ledger::outside_dir_in(run),
            &OutsideUsage {
                agent: "starlight".into(),
                chat_id: "oc_example".into(),
                tag: "example".into(),
                cli: "claude".into(),
                from_ms: now - DAY_MS,
                to_ms: now - DAY_MS + 10,
                usage: vec![ModelUsage {
                    model: "test-model".into(),
                    output: 50,
                    ..Default::default()
                }],
            },
        )
        .unwrap();
        price(run);
        let s = snapshot(run, now).unwrap();
        assert_eq!(
            s.windows
                .iter()
                .map(|w| w.spend.as_ref().unwrap().tokens.output)
                .collect::<Vec<_>>(),
            [250, 450, 650, 850]
        );
        assert_eq!(s.windows[0].spend.as_ref().unwrap().cost.total, 2.5);
        assert_eq!(s.prices_as_of_ms, Some(100));
        let output = serde_json::to_string(&s).unwrap();
        assert!(!output.contains("private content"));
        assert!(!output.contains("oc_example"));
    }

    #[test]
    fn missing_prices_and_missing_usage_stay_explicit() {
        let dir = tempfile::tempdir().unwrap();
        add_turn(dir.path(), now(), Some("unknown-model"));
        add_turn(dir.path(), now(), None);
        let s = snapshot(dir.path(), now()).unwrap();
        let w = &s.windows[0];
        assert_eq!(w.missing_usage_turns, 1);
        assert_eq!(
            w.spend.as_ref().unwrap().unpriced.as_ref().unwrap().tokens,
            100
        );
        assert_eq!(s.prices_as_of_ms, None);
        price(dir.path());
        assert!(snapshot(dir.path(), now()).unwrap().windows[0]
            .spend
            .as_ref()
            .unwrap()
            .unpriced
            .is_some());
    }

    #[test]
    fn empty_snapshot_matches_shared_golden() {
        let dir = tempfile::tempdir().unwrap();
        let actual = serde_json::to_value(snapshot(dir.path(), 1000).unwrap()).unwrap();
        let expected: serde_json::Value = serde_json::from_str(include_str!(
            "../../../../tests/fixtures/menubar-usage.golden.json"
        ))
        .unwrap();
        assert_eq!(actual, expected);
    }
    #[test]
    fn corrupt_records_fail_and_partial_appends_wait() {
        let dir = tempfile::tempdir().unwrap();
        add_turn(dir.path(), now(), Some("test-model"));
        let file = turn_ledger::dir_in(dir.path())
            .join("starlight")
            .join(summon_core::turn_ledger::file_name(now()));
        use std::io::Write;
        let mut out = std::fs::OpenOptions::new().append(true).open(file).unwrap();
        write!(out, "{{").unwrap();
        assert_eq!(
            snapshot(dir.path(), now()).unwrap().windows[0]
                .spend
                .as_ref()
                .unwrap()
                .tokens
                .output,
            100
        );
        writeln!(out).unwrap();
        assert!(snapshot(dir.path(), now()).is_err());
    }

    #[test]
    fn a_non_directory_ledger_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(turn_ledger::dir_in(dir.path()), "not a directory").unwrap();
        assert!(snapshot(dir.path(), now()).is_err());
    }
    #[test]
    fn priced_snapshot_matches_shared_golden() {
        let dir = tempfile::tempdir().unwrap();
        add_turn(dir.path(), 1000, Some("test-model"));
        price(dir.path());
        let actual = serde_json::to_value(snapshot(dir.path(), 1000).unwrap()).unwrap();
        let expected: serde_json::Value = serde_json::from_str(include_str!(
            "../../../../tests/fixtures/menubar-usage-priced.golden.json"
        ))
        .unwrap();
        assert_eq!(actual, expected);
    }

    #[test]
    fn mixed_prices_expose_the_unpriced_portion() {
        let dir = tempfile::tempdir().unwrap();
        price(dir.path());
        add_turn(dir.path(), now(), Some("test-model"));
        add_turn(dir.path(), now(), Some("unknown-model"));
        let s = snapshot(dir.path(), now()).unwrap();
        let spend = s.windows[0].spend.as_ref().unwrap();
        assert_eq!(spend.cost.total, 1.0);
        assert_eq!(spend.tokens.output, 200);
        assert_eq!(spend.unpriced.as_ref().unwrap().tokens, 100);
    }
}
