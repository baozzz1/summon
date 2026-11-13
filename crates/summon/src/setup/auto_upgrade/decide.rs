//! 自动升级的判据:问到的新版该不该采用、什么时候该问、整机空不空。

use super::state::{Seen, State};
use crate::runtime::inbox_wal::{InboxRecord, InboxWal};
use anyhow::Result;
use std::time::{Duration, SystemTime};
use summon_core::config::{Upgrade, UpgradeRange};
use summon_core::fleet::BusyTurn;
use summon_core::install_stamp;

/// 问到的最新版该不该装。
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Verdict {
    Install,
    Skip(String),
}

pub(crate) fn decide(
    running: &str,
    latest: &Seen,
    policy: &Upgrade,
    state: &State,
    now_secs: u64,
    today: &str,
) -> Verdict {
    let skip = |why: String| Verdict::Skip(why);
    let version = &latest.version;
    let (Some(have), Some(want)) = (
        install_stamp::release_version(running),
        install_stamp::release_version(version),
    ) else {
        return skip(summon_i18n::format!("re147a34d14"));
    };
    if want <= have {
        return skip(summon_i18n::format!("rcdd9835a46"));
    }
    if want.0 != have.0 {
        return skip(summon_i18n::format!("r3af0f6f5ad"));
    }
    if policy.range == UpgradeRange::Patch && want.1 != have.1 {
        return skip(summon_i18n::format!("rcd691eb9cb"));
    }
    if state.rolled_back.iter().any(|v| v == version) {
        return skip(summon_i18n::format!("r0cbfbcf54e"));
    }
    if let Some(held) = state
        .held
        .as_ref()
        .filter(|held| &held.version == version && held.day == today)
    {
        return skip(summon_i18n::format!("r148a74c19d", held.reason));
    }
    let Some(published) = latest.published_secs else {
        return skip(summon_i18n::format!("raccd004414"));
    };
    if now_secs < published.saturating_add(policy.min_age_hours.saturating_mul(3600)) {
        return skip(summon_i18n::format!("r8bc9cb0685", policy.min_age_hours));
    }
    Verdict::Install
}

/// 该不该问发布源了。间隔至少 1 小时;时钟回拨按该问算。
pub(crate) fn check_due(state: &State, policy: &Upgrade, now_secs: u64) -> bool {
    let interval = policy.check_interval_hours.max(1).saturating_mul(3600);
    state
        .last_check_secs
        .is_none_or(|last| now_secs < last || now_secs - last >= interval)
}

/// 整机连续空闲了多久。一次没空就从头算;中间有会话处理过消息,从那一刻起算 —— 采样之间
/// 跑完的短轮也不会被漏掉。
#[derive(Debug, Default)]
pub(crate) struct IdleClock {
    since: Option<SystemTime>,
}

impl IdleClock {
    pub(crate) fn tick(
        &mut self,
        quiet: bool,
        last_activity: Option<SystemTime>,
        now: SystemTime,
        need: Duration,
    ) -> bool {
        if !quiet {
            self.since = None;
            return false;
        }
        let mut since = *self.since.get_or_insert(now);
        if let Some(activity) = last_activity.filter(|at| *at > since) {
            since = activity;
        }
        self.since = Some(since);
        now.duration_since(since).is_ok_and(|idle| idle >= need)
    }

    pub(crate) fn reset(&mut self) {
        self.since = None;
    }
}

/// 整机此刻空不空;不空时说明原因。判据与 Menu Bar 的 Working 同一份(在跑一轮,或子 agent /
/// workflow 还在跑),外加有 chat 线程在处理消息、入站 WAL 里有排队没开跑的任务。
pub(crate) fn machine_quiet(wal: &InboxWal, now: SystemTime) -> Result<(), String> {
    use crate::observe::runtime_status;
    quiet_from(
        &runtime_status::busy_turns(),
        &runtime_status::active_subagents(now),
        &runtime_status::handling_chats(),
        wal.scan_pending_strict(),
    )
}

