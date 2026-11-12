//! 定时器 IO：启用与归档分文件存储，共用状态锁与可恢复事务；flock `.counter` 分配 ID。
//! 纯字符串/补火决策在 summon-core::schedule;这里只读写文件。

use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, Write};
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Local};
use summon_core::{due_plan, FirePlan, Schedule};

use crate::runtime::fsio::{atomic_write, atomic_write_mode};

pub struct ScheduleStore {
    root: PathBuf, // 默认 ~/.summon
    /// Schedule 层只验证“保存失败时不提交状态”；原子写自身的临时文件/rename 细节由
    /// runtime::fsio 的测试负责。一次性 failpoint 避免测试绑定 tmp 命名实现。
    #[cfg(test)]
    fail_next_save: AtomicBool,
    #[cfg(test)]
    fail_after_write: AtomicU8,
}

#[derive(Debug, Default)]
pub struct SchedulePatch {
    pub rhythm: Option<(summon_core::Kind, String)>,
    pub prompt: Option<String>,
    pub output: Option<summon_core::ScheduleOutput>,
    pub context: Option<summon_core::ScheduleContext>,
    pub enabled: Option<bool>,
}

pub enum DueClaim<T> {
    Stale,
    Accepted {
        schedule: Box<Schedule>,
        plan: FirePlan,
        value: T,
    },
}

impl SchedulePatch {
    fn is_empty(&self) -> bool {
        self.rhythm.is_none()
            && self.prompt.is_none()
            && self.output.is_none()
            && self.context.is_none()
            && self.enabled.is_none()
    }
}

