//! 在跑的无头一轮的进程级注册表 + `pending.json` 落盘/恢复(Item A:不留孤儿 + 重启韧性)。
//!
//! - **注册表**([`InFlight`]):`Arc<Mutex<HashMap<TurnKey, ChildKiller>>>`。chat 线程在 `drive`
//!   的 `on_spawn` 回调里把本轮的 [`ChildKiller`] **insert** 进来,turn 结束(正常/被杀)**remove**。
//!   这是唯一让「在 chat 线程栈帧里被 `drive` 独占的 child」逃出去、给停机 hook(或 pause 路)
//!   够得着的途径。同 key 至多一条:再来一条就**拒绝并当场杀掉新的**(不覆盖,见 [`InFlight::insert`]);
//!   代价是 detach 掉永不返回的 worker 时必须显式 [`InFlight::remove_if`] 兜底,否则条目会永久占位。
//! - **优雅停机**([`InFlight::shutdown_interrupt_all`]):daemon 收到 SIGTERM/SIGINT 后,对所有
//!   在跑的无头子进程发 SIGINT(整组),给短宽限等它们退,超时 SIGKILL 兜底——**绝不碰 tmux
//!   会话**(claude/Transcript 持久层,硬规则:注册表里压根没有 tmux 句柄,故天然碰不到)。
//! - **`pending.json`**:停机/打断时把未完成的一轮落成 `threads/<tag>/pending.json`
//!   ([`summon_core::pending::PendingTurn`]),下次启动 chat 线程前 [`read_pending`] 检测 + 重跑
//!   + [`clear_pending`] 删除。落盘是 best-effort:失败只 log,退化为「上轮被中断」(drop)。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use anyhow::{bail, Context as _};
use summon_core::pending::PendingTurn;
use summon_core::session::stream_card::{stream_state_path, StreamCardState};
use summon_core::session::turn_anchor::{turn_anchor_path, TurnAnchor};

use crate::observe::log::log;
use crate::runtime::executor::ChildKiller;
use crate::runtime::fsio::atomic_write;

/// 注册表键:同一 daemon 进程里唯一标识「哪个 agent 的哪个 chat 的当前这一轮」。
/// chat tag 已含 chat_type(g/p 前缀),故 `(agent, tag)` 足够唯一;同一 chat 串行跑、同一时刻
/// 至多一轮在跑,故无需 turn 序号。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TurnKey {
    pub agent: String,
    pub tag: String,
}

impl TurnKey {
    pub fn new(agent: impl Into<String>, tag: impl Into<String>) -> Self {
        Self {
            agent: agent.into(),
            tag: tag.into(),
        }
    }
}

/// 这条登记背后的进程组**属于谁**。只影响一件事:[`InFlight::insert`] 拒绝登记时,
/// 能不能就地把那个进程收掉。
///
/// 拒绝路径原本恒是「SIGINT + 零宽限 SIGKILL」——对独占进程这是对的(没人认领它,不收就是孤儿),
/// 对**借来的** RPC 常驻会话进程则是灾难:停机屏障立起之后,一条走热路径(`Reused`)的轮照样
/// 会来登记,被拒时那一枪打死的是整条跨轮会话,连同它背后的上下文与下一轮。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TurnShape {
    /// 这一轮**独占**这个进程组:exec 无头轮,以及 RPC 会话进程刚 spawn、还没被池认领的那一刻。
    /// 登记被拒 = 没有别的主人,必须当场收掉。
    OwnedProcess,
    /// 这一轮**借用**一个跨轮存活的 RPC 会话进程(它自己的主人是池那条 `rpc:<tag>` 登记)。
    /// 登记被拒只损失这一轮,绝不许发信号。
    SharedSession,
}

/// 注册表里一条在跑的无头一轮:可 kill 的句柄 + agent 家目录 + 一份可被 chat 线程实时更新的
/// `pending` 快照(`partial`/`session_id` 随流增长)。停机 hook 据此既能打断子进程,又能为每条
/// 在跑轮落 `pending.json`(race-free:主线程打断后统一落,不与正退出的 chat 线程抢)。
#[derive(Clone)]
pub struct InFlightTurn {
    pub killer: ChildKiller,
    /// agent 家目录(落 `threads/<tag>/pending.json` 用)。
    pub base: PathBuf,
    /// 本轮重跑所需快照;chat 线程随流更新 `partial`/`session_id`(共享 `Arc<Mutex>`)。
    pub pending: Arc<Mutex<PendingTurn>>,
    /// 进程归属,见 [`TurnShape`]。**只在拒绝路径上被读**。
    pub shape: TurnShape,
}

/// 注册表内部状态。`accepting=false` 是停机屏障:它与 map 共用同一把锁,
/// 因此不存在「检查还可接受 → 停机快照 → 才 insert」的空窗。
struct InFlightState {
    accepting: bool,
    map: HashMap<TurnKey, InFlightEntry>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TurnOwnership {
    Running,
    ShutdownClaimed,
}

struct InFlightEntry {
    turn: InFlightTurn,
    ownership: TurnOwnership,
}

/// 一轮返回时对完成所有权的判定。正常完成与 shutdown claim 在注册表同一把锁内竞争，
/// 因而不再靠“谁先 clear_pending”这种时序偶然性决定是否恢复。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FinishDisposition {
    Completed,
    ShutdownClaimed,
    Missing,
}

impl Default for InFlightState {
    fn default() -> Self {
        Self {
            accepting: true,
            map: HashMap::new(),
        }
    }
}

/// 进程内的注册表实例计数器。见 [`InFlight::instance_id`]。
static NEXT_INFLIGHT_ID: AtomicU64 = AtomicU64::new(1);

/// 在跑的无头子进程注册表。clone 廉价(`Arc`),停机 hook 与各 chat 线程共享同一份。
#[derive(Clone)]
pub struct InFlight {
    state: Arc<Mutex<InFlightState>>,
    /// 实例身份。**clone 共享同一个 id**(clone 就是同一张表),`new`/`default` 各拿一个新号。
    /// 唯一用途:让「必须挂在 daemon 那一份表上」这条不变量可被下游(RPC 会话池的全局单例)
    /// 当场校验,而不是靠调用方自觉。
    id: u64,
}

/// 一次尚未提交的 `InFlight` exact rekey。
///
/// guard 整个生命周期持有注册表锁：调用方可在其中完成伴随的短 IO（RPC 跨代登记 retag），
/// daemon shutdown 不能在两步之间 claim 半迁移状态。显式 [`commit`](Self::commit) 后保留 target；
/// 其它任何退出路径由 `Drop` 原地恢复 source。两条路径都只搬 map entry，绝不触发 killer。
pub(crate) struct InFlightRekey<'a> {
    state: MutexGuard<'a, InFlightState>,
    source: TurnKey,
    target: TurnKey,
    committed: bool,
}

impl InFlightRekey<'_> {
    pub(crate) fn commit(mut self) {
        self.committed = true;
    }
}

