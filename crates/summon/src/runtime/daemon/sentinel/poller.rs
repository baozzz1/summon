//! sentinel 常驻轮询线程:daemon 内单条 detached 线程,骨架同 scheduler(1s 分片睡 + stop 位),
//! 每 tick 重读 Config(热重载,`enabled=false` 时空转)。
//!
//! 三通道触发检测:
//! - **fast tail**(主):每 5s 取少量未勿扰的活跃会话,按水位增量扫新消息;
//! - **slow tail**(补偿):每分钟刷新较大的活跃集,每个 fast tick 只分批扫少量、按最久未扫优先;
//! - **搜索**(兜底):每分钟只搜索规范 `/summon` 入口(覆盖 tail 长尾,索引延迟 ~21s)。
//!
//! 三通道按 message_id 合流去重;冷启轮(首次运行/状态文件丢失)**只建水位不动作**——
//! boot 补发事故的同构防御:判据必须落「时间窗 + 已见集合」,不能只看技术条件。
//!
//! **红线:只产 SentinelFire 与 lark-cli 调用,绝不碰会话热路径与终端会话层**(文件底部有
//! 结构性测试自扫源码钉死这条)。

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use super::super::log;
use super::super::outbox_drain::DrainAgents;
use super::actions::{self, PendingTrigger};
use super::lark::{ChatBrief, LarkUser};
use crate::runtime::core::Core;
use summon_core::config::Sentinel as SentinelCfg;
use summon_core::sentinel::gate::{self, GateState};
use summon_core::sentinel::trigger::parse_trigger;

/// 快速轮询下限:lark-cli 是逐次起进程的 shell-out,再密只会把延迟花在进程启动上。
const FAST_TICK_FLOOR_SECS: u64 = 3;
/// 全文索引实测约 21s 才可见；慢通道低于 30s 没有收益。
const SLOW_REFRESH_FLOOR_SECS: u64 = 30;
/// 每个会话每 tick 只取最新一页。
const TAIL_PAGE: usize = 20;

/// fast 通道并发拉 tail 的路数。
///
/// **为什么并发**:一轮的成本几乎全在网络往返上——实测 `lark-cli --help`(纯 fork)0.10s、
/// `auth status`(命中本地 token)0.11s,而一次 `+chat-messages-list` 要 1.7s,也就是 ~0.1s 进程
/// 加 ~1.6s 往返。这些往返彼此独立,串行只是白等:名单 5 个群就是 8s,而 `fast_interval_secs`
/// 才 5s——cadence 是补偿式的(见主循环 `remaining_interval`),于是真实周期退化成「一轮耗时」。
///
/// **为什么是 6 而不是更大**:fast 名单被 `fast_top_k` 与勿扰过滤压到个位数,6 路已经能把
/// 常见规模压到一轮往返;再大只是给飞书网关加瞬时压力,换不到墙钟时间。owner 的网络本身
/// 在抖(2026-08-12 一天 10 次 tail 超时/TLS 错),不宜再加并发放大失败面。
const TAIL_FETCH_CONCURRENCY: usize = 6;
/// mget 单批上限(API 约束)。
const MGET_BATCH: usize = 50;
/// 搜索通道只认规范入口。自然语言触发仍由 fast/slow tail 本地解析。
const CANONICAL_SEARCH_QUERY: &str = "/summon";

/// 低优先级 tail 的公平队列。`last_scanned` 只影响性能调度，不参与触发正确性，故无需持久化；
/// 真正的正确性水位仍由 [`GateState`] 落盘。
#[derive(Default)]
struct SlowLane {
    queue: VecDeque<ChatBrief>,
    last_scanned: HashMap<String, i64>,
    last_refresh_ms: i64,
    /// 本轮启用 sentinel 的精确时刻；比持久化秒级 search_since 更适合作为“不得复放”的边界。
    boot_ms: i64,
}

impl SlowLane {
    fn refresh_due(&self, now_ms: i64, interval_secs: u64) -> bool {
        self.last_refresh_ms == 0 || now_ms - self.last_refresh_ms >= interval_secs as i64 * 1000
    }

    fn mark_refreshed(&mut self, now_ms: i64) {
        self.last_refresh_ms = now_ms;
    }

    /// 当前活跃集按「从未扫 → 最久未扫」排序，既优先新出现的勿扰群，也保证老会话不会饿死。
    fn refresh(&mut self, mut chats: Vec<ChatBrief>, fast_ids: &HashSet<String>) {
        chats.retain(|c| !fast_ids.contains(&c.chat_id));
        chats.sort_by_key(|c| {
            self.last_scanned
                .get(&c.chat_id)
                .copied()
                .unwrap_or(i64::MIN)
        });
        self.queue = chats.into();
    }

    fn exclude(&mut self, ids: &HashSet<String>) {
        self.queue.retain(|c| !ids.contains(&c.chat_id));
    }

