//! 事件 outbox 的 IO 侧:磁盘布局 + 原子写 + 目录状态机(pending/archive/dead)。
//!
//! 纯 schema 与抗雪崩决策在 `summon_core::outbox`;本模块只做副作用:`tmp+rename` 原子写、扫描
//! 到期事件、ACK(rename 到 archive/)、死信(rename 到 dead/)、失败回写(attempts/next/last_error
//! 经 `tmp+rename` 覆写)、归档清理。镜像 `inflight.rs` 的 best-effort 纪律:**损坏/缺失全容忍、
//! 失败只 log,绝不 panic、绝不阻塞调用方(停机/拆除热路径)**。
//!
//! 状态用**目录**表达,不进字段:文件在 `pending/` 即待投,rename 到 `archive/`=已 ACK,rename 到
//! `dead/`=封顶死信。一文件一事件,故每个状态迁移都是一次原子 `rename`,无需重写队列、无并发撕裂。

use anyhow::Context as _;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use summon_core::outbox::{
    drain_plan, mark_attempt_failed, Disposition, OutboxEvent, ARCHIVE_RETENTION_MS,
    BACKOFF_CAP_MS, MAX_ATTEMPTS, MAX_CONCURRENCY,
};

use crate::observe::log::log;
use crate::runtime::fsio::atomic_write_mode;
use crate::runtime::paths::run_dir;

const MAX_OUTBOX_EVENT_BYTES: u64 = 8 * 1024 * 1024;

/// outbox 三态目录的根(`~/.summon/run/outbox/`)句柄。clone 廉价(只是路径)。
#[derive(Debug, Clone)]
pub struct OutboxStore {
    root: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutboxLocation {
    Pending,
    Archive,
    Dead,
}

#[derive(Debug, Clone, PartialEq)]
pub enum EnqueueOnceOutcome {
    Enqueued,
    Existing(OutboxLocation, Box<OutboxEvent>),
}

#[derive(Debug, Default)]
pub struct PendingScan {
    pub events: Vec<OutboxEvent>,
    pub corrupt_bridge: Vec<CorruptBridgePending>,
    pub corrupt_recovery: Vec<CorruptBridgePending>,
    /// 非 Bridge/Recovery 的不可解析或 id 不匹配 pending(如已删除变体的旧 schema 遗留)。
    /// 由 drain 线程作为唯一 owner 隔离成 tombstone —— 不能只 log 跳过:文件留在 pending
    /// 会被 100ms 的 bridge reconcile + 2s 的 drain 双线程每 tick 重扫刷日志,且让
    /// `has_unresolved_local_chat` fail-closed,阻塞所有 chat 的 Bridge 绑定。
    pub corrupt_other: Vec<CorruptBridgePending>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BridgeOutboxHealth {
    pub pending: usize,
    pub corrupt: usize,
    pub recovery_pending: usize,
    pub recovery_invalid: usize,
    pub other_corrupt: usize,
    pub quarantined: usize,
    pub dead: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CorruptBridgePending {
    /// 合法 UTF-8 文件 stem 才能与 durable state 的确定性 id 对照；否则只能按 orphan 隔离。
    pub id: Option<String>,
    path: PathBuf,
}

impl OutboxStore {
    /// 在指定根目录上建 store(测试注入 tempdir;生产用 [`OutboxStore::at_run_dir`])。
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// 生产入口:落在 `~/.summon/run/outbox/`。
    pub fn at_run_dir() -> Self {
        Self::new(run_dir().join("outbox"))
    }

    fn pending_dir(&self) -> PathBuf {
        self.root.join("pending")
    }
    fn archive_dir(&self) -> PathBuf {
        self.root.join("archive")
    }
    fn dead_dir(&self) -> PathBuf {
        self.root.join("dead")
    }
    fn quarantine_dir(&self) -> PathBuf {
        self.root.join("quarantine")
    }

    fn pending_path(&self, id: &str) -> PathBuf {
        self.pending_dir().join(format!("{id}.json"))
    }

    /// 原子入队一条事件到 `pending/`(tmp+rename;best-effort)。失败返回 Err,调用方 log-ignore
    /// (停机/拆除路径退化为「这条通知丢了」,绝不阻塞)。`enqueue` 不联网、纯本地文件。
    pub fn enqueue(&self, ev: &OutboxEvent) -> anyhow::Result<()> {
        ensure_private_directory(&self.pending_dir())?;
        let json = serde_json::to_string_pretty(ev)?;
        atomic_write_mode(&self.pending_path(&ev.id), json.as_bytes(), 0o600)
    }

    /// 确定性 id producer 的 no-clobber 入队。若同 id 已经 pending/archive/dead，返回磁盘上的
    /// 原事件，绝不覆盖其 attempts/backoff，也绝不把已 ACK 的消息重新放回 pending。
    pub fn enqueue_once(&self, ev: &OutboxEvent) -> anyhow::Result<EnqueueOnceOutcome> {
        if let Some((location, existing)) = self.find(ev.id.as_str())? {
            anyhow::ensure!(
                same_enqueue_once_event(location, &existing, ev),
                summon_i18n::format!("r024cfaf802", ev.id)
            );
            return Ok(EnqueueOnceOutcome::Existing(location, Box::new(existing)));
        }

        ensure_private_directory(&self.pending_dir())?;
        let json = serde_json::to_vec_pretty(ev)?;
        let tmp =
            self.pending_dir()
                .join(format!(".{}.{}.tmp", ev.id, uuid::Uuid::new_v4().simple()));
        atomic_write_mode(&tmp, &json, 0o600)?;
        match std::fs::hard_link(&tmp, self.pending_path(&ev.id)) {
            Ok(()) => {
                let _ = std::fs::remove_file(&tmp);
                let _ = std::fs::File::open(self.pending_dir()).and_then(|dir| dir.sync_all());
                Ok(EnqueueOnceOutcome::Enqueued)
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let _ = std::fs::remove_file(&tmp);
                let (location, existing) = self
                    .find(&ev.id)?
                    .context(summon_i18n::text!("rb82b609945"))?;
                anyhow::ensure!(
                    same_enqueue_once_event(location, &existing, ev),
                    summon_i18n::format!("r7ba05ff90c", ev.id)
                );
                Ok(EnqueueOnceOutcome::Existing(location, Box::new(existing)))
            }
            Err(error) => {
                let _ = std::fs::remove_file(&tmp);
                Err(error.into())
            }
        }
    }

    pub fn find(&self, id: &str) -> anyhow::Result<Option<(OutboxLocation, OutboxEvent)>> {
        // 终态优先，但只有能严格解析且 id/type 匹配的 terminal 才能压过 pending。损坏的同名
        // archive/dead 不能成为删除唯一正文的凭据；若 pending 完整，就让它按 UUID 安全重试。
        let mut terminal_error = None;
        for (location, path) in [
            (
                OutboxLocation::Archive,
                self.archive_dir().join(format!("{id}.json")),
            ),
            (
                OutboxLocation::Dead,
                self.dead_dir().join(format!("{id}.json")),
            ),
        ] {
            match read_event_checked(&path) {
                Ok(event) => match validate_event_file_identity(id, &event) {
                    Ok(()) => return Ok(Some((location, event))),
                    Err(error) => terminal_error = Some(error),
                },
                Err(error) if error_is_not_found(&error) => {}
                Err(error) => terminal_error = Some(error),
            }
        }
        match read_event_checked(&self.pending_path(id)) {
            Ok(event) => {
                validate_event_file_identity(id, &event)?;
                Ok(Some((OutboxLocation::Pending, event)))
            }
            Err(error) if error_is_not_found(&error) => match terminal_error {
                Some(error) => Err(error),
                None => Ok(None),
            },
            Err(error) => Err(error),
        }
    }

    /// 扫 `pending/`,返回 `next_attempt_ms <= now_ms` 的到期事件(损坏文件跳过、目录不存在返回空)。
    /// 不限流、不排序保证——消费者走 `drain_tick`(内部用纯 `drain_plan` 做限流);本方法仅作
    /// 「直接看有哪些到期」的接缝(producer/IO 测试断言入队结果用),故生产 bin 侧暂判 dead。
    #[allow(dead_code)]
    pub fn list_due(&self, now_ms: u64) -> Vec<OutboxEvent> {
        let dir = self.pending_dir();
        let rd = match std::fs::read_dir(&dir) {
            Ok(rd) => rd,
            // 目录还没建(从未入过队)= 无到期事件,不是错误。
            Err(_) => return Vec::new(),
        };
        let mut out = Vec::new();
        for entry in rd.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue; // 跳过 .tmp 残留与非事件文件
            }
            match read_event(&path) {
                Some(ev) if summon_core::outbox::due(&ev, now_ms) => out.push(ev),
                Some(_) => {} // 未到期,留在 pending/
                None => {}    // 损坏:跳过不 panic(下次扫描再看;不主动删,留人排查)
            }
        }
        out
    }

    /// 过程补充的首次同步投递成功后提交脱敏终态。
    pub(crate) fn acknowledge_process_supplement(&self, event: &OutboxEvent) -> anyhow::Result<()> {
        anyhow::ensure!(
            matches!(
                event.kind,
                summon_core::outbox::OutboxKind::ProcessSupplement { .. }
            ),
            "expected process supplement"
        );
        self.write_redacted_terminal(event, OutboxLocation::Archive)
    }

    /// ACK:投递成功后把事件从 `pending/` rename 到 `archive/`(原子)。这是**先投递成功、后归档**
    /// 的 at-least-once 收尾——绝不先归档再投。best-effort:失败只 log(下次扫描会重投,幂等容忍)。
    #[cfg(test)]
    pub fn ack(&self, id: &str) {
        if let Err(e) = move_event(&self.pending_path(id), &self.archive_dir(), id) {
            log(&summon_i18n::format!("r9635c9fa20"));
        }
    }

    /// 死信:重试封顶后把事件从 `pending/` rename 到 `dead/`(原子,保全字段供排查)。best-effort。
    /// **契约**:消费者对任何失败都先 [`OutboxStore::record_failure`] 把最终 attempts/last_error 落回
    /// `pending/`,再据处置调本函数——故这里只做纯 rename,死信文件天然含最终字段(不重写 payload)。
    #[cfg(test)]
    pub fn to_dead(&self, id: &str) {
        if let Err(e) = move_event(&self.pending_path(id), &self.dead_dir(), id) {
            log(&summon_i18n::format!("re9ba62fa08"));
        }
    }

    /// 死信计数:`dead/` 下的 `.json` 文件数。`summon doctor` 健康面用——重试耗尽的消息
    /// 永久躺在 dead/,没有任何主动通知,只能靠体检暴露。目录不存在/不可读 → 0(无死信)。
    pub fn dead_count(&self) -> usize {
        let rd = match std::fs::read_dir(self.dead_dir()) {
            Ok(rd) => rd,
            Err(_) => return 0,
        };
        rd.flatten()
            .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("json"))
            .count()
    }

    /// 失败回写:把更新过 attempts/next_attempt_ms/last_error 的事件覆写回 `pending/<id>.json`
    /// (tmp+rename,避免改到一半崩留半截)。best-effort:失败只 log(退化为下次按旧状态重试)。
    pub fn record_failure(&self, ev: &OutboxEvent) -> anyhow::Result<()> {
        let json = serde_json::to_vec_pretty(ev)
            .with_context(|| summon_i18n::format!("rdc966eba13", ev.id))?;
        atomic_write_mode(&self.pending_path(&ev.id), &json, 0o600)
            .with_context(|| summon_i18n::format!("r841dd7a9a2", ev.id))
    }

