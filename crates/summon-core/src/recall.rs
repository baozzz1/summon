//! 会话召回(纯逻辑):provider 身份、统一候选、合并去重、按时间降序、分页。Claude 的
//! `claude agents + 扫盘` 合并仍在本模块；Cursor/Codex 的盘上解析留在 summon IO 侧。
//!
//! 为什么放 core:合并/去重/排序/分页是纯决策,与子进程/文件系统无关,抽成纯函数才能脱离
//! `claude agents`、脱离硬盘单测。IO(跑命令、扫盘、读 cwd)留在 summon 侧。
//!
//! 活跃 vs 历史的**列表展示**只看 `live_pid` 有无；恢复侧必须重新证明 source 状态，不能把
//! `None` 直接解释成“没有 writer”。

use crate::bridge::ProcessFingerprint;
use serde::{Deserialize, Serialize};

/// 会话召回的 provider 身份。它同时进入卡片 callback 和本地 resume 记录，避免只靠裸 SID
/// 猜 CLI：不同 CLI 即使碰巧用了相同 UUID，也必须视为两个完全不同的会话。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecallProvider {
    Claude,
    Cursor,
    Codex,
}

/// 候选当前由谁持有。外部历史会话只提交一个新的本地 binding；summon 托管的 RPC 会话则必须
/// 做单一所有权迁移，不能复用外部 `recall_resume` 的 copy 语义。
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecallOwnership {
    #[default]
    External,
    ManagedRpc,
}

impl RecallProvider {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Cursor => "cursor",
            Self::Codex => "codex",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Claude => "Claude",
            Self::Cursor => "Cursor",
            Self::Codex => "Codex",
        }
    }

    pub fn from_cli_kind(kind: &str) -> Option<Self> {
        match kind {
            "claude" => Some(Self::Claude),
            "cursor" | "agent" => Some(Self::Cursor),
            "codex" => Some(Self::Codex),
            _ => None,
        }
    }
}

/// `claude agents --json` 的一条活跃会话(字段对齐其 camelCase 输出;pid/name 可缺)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaudeAgentEntry {
    #[serde(rename = "sessionId")]
    pub session_id: String,
    pub cwd: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(rename = "startedAt")]
    pub started_at: u64,
    /// interactive 会话有;background 会话缺(无前台进程)。
    #[serde(default)]
    pub pid: Option<u32>,
    /// interactive 会话的即时状态(如 "busy"/"idle");旧版/background 可缺。
    #[serde(default)]
    pub status: Option<String>,
    /// IO 层在卡片渲染时从 OS 现采的 exact 指纹。`claude agents --json` 不提供该字段，
    /// 也不能反序列化客户端输入来伪造；只用于生成本机 durable observation。
    #[serde(skip)]
    pub observed_process: Option<ProcessFingerprint>,
}

/// 扫盘得到的历史会话(summon 侧扫,喂给 core)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiskSessionEntry {
    pub sid: String,
    pub cwd: String,
    #[serde(default)]
    pub name: Option<String>,
    pub last_active_ms: u64,
}

/// 召回候选:活跃与历史的统一表达。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionCandidate {
    pub provider: RecallProvider,
    #[serde(default)]
    pub ownership: RecallOwnership,
    pub sid: String,
    pub cwd: String,
    pub name: String,
    /// transcript 第一条可见用户输入的有界单行预览；为空时卡片回退会话名。
    #[serde(default)]
    pub prompt_preview: String,
    /// 排序键:活跃=startedAt,历史=transcript mtime。
    pub last_active_ms: u64,
    /// Some=列表采集时活跃；None=列表采集时未发现活跃源。恢复侧仍须重新证明。
    pub live_pid: Option<u32>,
    /// 活跃源的即时状态(`claude agents` 的 `status`,如 "busy"/"idle")。`busy` 只决定接管
    /// 确认框怎么写:接管会打断这一轮并停掉后台任务;是否放行由身份/资产/Gone 闸决定。
    #[serde(default)]
    pub live_status: Option<String>,
    /// 本机 durable source observation 的不透明 UUID。卡片只携带此 ID，不下发进程路径。
    #[serde(default)]
    pub source_observation_id: Option<String>,
    /// 渲染卡片时活跃源账上还在跑的后台任务,一项一句人话(如「后台命令「跑全量测试」」)。
    /// 只进接管确认框,不进 callback。
    #[serde(default)]
    pub background_tasks: Vec<String>,
}