    fn take_batch(&mut self, size: usize) -> Vec<ChatBrief> {
        let n = size.max(1).min(self.queue.len());
        self.queue.drain(..n).collect()
    }

    fn note_scanned(&mut self, chat_id: &str, now_ms: i64) {
        self.last_scanned.insert(chat_id.to_string(), now_ms);
    }
}

/// 闸门状态落盘。损坏时**当冷启处理**(重建水位、本轮不动作)——比报错停摆更符合
/// fail-closed:丢的是"已见集合",而冷启语义正是"不信任已见集合"。
struct GateStore {
    path: std::path::PathBuf,
}

impl GateStore {
    fn at_run_dir() -> Self {
        Self {
            path: crate::runtime::paths::run_dir().join("sentinel-gate.json"),
        }
    }

    fn load(&self) -> GateState {
        std::fs::read_to_string(&self.path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn save(&self, state: &GateState) -> Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string(state)?)?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }
}

fn remaining_interval(interval: Duration, elapsed: Duration) -> Duration {
    interval.saturating_sub(elapsed)
}

fn after_boot(create_time_ms: i64, boot_ms: i64) -> bool {
    create_time_ms > boot_ms.max(0)
}

pub(crate) fn spawn_sentinel(
    core: Arc<Core>,
    drain: DrainAgents,
    stop: Arc<AtomicBool>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        log(summon_i18n::text!("rf82fc17e3f"));
        let mut slow_lane = SlowLane::default();
        let mut failures = summon_core::sentinel::failure_log::FailureLog::default();
        while !stop.load(Ordering::Relaxed) {
            let tick_started = Instant::now();
            // sentinel 保留每 tick 热重读语义；本轮加载一次完整配置，同时解析 tools.lark_cli。
            // resolver 的普通热路径只读内存快照，不能为了 sentinel 把所有 spawn 都变成磁盘 IO。
            let root_cfg = summon_core::config::Config::load();
            let lark_cli =
                summon_core::tools::resolve_from(&root_cfg, summon_core::tools::Tool::LarkCli);
            let cfg = root_cfg.sentinel;
            if cfg.enabled {
                match tick_once(&core, &drain, &cfg, &lark_cli, &stop, &mut slow_lane) {
                    Ok(()) => {
                        if let Some(line) = failures.succeeded() {
                            log(&line);
                        }
                    }
                    Err(e) => {
                        if let Some(line) = failures.failed(&format!("{e:#}"), Instant::now()) {
                            log(&line);
                        }
                        // 授权缺口是**有明确补救动作**的失败,不该只躺在日志里:2026-08-07 现场,
                        // sentinel 因 owner 用户身份掉线整体停摆两小时,飞书侧毫无表示,owner 只
                        // 觉得「@ 了没反应」。这里私聊 owner 一张卡,把缺什么、怎么补原样告诉他。
                        actions::notify_auth_gap_once(&drain, &e);
                    }
                }
            } else {
                slow_lane = SlowLane::default();
                failures = Default::default();
            }
            // 关闭态每秒只重读本地配置、无网络调用；开启后按 fast cadence 唤醒。
            let interval = Duration::from_secs(if cfg.enabled {
                cfg.fast_interval_secs.max(FAST_TICK_FLOOR_SECS)
            } else {
                1
            });
            // cadence 按 tick 起点计算；外呼已经花掉的时间不能再完整睡一遍，否则“5s 配置”
            // 会退化成“本轮耗时 + 5s”。单轮本身超过间隔时立刻进入下一轮。
            let interval = remaining_interval(interval, tick_started.elapsed());
            let mut slept = Duration::ZERO;
            while slept < interval && !stop.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_secs(1));
                slept += Duration::from_secs(1);
            }
        }
        log(summon_i18n::text!("r0b65d05d77"));
    })
}