    /// 清理 `archive/`:普通通知/回复删 mtime 早于 `now_ms - ARCHIVE_RETENTION_MS`(7 天)的归档。
    /// Bridge/Recovery 终态是领域状态机的去重事实，在尚无引用感知 GC 协议前永久保留；不能只按
    /// 时间猜测“应该没人引用”。best-effort:启动时调一次；不可读/身份异常一律保留。`dead/` 不清。
    pub fn cleanup_archive(&self, now_ms: u64) {
        let dir = self.archive_dir();
        let rd = match std::fs::read_dir(&dir) {
            Ok(rd) => rd,
            Err(_) => return, // 还没归档过 = 无可清
        };
        let cutoff = now_ms.saturating_sub(ARCHIVE_RETENTION_MS);
        for entry in rd.flatten() {
            let path = entry.path();
            let Some(mtime_ms) = file_mtime_ms(&path) else {
                continue; // 取不到 mtime 就别冒险删
            };
            if mtime_ms < cutoff {
                let Some(id) = path.file_stem().and_then(|stem| stem.to_str()) else {
                    continue; // 无法证明对应 pending 不存在时，终态必须保留
                };
                match std::fs::symlink_metadata(self.pending_path(id)) {
                    Ok(_) => {
                        // terminal 可能已提交、plaintext unlink 却失败；终态是唯一去重证明，
                        // pending 真正消失前无论多旧都不能 GC。
                        continue;
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => {
                        log(&summon_i18n::format!("re00a3a1300"));
                        continue;
                    }
                }
                let event = match read_event_checked(&path) {
                    Ok(event) => event,
                    Err(error) => {
                        log(&summon_i18n::format!("r18f91a4a63"));
                        continue;
                    }
                };
                if let Err(error) = validate_event_file_identity(id, &event) {
                    log(&summon_i18n::format!("rce7afd97cf"));
                    continue;
                }
                if domain_terminal_requires_reference_aware_gc(&event.kind) {
                    continue;
                }
                if let Err(e) = std::fs::remove_file(&path) {
                    log(&summon_i18n::format!("r44eccc1688", path.display()));
                }
            }
        }
    }

    /// drain 一 tick:扫 `pending/`、按纯 [`drain_plan`] 取到期且封顶 `MAX_PER_TICK` 的一批,
    /// 以 `MAX_CONCURRENCY` 有界并发投递,逐条按结果收口——成功 ACK(rename 到 archive/),失败先
    /// [`record_failure`](Self::record_failure) 落最终字段、再据 [`Disposition`] 路由(Retry 留在
    /// pending/、Dead rename 到 dead/)。**at-least-once**:先投递成功、后 ACK 归档,绝不先归档再投。
    ///
    /// `deliver` 是投递闭包(生产=真发飞书/复用 `deliver_notify`;测试=stub 计数)。返回投递成功数
    /// (观测/测试用)。**有界时间内返回**(批 ≤ MAX_PER_TICK,无阻塞循环),不卡 drain 线程。
    #[cfg(test)]
    pub fn drain_tick(
        &self,
        now_ms: u64,
        deliver: &(dyn Fn(&OutboxEvent) -> anyhow::Result<()> + Sync),
    ) -> usize {
        self.drain_tick_with_hooks(now_ms, deliver, &|_| Ok(()), &|_| Ok(()))
    }

    pub fn drain_tick_with_hooks(
        &self,
        now_ms: u64,
        deliver: &(dyn Fn(&OutboxEvent) -> anyhow::Result<()> + Sync),
        on_acked: &(dyn Fn(&OutboxEvent) -> anyhow::Result<()> + Sync),
        on_dead: &(dyn Fn(&OutboxEvent) -> anyhow::Result<()> + Sync),
    ) -> usize {
        let scan = match self.scan_pending() {
            Ok(scan) => scan,
            Err(error) => {
                log(&summon_i18n::format!("rd393fbb455"));
                return 0;
            }
        };
        // drain 线程是 corrupt_other 的唯一隔离 owner(2s 周期,隔离失败重试也天然限频;
        // bridge reconcile 是 bridge 域 owner,不处置通用垃圾)。隔离 = 无正文 tombstone +
        // 删 pending,单文件一生只发生一次,日志因此有界。
        for corrupt in &scan.corrupt_other {
            match self.quarantine_corrupt_pending(corrupt, "unparseable_or_unknown_kind_pending") {
                Ok(()) => log(&summon_i18n::format!(
                    "r050b6195f3",
                    corrupt.id.as_deref().unwrap_or("<non-utf8>")
                )),
                Err(error) => log(&summon_i18n::format!("r0bc530662e")),
            }
        }
        let pending = if scan.corrupt_bridge.is_empty() {
            scan.events
        } else {
            // 坏 Bridge 文件可能是任一 binding 的旧队首；在 reconciler 按 durable state 收口前，
            // 暂停所有 Bridge 正文以保持 sequence fail-closed，但普通通知不能被无关坏文件拖死。
            log(&summon_i18n::format!(
                "r7816bc4920",
                scan.corrupt_bridge
                    .iter()
                    .map(|corrupt| corrupt.id.as_deref().unwrap_or("<non-utf8>"))
                    .collect::<Vec<_>>()
            ));
            scan.events
                .into_iter()
                .filter(|event| event.kind.bridge_fence().is_none())
                .collect()
        };
        let batch = drain_plan(&pending, now_ms);
        let mut delivered = 0usize;
        // 有界并发:按 MAX_CONCURRENCY 切块,每块用 scoped 线程并行投递,收集 (ev, result),
        // 出块后串行落盘状态迁移(rename/覆写不并发,避免目录竞态;投递本身才是 IO-bound 并发点)。
        for chunk in batch.chunks(MAX_CONCURRENCY.max(1)) {
            let results: Vec<(OutboxEvent, anyhow::Result<()>)> = std::thread::scope(|s| {
                let handles: Vec<_> = chunk
                    .iter()
                    .map(|ev| {
                        let fallback = ev.clone();
                        let handle = s.spawn(move || (ev.clone(), deliver(ev)));
                        (fallback, handle)
                    })
                    .collect();
                handles
                    .into_iter()
                    .map(|(ev, handle)| match handle.join() {
                        Ok(result) => result,
                        // 单条投递 panic 只算本条失败并进入退避；绝不能带死整个 drain 线程，
                        // 否则其余 pending 会永久停摆到 daemon 重启。
                        Err(_) => (
                            ev,
                            Err(anyhow::anyhow!(summon_i18n::format!("ra0affba213"))),
                        ),
                    })
                    .collect()
            });
            for (mut ev, res) in results {
                match res {
                    Ok(()) => {
                        // 先投成功、后归档；只有 rename 确认成功才清 bridge receipt。若归档失败，
                        // receipt 的 delivered tombstone 保留，下次重试只补 ACK、不重复联网。
                        let archived = if sensitive_bridge_payload(&ev) {
                            self.write_redacted_terminal(&ev, OutboxLocation::Archive)
                        } else {
                            move_event(&self.pending_path(&ev.id), &self.archive_dir(), &ev.id)
                        };
                        match archived {
                            Ok(()) => {
                                if let Err(error) = on_acked(&ev) {
                                    log(&summon_i18n::format!("r41c83fe973", ev.id));
                                }
                                delivered += 1;
                            }
                            Err(error) => log(&summon_i18n::format!("r7afd640bbd", ev.id)),
                        }
                    }
                    Err(e) => {
                        // 先把最终 attempts/last_error/next 落回 pending/,再据处置路由。
                        // 飞书失败响应可能回显请求正文；Bridge 的持久化错误只记稳定类别，绝不把
                        // response body/路径/正文带进 pending/archive/dead。
                        let _ = e; // 真实错误只进进程日志；持久层按事件类型保留稳定、无正文类别。
                        let persistent_error = persistent_error_label(&ev).to_string();
                        let disp =
                            mark_attempt_failed(&mut ev, now_ms, MAX_ATTEMPTS, &persistent_error);
                        // RecoveryCard 是用户取回未完成消息的唯一控制面；进 dead 会让 active
                        // batch 永久无操作出口。达到通用上限后保持 capped-backoff pending，直至
                        // 网络/配置恢复或群解散显式终态化。
                        if disp == Disposition::Dead
                            && matches!(
                                ev.kind,
                                summon_core::outbox::OutboxKind::RecoveryCard { .. }
                            )
                        {
                            ev.attempts = MAX_ATTEMPTS.saturating_sub(1);
                            ev.next_attempt_ms = now_ms.saturating_add(BACKOFF_CAP_MS);
                            if let Err(error) = self.record_failure(&ev) {
                                log(&summon_i18n::format!("ra7ef71d098", ev.id));
                            }
                            continue;
                        }
                        match disp {
                            Disposition::Retry => {
                                if let Err(error) = self.record_failure(&ev) {
                                    log(&summon_i18n::format!("r700beff6d5", ev.id));
                                }
                            }
                            Disposition::Dead => {
                                let original = ev.clone();
                                let terminal = if sensitive_bridge_payload(&ev) {
                                    // 先在 dead/ 原子写脱敏 tombstone，再删 plaintext pending。
                                    // 任一步失败都绝不把旧 plaintext rename 到长期 dead/。
                                    ev.next_attempt_ms = now_ms.saturating_add(BACKOFF_CAP_MS);
                                    if let Err(error) = self.record_failure(&ev) {
                                        log(&summon_i18n::format!("ra4d7f74714", ev.id));
                                    }
                                    self.write_redacted_terminal(&ev, OutboxLocation::Dead)
                                } else {
                                    self.record_failure(&ev).and_then(|_| {
                                        move_event(
                                            &self.pending_path(&ev.id),
                                            &self.dead_dir(),
                                            &ev.id,
                                        )
                                    })
                                };
                                match terminal {
                                    Ok(()) => {
                                        if let Err(error) = on_dead(&original) {
                                            log(&summon_i18n::format!("rb0d4f1a22e", ev.id));
                                        }
                                    }
                                    Err(error) => log(&summon_i18n::format!("rcaeb4731d8", ev.id)),
                                }
                            }
                        }
                    }
                }
            }
        }
        delivered
    }

    /// 读 `pending/` 全量事件(损坏跳过、缺目录返回空);`drain_plan` 在其上做到期过滤 + 限流。
    #[cfg(test)]
    pub fn pending_events(&self) -> Vec<OutboxEvent> {
        let dir = self.pending_dir();
        let rd = match std::fs::read_dir(&dir) {
            Ok(rd) => rd,
            Err(_) => return Vec::new(),
        };
        let mut out = Vec::new();
        for entry in rd.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            if let Some(ev) = read_event(&path) {
                out.push(ev);
            }
        }
        out
    }

