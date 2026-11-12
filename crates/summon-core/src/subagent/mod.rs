//! Subagent 状态检测纯逻辑。
//!
//! 从 claude 转录 JSONL 行里解析 Agent/Workflow tool_use 的 spawn / 完成事件,
//! 以及从 subagent 独立文件的内容判定运行状态。所有函数均无 IO,可单测。
//!
//! 转录行结构参考:`docs/spec/2026-07-06-subagent-observability-design.md` §1.1
//!
//! ## 历史正确性铁律(2026-08-14 真机对账后立)
//!
//! 面板/live 卡要回答的是「**此刻**在跑什么」,而转录与 `subagents/` 目录承载的是
//! 「本会话**历史上**发生过什么」。两者之间的桥是**事件自带的时间戳**:
//! - 状态迁移一律使用**事件发生时刻**(转录行 `timestamp` / 目录文件 mtime),绝不用
//!   「watcher 看到这行的时刻」——否则重放历史(watcher 冷启 offset=0)会把几天前的
//!   完成/失败整批渲成「刚刚发生」。
//! - 终态条目**软删**:留在状态里只是不再展示(展示窗口见 `COMPLETED_DISPLAY_TTL` /
//!   `FAILED_DISPLAY_TTL`)。物理删除曾造成「目录重扫→按 now 复活」的无限循环。
//! - 权威对账:主转录 `turn_duration` 行的 `pendingBackgroundAgentCount` /
//!   `pendingWorkflowCount` 是 claude 自己的计数,漂移落日志(注意它有排队语义:
//!   完成通知已入队但未被主对话消费时计数不减,真机实测过)。

use chrono::{DateTime, Utc};
use serde::Deserialize;
use std::path::Path;

// ── 状态模型 ──────────────────────────────────────────────

/// 单个 subagent 的运行状态。
#[derive(Debug, Clone, PartialEq)]
pub enum SubagentStatus {
    /// 转录里检测到 tool_use 但还没拿到 agentId(同步执行中或刚 spawn 未确认)。
    Spawning {
        description: String,
        agent_type: String,
        tool_use_id: String,
        detected_at: DateTime<Utc>,
        /// Agent tool_use input 里的 `model` 参数(用户显式指定才有;未指定=继承,不猜)。
        model: Option<String>,
    },
    /// 异步 subagent 已确认在跑。
    Running {
        agent_id: String,
        description: String,
        agent_type: String,
        tool_use_id: String,
        started_at: DateTime<Utc>,
        /// 子代理转录 `agent-<id>.jsonl` 的最近写入时间(目录扫描回填;转录侧刚检测到时是 None)。
        /// started_at 只说明起了多久,last_activity 才说明此刻还在动——把「轮内在跑/僵尸/陈旧」区分开。
        last_activity: Option<DateTime<Utc>>,
        /// 实际解析到的模型:async_launched 的 `resolvedModel`(权威)> spawn input 的 `model`
        /// > workflow 子代理 meta.json 的 `model`。哪都拿不到就 None,渲染层不显示、不猜。
        model: Option<String>,
    },
    /// 已完成(检测到 end_turn 或 task-notification completed)。
    Completed {
        agent_id: String,
        description: String,
        finished_at: DateTime<Utc>,
        model: Option<String>,
    },
    /// 失败(task-notification failed)。
    Failed {
        agent_id: String,
        description: String,
        error: String,
        finished_at: DateTime<Utc>,
        model: Option<String>,
    },
}

impl SubagentStatus {
    /// 展示用描述。
    pub fn description(&self) -> &str {
        match self {
            Self::Spawning { description, .. } => description,
            Self::Running { description, .. } => description,
            Self::Completed { description, .. } => description,
            Self::Failed { description, .. } => description,
        }
    }

    /// 关联的 tool_use_id(如果有)。
    pub fn tool_use_id(&self) -> Option<&str> {
        match self {
            Self::Spawning { tool_use_id, .. } => Some(tool_use_id),
            Self::Running { tool_use_id, .. } => Some(tool_use_id),
            _ => None,
        }
    }

    /// 关联的 agent_id(如果有)。
    pub fn agent_id(&self) -> Option<&str> {
        match self {
            Self::Spawning { .. } => None,
            Self::Running { agent_id, .. } => Some(agent_id),
            Self::Completed { agent_id, .. } => Some(agent_id),
            Self::Failed { agent_id, .. } => Some(agent_id),
        }
    }

    /// 采到的模型(如果有)。
    pub fn model(&self) -> Option<&str> {
        match self {
            Self::Spawning { model, .. }
            | Self::Running { model, .. }
            | Self::Completed { model, .. }
            | Self::Failed { model, .. } => model.as_deref(),
        }
    }

    /// 是否已结束(Completed 或 Failed)。
    pub fn is_finished(&self) -> bool {
        matches!(self, Self::Completed { .. } | Self::Failed { .. })
    }

    /// 是否在跑(Running 或 Spawning)。
    pub fn is_active(&self) -> bool {
        !self.is_finished()
    }

    /// 「此子代理最近还活着」的最好证据时刻:last_activity 与 started_at 取**最大**。
    /// 不能只信 last_activity:被 SendMessage 续起(reactivate)的 Running,其 jsonl mtime
    /// 在真正开跑前还停在上一段的旧值——目录回填会把锚拖回过去,时效闸会把刚续起的误隐。
    pub fn activity_anchor(&self) -> DateTime<Utc> {
        match self {
            Self::Spawning { detected_at, .. } => *detected_at,
            Self::Running {
                started_at,
                last_activity,
                ..
            } => last_activity.map_or(*started_at, |a| a.max(*started_at)),
            Self::Completed { finished_at, .. } | Self::Failed { finished_at, .. } => *finished_at,
        }
    }

    /// 人类可读的耗时/时间。
    pub fn elapsed_human(&self, now: DateTime<Utc>) -> String {
        let (start, label) = match self {
            Self::Spawning { detected_at, .. } => (detected_at, summon_i18n::text!("r33439d2631")),
            Self::Running { started_at, .. } => (started_at, summon_i18n::text!("r1f0eb99b7e")),
            Self::Completed { finished_at, .. } => (finished_at, summon_i18n::text!("rf28461bb49")),
            Self::Failed { finished_at, .. } => (finished_at, summon_i18n::text!("r28384d7afd")),
        };
        format!("{label} {}", human_duration((now - *start).num_seconds()))
    }

    /// 运行中条目的最后活跃时刻,渲成**绝对时间**(如「最后活跃 01:38:42」,HH:MM:SS,
    /// 时区由调用方传入——面板传本地时区,与「更新于」时间戳同风格)。
    /// 仅 Running 且目录扫描已回填 last_activity 时有值;其余状态谈不上「活跃」。
    ///
    /// 为什么不渲「Ys 前」:面板 PATCH 只由 mtime 量化推进触发,子代理停写/卡死后 mtime
    /// 不再变、脏标记永不再来,相对文案会永久停在「活跃 0s 前」——恰好把要区分的
    /// 「在跑/僵尸/陈旧」重新糊在一起。绝对时刻静止时读起来就是静止的,无需时间驱动刷新;
    /// mtime 量化带来的显示误差至多约一个步长(`ACTIVITY_QUANTUM_SECS`)。
    pub fn activity_human<Tz: chrono::TimeZone>(&self, tz: &Tz) -> Option<String>
    where
        Tz::Offset: std::fmt::Display,
    {
        match self {
            Self::Running {
                last_activity: Some(at),
                ..
            } => Some(summon_i18n::format!(
                "r6acd4cc5de",
                at.with_timezone(tz).format("%H:%M:%S")
            )),
            _ => None,
        }
    }
}

/// 秒数 → 人类可读时长(45s / 2m5s / 1h2m)。负数(时钟回拨)按 0 处理。
fn human_duration(secs: i64) -> String {
    let secs = secs.max(0);
    if secs >= 3600 {
        format!("{}h{}m", secs / 3600, (secs % 3600) / 60)
    } else if secs >= 60 {
        format!("{}m{}s", secs / 60, secs % 60)
    } else {
        format!("{secs}s")
    }
}

/// 一个 Workflow run(`Workflow` 工具起的动态工作流)。
///
/// 真机实测(claude 2.1.229/231)workflow 的转录事件与顶层 Agent **不同构**:
/// - 启动确认:`toolUseResult{status:"async_launched", taskType:"local_workflow",
///   taskId, runId, workflowName, summary}` —— **没有 agentId**;
/// - 完成通知:`<task-notification>` 的 `<task-id>` = **taskId**(不是 runId);
/// - 扇出的子代理落在 `subagents/workflows/<runId>/` 下,journal.jsonl 记
///   `{"type":"started"|"result","agentId":...}` 配对。
#[derive(Debug, Clone, PartialEq)]
pub struct WorkflowRun {
    /// 完成通知用的 task id(如 `wco3l6qra`)。目录兜底建出的 run 拿不到,留空。
    pub task_id: String,
    /// `subagents/workflows/` 下的目录名(如 `wf_8448782b-2ee`)。
    pub run_id: String,
    /// workflow 名(workflowName / 脚本 meta.name / 脚本文件名反解)。
    pub name: String,
    pub tool_use_id: String,
    pub started_at: DateTime<Utc>,
    /// None=还在跑;Some=终态(failed 标记失败)。
    pub outcome: Option<WorkflowOutcome>,
    /// 脚本 meta.phases 的阶段总数(阶段**进行位置**盘上无记录,不猜、不渲)。
    pub phase_total: Option<usize>,
    /// journal.jsonl 的 started/result 配对计数。
    pub journal_started: usize,
    pub journal_done: usize,
    /// journal.jsonl mtime(工作流最近一次记账 ≈ 最近活跃)。
    pub last_activity: Option<DateTime<Utc>>,
    /// 扇出的子代理(目录扫描回填)。
    pub children: Vec<SubagentStatus>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct WorkflowOutcome {
    pub failed: bool,
    pub finished_at: DateTime<Utc>,
}

impl WorkflowRun {
    pub fn is_finished(&self) -> bool {
        self.outcome.is_some()
    }

    /// 「这个 run 最近还活着」的最好证据:journal mtime 与**活跃 child 的转录 mtime** 取最大。
    /// 只看 journal 会把长跑(>2h)但 child 仍在持续输出的 workflow 整组误隐——journal 只在
    /// started/result 记账时写,phase 内长任务期间它可以几小时不动。
    fn activity_anchor(&self) -> DateTime<Utc> {
        if let Some(o) = &self.outcome {
            return o.finished_at;
        }
        self.children
            .iter()
            .filter(|c| c.is_active())
            .map(|c| c.activity_anchor())
            .chain(self.last_activity)
            .chain(std::iter::once(self.started_at))
            .max()
            .expect(summon_i18n::text!("re540665f7d"))
    }
}

/// turn_duration 行里的权威计数(claude 自己维护的 pending 数)。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TurnAuthority {
    pub at: DateTime<Utc>,
    pub pending_background: Option<u64>,
    pub pending_workflows: Option<u64>,
}

/// 一个 chat 的 subagent 全量状态。
#[derive(Debug, Clone, Default)]
pub struct SubagentState {
    /// 顶层 Agent 工具的子代理(含 Workflow tool_use 尚未确认时的 Spawning 占位)。
    pub agents: Vec<SubagentStatus>,
    /// Workflow run(确认启动后从 Spawning 转入;或目录兜底建出)。
    pub workflows: Vec<WorkflowRun>,
    /// 最后一次检测到变化的时间(用于 debounce card update)。
    pub last_changed: DateTime<Utc>,
    /// 最近一次 turn_duration 权威计数(对账基准,漂移由 watcher 落日志)。
    pub authority: Option<TurnAuthority>,
}

impl SubagentState {
    pub fn new() -> Self {
        Self::default()
    }

    /// 插入一个 Spawning 条目(去重:同 tool_use_id 不重复插)。
    /// `at` 是转录行时间戳——重放历史时必须是事件时刻,不是「现在」。
    pub fn insert_spawning(
        &mut self,
        description: String,
        agent_type: String,
        tool_use_id: String,
        model: Option<String>,
        at: DateTime<Utc>,
    ) -> bool {
        if self
            .agents
            .iter()
            .any(|a| a.tool_use_id() == Some(tool_use_id.as_str()))
            || self.workflows.iter().any(|w| w.tool_use_id == tool_use_id)
        {
            return false;
        }
        self.agents.push(SubagentStatus::Spawning {
            description,
            agent_type,
            tool_use_id,
            detected_at: at,
            model,
        });
        self.last_changed = Utc::now();
        true
    }

    /// 把 Spawning 提升为 Running(按 tool_use_id 匹配)。`model` 传 async_launched 的
    /// resolvedModel(权威),None 时保留 spawn input 的 model。
    pub fn promote_to_running(
        &mut self,
        agent_id: &str,
        description: &str,
        tool_use_id: &str,
        model: Option<&str>,
        at: DateTime<Utc>,
    ) -> bool {
        // 同 agent_id 已存在(终态/在跑)→ 不重复插(重放去重)。
        if self.agents.iter().any(|a| a.agent_id() == Some(agent_id)) {
            return false;
        }
        for a in &mut self.agents {
            if let SubagentStatus::Spawning {
                tool_use_id: tuid,
                agent_type,
                model: spawn_model,
                description: spawn_desc,
                ..
            } = a
            {
                if tuid == tool_use_id {
                    let agent_type = agent_type.clone();
                    let model = model.map(str::to_string).or_else(|| spawn_model.clone());
                    // launch 记录的 description 缺席时保留 spawn 时的(别把有信息换成空串)。
                    let description = if description.is_empty() {
                        spawn_desc.clone()
                    } else {
                        description.to_string()
                    };
                    *a = SubagentStatus::Running {
                        agent_id: agent_id.to_string(),
                        description,
                        agent_type,
                        tool_use_id: tool_use_id.to_string(),
                        started_at: at,
                        last_activity: None,
                        model,
                    };
                    self.last_changed = Utc::now();
                    return true;
                }
            }
        }
        // 没找到 Spawning,直接插一个 Running(可能转录里先看到 async_launched 再看到 spawn)
        self.agents.push(SubagentStatus::Running {
            agent_id: agent_id.to_string(),
            description: description.to_string(),
            agent_type: String::new(),
            tool_use_id: tool_use_id.to_string(),
            started_at: at,
            last_activity: None,
            model: model.map(str::to_string),
        });
        self.last_changed = Utc::now();
        true
    }

    /// 派出调用 `tool_use_id` 的子代理结束了没有。在跑的条目带着调用 id;结束的条目只剩
    /// agentId,由调用方另外记下的 `agent_id` 认。`None` = 两边都认不出,不下结论。
    ///
    /// 带调用 id 的 Spawning 不是定论:重放时终态条目已不带调用 id,`insert_spawning` 的去重
    /// 拦不住同一调用再插一条 Spawning。所以先认非 Spawning 的条目,只认得出 Spawning 时再按
    /// agentId 查,查不到才按它说的「还没结束」答。
    pub fn finished_for(&self, tool_use_id: &str, agent_id: Option<&str>) -> Option<bool> {
        let mut spawning = false;
        for agent in self
            .agents
            .iter()
            .filter(|agent| agent.tool_use_id() == Some(tool_use_id))
        {
            if matches!(agent, SubagentStatus::Spawning { .. }) {
                spawning = true;
            } else {
                return Some(agent.is_finished());
            }
        }
        if let Some(agent) = agent_id.and_then(|agent_id| {
            self.agents
                .iter()
                .find(|agent| agent.agent_id() == Some(agent_id))
        }) {
            return Some(agent.is_finished());
        }
        spawning.then_some(false)
    }

    /// 在跑条目里派出调用 id 对应的 agentId(结束后就只能按它认人)。
    pub fn agent_id_for(&self, tool_use_id: &str) -> Option<&str> {
        self.agents
            .iter()
            .filter(|agent| agent.tool_use_id() == Some(tool_use_id))
            .find_map(SubagentStatus::agent_id)
    }