impl Drop for InFlightRekey<'_> {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        // begin_rekey_exact 持锁完成 source→target 后才构造 guard；持锁期间没有第三方能改 map，
        // 所以这里应恒可逆。仍不 unwrap：锁内状态异常时宁可保留 target 这条可达登记，也不能 panic
        // 或覆盖 source 把某个进程变孤儿。
        if self.state.map.contains_key(&self.source) {
            log(&summon_i18n::format!(
                "r86b16c2e1b",
                self.source.tag,
                self.target.tag
            ));
            return;
        }
        if let Some(entry) = self.state.map.remove(&self.target) {
            self.state.map.insert(self.source.clone(), entry);
        } else {
            log(&summon_i18n::format!(
                "rbb21bb4b70",
                self.source.tag,
                self.target.tag
            ));
        }
    }
}

impl Default for InFlight {
    fn default() -> Self {
        Self {
            state: Arc::new(Mutex::new(InFlightState::default())),
            id: NEXT_INFLIGHT_ID.fetch_add(1, Ordering::Relaxed),
        }
    }
}

impl InFlight {
    pub fn new() -> Self {
        Self::default()
    }

    /// 这张表的实例身份,见字段注释。
    pub fn instance_id(&self) -> u64 {
        self.id
    }

    /// 停机屏障此刻是否仍放行。**只是「不做无用功」的前置判定,不是保证**:返回 true 之后
    /// 屏障随时可能立起,真正的 fail-closed 仍在 [`Self::insert`](与 map 同一把锁)。
    /// 锁毒化时返回 false —— 证明不了还能收就当已经停机。
    pub fn accepting(&self) -> bool {
        self.state
            .lock()
            .map(|state| state.accepting)
            .unwrap_or(false)
    }

    /// turn 起跑:登记本轮。两种情况**拒绝**登记,并当场终止这个刚 spawn 的新生进程组
    /// (先 SIGINT 再补 SIGKILL):
    /// - 停机屏障已立起:最后一道防线,保证停机快照之后即使有正在收尾的 producer 竞态 spawn,
    ///   也不会留孤儿;
    /// - 同 key 已有在跑轮:同 chat 串行,本不该并存。这里**不覆盖**——旧条目才是唯一还能
    ///   kill 到那个老进程组的句柄,覆盖等于当场把它泄漏成永久孤儿;而拒绝只损失新的一轮,
    ///   且新轮就在眼前、当场收得掉。代价是旧条目若因 worker 永不返回而卡住(见 [`Self::remove_if`]),
    ///   该 (agent, chat) 会持续拒绝新轮,故 detach worker 的一方必须调 `remove_if` 兜底摘除。
    ///
    /// 拒绝路径**按进程归属分派**(见 [`TurnShape`]):独占进程当场收掉,借来的会话进程一枪不发。
    ///
    /// 返回是否真正登记成功。
    pub fn insert(&self, key: TurnKey, turn: InFlightTurn) -> bool {
        // lock 失败(另一线程 panic 毒化锁)只能尽力放弃登记,不 panic(运行路径不 panic 约束)。
        // 实践中本锁临界区极短、不会 panic。
        let admitted = match self.state.lock() {
            Ok(mut state) if state.accepting && !state.map.contains_key(&key) => {
                state.map.insert(
                    key.clone(),
                    InFlightEntry {
                        turn: turn.clone(),
                        ownership: TurnOwnership::Running,
                    },
                );
                true
            }
            Ok(_) | Err(_) => false,
        };
        if admitted {
            return true;
        }
        match turn.shape {
            // 停机 / 同 key 已占 / 锁毒化:宁可终止新生无头轮,不可让未登记子进程脱离 daemon。
            TurnShape::OwnedProcess => {
                turn.killer.interrupt();
                turn.killer.kill();
            }
            // 借来的会话进程另有主人(池的 `rpc:<tag>` 登记),它会在停机时被正常收敛。
            // 这里开枪等于替别人做主,零宽限打死一条还带着上下文的跨轮会话。
            TurnShape::SharedSession => {
                log(&summon_i18n::format!("r96a71b503d", key.agent, key.tag))
            }
        }
        false
    }