impl SessionCandidate {
    pub fn is_active(&self) -> bool {
        self.live_pid.is_some()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecallPage {
    pub items: Vec<SessionCandidate>,
    pub page: usize,
    pub total: usize,
    pub total_pages: usize,
}

/// 已完成 provider 内去重的候选统一排序、分页。活跃项排在前面用于提示 source 占用；是否允许
/// 接管由 provider 的恢复策略决定，不能从这里的排序推导。
pub fn paginate_candidates(
    mut all: Vec<SessionCandidate>,
    page: usize,
    page_size: usize,
) -> RecallPage {
    all.sort_by(|l, r| {
        r.is_active()
            .cmp(&l.is_active())
            .then_with(|| r.last_active_ms.cmp(&l.last_active_ms))
            .then_with(|| l.provider.as_str().cmp(r.provider.as_str()))
            .then_with(|| l.sid.cmp(&r.sid))
    });

    let total = all.len();
    let total_pages = if page_size == 0 {
        0
    } else {
        total.div_ceil(page_size)
    };
    let start = page.saturating_sub(1).saturating_mul(page_size);
    let items = all.into_iter().skip(start).take(page_size).collect();
    RecallPage {
        items,
        page,
        total,
        total_pages,
    }
}

/// 活跃(claude agents)+ 历史(扫盘)→ 合并去重(同 sid 活跃优先)、按 last_active 降序、分页。
///
/// `page` 从 1 起。越界页返回空 items,但 total/total_pages 仍是真实总量(供卡片显示「共 N 页」)。
pub fn recall_page(
    active: &[ClaudeAgentEntry],
    historical: &[DiskSessionEntry],
    page: usize,
    page_size: usize,
) -> RecallPage {
    use std::collections::HashMap;

    // sid → 候选。先放历史,再放活跃(覆盖)——活跃更权威(它此刻在跑)。
    let mut by_sid: HashMap<String, SessionCandidate> = HashMap::new();
    for d in historical {
        by_sid.insert(
            d.sid.clone(),
            SessionCandidate {
                provider: RecallProvider::Claude,
                ownership: RecallOwnership::External,
                sid: d.sid.clone(),
                cwd: d.cwd.clone(),
                name: d.name.clone().unwrap_or_default(),
                prompt_preview: String::new(),
                last_active_ms: d.last_active_ms,
                live_pid: None,
                live_status: None,
                source_observation_id: None,
                background_tasks: Vec::new(),
            },
        );
    }
    for a in active {
        by_sid.insert(
            a.session_id.clone(),
            SessionCandidate {
                provider: RecallProvider::Claude,
                ownership: RecallOwnership::External,
                sid: a.session_id.clone(),
                cwd: a.cwd.clone(),
                name: a.name.clone().unwrap_or_default(),
                prompt_preview: String::new(),
                last_active_ms: a.started_at,
                live_pid: a.pid,
                live_status: a.status.clone(),
                source_observation_id: None,
                background_tasks: Vec::new(),
            },
        );
    }

    paginate_candidates(by_sid.into_values().collect(), page, page_size)
}

/// 恢复一个召回候选的方式。活跃(有前台源)必须先退出源进程防双写;历史无源直接起。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecallResumeMode {
    /// 历史/无前台源:直接 `claude --resume <sid>` 起为托管。
    Historical,
    /// 活跃:先安全退出这个源 PID(防双写),再起。
    ActiveKillSource { pid: u32 },
}

/// 候选 → 恢复方式。`live_pid` 有 → 先杀源;无 → 直接起(纯决策,便于单测)。
pub fn resume_mode(candidate: &SessionCandidate) -> RecallResumeMode {
    match candidate.live_pid {
        Some(pid) => RecallResumeMode::ActiveKillSource { pid },
        None => RecallResumeMode::Historical,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resume_mode_kills_source_for_active_and_starts_directly_for_historical() {
        let active = SessionCandidate {
            provider: RecallProvider::Claude,
            ownership: RecallOwnership::External,
            sid: "a".into(),
            cwd: "/x".into(),
            name: "n".into(),
            prompt_preview: String::new(),
            last_active_ms: 1,
            live_pid: Some(4242),
            live_status: None,
            source_observation_id: None,
            background_tasks: Vec::new(),
        };
        assert_eq!(
            resume_mode(&active),
            RecallResumeMode::ActiveKillSource { pid: 4242 }
        );
        let historical = SessionCandidate {
            live_pid: None,
            ..active
        };
        assert_eq!(resume_mode(&historical), RecallResumeMode::Historical);
    }

    #[test]
    fn claude_agent_entry_decodes_real_json_shape() {
        // 对齐 `claude agents --json` 的真实字段:camelCase、interactive 有 pid、background 缺 pid。
        let json = r#"[
          {"pid":46857,"cwd":"/x/summon","kind":"interactive","startedAt":1784084840008,
           "sessionId":"72288607-cb25","name":"summon-8b","status":"busy"},
          {"id":"fb7a2b92","cwd":"/x/wwdc","kind":"background","startedAt":1783572556797,
           "sessionId":"fb7a2b92-9a2e","name":"分析","state":"blocked"}
        ]"#;
        let entries: Vec<ClaudeAgentEntry> = serde_json::from_str(json).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].session_id, "72288607-cb25");
        assert_eq!(entries[0].pid, Some(46857));
        assert_eq!(entries[0].started_at, 1784084840008);
        // background 缺 pid,不能让整条解析失败。
        assert_eq!(entries[1].pid, None);
        assert_eq!(entries[1].session_id, "fb7a2b92-9a2e");
        // interactive 的即时状态要解出来(busy 时接管拒杀源);background 无 status → None。
        assert_eq!(entries[0].status.as_deref(), Some("busy"));
        assert_eq!(entries[1].status, None);
    }

    #[test]
    fn candidate_is_active_iff_live_pid_present() {
        let active = SessionCandidate {
            provider: RecallProvider::Claude,
            ownership: RecallOwnership::External,
            sid: "a".into(),
            cwd: "/x".into(),
            name: "n".into(),
            prompt_preview: String::new(),
            last_active_ms: 1,
            live_pid: Some(42),
            live_status: None,
            source_observation_id: None,
            background_tasks: Vec::new(),
        };
        let historical = SessionCandidate {
            live_pid: None,
            ..active.clone()
        };
        assert!(active.is_active());
        assert!(!historical.is_active());
    }

    fn agent(sid: &str, pid: Option<u32>, started: u64) -> ClaudeAgentEntry {
        ClaudeAgentEntry {
            session_id: sid.into(),
            cwd: "/x".into(),
            name: Some(sid.into()),
            started_at: started,
            pid,
            status: None,
            observed_process: None,
        }
    }
    fn disk(sid: &str, mtime: u64) -> DiskSessionEntry {
        DiskSessionEntry {
            sid: sid.into(),
            cwd: "/x".into(),
            name: None,
            last_active_ms: mtime,
        }
    }

    #[test]
    fn merges_active_and_historical_sorted_by_recency_desc() {
        let active = vec![agent("live-new", Some(1), 300)];
        let historical = vec![disk("hist-old", 100), disk("hist-mid", 200)];
        let page = recall_page(&active, &historical, 1, 10);
        let sids: Vec<&str> = page.items.iter().map(|c| c.sid.as_str()).collect();
        assert_eq!(sids, vec!["live-new", "hist-mid", "hist-old"]); // 300>200>100
        assert!(page.items[0].is_active());
        assert!(!page.items[1].is_active());
        assert_eq!(page.total, 3);
    }

    #[test]
    fn active_sorts_before_historical_even_when_less_recent() {
        // owner 要:可接管(活跃)优先于可恢复(历史),即便活跃的最近活跃时间更小也排前。
        let active = vec![agent("live-stale", Some(7), 10)]; // 活跃但很旧(10)
        let historical = vec![disk("hist-fresh", 999)]; // 历史但很新(999)
        let page = recall_page(&active, &historical, 1, 10);
        let sids: Vec<&str> = page.items.iter().map(|c| c.sid.as_str()).collect();
        assert_eq!(
            sids,
            vec!["live-stale", "hist-fresh"],
            "活跃排最前,不看时间"
        );
        // 同类内部仍按时间降序。
        let two_active = vec![agent("a-old", Some(1), 5), agent("a-new", Some(2), 50)];
        let p2 = recall_page(&two_active, &[disk("h", 999)], 1, 10);
        let s2: Vec<&str> = p2.items.iter().map(|c| c.sid.as_str()).collect();
        assert_eq!(s2, vec!["a-new", "a-old", "h"], "活跃组内时间降序,历史殿后");
    }

    #[test]
    fn same_sid_prefers_active_over_historical() {
        // 活会话在硬盘上也有 transcript,同 sid 两边都出现 → 以活跃为准,只留一条。
        let active = vec![agent("dup", Some(99), 500)];
        let historical = vec![disk("dup", 100)];
        let page = recall_page(&active, &historical, 1, 10);
        assert_eq!(page.total, 1, "同 sid 去重");
        assert_eq!(page.items[0].live_pid, Some(99), "保留活跃那条");
        assert_eq!(page.items[0].last_active_ms, 500, "活跃的 started_at 胜出");
    }

    #[test]
    fn active_entry_without_pid_becomes_historical_kind() {
        // background 会话在 active 列表里但无 pid → live_pid=None,当历史类(无前台源可杀)。
        let active = vec![agent("bg", None, 400)];
        let page = recall_page(&active, &[], 1, 10);
        assert!(!page.items[0].is_active());
    }

    #[test]
    fn paginates_from_page_one() {
        let historical: Vec<_> = (0..25)
            .map(|i| disk(&format!("s{i:02}"), i as u64))
            .collect();
        let p1 = recall_page(&[], &historical, 1, 10);
        assert_eq!(p1.items.len(), 10);
        assert_eq!(p1.items[0].sid, "s24", "首页是最近的");
        assert_eq!(p1.total, 25);
        assert_eq!(p1.total_pages, 3);
        let p3 = recall_page(&[], &historical, 3, 10);
        assert_eq!(p3.items.len(), 5, "末页 5 条");
        let over = recall_page(&[], &historical, 99, 10);
        assert!(over.items.is_empty(), "越界页空 items");
        assert_eq!(over.total, 25, "但 total 仍真实");
    }

    #[test]
    fn empty_sources_yield_empty_page() {
        let page = recall_page(&[], &[], 1, 10);
        assert!(page.items.is_empty());
        assert_eq!(page.total, 0);
        assert_eq!(page.total_pages, 0);
    }
}