    /// 标记为已完成(按 agent_id 匹配)。已完成的不重复标记;**未知 id 不建条目**——
    /// task-notification 的 task-id 还覆盖后台 shell 任务等非 agent 实体,凭通知建条目
    /// 会把「Background command finished」渲成子代理。
    ///
    /// 描述保留原值:通知 summary 是「Agent "X" finished」这类套话,信息量低于 spawn 时
    /// 的 description,不覆盖。
    pub fn mark_completed(&mut self, agent_id: &str, at: DateTime<Utc>) -> bool {
        for a in &mut self.agents {
            if a.agent_id() == Some(agent_id) {
                if a.is_finished() {
                    return false; // 已经结束了,不重复标记
                }
                *a = SubagentStatus::Completed {
                    agent_id: agent_id.to_string(),
                    description: a.description().to_string(),
                    finished_at: at,
                    model: a.model().map(str::to_string),
                };
                self.last_changed = Utc::now();
                return true;
            }
        }
        false
    }

    /// 同步 Agent 完成:同步没有 async_launched,一步从 Spawning(按 tool_use_id)或
    /// 未知态直接到 Completed。`model` 传 toolUseResult 的 resolvedModel(有就带)。
    pub fn mark_sync_completed(
        &mut self,
        agent_id: &str,
        description: &str,
        model: Option<&str>,
        at: DateTime<Utc>,
    ) -> bool {
        if self.mark_completed(agent_id, at) {
            return true;
        }
        // 还在 Spawning(同步 agent 没有 agent_id)→ 按最早的 Spawning 收敛。
        for a in &mut self.agents {
            if let SubagentStatus::Spawning {
                description: d,
                model: m,
                ..
            } = a
            {
                if d == description {
                    let model = model.map(str::to_string).or_else(|| m.clone());
                    *a = SubagentStatus::Completed {
                        agent_id: agent_id.to_string(),
                        description: description.to_string(),
                        finished_at: at,
                        model,
                    };
                    self.last_changed = Utc::now();
                    return true;
                }
            }
        }
        false
    }

    /// 标记为失败(按 agent_id 匹配)。已结束的不重复标记;未知 id 不建条目(同上)。
    pub fn mark_failed(&mut self, agent_id: &str, error: &str, at: DateTime<Utc>) -> bool {
        for a in &mut self.agents {
            if a.agent_id() == Some(agent_id) {
                if a.is_finished() {
                    return false;
                }
                *a = SubagentStatus::Failed {
                    agent_id: agent_id.to_string(),
                    description: a.description().to_string(),
                    error: error.to_string(),
                    finished_at: at,
                    model: a.model().map(str::to_string),
                };
                self.last_changed = Utc::now();
                return true;
            }
        }
        false
    }

    /// SendMessage 改名:卡片显示**最新名字**(owner 裁决)——续用/追加消息时 `input.summary`
    /// 就是这个 subagent 当下在干什么的最新标签,覆盖旧 description。未知 id(发给 teammate、
    /// 后台任务等)no-op,不建条目。所有状态都可改:Completed 被改名后若紧接着被续起,
    /// 复活时带的就是新名。
    pub fn rename(&mut self, agent_id: &str, name: &str) -> bool {
        if name.trim().is_empty() {
            return false;
        }
        for a in &mut self.agents {
            if a.agent_id() == Some(agent_id) {
                let slot = match a {
                    SubagentStatus::Spawning { description, .. }
                    | SubagentStatus::Running { description, .. }
                    | SubagentStatus::Completed { description, .. }
                    | SubagentStatus::Failed { description, .. } => description,
                };
                if slot == name {
                    return false;
                }
                *slot = name.to_string();
                self.last_changed = Utc::now();
                return true;
            }
        }
        false
    }

    /// SendMessage 续用:已终态的 subagent 被重新激活为 Running(owner 裁决,推翻此前
    /// 「保持 Completed」的取舍)。真机记录形态见 `detect_resume`:
    /// - Form A/B(`resumedAgentId`)= claude 已确认重启;
    /// - Form C(queued)= 消息入队,claude 会在该 agent 下一 tool round 投递——对已终态的
    ///   agent 意味着**必然复活**(投递本身不再留任何转录记录,真机验证过),所以入队即激活。
    ///
    /// Running/未知 id → no-op。started_at 取事件时刻;last_activity 清空(旧 mtime 属上一段,
    /// 时效判定见 `activity_anchor` 的取大)。
    pub fn reactivate(&mut self, agent_id: &str, at: DateTime<Utc>) -> bool {
        for a in &mut self.agents {
            if a.agent_id() == Some(agent_id) {
                if !a.is_finished() {
                    return false; // 还在跑:入队消息不改变状态
                }
                *a = SubagentStatus::Running {
                    agent_id: agent_id.to_string(),
                    description: a.description().to_string(),
                    agent_type: String::new(),
                    tool_use_id: String::new(),
                    started_at: at,
                    last_activity: None,
                    model: a.model().map(str::to_string),
                };
                self.last_changed = Utc::now();
                return true;
            }
        }
        false
    }

    /// Workflow 启动确认:移除同 tool_use_id 的 Spawning 占位,登记 WorkflowRun。
    /// 重放去重:同 run_id / task_id 已存在则只回填缺失字段。
    pub fn workflow_launched(
        &mut self,
        task_id: &str,
        run_id: &str,
        name: &str,
        tool_use_id: &str,
        at: DateTime<Utc>,
    ) -> bool {
        let spawn_idx = self.agents.iter().position(|a| {
            matches!(a, SubagentStatus::Spawning { .. }) && a.tool_use_id() == Some(tool_use_id)
        });
        if let Some(idx) = spawn_idx {
            self.agents.remove(idx);
        }
        if let Some(existing) = self
            .workflows
            .iter_mut()
            .find(|w| w.run_id == run_id || (!task_id.is_empty() && w.task_id == task_id))
        {
            // 目录兜底先建了 run(task_id 空)→ 回填转录侧的权威字段。
            let mut changed = spawn_idx.is_some();
            if existing.task_id.is_empty() && !task_id.is_empty() {
                existing.task_id = task_id.to_string();
                changed = true;
            }
            if existing.tool_use_id.is_empty() && !tool_use_id.is_empty() {
                existing.tool_use_id = tool_use_id.to_string();
                changed = true;
            }
            if changed {
                self.last_changed = Utc::now();
            }
            return changed;
        }
        self.workflows.push(WorkflowRun {
            task_id: task_id.to_string(),
            run_id: run_id.to_string(),
            name: name.to_string(),
            tool_use_id: tool_use_id.to_string(),
            started_at: at,
            outcome: None,
            phase_total: None,
            journal_started: 0,
            journal_done: 0,
            last_activity: None,
            children: Vec::new(),
        });
        self.last_changed = Utc::now();
        true
    }

    /// Workflow 完成/失败(task-notification 的 task-id = taskId)。
    pub fn finish_workflow(&mut self, task_id: &str, failed: bool, at: DateTime<Utc>) -> bool {
        if task_id.is_empty() {
            return false;
        }
        for w in &mut self.workflows {
            if w.task_id == task_id {
                if w.outcome.is_some() {
                    return false;
                }
                w.outcome = Some(WorkflowOutcome {
                    failed,
                    finished_at: at,
                });
                self.last_changed = Utc::now();
                return true;
            }
        }
        false
    }

    /// 从目录扫描结果补全**顶层**子代理状态(转录里没检测到的,从文件系统兜底)。
    /// 返回是否发生状态变化,供 watcher 决定是否刷新面板卡。
    ///
    /// 终态条目在状态里**软删**(不展示但不移除),所以这里对「已知且已终态」的目录条目
    /// 恒 no-op——这是挡住「prune 删掉 → 重扫按 now 复活 → 30s 后再删」无限循环的关键。
    /// 新见的已完成条目 finished_at 取 `last_activity`(jsonl 最后写入 ≈ 完成时刻),
    /// 不取「现在」:几小时前完成的历史不得渲成「刚完成」。
    pub fn reconcile_from_dir(&mut self, dir_entries: Vec<DirSubagent>) -> bool {
        let now = Utc::now();
        let mut changed = false;
        for entry in dir_entries {
            if let Some(existing) = self
                .agents
                .iter_mut()
                .find(|a| a.agent_id() == Some(entry.agent_id.as_str()))
            {
                // 完成证据(jsonl 末行 end_turn)带的是**证据时刻**(mtime)。被 SendMessage 续起的
                // Running 在真正开跑前,jsonl 末行还是上一段的 end_turn——证据早于本次激活的,
                // 属上一段,不得拿来终结这一段(否则复活一出生就被目录扫描杀掉)。
                let evidence_predates_activation = matches!(
                    existing,
                    SubagentStatus::Running { started_at, .. }
                        if *started_at > entry.last_activity.unwrap_or(entry.started_at)
                );
                if entry.completed && !existing.is_finished() && !evidence_predates_activation {
                    let desc = existing.description().to_string();
                    let model = existing.model().map(str::to_string).or(entry.model);
                    *existing = SubagentStatus::Completed {
                        agent_id: entry.agent_id,
                        description: desc,
                        finished_at: entry.last_activity.unwrap_or(entry.started_at),
                        model,
                    };
                    self.last_changed = now;
                    changed = true;
                } else if let SubagentStatus::Running {
                    last_activity,
                    model,
                    ..
                } = existing
                {
                    // 转录侧没采到 model 时,目录 meta 的 model 兜底(一次性回填)。
                    if model.is_none() && entry.model.is_some() {
                        *model = entry.model.clone();
                        self.last_changed = now;
                        changed = true;
                    }
                    // 刷新活跃时间,但按 ACTIVITY_QUANTUM_SECS 量化推进:目录 5s 扫一次,
                    // 活跃子代理每次 mtime 都在涨,原样透传会让面板每 5s PATCH 一次飞书。
                    // 量化后展示精度是「±量化步长」,对区分「在跑/僵尸」足够。只增不减
                    // (mtime 回退视为时钟抖动,忽略)。
                    if let Some(new_at) = entry.last_activity {
                        let stale_enough = match *last_activity {
                            None => true,
                            Some(old) => {
                                new_at - old >= chrono::Duration::seconds(ACTIVITY_QUANTUM_SECS)
                            }
                        };
                        if stale_enough {
                            *last_activity = Some(new_at);
                            self.last_changed = now;
                            changed = true;
                        }
                    }
                }
                continue;
            }
            let status = if entry.completed {
                SubagentStatus::Completed {
                    agent_id: entry.agent_id,
                    description: entry.description,
                    finished_at: entry.last_activity.unwrap_or(entry.started_at),
                    model: entry.model,
                }
            } else {
                SubagentStatus::Running {
                    agent_id: entry.agent_id,
                    description: entry.description,
                    agent_type: entry.agent_type,
                    tool_use_id: entry.tool_use_id,
                    started_at: entry.started_at,
                    last_activity: entry.last_activity,
                    model: entry.model,
                }
            };
            self.agents.push(status);
            self.last_changed = now;
            changed = true;
        }
        changed
    }

    /// 从目录扫描结果补全一个 workflow run 的组信息(journal 计数 / 阶段总数 / 子代理)。
    /// run 不存在(watcher 冷启漏了转录、或 /bridge 召回)→ 按目录事实建一个(task_id 留空,
    /// 完成只能靠 journal 配对 + 时效窗判定,展示层按 activity 过滤)。
    pub fn reconcile_workflow_dir(&mut self, info: WorkflowDirInfo) -> bool {
        let now = Utc::now();
        let mut changed = false;
        let run = match self.workflows.iter_mut().find(|w| w.run_id == info.run_id) {
            Some(r) => r,
            None => {
                self.workflows.push(WorkflowRun {
                    task_id: String::new(),
                    run_id: info.run_id.clone(),
                    name: info.name.clone().unwrap_or_else(|| "workflow".to_string()),
                    tool_use_id: String::new(),
                    started_at: info.started_at,
                    outcome: None,
                    phase_total: None,
                    journal_started: 0,
                    journal_done: 0,
                    last_activity: None,
                    children: Vec::new(),
                });
                changed = true;
                self.workflows
                    .last_mut()
                    .expect(summon_i18n::text!("r86ec618320"))
            }
        };
        if run.phase_total.is_none() && info.phase_total.is_some() {
            run.phase_total = info.phase_total;
            changed = true;
        }
        if (run.journal_started, run.journal_done) != (info.journal_started, info.journal_done) {
            run.journal_started = info.journal_started;
            run.journal_done = info.journal_done;
            changed = true;
        }
        // journal mtime 量化推进(与顶层 last_activity 同理,防每 5s 一次 PATCH)。
        if let Some(new_at) = info.journal_mtime {
            let stale_enough = match run.last_activity {
                None => true,
                Some(old) => new_at - old >= chrono::Duration::seconds(ACTIVITY_QUANTUM_SECS),
            };
            if stale_enough {
                run.last_activity = Some(new_at);
                changed = true;
            }
        }
        // 子代理清单:目录是唯一来源(转录里没有 per-child 事件),按 agent_id 逐条合并。
        // **不整组替换**:child 的 jsonl mtime 每次扫描都在涨,整组 PartialEq 恒判「变了」,
        // 会绕过 30s 量化、让面板每 5s PATCH 一次。合并规则与顶层 reconcile_from_dir 同构:
        // 终态软删恒 no-op、活跃 child 的 last_activity 量化推进、完成翻转吃 mtime 当 finished_at。
        for incoming in info.children {
            changed |= merge_workflow_child(&mut run.children, incoming);
        }
        if changed {
            self.last_changed = now;
        }
        changed
    }

    /// 记录 turn_duration 权威计数。返回是否变化。
    pub fn note_authority(&mut self, auth: TurnAuthority) -> bool {
        if self.authority == Some(auth) {
            return false;
        }
        self.authority = Some(auth);
        // 权威计数只做对账,不驱动展示 → 不动 last_changed。
        true
    }

    /// 对账:watcher 状态机的「在跑」计数 vs 最近一次权威计数。漂移返回 Some(描述),
    /// 由 watcher 落日志。**注意排队语义**:完成通知已入队但主对话还没消费时,
    /// pendingBackgroundAgentCount 不减(真机实测),所以「权威 ≥ 状态机」的短暂漂移是常态,
    /// 只报「状态机 > 权威」这种真正意味着漏报完成/多报运行的方向。
    pub fn count_drift(&self) -> Option<String> {
        let auth = self.authority?;
        let running_bg = self
            .agents
            .iter()
            .filter(|a| matches!(a, SubagentStatus::Running { .. }))
            .count() as u64;
        let running_wf = self.workflows.iter().filter(|w| !w.is_finished()).count() as u64;
        let mut drifts = Vec::new();
        if let Some(bg) = auth.pending_background {
            if running_bg > bg {
                drifts.push(summon_i18n::format!("r87e217f9b9"));
            }
        }
        if let Some(wf) = auth.pending_workflows {
            if running_wf > wf {
                drifts.push(summon_i18n::format!("rf52f711a46"));
            }
        }
        if drifts.is_empty() {
            None
        } else {
            Some(drifts.join("; "))
        }
    }

    /// 展示快照:按各状态的展示窗口过滤(历史条目软删在此生效)。
    pub fn display(&self, now: DateTime<Utc>) -> SubagentDisplay {
        let agents: Vec<SubagentStatus> = self
            .agents
            .iter()
            .filter(|a| status_displayable(a, now))
            .cloned()
            .collect();
        let workflows: Vec<WorkflowGroup> = self
            .workflows
            .iter()
            .filter(|w| workflow_displayable(w, now))
            .map(|w| WorkflowGroup {
                run_id: w.run_id.clone(),
                name: w.name.clone(),
                finished: w.outcome.clone(),
                phase_total: w.phase_total,
                journal_started: w.journal_started,
                journal_done: w.journal_done,
                started_at: w.started_at,
                children: w
                    .children
                    .iter()
                    .filter(|c| status_displayable(c, now))
                    .cloned()
                    .collect(),
            })
            .collect();
        SubagentDisplay { agents, workflows }
    }