/// 一轮完整扫描。**停机响应约束**:一轮要跑几十次 lark-cli 子进程,SIGTERM 的 join 不能等它
/// 跑完(2026-08-07 部署实测:10s 宽限超时、deploy 中止)。因此每次外呼前都检查 `stop`
/// (fast 段并发化后改为**每批之前**检查,见 [`map_in_parallel`]),命中即**直接返回且不落盘**
/// ——被打断的 tick 视同没发生:水位不推进、seen 不登记,重启后整轮重扫,无丢失也无双跑
/// (动作执行前才登记 seen)。
///
/// fast 段的 tail 拉取已并发(`TAIL_FETCH_CONCURRENCY`),slow 段与搜索通道仍串行。
fn tick_once(
    core: &Arc<Core>,
    drain: &DrainAgents,
    cfg: &SentinelCfg,
    lark_cli: &std::path::Path,
    stop: &AtomicBool,
    slow_lane: &mut SlowLane,
) -> Result<()> {
    let aborted = || stop.load(Ordering::Relaxed);
    if aborted() {
        return Ok(());
    }
    let lark = LarkUser::new(
        cfg.lark_profile.clone(),
        Some(lark_cli.to_string_lossy().to_string()),
    );
    // 触发 sender 过滤用 lark-cli 命名空间的 owner id:优先显式配置,否则 auth status 自取。
    let owner = match cfg.owner_open_id.as_deref().filter(|s| !s.is_empty()) {
        Some(o) => o.to_string(),
        None => lark
            .owner_open_id()
            .context(summon_i18n::text!("re59ca1dc3d"))?,
    };
    let (aliases, canon) = build_alias_table(&known_agents(drain), &cfg.aliases);
    if aliases.is_empty() {
        return Ok(());
    }
    let store = GateStore::at_run_dir();
    let mut state = store.load();
    let cold = gate::cold_start(&state);
    let now = chrono::Local::now().timestamp_millis();
    if slow_lane.boot_ms == 0 {
        slow_lane.boot_ms = now;
    }
    let mut candidates: Vec<PendingTrigger> = Vec::new();
    let mut chat_modes: HashMap<String, String> = HashMap::new();

    // ── 通道 A:未勿扰活跃会话 fast tail ──
    let fast_chats = lark.active_chats(cfg.fast_top_k, true)?;
    let fast_ids: HashSet<String> = fast_chats
        .iter()
        .filter(|c| !cfg.blocklist.contains(&c.chat_id))
        .map(|c| c.chat_id.clone())
        .collect();
    // 拉与判分两段:网络往返彼此独立,并发跑;判据改写 `state`/`candidates`,按原顺序串行。
    let fast_targets: Vec<&ChatBrief> = fast_chats
        .iter()
        .take(cfg.fast_top_k)
        .filter(|c| !cfg.blocklist.contains(&c.chat_id))
        .collect();
    let fetched = fetch_tails(&lark, &fast_targets, &aborted);
    // 停机:已拉到的一并丢弃,不落盘,整轮作废(与串行版语义一致)。
    if aborted() {
        return Ok(());
    }
    for FetchedTail { chat: c, msgs } in fetched {
        let Some(msgs) = msgs else { continue };
        if apply_tail(
            c,
            msgs,
            cfg,
            &owner,
            &aliases,
            &canon,
            &mut state,
            cold,
            now,
            slow_lane.boot_ms,
            &mut candidates,
        ) {
            slow_lane.note_scanned(&c.chat_id, now);
            chat_modes.insert(c.chat_id.clone(), c.chat_mode.clone());
        }
    }

    // ── 通道 B:慢速公平队列。勿扰会话只从这里进入，降低优先级但不忽略。──
    let slow_due = slow_lane.refresh_due(now, cfg.interval_secs.max(SLOW_REFRESH_FLOOR_SECS));
    if slow_due {
        // 即使刷新失败也记本次尝试，避免故障时每个 5s tick 都重锤同一接口。
        slow_lane.mark_refreshed(now);
        match lark.active_chats(cfg.top_k, false) {
            Ok(chats) => {
                let chats = chats
                    .into_iter()
                    .filter(|c| !cfg.blocklist.contains(&c.chat_id))
                    .collect();
                slow_lane.refresh(chats, &fast_ids);
            }
            Err(e) => log(&summon_i18n::format!("r28d9d1dc5f")),
        }
    }
    // 会话可能在上次刷新后刚进入 fast 头部，取批前再排除一次，避免本轮重复 shell-out。
    slow_lane.exclude(&fast_ids);
    for c in slow_lane.take_batch(cfg.slow_batch_size) {
        if aborted() {
            return Ok(());
        }
        if scan_chat(
            &lark,
            &c,
            cfg,
            &owner,
            &aliases,
            &canon,
            &mut state,
            cold,
            now,
            slow_lane.boot_ms,
            &mut candidates,
        ) {
            slow_lane.note_scanned(&c.chat_id, now);
            chat_modes.insert(c.chat_id.clone(), c.chat_mode.clone());
        }
    }

    // ── 通道 C:套件搜索兜底。只搜稳定的 `/summon`，不再按每个自然语言动作词逐次外呼。──
    if !cold && slow_due {
        let mut ids = match lark.search_ids(CANONICAL_SEARCH_QUERY, &owner) {
            Ok(got) => got,
            Err(e) => {
                log(&summon_i18n::format!("r7dc95aa6df"));
                Vec::new()
            }
        };
        if aborted() {
            return Ok(());
        }
        ids.sort();
        ids.dedup();
        // 便宜的预过滤:已见/已是 tail 候选的不再 mget。
        ids.retain(|id| {
            !state.seen.iter().any(|s| s == id) && !candidates.iter().any(|t| &t.message_id == id)
        });
        for batch in ids.chunks(MGET_BATCH) {
            if aborted() {
                return Ok(());
            }
            let msgs = match lark.mget(batch) {
                Ok(m) => m,
                Err(e) => {
                    log(&summon_i18n::format!("r086429a309"));
                    continue;
                }
            };
            for m in msgs {
                if !after_boot(m.create_time_ms, slow_lane.boot_ms)
                    || m.sender_id != owner
                    || !sender_is_human(&m)
                    || cfg.blocklist.contains(&m.chat_id)
                {
                    continue;
                }
                let Some(trig) = parse_trigger(
                    &m.text,
                    &aliases,
                    &cfg.join_verbs,
                    &cfg.create_verbs,
                    &cfg.invite_verbs,
                ) else {
                    continue;
                };
                // 搜索结果不带会话模式。`/summon` 在私聊要建群、在群聊要拉 bot，猜错不是显示
                // 偏差而是执行错误；tail 没拿到真实类型时按 id 补查，查不到就 fail-closed。
                let (chat_mode, chat_name) = match chat_modes.get(&m.chat_id) {
                    Some(mode) => (mode.clone(), m.chat_name.clone()),
                    None => {
                        if aborted() {
                            return Ok(());
                        }
                        match lark.chat(&m.chat_id) {
                            Ok(c) => {
                                let name = if m.chat_name.is_empty() {
                                    c.name
                                } else {
                                    m.chat_name.clone()
                                };
                                (c.chat_mode, name)
                            }
                            Err(e) => {
                                log(&summon_i18n::format!("r2760af2f55", m.chat_id));
                                continue;
                            }
                        }
                    }
                };
                candidates.push(PendingTrigger {
                    trigger: canonicalize(trig, &canon),
                    chat_id: m.chat_id.clone(),
                    chat_mode,
                    chat_name,
                    message_id: m.message_id.clone(),
                    create_time_ms: m.create_time_ms,
                    sender_name: m.sender_name.clone(),
                });
            }
        }
    }

    // ── 闸门 + 执行 ──
    if cold {
        state.search_since = now / 1000;
        log(&summon_i18n::format!(
            "r5e3394abf6",
            state.chat_watermarks.len()
        ));
    } else {
        // dry_run 在**执行阶段**重读:tick 开头的快照可能在扫描期间被 owner 翻转
        // (2026-08-07 实测踩中:切真发的同一分钟里,旧快照 tick 把触发按 dry-run 消费掉了)。
        // 只重读这一个开关——动作词/称呼表沿用本轮解析时的快照,避免半轮换表。
        let dry_run_now = summon_core::config::Config::load().sentinel.dry_run;
        for t in candidates {
            if aborted() {
                return Ok(()); // 未执行的候选连 seen 都没登记,重启后重新发现。
            }
            if !gate::fresh(t.create_time_ms, now, cfg.max_age_secs) {
                continue;
            }
            if !gate::dedup_admit(&mut state, &t.message_id) {
                continue;
            }
            if !dry_run_now && !gate::rate_admit(&mut state, now / 1000, cfg.per_hour) {
                log(&summon_i18n::format!(
                    "rceb634f64a",
                    cfg.per_hour,
                    t.message_id
                ));
                continue;
            }
            match actions::execute(core, drain, &lark, &t, dry_run_now) {
                Ok(desc) => log(&summon_i18n::format!(
                    "r0cfd937115",
                    if dry_run_now { "[dry-run] " } else { "" },
                    t.trigger.alias,
                    t.chat_name
                )),
                Err(e) => log(&summon_i18n::format!(
                    "rb276eed791",
                    t.message_id,
                    t.chat_id
                )),
            }
        }
    }
    store.save(&state)?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
/// 分批并发地把 `items` 各自映射成结果,保持**输入顺序**返回。
///
/// 单独抽出来有两个原因:并发本身可以脱离网络单测(见本文件 tests);以及调用方能继续按原顺序
/// 串行消费结果——判据、水位推进、候选入列都依赖顺序,并发只该出现在「等网络」这一段。
///
/// `aborted` 在**每批之前**检查一次(与串行版每次外呼前检查等价):命中即停,已完成的批次照常
/// 返回,调用方按停机语义整轮作废。批内不再检查——一批最多 `concurrency` 个在途,让它们跑完
/// 比留下半途 join 的线程更简单。
fn map_in_parallel<T, R>(
    items: &[T],
    concurrency: usize,
    aborted: &(dyn Fn() -> bool + Sync),
    f: &(dyn Fn(&T) -> R + Sync),
) -> Vec<R>
where
    T: Sync,
    R: Send,
{
    let mut out = Vec::with_capacity(items.len());
    for batch in items.chunks(concurrency.max(1)) {
        if aborted() {
            break;
        }
        // scope 保证借用在函数返回前全部 join,不需要 'static,也就不用把 lark/cfg 全塞进 Arc。
        std::thread::scope(|s| {
            let handles: Vec<_> = batch.iter().map(|it| s.spawn(move || f(it))).collect();
            for h in handles {
                match h.join() {
                    Ok(r) => out.push(r),
                    // 子线程 panic 不该带走整个 sentinel 线程:这一轮少扫一个会话,下一轮还会再来。
                    Err(_) => log(summon_i18n::text!("r240b44570f")),
                }
            }
        });
    }
    out
}

/// 一个会话的 tail 拉取结果。并发阶段只产出它,判据一律留到串行阶段。
struct FetchedTail<'a> {
    chat: &'a ChatBrief,
    msgs: Option<Vec<super::lark::MsgBrief>>,
}