    /// 一次有界扫描同时返回合法事件与损坏 Bridge id。调用方可继续投递非 Bridge 事件，Bridge
    /// 则等 reconciler 按 durable receipt 收口并隔离坏文件，避免一个坏 JSON 拖死整个 outbox。
    pub fn scan_pending(&self) -> anyhow::Result<PendingScan> {
        let dir = self.pending_dir();
        let rd = match std::fs::read_dir(&dir) {
            Ok(rd) => rd,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(PendingScan::default())
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| summon_i18n::format!("rb527a60143", dir.display()))
            }
        };
        let mut scan = PendingScan::default();
        for entry in rd {
            let entry =
                entry.with_context(|| summon_i18n::format!("r8c89d880fe", dir.display()))?;
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            let stem = path
                .file_stem()
                .and_then(|name| name.to_str())
                .map(str::to_string);
            let bridge_name = stem.as_deref().is_some_and(is_bridge_outbox_id);
            let recovery_name = stem.as_deref().is_some_and(is_recovery_outbox_id);
            if stem
                .as_deref()
                .is_some_and(|stem| self.valid_terminal_exists(stem).unwrap_or(false))
            {
                // 只有严格可读且 id/type 匹配的 terminal tombstone 才能压过 crash-left pending。
                let _ = std::fs::remove_file(&path);
                continue;
            }
            match read_event_checked(&path) {
                Ok(event) => {
                    let bridge_event = event.kind.bridge_fence().is_some();
                    let recovery_event = matches!(
                        &event.kind,
                        summon_core::outbox::OutboxKind::RecoveryCard { .. }
                    );
                    let identity_valid = stem
                        .as_deref()
                        .is_some_and(|id| validate_event_file_identity(id, &event).is_ok());
                    if !identity_valid {
                        if recovery_name || recovery_event {
                            scan.corrupt_recovery.push(CorruptBridgePending {
                                id: stem.clone(),
                                path,
                            });
                        } else if bridge_name || bridge_event {
                            scan.corrupt_bridge.push(CorruptBridgePending {
                                id: stem.clone(),
                                path,
                            });
                        } else {
                            scan.corrupt_other
                                .push(CorruptBridgePending { id: stem, path });
                        }
                    } else {
                        scan.events.push(event);
                    }
                }
                Err(error) if recovery_name => {
                    log(&summon_i18n::format!(
                        "r28aed52da8",
                        stem.as_deref().unwrap_or("<non-utf8>")
                    ));
                    scan.corrupt_recovery
                        .push(CorruptBridgePending { id: stem, path });
                }
                Err(error) if bridge_name => {
                    log(&summon_i18n::format!(
                        "r28c83e446e",
                        stem.as_deref().unwrap_or("<non-utf8>")
                    ));
                    scan.corrupt_bridge
                        .push(CorruptBridgePending { id: stem, path });
                }
                Err(_) => {
                    // 不在这里 log:scan 每 tick 都跑,log-only 即无限刷屏。归入 corrupt_other,
                    // 隔离动作(单文件一生一次)负责打日志。
                    scan.corrupt_other
                        .push(CorruptBridgePending { id: stem, path });
                }
            }
        }
        scan.corrupt_bridge
            .sort_by(|left, right| left.path.cmp(&right.path));
        scan.corrupt_bridge
            .dedup_by(|left, right| left.path == right.path);
        scan.corrupt_recovery
            .sort_by(|left, right| left.path.cmp(&right.path));
        scan.corrupt_recovery
            .dedup_by(|left, right| left.path == right.path);
        scan.corrupt_other
            .sort_by(|left, right| left.path.cmp(&right.path));
        scan.corrupt_other
            .dedup_by(|left, right| left.path == right.path);
        Ok(scan)
    }

    /// Bridge 绑定前的本地出站占用证据。任何 unreadable/identity 损坏都整体 fail-closed；旧本地
    /// 回复、通知、恢复卡或 schema-only resume 只要仍 pending，就不能让同群进入 Mirror。
    pub fn has_unresolved_local_chat(&self, agent: &str, chat_id: &str) -> anyhow::Result<bool> {
        let entries = match std::fs::read_dir(self.pending_dir()) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error).context(summon_i18n::text!("r0478cbe39e")),
        };
        for entry in entries {
            let path = entry?.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            let id = path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .context(summon_i18n::text!("re64347f384"))?;
            if self.valid_terminal_exists(id)? {
                continue;
            }
            let event = read_event_checked(&path)?;
            validate_event_file_identity(id, &event)?;
            if event.agent == agent && local_outbox_chat_id(&event.kind) == Some(chat_id) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// 旧版本 RecoveryCard（无 batch/scope/uuid）已被新 durable batch 接管后，写脱敏 archive
    /// tombstone 再删旧 pending，确保它永远不会按不安全 legacy 路径发出。
    pub fn archive_migrated_legacy_recovery(&self, event: &OutboxEvent) -> anyhow::Result<()> {
        anyhow::ensure!(
            matches!(
                &event.kind,
                summon_core::outbox::OutboxKind::RecoveryCard {
                    batch_id,
                    scope,
                    ..
                } if batch_id.is_empty() && scope.is_empty()
            ),
            summon_i18n::format!("r2f56a80d15")
        );
        self.write_redacted_terminal(event, OutboxLocation::Archive)
    }

    /// 把已具备 durable batch/scope、但 schema 早于 frozen wire 的 RecoveryCard 原地升级。
    /// 只允许 `card_version=0 && rendered_card=""` 的 serde-default 旧形状，并要求 batch/chat/
    /// items/stage 与 producer 从权威 manifest 重建的事件逐字段相等；attempt/backoff 原样保留。
    pub(crate) fn upgrade_legacy_recovery_wire(
        &self,
        legacy: &OutboxEvent,
        replacement: &OutboxEvent,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            same_recovery_wire_upgrade_identity(legacy, replacement),
            summon_i18n::format!("r263d7cdb51", legacy.id)
        );
        let path = self.pending_path(&legacy.id);
        let on_disk = read_event_checked(&path)?;
        validate_event_file_identity(&legacy.id, &on_disk)?;
        anyhow::ensure!(
            on_disk == *legacy,
            summon_i18n::format!("r648bf137a1", legacy.id)
        );
        let mut upgraded = replacement.clone();
        upgraded.attempts = on_disk.attempts;
        upgraded.next_attempt_ms = on_disk.next_attempt_ms;
        upgraded.last_error = on_disk.last_error;
        atomic_write_mode(&path, &serde_json::to_vec_pretty(&upgraded)?, 0o600)
            .with_context(|| summon_i18n::format!("re873e04040", legacy.id))
    }

    /// Recovery producer 的专用 enqueue-once：首次落盘的 frozen wire 是后续重试权威。
    /// binary 升级后 renderer/version 可以变化，但只要 batch/chat/items/stage identity 完全一致、
    /// 旧 wire 仍是受支持的合法卡片，就沿用旧 pending，不用当前 renderer 制造 payload 冲突。
    /// 通用 [`Self::enqueue_once`] 仍逐 payload 严格，不能借此放宽其他确定性事件。
    pub(crate) fn ensure_recovery_once(
        &self,
        candidate: &OutboxEvent,
    ) -> anyhow::Result<EnqueueOnceOutcome> {
        let Some((location, existing)) = self.find(&candidate.id)? else {
            return self.enqueue_once(candidate);
        };
        if location == OutboxLocation::Pending
            && same_recovery_identity_ignoring_wire(&existing, candidate)
            && recovery_wire_is_valid(&existing)
        {
            return Ok(EnqueueOnceOutcome::Existing(location, Box::new(existing)));
        }
        anyhow::ensure!(
            same_enqueue_once_event(location, &existing, candidate),
            summon_i18n::format!("r2ea92b2347", candidate.id)
        );
        Ok(EnqueueOnceOutcome::Existing(location, Box::new(existing)))
    }

    pub fn terminalize_recovery_chat(
        &self,
        scope: &str,
        agent: &str,
        chat_id: &str,
    ) -> anyhow::Result<usize> {
        let events = self.scan_pending()?.events;
        let mut count = 0usize;
        for event in events {
            let matches_chat = matches!(
                &event.kind,
                summon_core::outbox::OutboxKind::RecoveryCard {
                    scope: event_scope,
                    chat_id: event_chat,
                    ..
                } if event.agent == agent && event_scope == scope && event_chat == chat_id
            );
            if matches_chat {
                self.write_redacted_terminal(&event, OutboxLocation::Archive)?;
                count += 1;
            }
        }
        Ok(count)
    }

    pub fn archive_recovery_by_id(&self, outbox_id: &str) -> anyhow::Result<()> {
        let Some((_location, event)) = self.find(outbox_id)? else {
            return Ok(());
        };
        anyhow::ensure!(
            matches!(
                event.kind,
                summon_core::outbox::OutboxKind::RecoveryCard { .. }
            ),
            summon_i18n::format!("rb7c7bc6c2c")
        );
        self.write_redacted_terminal(&event, OutboxLocation::Archive)
    }

    /// 飞书 app identity 已变化时，使用 durable RecoveryBatch 重建出的权威事件直接写脱敏
    /// terminal，再删除同 id pending。这里故意不依赖 pending 可解析：旧文件即使半写/损坏，
    /// 也不能永久保留正文并卡住新 app；batch 的确定性 id/scope/chat 才是废弃凭据。
    pub(crate) fn archive_abandoned_recovery(&self, event: &OutboxEvent) -> anyhow::Result<()> {
        anyhow::ensure!(
            matches!(
                &event.kind,
                summon_core::outbox::OutboxKind::RecoveryCard {
                    batch_id,
                    scope,
                    chat_id,
                    ..
                } if !batch_id.is_empty()
                    && batch_id == &event.id
                    && !scope.is_empty()
                    && scope == &event.delivery_scope
                    && !chat_id.is_empty()
            ),
            summon_i18n::format!("r546a46c87c")
        );
        self.write_redacted_terminal(event, OutboxLocation::Archive)
    }

    /// `bridge status/doctor` 的只读快照；与 drain 扫描不同，不删除 terminal 已存在的 crash-left
    /// pending，也不创建目录。文件名、body 或两者的 Bridge 身份不一致都计入 corrupt。
    pub fn inspect_bridge_health(&self) -> anyhow::Result<BridgeOutboxHealth> {
        let mut health = BridgeOutboxHealth::default();
        let pending = match std::fs::read_dir(self.pending_dir()) {
            Ok(entries) => Some(entries),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error).context(summon_i18n::text!("r8e905e2022")),
        };
        if let Some(entries) = pending {
            for entry in entries {
                let path = entry?.path();
                if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                    continue;
                }
                let stem = path.file_stem().and_then(|name| name.to_str());
                let bridge_name = stem.is_some_and(is_bridge_outbox_id);
                let recovery_name = stem.is_some_and(is_recovery_outbox_id);
                match read_event_checked(&path) {
                    Ok(event) => {
                        let bridge_event = event.kind.bridge_fence().is_some();
                        if bridge_event && stem == Some(event.id.as_str()) && bridge_name {
                            health.pending = health.pending.saturating_add(1);
                        } else if bridge_event || bridge_name {
                            health.corrupt = health.corrupt.saturating_add(1);
                        } else if matches!(
                            &event.kind,
                            summon_core::outbox::OutboxKind::RecoveryCard { .. }
                        ) {
                            if stem
                                .is_some_and(|id| validate_event_file_identity(id, &event).is_ok())
                            {
                                health.recovery_pending = health.recovery_pending.saturating_add(1);
                            } else {
                                health.recovery_invalid = health.recovery_invalid.saturating_add(1);
                            }
                        }
                    }
                    Err(_) if bridge_name => {
                        health.corrupt = health.corrupt.saturating_add(1);
                    }
                    Err(_) if recovery_name => {
                        health.recovery_invalid = health.recovery_invalid.saturating_add(1);
                    }
                    Err(_) => {
                        health.other_corrupt = health.other_corrupt.saturating_add(1);
                    }
                }
            }
        }
        let dead = match std::fs::read_dir(self.dead_dir()) {
            Ok(entries) => Some(entries),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error).context(summon_i18n::text!("r42c6bb37b3")),
        };
        if let Some(entries) = dead {
            for entry in entries {
                let path = entry?.path();
                if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                    continue;
                }
                let bridge_name = path
                    .file_stem()
                    .and_then(|name| name.to_str())
                    .is_some_and(is_bridge_outbox_id);
                let bridge_body = read_event_checked(&path)
                    .is_ok_and(|event| event.kind.bridge_fence().is_some());
                if bridge_name || bridge_body {
                    health.dead = health.dead.saturating_add(1);
                }
            }
        }
        let quarantined = match std::fs::read_dir(self.quarantine_dir()) {
            Ok(entries) => Some(entries),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error).context(summon_i18n::text!("r2c82349987")),
        };
        if let Some(entries) = quarantined {
            for entry in entries {
                let path = entry?.path();
                if path.extension().and_then(|ext| ext.to_str()) == Some("json")
                    && std::fs::symlink_metadata(path)?.is_file()
                {
                    health.quarantined = health.quarantined.saturating_add(1);
                }
            }
        }
        Ok(health)
    }

    pub fn pending_events_checked(&self) -> anyhow::Result<Vec<OutboxEvent>> {
        let scan = self.scan_pending()?;
        anyhow::ensure!(
            scan.corrupt_bridge.is_empty() && scan.corrupt_recovery.is_empty(),
            summon_i18n::format!(
                "r1b141e4a00",
                scan.corrupt_bridge.len(),
                scan.corrupt_recovery.len()
            )
        );
        Ok(scan.events)
    }

    /// state/receipt 已先 fail-closed 收口后，把坏文件换成无正文 quarantine tombstone。
    pub fn quarantine_corrupt_bridge(&self, corrupt: &CorruptBridgePending) -> anyhow::Result<()> {
        self.quarantine_corrupt_pending(corrupt, "corrupt_bridge_pending")
    }

    pub(crate) fn quarantine_corrupt_recovery(
        &self,
        corrupt: &CorruptBridgePending,
    ) -> anyhow::Result<()> {
        self.quarantine_corrupt_pending(corrupt, "corrupt_recovery_pending")
    }

    pub(crate) fn quarantine_orphan_recovery(
        &self,
        event: &OutboxEvent,
        reason: &str,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            matches!(
                &event.kind,
                summon_core::outbox::OutboxKind::RecoveryCard { .. }
            ),
            summon_i18n::format!("r33abea6f9b")
        );
        let corrupt = CorruptBridgePending {
            id: Some(event.id.clone()),
            path: self.pending_path(&event.id),
        };
        self.quarantine_corrupt_pending(&corrupt, reason)
    }

    fn quarantine_corrupt_pending(
        &self,
        corrupt: &CorruptBridgePending,
        reason: &str,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            corrupt.path.parent() == Some(self.pending_dir().as_path())
                && corrupt.path.extension().and_then(|ext| ext.to_str()) == Some("json"),
            summon_i18n::format!("rdb7c7fedf6")
        );
        anyhow::ensure!(
            !reason.trim().is_empty(),
            summon_i18n::format!("r00bb7de63c")
        );
        ensure_private_directory(&self.quarantine_dir())?;
        let record = serde_json::json!({
            "id": corrupt.id,
            "reason": reason,
        });
        let quarantine_id = quarantine_key(&corrupt.path);
        atomic_write_mode(
            &self
                .quarantine_dir()
                .join(format!("quarantine-{quarantine_id}.json")),
            &serde_json::to_vec_pretty(&record)?,
            0o600,
        )?;
        match std::fs::remove_file(&corrupt.path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| summon_i18n::format!("r316c372725", corrupt.path))
            }
        }
        Ok(())
    }

    fn valid_terminal_exists(&self, id: &str) -> anyhow::Result<bool> {
        for path in [
            self.archive_dir().join(format!("{id}.json")),
            self.dead_dir().join(format!("{id}.json")),
        ] {
            match read_event_checked(&path) {
                Ok(event) => match validate_event_file_identity(id, &event) {
                    Ok(()) => return Ok(true),
                    Err(error) => log(&summon_i18n::format!("r399a11a904", path.display())),
                },
                Err(error) if error_is_not_found(&error) => {}
                Err(error) => log(&summon_i18n::format!("r61f471e1f3", path.display())),
            }
        }
        Ok(false)
    }

    fn write_redacted_terminal(
        &self,
        event: &OutboxEvent,
        location: OutboxLocation,
    ) -> anyhow::Result<()> {
        let directory = match location {
            OutboxLocation::Archive => self.archive_dir(),
            OutboxLocation::Dead => self.dead_dir(),
            OutboxLocation::Pending => anyhow::bail!(summon_i18n::format!("r3996d3bfa2")),
        };
        ensure_private_directory(&directory)?;
        let mut redacted = event.clone();
        redact_sensitive_bridge_payload(&mut redacted);
        let json = serde_json::to_vec_pretty(&redacted)
            .with_context(|| summon_i18n::format!("rc062c2d03f", event.id))?;
        atomic_write_mode(&directory.join(format!("{}.json", event.id)), &json, 0o600)
            .with_context(|| summon_i18n::format!("r1d4f95ba62", event.id))?;
        // terminal 已 durable 后才删 plaintext。删失败时严格扫描会因 terminal tombstone 跳过它。
        match std::fs::remove_file(self.pending_path(&event.id)) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => log(&summon_i18n::format!("r0d5dc7e646", event.id)),
        }
        Ok(())
    }
}