    /// 空闲判定的活跃探针(dispatcher 消费,纯读、零 IO)。
    ///
    /// - `active`:此刻是否有活跃 subagent/workflow。判活**沿用展示层同一条时效轴**
    ///   (`status_displayable`/`workflow_displayable` 的活跃分支,窗宽
    ///   [`DIR_SUBAGENT_STALE_AFTER`]),不新增任何时间启发式 —— watcher 死掉后冻结的
    ///   Running 条目最多把「空闲回收」压后一个时效窗,随后自然过期,不会让会话永不回收。
    /// - `last_activity`:全部条目(**含已终态**)的最近活动时刻。subagent 结束后的空闲期
    ///   应从它的最后活动起算,而不是主轮结束起算;历史条目的旧时刻由调用方与入站时钟取
    ///   max,自然被淹没,不会把时钟拨回过去。
    pub fn activity_probe(&self, now: DateTime<Utc>) -> ActivityProbe {
        let active = self
            .agents
            .iter()
            .any(|a| a.is_active() && status_displayable(a, now))
            || self
                .workflows
                .iter()
                .any(|w| !w.is_finished() && workflow_displayable(w, now));
        let last_activity = self
            .agents
            .iter()
            .map(SubagentStatus::activity_anchor)
            .chain(self.workflows.iter().map(WorkflowRun::activity_anchor))
            .max();
        ActivityProbe {
            active,
            last_activity,
        }
    }
}

/// [`SubagentState::activity_probe`] 的产出:空闲判定要的两个事实。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ActivityProbe {
    /// 此刻有活跃(时效窗内的 Spawning/Running/未完 workflow)subagent。
    pub active: bool,
    /// 全部条目的最近活动时刻(活跃条目取活跃锚,终态条目取完成时刻);None = 从未观测到。
    pub last_activity: Option<DateTime<Utc>>,
}

/// 把目录扫出的一条 workflow child 合并进组内清单。返回是否发生**展示可见**的变化。
/// 与顶层 `reconcile_from_dir` 同一套纪律:
/// - 已终态的条目软删恒 no-op(挡复活循环);
/// - 完成翻转:finished_at 取 child 转录 mtime(`Completed.finished_at` 由 IO 层从 mtime 生成);
/// - 双 Running:last_activity 按 `ACTIVITY_QUANTUM_SECS` 量化推进(只增不减),model 缺则回填。
fn merge_workflow_child(children: &mut Vec<SubagentStatus>, incoming: SubagentStatus) -> bool {
    let Some(incoming_id) = incoming.agent_id().map(str::to_string) else {
        return false; // dir 投影恒有 agent_id;没有就丢,不猜
    };
    let Some(existing) = children
        .iter_mut()
        .find(|c| c.agent_id() == Some(incoming_id.as_str()))
    else {
        children.push(incoming);
        return true;
    };
    if existing.is_finished() {
        return false;
    }
    if incoming.is_finished() {
        let model = existing
            .model()
            .map(str::to_string)
            .or_else(|| incoming.model().map(str::to_string));
        *existing = SubagentStatus::Completed {
            agent_id: incoming_id,
            description: existing.description().to_string(),
            finished_at: match &incoming {
                SubagentStatus::Completed { finished_at, .. }
                | SubagentStatus::Failed { finished_at, .. } => *finished_at,
                _ => unreachable!("{}", summon_i18n::format!("r5ab9184b4b")),
            },
            model,
        };
        return true;
    }
    // 双活跃:量化推进 + model 回填。
    let mut changed = false;
    if let (
        SubagentStatus::Running {
            last_activity,
            model,
            ..
        },
        SubagentStatus::Running {
            last_activity: new_act,
            model: new_model,
            ..
        },
    ) = (existing, &incoming)
    {
        if model.is_none() && new_model.is_some() {
            *model = new_model.clone();
            changed = true;
        }
        if let Some(new_at) = new_act {
            let stale_enough = match *last_activity {
                None => true,
                Some(old) => *new_at - old >= chrono::Duration::seconds(ACTIVITY_QUANTUM_SECS),
            };
            if stale_enough {
                *last_activity = Some(*new_at);
                changed = true;
            }
        }
    }
    changed
}

/// 单条状态此刻是否可展示。
/// - Spawning/Running:活跃锚点(last_activity 或 started_at)在时效窗内。started_at 是
///   转录事件时刻——重放几天前的历史不会被当成「刚起的」。
/// - Completed:30s 展示窗(与旧行为一致)。
/// - Failed:30min 展示窗(旧行为是「永远展示」——重放历史时会把几天前的失败挂在面板上;
///   30min 足够人看到失败,又不会让历史失败永久驻留)。
fn status_displayable(s: &SubagentStatus, now: DateTime<Utc>) -> bool {
    match s {
        SubagentStatus::Spawning { .. } | SubagentStatus::Running { .. } => {
            now - s.activity_anchor() < DIR_SUBAGENT_STALE_AFTER
        }
        SubagentStatus::Completed { finished_at, .. } => now - *finished_at < COMPLETED_DISPLAY_TTL,
        SubagentStatus::Failed { finished_at, .. } => now - *finished_at < FAILED_DISPLAY_TTL,
    }
}

fn workflow_displayable(w: &WorkflowRun, now: DateTime<Utc>) -> bool {
    match &w.outcome {
        None => now - w.activity_anchor() < DIR_SUBAGENT_STALE_AFTER,
        Some(o) if o.failed => now - o.finished_at < FAILED_DISPLAY_TTL,
        Some(o) => now - o.finished_at < COMPLETED_DISPLAY_TTL,
    }
}

/// 展示快照:面板卡渲全量;live 卡尾部经 [`subagent_tail_lines`] 截断。
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SubagentDisplay {
    pub agents: Vec<SubagentStatus>,
    pub workflows: Vec<WorkflowGroup>,
}

impl SubagentDisplay {
    pub fn is_empty(&self) -> bool {
        self.agents.is_empty() && self.workflows.is_empty()
    }
}

/// workflow 组的展示投影(面板成组渲染 + live 卡尾部一行)。
#[derive(Debug, Clone, PartialEq)]
pub struct WorkflowGroup {
    /// 稳定身份(`subagents/workflows/` 下的目录名),供对账/测试把展示组映射回 run。
    pub run_id: String,
    pub name: String,
    pub finished: Option<WorkflowOutcome>,
    pub phase_total: Option<usize>,
    pub journal_started: usize,
    pub journal_done: usize,
    pub started_at: DateTime<Utc>,
    pub children: Vec<SubagentStatus>,
}

impl WorkflowGroup {
    /// 组头一行(不含缩进子代理):「workflow <名> · 阶段共 y · N 跑 M 完」。
    /// 阶段**进行位置**盘上无记录(journal 只有 started/result),不猜,只渲总数。
    pub fn header_line(&self, now: DateTime<Utc>) -> String {
        let state = match &self.finished {
            None => summon_i18n::format!(
                "ra4ed034be8",
                human_duration((now - self.started_at).num_seconds())
            ),
            Some(o) if o.failed => summon_i18n::text!("r28384d7afd").to_string(),
            Some(_) => summon_i18n::text!("rf28461bb49").to_string(),
        };
        let mut parts = vec![format!("workflow {}", self.name), state];
        if let Some(total) = self.phase_total {
            parts.push(summon_i18n::format!("ra2f36afca7"));
        }
        let running = self.journal_started.saturating_sub(self.journal_done);
        parts.push(summon_i18n::format!("rf679c93439", self.journal_done));
        parts.join(" · ")
    }
}

// ── live 卡尾部(P1) ─────────────────────────────────────

/// live 卡尾部最多列几条 subagent(超出折成「等 N 个」)。
pub const SUBAGENT_TAIL_MAX: usize = 3;
/// 尾部单条 title 的最大字符数(含省略号)。
const TAIL_TITLE_CHAR_CAP: usize = 30;

/// 把展示快照渲成 live 卡尾部的紧凑行(**纯文本**,飞书转义由渲染层做):
/// 运行中在前(Spawning/Running/未完 workflow),已完成/失败在后;超过 `max` 条截断,
/// 末尾补「等 N 个」。行形如「🔄 修空闲卡去重三条阻塞 (opus)」/「▸ workflow code-review · 2 跑 1 完」。
pub fn subagent_tail_lines(display: &SubagentDisplay, max: usize) -> Vec<String> {
    let now = Utc::now();
    let mut running: Vec<String> = Vec::new();
    let mut finished: Vec<String> = Vec::new();
    for a in &display.agents {
        let emoji = match a {
            SubagentStatus::Spawning { .. } => "🟡",
            SubagentStatus::Running { .. } => "🔄",
            SubagentStatus::Completed { .. } => "✅",
            SubagentStatus::Failed { .. } => "❌",
        };
        let title = clip_chars(a.description(), TAIL_TITLE_CHAR_CAP);
        let line = match a.model().map(short_model) {
            Some(m) => format!("{emoji} {title} ({m})"),
            None => format!("{emoji} {title}"),
        };
        if a.is_active() {
            running.push(line);
        } else {
            finished.push(line);
        }
    }
    for w in &display.workflows {
        let running_n = w.journal_started.saturating_sub(w.journal_done);
        let emoji = match &w.finished {
            None => "🔄",
            Some(o) if o.failed => "❌",
            Some(_) => "✅",
        };
        let line = summon_i18n::format!(
            "r6160e19536",
            clip_chars(&w.name, TAIL_TITLE_CHAR_CAP),
            w.journal_done
        );
        if w.finished.is_none() {
            running.push(line);
        } else {
            finished.push(line);
        }
    }
    let _ = now; // now 保留给未来「耗时」渲染;当前尾部不渲耗时,保持行宽紧凑。
    let mut lines: Vec<String> = running.into_iter().chain(finished).collect();
    if lines.len() > max {
        let rest = lines.len() - max;
        lines.truncate(max);
        lines.push(summon_i18n::format!("rf6673be697"));
    }
    lines
}

/// 模型短名:`claude-fable-5` → `fable-5`(固定已知前缀,纯展示裁剪,不改语义)。
/// workflow meta 里的 `opus` 这类短名原样保留。
pub fn short_model(model: &str) -> &str {
    model.strip_prefix("claude-").unwrap_or(model)
}

/// 按字符截断(含省略号),外部文本可能是多字节中文,绝不能按字节切。
fn clip_chars(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    let mut chars = s.chars();
    let mut out: String = chars.by_ref().take(max).collect();
    if chars.next().is_some() {
        out.pop();
        out.push('…');
    }
    out
}

/// 从目录扫描得到的 subagent 条目(IO 层填充,纯逻辑层消费)。
#[derive(Debug, Clone)]
pub struct DirSubagent {
    pub agent_id: String,
    pub description: String,
    pub agent_type: String,
    pub tool_use_id: String,
    pub completed: bool,
    pub started_at: DateTime<Utc>,
    /// `agent-<id>.jsonl` 的 mtime:子代理最近一次写转录的时间。文件缺失/读不到时 None。
    pub last_activity: Option<DateTime<Utc>>,
    /// meta.json 的 `model` 字段(workflow 子代理有;顶层 meta 没有,靠转录 resolvedModel)。
    pub model: Option<String>,
}

/// 目录扫描得到的一个 workflow run 的组信息(IO 层填充)。
#[derive(Debug, Clone)]
pub struct WorkflowDirInfo {
    pub run_id: String,
    /// scripts 目录文件名反解出的 workflow 名(反解不出=None,建 run 时回落「workflow」)。
    pub name: Option<String>,
    pub phase_total: Option<usize>,
    pub journal_started: usize,
    pub journal_done: usize,
    pub journal_mtime: Option<DateTime<Utc>>,
    pub started_at: DateTime<Utc>,
    pub children: Vec<SubagentStatus>,
}

/// last_activity 经 `reconcile_from_dir` 推进的量化步长(秒),动机见其内注释。
const ACTIVITY_QUANTUM_SECS: i64 = 30;

/// 目录兜底扫描的默认时效窗口。超过它、又拿不到完成证据的条目一律不当「活跃」。
pub const DIR_SUBAGENT_STALE_AFTER: chrono::Duration = chrono::Duration::hours(2);
/// Completed 条目的展示窗口(完成后短暂可见,让人知道「刚跑完」)。
pub const COMPLETED_DISPLAY_TTL: chrono::Duration = chrono::Duration::seconds(30);
/// Failed 条目的展示窗口。不再「永远展示」:重放历史会把几天前的失败挂成永久条目。
pub const FAILED_DISPLAY_TTL: chrono::Duration = chrono::Duration::minutes(30);

/// 目录条目是否还算「活跃」。**面板标题是「活跃子代理」,不是「本会话历史上起过的子代理」**。
///
/// 判活轴是 **last_activity(jsonl mtime)**,拿不到才回落 started_at:
/// - 起了 3h、此刻还在写转录的后台子代理是活的(按 started_at 判会误伤);
/// - 起了 20min、40min 前就停写的被打断子代理不是活的(按 started_at 判会误留)。
///
/// 背景(2026-07 实测事故):`/bridge` 召回历史会话,整目录残留被扫进来,面板列出十几条
/// 「运行中 38h31m」的「(无描述)」。时效窗口对「没有终态证据的陈旧条目」是必要防线。
/// 已完成的条目照收(finished_at 取 mtime,展示层按 `COMPLETED_DISPLAY_TTL` 过滤)。
pub fn dir_subagent_is_live(
    completed: bool,
    started_at: DateTime<Utc>,
    last_activity: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
    stale_after: chrono::Duration,
) -> bool {
    completed || (now - last_activity.unwrap_or(started_at)) < stale_after
}

// ── 转录行解析 ────────────────────────────────────────────

/// 转录 JSONL 的最小结构(只取我们关心的字段)。
#[derive(Debug, Deserialize)]
struct TranscriptLine {
    #[serde(rename = "type")]
    line_type: String,
    #[serde(default)]
    subtype: Option<String>,
    #[serde(default)]
    message: Option<Message>,
    #[serde(default, rename = "toolUseResult")]
    tool_use_result: Option<ToolUseResult>,
    #[serde(default)]
    operation: Option<String>,
    #[serde(default)]
    content: Option<serde_json::Value>,
    #[serde(default)]
    origin: Option<Origin>,
    #[serde(default)]
    timestamp: Option<String>,
    #[serde(default, rename = "pendingBackgroundAgentCount")]
    pending_background_agent_count: Option<u64>,
    #[serde(default, rename = "pendingWorkflowCount")]
    pending_workflow_count: Option<u64>,
}

impl TranscriptLine {
    /// 行时间戳(RFC3339)。解析不出 → None,调用方回落「现在」——只影响 live tail
    /// 的边缘情况;真机验证 turn/notification/toolUseResult 行 100% 带 timestamp。
    fn at(&self) -> Option<DateTime<Utc>> {
        let ts = self.timestamp.as_deref()?;
        DateTime::parse_from_rfc3339(ts)
            .ok()
            .map(|t| t.with_timezone(&Utc))
    }
}

#[derive(Debug, Deserialize)]
struct Message {
    #[allow(dead_code)]
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    content: serde_json::Value,
}

#[derive(Debug, Deserialize)]
struct ContentBlock {
    #[serde(rename = "type")]
    block_type: String,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    input: Option<serde_json::Value>,
    #[allow(dead_code)]
    #[serde(default)]
    text: Option<String>,
    #[serde(default, rename = "tool_use_id")]
    tool_use_id: Option<String>,
    #[allow(dead_code)]
    #[serde(default)]
    content: Option<Vec<ContentBlock>>,
}

#[derive(Debug, Deserialize)]
struct ToolUseResult {
    #[serde(default)]
    status: String,
    #[serde(default, rename = "isAsync")]
    is_async: Option<bool>,
    #[serde(default, rename = "agentId")]
    agent_id: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[allow(dead_code)]
    #[serde(default, rename = "agentType")]
    agent_type: Option<String>,
    #[serde(default, rename = "resolvedModel")]
    resolved_model: Option<String>,
    // Workflow(taskType=local_workflow)独有的字段(真机 2.1.229 实测)。
    #[serde(default, rename = "taskType")]
    task_type: Option<String>,
    #[serde(default, rename = "taskId")]
    task_id: Option<String>,
    #[serde(default, rename = "runId")]
    run_id: Option<String>,
    #[serde(default, rename = "workflowName")]
    workflow_name: Option<String>,
    // SendMessage 结果(续用)独有的字段(真机 2.1.229/231 实测,见 detect_resume)。
    #[serde(default)]
    success: Option<bool>,
    #[serde(default, rename = "resumedAgentId")]
    resumed_agent_id: Option<String>,
    #[serde(default)]
    pin: Option<Pin>,
}