/// 并发拉取 fast 名单各会话的 tail。失败只记日志并落成 `None`,不影响同批其它会话。
fn fetch_tails<'a>(
    lark: &LarkUser,
    chats: &[&'a ChatBrief],
    aborted: &(dyn Fn() -> bool + Sync),
) -> Vec<FetchedTail<'a>> {
    map_in_parallel(chats, TAIL_FETCH_CONCURRENCY, aborted, &|c: &&ChatBrief| {
        let msgs = match lark.latest_messages(&c.chat_id, TAIL_PAGE) {
            Ok(m) => Some(m),
            Err(e) => {
                log(&summon_i18n::format!("r0375988af3", c.chat_id));
                None
            }
        };
        FetchedTail { chat: c, msgs }
    })
}

/// 判据阶段:吃已经拉好的 tail,不做任何网络 IO。
///
/// 从 `scan_chat` 里拆出来是为了让「等网络」可以并发、而「推水位 / 入候选」保持串行 ——
/// 后者按会话顺序改写 `GateState`,并发化会平白引入锁和顺序不确定性。
#[allow(clippy::too_many_arguments)] // 参数各自独立(会话事实 / 配置 / 闸门状态 / 产出),合并无意义
fn apply_tail(
    c: &ChatBrief,
    msgs: Vec<super::lark::MsgBrief>,
    cfg: &SentinelCfg,
    owner: &str,
    aliases: &[String],
    canon: &HashMap<String, String>,
    state: &mut GateState,
    cold: bool,
    now: i64,
    boot_ms: i64,
    candidates: &mut Vec<PendingTrigger>,
) -> bool {
    let wm = state.chat_watermarks.get(&c.chat_id).copied();
    let max_t = msgs
        .iter()
        .map(|m| m.create_time_ms)
        .max()
        .unwrap_or(0)
        .max(wm.unwrap_or(0));
    if !cold {
        let floor = tail_floor_ms(wm, now, cfg.max_age_secs, boot_ms);
        for m in &msgs {
            // **`<` 而不是 `<=`**:飞书返回的 `create_time` 只到**分钟**精度,而水位被推到本页
            // 最大时间 —— 用严格大于的话,同一分钟内先到的消息把水位推满,后到的那条(哪怕是
            // 真召唤)就被永久吞掉。2026-08-10 实测:owner 15:18 在私聊里回复对方并 @Arcadia,
            // 同分钟内对方的消息先把水位推到 15:18:00,这条召唤于是 `<= floor` 被跳过 ——
            // 消息读到了、水位推进了、seen 里没有、日志一行没有。
            //
            // 放宽是安全的:**精确去重本来就该由 message_id 负责**(下游 `gate::dedup_admit`),
            // 时间戳只配做粗筛;回看范围另有 `gate::fresh` 的时效窗兜住。
            if m.create_time_ms < floor || m.sender_id != owner || !sender_is_human(m) {
                continue;
            }
            if let Some(trig) = parse_trigger(
                &m.text,
                aliases,
                &cfg.join_verbs,
                &cfg.create_verbs,
                &cfg.invite_verbs,
            ) {
                candidates.push(PendingTrigger {
                    trigger: canonicalize(trig, canon),
                    chat_id: c.chat_id.clone(),
                    chat_mode: c.chat_mode.clone(),
                    chat_name: c.name.clone(),
                    message_id: m.message_id.clone(),
                    create_time_ms: m.create_time_ms,
                    sender_name: m.sender_name.clone(),
                });
            }
        }
    }
    if max_t > 0 {
        state.chat_watermarks.insert(c.chat_id.clone(), max_t);
    }
    true
}