impl ScheduleStore {
    pub fn new(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
            #[cfg(test)]
            fail_next_save: AtomicBool::new(false),
            #[cfg(test)]
            fail_after_write: AtomicU8::new(0),
        }
    }

    pub fn at_summon_home() -> Self {
        Self::new(&crate::runtime::paths::summon_home())
    }

    fn yaml_path(&self) -> PathBuf {
        self.root.join("schedules.yaml")
    }

    fn archive_path(&self) -> PathBuf {
        self.root.join("schedules-archive.yaml")
    }

    fn pending_path(&self) -> PathBuf {
        self.root.join("schedules").join(".pending.yaml")
    }

    fn counter_path(&self) -> PathBuf {
        self.root.join("schedules").join(".counter")
    }

    fn state_lock_path(&self) -> PathBuf {
        self.root.join("schedules").join(".state.lock")
    }

    fn with_state_lock<T>(&self, f: impl FnOnce() -> Result<T>) -> Result<T> {
        let path = self.state_lock_path();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| summon_i18n::format!("r72ea1f78da"))?;
        }
        let lock = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&path)
            .with_context(|| summon_i18n::format!("raf90dba99d"))?;
        lock.lock().with_context(|| format!("flock {path:?}"))?;
        f()
    }

    /// flock 下分配下一个 `SCH-<n>`(参 task_store)。
    pub fn next_id(&self) -> Result<String> {
        self.with_state_lock(|| {
            let c = self.counter_path();
            let mut counter = OpenOptions::new()
                .create(true)
                .read(true)
                .write(true)
                .truncate(false)
                .open(&c)
                .with_context(|| summon_i18n::format!("r140910d249"))?;
            // 兼容仍只认 .counter flock 的旧进程；新进程还持有覆盖两份定时器文件 RMW 的 state lock。
            counter.lock().context("flock schedules/.counter")?;
            let mut raw = Vec::new();
            counter
                .read_to_end(&mut raw)
                .context(summon_i18n::text!("rdc9de0c91f"))?;
            let recorded = std::str::from_utf8(&raw)
                .ok()
                .and_then(|s| s.trim().parse::<u64>().ok())
                .unwrap_or(0);
            let existing = self
                .load_unlocked()?
                .iter()
                .filter_map(|s| s.id.strip_prefix("SCH-")?.parse::<u64>().ok())
                .max()
                .unwrap_or(0);
            let n = recorded
                .max(existing)
                .checked_add(1)
                .context(summon_i18n::text!("rd2c75c5821"))?;
            counter
                .seek(std::io::SeekFrom::Start(0))
                .context(summon_i18n::text!("r552dcc4e83"))?;
            counter
                .set_len(0)
                .context(summon_i18n::text!("r0dd6b6199d"))?;
            counter
                .write_all(n.to_string().as_bytes())
                .context(summon_i18n::text!("rd41b71c363"))?;
            counter.sync_all().context("sync schedules/.counter")?;
            Ok(format!("SCH-{n}"))
        })
    }

    /// 读取启用与归档的完整快照，并在状态锁内完成事务恢复与按启停状态迁移。
    /// 损坏文件必须向上传错，禁止把解析失败当空列表后覆盖原文件。
    pub fn load(&self) -> Result<Vec<Schedule>> {
        self.with_state_lock(|| self.load_unlocked())
    }

    /// 持锁复验此 agent 的定时器；文件清理与共享索引提交使用同一快照。
    pub(crate) fn remove_agent_with<T>(
        &self,
        alias: &str,
        expected: &[Schedule],
        action: impl FnOnce(&dyn Fn() -> Result<()>) -> Result<T>,
    ) -> Result<T> {
        self.with_state_lock(|| {
            let all = self.load_unlocked()?;
            let selected: Vec<_> = all.iter().filter(|s| s.agent == alias).cloned().collect();
            anyhow::ensure!(selected == expected, summon_i18n::format!("ra196e81e45"));
            anyhow::ensure!(
                selected.iter().all(|s| !s.enabled),
                summon_i18n::format!("r4ca4101a88")
            );
            let keep: Vec<_> = all.iter().filter(|s| s.agent != alias).cloned().collect();
            action(&|| {
                if selected.is_empty() {
                    return Ok(());
                }
                if let Err(error) = self.save_unlocked(&keep) {
                    self.save_unlocked(&all)
                        .context(summon_i18n::text!("rc7f203e4e1"))?;
                    return Err(error);
                }
                Ok(())
            })
        })
    }

    fn read_yaml(path: &Path) -> Result<Option<Vec<Schedule>>> {
        let yaml = match fs::read_to_string(path) {
            Ok(yaml) => yaml,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).with_context(|| summon_i18n::format!("rf45ace8440")),
        };
        serde_yaml::from_str(&yaml)
            .with_context(|| summon_i18n::format!("rb63e6009af"))
            .map(Some)
    }

    fn validate_ids(all: &[Schedule]) -> Result<()> {
        let mut ids = HashSet::new();
        for schedule in all {
            if !ids.insert(&schedule.id) {
                bail!(summon_i18n::format!("ra5376c7005", schedule.id));
            }
        }
        Ok(())
    }

    fn load_unlocked(&self) -> Result<Vec<Schedule>> {
        if let Some(pending) = Self::read_yaml(&self.pending_path())? {
            Self::validate_ids(&pending)?;
            self.finish_save_unlocked(&pending)
                .context(summon_i18n::text!("r153eb8c9b3"))?;
        }
        let mut active = Self::read_yaml(&self.yaml_path())?.unwrap_or_default();
        let archived = Self::read_yaml(&self.archive_path())?.unwrap_or_default();
        let misplaced = active.iter().any(|s| !s.enabled) || archived.iter().any(|s| s.enabled);
        active.extend(archived);
        Self::validate_ids(&active)?;
        if misplaced {
            active.sort_by_key(|s| !s.enabled);
            self.save_unlocked(&active)?;
        }
        Ok(active)
    }

    fn save_unlocked(&self, all: &[Schedule]) -> Result<()> {
        Self::validate_ids(all)?;
        let yaml = serde_yaml::to_string(all).context(summon_i18n::text!("raaf79ad379"))?;
        #[cfg(test)]
        if self.fail_next_save.swap(false, Ordering::SeqCst) {
            bail!(summon_i18n::format!("r4be02b1a9b"));
        }
        // 两次 rename 不能一起原子提交。先持久化完整目标快照，持锁读者先重放它再读两份文件。
        // 事务落盘后即确定目标状态，后续 IO 失败保留事务，供下次读取继续完成。
        atomic_write_mode(&self.pending_path(), yaml.as_bytes(), 0o600)
            .context(summon_i18n::text!("r5f1673a430"))?;
        self.finish_save_unlocked(all)
            .context(summon_i18n::text!("r79f1f73d49"))
    }

    fn finish_save_unlocked(&self, all: &[Schedule]) -> Result<()> {
        let active: Vec<_> = all.iter().filter(|s| s.enabled).collect();
        let archived: Vec<_> = all.iter().filter(|s| !s.enabled).collect();
        for (index, (path, schedules)) in
            [(self.yaml_path(), active), (self.archive_path(), archived)]
                .into_iter()
                .enumerate()
        {
            let yaml =
                serde_yaml::to_string(&schedules).context(summon_i18n::text!("rb5b8f68011"))?;
            atomic_write(&path, yaml.as_bytes())
                .with_context(|| summon_i18n::format!("reda6565c38"))?;
            #[cfg(test)]
            if self
                .fail_after_write
                .compare_exchange((index + 1) as u8, 0, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                bail!(summon_i18n::format!("r29ff44d0ae", index + 1));
            }
            #[cfg(not(test))]
            let _ = index;
        }
        let pending = self.pending_path();
        fs::remove_file(&pending).with_context(|| summon_i18n::format!("rc8aa1b81c8"))?;
        // 与原子写相同：目录 fsync 增强耐久性，不把已经提交的状态误报为失败。
        let _ = File::open(
            pending
                .parent()
                .context(summon_i18n::text!("r4dc706ea9c"))?,
        )
        .and_then(|dir| dir.sync_all());
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn fail_next_save_for_test(&self) {
        self.fail_next_save.store(true, Ordering::SeqCst);
    }

    pub fn add(&self, s: &Schedule) -> Result<()> {
        self.with_state_lock(|| {
            let mut all = self.load_unlocked()?;
            if all.iter().any(|old| old.id == s.id) {
                bail!(summon_i18n::format!("r9092b81458", s.id));
            }
            all.push(s.clone());
            self.save_unlocked(&all)
        })
    }

    pub fn set_enabled(&self, id: &str, enabled: bool) -> Result<()> {
        self.mutate(id, |s| s.enabled = enabled)
    }

    /// 在 schedule 状态锁内复验完整快照，并执行一次仅含本地持久化的接受动作。
    /// 接受动作成功返回就是 fire 的线性化点；外部 dispatch 必须在锁外完成。
    pub fn claim_due<T>(
        &self,
        expected: &Schedule,
        now: DateTime<Local>,
        accept: impl FnOnce(&Schedule, &FirePlan) -> Result<T>,
    ) -> Result<DueClaim<T>> {
        self.with_state_lock(|| {
            let all = self.load_unlocked()?;
            let Some(current) = all.iter().find(|schedule| schedule.id == expected.id) else {
                return Ok(DueClaim::Stale);
            };
            if current != expected {
                return Ok(DueClaim::Stale);
            }
            let plan = due_plan(current, now);
            if !plan.fire_now {
                return Ok(DueClaim::Stale);
            }
            let value = accept(current, &plan)?;
            Ok(DueClaim::Accepted {
                schedule: Box::new(current.clone()),
                plan,
                value,
            })
        })
    }

    /// 只给仍与已接受快照相同的记录写 checkpoint。接受之后发生的 edit 是更新的一代，不能被
    /// 旧 fire 的收尾覆盖。返回 false 表示新一代已经接管，无需 checkpoint。
    pub fn record_fire_if_unchanged(
        &self,
        expected: &Schedule,
        rfc3339: &str,
        disable: bool,
    ) -> Result<bool> {
        self.with_state_lock(|| {
            let mut all = self.load_unlocked()?;
            let schedule = all
                .iter_mut()
                .find(|schedule| schedule.id == expected.id)
                .with_context(|| summon_i18n::format!("rbc6aa13cfe", expected.id))?;
            if schedule != expected {
                return Ok(false);
            }
            schedule.last_fired = Some(rfc3339.to_string());
            if disable {
                schedule.enabled = false;
            }
            self.save_unlocked(&all)?;
            Ok(true)
        })
    }

    /// 原子应用部分更新。返回更新后的完整记录与「节律是否变化」。
    pub fn edit(
        &self,
        id: &str,
        patch: SchedulePatch,
        now_rfc3339: &str,
    ) -> Result<(Schedule, bool)> {
        if patch.is_empty() {
            bail!(summon_i18n::format!("r6f97b78a3a"));
        }
        self.with_state_lock(|| {
            let mut all = self.load_unlocked()?;
            let schedule = all
                .iter_mut()
                .find(|schedule| schedule.id == id)
                .with_context(|| summon_i18n::format!("rca76a985fd"))?;
            let before = schedule.clone();
            let rescheduled = patch
                .rhythm
                .as_ref()
                .is_some_and(|(kind, spec)| schedule.kind != *kind || schedule.spec != *spec);
            if let Some((kind, spec)) = patch.rhythm {
                schedule.kind = kind;
                schedule.spec = spec;
            }
            if let Some(prompt) = patch.prompt {
                schedule.prompt = prompt;
            }
            if let Some(output) = patch.output {
                schedule.output = output;
            }
            if let Some(context) = patch.context {
                schedule.context = context;
            }
            if let Some(enabled) = patch.enabled {
                schedule.enabled = enabled;
            }
            // 修订号只随触发内容变化:单纯停用 / 启用不算,否则接受之后、checkpoint 之前的一次
            // 停用再启用会让同一次触发换个编号再投一遍。
            if rescheduled
                || schedule.prompt != before.prompt
                || schedule.output != before.output
                || schedule.context != before.context
            {
                schedule.revision = schedule.revision.saturating_add(1);
            }
            if rescheduled {
                schedule.last_fired = match schedule.kind {
                    summon_core::Kind::Once => None,
                    summon_core::Kind::Cron | summon_core::Kind::Interval => {
                        Some(now_rfc3339.to_string())
                    }
                };
            }
            let updated = schedule.clone();
            self.save_unlocked(&all)?;
            Ok((updated, rescheduled))
        })
    }

    /// 改一条定时器的归属 agent(`schedules mv`)。chat 不动 —— 换执行者但留在原群,
    /// 是「这活以后交给谁跑」的语义;换群是另一件事,不在这条命令里。
    ///
    /// **目标 agent 必须存在**,校验在调用方(store 只认字段,不认 registry)。
    /// 漏了这层校验后果是静默的:scheduler 查不到 agent 的 delivery scope 时**跳过这个 tick
    /// 而不报错**(见 `daemon::scheduler::DeliveryScopes`),定时器会变成不响也不报的哑弹。
    pub fn set_agent(&self, id: &str, agent: &str) -> Result<()> {
        self.mutate(id, |s| s.agent = agent.to_string())
    }

    /// 覆盖一条定时器的节律与正文(卡片「保存」)。返回**节律是否被改动**,供回执措辞。
    /// 归属校验必须在调用前完成——store 只认 id,不知道谁有权改哪条。
    ///
    /// 改了节律就重排 `last_fired`,但**周期与一次性的重排方式相反**:
    ///
    /// - `Cron`/`Interval`:置 `now`。保持原值会让「2h 改 30m」立刻补触发一次(`base = last_fired`);
    ///   置 `None` 会让 cron 从 `created_at` 起算、`next_after` 早已过期,同样立刻补触发。
    ///   置 `now` = 从此刻起算下一个槽,即用户直觉里的「改完从下个周期开始生效」。
    /// - `Once`:置 `None`。**它的 `last_fired` 根本不是时间基准,而是「已经跑过了」的哨兵**
    ///   ——`due_plan` 的 Once 分支要求 `last.is_none()` 才触发。给它盖上时间戳等于把这条定时器
    ///   变成永不触发的哑弹,而回执还说着「已更新」。Once 的 spec 本身是绝对时刻,`to_spec` 也已
    ///   保证它在将来,不存在补触发风险,置 `None` 才是对的。
    ///
    /// 这条不对称是本函数存在的全部理由,改动前先看 `due_plan`。
    pub fn update_rhythm(
        &self,
        id: &str,
        kind: summon_core::Kind,
        spec: &str,
        prompt: &str,
        output: summon_core::ScheduleOutput,
        now_rfc3339: &str,
    ) -> Result<bool> {
        self.edit(
            id,
            SchedulePatch {
                rhythm: Some((kind, spec.to_string())),
                prompt: Some(prompt.to_string()),
                output: Some(output),
                context: None,
                enabled: None,
            },
            now_rfc3339,
        )
        .map(|(_, rescheduled)| rescheduled)
    }

    /// 成功路由一次 fire 后，一次原子 RMW 同时 checkpoint 时间与 once disable，避免两次写之间崩溃。
    #[cfg(test)]
    pub fn record_fire(&self, id: &str, rfc3339: &str, disable: bool) -> Result<()> {
        self.mutate(id, |s| {
            s.last_fired = Some(rfc3339.to_string());
            if disable {
                s.enabled = false;
            }
        })
    }

    fn mutate(&self, id: &str, f: impl Fn(&mut Schedule)) -> Result<()> {
        self.with_state_lock(|| {
            let mut all = self.load_unlocked()?;
            let schedule = all
                .iter_mut()
                .find(|schedule| schedule.id == id)
                .with_context(|| summon_i18n::format!("rca76a985fd"))?;
            f(schedule);
            self.save_unlocked(&all)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn schedule(id: &str) -> Schedule {
        Schedule {
            id: id.into(),
            kind: summon_core::Kind::Interval,
            spec: "3600".into(),
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

    fn read_schedules(root: &Path, name: &str) -> Vec<Schedule> {
        serde_yaml::from_str(&fs::read_to_string(root.join(name)).unwrap()).unwrap()
    }

    #[test]
    fn agent_cleanup_removes_only_frozen_disabled_schedules() {
        let root = tempfile::tempdir().unwrap();
        let store = ScheduleStore::new(root.path());
        let mut selected = schedule("SCH-1");
        selected.agent = "target".into();
        selected.enabled = false;
        let other = schedule("SCH-2");
        store.add(&selected).unwrap();
        store.add(&other).unwrap();
        assert!(store.remove_agent_with("target", &[], |_| Ok(())).is_err());
        assert_eq!(store.load().unwrap().len(), 2);
        assert!(store
            .remove_agent_with::<()>("target", &[selected.clone()], |_| anyhow::bail!(
                "file failed"
            ))
            .is_err());
        assert_eq!(store.load().unwrap().len(), 2);
        store
            .remove_agent_with("target", &[selected], |commit| commit())
            .unwrap();
        assert_eq!(store.load().unwrap(), vec![other]);
    }

    #[test]
    fn disable_and_restore_move_the_complete_record_between_files() {
        let tmp = tempfile::tempdir().unwrap();
        let st = ScheduleStore::new(tmp.path());
        let mut original = schedule("SCH-1");
        original.context = summon_core::ScheduleContext::Fresh;
        original.last_fired = Some("2026-10-14T09:00:00+08:00".into());
        st.add(&original).unwrap();
        st.set_enabled("SCH-1", false).unwrap();
        assert!(read_schedules(tmp.path(), "schedules.yaml").is_empty());
        let mut archived = original.clone();
        archived.enabled = false;
        assert_eq!(
            read_schedules(tmp.path(), "schedules-archive.yaml"),
            vec![archived]
        );
        st.edit(
            "SCH-1",
            SchedulePatch {
                enabled: Some(true),
                ..Default::default()
            },
            "2026-10-14T10:00:00+08:00",
        )
        .unwrap();
        assert_eq!(read_schedules(tmp.path(), "schedules.yaml"), vec![original]);
        assert!(read_schedules(tmp.path(), "schedules-archive.yaml").is_empty());
    }

    #[test]
    fn load_migrates_mixed_yaml_and_preserves_existing_archive() {
        let tmp = tempfile::tempdir().unwrap();
        let active = schedule("SCH-1");
        let mut disabled = schedule("SCH-2");
        disabled.enabled = false;
        let mut archived = schedule("SCH-3");
        archived.enabled = false;
        fs::write(
            tmp.path().join("schedules.yaml"),
            serde_yaml::to_string(&vec![disabled.clone(), active.clone()]).unwrap(),
        )
        .unwrap();
        fs::write(
            tmp.path().join("schedules-archive.yaml"),
            serde_yaml::to_string(&vec![archived.clone()]).unwrap(),
        )
        .unwrap();
        let st = ScheduleStore::new(tmp.path());
        let all = st.load().unwrap();
        assert_eq!(all.len(), 3);
        assert_eq!(read_schedules(tmp.path(), "schedules.yaml"), vec![active]);
        assert_eq!(
            read_schedules(tmp.path(), "schedules-archive.yaml"),
            vec![disabled, archived]
        );
        assert_eq!(st.load().unwrap(), all);
    }

    #[test]
    fn once_checkpoint_archives_the_fired_record() {
        let tmp = tempfile::tempdir().unwrap();
        let st = ScheduleStore::new(tmp.path());
        let mut once = schedule("SCH-1");
        once.kind = summon_core::Kind::Once;
        once.spec = Local::now().to_rfc3339();
        st.add(&once).unwrap();
        let now = Local::now().to_rfc3339();
        assert!(st.record_fire_if_unchanged(&once, &now, true).unwrap());
        once.enabled = false;
        once.last_fired = Some(now);
        assert!(read_schedules(tmp.path(), "schedules.yaml").is_empty());
        assert_eq!(
            read_schedules(tmp.path(), "schedules-archive.yaml"),
            vec![once]
        );
    }

    #[test]
    fn archived_ids_are_reserved_even_without_active_yaml_or_counter() {
        let tmp = tempfile::tempdir().unwrap();
        let st = ScheduleStore::new(tmp.path());
        let mut archived = schedule("SCH-90");
        archived.enabled = false;
        fs::write(
            tmp.path().join("schedules-archive.yaml"),
            serde_yaml::to_string(&vec![archived]).unwrap(),
        )
        .unwrap();
        assert_eq!(st.next_id().unwrap(), "SCH-91");
        assert!(st.add(&schedule("SCH-90")).is_err());
        st.set_enabled("SCH-90", true).unwrap();
        assert_eq!(read_schedules(tmp.path(), "schedules.yaml")[0].id, "SCH-90");
        assert!(read_schedules(tmp.path(), "schedules-archive.yaml").is_empty());
    }

    #[test]
    fn corrupt_archive_blocks_mutations_and_migration_without_overwriting_files() {
        let tmp = tempfile::tempdir().unwrap();
        let st = ScheduleStore::new(tmp.path());
        let mut disabled = schedule("SCH-1");
        disabled.enabled = false;
        let yaml = serde_yaml::to_string(&vec![disabled]).unwrap();
        fs::write(tmp.path().join("schedules.yaml"), &yaml).unwrap();
        fs::write(tmp.path().join("schedules-archive.yaml"), "not: [valid").unwrap();
        assert!(st.load().is_err());
        assert!(st.add(&schedule("SCH-2")).is_err());
        assert_eq!(
            fs::read_to_string(tmp.path().join("schedules.yaml")).unwrap(),
            yaml
        );
        assert_eq!(
            fs::read_to_string(tmp.path().join("schedules-archive.yaml")).unwrap(),
            "not: [valid"
        );
    }

    #[test]
    fn interrupted_moves_recover_both_files_on_a_new_store() {
        use std::os::unix::fs::MetadataExt;
        for enabled in [false, true] {
            for stage in [1, 2] {
                let tmp = tempfile::tempdir().unwrap();
                let st = ScheduleStore::new(tmp.path());
                let mut record = schedule("SCH-1");
                record.enabled = !enabled;
                st.add(&record).unwrap();
                st.fail_after_write.store(stage, Ordering::SeqCst);
                assert!(st.set_enabled("SCH-1", enabled).is_err());
                assert_eq!(
                    fs::metadata(st.pending_path()).unwrap().mode() & 0o777,
                    0o600
                );
                record.enabled = enabled;
                let recovered = ScheduleStore::new(tmp.path());
                assert_eq!(recovered.load().unwrap(), vec![record.clone()]);
                assert!(!st.pending_path().exists());
                let active = read_schedules(tmp.path(), "schedules.yaml");
                let archived = read_schedules(tmp.path(), "schedules-archive.yaml");
                assert_eq!(active.len() + archived.len(), 1);
                if enabled {
                    assert_eq!(active, vec![record]);
                } else {
                    assert_eq!(archived, vec![record]);
                }
                assert_eq!(recovered.load().unwrap().len(), 1);
            }
        }
    }

    #[test]
    fn pending_transaction_recovers_before_any_file_was_replaced() {
        let tmp = tempfile::tempdir().unwrap();
        let st = ScheduleStore::new(tmp.path());
        st.add(&schedule("SCH-1")).unwrap();
        let mut desired = schedule("SCH-1");
        desired.enabled = false;
        atomic_write_mode(
            &st.pending_path(),
            serde_yaml::to_string(&vec![desired.clone()])
                .unwrap()
                .as_bytes(),
            0o600,
        )
        .unwrap();
        assert_eq!(
            ScheduleStore::new(tmp.path()).load().unwrap(),
            vec![desired.clone()]
        );
        assert!(read_schedules(tmp.path(), "schedules.yaml").is_empty());
        assert_eq!(
            read_schedules(tmp.path(), "schedules-archive.yaml"),
            vec![desired]
        );
    }

    #[test]
    fn corrupt_transaction_and_duplicate_ids_fail_closed() {
        let tmp = tempfile::tempdir().unwrap();
        let st = ScheduleStore::new(tmp.path());
        st.add(&schedule("SCH-1")).unwrap();
        let before = fs::read(st.yaml_path()).unwrap();
        fs::write(st.pending_path(), "not: [valid").unwrap();
        assert!(st.load().is_err());
        assert!(st.set_enabled("SCH-1", false).is_err());
        assert_eq!(fs::read(st.yaml_path()).unwrap(), before);
        fs::remove_file(st.pending_path()).unwrap();
        let mut duplicate = schedule("SCH-1");
        duplicate.enabled = false;
        fs::write(
            st.archive_path(),
            serde_yaml::to_string(&vec![duplicate]).unwrap(),
        )
        .unwrap();
        let archive_before = fs::read(st.archive_path()).unwrap();
        assert!(st.load().is_err());
        assert!(st.next_id().is_err());
        assert_eq!(fs::read(st.yaml_path()).unwrap(), before);
        assert_eq!(fs::read(st.archive_path()).unwrap(), archive_before);
    }

    #[test]
    fn independent_stores_serialize_moves_edits_and_reads() {
        let tmp = tempfile::tempdir().unwrap();
        let st = ScheduleStore::new(tmp.path());
        st.add(&schedule("SCH-1")).unwrap();
        st.add(&schedule("SCH-2")).unwrap();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                let writer = ScheduleStore::new(tmp.path());
                for i in 0..12 {
                    writer.set_enabled("SCH-1", i % 2 != 0).unwrap();
                }
            });
            scope.spawn(|| {
                let writer = ScheduleStore::new(tmp.path());
                for i in 0..12 {
                    writer
                        .edit(
                            "SCH-2",
                            SchedulePatch {
                                prompt: Some(format!("prompt-{i}")),
                                ..Default::default()
                            },
                            "2026-10-14T10:00:00+08:00",
                        )
                        .unwrap();
                }
            });
            scope.spawn(|| {
                let reader = ScheduleStore::new(tmp.path());
                for _ in 0..24 {
                    let all = reader.load().unwrap();
                    assert_eq!(all.len(), 2);
                    assert_eq!(all.iter().filter(|s| s.id == "SCH-1").count(), 1);
                    assert_eq!(all.iter().filter(|s| s.id == "SCH-2").count(), 1);
                }
            });
        });
        let all = st.load().unwrap();
        assert!(all.iter().find(|s| s.id == "SCH-1").unwrap().enabled);
        assert_eq!(
            all.iter().find(|s| s.id == "SCH-2").unwrap().prompt,
            "prompt-11"
        );
        assert_eq!(read_schedules(tmp.path(), "schedules.yaml").len(), 2);
        assert!(read_schedules(tmp.path(), "schedules-archive.yaml").is_empty());
    }

    #[test]
    fn edit_context_alone_flips_only_that_field() {
        let tmp = tempfile::tempdir().unwrap();
        let st = ScheduleStore::new(tmp.path());
        st.add(&schedule("SCH-1")).unwrap();
        let (updated, rescheduled) = st
            .edit(
                "SCH-1",
                SchedulePatch {
                    context: Some(summon_core::ScheduleContext::Fresh),
                    ..Default::default()
                },
                "2026-09-02T14:30:00+08:00",
            )
            .unwrap();
        assert!(!rescheduled, "只改上下文模式不算改节律");
        assert_eq!(updated.context, summon_core::ScheduleContext::Fresh);
        assert_eq!(updated.output, summon_core::ScheduleOutput::Progress);
        assert_eq!(
            st.load().unwrap()[0].context,
            summon_core::ScheduleContext::Fresh,
            "落盘后重读仍是 fresh"
        );
    }

    #[test]
    fn store_round_trips_and_counter_increments() {
        let tmp = tempfile::tempdir().unwrap();
        let st = ScheduleStore::new(tmp.path());
        let id1 = st.next_id().unwrap();
        let id2 = st.next_id().unwrap();
        assert_eq!((id1.as_str(), id2.as_str()), ("SCH-1", "SCH-2"));
        let s = schedule(&id1);
        st.add(&s).unwrap();
        let all = st.load().unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].spec, "3600");
        st.record_fire("SCH-1", "2026-06-14T12:00:00+08:00", false)
            .unwrap();
        assert!(st.load().unwrap()[0].last_fired.is_some());
        st.set_enabled("SCH-1", false).unwrap();
        assert!(!st.load().unwrap()[0].enabled);
    }

    #[test]
    fn partial_edit_changes_only_explicit_fields_and_can_reenable() {
        let tmp = tempfile::tempdir().unwrap();
        let st = ScheduleStore::new(tmp.path());
        let mut original = schedule("SCH-1");
        original.enabled = false;
        original.last_fired = Some("2026-08-28T08:00:00+08:00".into());
        st.add(&original).unwrap();

        let (updated, rescheduled) = st
            .edit(
                "SCH-1",
                SchedulePatch {
                    prompt: Some("新正文".into()),
                    enabled: Some(true),
                    ..Default::default()
                },
                "2026-08-28T09:00:00+08:00",
            )
            .unwrap();
        assert!(!rescheduled);
        assert_eq!(updated.prompt, "新正文");
        assert!(updated.enabled);
        assert_eq!(updated.kind, original.kind);
        assert_eq!(updated.spec, original.spec);
        assert_eq!(updated.output, original.output);
        assert_eq!(updated.last_fired, original.last_fired);
    }

    #[test]
    fn due_claim_linearizes_against_edit_and_old_checkpoint_cannot_overwrite() {
        let tmp = tempfile::tempdir().unwrap();
        let st = ScheduleStore::new(tmp.path());
        let mut original = schedule("SCH-1");
        original.kind = summon_core::Kind::Once;
        original.spec = (Local::now() - chrono::Duration::minutes(1)).to_rfc3339();
        original.prompt = "旧正文".into();
        st.add(&original).unwrap();

        let claimed = st
            .claim_due(&original, Local::now(), |accepted, _| {
                Ok(accepted.prompt.clone())
            })
            .unwrap();
        let DueClaim::Accepted {
            schedule: accepted,
            value: accepted_prompt,
            ..
        } = claimed
        else {
            panic!("到期快照必须被接受")
        };
        assert_eq!(accepted_prompt, "旧正文");

        st.edit(
            "SCH-1",
            SchedulePatch {
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
        assert!(
            !st.record_fire_if_unchanged(&accepted, &Local::now().to_rfc3339(), true)
                .unwrap(),
            "旧 fire 不得给编辑后的新一代写 checkpoint"
        );
        let current = st.load().unwrap().remove(0);
        assert_eq!(current.prompt, "新正文");
        assert!(current.enabled);
        assert_eq!(current.last_fired, None);

        assert!(matches!(
            st.claim_due(&original, Local::now(), |_, _| Ok(()))
                .unwrap(),
            DueClaim::Stale
        ));
    }

    /// 换执行者只动 `agent` 一个字段:节律、群、正文、`last_fired` 全部原样。
    ///
    /// `last_fired` 尤其不能碰 —— 它是节拍基准(Cron/Interval)或「已经跑过了」的哨兵(Once)。
    /// 交接执行者不该让下一 tick 补触发一次,也不该把一次性定时器复活。
    #[test]
    fn set_agent_moves_owner_and_leaves_the_rest_untouched() {
        let tmp = tempfile::tempdir().unwrap();
        let st = ScheduleStore::new(tmp.path());
        st.add(&schedule("SCH-1")).unwrap();
        st.record_fire("SCH-1", "2026-06-14T12:00:00+08:00", false)
            .unwrap();
        let before = st.load().unwrap().remove(0);

        st.set_agent("SCH-1", "b").unwrap();

        let after = st.load().unwrap().remove(0);
        assert_eq!(after.agent, "b", "归属没换过去");
        assert_eq!(after.chat_id, before.chat_id, "群不该跟着变");
        assert_eq!(after.spec, before.spec, "节律不该跟着变");
        assert_eq!(after.prompt, before.prompt, "正文不该跟着变");
        assert_eq!(after.last_fired, before.last_fired, "节拍基准不该被重置");
        assert_eq!(after.enabled, before.enabled, "启停状态不该跟着变");
    }

    /// 编辑语义的核心:**改节律 → `last_fired` 置 now**(否则下一 tick 立刻补触发一次),
    /// **只改正文 → `last_fired` 不动**(改文案不该影响节拍)。
    #[test]
    fn update_rhythm_resets_last_fired_only_when_rhythm_changed() {
        let tmp = tempfile::tempdir().unwrap();
        let st = ScheduleStore::new(tmp.path());
        st.add(&schedule("SCH-1")).unwrap();
        st.record_fire("SCH-1", "2026-06-14T12:00:00+08:00", false)
            .unwrap();
        let fired_at = st.load().unwrap()[0].last_fired.clone().unwrap();

        // 只改正文:节律没动,last_fired 必须原样保留。
        let rescheduled = st
            .update_rhythm(
                "SCH-1",
                summon_core::Kind::Interval,
                "3600",
                "新正文",
                summon_core::ScheduleOutput::Result,
                "2026-06-14T18:00:00+08:00",
            )
            .unwrap();
        assert!(!rescheduled);
        let s = st.load().unwrap().remove(0);
        assert_eq!(s.prompt, "新正文");
        assert_eq!(s.output, summon_core::ScheduleOutput::Result);
        assert_eq!(s.last_fired.as_deref(), Some(fired_at.as_str()));

        // 把 1h 改成 30m:若保留 12:00 的 last_fired,下一 tick 就会立刻补触发。置 now 才对。
        let rescheduled = st
            .update_rhythm(
                "SCH-1",
                summon_core::Kind::Interval,
                "1800",
                "新正文",
                summon_core::ScheduleOutput::Result,
                "2026-06-14T18:00:00+08:00",
            )
            .unwrap();
        assert!(rescheduled);
        let s = st.load().unwrap().remove(0);
        assert_eq!(s.spec, "1800");
        assert_eq!(s.last_fired.as_deref(), Some("2026-06-14T18:00:00+08:00"));
        // 改节律不该顺手改动其它字段(尤其 enabled / created_by)。
        assert!(s.enabled);
        assert_eq!(s.created_by, "ou");
    }

    /// Once 的 `last_fired` 是「已经跑过了」的哨兵,不是时间基准(`due_plan` 要求 `last.is_none()`)。
    /// 给它盖时间戳 = 这条定时器再也不会触发,而回执还在说「已更新」。
    #[test]
    fn update_rhythm_clears_last_fired_for_once_so_it_still_fires() {
        use chrono::TimeZone;
        let tmp = tempfile::tempdir().unwrap();
        let st = ScheduleStore::new(tmp.path());
        let mut once = schedule("SCH-1");
        once.kind = summon_core::Kind::Once;
        once.spec = "2026-08-04T09:00:00+08:00".into();
        st.add(&once).unwrap();

        // 把时间从 09:00 改到 20:00 —— 编辑一条 Once 定时器的唯一目的。
        let rescheduled = st
            .update_rhythm(
                "SCH-1",
                summon_core::Kind::Once,
                "2026-08-04T20:00:00+08:00",
                "提交周报",
                summon_core::ScheduleOutput::Progress,
                "2026-08-03T15:00:00+08:00",
            )
            .unwrap();
        assert!(rescheduled);
        let s = st.load().unwrap().remove(0);
        assert_eq!(
            s.last_fired, None,
            "Once 改完必须清哨兵,否则 due_plan 永远不会让它触发"
        );

        // 直接拿 due_plan 验收:到点必须真的会触发。
        let after = chrono::Local
            .with_ymd_and_hms(2026, 8, 4, 20, 1, 0)
            .unwrap();
        assert!(
            summon_core::due_plan(&s, after).fire_now,
            "改过时间的一次性定时器到点必须触发"
        );
    }

    #[test]
    fn update_rhythm_rejects_unknown_id() {
        let tmp = tempfile::tempdir().unwrap();
        let st = ScheduleStore::new(tmp.path());
        st.add(&schedule("SCH-1")).unwrap();
        assert!(st
            .update_rhythm(
                "SCH-404",
                summon_core::Kind::Interval,
                "1800",
                "x",
                summon_core::ScheduleOutput::Progress,
                "2026-06-14T18:00:00+08:00"
            )
            .is_err());
        // 失败不得留下半截写入。
        assert_eq!(st.load().unwrap()[0].spec, "3600");
    }

    #[test]
    fn next_id_repairs_corrupt_or_rolled_back_counter() {
        for counter in ["broken", "2"] {
            let tmp = tempfile::tempdir().unwrap();
            let st = ScheduleStore::new(tmp.path());
            st.add(&schedule("SCH-9")).unwrap();
            fs::create_dir_all(tmp.path().join("schedules")).unwrap();
            fs::write(st.counter_path(), counter).unwrap();
            assert_eq!(st.next_id().unwrap(), "SCH-10");
            assert_eq!(fs::read_to_string(st.counter_path()).unwrap(), "10");
        }
    }

    #[test]
    fn corrupt_yaml_is_not_treated_as_empty_on_mutation() {
        let tmp = tempfile::tempdir().unwrap();
        let st = ScheduleStore::new(tmp.path());
        fs::write(st.yaml_path(), "not: [valid").unwrap();
        assert!(st.add(&schedule("SCH-1")).is_err());
        assert_eq!(fs::read_to_string(st.yaml_path()).unwrap(), "not: [valid");
    }

    #[test]
    fn failed_atomic_mutation_keeps_previous_yaml() {
        let tmp = tempfile::tempdir().unwrap();
        let st = ScheduleStore::new(tmp.path());
        st.add(&schedule("SCH-1")).unwrap();
        let before = fs::read_to_string(st.yaml_path()).unwrap();
        st.fail_next_save_for_test();
        assert!(st.set_enabled("SCH-1", false).is_err());
        assert_eq!(fs::read_to_string(st.yaml_path()).unwrap(), before);
    }

    #[test]
    fn concurrent_mutations_do_not_overwrite_each_other() {
        let tmp = tempfile::tempdir().unwrap();
        let st = Arc::new(ScheduleStore::new(tmp.path()));
        st.add(&schedule("SCH-1")).unwrap();
        let a = Arc::clone(&st);
        let enabled = std::thread::spawn(move || a.set_enabled("SCH-1", false));
        let b = Arc::clone(&st);
        let fired =
            std::thread::spawn(move || b.record_fire("SCH-1", "2026-06-14T12:00:00+08:00", false));
        enabled.join().unwrap().unwrap();
        fired.join().unwrap().unwrap();
        let saved = st.load().unwrap();
        assert!(!saved[0].enabled);
        assert_eq!(
            saved[0].last_fired.as_deref(),
            Some("2026-06-14T12:00:00+08:00")
        );
    }
}