    /// 停机屏障:从这一刻起拒绝新登记。与 `insert` 同锁,故返回后已经没有
    /// 任何「看到 accepting=true 但还没进 map」的在途 insert。
    pub fn freeze(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.accepting = false;
        }
    }

    /// Bridge 本地占用探测。锁毒化时返回 true：无法证明空闲就禁止绑定，避免错误 cold-start。
    pub(crate) fn has_agent(&self, agent: &str) -> bool {
        self.state
            .lock()
            .map(|s| s.map.keys().any(|k| k.agent == agent))
            .unwrap_or(true)
    }

    pub fn contains(&self, key: &TurnKey) -> bool {
        self.state
            .lock()
            .map(|state| state.map.contains_key(key))
            .unwrap_or(true)
    }

    /// 准备把一条登记从 `source` exact 搬到 `target`。
    ///
    /// 校验与搬迁在同一把锁内完成：source 必须仍是 `expected` 那把 killer、处于 Running、target
    /// 与所有 `must_be_absent` 均为空，且 daemon 未进入停机屏障。任一条件不满足时 map 零变化；
    /// 成功后调用方必须显式 `commit`，否则 guard drop 自动回滚。
    pub(crate) fn begin_rekey_exact(
        &self,
        source: &TurnKey,
        target: &TurnKey,
        expected: &ChildKiller,
        must_be_absent: &[TurnKey],
    ) -> anyhow::Result<InFlightRekey<'_>> {
        if source == target {
            bail!(summon_i18n::format!("re2d97f8a55"));
        }
        if source.agent != target.agent {
            bail!(summon_i18n::format!("r9ec9288e22"));
        }
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!(summon_i18n::format!("r33657bd19f")))?;
        if !state.accepting {
            bail!(summon_i18n::format!("rb8edd6384a"));
        }
        if state.map.contains_key(target) {
            bail!(summon_i18n::format!("r127ce12f6d", target.tag));
        }
        if let Some(blocker) = must_be_absent
            .iter()
            .find(|key| state.map.contains_key(*key))
        {
            bail!(summon_i18n::format!("r1fec2bd13a", blocker.tag));
        }
        let Some(entry) = state.map.get(source) else {
            bail!(summon_i18n::format!("r60f6eb7a0e", source.tag));
        };
        if entry.ownership != TurnOwnership::Running {
            bail!(summon_i18n::format!("r933770fb5a", source.tag));
        }
        if !entry.turn.killer.same_handle(expected) {
            bail!(summon_i18n::format!("rd4aa827263", source.tag));
        }

        let Some(entry) = state.map.remove(source) else {
            bail!(summon_i18n::format!("r1a5b5cbbc4", source.tag));
        };
        state.map.insert(target.clone(), entry);
        Ok(InFlightRekey {
            state,
            source: source.clone(),
            target: target.clone(),
            committed: false,
        })
    }

    /// turn 结束时原子争夺“正常完成”所有权。shutdown 已先 claim 时返回
    /// `ShutdownClaimed`，调用方必须保留 pending、不得按成功/DONE 收尾。
    /// killer 身份同时参与核对，旧 worker 绝不会误摘同 key 的下一轮。
    pub fn finish(&self, key: &TurnKey, killer: &ChildKiller) -> FinishDisposition {
        let Ok(mut state) = self.state.lock() else {
            return FinishDisposition::Missing;
        };
        let disposition = match state.map.get(key) {
            Some(entry) if entry.turn.killer.same_handle(killer) => match entry.ownership {
                TurnOwnership::Running => FinishDisposition::Completed,
                TurnOwnership::ShutdownClaimed => FinishDisposition::ShutdownClaimed,
            },
            _ => FinishDisposition::Missing,
        };
        if disposition != FinishDisposition::Missing {
            state.map.remove(key);
        }
        disposition
    }

    /// 同一轮启动失败后的唯一替换：旧进程已回收，完成所有权与 pending 仍属于本轮。
    /// 不先摘再插，否则停机可从两步之间穿过，遗漏新进程或恢复点。
    pub(crate) fn replace_running(
        &self,
        key: &TurnKey,
        expected: &ChildKiller,
        replacement: ChildKiller,
    ) -> bool {
        let admitted = self.state.lock().ok().is_some_and(|mut state| {
            if !state.accepting {
                return false;
            }
            let Some(entry) = state.map.get_mut(key) else {
                return false;
            };
            if entry.ownership != TurnOwnership::Running
                || entry.turn.shape != TurnShape::OwnedProcess
                || !entry.turn.killer.same_handle(expected)
            {
                return false;
            }
            entry.turn.killer = replacement.clone();
            true
        });
        if !admitted {
            replacement.interrupt();
            replacement.kill();
        } else if expected.cancellation_flag().load(Ordering::Acquire) {
            // 手动取消仍由本轮正常收尾，不是停机接管；登记取消后的新把手并立即停掉它。
            replacement.interrupt();
        }
        admitted
    }

    /// detach 兜底:摘除**自己那一轮**的注册(按 killer 身份核对)。
    ///
    /// 摘除条目的常规路径只有 worker 线程尾部的 [`Self::finish`];worker 若永不返回(SIGKILL 后
    /// 仍有逃逸孙进程持住 stdout,读到不了 EOF),调用方只能 detach 它,条目就此永久占位——之后
    /// 该 (agent, chat) 的每一轮都会被 [`Self::insert`] 拒绝并当场杀掉(用户零回复),
    /// `interrupt_one*` 只发信号不摘除也清不掉,`contains` 还让 Bridge 永久判忙。detach 前调本函数
    /// 即可断掉这条毒化链。
    ///
    /// 身份核对与 `finish` 同锁同语义:pgid 会被 OS 复用,只有 `same_handle` 的 Arc 身份能保证
    /// 不误摘同 key 的下一轮。不参与「正常完成 vs 停机接管」的所有权判定——detach 方不声称完成,
    /// 且停机快照早已 clone 走 killer,摘除影响不到它的信号与落盘。返回是否真的摘除了。
    ///
    /// 唯一调用侧:`chat_thread::detach_inflight_turn`(worker 未在宽限内返回、只能 detach 那条路)。
    pub fn remove_if(&self, key: &TurnKey, killer: &ChildKiller) -> bool {
        let Ok(mut state) = self.state.lock() else {
            return false;
        };
        let matched = state
            .map
            .get(key)
            .is_some_and(|entry| entry.turn.killer.same_handle(killer));
        if matched {
            state.map.remove(key);
        }
        matched
    }

    /// 有界打断一条在跑轮:`SIGINT` 后给 `grace`,届时若同一条注册仍在则
    /// `SIGKILL` 进程组。升级线程会核对 ChildKiller 的 Arc 身份,不会误杀同 key 的下一轮。
    /// 只能触及 InFlight 里的无头进程组,类型上不可能碰 tmux。
    pub fn interrupt_one_bounded(&self, key: &TurnKey, grace: Duration) -> bool {
        let killer = match self.state.lock() {
            Ok(state) => state.map.get(key).map(|e| e.turn.killer.clone()),
            Err(_) => return false,
        };
        let Some(killer) = killer else {
            return false;
        };
        killer.interrupt();
        let registry = self.clone();
        let key = key.clone();
        let expected = killer.clone();
        std::thread::spawn(move || {
            std::thread::sleep(grace);
            let still_same_turn = registry
                .state
                .lock()
                .ok()
                .and_then(|state| {
                    state
                        .map
                        .get(&key)
                        .map(|e| e.turn.killer.same_handle(&expected))
                })
                .unwrap_or(false);
            if still_same_turn {
                expected.kill();
            }
        });
        true
    }

    /// 当前在跑轮数。仅测试断言用(生产路径不查计数);#[cfg(test)] 免 dead_code 告警。
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.state.lock().map(|s| s.map.len()).unwrap_or(0)
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// 优雅停机:对所有在跑的无头一轮 ① 落 `pending.json`(best-effort)② 发 SIGINT(整组)
    /// ③ 给 `grace` 宽限等它们自行退出 ④ 仍在跑的 SIGKILL 兜底。**只动注册表里的无头子进程组,
    /// 绝不碰 tmux 会话**(注册表里就没有 tmux 句柄)。`still_alive` 是「某 pgid 是否还在」的探测
    /// 注入点(便于单测不依赖真信号)。
    ///
    /// `enqueue_notify` 是 outbox producer 钩子:step 0 里**为每条在跑轮**额外入队一条「会话被打断」
    /// 通知(`(agent, &PendingTurn)`),纯本地、不联网、不阻塞停机(失败由钩子内部 log-ignore)。
    /// daemon 重启后由 drain 线程投递。传 agent 名(TurnKey.agent)+ pending 快照让钩子算文案/目标。
    ///
    /// 返回被 interrupt 的句柄数(观测/测试用)。
    pub fn shutdown_interrupt_all(
        &self,
        grace: Duration,
        still_alive: &dyn Fn(i32) -> bool,
        enqueue_notify: &dyn Fn(&str, &PendingTurn),
    ) -> usize {
        // 停机屏障 + 快照必须在同一临界区:返回后所有新 insert 都会当场被杀,
        // 故不会再出现「快照完才登记」的孤儿空窗。出锁后才 sleep/发信号。
        let turns: Vec<(String, InFlightTurn)> = match self.state.lock() {
            Ok(mut state) => {
                state.accepting = false;
                state
                    .map
                    .iter_mut()
                    .filter_map(|(k, entry)| {
                        if entry.ownership != TurnOwnership::Running {
                            return None;
                        }
                        entry.ownership = TurnOwnership::ShutdownClaimed;
                        Some((k.agent.clone(), entry.turn.clone()))
                    })
                    .collect()
            }
            Err(_) => return 0,
        };
        if turns.is_empty() {
            return 0;
        }
        // 0) 先落 pending.json(在打断前落,确保 partial/sid 已是打断时刻的最新快照)+ 入队一条
        //    outbox 退出通知,均 best-effort(不阻断停机)。
        //    按 kind 过滤:只有 Chat 种类需要落 pending(供 resume)和发通知(给 owner 看)。
        //    Worker 是一次性子任务,不产孤儿 pending.json、不误报停机通知。
        for (agent, t) in &turns {
            if let Ok(mut p) = t.pending.lock() {
                if p.kind.persists_pending() {
                    // 自动续跑权只能由优雅停机在接管完成权之后授予。运行中/on_spawn 落盘的
                    // checkpoint 保持 false，终态删除失败也不会在下次启动被误重放。
                    p.resume_authorized = true;
                    let tag = summon_core::registry::chat_tag(&p.chat_type, &p.chat_id);
                    let _ = write_pending(&t.base, &tag, &p);
                }
                // 飞书身份必须由轮创建时冻结。旧/异常 Chat 若 scope 为空，仍打断并落 checkpoint，
                // 但绝不让 producer 从当前 agent 配置猜身份后入队到另一个 app。
                if p.kind.needs_shutdown_notify() && !p.delivery_scope.is_empty() {
                    enqueue_notify(agent, &p);
                }
            }
        }
        // 1) 全体 SIGINT(整组):cursor 干净退、codex 写 <turn_aborted> 保留上下文。
        for (_, t) in &turns {
            t.killer.interrupt();
        }
        // 2) 宽限轮询:全退则提前结束;到点仍有活着的 → SIGKILL 兜底。
        let deadline = Instant::now() + grace;
        loop {
            let any_alive = turns.iter().any(|(_, t)| still_alive(t.killer.pgid()));
            if !any_alive || Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        for (_, t) in &turns {
            if still_alive(t.killer.pgid()) {
                t.killer.kill(); // SIGKILL 整组(不可屏蔽)
            }
        }
        turns.len()
    }
}