fn quiet_from(
    busy: &[BusyTurn],
    subagents: &[BusyTurn],
    handling: &[BusyTurn],
    pending: Result<Vec<InboxRecord>>,
) -> Result<(), String> {
    let name = |turns: &[BusyTurn]| {
        turns
            .iter()
            .map(|t| format!("{}/{}", t.agent, t.tag))
            .collect::<Vec<_>>()
            .join("、")
    };
    if !busy.is_empty() {
        return Err(summon_i18n::format!("r22ba601f8e", name(busy)));
    }
    if !subagents.is_empty() {
        return Err(summon_i18n::format!("r4939a7f53a", name(subagents)));
    }
    if !handling.is_empty() {
        return Err(summon_i18n::format!("reaa445b9d3", name(handling)));
    }
    // 已开跑的记录对应的轮已经算在上面;留在 pending 里的已开跑记录是没销账的旧轮,不挡。
    let queued = pending
        .map_err(|error| summon_i18n::format!("ra0bcf4f24b"))?
        .into_iter()
        .filter(|r| !r.started && !r.rejected_disabled && !r.superseded_by_new)
        .count();
    if queued > 0 {
        return Err(summon_i18n::format!("ra2117b1c66"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::UNIX_EPOCH;

    fn policy() -> Upgrade {
        Upgrade::default()
    }

    const NOW: u64 = 2_000_000_000;
    const TODAY: &str = "2033-05-18";

    fn seen(version: &str, age_hours: u64) -> Seen {
        Seen {
            version: version.to_string(),
            published_secs: Some(NOW - age_hours * 3600),
        }
    }

    fn verdict(running: &str, latest: &Seen, policy: &Upgrade, state: &State) -> Verdict {
        decide(running, latest, policy, state, NOW, TODAY)
    }

    fn skipped(v: Verdict, needle: &str) {
        match v {
            Verdict::Skip(why) => assert!(why.contains(needle), "{why}"),
            Verdict::Install => panic!("不该装(期望:{needle})"),
        }
    }

    #[test]
    fn installs_a_newer_release_within_range_once_it_is_old_enough() {
        let state = State::default();
        assert_eq!(
            verdict("0.6.34", &seen("0.6.35", 3), &policy(), &state),
            Verdict::Install
        );
        assert_eq!(
            verdict("0.6.34", &seen("0.7.0", 3), &policy(), &state),
            Verdict::Install,
            "默认跟到次版本"
        );
        // 恰好满 min_age 就采用。
        assert_eq!(
            verdict("0.6.34", &seen("0.6.35", 2), &policy(), &state),
            Verdict::Install
        );
    }

    #[test]
    fn skips_what_it_must_not_follow() {
        let state = State::default();
        skipped(
            verdict("0.6.34", &seen("0.6.34", 9), &policy(), &state),
            "已是最新",
        );
        skipped(
            verdict("0.6.35", &seen("0.6.34", 9), &policy(), &state),
            "已是最新",
        );
        skipped(
            verdict("0.6.34", &seen("1.0.0", 9), &policy(), &state),
            "主版本",
        );
        let patch = Upgrade {
            range: UpgradeRange::Patch,
            ..policy()
        };
        skipped(
            verdict("0.6.34", &seen("0.7.0", 9), &patch, &state),
            "次版本",
        );
        assert_eq!(
            verdict("0.6.34", &seen("0.6.40", 9), &patch, &state),
            Verdict::Install
        );
        skipped(
            verdict("0.6.34", &seen("0.6.35", 1), &policy(), &state),
            "未满 2 小时",
        );
        let unknown_time = Seen {
            published_secs: None,
            ..seen("0.6.35", 9)
        };
        skipped(
            verdict("0.6.34", &unknown_time, &policy(), &state),
            "发布时间",
        );
        skipped(
            verdict("0.6.34-dev", &seen("0.6.35", 9), &policy(), &state),
            "认不出",
        );
    }

    #[test]
    fn rolled_back_versions_are_never_retaken_and_held_ones_wait_a_day() {
        let rolled = State {
            rolled_back: vec!["0.6.35".into()],
            ..State::default()
        };
        skipped(
            verdict("0.6.34", &seen("0.6.35", 9), &policy(), &rolled),
            "退回过",
        );
        assert_eq!(
            verdict("0.6.34", &seen("0.6.36", 9), &policy(), &rolled),
            Verdict::Install
        );
        let mut held = State::default();
        held.hold("0.6.35", TODAY, "装包失败");
        skipped(
            verdict("0.6.34", &seen("0.6.35", 9), &policy(), &held),
            "今天不再试",
        );
        held.hold("0.6.35", "2033-05-17", "装包失败");
        assert_eq!(
            verdict("0.6.34", &seen("0.6.35", 9), &policy(), &held),
            Verdict::Install,
            "隔天再试"
        );
    }

    #[test]
    fn checks_on_the_configured_interval() {
        let mut state = State::default();
        assert!(check_due(&state, &policy(), NOW), "从没查过");
        state.last_check_secs = Some(NOW - 5 * 3600);
        assert!(!check_due(&state, &policy(), NOW));
        state.last_check_secs = Some(NOW - 6 * 3600);
        assert!(check_due(&state, &policy(), NOW));
        let zero = Upgrade {
            check_interval_hours: 0,
            ..policy()
        };
        state.last_check_secs = Some(NOW - 1800);
        assert!(!check_due(&state, &zero, NOW), "间隔至少 1 小时");
        state.last_check_secs = Some(NOW + 60);
        assert!(check_due(&state, &policy(), NOW), "时钟回拨按该问算");
    }

    #[test]
    fn the_idle_clock_restarts_on_any_activity() {
        let need = Duration::from_secs(600);
        let t0 = UNIX_EPOCH + Duration::from_secs(NOW);
        let mut clock = IdleClock::default();
        assert!(!clock.tick(true, None, t0, need));
        assert!(clock.tick(true, None, t0 + need, need));
        // 一次不空就从头算。
        assert!(!clock.tick(false, None, t0 + need, need));
        assert!(!clock.tick(true, None, t0 + need * 2 - Duration::from_secs(1), need));
        // 采样之间有会话处理过消息:从那一刻起算。
        let mut clock = IdleClock::default();
        assert!(!clock.tick(true, None, t0, need));
        let activity = t0 + Duration::from_secs(300);
        assert!(!clock.tick(true, Some(activity), t0 + need, need));
        assert!(clock.tick(true, Some(activity), activity + need, need));
    }

    fn record(started: bool) -> InboxRecord {
        serde_json::from_value(serde_json::json!({
            "agent": "a",
            "inbound": {"chat_id": "oc_1", "chat_type": "group", "message_id": "om_1",
                        "text": "hi", "sender": "ou_1", "sender_union_id": "",
                        "mentions": [], "resources": []},
            "started": started,
        }))
        .expect("合成 WAL 记录")
    }

    fn turn(tag: &str) -> BusyTurn {
        BusyTurn {
            agent: "a".into(),
            tag: tag.into(),
        }
    }

    #[test]
    fn the_machine_is_quiet_only_with_nothing_running_or_queued() {
        assert_eq!(quiet_from(&[], &[], &[], Ok(vec![])), Ok(()));
        let busy = quiet_from(&[turn("g1")], &[], &[], Ok(vec![]));
        assert!(busy.unwrap_err().contains("在跑一轮:a/g1"));
        let sub = quiet_from(&[], &[turn("g2")], &[], Ok(vec![]));
        assert!(sub.unwrap_err().contains("子 agent"));
        let handling = quiet_from(&[], &[], &[turn("g3")], Ok(vec![]));
        assert!(handling.unwrap_err().contains("正在处理消息"));
        let queued = quiet_from(&[], &[], &[], Ok(vec![record(false)]));
        assert!(queued.unwrap_err().contains("1 条排队"));
        // 已开跑而没销账的旧记录不挡;WAL 读不出按不空算。
        assert_eq!(quiet_from(&[], &[], &[], Ok(vec![record(true)])), Ok(()));
        let unreadable = quiet_from(&[], &[], &[], Err(anyhow::anyhow!("坏了")));
        assert!(unreadable.unwrap_err().contains("读不出"));
    }

    /// `RuntimeStatus` 快照(Menu Bar、fleet 读的那份)不含「正在处理」的登记:评论起草的
    /// `comment-…` 登记不会冒成会话。
    #[test]
    fn handling_entries_never_reach_the_fleet_snapshot() {
        let _drafting =
            crate::observe::runtime_status::enter_handling("t-snapshot", "comment-0123456789ab");
        let snapshot = crate::observe::runtime_status::snapshot(1, None, SystemTime::now());
        let mentions = |turns: &[BusyTurn]| turns.iter().any(|t| t.agent == "t-snapshot");
        assert!(!mentions(&snapshot.busy));
        assert!(!mentions(&snapshot.subagents));
        assert!(!snapshot
            .live_headless
            .unwrap_or_default()
            .iter()
            .any(|s| s.agent == "t-snapshot"));
    }
}
