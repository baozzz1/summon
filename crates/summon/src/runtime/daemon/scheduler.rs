//! 定时器 scheduler 线程(P2-自动化):daemon 内单条 detached 线程,~30s tick,读 schedules.yaml,
//! 经 due_plan 选到期项,经 Core::dispatch_scheduled 投 ScheduledFire 到目标 agent 的 dispatcher,
//! 回写 last_fired。**红线:只产 ScheduledFire,绝不碰 chat 热路径 / tmux。**

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use chrono::{DateTime, Local};
use summon_core::{due_plan, Schedule};

use super::{log, DispatchMsg};
use crate::runtime::core::Core;
use crate::runtime::inbox_wal::{InboxWal, ScheduledAppend};
use crate::schedule::schedule_store::{DueClaim, ScheduleStore};

const TICK: Duration = Duration::from_secs(30);
const MAX_PER_TICK: usize = 32; // 抗雪崩

/// 纯收口:只扫到期 schedule，不提前 checkpoint。读取失败向上传递，禁止把损坏 yaml 当空列表。
fn collect_due(st: &ScheduleStore, now: DateTime<Local>) -> Result<Vec<Schedule>> {
    let mut out = Vec::new();
    for s in st.load()? {
        let p = due_plan(&s, now);
        if !p.fire_now {
            continue;
        }
        out.push(s);
    }
    Ok(out)
}