/// 串行版(拉 + 判)。slow 通道每轮只取 `slow_batch_size` 个会话(默认 2),并发省下的那一次
/// 往返抵不上多一条代码路径,继续走这条。
#[allow(clippy::too_many_arguments)] // 同 `apply_tail`:各参数职责独立
fn scan_chat(
    lark: &LarkUser,
    c: &ChatBrief,
    cfg: &SentinelCfg,
    owner: &str,
    aliases: &[String],
    canon: &HashMap<String, String>,
    state: &mut GateState,
    cold: bool,
    now: i64,
    boot_ms: i64,
    candidates: &mut Vec<PendingTrigger>,
) -> bool {
    let msgs = match lark.latest_messages(&c.chat_id, TAIL_PAGE) {
        Ok(m) => m,
        Err(e) => {
            log(&summon_i18n::format!("r0375988af3", c.chat_id));
            return false;
        }
    };
    apply_tail(
        c, msgs, cfg, owner, aliases, canon, state, cold, now, boot_ms, candidates,
    )
}

/// tail 扫描的时间下界:`create_time_ms` 必须**不早于**它才当新消息(`>=`,不是 `>`)。
///
/// **为什么是 `>=`**:飞书 `create_time` 只到分钟精度,而水位取本页最大时间 —— 严格大于会让
/// 同一分钟内的后续消息全部被吞(见调用处的实测注释)。同一条消息被重复看到由 message_id
/// 去重(`gate::dedup_admit`)负责,不该指望时间戳。
///
/// 有水位就用水位。**首见会话(无水位)用「now - 时效窗」而不是整段跳过** ——
/// 2026-08-08 事故:owner 在一个长期沉寂的私聊里发「@Arcadia 帮帮我」,而正是这条消息
/// 把该会话顶进活跃 top-K、让 sentinel 第一次看见它;旧代码此时只建水位不扫,于是
/// **必然吞掉第一条触发**,恰好吞的就是这个功能最该响应的场景。
///
/// 防御的目的是「重启后别把历史消息当新触发补发」。首见时下界取时效窗口沿与本次启动时刻
/// 的较新者：慢速分批会在启动后逐渐首见会话，启动前的消息不能因此复放；启动后把会话顶进
/// 活跃集的新消息仍会被放行。
fn tail_floor_ms(wm: Option<i64>, now_ms: i64, max_age_secs: i64, boot_ms: i64) -> i64 {
    wm.unwrap_or_else(|| now_ms - max_age_secs.max(0) * 1000)
        .max(boot_ms.max(0))
}

