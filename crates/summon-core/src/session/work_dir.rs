//! 起进程(tmux 冷启 / 无头子进程)时「用哪个工作目录」的判定。
//!
//! 抽成纯函数是因为这条链路此前**没有任何存在性校验**:per-chat `.cwd` 里躺着一个已被删掉的
//! 目录时,`tmux new-session -c <不存在>` 会**静默成功**并把 pane 落到 `$HOME`(实测:退出码 0,
//! `pane_current_path` = 用户家目录)。爆炸半径从一个沙箱目录变成整个家目录,而日志里只有一条
//! 「转录不在预期目录」的噪声警告,既不阻断也不回退。
//!
//! 因此判定必须显式区分三种结局,不允许「找不到就算了」这种会退化成 `$HOME` 的写法:
//! 命中某一级 / 配了但一级都不存在(**明确失败**)/ 压根没配(沿用调用方既有的进程 cwd 兜底)。

/// 工作目录的字符串归一:去尾斜杠、折叠重复斜杠、去掉 `.` 段。
///
/// summon 与 CLI 之间有一条隐式契约:CLI 进程拿到的 cwd 是内核规范化过的(`-c /a/b/` 起来
/// 就是 `/a/b`),而 summon 侧的 cwd 全程是**字符串** —— 从 `.cwd`、`agent.yaml`、`/cd` 参数
/// 一路流到 `project_dirname` 按字面 munge 成 Claude 工程目录名。两边差一个字符,转录就定位到
/// 一个永不存在的目录:2026-09-08 现场,`.cwd` 里的尾斜杠 munge 出多一个尾 `-` 的目录名,
/// 冷启首轮(转录文件尚未创建、存在性自愈无从校验)必等到超时,那一轮的产出发不回群里。
///
/// 不解析软链接或 `..`，本模块保持纯字符串操作；转录冷启动预测由捕获层解析真实目录。
pub fn normalize_work_dir(dir: &str) -> String {
    let trimmed = dir.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    std::path::Path::new(trimmed)
        .components()
        .collect::<std::path::PathBuf>()
        .to_string_lossy()
        .into_owned()
}

/// 工作目录候选的来源,按优先级从高到低排列。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkDirSource {
    /// 调用方显式指定(task/worker 继承父任务目录、bridge 恢复外部会话的 cwd)。
    Explicit,
    /// per-chat `.cwd` 覆盖(`/cd` 命令或会话面板设的)。
    ChatOverride,
    /// `runtime.chat_working_dirs` 里按 chat_id 的映射。
    ChatMapping,
    /// agent 级 `runtime.working_dir`。
    AgentDefault,
}

impl WorkDirSource {
    /// 日志/回执里给人看的来源名(中文,指向用户能去改的那个地方)。
    pub fn label(self) -> &'static str {
        match self {
            WorkDirSource::Explicit => summon_i18n::text!("rdc0f91358d"),
            WorkDirSource::ChatOverride => summon_i18n::text!("rdfb0c98347"),
            WorkDirSource::ChatMapping => summon_i18n::text!("r3ac7cc7174"),
            WorkDirSource::AgentDefault => "agent.yaml runtime.working_dir",
        }
    }
}

/// 一个候选目录及其存在性。`exists` 由调用方在 IO 层探(`Path::is_dir`),本模块不碰文件系统。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkDirCandidate {
    pub source: WorkDirSource,
    pub path: String,
    pub exists: bool,
}

impl WorkDirCandidate {
    /// `path` 在此归一:四个来源都是人写的字符串(`/cd` 参数、agent.yaml、chat 映射),
    /// 带不带尾斜杠全凭手感,而下游要拿它推导 Claude 转录目录 —— 归一收在唯一入口,
    /// 判定链上就不会再出现同一目录的两种写法。
    pub fn new(source: WorkDirSource, path: impl Into<String>, exists: bool) -> Self {
        Self {
            source,
            path: normalize_work_dir(&path.into()),
            exists,
        }
    }
}