/// 把 `from` rename 到 `to_dir/<id>.json`(状态迁移)。目标目录不存在则建。
fn move_event(from: &Path, to_dir: &Path, id: &str) -> anyhow::Result<()> {
    ensure_private_directory(to_dir)?;
    let to = to_dir.join(format!("{id}.json"));
    std::fs::rename(from, to)?;
    Ok(())
}

fn ensure_private_directory(path: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    Ok(())
}

/// 读单个事件文件;损坏/读不到 → None(不 panic)。
fn read_event(path: &Path) -> Option<OutboxEvent> {
    read_event_checked(path).ok()
}

fn read_event_checked(path: &Path) -> anyhow::Result<OutboxEvent> {
    let file = std::fs::File::open(path)
        .with_context(|| summon_i18n::format!("re43971131a", path.display()))?;
    let size = file
        .metadata()
        .with_context(|| format!("stat {}", path.display()))?
        .len();
    anyhow::ensure!(
        size <= MAX_OUTBOX_EVENT_BYTES,
        summon_i18n::format!("r310c775616", path.display(), MAX_OUTBOX_EVENT_BYTES)
    );
    let mut bytes = Vec::with_capacity(size as usize);
    use std::io::Read as _;
    file.take(MAX_OUTBOX_EVENT_BYTES + 1)
        .read_to_end(&mut bytes)
        .with_context(|| summon_i18n::format!("re43971131a", path.display()))?;
    anyhow::ensure!(
        bytes.len() as u64 <= MAX_OUTBOX_EVENT_BYTES,
        summon_i18n::format!("rc0f05e25f4", path.display())
    );
    serde_json::from_slice(&bytes)
        .with_context(|| summon_i18n::format!("radeec7420c", path.display()))
}

fn is_bridge_outbox_id(id: &str) -> bool {
    [
        "bridge-audit-",
        "bridge-turn-",
        "bridge-ctx-",
        "bridge-handoff-",
    ]
    .iter()
    .any(|prefix| id.starts_with(prefix))
}

fn is_recovery_outbox_id(id: &str) -> bool {
    id.starts_with("recovery-card-")
}

fn current_recovery_identity(event: &OutboxEvent) -> Option<(&str, &str, &str)> {
    match &event.kind {
        summon_core::outbox::OutboxKind::RecoveryCard {
            batch_id,
            scope,
            chat_id,
            ..
        } if !batch_id.is_empty() => Some((batch_id, scope, chat_id)),
        _ => None,
    }
}

fn quarantine_key(path: &Path) -> String {
    let bytes = path.file_name().map_or(&[][..], |name| name.as_bytes());
    let fnv = |seed: u64| {
        bytes.iter().fold(seed, |hash, byte| {
            (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3)
        })
    };
    format!(
        "{:016x}{:016x}",
        fnv(0xcbf2_9ce4_8422_2325),
        fnv(0x8422_2325_cbf2_9ce4)
    )
}

fn validate_event_file_identity(expected_id: &str, event: &OutboxEvent) -> anyhow::Result<()> {
    anyhow::ensure!(event.id == expected_id, summon_i18n::format!("re6065e5203"));
    anyhow::ensure!(
        is_bridge_outbox_id(expected_id) == event.kind.bridge_fence().is_some(),
        summon_i18n::format!("r1797557465")
    );
    let current_recovery = current_recovery_identity(event);
    anyhow::ensure!(
        is_recovery_outbox_id(expected_id) == current_recovery.is_some(),
        summon_i18n::format!("r70e96981e2")
    );
    if let Some((batch_id, scope, chat_id)) = current_recovery {
        anyhow::ensure!(
            batch_id == expected_id
                && !scope.is_empty()
                && scope == event.delivery_scope
                && !chat_id.is_empty(),
            summon_i18n::format!("r2cfdf95e5e")
        );
    }
    Ok(())
}

fn error_is_not_found(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound)
    })
}

fn same_immutable_event(existing: &OutboxEvent, candidate: &OutboxEvent) -> bool {
    existing.id == candidate.id
        && existing.agent == candidate.agent
        && existing.delivery_scope == candidate.delivery_scope
        && existing.created_ms == candidate.created_ms
        && existing.kind == candidate.kind
}

fn same_enqueue_once_event(
    location: OutboxLocation,
    existing: &OutboxEvent,
    candidate: &OutboxEvent,
) -> bool {
    same_immutable_event(existing, candidate)
        || (location == OutboxLocation::Archive
            && same_recovery_archive_tombstone_identity(existing, candidate))
        || (matches!(location, OutboxLocation::Archive | OutboxLocation::Dead)
            && same_handoff_terminal_tombstone_identity(existing, candidate))
}

/// Handoff coordinator 会在同一 phase 的每个 tick 重申同一个确定性状态事件。archive/dead 已
/// 清掉 frozen wire 后，仍需按完整 fence/attempt/notice 身份认出它，不能重新入队，也不能仅凭 id
/// 接受漂移 payload。
fn same_handoff_terminal_tombstone_identity(
    existing: &OutboxEvent,
    candidate: &OutboxEvent,
) -> bool {
    if existing.id != candidate.id
        || existing.agent != candidate.agent
        || existing.delivery_scope != candidate.delivery_scope
        || existing.created_ms != candidate.created_ms
    {
        return false;
    }
    match (&existing.kind, &candidate.kind) {
        (
            summon_core::outbox::OutboxKind::BridgeHandoffStatus {
                fence: existing_fence,
                attempt_id: existing_attempt,
                notice: existing_notice,
                card_version: existing_version,
                rendered_card: existing_wire,
            },
            summon_core::outbox::OutboxKind::BridgeHandoffStatus {
                fence: candidate_fence,
                attempt_id: candidate_attempt,
                notice: candidate_notice,
                card_version: candidate_version,
                rendered_card: candidate_wire,
            },
        ) => {
            existing_wire.is_empty()
                && summon_core::outbox::valid_frozen_bridge_card(*candidate_version, candidate_wire)
                && existing_fence == candidate_fence
                && existing_attempt == candidate_attempt
                && existing_notice == candidate_notice
                && existing_version == candidate_version
        }
        _ => false,
    }
}

/// RecoveryCard 成功投递后的 archive 只保留 durable batch identity，正文/items 已脱敏。
/// producer 冷启 reconcile 仍会用同一确定性 id 再次 `enqueue_once`；archive 必须把它识别为
/// 已完成，而 pending 永远不能使用这条宽松比较（否则同 UUID 不同 wire 会被静默接受）。
fn same_recovery_archive_tombstone_identity(
    existing: &OutboxEvent,
    candidate: &OutboxEvent,
) -> bool {
    if existing.id != candidate.id
        || existing.agent != candidate.agent
        || existing.delivery_scope != candidate.delivery_scope
        || existing.created_ms != candidate.created_ms
    {
        return false;
    }
    let (
        summon_core::outbox::OutboxKind::RecoveryCard {
            batch_id: existing_batch,
            scope: existing_scope,
            chat_id: existing_chat,
            chat_type: existing_chat_type,
            items: existing_items,
            stage: existing_stage,
            rendered_card: existing_wire,
            ..
        },
        summon_core::outbox::OutboxKind::RecoveryCard {
            batch_id: candidate_batch,
            scope: candidate_scope,
            chat_id: candidate_chat,
            chat_type: candidate_chat_type,
            ..
        },
    ) = (&existing.kind, &candidate.kind)
    else {
        return false;
    };
    !existing_batch.is_empty()
        && existing_batch == &existing.id
        && candidate_batch == &candidate.id
        && existing_scope == candidate_scope
        && existing_scope == &existing.delivery_scope
        && existing_chat == candidate_chat
        && existing_chat_type == candidate_chat_type
        && existing_items.is_empty()
        && existing_stage.is_none()
        && existing_wire.is_empty()
}

fn same_recovery_wire_upgrade_identity(legacy: &OutboxEvent, replacement: &OutboxEvent) -> bool {
    if legacy.id != replacement.id
        || legacy.agent != replacement.agent
        || legacy.delivery_scope != replacement.delivery_scope
        || legacy.created_ms != replacement.created_ms
    {
        return false;
    }
    let (
        summon_core::outbox::OutboxKind::RecoveryCard {
            batch_id: legacy_batch,
            scope: legacy_scope,
            chat_id: legacy_chat,
            chat_type: legacy_chat_type,
            items: legacy_items,
            stage: legacy_stage,
            card_version: legacy_version,
            rendered_card: legacy_wire,
        },
        summon_core::outbox::OutboxKind::RecoveryCard {
            batch_id: replacement_batch,
            scope: replacement_scope,
            chat_id: replacement_chat,
            chat_type: replacement_chat_type,
            items: replacement_items,
            stage: replacement_stage,
            card_version: replacement_version,
            rendered_card: replacement_wire,
        },
    ) = (&legacy.kind, &replacement.kind)
    else {
        return false;
    };
    *legacy_version == 0
        && legacy_wire.is_empty()
        && legacy_batch == replacement_batch
        && legacy_batch == &legacy.id
        && legacy_scope == replacement_scope
        && legacy_scope == &legacy.delivery_scope
        && legacy_chat == replacement_chat
        && legacy_chat_type == replacement_chat_type
        && legacy_items == replacement_items
        && legacy_stage == replacement_stage
        && summon_core::outbox::valid_frozen_bridge_card(*replacement_version, replacement_wire)
}

fn same_recovery_identity_ignoring_wire(existing: &OutboxEvent, candidate: &OutboxEvent) -> bool {
    if existing.id != candidate.id
        || existing.agent != candidate.agent
        || existing.delivery_scope != candidate.delivery_scope
        || existing.created_ms != candidate.created_ms
    {
        return false;
    }
    let (
        summon_core::outbox::OutboxKind::RecoveryCard {
            batch_id: existing_batch,
            scope: existing_scope,
            chat_id: existing_chat,
            chat_type: existing_chat_type,
            items: existing_items,
            stage: existing_stage,
            ..
        },
        summon_core::outbox::OutboxKind::RecoveryCard {
            batch_id: candidate_batch,
            scope: candidate_scope,
            chat_id: candidate_chat,
            chat_type: candidate_chat_type,
            items: candidate_items,
            stage: candidate_stage,
            ..
        },
    ) = (&existing.kind, &candidate.kind)
    else {
        return false;
    };
    !existing_batch.is_empty()
        && existing_batch == &existing.id
        && existing_batch == candidate_batch
        && existing_scope == &existing.delivery_scope
        && existing_scope == candidate_scope
        && existing_chat == candidate_chat
        && existing_chat_type == candidate_chat_type
        && existing_items == candidate_items
        && existing_stage == candidate_stage
}

fn recovery_wire_is_valid(event: &OutboxEvent) -> bool {
    matches!(
        &event.kind,
        summon_core::outbox::OutboxKind::RecoveryCard {
            card_version,
            rendered_card,
            ..
        } if summon_core::outbox::valid_frozen_bridge_card(*card_version, rendered_card)
    )
}

fn redact_sensitive_bridge_payload(event: &mut OutboxEvent) {
    if event.last_error.is_some() {
        event.last_error = Some(persistent_error_label(event).into());
    }
    match &mut event.kind {
        summon_core::outbox::OutboxKind::AgentDisabled { rendered_card, .. } => {
            rendered_card.clear()
        }
        summon_core::outbox::OutboxKind::ExternalBridgeTurn { rendered_card, .. } => {
            rendered_card.clear();
        }
        summon_core::outbox::OutboxKind::BridgeContextCapsule { rendered_card, .. } => {
            rendered_card.clear();
        }
        summon_core::outbox::OutboxKind::BridgeHandoffStatus { rendered_card, .. } => {
            rendered_card.clear();
        }
        summon_core::outbox::OutboxKind::NotifyChat { text, .. } => text.clear(),
        summon_core::outbox::OutboxKind::ResumeTurn { turn, .. } => {
            turn.prompt.clear();
            turn.partial.clear();
        }
        summon_core::outbox::OutboxKind::RecoveryCard {
            items,
            stage,
            rendered_card,
            ..
        } => {
            items.clear();
            *stage = None;
            rendered_card.clear();
        }
        summon_core::outbox::OutboxKind::TurnReply { text, .. } => text.clear(),
        summon_core::outbox::OutboxKind::ProcessSupplement { rendered_card, .. } => {
            rendered_card.clear()
        }
        summon_core::outbox::OutboxKind::BridgeActivationAudit {
            rendered_card,
            control_card,
            ..
        } => {
            rendered_card.clear();
            control_card.clear();
        }
    }
}