#[derive(Debug, Deserialize)]
struct Pin {
    #[serde(default)]
    id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Origin {
    #[serde(default, rename = "kind")]
    kind: Option<String>,
}

/// spawn 事件(Agent/Workflow tool_use)。
#[derive(Debug, Clone, PartialEq)]
pub struct SpawnEvent {
    pub tool_use_id: String,
    pub description: String,
    pub agent_type: String,
    pub is_async: bool,
    /// Agent tool_use input 的 `model` 参数(显式指定才有)。
    pub model: Option<String>,
    /// 行时间戳。
    pub at: Option<DateTime<Utc>>,
}

/// 启动确认事件:顶层 Agent(有 agentId)或 Workflow(taskId/runId)。
#[derive(Debug, Clone, PartialEq)]
pub enum LaunchEvent {
    Agent {
        agent_id: String,
        description: String,
        tool_use_id: String,
        /// async_launched 的 resolvedModel(权威模型来源)。
        model: Option<String>,
        at: Option<DateTime<Utc>>,
    },
    Workflow {
        task_id: String,
        run_id: String,
        name: String,
        tool_use_id: String,
        at: Option<DateTime<Utc>>,
    },
}

/// 从转录 JSONL 行检测 Agent/Workflow tool_use(spawn 事件)。
pub fn detect_spawn(line: &str) -> Option<SpawnEvent> {
    let parsed: TranscriptLine = serde_json::from_str(line).ok()?;
    if parsed.line_type != "assistant" {
        return None;
    }
    let at = parsed.at();
    let msg = parsed.message?;
    let blocks = msg.content.as_array()?;

    for block in blocks {
        let cb: ContentBlock = serde_json::from_value(block.clone()).ok()?;
        if cb.block_type != "tool_use" {
            continue;
        }
        let name = cb.name.as_deref()?;
        if name != "Agent" && name != "Workflow" {
            continue;
        }

        let id = cb.id.clone()?;
        let input = cb.input?;

        if name == "Agent" {
            let desc = input
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or(summon_i18n::text!("rb30520db75"))
                .to_string();
            let atype = input
                .get("subagent_type")
                .and_then(|v| v.as_str())
                .unwrap_or("general-purpose")
                .to_string();
            let is_async = input
                .get("run_in_background")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let model = input
                .get("model")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            return Some(SpawnEvent {
                tool_use_id: id,
                description: desc,
                agent_type: atype,
                is_async,
                model,
                at,
            });
        } else {
            // Workflow:视为异步 workflow 类型 subagent。实际的 tool_use input **只有 `script`**
            // (整段 JS 文本;spec §1.1 记的 name/args 是早期观察,2.1.229 实测不存在)——名字从
            // script 头部的 `export const meta = { name: '…' }` 解析;input.name 仍优先认
            // (兼容旧格式),两处都拿不到回落「workflow」。
            let wf_name = input
                .get("name")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .or_else(|| {
                    input
                        .get("script")
                        .and_then(|v| v.as_str())
                        .and_then(workflow_name_from_script)
                })
                .unwrap_or_else(|| "workflow".to_string());
            let desc = format!("workflow: {wf_name}");
            let atype = format!("workflow:{wf_name}");
            return Some(SpawnEvent {
                tool_use_id: id,
                description: desc,
                agent_type: atype,
                is_async: true, // Workflow 总是异步
                model: None,
                at,
            });
        }
    }
    None
}

/// 从 Workflow tool_use 的 `input.script`(整段 JS 文本)头部解析 workflow 名。
///
/// 实测格式:脚本以 `export const meta = {\n  name: "code-review", …` 开头(引号有单/双引号
/// 两种变体,反引号一并容忍)。不引 JS 解析器,按字符串定位:只认 `export const meta` 后第一个
/// `{` 起、头部窗口(2048 字节)内、键边界正确(前一字符是 `{`/`,`/空白)的未加引号 `name` 键,
/// 值须是引号包住的单行字符串。
///
/// 容错边界(私有格式无稳定性承诺,解析不出一律 None,由调用方回落「workflow」):
/// - meta 不在头部窗口内、或 `name` 键缺失 → None;
/// - 值非字符串字面量 / 空 / 含换行(含未闭合引号吞到换行的情形)/ 超 200 字符 → None;
/// - 名字本身含同种引号会被截断——workflow 名实际会成为脚本文件名成分,不含引号,可接受;
/// - `name` 之前若有别的字符串值恰好嵌着 ` name: '…'` 片段会误取——实测生成格式 name 恒为
///   首键,风险可忽略。
pub fn workflow_name_from_script(script: &str) -> Option<String> {
    // 头部窗口截断:别切在 UTF-8 字符中间。
    let mut end = script.len().min(2048);
    while !script.is_char_boundary(end) {
        end -= 1;
    }
    let head = &script[..end];
    let meta_pos = head.find("export const meta")?;
    let body_start = meta_pos + head[meta_pos..].find('{')?;
    let body = &head[body_start + 1..];

    let mut from = 0;
    while let Some(rel) = body[from..].find("name") {
        let idx = from + rel;
        from = idx + "name".len();
        // 键边界:`name` 前一字符须是 `{`/`,`/空白(排除 filename/rename 这类子串命中)。
        let prev_ok = idx == 0
            || body[..idx]
                .chars()
                .next_back()
                .is_some_and(|c| c == '{' || c == ',' || c.is_whitespace());
        if !prev_ok {
            continue;
        }
        let rest = body[from..].trim_start();
        let Some(rest) = rest.strip_prefix(':') else {
            continue;
        };
        let rest = rest.trim_start();
        let mut chars = rest.chars();
        let quote = match chars.next() {
            Some(q @ ('\'' | '"' | '`')) => q,
            // 键匹配上了但值不是字符串字面量 → 格式变了,放弃而不是继续瞎猜。
            _ => return None,
        };
        let val: String = chars.take_while(|&c| c != quote).collect();
        let val = val.trim();
        if val.is_empty() || val.len() > 200 || val.contains('\n') {
            return None;
        }
        return Some(val.to_string());
    }
    None
}

/// 从 workflow 脚本头部解析 `meta.phases` 的阶段总数(`phases: [ {…}, {…} ]` 的顶层元素数)。
/// 真机格式:`phases: [\n  { title: 'Probe', … },\n  { title: 'Verdict', … },\n]`。
/// 不引 JS 解析器:定位 meta 头部窗口内的 `phases` 键 → 第一个 `[` → 括号配平地数顶层 `{`。
/// 任何不确定(键缺失/括号不配平/窗口截断)→ None,渲染层不显示阶段数,不猜。
pub fn workflow_phases_from_script(script: &str) -> Option<usize> {
    let mut end = script.len().min(4096);
    while !script.is_char_boundary(end) {
        end -= 1;
    }
    let head = &script[..end];
    let meta_pos = head.find("export const meta")?;
    let after = &head[meta_pos..];
    let phases_pos = after.find("phases")?;
    let after = &after[phases_pos..];
    let bracket = after.find('[')?;
    let body = &after[bracket + 1..];
    let mut depth_sq = 1i32; // 已进入 phases 数组
    let mut depth_curly = 0i32;
    let mut count = 0usize;
    let mut in_str: Option<char> = None;
    for c in body.chars() {
        if let Some(q) = in_str {
            if c == q {
                in_str = None;
            }
            continue;
        }
        match c {
            '\'' | '"' | '`' => in_str = Some(c),
            '[' => depth_sq += 1,
            ']' => {
                depth_sq -= 1;
                if depth_sq == 0 {
                    return (count > 0).then_some(count);
                }
            }
            '{' => {
                if depth_sq == 1 && depth_curly == 0 {
                    count += 1;
                }
                depth_curly += 1;
            }
            '}' => depth_curly -= 1,
            _ => {}
        }
    }
    None // 窗口内没扫到闭合 `]`,不确定 → 不猜
}

/// 从会话目录 `workflows/scripts/` 下的脚本文件名反解 workflow 名。
/// 文件名固定形如 `<name>-<wf目录名>.js`(如 `code-review-wf_41453ba1-ebc.js`,wf 目录名即
/// `subagents/workflows/` 下的 `wf_<runId>`)。对不上返回 None。
pub fn workflow_name_from_script_filename(file_name: &str, wf_dir_name: &str) -> Option<String> {
    let name = file_name
        .strip_suffix(".js")?
        .strip_suffix(wf_dir_name)?
        .strip_suffix('-')?;
    (!name.is_empty()).then(|| name.to_string())
}

/// journal.jsonl 的 started/result 配对计数。
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct JournalCounts {
    pub started: usize,
    pub done: usize,
}

/// 数 journal.jsonl 内容里的 `{"type":"started"}` / `{"type":"result"}` 事件。
/// 坏行跳过(append-only 流水可能有半写尾行)。
pub fn journal_counts(content: &str) -> JournalCounts {
    #[derive(Deserialize)]
    struct Line {
        #[serde(rename = "type")]
        line_type: String,
    }
    let mut c = JournalCounts::default();
    for line in content.lines() {
        let Ok(l) = serde_json::from_str::<Line>(line) else {
            continue;
        };
        match l.line_type.as_str() {
            "started" => c.started += 1,
            "result" => c.done += 1,
            _ => {}
        }
    }
    c
}

/// SendMessage 事件(assistant tool_use):父会话给某 subagent 续消息/追加消息。
/// `summary` 是该 subagent **最新的**活动标签(TUI 也用它),卡片按 owner 裁决取最新名。
#[derive(Debug, Clone, PartialEq)]
pub struct SendMessageEvent {
    pub to: String,
    pub summary: Option<String>,
    pub at: Option<DateTime<Utc>>,
}

/// 从转录 JSONL 行检测 SendMessage tool_use。目标取 `input.to`(缺则 `input.recipient`,
/// 真机两者都写)。目标可能是 teammate 名等非 subagent id——匹配责任在状态机(未知 id no-op)。
pub fn detect_send_message(line: &str) -> Option<SendMessageEvent> {
    let parsed: TranscriptLine = serde_json::from_str(line).ok()?;
    if parsed.line_type != "assistant" {
        return None;
    }
    let at = parsed.at();
    let msg = parsed.message?;
    let blocks = msg.content.as_array()?;
    for block in blocks {
        let Ok(cb) = serde_json::from_value::<ContentBlock>(block.clone()) else {
            continue;
        };
        if cb.block_type != "tool_use" || cb.name.as_deref() != Some("SendMessage") {
            continue;
        }
        let input = cb.input?;
        let to = input
            .get("to")
            .or_else(|| input.get("recipient"))
            .and_then(|v| v.as_str())?
            .to_string();
        let summary = input
            .get("summary")
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
            .map(str::to_string);
        return Some(SendMessageEvent { to, summary, at });
    }
    None
}

/// SendMessage 的续用确认(user 行 toolUseResult)。真机 2.1.229/231 三种形态:
/// - Form A:`Agent "X" was stopped (completed); resumed it in the background …` + `resumedAgentId`
/// - Form B:`Agent "X" had no active task; resumed from transcript …` + `resumedAgentId`
/// - Form C:`Message queued for delivery to X at its next tool round.` —— **没有 resumedAgentId**,
///   锚是 `pin.id`;之后的实际投递(对已终态 agent = 复活)不再留任何转录记录,真机验证过,
///   所以 queued 就是能拿到的最后一个结构化信号。
#[derive(Debug, Clone, PartialEq)]
pub struct ResumeEvent {
    pub agent_id: String,
    /// Form C(仅入队,复活发生在之后的静默投递)。状态机对两者同一处理:已终态 → 激活。
    pub queued: bool,
    pub at: Option<DateTime<Utc>>,
}

/// 从转录 JSONL 行检测续用确认。Form C 的判据刻意**不依赖英文文案**:success + pin.id 且无
/// resumedAgentId/status——发给还在跑的 agent 或 teammate 也长这样,但状态机只对「已知且已
/// 终态」的 id 激活,误报天然无害。
pub fn detect_resume(line: &str) -> Option<ResumeEvent> {
    let parsed: TranscriptLine = serde_json::from_str(line).ok()?;
    if parsed.line_type != "user" {
        return None;
    }
    let at = parsed.at();
    let tur = parsed.tool_use_result?;
    if let Some(rid) = tur.resumed_agent_id {
        return Some(ResumeEvent {
            agent_id: rid,
            queued: false,
            at,
        });
    }
    if tur.success == Some(true) && tur.status.is_empty() {
        if let Some(pin_id) = tur.pin.and_then(|p| p.id) {
            return Some(ResumeEvent {
                agent_id: pin_id,
                queued: true,
                at,
            });
        }
    }
    None
}

/// 从转录 JSONL 行检测启动确认(顶层 Agent 的 async_launched / Workflow 的 local_workflow)。
pub fn detect_launch(line: &str) -> Option<LaunchEvent> {
    let parsed: TranscriptLine = serde_json::from_str(line).ok()?;
    if parsed.line_type != "user" {
        return None;
    }
    let at = parsed.at();
    // 尝试从 message.content 里找 tool_use_id
    let tool_use_id = parsed
        .message
        .as_ref()
        .and_then(|m| m.content.as_array().cloned())
        .and_then(|arr| {
            arr.into_iter().find_map(|block| {
                let cb: ContentBlock = serde_json::from_value(block).ok()?;
                if cb.block_type == "tool_result" {
                    cb.tool_use_id
                } else {
                    None
                }
            })
        })
        .unwrap_or_default();
    let tur = parsed.tool_use_result?;
    if tur.status != "async_launched" {
        return None;
    }
    // Workflow 形态:taskType=local_workflow,**没有 agentId**(isAsync 也可能缺席)。
    if tur.task_type.as_deref() == Some("local_workflow") {
        return Some(LaunchEvent::Workflow {
            task_id: tur.task_id.unwrap_or_default(),
            run_id: tur.run_id.unwrap_or_default(),
            name: tur.workflow_name.unwrap_or_else(|| "workflow".to_string()),
            tool_use_id,
            at,
        });
    }
    if tur.is_async != Some(true) {
        return None;
    }
    let agent_id = tur.agent_id?;
    Some(LaunchEvent::Agent {
        agent_id,
        description: tur.description.unwrap_or_default(),
        tool_use_id,
        model: tur.resolved_model,
        at,
    })
}

/// 同步 Agent 完成事件(同步没有 async_launched,tool_result 即终态)。
#[derive(Debug, Clone, PartialEq)]
pub struct SyncCompletedEvent {
    pub agent_id: String,
    pub description: String,
    pub model: Option<String>,
    pub at: Option<DateTime<Utc>>,
}

/// 从转录 JSONL 行检测同步 subagent 已完成。
pub fn detect_sync_completed(line: &str) -> Option<SyncCompletedEvent> {
    let parsed: TranscriptLine = serde_json::from_str(line).ok()?;
    if parsed.line_type != "user" {
        return None;
    }
    let at = parsed.at();
    let tur = parsed.tool_use_result?;
    // 同步:isAsync 不存在(或 false)且 status == "completed"
    if tur.is_async.unwrap_or(false) {
        return None;
    }
    if tur.status != "completed" {
        return None;
    }
    let agent_id = tur.agent_id?;
    Some(SyncCompletedEvent {
        agent_id,
        description: tur.description.unwrap_or_default(),
        model: tur.resolved_model,
        at,
    })
}

/// 从转录 JSONL 行检测 task-notification(异步 subagent / workflow 完成/失败)。
/// 返回 (task_id, status, summary, at)。task_id 可能对应 agentId、workflow taskId,
/// 也可能是后台 shell 任务 —— 匹配责任在状态机(未知 id 不建条目)。
pub fn detect_task_notification(
    line: &str,
) -> Option<(String, String, String, Option<DateTime<Utc>>)> {
    let parsed: TranscriptLine = serde_json::from_str(line).ok()?;
    let at = parsed.at();

    // 形式 A: type=queue-operation, operation=enqueue
    if parsed.line_type == "queue-operation" && parsed.operation.as_deref() == Some("enqueue") {
        let content = parsed.content?;
        let text = content.as_str()?;
        return parse_task_notification_xml(text).map(|(a, s, m)| (a, s, m, at));
    }

    // 形式 B: type=user, origin.kind="task-notification"
    if parsed.line_type == "user"
        && parsed.origin.as_ref().and_then(|o| o.kind.as_deref()) == Some("task-notification")
    {
        let msg = parsed.message?;
        let text = msg.content.as_str()?;
        return parse_task_notification_xml(text).map(|(a, s, m)| (a, s, m, at));
    }

    None
}

/// 从转录 JSONL 行检测 turn_duration 权威计数(`type=system, subtype=turn_duration`)。
/// `pendingBackgroundAgentCount`/`pendingWorkflowCount` 字段缺席时为 None(旧版本转录)。
pub fn detect_turn_duration(line: &str) -> Option<TurnAuthority> {
    let parsed: TranscriptLine = serde_json::from_str(line).ok()?;
    if parsed.line_type != "system" || parsed.subtype.as_deref() != Some("turn_duration") {
        return None;
    }
    Some(TurnAuthority {
        at: parsed.at()?,
        pending_background: parsed.pending_background_agent_count,
        pending_workflows: parsed.pending_workflow_count,
    })
}

/// 解析 `<task-notification>` XML 文本。
/// 返回 (task_id, status, summary)。
fn parse_task_notification_xml(xml: &str) -> Option<(String, String, String)> {
    if !xml.contains("<task-notification>") {
        return None;
    }
    let task_id = extract_xml_tag(xml, "task-id")?;
    let status = extract_xml_tag(xml, "status").unwrap_or_else(|| "unknown".to_string());
    let summary = extract_xml_tag(xml, "summary").unwrap_or_default();
    Some((task_id, status, summary))
}

/// 从 XML 文本中提取指定标签的内容(简单字符串解析,不引入 XML crate)。
fn extract_xml_tag(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&close)?;
    Some(xml[start..start + end].trim().to_string())
}