/// 发送者是真人吗。bot 冒不了 owner 的 open_id,这层是廉价的纵深防御;空串 = 数据未带,放行。
fn sender_is_human(m: &super::lark::MsgBrief) -> bool {
    m.sender_type.is_empty() || m.sender_type == "user"
}

/// 在线 agent 的 (name, display) 对。每 tick 现取(热加载可见)。
fn known_agents(drain: &DrainAgents) -> Vec<(String, String)> {
    let Ok(map) = drain.read() else {
        return Vec::new();
    };
    map.values()
        // **管家不可召唤**。召唤的语义是「把它拉进来作答」,而 steward 没有会话层:
        // 拉过去它只会回一句「我只认 /fleet」—— owner 白建一个群、白等一轮。
        // 要看 fleet 状态,直接找管家自己说 `/fleet` 就是了。
        .filter(|v| !v.agent.is_steward())
        .map(|v| (v.agent.name.clone(), v.agent.display.clone()))
        .collect()
}

/// 组装触发称呼表:内置 name/display + config 自定义称呼(agent name → 额外叫法,热生效)。
/// 返回 (给解析器的称呼列表, 称呼 → canonical agent name 映射)。自定义称呼指向不在线的
/// agent 名时忽略该组——fail-closed,别让触发路由进不存在的 agent。
fn build_alias_table(
    agents: &[(String, String)],
    extra: &std::collections::HashMap<String, Vec<String>>,
) -> (Vec<String>, std::collections::HashMap<String, String>) {
    let mut list: Vec<String> = Vec::new();
    let mut canon: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let add = |alias: &str,
               target: &str,
               list: &mut Vec<String>,
               canon: &mut std::collections::HashMap<String, String>| {
        let a = alias.trim();
        if a.is_empty() || canon.contains_key(a) {
            return;
        }
        list.push(a.to_string());
        canon.insert(a.to_string(), target.to_string());
    };
    for (name, display) in agents {
        add(name, name, &mut list, &mut canon);
        add(display, name, &mut list, &mut canon);
    }
    for (agent_name, nicks) in extra {
        if !agents.iter().any(|(n, _)| n == agent_name) {
            continue;
        }
        for nick in nicks {
            add(nick, agent_name, &mut list, &mut canon);
        }
    }
    (list, canon)
}