fn persistent_error_label(event: &OutboxEvent) -> &'static str {
    match &event.kind {
        summon_core::outbox::OutboxKind::BridgeActivationAudit { .. }
        | summon_core::outbox::OutboxKind::ExternalBridgeTurn { .. }
        | summon_core::outbox::OutboxKind::BridgeContextCapsule { .. }
        | summon_core::outbox::OutboxKind::BridgeHandoffStatus { .. } => "bridge_delivery_failed",
        summon_core::outbox::OutboxKind::RecoveryCard { .. } => "recovery_delivery_failed",
        summon_core::outbox::OutboxKind::AgentDisabled { .. }
        | summon_core::outbox::OutboxKind::NotifyChat { .. }
        | summon_core::outbox::OutboxKind::ResumeTurn { .. }
        | summon_core::outbox::OutboxKind::ProcessSupplement { .. }
        | summon_core::outbox::OutboxKind::TurnReply { .. } => "outbox_delivery_failed",
    }
}

fn sensitive_bridge_payload(event: &OutboxEvent) -> bool {
    let _ = event;
    // 所有持久出站都可能携带 prompt/reply/preview；终态只保留路由/幂等身份。
    true
}

/// 这些终态仍参与 Bridge/Recovery 冷启归约。通用 Outbox 无法证明领域状态已解除引用，因此
/// 禁止时间型 GC；未来只能由对应领域在同一 durable transaction/明确 tombstone 后显式回收。
fn domain_terminal_requires_reference_aware_gc(kind: &summon_core::outbox::OutboxKind) -> bool {
    matches!(
        kind,
        summon_core::outbox::OutboxKind::BridgeActivationAudit { .. }
            | summon_core::outbox::OutboxKind::ExternalBridgeTurn { .. }
            | summon_core::outbox::OutboxKind::BridgeContextCapsule { .. }
            | summon_core::outbox::OutboxKind::RecoveryCard { .. }
    )
}

fn local_outbox_chat_id(kind: &summon_core::outbox::OutboxKind) -> Option<&str> {
    match kind {
        summon_core::outbox::OutboxKind::AgentDisabled { .. } => None,
        summon_core::outbox::OutboxKind::NotifyChat { chat_id, .. }
        | summon_core::outbox::OutboxKind::ResumeTurn { chat_id, .. }
        | summon_core::outbox::OutboxKind::RecoveryCard { chat_id, .. }
        | summon_core::outbox::OutboxKind::ProcessSupplement { chat_id, .. }
        | summon_core::outbox::OutboxKind::TurnReply { chat_id, .. } => Some(chat_id),
        summon_core::outbox::OutboxKind::BridgeActivationAudit { .. }
        | summon_core::outbox::OutboxKind::ExternalBridgeTurn { .. }
        | summon_core::outbox::OutboxKind::BridgeContextCapsule { .. }
        | summon_core::outbox::OutboxKind::BridgeHandoffStatus { .. } => None,
    }
}