/// 判定结果。三态**不可合并**:`AllMissing` 与 `Unconfigured` 都「没选出目录」,但前者必须失败、
/// 后者可以走调用方的进程 cwd 兜底 —— 混成一个 `None` 正是本 bug 的形态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkDirPlan {
    /// 选定目录。`stale` = 被跳过的、路径已不存在的更高优先级候选;非空即说明盘上有陈旧覆盖,
    /// 调用方应打 warning 让 owner 能发现并清理。
    Use {
        path: String,
        source: WorkDirSource,
        stale: Vec<WorkDirCandidate>,
    },
    /// 配了工作目录,但候选一个都不存在。**绝不静默落到 `$HOME` / 进程 cwd**,由调用方明确失败。
    AllMissing { stale: Vec<WorkDirCandidate> },
    /// 压根没配任何工作目录(不是错误):调用方沿用既有的「本进程 cwd」兜底,行为不变。
    Unconfigured,
}

impl WorkDirPlan {
    /// 只变换用于展示的路径文本，保留来源、存在性和优先级判定。
    pub fn map_paths(self, mut map: impl FnMut(&str) -> String) -> Self {
        let map_candidate = |mut candidate: WorkDirCandidate,
                             map: &mut dyn FnMut(&str) -> String| {
            candidate.path = map(&candidate.path);
            candidate
        };
        match self {
            Self::Use {
                path,
                source,
                stale,
            } => Self::Use {
                path: map(&path),
                source,
                stale: stale
                    .into_iter()
                    .map(|candidate| map_candidate(candidate, &mut map))
                    .collect(),
            },
            Self::AllMissing { stale } => Self::AllMissing {
                stale: stale
                    .into_iter()
                    .map(|candidate| map_candidate(candidate, &mut map))
                    .collect(),
            },
            Self::Unconfigured => Self::Unconfigured,
        }
    }
}

/// `/pwd` 能观测到的运行态。RPC 会话与 tmux pane 都只在真实运行时提供路径；拿不到时明确标记，
/// 不用配置值伪装运行真值。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeWorkDir {
    Stopped,
    Unknown,
    Running(Option<String>),
}

/// 渲染 `/pwd` 的只读对账结果。`process_fallback` 是未配置目录时，下次启动会继承的 daemon cwd。
pub fn pwd_report(plan: &WorkDirPlan, runtime: RuntimeWorkDir, process_fallback: &str) -> String {
    let (configured, configured_path) = match plan {
        WorkDirPlan::Use {
            path,
            source,
            stale,
        } => {
            let stale_note = if stale.is_empty() {
                String::new()
            } else {
                summon_i18n::format!("r9108fae851", stale.len())
            };
            (
                summon_i18n::format!("rd6ccb67992", source.label()),
                Some(path.as_str()),
            )
        }
        WorkDirPlan::AllMissing { stale } => {
            let paths = stale
                .iter()
                .map(|candidate| format!("`{}`", candidate.path))
                .collect::<Vec<_>>()
                .join("、");
            (summon_i18n::format!("r9af912b388"), None)
        }
        WorkDirPlan::Unconfigured => (summon_i18n::format!("rd1d6397ed8"), Some(process_fallback)),
    };

    let runtime_line = match runtime {
        RuntimeWorkDir::Stopped if matches!(plan, WorkDirPlan::AllMissing { .. }) => {
            summon_i18n::text!("r099ad5ae3f").to_string()
        }
        RuntimeWorkDir::Stopped => summon_i18n::text!("r22cf0a73e2").to_string(),
        RuntimeWorkDir::Unknown => summon_i18n::text!("r981eb90a50").to_string(),
        RuntimeWorkDir::Running(None) => summon_i18n::text!("rb857461bac").to_string(),
        RuntimeWorkDir::Running(Some(actual)) => match configured_path {
            Some(expected) if normalize_work_dir(expected) == normalize_work_dir(&actual) => {
                summon_i18n::format!("rd0a2e3d505")
            }
            Some(expected) => summon_i18n::format!("rba8be7a25e"),
            None => summon_i18n::format!("re5fb96c227"),
        },
    };

    summon_i18n::format!("rfbb919b46d")
}