/// 把解析命中的称呼改写成 canonical agent name,后续 resolve_agent 按 name 精确路由。
fn canonicalize(
    mut trig: summon_core::sentinel::trigger::Trigger,
    canon: &std::collections::HashMap<String, String>,
) -> summon_core::sentinel::trigger::Trigger {
    if let Some(name) = canon.get(&trig.alias) {
        trig.alias = name.clone();
    }
    trig
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 判据、水位推进、候选入列都按会话顺序串行消费,所以并发**必须**保持输入顺序。
    #[test]
    fn 并发映射保持输入顺序() {
        let items: Vec<usize> = (0..25).collect();
        let out = map_in_parallel(&items, 6, &|| false, &|i: &usize| i * 2);
        let expect: Vec<usize> = items.iter().map(|i| i * 2).collect();
        assert_eq!(out, expect);
    }

    /// 本改动的**全部意义**就是这条:8 个各睡 60ms 的任务,并发 8 路应在一个往返的量级里完成。
    /// 若哪天实现退化回串行(480ms),这条会红。
    #[test]
    fn 并发映射不退化成串行() {
        let items: Vec<usize> = (0..8).collect();
        let started = std::time::Instant::now();
        let out = map_in_parallel(&items, 8, &|| false, &|_: &usize| {
            std::thread::sleep(std::time::Duration::from_millis(60));
        });
        let elapsed = started.elapsed();
        assert_eq!(out.len(), 8);
        assert!(
            elapsed < std::time::Duration::from_millis(300),
            "8 路并发跑 8 个 60ms 任务不该接近串行的 480ms,实际 {elapsed:?}"
        );
    }

    /// 停机语义:`aborted` 在每批之前检查,命中即停,不再起新批。
    /// (与串行版「每次外呼前检查」等价——批内已在途的让它跑完,比留下半途 join 的线程简单。)
    #[test]
    fn 并发映射在批间响应停机() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let items: Vec<usize> = (0..12).collect();
        let done = AtomicUsize::new(0);
        let out = map_in_parallel(
            &items,
            4,
            &|| done.load(Ordering::SeqCst) >= 4,
            &|i: &usize| {
                done.fetch_add(1, Ordering::SeqCst);
                *i
            },
        );
        assert_eq!(
            out.len(),
            4,
            "第一批 4 个跑完后 aborted 已为真,不该再起第二批"
        );
    }

    fn chat(id: &str) -> super::super::lark::ChatBrief {
        super::super::lark::ChatBrief {
            chat_id: id.to_string(),
            name: id.to_string(),
            chat_mode: "group".to_string(),
        }
    }

    #[test]
    fn 慢速队列优先最久未扫描并且不会饿死() {
        let mut lane = SlowLane::default();
        lane.note_scanned("recent", 90);
        lane.note_scanned("old", 10);
        lane.refresh(
            vec![chat("recent"), chat("never"), chat("old")],
            &Default::default(),
        );

        let batch = lane.take_batch(2);
        assert_eq!(
            batch.iter().map(|c| c.chat_id.as_str()).collect::<Vec<_>>(),
            vec!["never", "old"]
        );
        let rest = lane.take_batch(2);
        assert_eq!(rest[0].chat_id, "recent");
    }

    #[test]
    fn 慢速队列不重复扫描本轮快速通道() {
        let mut lane = SlowLane::default();
        let fast = std::collections::HashSet::from(["hot".to_string()]);
        lane.refresh(vec![chat("hot"), chat("muted")], &fast);
        assert_eq!(lane.take_batch(10)[0].chat_id, "muted");
    }

    #[test]
    fn 快速cadence扣除本轮已花时间() {
        let interval = Duration::from_secs(5);
        assert_eq!(
            remaining_interval(interval, Duration::from_secs(2)),
            Duration::from_secs(3)
        );
        assert_eq!(
            remaining_interval(interval, Duration::from_secs(8)),
            Duration::ZERO
        );
    }

    #[test]
    fn 搜索补偿同样拒绝本次启动前的消息() {
        assert!(!after_boot(9_999, 10_000));
        assert!(!after_boot(10_000, 10_000));
        assert!(after_boot(10_001, 10_000));
    }

    #[test]
    fn 称呼表_内置加自定义并归一到agent名() {
        let agents = vec![("arcadia".to_string(), "arcadia".to_string())];
        let mut extra = std::collections::HashMap::new();
        extra.insert(
            "arcadia".to_string(),
            vec!["阿卡迪亚".to_string(), "Arcadia".to_string()],
        );
        // 指向不在线 agent 的自定义组要被忽略。
        extra.insert("ghost".to_string(), vec!["幽灵".to_string()]);
        let (list, canon) = build_alias_table(&agents, &extra);
        assert!(list.contains(&"arcadia".to_string()));
        assert!(list.contains(&"阿卡迪亚".to_string()));
        assert!(!list.contains(&"幽灵".to_string()));
        assert_eq!(canon.get("阿卡迪亚").unwrap(), "arcadia");

        let trig = summon_core::sentinel::trigger::parse_trigger(
            "@阿卡迪亚 进来回答 x",
            &list,
            &["进来回答".to_string()],
            &["建群".to_string()],
            &[], // 本测试只验 alias 归一化,invite 表留空 = 改动前语义
        )
        .unwrap();
        assert_eq!(canonicalize(trig, &canon).alias, "arcadia");
    }

    /// 首见会话必须放行时效窗内的新消息 —— 把会话顶进 top-K 的往往正是那条触发消息本身。
    #[test]
    fn 首见会话放行时效窗内的消息() {
        let now = 1_000_000_000;
        let floor = tail_floor_ms(None, now, 300, 0);
        assert_eq!(floor, now - 300_000);
        // 1 分钟前发的触发:窗内,必须放行。
        assert!(now - 60_000 > floor, "首见会话不该吞掉窗内新消息");
    }

    /// 但首见会话仍然不补发历史 —— 时效窗外的一条都进不来(旧防御的目的原样保住)。
    #[test]
    fn 首见会话仍然不补发时效窗外的历史() {
        let now = 1_000_000_000;
        let floor = tail_floor_ms(None, now, 300, 0);
        assert!(now - 301_000 <= floor, "窗外历史必须被挡住");
        assert!(now - 86_400_000 <= floor, "一天前的更该被挡住");
    }

    /// 有水位时行为逐字节等同改动前:严格大于水位才算新。
    #[test]
    fn 有水位时仍以水位为准() {
        let now = 1_000_000_000;
        let wm = now - 10_000;
        assert_eq!(tail_floor_ms(Some(wm), now, 300, 0), wm);
        // 水位比时效窗更老时也用水位(否则会漏掉水位与窗沿之间的消息)。
        let old = now - 900_000;
        assert_eq!(tail_floor_ms(Some(old), now, 300, 0), old);
    }

    /// max_age 配成 0/负数时不许把下界推到未来(那会静默吞掉所有消息)。
    #[test]
    fn 时效窗非正数时下界不越过现在() {
        let now = 1_000_000_000;
        assert_eq!(tail_floor_ms(None, now, 0, 0), now);
        assert_eq!(tail_floor_ms(None, now, -60, 0), now);
    }

    #[test]
    fn 慢速首见会话不复放本次启动前的消息() {
        let now = 1_000_000_000;
        let boot_ms = now - 120_000;
        let floor = tail_floor_ms(None, now, 300, boot_ms);
        assert_eq!(floor, now - 120_000);
        assert!(now - 121_000 <= floor, "启动前消息必须被挡住");
        assert!(now - 60_000 > floor, "启动后消息必须放行");
    }

    #[test]
    fn 重启边界覆盖旧水位防止停机期间消息复放() {
        let now = 1_000_000_000;
        let boot_ms = now - 5_000;
        let old_watermark = now - 60_000;
        assert_eq!(
            tail_floor_ms(Some(old_watermark), now, 300, boot_ms),
            boot_ms
        );
    }

    /// **同一分钟内的第二条召唤不能被吞**(owner 2026-08-10 实测)。
    ///
    /// 飞书 `create_time` 只到分钟精度,水位又取本页最大时间 —— 若用严格大于过滤,
    /// 同分钟先到的消息把水位推满后,后到的真召唤就永久静默:消息读到了、水位推进了、
    /// seen 里没有、日志一行没有,从外部完全看不出发生过什么。
    ///
    /// 这条钉的是**边界比较方向**:`create_time == floor` 必须放行,由 message_id 去重收尾。
    #[test]
    fn 同一分钟内的后续消息不被水位吞掉() {
        // 禁词运行时拼接:写成字面量的话,本测试自己的断言就会把自己扫红
        // (仓库里 sentinel/steward 的结构性守卫都踩过这个坑)。
        let field = format!("m.create{}time_ms", "_");
        let good = format!("{field} < floor");
        let bad = format!("{field} <{} floor", "=");
        let src = include_str!("poller.rs");
        let start = src
            .find("let floor = tail_floor_ms(")
            .expect("tail 过滤入口");
        // 窗口要盖住那段 why 注释 + 判据本身。
        let body = &src[start..(start + 1600).min(src.len())];
        assert!(
            body.contains(&good),
            "必须用 `< floor`(等于水位也放行),否则同一分钟的后续召唤会被吞"
        );
        assert!(
            !body.contains(&bad),
            "严格大于的比较方向已被实测证伪,不许退回"
        );
    }

    /// 放宽时间闸的前提:**精确去重由 message_id 负责**。这条守着那个前提 ——
    /// 一旦 `dedup_admit` 从执行链上消失,时间闸的放宽就会变成重复建群。
    #[test]
    fn 时间闸放宽依赖message_id去重仍在链上() {
        let src = include_str!("poller.rs");
        let src = src
            .split_once("\n#[cfg(test)]\nmod tests {")
            .expect("测试模块边界必须存在")
            .0;
        assert!(
            src.contains("gate::dedup_admit(&mut state, &t.message_id)"),
            "message_id 去重是时间闸放宽的安全前提,不许移除"
        );
    }

    /// 结构性红线:sentinel 是常驻轮询器,只许产 SentinelFire / 调 lark-cli / 发通知卡,
    /// 绝不许伸手进会话热路径(与 scheduler 的同名守卫一致)。
    /// 禁词用运行时拼接,免得本测试自身的字面量把自己扫红。
    #[test]
    fn sentinel_never_touches_chat_hot_path() {
        let forbidden = [
            format!("tm{}", "ux"),
            format!("spawn_chat_{}", "thread"),
            format!("handle_{}", "task"),
            format!("Thread{}", "Msg"),
        ];
        for src in [
            include_str!("poller.rs"),
            include_str!("actions.rs"),
            include_str!("lark.rs"),
            include_str!("proposals.rs"),
        ] {
            for f in &forbidden {
                assert!(!src.contains(f.as_str()), "sentinel 源码不得出现 {f}");
            }
        }
    }
}