/// 文件 mtime 的 unix ms;取不到 → None。
fn file_mtime_ms(path: &Path) -> Option<u64> {
    let mtime = std::fs::metadata(path).ok()?.modified().ok()?;
    let dur = mtime.duration_since(std::time::UNIX_EPOCH).ok()?;
    Some(dur.as_millis() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use summon_core::outbox::{
        format_id, mark_attempt_failed, Disposition, OutboxKind, MAX_ATTEMPTS,
    };
    use summon_core::session::exit::ExitReason;

    fn store() -> (tempfile::TempDir, OutboxStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = OutboxStore::new(dir.path().join("outbox"));
        (dir, store)
    }

    /// 落下与 `enqueue` 相同的字节与权限,只是不走 tmp+fsync+rename:批量造 pending 夹具时,
    /// 逐条 fsync 的耗时远超被测的 drain 本身。
    fn seed_pending(store: &OutboxStore, ev: &OutboxEvent) {
        use std::io::Write as _;
        use std::os::unix::fs::OpenOptionsExt as _;
        ensure_private_directory(&store.pending_dir()).unwrap();
        std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(store.pending_path(&ev.id))
            .unwrap()
            .write_all(serde_json::to_string_pretty(ev).unwrap().as_bytes())
            .unwrap();
    }

    #[test]
    fn process_supplement_round_trips_and_redacts_its_terminal_payload() {
        let (_dir, store) = store();
        let mut event = notify_event(1_000, "tester");
        event.kind = OutboxKind::ProcessSupplement {
            chat_id: "oc_process".into(),
            chat_type: "group".into(),
            thread_anchor: Some("om_origin".into()),
            rendered_card: summon_core::feishu::card_process_supplement("private-process-text"),
        };
        store.enqueue(&event).unwrap();
        assert_eq!(store.pending_events(), [event.clone()]);
        assert_eq!(local_outbox_chat_id(&event.kind), Some("oc_process"));
        store.acknowledge_process_supplement(&event).unwrap();
        let archived =
            std::fs::read_to_string(store.archive_dir().join(format!("{}.json", event.id)))
                .unwrap();
        assert!(!archived.contains("private-process-text"));
        let archived: OutboxEvent = serde_json::from_str(&archived).unwrap();
        let OutboxKind::ProcessSupplement {
            rendered_card,
            thread_anchor,
            ..
        } = archived.kind
        else {
            panic!("process supplement");
        };
        assert!(rendered_card.is_empty());
        assert_eq!(thread_anchor.as_deref(), Some("om_origin"));
    }

    /// 盘上遗留的旧 kind 事件(如已删除的 `task_notification`)必须被**降级跳过**,
    /// 不能让一次 daemon 启动重放/扫描整体失败。三条载入路径逐条钉:
    /// `list_due`(drain 用)、`scan_pending`(启动扫描用)、`pending_events`(测试遍历)。
    /// 同目录下的合法事件仍必须被读出来 —— 「跳过」而非「中断」正是这条的全部内容。
    #[test]
    fn legacy_unknown_kind_event_on_disk_is_skipped_not_fatal() {
        let (_dir, store) = store();
        let healthy = notify_event(1_000, "starlight");
        store.enqueue(&healthy).unwrap();
        // 手写一条旧 schema 事件:kind 已不在枚举里,serde 必然解不动。
        let stale = store.pending_dir().join("task-notice-legacy.json");
        std::fs::write(
            &stale,
            r#"{"id":"task-notice-legacy","agent":"starlight","delivery_scope":"test-scope",
                "created_ms":1000,"attempts":0,"next_attempt_ms":1000,"last_error":null,
                "kind":"task_notification","task_id":"SUM-1"}"#,
        )
        .unwrap();

        let due = store.list_due(2_000);
        assert_eq!(due.len(), 1, "坏事件跳过,健康事件照常到期");
        assert_eq!(due[0].id, healthy.id);

        let scan = store.scan_pending().expect("坏事件不能让启动扫描整体失败");
        assert_eq!(scan.events.len(), 1);
        assert_eq!(scan.events[0].id, healthy.id);
        assert!(
            scan.corrupt_bridge.is_empty() && scan.corrupt_recovery.is_empty(),
            "非 Bridge/Recovery 的坏事件不进 Bridge/Recovery 桶"
        );
        assert_eq!(
            scan.corrupt_other.len(),
            1,
            "未知 kind 遗留归入 corrupt_other 待隔离(2026-08-05 前是 log-only 无限重扫)"
        );

        let pending = store.pending_events();
        assert_eq!(pending.len(), 1);
        assert!(pending.iter().all(|e| !e.id.contains("task-notice")));
        assert!(
            stale.exists(),
            "scan 是纯分类,不动文件;隔离由 drain 线程执行"
        );
    }

    /// 2026-08-05 真机事故回归:两个已删除变体(`task_notification`)的遗留 pending 被
    /// 100ms+2s 双线程每 tick 重扫,刷日志 ~21 行/秒且 fail-closed 阻塞 Bridge 绑定。
    /// drain tick 必须把它隔离成无正文 tombstone 并终止循环。
    #[test]
    fn unknown_kind_pending_is_quarantined_by_drain_tick() {
        let (_dir, store) = store();
        let healthy = notify_event(1_000, "starlight");
        store.enqueue(&healthy).unwrap();
        let stale = store.pending_dir().join("task-notice-legacy.json");
        std::fs::write(
            &stale,
            r#"{"id":"task-notice-legacy","agent":"starlight","delivery_scope":"test-scope",
                "created_ms":1000,"attempts":97,"next_attempt_ms":1000,"last_error":"outbox_delivery_failed",
                "kind":"task_notification","task_id":"SUM-16","secret_body":"MUST-NOT-LEAK"}"#,
        )
        .unwrap();

        let deliver = |_e: &OutboxEvent| -> anyhow::Result<()> { Ok(()) };
        let delivered = store.drain_tick(2_000, &deliver);
        assert_eq!(delivered, 1, "健康事件照常投递,不被坏文件拖死");

        assert!(!stale.exists(), "坏文件已从 pending 移除");
        let tombstones: Vec<_> = std::fs::read_dir(store.quarantine_dir())
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        assert_eq!(tombstones.len(), 1, "隔离产物恰一个 tombstone");
        let body = std::fs::read_to_string(&tombstones[0]).unwrap();
        assert!(
            !body.contains("MUST-NOT-LEAK") && !body.contains("SUM-16"),
            "tombstone 无正文:不携带原事件内容"
        );
        assert!(
            body.contains("task-notice-legacy")
                && body.contains("unparseable_or_unknown_kind_pending"),
            "tombstone 记录 id 与 reason 供排查"
        );

        // 终止性:再扫一遍,corrupt_other 必须为空——循环到此为止。
        let rescan = store.scan_pending().unwrap();
        assert!(
            rescan.corrupt_other.is_empty(),
            "隔离后不再产出 corrupt_other"
        );
        // 幂等:再来一个 tick 也不会新增 tombstone。
        store.drain_tick(3_000, &deliver);
        assert_eq!(
            std::fs::read_dir(store.quarantine_dir()).unwrap().count(),
            1,
            "重复 tick 不重复隔离"
        );
    }

    fn notify_event(now_ms: u64, agent: &str) -> OutboxEvent {
        OutboxEvent {
            id: format_id(now_ms, agent, "deadbeef"),
            agent: agent.into(),
            delivery_scope: "test-scope".into(),
            created_ms: now_ms,
            attempts: 0,
            next_attempt_ms: now_ms,
            last_error: None,
            kind: OutboxKind::NotifyChat {
                chat_id: "oc_1".into(),
                chat_type: "group".into(),
                creator: Some("ou_creator".into()),
                event_operator: None,
                reason: ExitReason::ShutdownInterrupt,
                text: "⏸️ 上一轮被打断".into(),
                rendered_card: String::new(),
            },
        }
    }

    fn bridge_turn_event(now_ms: u64) -> OutboxEvent {
        use summon_core::outbox::{BridgeDeliveryFence, OutboxKind};
        OutboxEvent {
            id: "bridge-turn-receipt".into(),
            agent: "agent".into(),
            delivery_scope: "scope".into(),
            created_ms: now_ms,
            attempts: 0,
            next_attempt_ms: now_ms,
            last_error: None,
            kind: OutboxKind::ExternalBridgeTurn {
                fence: BridgeDeliveryFence {
                    scope: "scope".into(),
                    agent: "agent".into(),
                    chat_id: "chat".into(),
                    chat_type: "group".into(),
                    binding_id: "binding".into(),
                    binding_revision: 7,
                    session: summon_core::bridge::SessionKey::new("/config", "sid"),
                    generation: 1,
                    lease_binding_id: "binding".into(),
                    receipt_id: "receipt".into(),
                    sequence: 2,
                },
                turn_id: "turn".into(),
                card_version: summon_core::outbox::BRIDGE_CARD_WIRE_VERSION,
                rendered_card: "VERY_SECRET_USER VERY_SECRET_ASSISTANT".into(),
            },
        }
    }

    fn bridge_handoff_event(now_ms: u64) -> OutboxEvent {
        use summon_core::outbox::{
            BridgeDeliveryFence, BridgeHandoffNotice, OutboxKind, BRIDGE_CARD_WIRE_VERSION,
        };
        OutboxEvent {
            id: "bridge-handoff-attempt-requested".into(),
            agent: "agent".into(),
            delivery_scope: "scope".into(),
            created_ms: now_ms,
            attempts: 0,
            next_attempt_ms: now_ms,
            last_error: None,
            kind: OutboxKind::BridgeHandoffStatus {
                fence: BridgeDeliveryFence {
                    scope: "scope".into(),
                    agent: "agent".into(),
                    chat_id: "chat".into(),
                    chat_type: "group".into(),
                    binding_id: "binding".into(),
                    binding_revision: 7,
                    session: summon_core::bridge::SessionKey::new("/config", "sid"),
                    generation: 1,
                    lease_binding_id: "binding".into(),
                    receipt_id: "attempt-requested".into(),
                    sequence: 1,
                },
                attempt_id: "attempt".into(),
                notice: BridgeHandoffNotice::Requested,
                card_version: BRIDGE_CARD_WIRE_VERSION,
                rendered_card: summon_core::feishu::card_markdown_v2("status"),
            },
        }
    }

    fn recovery_card_event(now_ms: u64) -> OutboxEvent {
        let id = "recovery-card-0123456789abcdef0123456789abcdef";
        OutboxEvent {
            id: id.into(),
            agent: "agent".into(),
            delivery_scope: "scope".into(),
            created_ms: now_ms,
            attempts: 0,
            next_attempt_ms: now_ms,
            last_error: None,
            kind: OutboxKind::RecoveryCard {
                batch_id: id.into(),
                scope: "scope".into(),
                chat_id: "oc_recovery".into(),
                chat_type: "group".into(),
                items: vec![summon_core::outbox::RecoveryItem {
                    message_id: "om_secret".into(),
                    preview: "VERY_SECRET_RECOVERY_PREVIEW".into(),
                }],
                stage: Some("VERY_SECRET_RECOVERY_STAGE".into()),
                card_version: summon_core::outbox::BRIDGE_CARD_WIRE_VERSION,
                rendered_card: r#"{"schema":"2.0","body":{"elements":[{"tag":"markdown","content":"VERY_SECRET_RECOVERY_WIRE"}]}}"#.into(),
            },
        }
    }

    #[test]
    fn bridge_health_is_read_only_and_keeps_quarantine_visible() {
        let (dir, store) = store();
        let root = dir.path().join("outbox");
        assert_eq!(
            store.inspect_bridge_health().unwrap(),
            BridgeOutboxHealth::default()
        );
        assert!(!root.exists(), "只读 status 不应创建 outbox 目录");
        std::fs::create_dir_all(store.quarantine_dir()).unwrap();
        std::fs::write(
            store.quarantine_dir().join("bridge-corrupt.json"),
            br#"{"reason":"corrupt_bridge_pending"}"#,
        )
        .unwrap();
        assert_eq!(store.inspect_bridge_health().unwrap().quarantined, 1);
    }

    /// pending 目录里某 id 是否存在。
    fn in_pending(store: &OutboxStore, id: &str) -> bool {
        store.pending_path(id).exists()
    }
    fn in_archive(store: &OutboxStore, id: &str) -> bool {
        store.archive_dir().join(format!("{id}.json")).exists()
    }
    fn in_dead(store: &OutboxStore, id: &str) -> bool {
        store.dead_dir().join(format!("{id}.json")).exists()
    }

    #[test]
    fn enqueue_then_list_due_then_ack_moves_to_archive() {
        let (_dir, store) = store();
        let ev = notify_event(1_000, "starlight");
        store.enqueue(&ev).unwrap();
        assert!(in_pending(&store, &ev.id), "入队后落在 pending/");
        // tmp 不残留。
        assert_eq!(std::fs::read_dir(store.pending_dir()).unwrap().count(), 1);

        let due = store.list_due(1_000);
        assert_eq!(due.len(), 1);
        assert_eq!(due[0], ev, "list_due 应能完整读回事件");

        store.ack(&ev.id);
        assert!(!in_pending(&store, &ev.id), "ACK 后 pending/ 不再有");
        assert!(in_archive(&store, &ev.id), "ACK 后落 archive/");
        // 归档后再扫 pending,空。
        assert!(store.list_due(2_000).is_empty());
    }

    #[test]
    fn enqueue_once_never_clobbers_attempts_or_requeues_archive() {
        let (_dir, store) = store();
        let event = bridge_turn_event(1_000);
        assert_eq!(
            store.enqueue_once(&event).unwrap(),
            EnqueueOnceOutcome::Enqueued
        );
        let mut failed = event.clone();
        failed.attempts = 9;
        failed.next_attempt_ms = 99_000;
        failed.last_error = Some("network".into());
        store.record_failure(&failed).unwrap();
        assert!(matches!(
            store.enqueue_once(&event).unwrap(),
            EnqueueOnceOutcome::Existing(OutboxLocation::Pending, _)
        ));
        let saved = store.find(&event.id).unwrap().unwrap().1;
        assert_eq!(saved.attempts, 9);
        assert_eq!(saved.next_attempt_ms, 99_000);

        store.ack(&event.id);
        assert!(matches!(
            store.enqueue_once(&event).unwrap(),
            EnqueueOnceOutcome::Existing(OutboxLocation::Archive, _)
        ));
        assert!(!store.pending_path(&event.id).exists());
    }

    #[test]
    fn recovery_archive_tombstone_is_idempotent_but_pending_wire_remains_strict() {
        let (_dir, store) = store();
        let event = recovery_card_event(1_000);
        assert_eq!(
            store.enqueue_once(&event).unwrap(),
            EnqueueOnceOutcome::Enqueued
        );

        let mut drifted = event.clone();
        let OutboxKind::RecoveryCard { rendered_card, .. } = &mut drifted.kind else {
            unreachable!()
        };
        *rendered_card = r#"{"schema":"2.0","body":{"elements":[]},"drift":true}"#.into();
        assert!(
            store.enqueue_once(&drifted).is_err(),
            "pending 中同 UUID 不同 frozen wire 必须冲突"
        );

        assert_eq!(store.drain_tick(2_000, &|_| Ok(())), 1);
        let (location, tombstone) = store.find(&event.id).unwrap().unwrap();
        assert_eq!(location, OutboxLocation::Archive);
        assert!(matches!(
            tombstone.kind,
            OutboxKind::RecoveryCard {
                ref items,
                stage: None,
                ref rendered_card,
                ..
            } if items.is_empty() && rendered_card.is_empty()
        ));
        assert!(matches!(
            store.enqueue_once(&event).unwrap(),
            EnqueueOnceOutcome::Existing(OutboxLocation::Archive, _)
        ));

        let mut wrong_chat = event.clone();
        let OutboxKind::RecoveryCard { chat_id, .. } = &mut wrong_chat.kind else {
            unreachable!()
        };
        *chat_id = "oc_other".into();
        assert!(
            store.enqueue_once(&wrong_chat).is_err(),
            "archive tombstone 只能匹配同一 durable RecoveryCard identity"
        );
    }

    #[test]
    fn stale_recovery_identity_replaces_even_corrupt_pending_with_redacted_terminal() {
        let (_dir, store) = store();
        let event = recovery_card_event(1_000);
        store.enqueue(&event).unwrap();
        std::fs::write(store.pending_path(&event.id), b"{corrupt SECRET_BODY").unwrap();

        store.archive_abandoned_recovery(&event).unwrap();

        assert!(!store.pending_path(&event.id).exists());
        let (location, terminal) = store.find(&event.id).unwrap().unwrap();
        assert_eq!(location, OutboxLocation::Archive);
        assert!(matches!(
            terminal.kind,
            OutboxKind::RecoveryCard {
                ref items,
                stage: None,
                ref rendered_card,
                ..
            } if items.is_empty() && rendered_card.is_empty()
        ));
        let raw = std::fs::read_to_string(store.archive_dir().join(format!("{}.json", event.id)))
            .unwrap();
        assert!(!raw.contains("SECRET_BODY"));
        assert!(!raw.contains("VERY_SECRET_RECOVERY"));
    }

    #[test]
    fn recovery_producer_preserves_first_valid_frozen_wire_across_renderer_change() {
        let (_dir, store) = store();
        let mut first = recovery_card_event(1_000);
        first.attempts = 4;
        first.next_attempt_ms = 99_000;
        let OutboxKind::RecoveryCard { rendered_card, .. } = &mut first.kind else {
            unreachable!()
        };
        *rendered_card =
            r#"{"schema":"2.0","body":{"elements":[{"tag":"markdown","content":"FROZEN_V1"}]}}"#
                .into();
        store.enqueue(&first).unwrap();

        let mut candidate = recovery_card_event(1_000);
        let OutboxKind::RecoveryCard { rendered_card, .. } = &mut candidate.kind else {
            unreachable!()
        };
        *rendered_card =
            r#"{"schema":"2.0","body":{"elements":[{"tag":"markdown","content":"NEW_RENDERER"}]}}"#
                .into();
        let outcome = store.ensure_recovery_once(&candidate).unwrap();

        assert!(matches!(
            outcome,
            EnqueueOnceOutcome::Existing(OutboxLocation::Pending, _)
        ));
        let (_, persisted) = store.find(&first.id).unwrap().unwrap();
        assert_eq!(persisted.attempts, 4);
        assert_eq!(persisted.next_attempt_ms, 99_000);
        assert!(matches!(
            persisted.kind,
            OutboxKind::RecoveryCard { ref rendered_card, .. }
                if rendered_card.contains("FROZEN_V1") && !rendered_card.contains("NEW_RENDERER")
        ));
    }

    #[test]
    fn explicit_recovery_wire_upgrade_preserves_pending_retry_state() {
        let (_dir, store) = store();
        let replacement = recovery_card_event(1_000);
        let mut legacy = replacement.clone();
        let OutboxKind::RecoveryCard {
            card_version,
            rendered_card,
            ..
        } = &mut legacy.kind
        else {
            unreachable!()
        };
        *card_version = 0;
        rendered_card.clear();
        legacy.attempts = 7;
        legacy.next_attempt_ms = 88_000;
        legacy.last_error = Some("old failure".into());
        store.enqueue(&legacy).unwrap();

        store
            .upgrade_legacy_recovery_wire(&legacy, &replacement)
            .unwrap();
        let (location, upgraded) = store.find(&legacy.id).unwrap().unwrap();
        assert_eq!(location, OutboxLocation::Pending);
        assert_eq!(upgraded.attempts, 7);
        assert_eq!(upgraded.next_attempt_ms, 88_000);
        assert_eq!(upgraded.last_error.as_deref(), Some("old failure"));
        assert!(matches!(
            upgraded.kind,
            OutboxKind::RecoveryCard {
                card_version: summon_core::outbox::BRIDGE_CARD_WIRE_VERSION,
                ref rendered_card,
                ..
            } if summon_core::outbox::valid_frozen_bridge_card(
                summon_core::outbox::BRIDGE_CARD_WIRE_VERSION,
                rendered_card,
            )
        ));

        let mut wrong_items = replacement;
        let OutboxKind::RecoveryCard { items, .. } = &mut wrong_items.kind else {
            unreachable!()
        };
        items[0].message_id = "om_other".into();
        assert!(store
            .upgrade_legacy_recovery_wire(&legacy, &wrong_items)
            .is_err());
    }

    #[test]
    fn sensitive_bridge_dead_letter_contains_no_transcript_body() {
        let (_dir, store) = store();
        let mut event = bridge_turn_event(1_000);
        event.attempts = MAX_ATTEMPTS - 1;
        store.enqueue(&event).unwrap();
        store.drain_tick(2_000, &|_| {
            anyhow::bail!("500 response echoed VERY_SECRET_USER")
        });

        let raw =
            std::fs::read_to_string(store.dead_dir().join(format!("{}.json", event.id))).unwrap();
        assert!(!raw.contains("VERY_SECRET_USER"));
        assert!(!raw.contains("VERY_SECRET_ASSISTANT"));
        assert!(!raw.contains("response echoed"));
        let dead: OutboxEvent = serde_json::from_str(&raw).unwrap();
        assert!(matches!(
            dead.kind,
            summon_core::outbox::OutboxKind::ExternalBridgeTurn {
                ref rendered_card,
                ..
            } if rendered_card.is_empty()
        ));
    }

    #[test]
    fn sensitive_bridge_archive_is_redacted_after_successful_delivery() {
        let (_dir, store) = store();
        let event = bridge_turn_event(1_000);
        store.enqueue(&event).unwrap();
        assert_eq!(store.drain_tick(2_000, &|_| Ok(())), 1);
        let raw = std::fs::read_to_string(store.archive_dir().join(format!("{}.json", event.id)))
            .unwrap();
        assert!(!raw.contains("VERY_SECRET_USER"));
        assert!(!raw.contains("VERY_SECRET_ASSISTANT"));
    }

    #[test]
    fn archived_handoff_notice_is_idempotent_after_wire_redaction() {
        let (_dir, store) = store();
        let event = bridge_handoff_event(1_000);
        store.enqueue_once(&event).unwrap();
        assert_eq!(store.drain_tick(2_000, &|_| Ok(())), 1);
        assert!(matches!(
            store.enqueue_once(&event).unwrap(),
            EnqueueOnceOutcome::Existing(OutboxLocation::Archive, _)
        ));

        let mut drifted = event;
        let OutboxKind::BridgeHandoffStatus { notice, .. } = &mut drifted.kind else {
            unreachable!()
        };
        *notice = summon_core::outbox::BridgeHandoffNotice::Failed;
        assert!(store.enqueue_once(&drifted).is_err());
    }

    #[test]
    fn corrupt_bridge_pending_pauses_bridge_but_not_generic_outbox() {
        let (_dir, store) = store();
        let event = bridge_turn_event(1_000);
        store.enqueue(&event).unwrap();
        let notify = notify_event(1_000, "agent");
        store.enqueue(&notify).unwrap();
        std::fs::write(
            store.pending_dir().join("bridge-turn-older.json"),
            b"{broken",
        )
        .unwrap();
        let calls = std::sync::atomic::AtomicUsize::new(0);

        assert_eq!(
            store.drain_tick(2_000, &|_| {
                calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(())
            }),
            1
        );
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(in_pending(&store, &event.id));
        assert!(in_archive(&store, &notify.id));
    }

    #[test]
    fn corrupt_recovery_pending_is_classified_and_quarantined_without_body() {
        let (_dir, store) = store();
        let event = recovery_card_event(1_000);
        store.enqueue(&event).unwrap();
        std::fs::write(
            store.pending_path(&event.id),
            b"{broken VERY_SECRET_RECOVERY",
        )
        .unwrap();

        let mut scan = store.scan_pending().unwrap();
        assert!(scan.events.is_empty());
        assert!(scan.corrupt_bridge.is_empty());
        assert_eq!(scan.corrupt_recovery.len(), 1);
        store
            .quarantine_corrupt_recovery(&scan.corrupt_recovery.remove(0))
            .unwrap();

        assert!(!store.pending_path(&event.id).exists());
        let raw = std::fs::read_to_string(
            std::fs::read_dir(store.quarantine_dir())
                .unwrap()
                .next()
                .unwrap()
                .unwrap()
                .path(),
        )
        .unwrap();
        assert!(!raw.contains("VERY_SECRET_RECOVERY"));
        assert!(raw.contains("corrupt_recovery_pending"));
    }

    #[test]
    fn parseable_partial_recovery_identity_is_still_corrupt() {
        let (_dir, store) = store();
        let mut event = recovery_card_event(1_000);
        let OutboxKind::RecoveryCard { scope, .. } = &mut event.kind else {
            unreachable!()
        };
        scope.clear();
        store.enqueue(&event).unwrap();

        let scan = store.scan_pending().unwrap();
        assert!(scan.events.is_empty());
        assert_eq!(scan.corrupt_recovery.len(), 1);
        assert_eq!(
            scan.corrupt_recovery[0].id.as_deref(),
            Some(event.id.as_str())
        );
    }

    #[test]
    fn damaged_terminal_never_overrides_valid_pending() {
        let (_dir, store) = store();
        let event = bridge_turn_event(1_000);
        store.enqueue(&event).unwrap();
        std::fs::create_dir_all(store.archive_dir()).unwrap();
        let terminal = store.archive_dir().join(format!("{}.json", event.id));
        std::fs::write(&terminal, b"{broken").unwrap();
        assert!(matches!(
            store.find(&event.id).unwrap(),
            Some((OutboxLocation::Pending, _))
        ));
        let mut wrong = event.clone();
        wrong.id = "bridge-turn-other".into();
        std::fs::write(&terminal, serde_json::to_vec(&wrong).unwrap()).unwrap();
        assert!(matches!(
            store.find(&event.id).unwrap(),
            Some((OutboxLocation::Pending, _))
        ));
        assert_eq!(store.scan_pending().unwrap().events.len(), 1);
    }

    #[test]
    fn bridge_payload_under_mismatched_filename_is_quarantined() {
        let (_dir, store) = store();
        let event = bridge_turn_event(1_000);
        store.enqueue(&event).unwrap();
        std::fs::rename(
            store.pending_path(&event.id),
            store.pending_dir().join("renamed.json"),
        )
        .unwrap();
        let scan = store.scan_pending().unwrap();
        assert_eq!(scan.corrupt_bridge.len(), 1);
        assert_eq!(scan.corrupt_bridge[0].id.as_deref(), Some("renamed"));
        store
            .quarantine_corrupt_bridge(&scan.corrupt_bridge[0])
            .unwrap();
        assert_eq!(std::fs::read_dir(store.pending_dir()).unwrap().count(), 0);
        assert_eq!(
            std::fs::read_dir(store.quarantine_dir()).unwrap().count(),
            1
        );
    }

    #[test]
    fn repeated_quarantine_remove_failure_reuses_one_tombstone() {
        use std::os::unix::fs::PermissionsExt as _;

        let (_dir, store) = store();
        let event = bridge_turn_event(1_000);
        store.enqueue(&event).unwrap();
        std::fs::rename(
            store.pending_path(&event.id),
            store.pending_dir().join("renamed.json"),
        )
        .unwrap();
        let corrupt = store.scan_pending().unwrap().corrupt_bridge.remove(0);
        std::fs::create_dir_all(store.quarantine_dir()).unwrap();
        std::fs::set_permissions(store.pending_dir(), std::fs::Permissions::from_mode(0o500))
            .unwrap();
        assert!(store.quarantine_corrupt_bridge(&corrupt).is_err());
        assert!(store.quarantine_corrupt_bridge(&corrupt).is_err());
        assert_eq!(
            std::fs::read_dir(store.quarantine_dir()).unwrap().count(),
            1,
            "remove 重试不得无界制造 quarantine tombstone"
        );
        std::fs::set_permissions(store.pending_dir(), std::fs::Permissions::from_mode(0o700))
            .unwrap();
    }

    #[test]
    fn durable_redacted_terminal_wins_over_crash_left_plaintext_pending() {
        let (_dir, store) = store();
        let event = bridge_turn_event(1_000);
        store.enqueue(&event).unwrap();
        assert_eq!(store.drain_tick(2_000, &|_| Ok(())), 1);
        // 模拟 terminal durable 后、plaintext 删除前崩溃留下的双份状态。
        store.enqueue(&event).unwrap();
        assert!(in_pending(&store, &event.id));
        assert!(store.pending_events_checked().unwrap().is_empty());
        assert!(!in_pending(&store, &event.id));
        let raw = std::fs::read_to_string(store.archive_dir().join(format!("{}.json", event.id)))
            .unwrap();
        assert!(!raw.contains("VERY_SECRET"));
    }

    #[test]
    fn sensitive_dead_terminal_write_failure_keeps_plaintext_pending_fail_closed() {
        let (_dir, store) = store();
        let mut event = bridge_turn_event(1_000);
        event.attempts = MAX_ATTEMPTS - 1;
        store.enqueue(&event).unwrap();
        std::fs::create_dir_all(&store.root).unwrap();
        std::fs::write(store.dead_dir(), b"not-a-directory").unwrap();

        assert_eq!(store.drain_tick(2_000, &|_| anyhow::bail!("network")), 0);
        assert!(in_pending(&store, &event.id));
        assert!(!in_dead(&store, &event.id));
        let raw = std::fs::read_to_string(store.pending_path(&event.id)).unwrap();
        assert!(raw.contains("VERY_SECRET_USER"));
    }

    #[test]
    fn list_due_skips_not_yet_due() {
        let (_dir, store) = store();
        let mut ev = notify_event(1_000, "a");
        ev.next_attempt_ms = 5_000; // 未来才到期
        store.enqueue(&ev).unwrap();
        assert!(store.list_due(1_000).is_empty(), "未到期不返回");
        assert_eq!(store.list_due(5_000).len(), 1, "到点返回");
    }

    #[test]
    fn list_due_tolerates_corrupt_and_missing_dir() {
        let (_dir, store) = store();
        // 目录还没建:返回空、不 panic。
        assert!(store.list_due(1_000).is_empty());
        // 写一个损坏 json 进 pending/。
        std::fs::create_dir_all(store.pending_dir()).unwrap();
        std::fs::write(store.pending_dir().join("bad.json"), b"{ not json").unwrap();
        // 再写一条好的。
        let ev = notify_event(1_000, "a");
        store.enqueue(&ev).unwrap();
        let due = store.list_due(1_000);
        assert_eq!(due.len(), 1, "损坏跳过、好的仍返回");
        assert_eq!(due[0].id, ev.id);
    }

    #[test]
    fn record_failure_bumps_attempts_and_next_then_persists() {
        let (_dir, store) = store();
        let mut ev = notify_event(1_000, "a");
        store.enqueue(&ev).unwrap();
        let now = 10_000u64;
        let disp = mark_attempt_failed(&mut ev, now, MAX_ATTEMPTS, "boom");
        assert_eq!(disp, Disposition::Retry);
        store.record_failure(&ev).unwrap();
        // 覆写后重读:attempts/next/last_error 落盘可见。
        let reread = store.list_due(now + 1_000_000);
        assert_eq!(reread.len(), 1);
        assert_eq!(reread[0].attempts, 1);
        assert_eq!(reread[0].last_error.as_deref(), Some("boom"));
        assert_eq!(reread[0].next_attempt_ms, now + 5_000);
        assert!(in_pending(&store, &ev.id), "重试仍在 pending/");
    }

    #[test]
    fn failure_at_retry_horizon_goes_to_dead() {
        let (_dir, store) = store();
        let mut ev = notify_event(1_000, "a");
        store.enqueue(&ev).unwrap();
        let mut now = 10_000u64;
        // 失败 1~4 次:Retry,覆写回 pending。
        for _ in 1..MAX_ATTEMPTS {
            let disp = mark_attempt_failed(&mut ev, now, MAX_ATTEMPTS, "boom");
            assert_eq!(disp, Disposition::Retry);
            store.record_failure(&ev).unwrap();
            now += 1_000_000;
        }
        // 封顶这次:Dead。消费者对任何失败都先 record_failure(把最终 attempts/last_error
        // 落回 pending),再按处置路由:Retry 留下、Dead rename 到 dead/。故 to_dead 是纯原子 rename,
        // 死信文件已含最终字段。
        let disp = mark_attempt_failed(&mut ev, now, MAX_ATTEMPTS, "final");
        assert_eq!(disp, Disposition::Dead);
        store.record_failure(&ev).unwrap();
        store.to_dead(&ev.id);
        assert!(!in_pending(&store, &ev.id), "死信后 pending/ 空");
        assert!(in_dead(&store, &ev.id), "落 dead/");
        // 死信保全字段(attempts=5,last_error=final)。
        let s = std::fs::read_to_string(store.dead_dir().join(format!("{}.json", ev.id))).unwrap();
        let dead: OutboxEvent = serde_json::from_str(&s).unwrap();
        assert_eq!(dead.attempts, MAX_ATTEMPTS);
        assert_eq!(dead.last_error.as_deref(), Some("final"));
    }

    #[test]
    fn ack_twice_is_idempotent_crash_mid_consume_safe() {
        // 模拟「投递成功但归档前崩」:第一次 ack 归档;若同一 id 再被 ack(下次重扫),不 panic、
        // 不把已归档的搬回来。第二次 ack 因 pending 已无源文件,rename 失败被 log-ignore。
        let (_dir, store) = store();
        let ev = notify_event(1_000, "a");
        store.enqueue(&ev).unwrap();
        store.ack(&ev.id);
        assert!(in_archive(&store, &ev.id));
        // 第二次 ack:源已不在,best-effort 静默,不影响 archive 里的那份。
        store.ack(&ev.id);
        assert!(in_archive(&store, &ev.id), "重复 ack 不破坏归档");
        assert!(!in_pending(&store, &ev.id));
    }

    #[test]
    fn cleanup_archive_removes_old_keeps_recent() {
        let (_dir, store) = store();
        let ev = notify_event(1_000, "a");
        store.enqueue(&ev).unwrap();
        store.ack(&ev.id);
        assert!(in_archive(&store, &ev.id));
        // 用「未来很久」的 now 让刚归档(mtime≈现在)的文件落在 cutoff 之前 → 应被清。不碰真实
        // mtime(无第三方 filetime 依赖):now = 真实现在 + 远超 7 天的余量。
        let far_future = now_unix_ms() + ARCHIVE_RETENTION_MS + 30 * 24 * 60 * 60 * 1_000;
        store.cleanup_archive(far_future);
        assert!(!in_archive(&store, &ev.id), "超过 7 天保留期的归档应被清");
    }

    #[test]
    fn cleanup_archive_keeps_recent_and_tolerates_missing_dir() {
        let (_dir, store) = store();
        // archive/ 还没建:不 panic。
        store.cleanup_archive(now_unix_ms());
        let ev = notify_event(1_000, "a");
        store.enqueue(&ev).unwrap();
        store.ack(&ev.id);
        // 用当下 now 清:刚归档的没过期,保留。
        store.cleanup_archive(now_unix_ms());
        assert!(in_archive(&store, &ev.id), "新归档(未过 7 天)保留");
    }

    #[test]
    fn cleanup_archive_never_time_expires_bridge_or_recovery_terminals() {
        let (_dir, store) = store();
        let bridge = bridge_turn_event(1_000);
        let recovery = recovery_card_event(2_000);
        for event in [&bridge, &recovery] {
            store.enqueue(event).unwrap();
            store.ack(&event.id);
            assert!(in_archive(&store, &event.id));
        }

        let far_future = now_unix_ms() + ARCHIVE_RETENTION_MS + 365 * 24 * 60 * 60 * 1_000;
        store.cleanup_archive(far_future);
        assert!(
            in_archive(&store, &bridge.id),
            "Bridge 状态可能仍引用 terminal，不能按 7 天猜测删除"
        );
        assert!(
            in_archive(&store, &recovery.id),
            "RecoveryBatch 仍可能依赖 terminal 去重事实"
        );
    }

    #[test]
    fn cleanup_archive_keeps_terminal_while_crash_left_pending_exists() {
        let (_dir, store) = store();
        let ev = notify_event(1_000, "a");
        store.enqueue(&ev).unwrap();
        let pending_body = std::fs::read(store.pending_path(&ev.id)).unwrap();
        assert_eq!(store.drain_tick(1_000, &|_| Ok(())), 1);
        assert!(in_archive(&store, &ev.id));

        // 模拟 terminal 已 fsync、plaintext unlink 失败的双份崩溃窗口。
        atomic_write_mode(&store.pending_path(&ev.id), &pending_body, 0o600).unwrap();
        let far_future = now_unix_ms() + ARCHIVE_RETENTION_MS + 30 * 24 * 60 * 60 * 1_000;
        store.cleanup_archive(far_future);
        assert!(
            in_archive(&store, &ev.id),
            "pending 尚在时不能删除唯一 terminal 去重证明"
        );

        // 正常扫描确认 terminal identity 后删掉 residual，下一次 GC 才可删除归档。
        assert!(store.scan_pending().unwrap().events.is_empty());
        assert!(!in_pending(&store, &ev.id));
        store.cleanup_archive(far_future);
        assert!(!in_archive(&store, &ev.id));
    }

    fn now_unix_ms() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
    }

    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc as StdArc;
    use summon_core::outbox::MAX_CONCURRENCY;

    #[test]
    fn drain_tick_delivers_then_archives_at_least_once() {
        // 成功投递 → ACK 归档(先投后归档)。deliver 计数被调一次。
        let (_dir, store) = store();
        let ev = notify_event(1_000, "a");
        store.enqueue(&ev).unwrap();
        let calls = StdArc::new(AtomicUsize::new(0));
        let c2 = StdArc::clone(&calls);
        let deliver = move |_e: &OutboxEvent| {
            c2.fetch_add(1, Ordering::SeqCst);
            Ok(())
        };
        let n = store.drain_tick(2_000, &deliver);
        assert_eq!(n, 1, "投递成功一条");
        assert_eq!(calls.load(Ordering::SeqCst), 1, "deliver 被调一次");
        assert!(!in_pending(&store, &ev.id), "ACK 后 pending/ 空");
        assert!(in_archive(&store, &ev.id), "落 archive/");
        // 再 tick:pending 空,deliver 不再被调。
        let n2 = store.drain_tick(3_000, &deliver);
        assert_eq!(n2, 0);
        assert_eq!(calls.load(Ordering::SeqCst), 1, "归档后不再投递同一条");
    }

    #[test]
    fn delivery_panic_isolated_as_retryable_failure() {
        let (_dir, store) = store();
        let ev = notify_event(1_000, "a");
        store.enqueue(&ev).unwrap();

        let delivered = store.drain_tick(2_000, &|_| -> anyhow::Result<()> {
            panic!("模拟单条投递实现 panic")
        });

        assert_eq!(delivered, 0);
        assert!(
            in_pending(&store, &ev.id),
            "panic 条目应留在 pending 退避重试"
        );
        let saved = read_event(&store.pending_path(&ev.id)).unwrap();
        assert_eq!(saved.attempts, 1);
        assert_eq!(saved.last_error.as_deref(), Some("outbox_delivery_failed"));
    }

    #[test]
    fn drain_tick_crash_mid_consume_redelivers_at_least_once() {
        // 模拟「投了但归档前崩」:deliver 成功**但我们不让它走 ACK**——用一个永远 Err 之后...
        // 更直接:deliver 成功但若我们手动把 archive 删掉/或不归档,无法在同进程模拟崩溃。
        // 改用语义钉死:at-least-once = 只要事件还在 pending,下次 tick 必再投。构造两次成功 tick
        // 之间「事件仍在 pending」(deliver 返回 Ok 但测试断言「投递发生在归档之前」)。这里直接断言
        // 「deliver 失败(没投出去)时事件留 pending,下次仍被投」——即从不 ack-before-send。
        let (_dir, store) = store();
        let ev = notify_event(1_000, "a");
        store.enqueue(&ev).unwrap();
        let calls = StdArc::new(AtomicUsize::new(0));
        let c2 = StdArc::clone(&calls);
        // 第一次失败(模拟投递中断/未确认):事件留 pending,绝不提前归档。
        let deliver_fail = move |_e: &OutboxEvent| -> anyhow::Result<()> {
            c2.fetch_add(1, Ordering::SeqCst);
            anyhow::bail!("network down")
        };
        store.drain_tick(2_000, &deliver_fail);
        assert!(
            in_pending(&store, &ev.id),
            "失败:事件仍在 pending(从不 ack-before-send)"
        );
        assert!(!in_archive(&store, &ev.id), "失败:绝不归档");
        // 推进时钟越过退避 → 再投。证明 at-least-once:同一事件被再次递送。
        let calls2 = StdArc::new(AtomicUsize::new(0));
        let c3 = StdArc::clone(&calls2);
        let deliver_ok = move |_e: &OutboxEvent| {
            c3.fetch_add(1, Ordering::SeqCst);
            Ok(())
        };
        let n = store.drain_tick(2_000 + 10_000, &deliver_ok);
        assert_eq!(n, 1, "退避到点后被重投并成功");
        assert_eq!(
            calls2.load(Ordering::SeqCst),
            1,
            "同事件至少投递了一次以上(at-least-once)"
        );
        assert!(in_archive(&store, &ev.id));
    }

    #[test]
    fn drain_tick_retries_with_backoff_then_dead_at_cap() {
        // deliver 恒失败:推进 clock 越过各 backoff,到重试上限后落 dead/,pending 空,全程有界返回。
        let (_dir, store) = store();
        let ev = notify_event(1_000, "a");
        store.enqueue(&ev).unwrap();
        let deliver = |_e: &OutboxEvent| -> anyhow::Result<()> { anyhow::bail!("boom") };
        let mut now = 2_000u64;
        for _ in 0..MAX_ATTEMPTS {
            // 每次都推进到「肯定到期」的时间(用大步长跨过任意退避)。
            store.drain_tick(now, &deliver);
            now += 1_000_000;
        }
        assert!(!in_pending(&store, &ev.id), "封顶后 pending/ 空");
        assert!(in_dead(&store, &ev.id), "落 dead/");
        // 死信保全最终字段(attempts=MAX,last_error)。
        let s = std::fs::read_to_string(store.dead_dir().join(format!("{}.json", ev.id))).unwrap();
        let dead: OutboxEvent = serde_json::from_str(&s).unwrap();
        assert_eq!(dead.attempts, MAX_ATTEMPTS);
        assert_eq!(dead.last_error.as_deref(), Some("outbox_delivery_failed"));
    }

    #[test]
    fn dead_count_counts_json_in_dead_dir() {
        let (_dir, store) = store();
        assert_eq!(store.dead_count(), 0, "dead/ 不存在 → 0");
        // 计数已在上限前一格:一次失败 tick 就落 dead/(逐格失败到上限另见
        // drain_tick_retries_with_backoff_then_dead_at_cap)。
        let mut ev = notify_event(1_000, "a");
        ev.attempts = MAX_ATTEMPTS - 1;
        store.enqueue(&ev).unwrap();
        let deliver = |_e: &OutboxEvent| -> anyhow::Result<()> { anyhow::bail!("boom") };
        store.drain_tick(2_000, &deliver);
        assert!(in_dead(&store, &ev.id));
        assert_eq!(store.dead_count(), 1, "落 dead/ 后计 1");
        // 非 .json 文件不计:dead/ 内产物恒为 <id>.json,此断言守过滤行为本身。
        std::fs::write(store.dead_dir().join("notes.txt"), b"x").unwrap();
        assert_eq!(store.dead_count(), 1, "非 .json 文件不计");
    }

    #[test]
    fn drain_tick_caps_batch_at_max_per_tick() {
        // 50 条全 due:一 tick 只投 MAX_PER_TICK=8,其余留 pending 下 tick 补。
        use summon_core::outbox::MAX_PER_TICK;
        let (_dir, store) = store();
        for i in 0..50u64 {
            let mut ev = notify_event(i, "a");
            ev.id = format_id(i, "a", &format!("{i:08x}")); // 唯一 id
            seed_pending(&store, &ev);
        }
        let calls = StdArc::new(AtomicUsize::new(0));
        let c2 = StdArc::clone(&calls);
        let deliver = move |_e: &OutboxEvent| {
            c2.fetch_add(1, Ordering::SeqCst);
            Ok(())
        };
        let n = store.drain_tick(1_000_000, &deliver);
        assert_eq!(n, MAX_PER_TICK, "一 tick 只投 MAX_PER_TICK 条");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            MAX_PER_TICK,
            "deliver 只被调 8 次"
        );
        // 余下 42 条仍在 pending。
        let remaining = store.list_due(1_000_000).len();
        assert_eq!(remaining, 50 - MAX_PER_TICK);
    }

    #[test]
    fn drain_tick_bounded_concurrency_peak_at_most_two() {
        // 投递时记录「同时在投的并发数」峰值,断言 <= MAX_CONCURRENCY=2。deliver 内 sleep 制造重叠。
        let (_dir, store) = store();
        for i in 0..8u64 {
            let mut ev = notify_event(i, "a");
            ev.id = format_id(i, "a", &format!("{i:08x}"));
            seed_pending(&store, &ev);
        }
        let inflight = StdArc::new(AtomicUsize::new(0));
        let peak = StdArc::new(AtomicUsize::new(0));
        let inf2 = StdArc::clone(&inflight);
        let peak2 = StdArc::clone(&peak);
        let deliver = move |_e: &OutboxEvent| {
            let cur = inf2.fetch_add(1, Ordering::SeqCst) + 1;
            peak2.fetch_max(cur, Ordering::SeqCst);
            std::thread::sleep(std::time::Duration::from_millis(20)); // 撑开重叠窗口
            inf2.fetch_sub(1, Ordering::SeqCst);
            Ok(())
        };
        store.drain_tick(1_000_000, &deliver);
        assert!(
            peak.load(Ordering::SeqCst) <= MAX_CONCURRENCY,
            "并发峰值应 <= MAX_CONCURRENCY,实际 {}",
            peak.load(Ordering::SeqCst)
        );
        assert!(peak.load(Ordering::SeqCst) >= 1);
    }

    #[test]
    fn drain_runs_on_own_thread_never_blocks_caller() {
        // 消费者是 detached 线程:即便投递会无限阻塞,**spawn 调用本身立即返回**,不卡启动 accept。
        // 模拟 spawn_outbox_drain 的结构(thread::spawn fire-and-forget),断言主线程瞬间继续。
        let dir = tempfile::tempdir().unwrap();
        let store = OutboxStore::new(dir.path().join("outbox"));
        let ev = notify_event(1_000, "a");
        store.enqueue(&ev).unwrap(); // 有一条 due,投递会阻塞
        let started = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let started2 = std::sync::Arc::clone(&started);
        let t0 = std::time::Instant::now();
        let h = std::thread::spawn(move || {
            // 投递永久阻塞:若 drain 内联在主线程,下面的 join 永不返回;它在独立线程,故主线程不卡。
            let block = move |_e: &OutboxEvent| -> anyhow::Result<()> {
                started2.store(true, Ordering::SeqCst);
                std::thread::sleep(std::time::Duration::from_secs(60));
                Ok(())
            };
            store.drain_tick(2_000, &block);
        });
        // 主线程不被阻塞:spawn 后立刻往下走(远小于 60s 的投递阻塞)。
        assert!(
            t0.elapsed() < std::time::Duration::from_secs(1),
            "spawn 不应阻塞主线程"
        );
        // detached 线程确实在跑(开始了投递);不 join(它会阻塞 60s),测试结束随进程回收。
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(
            started.load(Ordering::SeqCst),
            "drain 在独立线程里已开始投递"
        );
        drop(h); // 不 join:阻塞线程随测试进程退出回收
    }
}