/// 按优先级挑第一个真实存在的候选目录。`candidates` 必须已按优先级从高到低排好,
/// 空串路径视作「没配」由调用方过滤掉(本函数不做字符串清洗)。
pub fn plan_work_dir(candidates: Vec<WorkDirCandidate>) -> WorkDirPlan {
    let mut stale = Vec::new();
    for candidate in candidates {
        if candidate.exists {
            return WorkDirPlan::Use {
                path: candidate.path,
                source: candidate.source,
                stale,
            };
        }
        stale.push(candidate);
    }
    if stale.is_empty() {
        WorkDirPlan::Unconfigured
    } else {
        WorkDirPlan::AllMissing { stale }
    }
}

/// 陈旧工作目录提醒卡正文:更高优先级的配置候选目录已不存在、启动侧已回退到 `fallback` 时,
/// 给用户的一次性提醒 —— 旧路径失效、现用哪个、怎么用 `/cd` 重设。无陈旧候选 → `None`
/// (没有回退就没有需要提醒的事)。
///
/// 纯函数(字符串进字符串出),IO 与发卡由调用方做:本仓约定 core 不碰文件系统 / 网络。
/// **不替代修复动作**:陈旧 `.cwd` 覆盖仍留在盘上(owner 2026-08-18 明确不自动清),
/// 提醒的目的是让用户自己发 `/cd <新路径>` 换掉它。
pub fn stale_work_dir_notice(
    stale: &[WorkDirCandidate],
    fallback: &str,
    fallback_source: WorkDirSource,
) -> Option<String> {
    if stale.is_empty() {
        return None;
    }
    let gone = stale
        .iter()
        .map(|c| format!("- {}:`{}`", c.source.label(), c.path))
        .collect::<Vec<_>>()
        .join("\n");
    Some(summon_i18n::format!("r66e6619f65", fallback_source.label()))
}

/// work_dir 私信选择器的候选分档,按推荐优先级从高到低。同群其他 agent 正用的目录是当前语境
/// 最相关的推荐,故置顶;本 agent 自己用过的次之;全 fleet 常用垫底。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkDirBucket {
    /// ① 同 chat_id 下**其他** agent 的 per-chat `.cwd`(权重最高)。
    SameChatOtherAgent,
    /// ② 本 agent 用过的目录(其它 chat 的 `.cwd` + agent 默认)。
    SelfAgent,
    /// ③ 全 fleet 其余 agent 的默认 / `.cwd`,去重后垫底。
    Fleet,
}

impl WorkDirBucket {
    /// 排序键(小=靠前)。
    fn priority(self) -> u8 {
        match self {
            WorkDirBucket::SameChatOtherAgent => 0,
            WorkDirBucket::SelfAgent => 1,
            WorkDirBucket::Fleet => 2,
        }
    }

    /// 私信卡里给人看的短来源提示。
    pub fn hint(self) -> &'static str {
        match self {
            WorkDirBucket::SameChatOtherAgent => summon_i18n::text!("reeb89b48c1"),
            WorkDirBucket::SelfAgent => summon_i18n::text!("rf0ba4003af"),
            WorkDirBucket::Fleet => summon_i18n::text!("ra40bea81cd"),
        }
    }
}

/// 扫盘产出的一个候选原料(未去重未排序)。`path` 应为**已 expand_tilde 的绝对路径**(去重按它);
/// `exists` 由 IO 层探(`Path::is_dir`),不存在的(`.cwd` 指向已删目录)在 rank 时滤掉。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkDirOption {
    pub path: String,
    pub bucket: WorkDirBucket,
    pub exists: bool,
}

impl WorkDirOption {
    pub fn new(path: impl Into<String>, bucket: WorkDirBucket, exists: bool) -> Self {
        Self {
            path: path.into(),
            bucket,
            exists,
        }
    }
}

/// 排序去重后的候选。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RankedWorkDir {
    pub path: String,
    pub bucket: WorkDirBucket,
}