/// 用 pgid 探测进程组是否还在(`kill(-pgid, 0)`:存在=Ok 或 EPERM,ESRCH=已没)。停机宽限轮询用。
pub fn group_alive(pgid: i32) -> bool {
    use nix::errno::Errno;
    match nix::sys::signal::killpg(nix::unistd::Pid::from_raw(pgid), None) {
        Ok(()) => true,
        // EPERM:组还在但无权(理论上不会,自己起的)。ESRCH:组已空。
        Err(Errno::ESRCH) => false,
        Err(_) => true,
    }
}

/// `threads/<tag>/pending.json` 路径。
fn pending_path(base: &Path, tag: &str) -> PathBuf {
    base.join("threads").join(tag).join("pending.json")
}

/// 只查 checkpoint 是否存在，不解析、更不删除损坏文件。Bridge 占用判定把任何 pending.json
/// （包括半写/损坏）都视为本地工作证据，避免调用 `read_pending` 的清理副作用。
pub(crate) fn pending_exists(base: &Path, tag: &str) -> std::io::Result<bool> {
    match std::fs::metadata(pending_path(base, tag)) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

/// 落 `pending.json`(best-effort):把被打断/停机的一轮快照写盘,下次启动重跑用。失败返回 Err,
/// 调用方 log-ignore(退化为 drop-and-notify,不中断停机)。
pub fn write_pending(base: &Path, tag: &str, p: &PendingTurn) -> anyhow::Result<()> {
    let path = pending_path(base, tag);
    let json = serde_json::to_string_pretty(p)?;
    atomic_write(&path, json.as_bytes())
}

/// 读 `pending.json`(无 / 损坏 → None,损坏不 panic):重启时 chat 线程起跑前检测。
/// 损坏文件无法恢复，当场删除避免冷启永久重复解析同一份垃圾。
pub fn read_pending(base: &Path, tag: &str) -> Option<PendingTurn> {
    let path = pending_path(base, tag);
    let s = std::fs::read_to_string(&path).ok()?;
    match serde_json::from_str(&s) {
        Ok(pending) => Some(pending),
        Err(error) => {
            log(&summon_i18n::format!("r3ea53b825f", path.display()));
            if let Err(remove_error) = std::fs::remove_file(&path) {
                log(&summon_i18n::format!("rc25cb14728", path.display()));
            }
            None
        }
    }
}

/// 读取一条经优雅停机授权、且完整属于当前 agent/chat/飞书 epoch 的 Chat checkpoint。
/// 旧 schema、运行中残留、身份不匹配或重复/空 Inbox id 都当场清掉 pending/stream；本函数
/// 没有进程、网络或 outbox 能力，从结构上保证不可信 checkpoint 只能放弃，不能产生副作用。
pub fn read_resumable_pending(
    base: &Path,
    tag: &str,
    current_scope: &str,
    agent_name: &str,
    chat_id: &str,
    chat_type: &str,
) -> Option<PendingTurn> {
    let pending = read_pending(base, tag)?;
    if pending.resumable_in_context(current_scope, agent_name, chat_id, chat_type) {
        return Some(pending);
    }
    log(&summon_i18n::format!(
        "r812b957f15",
        pending_path(base, tag).display()
    ));
    clear_pending(base, tag);
    clear_stream_state(base, tag);
    None
}

/// 删 `pending.json`(重跑成功后 / 决定 drop 后调)。不存在无害。
pub fn clear_pending(base: &Path, tag: &str) {
    let _ = clear_pending_checked(base, tag);
}

/// 启动恢复屏障使用的严格删除：崩溃轮已由 RecoveryBatch 接管后，只有确认 checkpoint 不再
/// 存在才能继续启动 dispatcher，否则它可能绕过人工恢复卡自动 resume。
pub fn clear_pending_checked(base: &Path, tag: &str) -> anyhow::Result<()> {
    let path = pending_path(base, tag);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| summon_i18n::format!("r107759e0e7")),
    }
}

/// 落 `stream_state.json`(best-effort):把正在流式推送的卡片的 message_id 写盘,
/// 供 daemon 重启后续传同一张卡。失败返回 Err,调用方 log-ignore。
#[cfg(test)]
pub fn write_stream_state(base: &Path, tag: &str, s: &StreamCardState) -> anyhow::Result<()> {
    let path = stream_state_path(base, tag);
    let json = serde_json::to_string_pretty(s)?;
    atomic_write(&path, json.as_bytes())
}

/// 读 `stream_state.json`(无 / 损坏 → None)。损坏文件当场删除，避免每次冷启
/// 都重复读同一个永不可恢复的 checkpoint。
pub fn read_stream_state(base: &Path, tag: &str) -> Option<StreamCardState> {
    let path = stream_state_path(base, tag);
    let s = std::fs::read_to_string(&path).ok()?;
    match serde_json::from_str(&s) {
        Ok(state) => Some(state),
        Err(error) => {
            log(&summon_i18n::format!("r68a71e0662", path.display()));
            if let Err(remove_error) = std::fs::remove_file(&path) {
                log(&summon_i18n::format!("rf05db91ace", path.display()));
            }
            None
        }
    }
}

/// 落 `turn_anchor.json`(best-effort):把这一轮的接管锚点写盘,供 daemon 重启后接管
/// 仍在 tmux 里跑着的 Transcript 轮。失败返回 Err,调用方 log-ignore——锚点缺失只是退化为
/// 人工恢复卡,不该反过来阻断这一轮。
pub fn write_turn_anchor(base: &Path, tag: &str, a: &TurnAnchor) -> anyhow::Result<()> {
    let path = turn_anchor_path(base, tag);
    let json = serde_json::to_string_pretty(a)?;
    atomic_write(&path, json.as_bytes())
}

/// 读 `turn_anchor.json`(无 / 损坏 → None,损坏不 panic)。损坏文件当场删除,
/// 避免每次冷启都重复解析同一份永不可用的锚点。
pub fn read_turn_anchor(base: &Path, tag: &str) -> Option<TurnAnchor> {
    let path = turn_anchor_path(base, tag);
    let s = std::fs::read_to_string(&path).ok()?;
    match serde_json::from_str(&s) {
        Ok(anchor) => Some(anchor),
        Err(error) => {
            log(&summon_i18n::format!("rbc862dd5f6", path.display()));
            if let Err(remove_error) = std::fs::remove_file(&path) {
                log(&summon_i18n::format!("r7959c89dba", path.display()));
            }
            None
        }
    }
}