// ── subagent 文件状态判定 ─────────────────────────────────

/// 从 subagent 的 .jsonl 最后一行判断是否已完成。
/// `last_line` 是 `agent-<id>.jsonl` 的最后一行 JSON。
pub fn subagent_completed_from_last_line(last_line: &str) -> bool {
    let parsed: TranscriptLine = match serde_json::from_str(last_line) {
        Ok(p) => p,
        Err(_) => return false,
    };
    if parsed.line_type != "assistant" {
        return false;
    }
    if parsed.message.is_none() {
        return false;
    }
    // 检查 message.stop_reason == "end_turn"(从原始 JSON 取,因为最小结构没包含)
    if let Ok(raw) = serde_json::from_str::<serde_json::Value>(last_line) {
        if let Some(sr) = raw
            .get("message")
            .and_then(|m| m.get("stop_reason"))
            .and_then(|v| v.as_str())
        {
            return sr == "end_turn";
        }
    }
    false
}

/// 从 `agent-<id>.meta.json` 内容解析 DirSubagent 条目。
/// `completed` / `started_at` / `last_activity` 由调用方(IO 层)从对应 .jsonl 判定后传入。
///
/// workflow 扇出的子代理 meta 实测形如 `{"agentType":"workflow-subagent","spawnDepth":1,
/// "model":"opus"}`——没有 description/toolUseId、但有 model。描述缺失时回落
/// `fallback_description`(由调用方按归属生成,如「code-review #2」/「workflow 子代理 #2」/
/// 顶层的「(无描述)」),渲染层不再自造占位文案。
pub fn parse_subagent_meta(
    meta_json: &str,
    agent_id: &str,
    completed: bool,
    started_at: DateTime<Utc>,
    last_activity: Option<DateTime<Utc>>,
    fallback_description: &str,
) -> Option<DirSubagent> {
    #[derive(Deserialize)]
    struct Meta {
        #[serde(rename = "agentType")]
        agent_type: Option<String>,
        description: Option<String>,
        #[serde(rename = "toolUseId")]
        tool_use_id: Option<String>,
        model: Option<String>,
    }
    let meta: Meta = serde_json::from_str(meta_json).ok()?;
    Some(DirSubagent {
        agent_id: agent_id.to_string(),
        description: meta
            .description
            .filter(|d| !d.trim().is_empty())
            .unwrap_or_else(|| fallback_description.to_string()),
        agent_type: meta.agent_type.unwrap_or_default(),
        tool_use_id: meta.tool_use_id.unwrap_or_default(),
        completed,
        started_at,
        last_activity,
        model: meta.model.filter(|m| !m.trim().is_empty()),
    })
}