/// 候选聚合排序(**纯函数**,IO 层备好素材再喂进来):
/// 1. 滤掉不存在的目录(`.cwd` 指向已删)。
/// 2. 按 bucket 优先级**稳定**排序(bucket 内保输入序 —— IO 层可预排成 recency)。
/// 3. 按 `normalize_work_dir` 归一后去重,**保留最高优先级 bucket 的那次出现**(同一路径既是
///    同群又在 fleet,记同群)。两侧都是 expand_tilde 后的绝对路径,归一后字符串比较即可。
pub fn rank_work_dir_candidates(options: Vec<WorkDirOption>) -> Vec<RankedWorkDir> {
    let mut present: Vec<WorkDirOption> = options.into_iter().filter(|o| o.exists).collect();
    // slice::sort_by_key 稳定:同 bucket 内保持输入顺序。
    present.sort_by_key(|o| o.bucket.priority());
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut out = Vec::new();
    for opt in present {
        if seen.insert(normalize_work_dir(&opt.path)) {
            out.push(RankedWorkDir {
                path: opt.path,
                bucket: opt.bucket,
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(source: WorkDirSource, path: &str, exists: bool) -> WorkDirCandidate {
        WorkDirCandidate::new(source, path, exists)
    }

    /// 归一必须与内核对 cwd 的处理一致:进程起在 `/a/b/` 里,它自报的 cwd 就是 `/a/b`。
    /// summon 侧若保留字面写法,`project_dirname` 会为同一个目录算出两个名字。
    #[test]
    fn normalize_work_dir_matches_what_the_kernel_reports() {
        assert_eq!(
            normalize_work_dir("/Users/me/Desktop/My_App/"),
            "/Users/me/Desktop/My_App"
        );
        assert_eq!(normalize_work_dir("/a/b//"), "/a/b");
        assert_eq!(normalize_work_dir("/a//b"), "/a/b");
        assert_eq!(normalize_work_dir("/a/./b"), "/a/b");
        assert_eq!(normalize_work_dir("  /a/b/  "), "/a/b");
        assert_eq!(normalize_work_dir("/"), "/", "根目录不能被削成空串");
        assert_eq!(normalize_work_dir(""), "");
        assert_eq!(normalize_work_dir("   "), "");
        assert_eq!(normalize_work_dir("/a/b"), "/a/b", "已是规范形态则原样");
    }

    /// `..` 要解析必须读文件系统,而本模块不碰 IO —— 原样留着,让下游的存在性校验去判。
    #[test]
    fn normalize_work_dir_keeps_parent_segments() {
        assert_eq!(normalize_work_dir("/a/b/../c"), "/a/b/../c");
    }

    /// 归一收在候选构造的唯一入口:四个来源都是人写的字符串,写法差异不该漏进判定链。
    #[test]
    fn candidate_normalizes_path_from_every_source() {
        for source in [
            WorkDirSource::Explicit,
            WorkDirSource::ChatOverride,
            WorkDirSource::ChatMapping,
            WorkDirSource::AgentDefault,
        ] {
            assert_eq!(cand(source, "/proj/a/", true).path, "/proj/a");
        }
    }

    /// 回归(2026-09-08 现场):`.cwd` 里存的是 `/Users/example/Desktop/My_App/`,选中的目录
    /// 带着尾斜杠一路传到转录定位,而 claude 写在无尾斜杠那个目录下,首轮 watcher 死等超时。
    #[test]
    fn selected_path_is_normalized_so_transcript_lookup_matches_the_cli() {
        let plan = plan_work_dir(vec![cand(
            WorkDirSource::ChatOverride,
            "/Users/example/Desktop/My_App/",
            true,
        )]);
        assert_eq!(
            plan,
            WorkDirPlan::Use {
                path: "/Users/example/Desktop/My_App".to_string(),
                source: WorkDirSource::ChatOverride,
                stale: Vec::new(),
            }
        );
    }

    #[test]
    fn uses_highest_priority_candidate_when_it_exists() {
        let plan = plan_work_dir(vec![
            cand(WorkDirSource::ChatOverride, "/proj/a", true),
            cand(WorkDirSource::AgentDefault, "/agent/home", true),
        ]);
        assert_eq!(
            plan,
            WorkDirPlan::Use {
                path: "/proj/a".to_string(),
                source: WorkDirSource::ChatOverride,
                stale: Vec::new(),
            },
            "覆盖存在时必须用覆盖,且不报陈旧"
        );
    }

    #[test]
    fn falls_back_to_agent_default_when_chat_override_is_gone() {
        let plan = plan_work_dir(vec![
            cand(WorkDirSource::ChatOverride, "/deleted/worktree", false),
            cand(WorkDirSource::AgentDefault, "/agent/home", true),
        ]);
        assert_eq!(
            plan,
            WorkDirPlan::Use {
                path: "/agent/home".to_string(),
                source: WorkDirSource::AgentDefault,
                // 陈旧覆盖必须被带出来:调用方据此打 warning,否则 owner 永远不知道盘上还躺着它。
                stale: vec![cand(
                    WorkDirSource::ChatOverride,
                    "/deleted/worktree",
                    false
                )],
            },
            "覆盖目录没了必须回退 agent 级,并报告被跳过的陈旧覆盖"
        );
    }

    /// 本 bug 的核心断言:配了目录但都不存在时,判定必须是**明确失败**,
    /// 绝不能退化成「没选出来」而被调用方兜底成 `$HOME` / 进程 cwd。
    #[test]
    fn reports_all_missing_instead_of_falling_through_to_process_cwd() {
        let plan = plan_work_dir(vec![
            cand(WorkDirSource::ChatOverride, "/deleted/worktree", false),
            cand(WorkDirSource::AgentDefault, "/also/gone", false),
        ]);
        assert_eq!(
            plan,
            WorkDirPlan::AllMissing {
                stale: vec![
                    cand(WorkDirSource::ChatOverride, "/deleted/worktree", false),
                    cand(WorkDirSource::AgentDefault, "/also/gone", false),
                ],
            },
            "都不存在必须明确失败,而不是 Unconfigured(后者会被兜底成 $HOME)"
        );
        assert_ne!(
            plan,
            WorkDirPlan::Unconfigured,
            "AllMissing 与 Unconfigured 永不可合并"
        );
    }

    #[test]
    fn uses_agent_default_when_no_override_configured() {
        let plan = plan_work_dir(vec![cand(WorkDirSource::AgentDefault, "/agent/home", true)]);
        assert_eq!(
            plan,
            WorkDirPlan::Use {
                path: "/agent/home".to_string(),
                source: WorkDirSource::AgentDefault,
                stale: Vec::new(),
            },
            "无覆盖时用 agent 级 working_dir"
        );
    }

    #[test]
    fn empty_candidates_mean_unconfigured_not_failure() {
        assert_eq!(
            plan_work_dir(Vec::new()),
            WorkDirPlan::Unconfigured,
            "压根没配不是错误:调用方沿用进程 cwd 兜底,行为不变"
        );
    }

    #[test]
    fn pwd_report_distinguishes_config_runtime_and_drift() {
        let plan = WorkDirPlan::Use {
            path: "/configured/repo".into(),
            source: WorkDirSource::ChatMapping,
            stale: Vec::new(),
        };
        let stopped = pwd_report(&plan, RuntimeWorkDir::Stopped, "/daemon");
        assert!(stopped.contains("/configured/repo"));
        assert!(stopped.contains("session 未运行"));

        let aligned = pwd_report(
            &plan,
            RuntimeWorkDir::Running(Some("/configured/repo".into())),
            "/daemon",
        );
        assert!(aligned.contains("运行目录"));
        assert!(aligned.contains("与配置一致"));

        let drifted = pwd_report(
            &plan,
            RuntimeWorkDir::Running(Some("/actual/repo".into())),
            "/daemon",
        );
        assert!(drifted.contains("/actual/repo"));
        assert!(drifted.contains("目录漂移"));
        assert!(drifted.contains("下一次冷启或重连"));
    }

    #[test]
    fn pwd_report_handles_unconfigured_and_missing_without_guessing() {
        let unconfigured = pwd_report(
            &WorkDirPlan::Unconfigured,
            RuntimeWorkDir::Stopped,
            "/daemon/cwd",
        );
        assert!(unconfigured.contains("未配置"));
        assert!(unconfigured.contains("/daemon/cwd"));

        let missing = pwd_report(
            &WorkDirPlan::AllMissing {
                stale: vec![cand(WorkDirSource::AgentDefault, "/gone", false)],
            },
            RuntimeWorkDir::Running(None),
            "/daemon/cwd",
        );
        assert!(missing.contains("配置目录均不存在"));
        assert!(missing.contains("运行目录不可得"));

        let stopped_missing = pwd_report(
            &WorkDirPlan::AllMissing {
                stale: vec![cand(WorkDirSource::AgentDefault, "/gone", false)],
            },
            RuntimeWorkDir::Stopped,
            "/daemon/cwd",
        );
        assert!(stopped_missing.contains("下次启动失败"));
        assert!(!stopped_missing.contains("以上为下次启动目录"));
    }

    /// 优先级链完整走一遍:显式 > per-chat 覆盖 > 映射 > agent 级,前三级全没了才落到最后一级。
    #[test]
    fn skips_every_missing_level_in_priority_order() {
        let plan = plan_work_dir(vec![
            cand(WorkDirSource::Explicit, "/explicit", false),
            cand(WorkDirSource::ChatOverride, "/override", false),
            cand(WorkDirSource::ChatMapping, "/mapping", false),
            cand(WorkDirSource::AgentDefault, "/default", true),
        ]);
        match plan {
            WorkDirPlan::Use {
                path,
                source,
                stale,
            } => {
                assert_eq!(path, "/default");
                assert_eq!(source, WorkDirSource::AgentDefault);
                assert_eq!(
                    stale.iter().map(|c| c.source).collect::<Vec<_>>(),
                    vec![
                        WorkDirSource::Explicit,
                        WorkDirSource::ChatOverride,
                        WorkDirSource::ChatMapping
                    ],
                    "被跳过的候选必须按优先级顺序完整列出"
                );
            }
            other => panic!("应选中 agent 级默认目录,实得 {other:?}"),
        }
    }

    /// 存在的低优先级候选不会被更高优先级的**存在**候选顶替之外的任何原因选中。
    #[test]
    fn never_picks_a_lower_level_while_a_higher_one_exists() {
        let plan = plan_work_dir(vec![
            cand(WorkDirSource::ChatOverride, "/override", true),
            cand(WorkDirSource::ChatMapping, "/mapping", true),
            cand(WorkDirSource::AgentDefault, "/default", true),
        ]);
        assert!(
            matches!(plan, WorkDirPlan::Use { ref source, .. } if *source == WorkDirSource::ChatOverride),
            "高优先级存在时绝不越级"
        );
    }

    #[test]
    fn stale_notice_lists_gone_paths_fallback_and_cd_hint() {
        let notice = stale_work_dir_notice(
            &[cand(WorkDirSource::ChatOverride, "/old/worktree", false)],
            "/agent/home",
            WorkDirSource::AgentDefault,
        )
        .expect("有陈旧候选必须出提醒");
        assert!(
            notice.contains("/old/worktree"),
            "提醒须含失效旧路径: {notice}"
        );
        assert!(
            notice.contains("/agent/home"),
            "提醒须含回退后的现用目录: {notice}"
        );
        assert!(
            notice.contains(WorkDirSource::AgentDefault.label()),
            "提醒须含回退来源,让用户知道去哪改: {notice}"
        );
        assert!(
            notice.contains("/cd"),
            "提醒须教用户怎么用 /cd 重设: {notice}"
        );
    }

    #[test]
    fn stale_notice_is_none_when_no_candidate_was_skipped() {
        assert_eq!(
            stale_work_dir_notice(&[], "/agent/home", WorkDirSource::AgentDefault),
            None,
            "没有陈旧候选时不提醒"
        );
    }

    #[test]
    fn stale_notice_lists_every_gone_candidate() {
        // 多级配置同时失效(ChatOverride + ChatMapping 都没了)时,提醒卡必须把失效路径**列全**,
        // 漏一条用户就少一处要清理的陈旧配置。
        let notice = stale_work_dir_notice(
            &[
                cand(WorkDirSource::ChatOverride, "/old/override", false),
                cand(WorkDirSource::ChatMapping, "/old/mapping", false),
            ],
            "/agent/home",
            WorkDirSource::AgentDefault,
        )
        .expect("有陈旧候选必须出提醒");
        assert!(
            notice.contains("/old/override") && notice.contains("/old/mapping"),
            "提醒须列全所有失效路径: {notice}"
        );
        assert!(
            notice.contains(WorkDirSource::ChatOverride.label())
                && notice.contains(WorkDirSource::ChatMapping.label()),
            "提醒须逐条标明来源,用户才知道去哪改: {notice}"
        );
    }

    fn opt(path: &str, bucket: WorkDirBucket, exists: bool) -> WorkDirOption {
        WorkDirOption::new(path, bucket, exists)
    }

    #[test]
    fn rank_orders_same_group_agent_highest_then_self_then_fleet() {
        let ranked = rank_work_dir_candidates(vec![
            opt("/fleet/x", WorkDirBucket::Fleet, true),
            opt("/self/recent", WorkDirBucket::SelfAgent, true),
            opt("/peer/worktree", WorkDirBucket::SameChatOtherAgent, true),
        ]);
        assert_eq!(
            ranked.iter().map(|r| r.path.as_str()).collect::<Vec<_>>(),
            vec!["/peer/worktree", "/self/recent", "/fleet/x"],
            "同群其他 agent > 本 agent > fleet"
        );
    }

    #[test]
    fn rank_dedups_by_normalized_path_keeping_highest_bucket() {
        // 同一路径(末尾斜杠不同)在 fleet 与同群各出现一次 → 去重后只留同群那次。
        let ranked = rank_work_dir_candidates(vec![
            opt("/shared/dir/", WorkDirBucket::Fleet, true),
            opt("/shared/dir", WorkDirBucket::SameChatOtherAgent, true),
            opt("/self/a", WorkDirBucket::SelfAgent, true),
        ]);
        assert_eq!(ranked.len(), 2, "同路径去重: {ranked:?}");
        assert_eq!(ranked[0].bucket, WorkDirBucket::SameChatOtherAgent);
        assert_eq!(ranked[0].path, "/shared/dir");
        assert_eq!(ranked[1].bucket, WorkDirBucket::SelfAgent);
    }

    #[test]
    fn rank_drops_nonexistent_dirs() {
        // .cwd 指向已删目录(exists=false)→ 不进候选。
        let ranked = rank_work_dir_candidates(vec![
            opt("/gone", WorkDirBucket::SameChatOtherAgent, false),
            opt("/live", WorkDirBucket::Fleet, true),
        ]);
        assert_eq!(
            ranked.iter().map(|r| r.path.as_str()).collect::<Vec<_>>(),
            vec!["/live"],
            "不存在的目录必须滤除"
        );
    }

    #[test]
    fn rank_preserves_input_order_within_a_bucket() {
        // 同 bucket 内保输入序(IO 层预排成 recency)。
        let ranked = rank_work_dir_candidates(vec![
            opt("/self/newest", WorkDirBucket::SelfAgent, true),
            opt("/self/older", WorkDirBucket::SelfAgent, true),
        ]);
        assert_eq!(
            ranked.iter().map(|r| r.path.as_str()).collect::<Vec<_>>(),
            vec!["/self/newest", "/self/older"]
        );
    }

    #[test]
    fn rank_empty_when_all_filtered() {
        assert!(
            rank_work_dir_candidates(vec![opt("/gone", WorkDirBucket::Fleet, false)]).is_empty()
        );
        assert!(rank_work_dir_candidates(Vec::new()).is_empty());
    }
}