/// 删 `turn_anchor.json`(本轮结束 / 决定不接管后调)。不存在无害；其他错误显式记录:
/// 残留锚点会让下次启动误以为还有一轮在跑。
pub fn clear_turn_anchor(base: &Path, tag: &str) {
    let path = turn_anchor_path(base, tag);
    if let Err(error) = std::fs::remove_file(&path) {
        if error.kind() != std::io::ErrorKind::NotFound {
            log(&summon_i18n::format!("r97b697b101", path.display()));
        }
    }
}

/// 删 `stream_state.json`(本轮正常结束 / 重跑完成后调)。不存在无害；
/// 其他错误显式记录，不再让“必须清旧 checkpoint”的失败完全不可观测。
pub fn clear_stream_state(base: &Path, tag: &str) {
    let path = stream_state_path(base, tag);
    if let Err(error) = std::fs::remove_file(&path) {
        if error.kind() != std::io::ErrorKind::NotFound {
            log(&summon_i18n::format!("r0da1d13056", path.display()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc as StdArc;

    /// 造一条 in-flight 条目:给定 pgid、家目录、chat 标识,partial/sid 给默认。
    fn turn_for(pgid: i32, base: &Path, chat_type: &str, chat_id: &str) -> InFlightTurn {
        InFlightTurn {
            killer: ChildKiller::new(pgid),
            base: base.to_path_buf(),
            pending: Arc::new(Mutex::new(PendingTurn {
                kind: summon_core::pending::PendingTurnKind::Chat,
                delivery_scope: "test-scope".into(),
                agent_name: "blue".into(),
                inbox_message_ids: vec!["om_1".into()],
                resume_authorized: false,
                chat_id: chat_id.into(),
                chat_type: chat_type.into(),
                cli: "codex".into(),
                prompt: "活儿".into(),
                session_id: Some("th_1".into()),
                partial: "半截".into(),
            })),
            shape: TurnShape::OwnedProcess,
        }
    }

    /// 起一个屏蔽 SIGINT 的独立进程组子进程,并等它自报 ready(避免固定 sleep 造成调度假阳性)。
    /// 只有 SIGKILL 收得掉它,故可用来证明某条路径确实补了第二枪。
    fn spawn_sigint_immune_child() -> std::process::Child {
        use std::io::BufRead as _;
        use std::os::unix::process::CommandExt as _;
        use std::process::{Command, Stdio};

        let mut child = Command::new("sh")
            .args(["-c", "trap '' INT; echo ready; exec sleep 30"])
            .process_group(0)
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut ready = String::new();
        std::io::BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut ready)
            .unwrap();
        assert_eq!(ready.trim(), "ready");
        child
    }

    /// 等子进程在 `timeout` 内退出;超时兜底收掉再返回 false(别把僵尸留给后续用例)。
    fn waited_exit(child: &mut std::process::Child, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            match child.try_wait().unwrap() {
                Some(_) => return true,
                None if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
                None => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return false;
                }
            }
        }
    }

    #[test]
    fn duplicate_insert_is_rejected_and_terminates_the_new_child() {
        let dir = tempfile::tempdir().unwrap();
        let reg = InFlight::new();
        let key = TurnKey::new("blue", "p-dup");
        let first = turn_for(900_130, dir.path(), "p2p", "ou_dup");
        let first_killer = first.killer.clone();
        assert!(reg.insert(key.clone(), first));

        let mut child = spawn_sigint_immune_child();
        let pgid = child.id() as i32;
        let second = turn_for(pgid, dir.path(), "p2p", "ou_dup");
        assert!(
            !reg.insert(key.clone(), second),
            "同 key 已有在跑轮时必须拒绝登记,不得覆盖"
        );
        assert!(
            waited_exit(&mut child, Duration::from_secs(2)),
            "被拒绝的新轮必须当场收掉,不能留成脱离 daemon 的孤儿"
        );

        assert_eq!(reg.len(), 1, "拒绝路径不得动老条目");
        assert_eq!(
            reg.finish(&key, &first_killer),
            FinishDisposition::Completed,
            "注册里仍应是先登记的那一轮(未被覆盖)"
        );
    }

    #[test]
    fn remove_if_detaches_only_the_matching_killer() {
        let dir = tempfile::tempdir().unwrap();
        let reg = InFlight::new();
        let key = TurnKey::new("blue", "p-detach");
        let turn = turn_for(900_140, dir.path(), "p2p", "ou_detach");
        let killer = turn.killer.clone();
        assert!(reg.insert(key.clone(), turn));

        // 同 pgid 但不同 Arc 身份:模拟"下一轮恰好复用了 pgid",绝不能被上一轮的 detach 误摘。
        let impostor = ChildKiller::new(900_140);
        assert!(
            !reg.remove_if(&key, &impostor),
            "killer 身份不匹配时不得摘除"
        );
        assert!(
            reg.contains(&key),
            "身份不匹配的 remove_if 必须完全无副作用"
        );

        assert!(reg.remove_if(&key, &killer), "身份匹配时应摘除自己那一轮");
        assert!(reg.is_empty());
        assert!(
            !reg.remove_if(&key, &killer),
            "条目已不在,重复 detach 兜底无害返回 false"
        );
    }

    #[test]
    fn exact_rekey_commits_or_rolls_back_without_signalling_the_process() {
        let dir = tempfile::tempdir().unwrap();
        let reg = InFlight::new();
        let source = TurnKey::new("blue", "rpc:g-source");
        let target = TurnKey::new("blue", "rpc:g-target");
        let turn = turn_for(900_141, dir.path(), "group", "g-source");
        let killer = turn.killer.clone();
        assert!(reg.insert(source.clone(), turn));

        // 未 commit 的 guard 必须自动回滚。这个能力给 RpcPool 的 rpc_procs 落盘失败路径用：
        // 任一后续步骤失败，进程仍完整挂在 source key 下。
        let rollback = reg
            .begin_rekey_exact(&source, &target, &killer, &[])
            .expect("身份一致且 target 空闲时应可准备迁移");
        assert!(!killer.was_killed(), "rekey 只搬登记，绝不能触发信号");
        drop(rollback);
        assert!(reg.contains(&source), "guard drop 后 source 必须恢复");
        assert!(!reg.contains(&target));

        let committed = reg
            .begin_rekey_exact(&source, &target, &killer, &[])
            .expect("回滚后应可再次准备迁移");
        committed.commit();
        assert!(!reg.contains(&source), "commit 后 source 不再持有登记");
        assert!(reg.contains(&target), "commit 后 target 成为唯一登记");
        assert!(!killer.was_killed(), "commit 同样不能触发信号");
    }

    #[test]
    fn exact_rekey_validation_failures_leave_the_registry_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let source = TurnKey::new("blue", "rpc:g-source");
        let target = TurnKey::new("blue", "rpc:g-target");
        let blocker = TurnKey::new("blue", "g-source");

        let wrong_identity = InFlight::new();
        let turn = turn_for(900_142, dir.path(), "group", "g-source");
        let killer = turn.killer.clone();
        assert!(wrong_identity.insert(source.clone(), turn));
        let impostor = ChildKiller::new(900_142);
        assert!(
            wrong_identity
                .begin_rekey_exact(&source, &target, &impostor, &[])
                .is_err(),
            "同 pgid、不同 Arc 身份也必须拒绝"
        );
        assert!(wrong_identity.contains(&source));
        assert!(!wrong_identity.contains(&target));
        assert!(!killer.was_killed());

        let occupied = InFlight::new();
        let source_turn = turn_for(900_143, dir.path(), "group", "g-source");
        let source_killer = source_turn.killer.clone();
        assert!(occupied.insert(source.clone(), source_turn));
        assert!(occupied.insert(
            target.clone(),
            turn_for(900_144, dir.path(), "group", "g-target")
        ));
        assert!(
            occupied
                .begin_rekey_exact(&source, &target, &source_killer, &[])
                .is_err(),
            "target 已占用时不得覆盖"
        );
        assert!(occupied.contains(&source));
        assert!(occupied.contains(&target));

        let blocked = InFlight::new();
        let source_turn = turn_for(900_145, dir.path(), "group", "g-source");
        let source_killer = source_turn.killer.clone();
        assert!(blocked.insert(source.clone(), source_turn));
        assert!(blocked.insert(
            blocker.clone(),
            turn_for(900_146, dir.path(), "group", "g-source")
        ));
        assert!(
            blocked
                .begin_rekey_exact(&source, &target, &source_killer, &[blocker])
                .is_err(),
            "裸 turn blocker 存在时不得迁移常驻进程登记"
        );
        assert!(blocked.contains(&source));
        assert!(!blocked.contains(&target));
        assert!(!source_killer.was_killed());
    }

    #[test]
    fn detached_turn_does_not_poison_the_next_turn_of_same_chat() {
        let dir = tempfile::tempdir().unwrap();
        let reg = InFlight::new();
        let key = TurnKey::new("blue", "p-next");
        let stuck = turn_for(900_150, dir.path(), "p2p", "ou_next");
        let stuck_killer = stuck.killer.clone();
        assert!(reg.insert(key.clone(), stuck));

        // worker 永不返回 → finish 永不被调,只能靠 detach 兜底摘除,否则该 chat 的后续每一轮
        // 都会在 spawn 后毫秒内被拒绝并杀掉。
        assert!(reg.remove_if(&key, &stuck_killer));

        let next = turn_for(900_151, dir.path(), "p2p", "ou_next");
        let next_killer = next.killer.clone();
        assert!(
            reg.insert(key.clone(), next),
            "detach 之后下一轮必须能正常登记"
        );
        assert_eq!(reg.len(), 1);
        assert_eq!(reg.finish(&key, &next_killer), FinishDisposition::Completed);
    }

    #[test]
    fn insert_then_finish_changes_len() {
        let dir = tempfile::tempdir().unwrap();
        let reg = InFlight::new();
        assert!(reg.is_empty());
        let key = TurnKey::new("blue", "p-abc");
        let turn = turn_for(12345, dir.path(), "p2p", "ou_a");
        let killer = turn.killer.clone();
        reg.insert(key.clone(), turn);
        assert_eq!(reg.len(), 1);
        assert!(reg.contains(&key));
        assert_eq!(reg.finish(&key, &killer), FinishDisposition::Completed);
        assert!(!reg.contains(&key));
        assert!(reg.is_empty(), "摘除后应空");
    }

    #[test]
    fn replace_running_preserves_pending_and_finishes_cancel_as_this_turn() {
        for scenario in ["running", "cancel", "shutdown", "wrong-handle"] {
            let dir = tempfile::tempdir().unwrap();
            let reg = InFlight::new();
            let key = TurnKey::new("blue", "g-retry");
            let turn = turn_for(900_800, dir.path(), "group", "oc_retry");
            let old = turn.killer.clone();
            let pending = Arc::clone(&turn.pending);
            assert!(reg.insert(key.clone(), turn));
            if scenario == "cancel" {
                old.cancel_waiters();
            }
            if scenario == "shutdown" {
                reg.shutdown_interrupt_all(Duration::ZERO, &|_| false, &|_, _| {});
            }
            let expected = if scenario == "wrong-handle" {
                ChildKiller::new(old.pgid())
            } else {
                old.clone()
            };
            let next = ChildKiller::new(900_801);
            assert_eq!(
                reg.replace_running(&key, &expected, next.clone()),
                matches!(scenario, "running" | "cancel")
            );
            let state = reg.state.lock().unwrap();
            let entry = state.map.get(&key).unwrap();
            assert!(Arc::ptr_eq(&entry.turn.pending, &pending));
            assert!(entry
                .turn
                .killer
                .same_handle(if matches!(scenario, "running" | "cancel") {
                    &next
                } else {
                    &old
                }));
            assert_eq!(next.was_killed(), scenario != "running");
            drop(state);
            if matches!(scenario, "running" | "cancel") {
                assert_eq!(reg.finish(&key, &next), FinishDisposition::Completed);
                assert!(reg.is_empty(), "取消交接不能留下待停机恢复的登记");
            }
        }
    }

    #[test]
    fn pending_exists_is_read_only_even_when_corrupt() {
        let dir = tempfile::tempdir().unwrap();
        let path = pending_path(dir.path(), "g-corrupt");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "not-json").unwrap();
        assert!(pending_exists(dir.path(), "g-corrupt").unwrap());
        assert_eq!(std::fs::read_to_string(path).unwrap(), "not-json");
    }

    #[test]
    fn normal_finish_and_shutdown_claim_are_mutually_exclusive() {
        let dir = tempfile::tempdir().unwrap();

        let normal = InFlight::new();
        let normal_key = TurnKey::new("blue", "p-normal");
        let normal_turn = turn_for(900_120, dir.path(), "p2p", "ou_normal");
        let normal_killer = normal_turn.killer.clone();
        assert!(normal.insert(normal_key.clone(), normal_turn));
        assert_eq!(
            normal.finish(&normal_key, &normal_killer),
            FinishDisposition::Completed
        );
        assert!(normal.is_empty());

        let stopping = InFlight::new();
        let stopping_key = TurnKey::new("blue", "p-stopping");
        let stopping_turn = turn_for(900_121, dir.path(), "p2p", "ou_stopping");
        let stopping_killer = stopping_turn.killer.clone();
        assert!(stopping.insert(stopping_key.clone(), stopping_turn));
        assert_eq!(
            stopping.shutdown_interrupt_all(Duration::ZERO, &|_| false, &|_, _| {}),
            1
        );
        assert_eq!(
            stopping.finish(&stopping_key, &stopping_killer),
            FinishDisposition::ShutdownClaimed,
            "shutdown 先 claim 后，worker 不得再取得正常完成所有权"
        );
        assert!(stopping.is_empty());
    }

    /// 停机屏障后,**借用型**一轮被拒时绝不许对会话进程开枪。
    ///
    /// 池化之后这条路每天都会走到:freeze 之后一条走热路径(`Reused`)的轮照样会来登记 ——
    /// 它不经 `admit`(没 spawn 新进程),于是直接撞上 `insert` 的拒绝路径。老实现在这里
    /// `interrupt()+kill()`,零宽限 SIGKILL 掉一条还带着上下文的跨轮会话(codex 连
    /// `<turn_aborted>` 都写不出),而那条会话本该由池的 `rpc:<tag>` 登记在停机时正常收敛。
    #[test]
    fn rejected_shared_session_turn_is_never_signalled() {
        let dir = tempfile::tempdir().unwrap();
        let mut child = spawn_sigint_immune_child();
        let pgid = child.id() as i32;
        let reg = InFlight::new();
        reg.freeze();

        let mut turn = turn_for(pgid, dir.path(), "p2p", "ou_shared");
        turn.shape = TurnShape::SharedSession;
        let killer = turn.killer.clone();
        assert!(
            !reg.insert(TurnKey::new("blue", "p-shared"), turn),
            "停机屏障后不得登记"
        );

        assert!(
            !killer.was_killed(),
            "借用型一轮被拒时一枪都不许发 —— 那是别人的常驻会话进程"
        );
        assert!(
            group_alive(pgid),
            "会话进程必须还活着(它由池那条 rpc: 登记负责收敛)"
        );
        crate::runtime::rpc_stdio::signal_group(pgid, nix::sys::signal::Signal::SIGKILL)
            .expect("测试 teardown 必须收掉整个进程组,不能只杀 shell leader 留下 sleep");
        assert!(
            waited_exit(&mut child, Duration::from_secs(2)),
            "测试 teardown 后进程组 leader 必须退出"
        );
    }

    /// `accepting()` 只是前置判定,但必须与屏障同步:freeze 之后立刻为 false。
    #[test]
    fn accepting_flips_with_the_shutdown_barrier() {
        let reg = InFlight::new();
        assert!(reg.accepting());
        reg.freeze();
        assert!(!reg.accepting(), "屏障立起后必须当场不再放行");
    }

    /// 实例身份:clone 是同一张表,新建的是另一张。RPC 会话池靠它校验「挂对了表」。
    #[test]
    fn instance_id_identifies_the_registry_not_the_handle() {
        let reg = InFlight::new();
        assert_eq!(reg.instance_id(), reg.clone().instance_id());
        assert_ne!(reg.instance_id(), InFlight::new().instance_id());
    }

    #[test]
    fn freeze_rejects_late_insert_without_leaving_untracked_turn() {
        let dir = tempfile::tempdir().unwrap();
        let reg = InFlight::new();
        reg.freeze();
        let inserted = reg.insert(
            TurnKey::new("late", "p-after-shutdown"),
            turn_for(900_099, dir.path(), "p2p", "ou_late"),
        );
        assert!(!inserted, "停机屏障后不得再登记新轮");
        assert!(reg.is_empty(), "被拒绝的新轮不得成为快照外孤儿");
    }

    #[test]
    fn bounded_interrupt_escalates_when_child_ignores_sigint() {
        let dir = tempfile::tempdir().unwrap();
        let mut child = spawn_sigint_immune_child();
        let pgid = child.id() as i32;
        let reg = InFlight::new();
        let key = TurnKey::new("blue", "p-ignore-int");
        reg.insert(key.clone(), turn_for(pgid, dir.path(), "p2p", "ou_ignore"));
        assert!(reg.interrupt_one_bounded(&key, Duration::from_millis(40)));

        assert!(
            waited_exit(&mut child, Duration::from_secs(2)),
            "SIGINT 宽限到期后必须 SIGKILL 收掉进程组"
        );
    }

    #[test]
    fn shutdown_interrupts_every_inflight_child() {
        let dir = tempfile::tempdir().unwrap();
        let reg = InFlight::new();
        // 三个不存在的 pgid:interrupt() 走幂等闸置位(killpg 对不存在 pgid 无副作用)。
        reg.insert(
            TurnKey::new("a", "p-1"),
            turn_for(900_001, dir.path(), "p2p", "u1"),
        );
        reg.insert(
            TurnKey::new("a", "p-2"),
            turn_for(900_002, dir.path(), "p2p", "u2"),
        );
        reg.insert(
            TurnKey::new("b", "g-3"),
            turn_for(900_003, dir.path(), "group", "g3"),
        );
        // 收集 producer 钩子被调的 (agent, chat_id):停机应**为每条在跑轮**入队一条退出通知,
        // 且钩子纯本地(测试闭包只记录,不联网)。
        let enq: StdArc<Mutex<Vec<(String, String, String)>>> = StdArc::new(Mutex::new(Vec::new()));
        let enq2 = StdArc::clone(&enq);
        let enqueue = move |agent: &str, p: &PendingTurn| {
            enq2.lock().unwrap().push((
                agent.to_string(),
                p.chat_id.clone(),
                p.delivery_scope.clone(),
            ));
        };
        // still_alive 恒 false:无需宽限,立即返回;断言全部被纳入 interrupt。
        let n = reg.shutdown_interrupt_all(Duration::from_millis(10), &|_| false, &enqueue);
        assert_eq!(n, 3, "三个在跑子进程都应被 interrupt");
        // 停机为每条在跑轮落了 pending.json(重启重跑用)。
        assert!(
            read_pending(dir.path(), &chat_tag_t("p2p", "u1"))
                .is_some_and(|pending| pending.resume_authorized),
            "只有 shutdown claim 后落盘的 checkpoint 才授予自动续跑权"
        );
        assert!(read_pending(dir.path(), &chat_tag_t("group", "g3"))
            .is_some_and(|pending| pending.resume_authorized));
        // 每条在跑轮都入队了一条退出通知(agent + chat_id 正确,无网络调用)。
        let mut got = enq.lock().unwrap().clone();
        got.sort();
        assert_eq!(
            got,
            vec![
                ("a".to_string(), "u1".to_string(), "test-scope".to_string()),
                ("a".to_string(), "u2".to_string(), "test-scope".to_string()),
                ("b".to_string(), "g3".to_string(), "test-scope".to_string()),
            ],
            "停机通知必须携带轮创建时冻结的 scope"
        );
    }

    #[test]
    fn shutdown_does_not_enqueue_notify_without_frozen_scope() {
        let dir = tempfile::tempdir().unwrap();
        let reg = InFlight::new();
        let turn = turn_for(900_004, dir.path(), "p2p", "u-old");
        turn.pending.lock().unwrap().delivery_scope.clear();
        reg.insert(TurnKey::new("a", "p-old"), turn);
        let calls = AtomicUsize::new(0);

        assert_eq!(
            reg.shutdown_interrupt_all(Duration::ZERO, &|_| false, &|_, _| {
                calls.fetch_add(1, Ordering::SeqCst);
            }),
            1,
            "无 scope 仍须正常收口无头子进程"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "不能从当前 agent 重新推导旧轮身份后产生 outbox"
        );
    }

    fn chat_tag_t(ct: &str, cid: &str) -> String {
        summon_core::registry::chat_tag(ct, cid)
    }

    #[test]
    fn shutdown_falls_back_to_sigkill_when_grace_expires() {
        // still_alive 头几次返回 true(模拟子进程没被 SIGINT 杀死),计数 still_alive 被查的次数,
        // 验证停机走了「宽限轮询 → 超时 → SIGKILL 兜底」路径(至少查了存活、没卡死)。
        let dir = tempfile::tempdir().unwrap();
        let reg = InFlight::new();
        reg.insert(
            TurnKey::new("a", "p-1"),
            turn_for(900_010, dir.path(), "p2p", "u1"),
        );
        let probes = StdArc::new(AtomicUsize::new(0));
        let p2 = StdArc::clone(&probes);
        let alive = move |_pgid: i32| {
            p2.fetch_add(1, Ordering::SeqCst);
            true // 恒活:逼停机走完宽限再 SIGKILL
        };
        let n = reg.shutdown_interrupt_all(Duration::from_millis(120), &alive, &|_, _| {});
        assert_eq!(n, 1);
        assert!(probes.load(Ordering::SeqCst) >= 1, "应至少探测过一次存活");
    }

    #[test]
    fn shutdown_with_no_inflight_is_noop() {
        let reg = InFlight::new();
        let n = reg.shutdown_interrupt_all(Duration::from_secs(5), &|_| true, &|_, _| {});
        assert_eq!(n, 0, "空注册表:不发任何信号、立即返回");
        let dir = tempfile::tempdir().unwrap();
        assert!(
            !reg.insert(
                TurnKey::new("late", "p-1"),
                turn_for(900_101, dir.path(), "p2p", "u1")
            ),
            "即使快照为空,shutdown 也必须立起屏障"
        );
    }

    // 「停机绝不碰 tmux」(CLAUDE.md 第一红线)此前有一个 shutdown_never_touches_tmux 测试,但它的
    // tmux 销毁 spy 从未接入任何代码、断言 `assert_eq!(0,0)` 恒真——纯剧场,注释还谎称接了 spy。已删。
    // 真守护落在 session::tmux 里的调用点白名单结构测试:全仓销毁 tmux 会话只此一个封装,源码扫描
    // 断言其调用点⊆白名单;本文件(shutdown_interrupt_all 所在)刻意不在白名单——谁往停机/回收/空闲
    // 路径新增 tmux 销毁调用,那个结构测试即变红。
    // 注:InFlight 只装 ChildKiller,类型上就够不到 tmux 句柄,故这里没有可 spy 的运行时入口。

    #[test]
    fn pending_json_write_read_clear_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();
        assert!(read_pending(base, "p-abc").is_none(), "起初无 pending");
        let p = PendingTurn {
            kind: summon_core::pending::PendingTurnKind::Chat,
            delivery_scope: "test-scope".into(),
            agent_name: "blue".into(),
            inbox_message_ids: vec!["om_1".into(), "om_2".into()],
            resume_authorized: true,
            chat_id: "oc_1".into(),
            chat_type: "group".into(),
            cli: "codex".into(),
            prompt: "继续上面的活".into(),
            session_id: Some("th_9".into()),
            partial: "已经写了一半".into(),
        };
        write_pending(base, "p-abc", &p).unwrap();
        assert_eq!(read_pending(base, "p-abc").unwrap(), p);
        clear_pending(base, "p-abc");
        assert!(read_pending(base, "p-abc").is_none(), "清除后应无");
    }

    #[test]
    fn stale_or_legacy_scope_is_deleted_without_resume_side_effects() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();
        for (tag, scope) in [("p-old", "app-old"), ("p-legacy", "")] {
            let pending = PendingTurn {
                kind: summon_core::pending::PendingTurnKind::Chat,
                delivery_scope: scope.into(),
                agent_name: "blue".into(),
                inbox_message_ids: vec!["om_old".into()],
                resume_authorized: true,
                chat_id: "ou_1".into(),
                chat_type: "p2p".into(),
                cli: "codex".into(),
                prompt: "不得重跑".into(),
                session_id: Some("sid-old".into()),
                partial: "不得发出".into(),
            };
            write_pending(base, tag, &pending).unwrap();
            let stream_state = StreamCardState {
                message_id: "om_old".into(),
                chat_tag: tag.into(),
                with_interrupt: true,
                created_ms: 1,
            };
            write_stream_state(base, tag, &stream_state).unwrap();
            assert_eq!(read_stream_state(base, tag), Some(stream_state));

            assert!(read_resumable_pending(base, tag, "app-new", "blue", "ou_1", "p2p").is_none());
            assert!(!pending_path(base, tag).exists(), "旧 pending 必须清除");
            assert!(
                !stream_state_path(base, tag).exists(),
                "旧 app 的卡片 checkpoint 也不能泄漏到新 app"
            );
        }
    }

    #[test]
    fn exact_scope_pending_remains_resumable() {
        let dir = tempfile::tempdir().unwrap();
        let pending = PendingTurn {
            kind: summon_core::pending::PendingTurnKind::Chat,
            delivery_scope: "app-current".into(),
            agent_name: "blue".into(),
            inbox_message_ids: vec!["om_current".into()],
            resume_authorized: true,
            chat_id: "ou_1".into(),
            chat_type: "p2p".into(),
            cli: "codex".into(),
            prompt: "继续".into(),
            session_id: Some("sid-current".into()),
            partial: String::new(),
        };
        write_pending(dir.path(), "p-current", &pending).unwrap();

        assert_eq!(
            read_resumable_pending(
                dir.path(),
                "p-current",
                "app-current",
                "blue",
                "ou_1",
                "p2p"
            ),
            Some(pending)
        );
        assert!(pending_path(dir.path(), "p-current").exists());
    }

    #[test]
    fn read_pending_tolerates_corrupt_file() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();
        let path = pending_path(base, "p-bad");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"{ not valid json").unwrap();
        // 损坏不 panic、当无 pending 处理(退化为冷启,不会卡启动)。
        assert!(read_pending(base, "p-bad").is_none());
        assert!(!path.exists());
    }

    #[test]
    fn read_stream_state_removes_corrupt_checkpoint() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();
        let path = stream_state_path(base, "p-bad-stream");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"{ not valid json").unwrap();

        assert!(read_stream_state(base, "p-bad-stream").is_none());
        assert!(
            !path.exists(),
            "损坏 checkpoint 必须删除，不永久污染后续冷启"
        );
    }

    /// 同时覆盖已消费边界与已发出的 CoT 卡片，避免恢复时丢失轮内进度。
    fn turn_anchor_fixture() -> TurnAnchor {
        TurnAnchor {
            agent_name: "a".into(),
            delivery_scope: "scope-1".into(),
            chat_id: "oc_1".into(),
            chat_type: "group".into(),
            inbox_message_ids: vec!["om_1".into()],
            native_session_id: "sid-1".into(),
            marker: "mk-1".into(),
            prompt_head: "正文开头".into(),
            inject_offset: 100,
            consumed_boundary: Some(240),
            injected_at_ms: 1_000,
            cot: Some(summon_core::TurnAnchorCot {
                cot_id: "c1".into(),
                message_id: "om_c".into(),
            }),
            credential: None,
        }
    }

    #[test]
    fn turn_anchor_write_read_clear_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();
        let a = turn_anchor_fixture();
        write_turn_anchor(base, "gtag", &a).unwrap();
        assert_eq!(read_turn_anchor(base, "gtag"), Some(a));
        clear_turn_anchor(base, "gtag");
        assert_eq!(read_turn_anchor(base, "gtag"), None);
    }

    #[test]
    fn corrupt_turn_anchor_reads_as_none() {
        let dir = tempfile::tempdir().unwrap();
        let p = turn_anchor_path(dir.path(), "gtag");
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, b"{not json").unwrap();
        assert_eq!(read_turn_anchor(dir.path(), "gtag"), None);
        assert!(!p.exists(), "损坏锚点必须删除，不永久污染后续冷启");
    }
}