/// 同一 schedule 在 checkpoint 成功前始终生成相同 fire_id。WAL 以它作幂等键，checkpoint
/// 失败后的 tick 只能补状态、不能再次路由同一个逻辑 fire。
///
/// 编号由上次触发时刻(代际)与内容改动次数(修订号)共同决定。一次性定时器改时间会把
/// `last_fired` 清回「没跑过」、代际回到 0,接受之后、checkpoint 之前的编辑也不推进代际;修订号让
/// 每次内容改动后的触发都有自己的编号。修订号为 0 时编号省掉这一段。
fn fire_id_for(s: &Schedule) -> String {
    let generation = s
        .last_fired
        .as_deref()
        .and_then(|last| DateTime::parse_from_rfc3339(last).ok())
        .map(|last| last.timestamp_millis().max(0) as u64)
        .unwrap_or(0);
    match s.revision {
        0 => format!("sched-{}-{generation}", s.id),
        revision => format!("sched-{}-{generation}-r{revision}", s.id),
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
struct TickStats {
    checkpointed: usize,
    no_route: usize,
    checkpoint_failed: usize,
    wal_failed: usize,
    deduplicated: usize,
    superseded: usize,
    load_failed: bool,
}

/// 一次 tick 的 durable-before-route 次序：先以 stable fire_id 幂等写 inbox WAL，再投 dispatcher，
/// 最后原子 checkpoint。checkpoint 失败时 pending/consumed WAL 是交付事实，下一 tick 只补
/// checkpoint、不重复路由。无路由则撤销本次新 WAL，让 schedule 保持 due 后续重试。
fn dispatch_due(
    st: &ScheduleStore,
    wal: &InboxWal,
    delivery_scopes: &std::collections::HashMap<String, String>,
    now: DateTime<Local>,
    mut dispatch: impl FnMut(&Schedule, &str) -> bool,
) -> TickStats {
    let due = match collect_due(st, now) {
        Ok(due) => due,
        Err(e) => {
            log(&summon_i18n::format!("r3eed8202ad"));
            return TickStats {
                load_failed: true,
                ..TickStats::default()
            };
        }
    };
    let mut stats = TickStats::default();
    for schedule in due {
        // MAX 只限制已经持久接受的 fire；无路由旧 schedule 不占预算，否则文件前 32 条坏路由
        // 会让后续有效 schedule 永久饥饿。
        if stats.checkpointed + stats.checkpoint_failed + stats.superseded >= MAX_PER_TICK {
            break;
        }
        let fire_id = fire_id_for(&schedule);
        let Some(delivery_scope) = delivery_scopes.get(&schedule.agent) else {
            stats.no_route += 1;
            log(&summon_i18n::format!("rb0800067a3", schedule.agent));
            continue;
        };
        let claim = st.claim_due(&schedule, now, |current, _plan| {
            wal.append_scheduled(
                &current.agent,
                delivery_scope,
                &current.chat_id,
                &current.chat_type,
                &current.prompt,
                current.output,
                current.context,
                &fire_id,
                now.timestamp_millis(),
            )
        });
        let (schedule, plan, append) = match claim {
            Ok(DueClaim::Stale) => continue,
            Ok(DueClaim::Accepted {
                schedule,
                plan,
                value,
            }) => (schedule, plan, value),
            Err(e) => {
                stats.wal_failed += 1;
                log(&summon_i18n::format!("r89136aa97a"));
                continue;
            }
        };
        match append {
            ScheduledAppend::Inserted => {
                if !dispatch(&schedule, &fire_id) {
                    stats.no_route += 1;
                    if let Err(e) = wal.discard_unrouted_scheduled(&schedule.agent, &fire_id) {
                        stats.wal_failed += 1;
                        log(&summon_i18n::format!("r597e88c9b9"));
                    }
                    log(&summon_i18n::format!("rfdacc35fa1", schedule.agent));
                    continue;
                }
            }
            ScheduledAppend::Pending | ScheduledAppend::Consumed => {
                stats.deduplicated += 1;
                // 这个 fire 已有持久交付事实。pending 由 chat 完成后销账、启动时自动重注入；
                // consumed 表示 turn 已完成。两者都只补 checkpoint，绝不能再进 dispatcher。
            }
        }
        match st.record_fire_if_unchanged(&schedule, &now.to_rfc3339(), plan.disable) {
            Ok(true) => stats.checkpointed += 1,
            Ok(false) => stats.superseded += 1,
            Err(e) => {
                stats.checkpoint_failed += 1;
                log(&summon_i18n::format!("re2f7ba28a7"));
            }
        }
    }
    stats
}

/// 一次 tick 的**完整接线**(生产循环与集成测试共用同一份):选到期项 → 构造 `ScheduledFire`
/// → 经 `Core` 按 agent 别名投给该 dispatcher。抽出来是为了让「真的产出并投递」这段可被
/// 测试穿过——生产循环里只剩节拍与存储位置。**这里是 scheduler 唯一的对外动作面**。
fn tick_once(
    core: &Core,
    st: &ScheduleStore,
    wal: &InboxWal,
    delivery_scopes: &std::collections::HashMap<String, String>,
    now: DateTime<Local>,
) -> TickStats {
    dispatch_due(st, wal, delivery_scopes, now, |s, fire_id| {
        core.dispatch_scheduled(
            &s.agent,
            DispatchMsg::ScheduledFire {
                chat_id: s.chat_id.clone(),
                chat_type: s.chat_type.clone(),
                prompt: s.prompt.clone(),
                fire_id: fire_id.to_string(),
                output: s.output,
                context: s.context,
            },
        )
    })
}

/// agent 别名 → 其飞书 delivery scope。运行期新增 agent 要往里插:表里没有该 agent 时,
/// 它的定时器 tick 会被静默跳过(查不到 scope 就不知道往哪个 app 身份投)。
pub(super) type DeliveryScopes = Arc<std::sync::RwLock<std::collections::HashMap<String, String>>>;

/// 起 detached scheduler 线程。`run()` 里 dispatcher / outbox-drain 都起好后调。
pub(super) fn spawn_scheduler(
    core: Arc<Core>,
    stop: Arc<AtomicBool>,
    delivery_scopes: DeliveryScopes,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let st = ScheduleStore::at_summon_home();
        let wal = InboxWal::new(crate::runtime::paths::run_dir().join("inbox"));
        while !stop.load(Ordering::Relaxed) {
            // 每 tick 现取读锁,拿到的就是当下最新的 scope 表(含刚上线的 agent)。
            // 锁中毒 → 本 tick 跳过(空表),下一 tick 再试,不 panic 掉 scheduler 线程。
            let scopes = delivery_scopes
                .read()
                .map(|s| s.clone())
                .unwrap_or_default();
            tick_once(&core, &st, &wal, &scopes, Local::now());
            let mut slept = Duration::ZERO;
            while slept < TICK && !stop.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_secs(1));
                slept += Duration::from_secs(1);
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn once_schedule(id: &str, spec: String) -> Schedule {
        Schedule {
            id: id.into(),
            kind: summon_core::Kind::Once,
            spec,
            agent: "a".into(),
            chat_id: "c".into(),
            chat_type: "p2p".into(),
            prompt: "p".into(),
            created_by: "ou".into(),
            created_at: "2026-06-14 00:00:00".into(),
            output: summon_core::ScheduleOutput::Progress,
            context: summon_core::ScheduleContext::Resume,
            enabled: true,
            last_fired: None,
            revision: 0,
        }
    }

    /// 只挂 `agent_routes` 的最小 Core:`dispatch_scheduled` 就是查这张表投递。
    fn core_routing_to(alias: &str, tx: std::sync::mpsc::Sender<DispatchMsg>) -> Core {
        Core::new(
            std::collections::HashMap::new(),
            std::collections::HashMap::from([(alias.to_string(), tx)]),
            Vec::new(),
        )
    }

    #[test]
    fn disabled_schedule_is_checkpointed_and_not_replayed_after_enable() {
        let root = tempfile::tempdir().unwrap();
        let store = ScheduleStore::new(root.path());
        let inbox_root = root.path().join("inbox");
        let wal = InboxWal::new(inbox_root.clone());
        let scopes = std::collections::HashMap::from([("a".into(), "scope-a".into())]);
        let now = Local::now();
        store
            .add(&once_schedule(
                "SCH-17",
                (now - chrono::Duration::minutes(1)).to_rfc3339(),
            ))
            .unwrap();
        crate::runtime::agent_availability::AgentAvailability::lock(&inbox_root, "a")
            .unwrap()
            .set_enabled(false)
            .unwrap();
        let stats = dispatch_due(&store, &wal, &scopes, now, |schedule, fire_id| {
            assert!(wal
                .reject_disabled(&schedule.agent, "scope-a", fire_id)
                .unwrap());
            true
        });
        assert_eq!(stats.checkpointed, 1);
        assert!(wal.scan_pending().is_empty());
        assert_eq!(
            std::fs::read_dir(root.path().join("outbox/pending"))
                .unwrap()
                .count(),
            1
        );
        let saved = store.load().unwrap();
        assert!(!saved[0].enabled);
        assert!(saved[0].last_fired.is_some());
        crate::runtime::agent_availability::AgentAvailability::lock(&inbox_root, "a")
            .unwrap()
            .set_enabled(true)
            .unwrap();
        let next = dispatch_due(
            &store,
            &wal,
            &scopes,
            now + chrono::Duration::minutes(2),
            |_, _| panic!("已跳过的触发不得补跑"),
        );
        assert_eq!(next.checkpointed, 0);
    }

    /// 「到点了 scheduler 真的产出 ScheduledFire 并投给 dispatcher」——此前只有 `dispatch_due`
    /// 的注入闭包被测过,`Core::dispatch_scheduled` + `ScheduledFire` 载荷这段接线零覆盖:把
    /// chat_id/prompt 写错、或改投别的 DispatchMsg,全量门禁照样绿。这里穿真 Core 路由表。
    #[test]
    fn due_schedule_routes_scheduled_fire_payload_through_core() {
        let tmp = tempfile::tempdir().unwrap();
        let st = ScheduleStore::new(tmp.path());
        let wal = InboxWal::new(tmp.path().join("inbox"));
        let scopes = std::collections::HashMap::from([("a".into(), "test-scope".into())]);
        let (tx, rx) = std::sync::mpsc::channel();
        let core = core_routing_to("a", tx);

        let past = (Local::now() - chrono::Duration::minutes(1)).to_rfc3339();
        let mut due = once_schedule("SCH-DUE", past);
        due.chat_id = "oc_due".into();
        due.chat_type = "group".into();
        due.prompt = "该写周报了".into();
        due.output = summon_core::ScheduleOutput::Result;
        st.add(&due).unwrap();
        let due_fire_id = fire_id_for(&due);
        // 未到点的那条必须一动不动(同一 tick 里的对照组)。
        let future = (Local::now() + chrono::Duration::hours(2)).to_rfc3339();
        let mut later = once_schedule("SCH-LATER", future);
        later.chat_id = "oc_later".into();
        st.add(&later).unwrap();

        let stats = tick_once(&core, &st, &wal, &scopes, Local::now());
        assert_eq!(stats.checkpointed, 1);

        match rx.try_recv() {
            Ok(DispatchMsg::ScheduledFire {
                chat_id,
                chat_type,
                prompt,
                fire_id,
                output,
                context,
            }) => {
                assert_eq!(chat_id, "oc_due");
                assert_eq!(context, summon_core::ScheduleContext::Resume);
                assert_eq!(chat_type, "group");
                assert_eq!(prompt, "该写周报了");
                assert_eq!(fire_id, due_fire_id);
                assert_eq!(output, summon_core::ScheduleOutput::Result);
            }
            Ok(_) => panic!("scheduler 只允许投 ScheduledFire,收到了别的 DispatchMsg"),
            Err(e) => panic!("到点必须投出一条 ScheduledFire,却什么都没收到:{e}"),
        }
        assert!(rx.try_recv().is_err(), "未到点的 schedule 不得产出任何投递");
        // durable-before-route:这条 fire 在 WAL 里有持久记录(daemon 崩了也能重注入)。
        assert!(
            wal.scan_pending().iter().any(|record| record
                .scheduled
                .as_ref()
                .is_some_and(|s| s.fire_id == due_fire_id && s.prompt == "该写周报了")),
            "投递前必须先落 inbox WAL"
        );
        // 原子 checkpoint:到点的写回 last_fired 且 Once 触发后停用;未到点的原样保留。
        let saved = st.load().unwrap();
        let fired = saved.iter().find(|s| s.id == "SCH-DUE").unwrap();
        assert!(fired.last_fired.is_some());
        assert!(!fired.enabled, "Kind::Once 触发后必须停用");
        let untouched = saved.iter().find(|s| s.id == "SCH-LATER").unwrap();
        assert!(untouched.enabled);
        assert!(untouched.last_fired.is_none());

        // 连跑第二个 tick:同一逻辑 fire 不得重复投(checkpoint + WAL 幂等键各挡一层)。
        let again = tick_once(&core, &st, &wal, &scopes, Local::now());
        assert_eq!(again.checkpointed, 0);
        assert!(
            rx.try_recv().is_err(),
            "已 checkpoint 的 Once schedule 不得再次投递"
        );
    }

    /// 红线(CLAUDE.md「定时器」行):scheduler 只产 `ScheduledFire`,绝不碰 chat 热路径 / tmux。
    /// 靠调用面断言钉不住「某天有人在这里直接 spawn 会话」,所以再加一道结构守护
    /// (同 `frontend/endpoints.rs` 里 `include_str!` 的既有做法)。注释行不参与判定。
    #[test]
    fn scheduler_only_emits_scheduled_fire_and_never_touches_chat_hot_path() {
        // 只扫生产代码(`#[cfg(test)]` 之前),否则本测试自己的禁词清单会把自己判红。
        let code = include_str!("scheduler.rs")
            .split("#[cfg(test)]")
            .next()
            .unwrap_or_default()
            .lines()
            .map(|line| line.split("//").next().unwrap_or_default())
            .collect::<Vec<_>>()
            .join("\n");
        for forbidden in [
            "handle_task",
            "chat_thread",
            "spawn_chat_thread",
            "tmux",
            "Session",
            "kill",
        ] {
            assert!(
                !code.contains(forbidden),
                "scheduler 红线:代码里不得出现 {forbidden}(只产 ScheduledFire,不碰 chat 热路径/tmux)"
            );
        }
        let mut variants = 0;
        for (at, _) in code.match_indices("DispatchMsg::") {
            variants += 1;
            assert!(
                code[at..].starts_with("DispatchMsg::ScheduledFire"),
                "scheduler 只允许构造 ScheduledFire 一种 DispatchMsg,发现别的变体"
            );
        }
        assert!(variants > 0, "守护本身要能看到 DispatchMsg 的构造点");
    }

    #[test]
    fn scheduler_checkpoints_only_after_route_success() {
        use chrono::Local;
        let tmp = tempfile::tempdir().unwrap();
        let st = crate::schedule::schedule_store::ScheduleStore::new(tmp.path());
        let wal = InboxWal::new(tmp.path().join("inbox"));
        let scopes = std::collections::HashMap::from([("a".into(), "test-scope".into())]);
        let id = st.next_id().unwrap();
        let past = (Local::now() - chrono::Duration::minutes(1)).to_rfc3339();
        st.add(&once_schedule(&id, past)).unwrap();

        let no_route = dispatch_due(&st, &wal, &scopes, Local::now(), |_schedule, _fire_id| {
            false
        });
        assert_eq!(no_route.no_route, 1);
        let still_due = st.load().unwrap();
        assert!(still_due[0].enabled);
        assert!(still_due[0].last_fired.is_none());

        let delivered = dispatch_due(&st, &wal, &scopes, Local::now(), |_schedule, _fire_id| true);
        assert_eq!(delivered.checkpointed, 1);
        let saved = st.load().unwrap();
        assert!(!saved[0].enabled);
        assert!(saved[0].last_fired.is_some());
    }

    #[test]
    fn edit_after_durable_claim_keeps_new_generation_out_of_old_checkpoint() {
        let tmp = tempfile::tempdir().unwrap();
        let st = ScheduleStore::new(tmp.path());
        let wal = InboxWal::new(tmp.path().join("inbox"));
        let scopes = std::collections::HashMap::from([("a".into(), "test-scope".into())]);
        let past = (Local::now() - chrono::Duration::minutes(1)).to_rfc3339();
        let mut schedule = once_schedule("SCH-EDIT-RACE", past);
        schedule.prompt = "旧正文".into();
        st.add(&schedule).unwrap();

        let stats = dispatch_due(&st, &wal, &scopes, Local::now(), |accepted, _| {
            assert_eq!(accepted.prompt, "旧正文");
            st.edit(
                &accepted.id,
                crate::schedule::schedule_store::SchedulePatch {
                    rhythm: Some((
                        summon_core::Kind::Once,
                        (Local::now() + chrono::Duration::hours(1)).to_rfc3339(),
                    )),
                    prompt: Some("新正文".into()),
                    ..Default::default()
                },
                &Local::now().to_rfc3339(),
            )
            .unwrap();
            true
        });
        assert_eq!(stats.superseded, 1);
        assert_eq!(stats.checkpointed, 0);
        let current = st.load().unwrap().remove(0);
        assert_eq!(current.prompt, "新正文");
        assert!(current.enabled);
        assert!(current.last_fired.is_none());
    }

    /// 跑完一次的一次性定时器再被安排(改到新时刻，或改走再改回原时刻，正文变不变都一样):
    /// 到点按编辑后的内容投递一次。
    #[test]
    fn a_fired_once_schedule_scheduled_again_fires_again() {
        enum Again {
            NewTime(&'static str),
            BackToTheOriginalTime,
        }
        for again in [
            Again::NewTime("新正文"),
            Again::NewTime("旧正文"),
            Again::BackToTheOriginalTime,
        ] {
            let tmp = tempfile::tempdir().unwrap();
            let st = ScheduleStore::new(tmp.path());
            let wal = InboxWal::new(tmp.path().join("inbox"));
            let scopes = std::collections::HashMap::from([("a".into(), "test-scope".into())]);
            let now = Local::now();
            let original = (now - chrono::Duration::minutes(1)).to_rfc3339();
            let mut schedule = once_schedule("SCH-153", original.clone());
            schedule.prompt = "旧正文".into();
            st.add(&schedule).unwrap();
            let mut first = None;
            dispatch_due(&st, &wal, &scopes, now, |_, fire_id| {
                first = Some(fire_id.to_string());
                true
            });
            let first = first.expect("第一次到点要投递");
            wal.mark_started("a", "test-scope", &first).unwrap();
            wal.mark_consumed("a", &first);

            let later = now + chrono::Duration::hours(1);
            let move_to = |spec: String, prompt: &str| {
                st.edit(
                    "SCH-153",
                    crate::schedule::schedule_store::SchedulePatch {
                        rhythm: Some((summon_core::Kind::Once, spec)),
                        prompt: Some(prompt.into()),
                        enabled: Some(true),
                        ..Default::default()
                    },
                    &now.to_rfc3339(),
                )
                .unwrap();
            };
            let (expected_prompt, fire_at) = match again {
                Again::NewTime(prompt) => {
                    move_to(later.to_rfc3339(), prompt);
                    (prompt, later + chrono::Duration::minutes(1))
                }
                Again::BackToTheOriginalTime => {
                    move_to(later.to_rfc3339(), "旧正文");
                    move_to(original.clone(), "旧正文");
                    ("旧正文", now)
                }
            };
            let mut routed = Vec::new();
            let stats = dispatch_due(&st, &wal, &scopes, fire_at, |schedule, fire_id| {
                routed.push((schedule.prompt.clone(), fire_id.to_string()));
                true
            });
            assert_eq!(stats.wal_failed, 0, "{expected_prompt}");
            assert_eq!(stats.deduplicated, 0, "{expected_prompt}");
            assert_eq!(routed.len(), 1, "{expected_prompt}");
            assert_eq!(routed[0].0, expected_prompt);
            assert_ne!(routed[0].1, first);
        }
    }

    /// 接受之后、checkpoint 之前被编辑(正文、输出方式、上下文模式任一项):这一代不 checkpoint,
    /// 下一个 tick 按编辑后的内容投递。
    #[test]
    fn a_schedule_edited_after_acceptance_fires_with_the_edit() {
        let patches = [
            crate::schedule::schedule_store::SchedulePatch {
                prompt: Some("新正文".into()),
                ..Default::default()
            },
            crate::schedule::schedule_store::SchedulePatch {
                output: Some(summon_core::ScheduleOutput::Result),
                ..Default::default()
            },
            crate::schedule::schedule_store::SchedulePatch {
                context: Some(summon_core::ScheduleContext::Fresh),
                ..Default::default()
            },
        ];
        for patch in patches {
            let tmp = tempfile::tempdir().unwrap();
            let st = ScheduleStore::new(tmp.path());
            let wal = InboxWal::new(tmp.path().join("inbox"));
            let scopes = std::collections::HashMap::from([("a".into(), "test-scope".into())]);
            let past = (Local::now() - chrono::Duration::minutes(1)).to_rfc3339();
            st.add(&once_schedule("SCH-EDITED", past)).unwrap();
            let mut patch = Some(patch);
            let stats = dispatch_due(&st, &wal, &scopes, Local::now(), |accepted, _| {
                st.edit(
                    &accepted.id,
                    patch.take().unwrap(),
                    &Local::now().to_rfc3339(),
                )
                .unwrap();
                true
            });
            assert_eq!(stats.superseded, 1);
            let edited = st.load().unwrap().remove(0);

            let mut routed = Vec::new();
            let next = dispatch_due(&st, &wal, &scopes, Local::now(), |schedule, _| {
                routed.push(schedule.clone());
                true
            });
            assert_eq!(next.wal_failed, 0);
            assert_eq!(routed, vec![edited]);
        }
    }

    #[test]
    fn checkpoint_failure_keeps_schedule_due_and_fire_id_stable() {
        let tmp = tempfile::tempdir().unwrap();
        let st = ScheduleStore::new(tmp.path());
        let wal = InboxWal::new(tmp.path().join("inbox"));
        let scopes = std::collections::HashMap::from([("a".into(), "test-scope".into())]);
        let past = (Local::now() - chrono::Duration::minutes(1)).to_rfc3339();
        st.add(&once_schedule("SCH-1", past)).unwrap();
        st.fail_next_save_for_test();
        let mut ids = Vec::new();
        let stats = dispatch_due(&st, &wal, &scopes, Local::now(), |_schedule, fire_id| {
            ids.push(fire_id.to_string());
            true
        });
        assert_eq!(stats.checkpoint_failed, 1);
        let saved = st.load().unwrap();
        assert!(saved[0].enabled);
        assert!(saved[0].last_fired.is_none());

        let retry = dispatch_due(&st, &wal, &scopes, Local::now(), |_schedule, fire_id| {
            ids.push(fire_id.to_string());
            true
        });
        assert_eq!(retry.checkpointed, 1);
        assert_eq!(retry.deduplicated, 1);
        assert_eq!(ids, vec!["sched-SCH-1-0"], "同一 pending fire 不得重复路由");
    }

    #[test]
    fn corrupt_store_does_not_dispatch() {
        let tmp = tempfile::tempdir().unwrap();
        let st = ScheduleStore::new(tmp.path());
        let wal = InboxWal::new(tmp.path().join("inbox"));
        let scopes = std::collections::HashMap::from([("a".into(), "test-scope".into())]);
        std::fs::write(tmp.path().join("schedules.yaml"), "not: [yaml").unwrap();
        let mut called = false;
        let stats = dispatch_due(&st, &wal, &scopes, Local::now(), |_schedule, _fire_id| {
            called = true;
            true
        });
        assert!(stats.load_failed);
        assert!(!called);
    }

    #[test]
    fn no_route_entries_do_not_starve_later_due_schedule() {
        let tmp = tempfile::tempdir().unwrap();
        let st = ScheduleStore::new(tmp.path());
        let wal = InboxWal::new(tmp.path().join("inbox"));
        let scopes = std::collections::HashMap::from([
            ("missing".into(), "test-scope".into()),
            ("valid".into(), "test-scope".into()),
        ]);
        let past = (Local::now() - chrono::Duration::minutes(1)).to_rfc3339();
        let schedules: Vec<_> = (1..=33)
            .map(|n| {
                let mut schedule = once_schedule(&format!("SCH-{n}"), past.clone());
                schedule.agent = if n == 33 { "valid" } else { "missing" }.into();
                schedule
            })
            .collect();
        // 一次写下整份启用中的定时器文件:逐条 add 是 33 轮带 fsync 的事务写,被测的只是 dispatch。
        std::fs::write(
            tmp.path().join("schedules.yaml"),
            serde_yaml::to_string(&schedules).unwrap(),
        )
        .unwrap();
        assert_eq!(st.load().unwrap(), schedules);

        let mut routed = Vec::new();
        let stats = dispatch_due(&st, &wal, &scopes, Local::now(), |schedule, _| {
            if schedule.agent == "valid" {
                routed.push(schedule.id.clone());
                true
            } else {
                false
            }
        });

        assert_eq!(stats.no_route, 32);
        assert_eq!(stats.checkpointed, 1);
        assert_eq!(routed, vec!["SCH-33"]);
        let saved = st.load().unwrap();
        assert!(saved
            .iter()
            .find(|s| s.id == "SCH-33")
            .is_some_and(|s| !s.enabled));
    }
}