/// 从文件名 `agent-<id>.meta.json` 提取 agent_id。
pub fn agent_id_from_meta_filename(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_str()?;
    name.strip_prefix("agent-")
        .and_then(|s| s.strip_suffix(".meta.json"))
        .map(|s| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t0() -> DateTime<Utc> {
        use chrono::TimeZone;
        Utc.with_ymd_and_hms(2026, 8, 13, 12, 0, 0)
            .single()
            .unwrap()
    }

    /// 按派出调用 id 问结束与否:在跑的条目带着调用 id;结束的条目只剩 agentId,要靠调用方
    /// 记下的 agentId 认。两边都认不出就不下结论。
    #[test]
    fn finished_for_answers_by_tool_use_id_then_by_agent_id() {
        let mut st = SubagentState::new();
        st.insert_spawning(
            "审一下".into(),
            "reviewer".into(),
            "toolu_a".into(),
            None,
            t0(),
        );
        assert_eq!(st.finished_for("toolu_a", None), Some(false));
        assert_eq!(st.agent_id_for("toolu_a"), None, "Spawning 还没有 agentId");

        st.promote_to_running("agent_a", "审一下", "toolu_a", None, t0());
        assert_eq!(st.finished_for("toolu_a", None), Some(false));
        assert_eq!(st.agent_id_for("toolu_a"), Some("agent_a"));

        st.mark_completed("agent_a", t0());
        assert_eq!(
            st.finished_for("toolu_a", None),
            None,
            "终态条目不带调用 id,光凭它认不出"
        );
        assert_eq!(st.finished_for("toolu_a", Some("agent_a")), Some(true));
        assert_eq!(st.finished_for("toolu_other", Some("agent_other")), None);

        st.promote_to_running("agent_b", "再审", "toolu_b", None, t0());
        st.mark_failed("agent_b", "boom", t0());
        assert_eq!(st.finished_for("toolu_b", Some("agent_b")), Some(true));
    }

    /// 同一派出调用 id 可能残留一条 Spawning:重放时终态条目已不带调用 id,`insert_spawning`
    /// 的去重拦不住它。按调用 id 先认非 Spawning 的条目;只认得出 Spawning 时再按 agentId 查。
    #[test]
    fn finished_for_looks_past_a_leftover_spawning_entry() {
        let mut st = SubagentState::new();
        st.promote_to_running("agent_a", "审一下", "toolu_a", None, t0());
        st.mark_completed("agent_a", t0());
        assert!(st.insert_spawning(
            "审一下".into(),
            "reviewer".into(),
            "toolu_a".into(),
            None,
            t0(),
        ));
        assert_eq!(st.finished_for("toolu_a", Some("agent_a")), Some(true));
        assert_eq!(
            st.finished_for("toolu_a", None),
            Some(false),
            "只认得出 Spawning 就是还没结束"
        );

        let spawning = SubagentStatus::Spawning {
            description: "再审".into(),
            agent_type: "reviewer".into(),
            tool_use_id: "toolu_b".into(),
            detected_at: t0(),
            model: None,
        };
        st.agents.push(spawning);
        st.promote_to_running("agent_b", "再审", "toolu_other", None, t0());
        if let Some(SubagentStatus::Running { tool_use_id, .. }) = st.agents.last_mut() {
            *tool_use_id = "toolu_b".into();
        }
        assert_eq!(
            st.agent_id_for("toolu_b"),
            Some("agent_b"),
            "Spawning 在前也要认出在跑条目的 agentId"
        );
        assert_eq!(st.finished_for("toolu_b", None), Some(false));
        st.mark_completed("agent_b", t0());
        assert_eq!(st.finished_for("toolu_b", Some("agent_b")), Some(true));
    }

    // ── detect_spawn ──

    #[test]
    fn detect_spawn_async_agent() {
        let line = r#"{
            "type": "assistant",
            "timestamp": "2026-08-13T12:00:00.000Z",
            "message": {
                "role": "assistant",
                "content": [{
                    "type": "tool_use",
                    "id": "toolu_01S9KerHThJTibukXi2uWgBb",
                    "name": "Agent",
                    "input": {
                        "description": "speechsdk Flute 桥接攻坚",
                        "subagent_type": "general-purpose",
                        "run_in_background": true,
                        "model": "opus",
                        "prompt": "..."
                    }
                }]
            }
        }"#;
        let ev = detect_spawn(line).expect("应检测到 spawn");
        assert_eq!(ev.tool_use_id, "toolu_01S9KerHThJTibukXi2uWgBb");
        assert_eq!(ev.description, "speechsdk Flute 桥接攻坚");
        assert_eq!(ev.agent_type, "general-purpose");
        assert!(ev.is_async);
        assert_eq!(ev.model.as_deref(), Some("opus"));
        assert_eq!(ev.at, Some(t0()), "spawn 事件必须带行时间戳");
    }

    #[test]
    fn detect_spawn_sync_agent_without_model() {
        let line = r#"{
            "type": "assistant",
            "message": {
                "role": "assistant",
                "content": [{
                    "type": "tool_use",
                    "id": "toolu_sync123",
                    "name": "Agent",
                    "input": {
                        "description": "同步任务",
                        "subagent_type": "Explore",
                        "prompt": "..."
                    }
                }]
            }
        }"#;
        let ev = detect_spawn(line).expect("应检测到 spawn");
        assert_eq!(ev.description, "同步任务");
        assert_eq!(ev.agent_type, "Explore");
        assert!(!ev.is_async);
        assert!(ev.model.is_none(), "input 没写 model 就不猜");
        assert!(ev.at.is_none(), "没有 timestamp 字段 → None(调用方回落)");
    }

    #[test]
    fn detect_spawn_workflow() {
        let line = r#"{
            "type": "assistant",
            "message": {
                "role": "assistant",
                "content": [{
                    "type": "tool_use",
                    "id": "toolu_wf123",
                    "name": "Workflow",
                    "input": {
                        "name": "deep-research",
                        "args": "WWDC 2026"
                    }
                }]
            }
        }"#;
        let ev = detect_spawn(line).expect("应检测到 workflow spawn");
        assert_eq!(ev.description, "workflow: deep-research");
        assert_eq!(ev.agent_type, "workflow:deep-research");
        assert!(ev.is_async); // Workflow 总是异步
    }

    #[test]
    fn detect_spawn_ignores_non_agent() {
        let line = r#"{
            "type": "assistant",
            "message": {
                "content": [{
                    "type": "tool_use",
                    "id": "toolu_bash123",
                    "name": "Bash",
                    "input": {"command": "ls"}
                }]
            }
        }"#;
        assert!(detect_spawn(line).is_none());
    }

    #[test]
    fn detect_spawn_ignores_user_lines() {
        let line = r#"{"type": "user", "message": {"content": "hello"}}"#;
        assert!(detect_spawn(line).is_none());
    }

    // ── detect_launch ──

    #[test]
    fn detect_launch_agent_with_resolved_model() {
        let line = r#"{
            "type": "user",
            "timestamp": "2026-08-13T23:18:27.369Z",
            "toolUseResult": {
                "isAsync": true,
                "status": "async_launched",
                "agentId": "a3859a255bbe30bb5",
                "description": "speechsdk Flute 桥接攻坚",
                "resolvedModel": "claude-fable-5"
            },
            "message": {
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": "toolu_01S9KerHThJTibukXi2uWgBb",
                    "content": [{"type": "text", "text": "Async agent launched successfully."}]
                }]
            }
        }"#;
        match detect_launch(line).expect("应检测到 launch") {
            LaunchEvent::Agent {
                agent_id,
                description,
                tool_use_id,
                model,
                at,
            } => {
                assert_eq!(agent_id, "a3859a255bbe30bb5");
                assert_eq!(description, "speechsdk Flute 桥接攻坚");
                assert_eq!(tool_use_id, "toolu_01S9KerHThJTibukXi2uWgBb");
                assert_eq!(model.as_deref(), Some("claude-fable-5"));
                assert!(at.is_some());
            }
            other => panic!("应是 Agent 形态: {other:?}"),
        }
    }

    /// 真机 2.1.229 实测:Workflow 的 async_launched **没有 agentId**,只有 taskId/runId/
    /// workflowName。旧实现要求 agentId → 返回 None → workflow 永远停在 Spawning(真机
    /// 事故根因之一)。
    #[test]
    fn detect_launch_workflow_task_id_form() {
        let line = r#"{
            "type": "user",
            "timestamp": "2026-08-13T19:00:22.899Z",
            "toolUseResult": {
                "status": "async_launched",
                "taskId": "w7rpwxkrx",
                "taskType": "local_workflow",
                "workflowName": "slash-passthrough-e2e-probe",
                "runId": "wf_0c2a0924-326",
                "summary": "透传真机证据补全"
            },
            "message": {
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": "toolu_01L6dyDCQ4cFDPL831HcarpW",
                    "content": [{"type": "text", "text": "Workflow launched."}]
                }]
            }
        }"#;
        match detect_launch(line).expect("workflow launch 必须被识别") {
            LaunchEvent::Workflow {
                task_id,
                run_id,
                name,
                tool_use_id,
                ..
            } => {
                assert_eq!(task_id, "w7rpwxkrx");
                assert_eq!(run_id, "wf_0c2a0924-326");
                assert_eq!(name, "slash-passthrough-e2e-probe");
                assert_eq!(tool_use_id, "toolu_01L6dyDCQ4cFDPL831HcarpW");
            }
            other => panic!("应是 Workflow 形态: {other:?}"),
        }
    }

    #[test]
    fn detect_launch_ignores_sync() {
        let line = r#"{
            "type": "user",
            "toolUseResult": {
                "status": "completed",
                "agentId": "sync123"
            }
        }"#;
        assert!(detect_launch(line).is_none());
    }

    // ── detect_sync_completed ──

    #[test]
    fn detect_sync_completed_ok() {
        let line = r#"{
            "type": "user",
            "toolUseResult": {
                "status": "completed",
                "agentId": "ac1082ee5160655da",
                "agentType": "general-purpose",
                "resolvedModel": "claude-fable-5",
                "totalDurationMs": 227577
            }
        }"#;
        let ev = detect_sync_completed(line).expect("应检测到同步完成");
        assert_eq!(ev.agent_id, "ac1082ee5160655da");
        assert_eq!(ev.model.as_deref(), Some("claude-fable-5"));
    }

    #[test]
    fn detect_sync_completed_ignores_async() {
        let line = r#"{
            "type": "user",
            "toolUseResult": {
                "isAsync": true,
                "status": "async_launched",
                "agentId": "async123"
            }
        }"#;
        assert!(detect_sync_completed(line).is_none());
    }

    // ── detect_task_notification ──

    #[test]
    fn detect_task_notification_queue_operation() {
        let line = r#"{
            "type": "queue-operation",
            "operation": "enqueue",
            "timestamp": "2026-08-13T23:21:32.739Z",
            "content": "<task-notification>\n<task-id>a3859a255bbe30bb5</task-id>\n<status>completed</status>\n<summary>Background agent finished</summary>\n</task-notification>"
        }"#;
        let (aid, status, summary, at) =
            detect_task_notification(line).expect("应检测到 task-notification");
        assert_eq!(aid, "a3859a255bbe30bb5");
        assert_eq!(status, "completed");
        assert_eq!(summary, "Background agent finished");
        assert!(at.is_some(), "通知行必须带事件时间戳(重放历史用)");
    }

    #[test]
    fn detect_task_notification_user_origin() {
        let line = r#"{
            "type": "user",
            "origin": {"kind": "task-notification"},
            "message": {
                "role": "user",
                "content": "<task-notification>\n<task-id>bs5oknztc</task-id>\n<status>failed</status>\n<summary>Background command failed</summary>\n</task-notification>"
            }
        }"#;
        let (aid, status, summary, _) =
            detect_task_notification(line).expect("应检测到 task-notification");
        assert_eq!(aid, "bs5oknztc");
        assert_eq!(status, "failed");
        assert_eq!(summary, "Background command failed");
    }

    #[test]
    fn detect_task_notification_ignores_normal() {
        let line = r#"{"type": "user", "message": {"content": "hello"}}"#;
        assert!(detect_task_notification(line).is_none());
    }

    // ── detect_turn_duration ──

    #[test]
    fn detect_turn_duration_authority_counts() {
        let line = r#"{"type":"system","subtype":"turn_duration","durationMs":77162,
            "pendingBackgroundAgentCount":2,"pendingWorkflowCount":1,
            "timestamp":"2026-08-13T23:18:45.682Z","sessionId":"186e4d8a"}"#;
        let auth = detect_turn_duration(line).expect("应检测到 turn_duration");
        assert_eq!(auth.pending_background, Some(2));
        assert_eq!(auth.pending_workflows, Some(1));
    }

    #[test]
    fn detect_turn_duration_missing_counts_is_none_not_zero() {
        let line = r#"{"type":"system","subtype":"turn_duration","durationMs":100,
            "timestamp":"2026-08-13T18:04:51.191Z"}"#;
        let auth = detect_turn_duration(line).expect("应检测到 turn_duration");
        assert_eq!(auth.pending_background, None, "字段缺席 ≠ 0,不猜");
        assert_eq!(auth.pending_workflows, None);
    }

    // ── subagent_completed_from_last_line ──

    #[test]
    fn subagent_completed_end_turn() {
        let line = r#"{
            "type": "assistant",
            "isSidechain": true,
            "agentId": "a3859a255bbe30bb5",
            "message": {
                "role": "assistant",
                "stop_reason": "end_turn",
                "content": [{"type": "text", "text": "All done."}]
            }
        }"#;
        assert!(subagent_completed_from_last_line(line));
    }

    #[test]
    fn subagent_not_completed_no_stop_reason() {
        let line = r#"{
            "type": "assistant",
            "isSidechain": true,
            "agentId": "abc123",
            "message": {
                "role": "assistant",
                "content": [{"type": "text", "text": "Still working..."}]
            }
        }"#;
        assert!(!subagent_completed_from_last_line(line));
    }

    // ── SubagentState 状态机 ──

    #[test]
    fn state_spawn_then_running_then_completed() {
        let mut state = SubagentState::new();
        let at = t0();
        assert!(state.insert_spawning(
            "测试任务".into(),
            "general-purpose".into(),
            "toolu_123".into(),
            Some("opus".into()),
            at,
        ));
        assert_eq!(state.agents.len(), 1);
        assert!(matches!(state.agents[0], SubagentStatus::Spawning { .. }));

        // 重复插入不生效
        assert!(!state.insert_spawning(
            "测试任务".into(),
            "general-purpose".into(),
            "toolu_123".into(),
            None,
            at,
        ));
        assert_eq!(state.agents.len(), 1);

        // 提升为 Running:resolvedModel 覆盖 spawn 时的 model
        assert!(state.promote_to_running(
            "agent_abc",
            "测试任务",
            "toolu_123",
            Some("claude-opus-5[1m]"),
            at,
        ));
        assert!(matches!(state.agents[0], SubagentStatus::Running { .. }));
        assert_eq!(state.agents[0].agent_id(), Some("agent_abc"));
        assert_eq!(state.agents[0].model(), Some("claude-opus-5[1m]"));

        // 标记完成:描述保留 spawn 时的原值(通知 summary 是套话,不覆盖)
        assert!(state.mark_completed("agent_abc", at + chrono::Duration::minutes(5)));
        assert!(matches!(state.agents[0], SubagentStatus::Completed { .. }));
        assert_eq!(state.agents[0].description(), "测试任务");
        assert_eq!(
            state.agents[0].model(),
            Some("claude-opus-5[1m]"),
            "终态保留 model"
        );

        // 重复标记不生效
        assert!(!state.mark_completed("agent_abc", at));
    }

    #[test]
    fn state_promote_running_without_prior_spawn() {
        let mut state = SubagentState::new();
        // 没看到 spawn 直接看到 async_launched —— 应该直接插 Running
        assert!(state.promote_to_running("agent_xyz", "直接启动的", "toolu_456", None, t0()));
        assert_eq!(state.agents.len(), 1);
        assert!(matches!(state.agents[0], SubagentStatus::Running { .. }));
        // 重放同一行不重复插
        assert!(!state.promote_to_running("agent_xyz", "直接启动的", "toolu_456", None, t0()));
        assert_eq!(state.agents.len(), 1);
    }

    #[test]
    fn state_promote_keeps_spawn_description_when_launch_has_none() {
        let mut state = SubagentState::new();
        state.insert_spawning(
            "有信息的描述".into(),
            "Explore".into(),
            "tu9".into(),
            None,
            t0(),
        );
        state.promote_to_running("a9", "", "tu9", None, t0());
        assert_eq!(state.agents[0].description(), "有信息的描述");
    }

    #[test]
    fn state_promote_keeps_spawn_model_when_launch_has_none() {
        let mut state = SubagentState::new();
        state.insert_spawning(
            "t".into(),
            "Explore".into(),
            "tu1".into(),
            Some("haiku".into()),
            t0(),
        );
        state.promote_to_running("a1", "t", "tu1", None, t0());
        assert_eq!(state.agents[0].model(), Some("haiku"));
    }

    #[test]
    fn state_mark_failed() {
        let mut state = SubagentState::new();
        state.promote_to_running("agent_fail", "会失败的", "toolu_789", None, t0());
        assert!(state.mark_failed("agent_fail", "超时了", t0()));
        assert!(matches!(state.agents[0], SubagentStatus::Failed { .. }));
    }

    /// task-notification 的 task-id 还覆盖后台 shell 任务:未知 id 不得建条目。
    #[test]
    fn unknown_task_id_creates_nothing() {
        let mut state = SubagentState::new();
        assert!(!state.mark_completed("bs5oknztc", t0()));
        assert!(!state.mark_failed("bs5oknztc", "Background command failed", t0()));
        assert!(state.agents.is_empty());
    }

    // ── SendMessage 续用(resume)语义 ──

    /// 真机形态(2026-08-14 采样):assistant tool_use SendMessage,input 同时带 to/recipient,
    /// summary 是最新活动标签。
    #[test]
    fn detect_send_message_real_shape() {
        let line = serde_json::json!({
            "type": "assistant",
            "timestamp": "2026-08-13T23:21:41.450Z",
            "message": {"content": [{
                "type": "tool_use",
                "id": "toolu_sm1",
                "name": "SendMessage",
                "input": {
                    "to": "afdcd579162d517eb",
                    "summary": "前台等验证跑完再交付",
                    "message": "继续:……",
                    "type": "message",
                    "recipient": "afdcd579162d517eb",
                    "content": "继续:……"
                }
            }]}
        })
        .to_string();
        let ev = detect_send_message(&line).expect("应检测到 SendMessage");
        assert_eq!(ev.to, "afdcd579162d517eb");
        assert_eq!(ev.summary.as_deref(), Some("前台等验证跑完再交付"));
        assert!(ev.at.is_some());
        // 非 SendMessage 的 tool_use 不误报
        assert!(detect_send_message(
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t","name":"Bash","input":{"command":"ls"}}]}}"#
        )
        .is_none());
    }

    /// 真机三形态:A(stopped→resumed)与 B(no active task→resumed)带 resumedAgentId;
    /// C(queued)只有 pin.id、无 resumedAgentId、无 status。带 status 的 toolUseResult
    /// (async_launched/completed)绝不能被误判成续用。
    #[test]
    fn detect_resume_forms() {
        let form_a = r#"{"type":"user","timestamp":"2026-08-13T23:21:41.463Z","toolUseResult":{
            "success": true,
            "message": "Agent \"afdcd579162d517eb\" was stopped (completed); resumed it in the background with your message.",
            "resumedAgentId": "afdcd579162d517eb",
            "pin": {"id": "afdcd579162d517eb", "name": "afdcd579162d517eb", "ref": "8d20a5"}}}"#;
        let ev = detect_resume(form_a).expect("Form A");
        assert_eq!(ev.agent_id, "afdcd579162d517eb");
        assert!(!ev.queued);

        let form_c = r#"{"type":"user","toolUseResult":{
            "success": true,
            "message": "Message queued for delivery to a42b155eb070664ea at its next tool round.",
            "pin": {"id": "a42b155eb070664ea", "name": "a42b155eb070664ea", "ref": "8ddfe3"}}}"#;
        let ev = detect_resume(form_c).expect("Form C");
        assert_eq!(ev.agent_id, "a42b155eb070664ea");
        assert!(ev.queued);

        // async_launched(有 status)不是续用
        let launched = r#"{"type":"user","toolUseResult":{
            "isAsync": true, "status": "async_launched", "agentId": "x1"}}"#;
        assert!(detect_resume(launched).is_none());
    }

    /// owner 裁决:completed 的 subagent 被 SendMessage 续起 → 重新激活为 Running,
    /// 且卡片显示**最新名字**(SendMessage summary)。
    #[test]
    fn resume_reactivates_finished_agent_with_latest_name() {
        let mut state = SubagentState::new();
        let t0 = Utc::now() - chrono::Duration::minutes(30);
        state.promote_to_running(
            "a1",
            "开放透传命令有参形态",
            "tu1",
            Some("claude-fable-5"),
            t0,
        );
        state.mark_completed("a1", t0 + chrono::Duration::minutes(10));

        // 改名(SendMessage summary)先于续用确认到达(真机行序)
        assert!(state.rename("a1", "前台等验证跑完再交付"));
        assert!(state.reactivate("a1", Utc::now()));
        let display = state.display(Utc::now());
        assert_eq!(display.agents.len(), 1);
        assert!(matches!(display.agents[0], SubagentStatus::Running { .. }));
        assert_eq!(display.agents[0].description(), "前台等验证跑完再交付");
        assert_eq!(
            display.agents[0].model(),
            Some("claude-fable-5"),
            "model 保留"
        );

        // 续起后再次完成:照常收敛
        assert!(state.mark_completed("a1", Utc::now()));
        // 又一条 queued(Form C)→ 再次复活;在跑时的续用信号 no-op
        assert!(state.reactivate("a1", Utc::now()));
        assert!(!state.reactivate("a1", Utc::now()));
        // 未知 id(teammate/后台任务)no-op,不建条目
        assert!(!state.rename("teammate@team", "别人的名字"));
        assert!(!state.reactivate("unknown_id", Utc::now()));
        assert_eq!(state.agents.len(), 1);
    }

    /// 复活不能被上一段的完成证据杀掉:目录 jsonl 末行还是旧 end_turn(mtime 早于本次激活)
    /// 时,reconcile 不得把刚续起的 Running 翻回 Completed。
    #[test]
    fn dir_completion_evidence_from_previous_segment_does_not_kill_reactivation() {
        let mut state = SubagentState::new();
        let t0 = Utc::now() - chrono::Duration::hours(1);
        state.promote_to_running("a1", "任务", "tu1", None, t0);
        state.mark_completed("a1", t0 + chrono::Duration::minutes(5));
        state.reactivate("a1", Utc::now());

        let stale_mtime = t0 + chrono::Duration::minutes(5);
        let entry = DirSubagent {
            agent_id: "a1".into(),
            description: "任务".into(),
            agent_type: "general-purpose".into(),
            tool_use_id: "tu1".into(),
            completed: true, // 上一段留下的 end_turn
            started_at: t0,
            last_activity: Some(stale_mtime),
            model: None,
        };
        state.reconcile_from_dir(vec![entry.clone()]);
        assert!(
            matches!(state.agents[0], SubagentStatus::Running { .. }),
            "旧段完成证据不得终结新激活: {:?}",
            state.agents[0]
        );
        // 旧 mtime 回填不把活跃锚拖回过去(anchor 取 max(started_at, last_activity))
        assert_eq!(
            state.display(Utc::now()).agents.len(),
            1,
            "刚续起的必须可见"
        );

        // 新一段真正完成(mtime 晚于激活)→ 照常翻转
        let fresh = DirSubagent {
            completed: true,
            last_activity: Some(Utc::now() + chrono::Duration::seconds(1)),
            ..entry
        };
        state.reconcile_from_dir(vec![fresh]);
        assert!(matches!(state.agents[0], SubagentStatus::Completed { .. }));
    }

    // ── 历史重放正确性(真机事故的核心) ──

    /// 重放历史转录:几小时前完成/失败的子代理,展示层必须不可见——它们带的是**事件时刻**,
    /// 不是「watcher 看到的时刻」。旧实现用 Utc::now() 盖时间戳,重放后全部渲成「刚刚」。
    #[test]
    fn replayed_history_is_not_displayed_as_fresh() {
        let mut state = SubagentState::new();
        let ancient = Utc::now() - chrono::Duration::hours(10);
        state.promote_to_running("a_old", "十小时前的任务", "tu_old", None, ancient);
        state.mark_completed("a_old", ancient + chrono::Duration::minutes(20));
        state.promote_to_running("a_fail", "十小时前失败的", "tu_fail", None, ancient);
        state.mark_failed("a_fail", "崩了", ancient + chrono::Duration::minutes(5));
        // 被打断、永远没有完成通知的历史 Running:活跃锚点(started_at)已超时效窗。
        state.promote_to_running("a_orphan", "被打断的孤儿", "tu_orphan", None, ancient);

        let display = state.display(Utc::now());
        assert!(
            display.agents.is_empty(),
            "历史重放不得出现在展示层: {display:?}"
        );

        // 而「此刻」的事件照常展示。
        state.promote_to_running("a_now", "现在在跑的", "tu_now", None, Utc::now());
        let display = state.display(Utc::now());
        assert_eq!(display.agents.len(), 1);
        assert_eq!(display.agents[0].description(), "现在在跑的");
    }

    /// 复活循环回归:终态条目**软删**——目录重扫不得把已完成条目再度带回展示。
    /// 旧实现 prune 物理删除 + 5s 重扫按 now 复活,历史完成条目永远在面板上循环出现。
    #[test]
    fn dir_rescan_does_not_resurrect_finished_entries() {
        let mut state = SubagentState::new();
        let done_at = Utc::now() - chrono::Duration::minutes(10);
        let entry = DirSubagent {
            agent_id: "done1".into(),
            description: "早就完成的".into(),
            agent_type: "general-purpose".into(),
            tool_use_id: "tu1".into(),
            completed: true,
            started_at: done_at - chrono::Duration::minutes(30),
            last_activity: Some(done_at),
            model: None,
        };
        // 第一次扫描:建 Completed,finished_at = jsonl mtime(不是 now)→ 已超 30s 展示窗
        assert!(state.reconcile_from_dir(vec![entry.clone()]));
        assert!(
            state.display(Utc::now()).agents.is_empty(),
            "10 分钟前完成的不展示"
        );
        // 后续每 5s 重扫:恒 no-op,不复活、不刷新 last_changed
        assert!(!state.reconcile_from_dir(vec![entry.clone()]));
        assert!(!state.reconcile_from_dir(vec![entry]));
        assert!(state.display(Utc::now()).agents.is_empty());
    }

    /// 刚完成的(30s 内)短暂可见——「刚跑完」的反馈保留。
    #[test]
    fn freshly_completed_shows_briefly() {
        let mut state = SubagentState::new();
        state.promote_to_running(
            "a1",
            "刚完成的",
            "tu1",
            None,
            Utc::now() - chrono::Duration::minutes(3),
        );
        state.mark_completed("a1", Utc::now());
        let display = state.display(Utc::now());
        assert_eq!(display.agents.len(), 1);
        assert!(matches!(
            display.agents[0],
            SubagentStatus::Completed { .. }
        ));
    }

    /// Failed 展示窗:活失败可见(30min),历史失败不可见。
    #[test]
    fn failed_display_window() {
        let now = Utc::now();
        let fresh_fail = SubagentStatus::Failed {
            agent_id: "f1".into(),
            description: "刚失败".into(),
            error: "e".into(),
            finished_at: now - chrono::Duration::minutes(5),
            model: None,
        };
        let old_fail = SubagentStatus::Failed {
            agent_id: "f2".into(),
            description: "昨天失败".into(),
            error: "e".into(),
            finished_at: now - chrono::Duration::hours(20),
            model: None,
        };
        assert!(status_displayable(&fresh_fail, now));
        assert!(!status_displayable(&old_fail, now));
    }

    #[test]
    fn state_reconcile_from_dir() {
        let mut state = SubagentState::new();
        state.promote_to_running("known", "已知的", "tuid_known", None, Utc::now());

        let dir_entries = vec![
            DirSubagent {
                agent_id: "known".into(),
                description: "目录里的已知任务".into(),
                agent_type: "Explore".into(),
                tool_use_id: "tuid_known".into(),
                completed: false,
                started_at: Utc::now(),
                last_activity: None,
                model: None,
            },
            DirSubagent {
                agent_id: "orphan".into(),
                description: "目录里的孤儿任务".into(),
                agent_type: "general-purpose".into(),
                tool_use_id: "tuid_orphan".into(),
                completed: true,
                started_at: Utc::now(),
                last_activity: Some(Utc::now()),
                model: Some("opus".into()),
            },
        ];
        let changed = state.reconcile_from_dir(dir_entries);

        // 已知的不被覆盖(仍是 Running,描述不变)
        assert!(changed);
        assert_eq!(state.agents[0].description(), "已知的");
        // 孤儿被加进来,model 采自 meta
        assert_eq!(state.agents.len(), 2);
        assert!(matches!(state.agents[1], SubagentStatus::Completed { .. }));
        assert_eq!(state.agents[1].model(), Some("opus"));
    }

    #[test]
    fn state_reconcile_marks_existing_completed_from_dir_with_mtime() {
        let mut state = SubagentState::new();
        // 启动时刻早于完成证据 mtime(真实时序恒如此;mtime 早于激活的是上一段证据,
        // 由 dir_completion_evidence_from_previous_segment_does_not_kill_reactivation 钉)。
        state.promote_to_running(
            "known",
            "已知的",
            "tuid_known",
            None,
            Utc::now() - chrono::Duration::minutes(9),
        );

        let mtime = Utc::now() - chrono::Duration::minutes(2);
        let changed = state.reconcile_from_dir(vec![DirSubagent {
            agent_id: "known".into(),
            description: "目录里的已知任务".into(),
            agent_type: "Explore".into(),
            tool_use_id: "tuid_known".into(),
            completed: true,
            started_at: Utc::now() - chrono::Duration::minutes(9),
            last_activity: Some(mtime),
            model: None,
        }]);

        assert!(changed);
        match &state.agents[0] {
            SubagentStatus::Completed {
                finished_at,
                description,
                ..
            } => {
                assert_eq!(description, "已知的");
                assert_eq!(*finished_at, mtime, "finished_at 取 jsonl mtime,不取 now");
            }
            other => panic!("应为 Completed: {other:?}"),
        }
    }

    // ── Workflow run 生命周期 ──

    #[test]
    fn workflow_spawn_launch_notify_lifecycle() {
        let mut state = SubagentState::new();
        let at = Utc::now();
        // spawn(tool_use)→ Spawning 占位
        state.insert_spawning(
            "workflow: e2e-probe".into(),
            "workflow:e2e-probe".into(),
            "toolu_wf".into(),
            None,
            at,
        );
        // 启动确认(taskId/runId 形态)→ Spawning 占位移除,登记 WorkflowRun
        assert!(state.workflow_launched(
            "w7rpwxkrx",
            "wf_0c2a0924-326",
            "e2e-probe",
            "toolu_wf",
            at
        ));
        assert!(state.agents.is_empty(), "Spawning 占位应被移除");
        assert_eq!(state.workflows.len(), 1);
        assert!(!state.workflows[0].is_finished());
        // 重放同一行:no-op
        assert!(!state.workflow_launched(
            "w7rpwxkrx",
            "wf_0c2a0924-326",
            "e2e-probe",
            "toolu_wf",
            at
        ));
        assert_eq!(state.workflows.len(), 1);
        // 完成通知按 taskId 收敛
        assert!(state.finish_workflow("w7rpwxkrx", false, at + chrono::Duration::minutes(16)));
        assert!(state.workflows[0].is_finished());
        // 重复通知(真机有 8ms 内双写)不重复标记
        assert!(!state.finish_workflow("w7rpwxkrx", false, at));
    }

    #[test]
    fn workflow_dir_reconcile_counts_and_children() {
        let mut state = SubagentState::new();
        state.workflow_launched("wtask", "wf_x", "code-review", "tu", Utc::now());
        let changed = state.reconcile_workflow_dir(WorkflowDirInfo {
            run_id: "wf_x".into(),
            name: Some("code-review".into()),
            phase_total: Some(4),
            journal_started: 3,
            journal_done: 1,
            journal_mtime: Some(Utc::now()),
            started_at: Utc::now(),
            children: vec![SubagentStatus::Running {
                agent_id: "c1".into(),
                description: "code-review #1".into(),
                agent_type: "workflow-subagent".into(),
                tool_use_id: String::new(),
                started_at: Utc::now(),
                last_activity: Some(Utc::now()),
                model: Some("opus".into()),
            }],
        });
        assert!(changed);
        let w = &state.workflows[0];
        assert_eq!(w.phase_total, Some(4));
        assert_eq!((w.journal_started, w.journal_done), (3, 1));
        assert_eq!(w.children.len(), 1);
        // 展示投影:组头包含名字/阶段总数/配对计数
        let display = state.display(Utc::now());
        assert_eq!(display.workflows.len(), 1);
        let header = display.workflows[0].header_line(Utc::now());
        assert!(header.contains("workflow code-review"), "{header}");
        assert!(header.contains("阶段共 4"), "{header}");
        assert!(header.contains("2 跑 1 完"), "{header}");
    }

    /// 复审阻塞 3①:长跑(>2h)但 child 仍在输出的 workflow 不得被整组隐藏——
    /// 活跃锚要取 journal mtime 与活跃 child 转录 mtime 的最大值。
    #[test]
    fn workflow_anchor_tracks_active_children() {
        let now = Utc::now();
        let ancient = now - chrono::Duration::hours(3);
        let child_running = |act: DateTime<Utc>| SubagentStatus::Running {
            agent_id: "c1".into(),
            description: "x #1".into(),
            agent_type: "workflow-subagent".into(),
            tool_use_id: String::new(),
            started_at: ancient,
            last_activity: Some(act),
            model: None,
        };
        let mut state = SubagentState::new();
        state.reconcile_workflow_dir(WorkflowDirInfo {
            run_id: "wf_long".into(),
            name: Some("long-run".into()),
            phase_total: None,
            journal_started: 1,
            journal_done: 0,
            journal_mtime: Some(ancient), // journal 3h 没动
            started_at: ancient,
            children: vec![child_running(now - chrono::Duration::seconds(30))],
        });
        assert_eq!(
            state.display(now).workflows.len(),
            1,
            "child 仍在写转录的 workflow 必须展示"
        );

        // 对照:child 也停写超窗 → 整组按陈旧隐藏。
        let mut stale = SubagentState::new();
        stale.reconcile_workflow_dir(WorkflowDirInfo {
            run_id: "wf_dead".into(),
            name: Some("dead-run".into()),
            phase_total: None,
            journal_started: 1,
            journal_done: 0,
            journal_mtime: Some(ancient),
            started_at: ancient,
            children: vec![child_running(ancient)],
        });
        assert!(stale.display(now).workflows.is_empty());
    }

    /// 复审阻塞 3②:child 清单不得整组替换——child jsonl mtime 每次扫描都在涨,
    /// 整组 PartialEq 恒判「变了」会绕过 30s 量化,面板每 5s PATCH 一次。
    #[test]
    fn workflow_children_merge_quantizes_activity_and_flips_completion() {
        let mut state = SubagentState::new();
        state.workflow_launched("wt", "wf_m", "merge-flow", "tu", Utc::now());
        let t0 = Utc::now();
        let info = |act: DateTime<Utc>, completed: bool| WorkflowDirInfo {
            run_id: "wf_m".into(),
            name: Some("merge-flow".into()),
            phase_total: Some(2),
            journal_started: 1,
            journal_done: 0,
            journal_mtime: Some(t0),
            started_at: t0,
            children: vec![if completed {
                SubagentStatus::Completed {
                    agent_id: "c1".into(),
                    description: "merge-flow #1".into(),
                    finished_at: act,
                    model: Some("opus".into()),
                }
            } else {
                SubagentStatus::Running {
                    agent_id: "c1".into(),
                    description: "merge-flow #1".into(),
                    agent_type: "workflow-subagent".into(),
                    tool_use_id: String::new(),
                    started_at: t0,
                    last_activity: Some(act),
                    model: Some("opus".into()),
                }
            }],
        };
        // 首扫:新 child → changed
        assert!(state.reconcile_workflow_dir(info(t0, false)));
        // 5s 后 mtime 微涨:量化步长内 → 不判 changed(不触发 PATCH)
        assert!(!state.reconcile_workflow_dir(info(t0 + chrono::Duration::seconds(5), false)));
        assert!(!state.reconcile_workflow_dir(info(t0 + chrono::Duration::seconds(25), false)));
        // 超过量化步长 → changed
        assert!(state.reconcile_workflow_dir(info(
            t0 + chrono::Duration::seconds(ACTIVITY_QUANTUM_SECS),
            false
        )));
        // 完成翻转 → changed,finished_at 吃 mtime
        let done_at = t0 + chrono::Duration::seconds(90);
        assert!(state.reconcile_workflow_dir(info(done_at, true)));
        match &state.workflows[0].children[0] {
            SubagentStatus::Completed { finished_at, .. } => assert_eq!(*finished_at, done_at),
            other => panic!("应翻转为 Completed: {other:?}"),
        }
        // 终态软删:目录继续报同一 child(无论形态)恒 no-op,不复活
        assert!(!state.reconcile_workflow_dir(info(done_at, true)));
        assert!(!state.reconcile_workflow_dir(info(done_at + chrono::Duration::seconds(60), false)));
    }

    /// 目录兜底建的 run(转录没看到 launch,如 /bridge 召回):历史残留不得展示为活跃。
    #[test]
    fn stale_workflow_dir_run_not_displayed() {
        let mut state = SubagentState::new();
        let ancient = Utc::now() - chrono::Duration::hours(12);
        state.reconcile_workflow_dir(WorkflowDirInfo {
            run_id: "wf_old".into(),
            name: Some("old-flow".into()),
            phase_total: None,
            journal_started: 10,
            journal_done: 10,
            journal_mtime: Some(ancient),
            started_at: ancient,
            children: Vec::new(),
        });
        assert!(
            state.display(Utc::now()).workflows.is_empty(),
            "12h 前的 run 不算活跃"
        );
    }

    // ── 权威对账 ──

    #[test]
    fn count_drift_only_reports_overcount() {
        let mut state = SubagentState::new();
        state.promote_to_running("a1", "在跑", "t1", None, Utc::now());
        // 权威说 1 个 pending:一致,无漂移
        state.note_authority(TurnAuthority {
            at: Utc::now(),
            pending_background: Some(1),
            pending_workflows: Some(0),
        });
        assert!(state.count_drift().is_none());
        // 权威说 2(完成通知已入队未消费的排队语义):权威 ≥ 状态机,不报
        state.note_authority(TurnAuthority {
            at: Utc::now(),
            pending_background: Some(2),
            pending_workflows: Some(0),
        });
        assert!(state.count_drift().is_none());
        // 权威说 0 而状态机还有 Running:漏报完成,必须报漂移
        state.note_authority(TurnAuthority {
            at: Utc::now(),
            pending_background: Some(0),
            pending_workflows: Some(0),
        });
        let drift = state.count_drift().expect("状态机>权威必须报");
        assert!(drift.contains("bg 状态机=1 > 权威=0"), "{drift}");
    }

    // ── live 卡尾部 ──

    #[test]
    fn tail_lines_running_first_truncated_with_model() {
        let now = Utc::now();
        let running = |desc: &str, model: Option<&str>| SubagentStatus::Running {
            agent_id: desc.to_string(),
            description: desc.to_string(),
            agent_type: "x".into(),
            tool_use_id: desc.to_string(),
            started_at: now,
            last_activity: None,
            model: model.map(str::to_string),
        };
        let display = SubagentDisplay {
            agents: vec![
                SubagentStatus::Completed {
                    agent_id: "d1".into(),
                    description: "先完成的".into(),
                    finished_at: now,
                    model: None,
                },
                running("跑1", Some("claude-fable-5")),
                running("跑2", None),
                running("跑3", Some("opus")),
            ],
            workflows: vec![],
        };
        let lines = subagent_tail_lines(&display, 3);
        assert_eq!(lines.len(), 4, "3 条 + 「等 N 个」");
        assert!(
            lines[0].contains("跑1") && lines[0].contains("(fable-5)"),
            "{lines:?}"
        );
        assert!(
            lines[1].contains("跑2") && !lines[1].contains('('),
            "没 model 不显示: {lines:?}"
        );
        assert!(
            lines[2].contains("跑3") && lines[2].contains("(opus)"),
            "{lines:?}"
        );
        assert_eq!(lines[3], "… 等 1 个", "已完成的被挤到截断区: {lines:?}");
    }

    #[test]
    fn tail_lines_workflow_group_compact() {
        let display = SubagentDisplay {
            agents: vec![],
            workflows: vec![WorkflowGroup {
                run_id: "wf_cr".into(),
                name: "code-review".into(),
                finished: None,
                phase_total: Some(3),
                journal_started: 5,
                journal_done: 2,
                started_at: Utc::now(),
                children: vec![],
            }],
        };
        let lines = subagent_tail_lines(&display, 3);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("workflow code-review"), "{lines:?}");
        assert!(lines[0].contains("3 跑 2 完"), "{lines:?}");
    }

    #[test]
    fn tail_title_clipped_by_chars_not_bytes() {
        let long = "多字节标题".repeat(20);
        let display = SubagentDisplay {
            agents: vec![SubagentStatus::Running {
                agent_id: "a".into(),
                description: long,
                agent_type: "x".into(),
                tool_use_id: "t".into(),
                started_at: Utc::now(),
                last_activity: None,
                model: None,
            }],
            workflows: vec![],
        };
        let lines = subagent_tail_lines(&display, 3);
        assert!(
            lines[0].chars().count() < 40,
            "标题必须被截断: {}",
            lines[0]
        );
        assert!(lines[0].ends_with('…'));
    }

    #[test]
    fn short_model_strips_claude_prefix_only() {
        assert_eq!(short_model("claude-fable-5"), "fable-5");
        assert_eq!(short_model("claude-opus-5[1m]"), "opus-5[1m]");
        assert_eq!(short_model("opus"), "opus");
    }

    // ── 辅助函数 ──

    #[test]
    fn extract_xml_tag_basic() {
        let xml = "<task-id>abc123</task-id>";
        assert_eq!(extract_xml_tag(xml, "task-id").as_deref(), Some("abc123"));
    }

    #[test]
    fn extract_xml_tag_missing() {
        let xml = "<task-id>abc123</task-id>";
        assert!(extract_xml_tag(xml, "status").is_none());
    }

    #[test]
    fn agent_id_from_meta_filename_ok() {
        let p = Path::new("/x/agent-a3859a255bbe30bb5.meta.json");
        assert_eq!(
            agent_id_from_meta_filename(p).as_deref(),
            Some("a3859a255bbe30bb5")
        );
    }

    #[test]
    fn agent_id_from_meta_filename_wrong_pattern() {
        let p = Path::new("/x/agent-a3859a255bbe30bb5.jsonl");
        assert!(agent_id_from_meta_filename(p).is_none());
    }

    /// 实测事故:`/bridge` 召回历史会话后,面板列出十几条「🔄 (无描述) 运行中 38h31m」——
    /// 全是 39 小时前那次 workflow 的目录残留。判活轴改为 last_activity(jsonl mtime),
    /// started_at 只在拿不到 mtime 时兜底。
    #[test]
    fn dir_scan_liveness_uses_activity_axis() {
        let now = Utc::now();
        let window = DIR_SUBAGENT_STALE_AFTER;
        let fresh = now - chrono::Duration::minutes(3);
        let ancient = now - chrono::Duration::hours(38);

        // 起了 3h、此刻还在写转录 → 活的(旧 started_at 轴会误杀)
        assert!(dir_subagent_is_live(
            false,
            now - chrono::Duration::hours(3),
            Some(fresh),
            now,
            window
        ));
        // 起了 20min、但 3h 前就停写 → 不是活的
        assert!(!dir_subagent_is_live(
            false,
            now - chrono::Duration::minutes(20),
            Some(now - chrono::Duration::hours(3)),
            now,
            window
        ));
        // 39h 前的残留(无 mtime)→ 回落 started_at,不算活
        assert!(!dir_subagent_is_live(false, ancient, None, now, window));
        // 已完成的照收(展示层按 TTL 过滤)
        assert!(dir_subagent_is_live(
            true,
            ancient,
            Some(ancient),
            now,
            window
        ));
        // 边界:恰好等于窗口即视为超期(严格小于才算活)。
        assert!(!dir_subagent_is_live(
            false,
            now - window,
            None,
            now,
            window
        ));
    }

    #[test]
    fn elapsed_human_formats() {
        let now = Utc::now();
        let running = |secs: i64| SubagentStatus::Running {
            agent_id: "x".into(),
            description: "x".into(),
            agent_type: "x".into(),
            tool_use_id: "x".into(),
            started_at: now - chrono::Duration::seconds(secs),
            last_activity: None,
            model: None,
        };
        assert!(running(45).elapsed_human(now).contains("45s"));
        assert!(running(125).elapsed_human(now).contains("2m5s"));
        assert!(running(3725).elapsed_human(now).contains("1h2m"));
    }

    // ── Workflow 名/阶段解析 ──

    #[test]
    fn workflow_name_from_script_double_quotes() {
        let script = "export const meta = {\n  name: \"code-review\",\n  description: \"…\",\n};\nexport default async function run() {}";
        assert_eq!(
            workflow_name_from_script(script).as_deref(),
            Some("code-review")
        );
    }

    #[test]
    fn workflow_name_from_script_single_quotes() {
        let script =
            "export const meta = {\n  name: 'fast-cold-scan-design',\n  description: 'x',\n}";
        assert_eq!(
            workflow_name_from_script(script).as_deref(),
            Some("fast-cold-scan-design")
        );
    }

    #[test]
    fn workflow_name_from_script_backtick_and_compact_layout() {
        let script = "export const meta = {name:`deep-research`,description:`x`}";
        assert_eq!(
            workflow_name_from_script(script).as_deref(),
            Some("deep-research")
        );
    }

    #[test]
    fn workflow_name_from_script_missing_meta_or_name() {
        // 没有 meta 对象
        assert!(workflow_name_from_script("export default async function run() {}").is_none());
        // 有 meta 但没有 name 键(description 里的 "name" 属子串命中,键边界应排除)
        let script = "export const meta = {\n  description: \"a nickname: 'x'\",\n}";
        assert!(workflow_name_from_script(script).is_none());
        // 空脚本
        assert!(workflow_name_from_script("").is_none());
    }

    #[test]
    fn workflow_name_from_script_rejects_non_string_value() {
        // name 的值不是字符串字面量 → 私有格式变了,放弃(回落交给调用方)
        let script = "export const meta = { name: someIdent, description: 'x' }";
        assert!(workflow_name_from_script(script).is_none());
    }

    /// 真机脚本头(2.1.231):phases 数组 2 个元素,元素内有嵌套引号/花括号。
    #[test]
    fn workflow_phases_from_script_real_shape() {
        let script = "export const meta = {\n  name: 'slash-passthrough-e2e-probe',\n  description: '透传真机证据补全',\n  phases: [\n    { title: 'Probe', detail: '一次性 tmux+claude 会话真机采样', model: 'opus' },\n    { title: 'Verdict', detail: '判据对样本裁决 + 手动验收单', model: 'opus' },\n  ],\n}\n";
        assert_eq!(workflow_phases_from_script(script), Some(2));
    }

    #[test]
    fn workflow_phases_from_script_absent_or_broken() {
        assert!(workflow_phases_from_script("export const meta = { name: 'x' }").is_none());
        // 数组未闭合(超出窗口/格式变了)→ 不猜
        assert!(
            workflow_phases_from_script("export const meta = { phases: [ { title: 'a' }").is_none()
        );
        assert!(workflow_phases_from_script("").is_none());
    }

    /// 字符串里的括号不干扰配平:detail 含 `[`/`{`。
    #[test]
    fn workflow_phases_ignores_brackets_inside_strings() {
        let script =
            "export const meta = { phases: [ { title: 'a[{', detail: \"}b]\" }, { title: 'c' } ] }";
        assert_eq!(workflow_phases_from_script(script), Some(2));
    }

    #[test]
    fn workflow_name_from_script_filename_ok() {
        assert_eq!(
            workflow_name_from_script_filename("code-review-wf_41453ba1-ebc.js", "wf_41453ba1-ebc")
                .as_deref(),
            Some("code-review")
        );
        // 名字本身含连字符
        assert_eq!(
            workflow_name_from_script_filename(
                "summon-borrow-agentteam-ideas-wf_24020ecb-8e0.js",
                "wf_24020ecb-8e0"
            )
            .as_deref(),
            Some("summon-borrow-agentteam-ideas")
        );
    }

    #[test]
    fn workflow_name_from_script_filename_mismatch() {
        // 别的 run 的脚本对不上
        assert!(workflow_name_from_script_filename(
            "code-review-wf_other-123.js",
            "wf_41453ba1-ebc"
        )
        .is_none());
        // 非 .js 文件
        assert!(workflow_name_from_script_filename(
            "code-review-wf_41453ba1-ebc.txt",
            "wf_41453ba1-ebc"
        )
        .is_none());
    }

    /// 真实格式(2.1.229 实测):Workflow tool_use 的 input 只有 `script`,没有 name/args。
    #[test]
    fn detect_spawn_workflow_script_only_input() {
        let line = serde_json::json!({
            "type": "assistant",
            "message": {
                "role": "assistant",
                "content": [{
                    "type": "tool_use",
                    "id": "toolu_wf_script",
                    "name": "Workflow",
                    "input": {
                        "script": "export const meta = {\n  name: 'deep-research',\n  description: 'x',\n};\nexport default async function run() {}"
                    }
                }]
            }
        })
        .to_string();
        let ev = detect_spawn(&line).expect("应检测到 workflow spawn");
        assert_eq!(ev.description, "workflow: deep-research");
        assert_eq!(ev.agent_type, "workflow:deep-research");
        assert!(ev.is_async);
    }

    /// script 解析不出 meta.name 时回落「workflow」,不再渲染成空/报错。
    #[test]
    fn detect_spawn_workflow_unparsable_script_falls_back() {
        let line = serde_json::json!({
            "type": "assistant",
            "message": {
                "content": [{
                    "type": "tool_use",
                    "id": "toolu_wf_noname",
                    "name": "Workflow",
                    "input": {"script": "export default async function run() {}"}
                }]
            }
        })
        .to_string();
        let ev = detect_spawn(&line).expect("应检测到 workflow spawn");
        assert_eq!(ev.description, "workflow: workflow");
        assert_eq!(ev.agent_type, "workflow:workflow");
    }

    // ── journal 配对计数 ──

    #[test]
    fn journal_counts_started_result_pairs() {
        let content = concat!(
            "{\"type\":\"started\",\"key\":\"v2:aaa\",\"agentId\":\"a1\"}\n",
            "{\"type\":\"result\",\"key\":\"v2:aaa\",\"agentId\":\"a1\",\"result\":\"done\"}\n",
            "{\"type\":\"started\",\"key\":\"v2:bbb\",\"agentId\":\"a2\"}\n",
            "not-json-half-written-tail",
        );
        let c = journal_counts(content);
        assert_eq!((c.started, c.done), (2, 1));
    }

    #[test]
    fn journal_counts_empty() {
        let c = journal_counts("");
        assert_eq!((c.started, c.done), (0, 0));
    }

    // ── parse_subagent_meta 描述/模型回落 ──

    /// workflow 子代理 meta 实测无 description、有 model → 描述用调用方给的归属描述,
    /// model 采 meta 值。
    #[test]
    fn parse_subagent_meta_workflow_model_and_fallback_description() {
        let meta = r#"{"agentType":"workflow-subagent","spawnDepth":1,"model":"opus"}"#;
        let e = parse_subagent_meta(meta, "a1", false, Utc::now(), None, "code-review #2")
            .expect("meta 应可解析");
        assert_eq!(e.description, "code-review #2");
        assert_eq!(e.agent_type, "workflow-subagent");
        assert_eq!(e.model.as_deref(), Some("opus"));
    }

    #[test]
    fn parse_subagent_meta_keeps_own_description_and_last_activity() {
        let meta = r#"{"agentType":"Explore","description":"自带描述","toolUseId":"t9"}"#;
        let act = Utc::now() - chrono::Duration::seconds(7);
        let e = parse_subagent_meta(meta, "a2", false, Utc::now(), Some(act), "不该用到")
            .expect("meta 应可解析");
        assert_eq!(e.description, "自带描述");
        assert_eq!(e.last_activity, Some(act));
        assert!(e.model.is_none(), "顶层 meta 没有 model 字段,不猜");
    }

    // ── last_activity 采集/推进/渲染 ──

    #[test]
    fn reconcile_refreshes_last_activity_with_quantization() {
        let mut state = SubagentState::new();
        state.promote_to_running("a1", "任务", "t1", None, Utc::now());

        let t0 = Utc::now();
        let entry = |at: DateTime<Utc>| DirSubagent {
            agent_id: "a1".into(),
            description: "任务".into(),
            agent_type: "Explore".into(),
            tool_use_id: "t1".into(),
            completed: false,
            started_at: t0,
            last_activity: Some(at),
            model: None,
        };
        // None → Some 是真变化
        assert!(state.reconcile_from_dir(vec![entry(t0)]));
        // 量化步长内的推进不算变化(避免每轮扫描都 PATCH 面板)
        assert!(!state.reconcile_from_dir(vec![entry(t0 + chrono::Duration::seconds(5))]));
        // 超过步长才推进
        assert!(state.reconcile_from_dir(vec![entry(
            t0 + chrono::Duration::seconds(ACTIVITY_QUANTUM_SECS)
        )]));
        // mtime 回退(时钟抖动)忽略
        assert!(!state.reconcile_from_dir(vec![entry(t0 - chrono::Duration::hours(1))]));
    }

    #[test]
    fn activity_human_only_for_running_with_activity() {
        use chrono::TimeZone;
        // 固定时刻 + 固定时区(UTC+8),断言与本机时区/当前时间无关。
        let tz = chrono::FixedOffset::east_opt(8 * 3600).expect("UTC+8 合法");
        let at = Utc
            .with_ymd_and_hms(2026, 8, 13, 17, 38, 42)
            .single()
            .expect("固定时刻合法");
        let s = SubagentStatus::Running {
            agent_id: "x".into(),
            description: "x".into(),
            agent_type: "x".into(),
            tool_use_id: "x".into(),
            started_at: at - chrono::Duration::minutes(5),
            last_activity: Some(at),
            model: None,
        };
        // 17:38:42 UTC == 次日 01:38:42 UTC+8
        assert_eq!(s.activity_human(&tz).as_deref(), Some("最后活跃 01:38:42"));

        let s_no_act = SubagentStatus::Running {
            agent_id: "x".into(),
            description: "x".into(),
            agent_type: "x".into(),
            tool_use_id: "x".into(),
            started_at: at,
            last_activity: None,
            model: None,
        };
        assert!(s_no_act.activity_human(&tz).is_none());

        let done = SubagentStatus::Completed {
            agent_id: "x".into(),
            description: "x".into(),
            finished_at: at,
            model: None,
        };
        assert!(done.activity_human(&tz).is_none());
    }

    /// 终审点名的空缺:子代理静止(停写/卡死)后 mtime 不再推进 → 面板不再 PATCH,
    /// 卡片不得继续宣称「刚刚活跃」。绝对时间方案下这自然成立:`activity_human` 的输入里
    /// 根本没有 now,输出恒等于 last_activity 那一刻的时间戳——结构上排除了「永久停在
    /// 活跃 0s 前」的可能。这里钉死两点:① 输出就是该时刻的 HH:MM:SS;② 不含相对措辞。
    #[test]
    fn stalled_subagent_renders_frozen_timestamp_not_relative_text() {
        use chrono::TimeZone;
        let tz = chrono::FixedOffset::east_opt(8 * 3600).expect("UTC+8 合法");
        let at = Utc
            .with_ymd_and_hms(2026, 8, 13, 17, 38, 42)
            .single()
            .expect("固定时刻合法");
        let s = SubagentStatus::Running {
            agent_id: "x".into(),
            description: "静止 90s 的子代理".into(),
            agent_type: "x".into(),
            tool_use_id: "x".into(),
            started_at: at - chrono::Duration::minutes(5),
            last_activity: Some(at),
            model: None,
        };
        // 即便真实时间已过去 90s(乃至任意久),渲染结果不变:它是时间戳,不是相对文案。
        let rendered = s.activity_human(&tz).expect("Running+有活跃时间应有输出");
        assert_eq!(rendered, "最后活跃 01:38:42");
        assert!(!rendered.contains('前'), "不得出现相对时间措辞:{rendered}");
        assert_eq!(
            s.activity_human(&tz).as_deref(),
            Some(rendered.as_str()),
            "重复渲染(时间流逝后)输出必须逐字节相同"
        );
    }

    // ── activity_probe(空闲判定的数据源) ──

    /// 时效窗内的 Running → active,且 last_activity = 它的活跃锚。
    /// 空状态 → 不活跃、无时刻(调用方按「无 subagent」回退)。
    #[test]
    fn activity_probe_reports_running_within_window() {
        let now = t0();
        let empty = SubagentState::new().activity_probe(now);
        assert!(!empty.active);
        assert!(empty.last_activity.is_none());

        let mut st = SubagentState::new();
        st.promote_to_running(
            "a1",
            "后台修 bug",
            "tu_1",
            None,
            now - chrono::Duration::minutes(30),
        );
        let probe = st.activity_probe(now);
        assert!(probe.active, "窗内 Running 必须判活跃");
        assert_eq!(
            probe.last_activity,
            Some(now - chrono::Duration::minutes(30))
        );
    }

    /// 判活沿用展示层同一条时效轴(`DIR_SUBAGENT_STALE_AFTER`):超窗的 Running(watcher
    /// 冻死后的残留形态)不算活跃 —— 这正是「卡死的 watcher 不把会话变成永不回收」的机制;
    /// 但 last_activity 照报,由调用方与入站时钟取 max 后自然淹没。
    #[test]
    fn activity_probe_expires_stale_running_on_display_axis() {
        let now = t0();
        let stale_at = now - DIR_SUBAGENT_STALE_AFTER - chrono::Duration::minutes(1);
        let mut st = SubagentState::new();
        st.promote_to_running("a1", "冻结的残留", "tu_1", None, stale_at);
        let probe = st.activity_probe(now);
        assert!(!probe.active, "超时效窗的 Running 不得再挡空闲回收");
        assert_eq!(probe.last_activity, Some(stale_at));
    }

    /// 全部终态 → 不活跃,last_activity = 最晚的完成时刻(空闲期从它起算)。
    /// 未完 workflow(journal 还在动)→ 活跃。
    #[test]
    fn activity_probe_counts_finish_time_and_unfinished_workflow() {
        let now = t0();
        let mut st = SubagentState::new();
        st.promote_to_running(
            "a1",
            "先完的",
            "tu_1",
            None,
            now - chrono::Duration::hours(3),
        );
        st.mark_completed("a1", now - chrono::Duration::hours(2));
        st.promote_to_running(
            "a2",
            "后完的",
            "tu_2",
            None,
            now - chrono::Duration::hours(3),
        );
        st.mark_completed("a2", now - chrono::Duration::minutes(10));
        let probe = st.activity_probe(now);
        assert!(!probe.active, "全部终态不算活跃");
        assert_eq!(
            probe.last_activity,
            Some(now - chrono::Duration::minutes(10)),
            "空闲期从最后一个 subagent 的完成时刻起算"
        );

        st.workflow_launched(
            "task9",
            "wf_run9",
            "code-review",
            "tu_wf",
            now - chrono::Duration::minutes(5),
        );
        assert!(
            st.activity_probe(now).active,
            "未完 workflow 必须判活跃(它的子代理在途工作不可丢)"
        );
        let mut done = st.clone();
        done.finish_workflow("task9", false, now - chrono::Duration::minutes(1));
        let probe = done.activity_probe(now);
        assert!(!probe.active, "workflow 完成后不再挡回收");
        assert_eq!(
            probe.last_activity,
            Some(now - chrono::Duration::minutes(1)),
            "workflow 的完成时刻也推进空闲时钟"
        );
    }
}
