use crate::{config::config::MessagingOverride, driver::CliDriver};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// 捕获源选择:每个 agent 的输出如何抓取。
///
/// 2026-08-20 砍掉过 `CliSession`(当时 0 个 agent 在走)。2026-09-03 作为 **Cursor 显式
/// opt-in** 加回来:`runtime.capture: cli` → `cursor -p --resume`。默认仍是 Rpc(ACP)。
/// 没有恢复 `Subprocess`(codex `exec` 逃生门)——codex 仍只有一条通道。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureMode {
    /// Transcript 模式:claude 交互式跑在 tmux,本轮回复 tail claude 自己的结构化 jsonl
    /// 转录(append-only,无 marker)。**claude 唯一的捕获通道**。
    Transcript,
    /// Cursor Headless CLI Session:`cursor-agent -p --output-format stream-json`,上下文由
    /// provider 持久化,后续轮 `--resume=<sid>`。进程仍是一轮一个,但 session 是一等资源。
    /// 仅 Cursor + 显式 `runtime.capture: cli|exec` 走这条;ACP 与 `-p` 的 sid **不互通**。
    CliSession,
    /// 原生常驻协议(codex `app-server` JSON-RPC / cursor ACP,都跑在 stdio 上):每 chat 一个
    /// 进程,懒建懒重连,一轮 = 投递 prompt + 消费事件流。codex / traex 默认;cursor 缺省也走这条。
    ///
    /// **它没有 tmux、不能 attach、观测靠事件流** —— 所有「Transcript vs 其它」的判定都该把
    /// 它归到「其它」那边(见 `capture_mode` 调用点)。
    Rpc,
}

impl CaptureMode {
    /// 是否是无 tmux 的无头形态。调用点表达的是「有没有 tmux 可 attach」,不是某个具体变体。
    pub fn is_headless(self) -> bool {
        matches!(self, CaptureMode::CliSession | CaptureMode::Rpc)
    }
}

/// `runtime.capture` 的取值:Cursor 走哪条捕获通道的**显式**覆盖(缺省=ACP/Rpc)。
///
/// 存在的理由:ACP 不发 `usage_update`,而 `-p` 的 `result.usage` 有 token 数;以及 ACP
/// 传输层偶发 stall 后空卡收尾。只给 Cursor 用 —— codex 两条通道共用同一套 id,切开会丢上下文。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureOverride {
    /// `cursor -p` + 原生 sid / `--resume`。`exec` / `subprocess` / `session` 是兼容别名。
    Cli,
    /// 显式选用 ACP 常驻协议(也是 Cursor 缺省)。
    Rpc,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("{}", summon_i18n::format!("rb48f851b35", value = .value))]
pub struct CaptureOverrideError {
    pub value: String,
}

impl CaptureOverride {
    /// **写入时**的严格校验。任何把值写进 `agent.yaml` 的路径都该先过这里。
    pub fn parse(value: &str) -> Result<Self, CaptureOverrideError> {
        match value.trim().to_ascii_lowercase().as_str() {
            "cli" | "session" | "headless-cli" | "exec" | "subprocess" => Ok(CaptureOverride::Cli),
            "rpc" => Ok(CaptureOverride::Rpc),
            _ => Err(CaptureOverrideError {
                value: value.to_string(),
            }),
        }
    }
}

/// 这个飞书身份是**干活的**还是**管事的**。
///
/// 加这一维而不是另起一套并行机制,是因为凭据加载、sidecar app 下发、`routes` 路由、卡片出站、
/// operate-gate、热加载这六样恰恰是最不该有第二份的东西 —— steward 需要它们**全部**,
/// 唯一的差别只在「收到消息之后干什么」。角色是一个维度,不是一套系统。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentRole {
    /// 常规 agent:消息进会话层,由 CLI 作答。缺省。
    #[default]
    Agent,
    /// 管家:只处理 fleet 管理命令,**永不进会话层、不碰 tmux、不消耗模型额度**。
    /// 它是 daemon 的一条同步路径(收消息 → 读快照 → 回卡片),与 sentinel 同族。
    Steward,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Agent {
    pub name: String,
    #[serde(default)]
    pub display: String,
    #[serde(default = "default_status")]
    pub status: String,
    /// 见 [`AgentRole`]。**坏值不该让整份 registry 加载失败**,但这里用强类型枚举 ——
    /// 与 `capture` 存字符串的取舍不同:capture 写错只是降级到默认通道,而 role 写错
    /// (比如想写 steward 拼成 stewart)会让一个本该只读的身份变成能起会话的 agent,
    /// 那是权限方向的静默升级,必须在加载时就炸出来。
    #[serde(default)]
    pub role: AgentRole,
    /// 启动该 agent 的 CLI 时传给 `--model` 的模型名(各 CLI 私有命名空间,如 cursor
    /// `gpt-5.6-sol-high`)。
    /// 是否真的下发由 driver 决定(见 `CliDriver::model_flag`);空/缺省则用 CLI 默认模型。
    #[serde(default)]
    pub model: Option<String>,
    /// 逻辑 reasoning effort 档位(规范化小写:none/minimal/low/medium/high/xhigh/max)。
    /// 各 CLI 表达形态不同(claude `--effort`、codex `-c model_reasoning_effort=`;cursor 在 bind 时
    /// 已把档位烤进 model 名、故此字段只服务 claude/codex),解析收口在 driver。空/缺省=不指定,用 CLI 默认。
    #[serde(default)]
    pub effort: Option<String>,
    /// codex 专用:是否吐思考摘要 none/concise/auto/detailed(空=回退用户 ~/.codex/config.toml,
    /// 再回退 detailed)。why:gpt-5.x 默认 model_reasoning_summary=none → exec --json 不吐 reasoning、
    /// summon 双卡退化单卡;解析/注入收口在 driver(`codex::reasoning_summary_decision`)。
    #[serde(default)]
    pub reasoning_summary: Option<String>,
    pub feishu: Option<Feishu>,
    pub runtime: Option<Runtime>,
    /// open_id(ou_xxx)→ 昵称。注入会话的 prompt 用昵称称呼发送人;
    /// 未命中再回退 contact.v3 真名。agent.toml `[aliases]` 表配置。
    #[serde(default)]
    pub aliases: HashMap<String, String>,
    /// 该 agent 对全局 `[messaging]` 的逐字段覆盖(agent.toml `[messaging]`,全可选)。
    #[serde(default)]
    pub messaging: Option<MessagingOverride>,
    /// agent 自我认知目录(load 时填;手动构造为空)。
    #[serde(skip)]
    pub dir: PathBuf,
    /// 所属机器的 config.yaml；未绑定时使用 agents 目录的同级配置。
    #[serde(skip)]
    pub global_config_path: Option<PathBuf>,
    /// 飞书应用所有者,运行期由 daemon 查询后填写(见 [`Agent::can_operate`])。
    #[serde(skip)]
    pub app_owner: AppOwnerSlot,
}

/// 飞书应用所有者(企业成员)。open_id 属于本 app 的命名空间,union_id 同开发者下跨 app 稳定。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppOwner {
    pub open_id: String,
    pub union_id: String,
}

/// 应用所有者的查询状态。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum AppOwnerState {
    /// 还没查到(尚未查询、查询失败,或进程里没有可用缓存)。
    #[default]
    Unknown,
    /// 已查到:`None` 表示所有者不是企业成员(飞书科技、合作伙伴),没有可放行的人。
    Known(Option<AppOwner>),
}

/// 应用所有者的共享槽:clone 与 [`Agent::reread`] 都指向同一份,daemon 填一次,所有判权副本可见。
#[derive(Debug, Clone, Default)]
pub struct AppOwnerSlot(std::sync::Arc<std::sync::RwLock<AppOwnerState>>);

impl AppOwnerSlot {
    /// 锁中毒时按未知处理,判权随之 fail-closed。
    pub fn get(&self) -> AppOwnerState {
        self.0.read().map(|state| state.clone()).unwrap_or_default()
    }

    pub fn set(&self, state: AppOwnerState) {
        if let Ok(mut slot) = self.0.write() {
            *slot = state;
        }
    }
}

fn default_status() -> String {
    "active".to_string()
}

#[derive(Debug, Clone, Deserialize)]
pub struct Feishu {
    pub app_id_env: Option<String>,
    pub app_secret_env: Option<String>,
    #[serde(default)]
    pub scope: String,
    /// **talk 名单**:谁能让该 agent 应答(空 = 全放行)。见 `allows`。
    #[serde(default)]
    pub allowed_users: Vec<String>,
    /// **operate 名单**:谁能做破坏性操作(点卡片「清理」kill 会话)。与 talk 解耦。
    /// 缺省继承 config.yaml；显式空名单回退到 `allowed_users`。见 `can_operate`。
    #[serde(default)]
    pub operators: Option<Vec<String>>,
    /// **本群 operate 名单**:键 = chat_id,值 = 只在该 chat 里有 operate 权限的人。
    /// 与 agent 级名单取并集(只加不减),且只对效果落在该 chat 上的动作生效,见 [`OperateScope`]。
    /// ```yaml
    /// feishu:
    ///   chat_operators:
    ///     oc_abc123: ["ou_xxx", "on_yyy"]
    /// ```
    #[serde(default)]
    pub chat_operators: HashMap<String, Vec<String>>,
    /// 按精确 chat ID 开启一人一 Bot 群的普通消息免 @ 回复。
    #[serde(default)]
    pub chat_reply_without_mention: HashMap<String, bool>,
}

/// operate 动作的效果落在哪 —— 决定本群名单(`feishu.chat_operators`)算不算数。
///
/// 本群 operator 的权力不能漏出那个群:效果跨 chat 的动作(agent 共享配置、本机外部会话、
/// 跨 agent)只认 agent 级名单,否则「只给这个群开权限」就成了「给整个 agent 开权限」。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperateScope<'a> {
    /// 效果只落在这个 chat 上:agent 级名单 ∪ 该 chat 的本群名单。
    Chat(&'a str),
    /// 效果跨 chat:只认 agent 级名单。
    Agent,
}

/// operate-gate 拒绝的原因,供拒绝回复说明缺哪项、该改哪个字段。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperateDenial {
    /// `operators` 与 `allowed_users` 都为空:群里不开放操作。
    NoListConfigured,
    /// 生效名单(`operators`,为空时回退的 `allowed_users`)里没有此人。
    NotListed(OperateList),
}

/// operate-gate 生效的那份 agent 级名单。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperateList {
    Operators,
    AllowedUsers,
}

impl OperateList {
    /// agent.yaml `feishu:` 下的字段名。
    pub fn field(self) -> &'static str {
        match self {
            Self::Operators => "operators",
            Self::AllowedUsers => "allowed_users",
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Runtime {
    pub working_dir: Option<String>,
    /// 远程文件/命令工具绑定；云端模型与对话仍由本 agent 持有。
    pub execution: Option<crate::remote_tools::ExecutionBinding>,
    pub tmux_name: Option<String>,
    pub cli: Option<String>,
    pub cli_cmd: Option<String>,
    /// codex 家族(codex / traex)专用:per-agent 的上下文窗口 token 数。
    /// 配了 → 启动 app-server 时通过 `-c model_context_window=<值>` 传入,
    /// 不写死在全局 config.toml / traecli.toml(bot 之间隔离)。
    /// 没配 → 不传,尊重 CLI 自己的全局配置。
    #[serde(default)]
    pub model_context_window: Option<u64>,
    pub seed: Option<String>,
    /// codex 专用:允许在非 Git 聚合目录运行 `codex exec`。默认 false，显式开启后才传
    /// `--skip-git-repo-check`，避免无意绕过 Codex 的 trusted-directory 检查。
    #[serde(default)]
    pub codex_skip_git_repo_check: bool,
    /// 是否记录原生 session id 并在 tmux 会话死后 resume 续接。
    /// None = 跟随全局 `Config.resume_sessions`;Some(_) = 本 agent 显式覆盖。
    pub resume: Option<bool>,
    /// 是否走无头捕获(`claude -p` stream-json)而非 tmux 路径。None = 默认 false。
    /// claude 默认走 Transcript(tmux 交互 + tail jsonl);`headless:true` 是知情逃生舱
    /// (切到独立计费的 `claude -p`)。cursor/codex 默认即无头。
    #[serde(default)]
    pub headless: Option<bool>,
    /// 捕获通道覆盖。仅 Cursor 认 `cli`/`exec` → [`CaptureMode::CliSession`];
    /// 其余 CLI 忽略(codex 不许分通道)。缺省 / `rpc` = ACP。
    #[serde(default)]
    pub capture: Option<String>,
    /// Cursor RPC 的 legacy 参数化 seed。与顶层 `model` **分开存**：顶层仍是
    /// `cursor -p --model` 的 dash-id 命名空间；此值可用旧 bracket 形态携带
    /// model/context/reasoning/fast。新的 per-agent bundle 一旦落盘即成为唯一事实源，之后不再
    /// 回写本字段；保留它只为现有 agent 无损迁移。空/缺省 = bundle 建立前沿用 Cursor 默认。
    #[serde(default)]
    pub rpc_model: Option<String>,
    /// 唤起 CLI 前注入的额外环境变量(per-agent 可配)。
    /// 注入在 SUMMON_* 系统变量之后,用户可覆盖默认值。键值对原样传给子进程 env。
    /// 示例(agent.yaml):
    /// ```yaml
    /// runtime:
    ///   env:
    ///     ANTHROPIC_BASE_URL: "https://..."
    ///     HTTP_PROXY: "http://..."
    /// ```
    #[serde(default)]
    pub env: Option<std::collections::HashMap<String, String>>,
    /// codex 托管会话的 sandbox 档位(per-agent 覆盖)。
    /// None = 跟随 `~/.summon/config.yaml` 的 `codex.sandbox`,再没有就用内置默认。
    /// 合法值见 `codex_policy::SANDBOX_VALUES`;**非法值降级成默认并留日志**,不让会话起不来。
    #[serde(default)]
    pub codex_sandbox: Option<String>,
    /// codex 托管会话的 approval 策略(per-agent 覆盖)。同上分层。
    ///
    /// **改成非 `never` 之前先读 `CodexPolicy::approval_warning`**:托管会话没有人能答
    /// 批准框,summon 对所有审批请求一律自动拒绝,所以调松它只会让操作被拒,不会让人来把关。
    #[serde(default)]
    pub codex_approval_policy: Option<String>,
    /// cursor 专用:取本 agent 额度凭据的命令(经 `sh -c` 执行,stdout 第一行即 access token)。
    /// 命令在本 agent 的环境里跑:系统基础环境加上本 agent 的 `runtime.env`,与它的 cursor
    /// 子进程看到的一致,所以命令里引用的 `$XDG_CONFIG_HOME` 之类就是 agent 实际用的那个。
    ///
    /// 配了 `cli_cmd` 或 `env` 的 agent 可能登录的是另一个 Cursor 账号,summon 证明不了它和本机
    /// 默认登录态是同一账号,就不替它查额度 —— 拿错账号的额度比不显示更糟。这类 agent 要显示
    /// 额度,在这里给出它自己的凭据来源;配了就以它为准,全局 `cursor.quota.token_command` 不再参与。
    #[serde(default)]
    pub cursor_quota_token_command: Option<String>,
    /// 按 chat_id 绑定的工作目录(per-chat 持久映射,无需每次 /cd)。
    /// 优先级:per-chat .cwd(/cd 设的) > 此映射 > agent 级 working_dir。
    /// 键 = chat_id(如 `oc_xxx` 群或 `ou_xxx` 私聊),值 = 目录路径(支持 ~/)。
    /// 示例:
    /// ```yaml
    /// runtime:
    ///   chat_working_dirs:
    ///     oc_abc123: "~/projects/demo"
    ///     oc_def456: "~/projects/other"
    /// ```
    #[serde(default)]
    pub chat_working_dirs: Option<std::collections::HashMap<String, String>>,
}

#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    #[error("agents dir not found: {0}")]
    NoDir(PathBuf),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("parse {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: serde_yaml::Error,
    },
    #[error("state migration: {0}")]
    Migration(String),
}

pub fn expand_tilde(p: &str) -> String {
    if let Some(rest) = p.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest).to_string_lossy().into_owned();
        }
    }
    p.to_string()
}

/// `expand_tilde` 的逆:把路径开头的 home 目录折回 `~`,**仅用于展示**(别把 `/Users/<人>` 暴露在
/// session 卡片等所有授权用户可见处)。非 home 前缀原样返回。提交回来的 `~/...` 由 `expand_tilde`
/// 还原,故 form 默认值用它折叠也能正确 round-trip。
pub fn collapse_tilde(p: &str) -> String {
    let Some(home) = dirs::home_dir() else {
        return p.to_string();
    };
    let home = home.to_string_lossy();
    let home = home.trim_end_matches('/');
    if home.is_empty() {
        return p.to_string();
    }
    if p == home {
        return "~".to_string();
    }
    match p.strip_prefix(home).and_then(|r| r.strip_prefix('/')) {
        Some(rest) => format!("~/{rest}"),
        None => p.to_string(),
    }
}

/// chat_id 的稳定 16 位 hex 标识(完整 64-bit FNV-1a,跨重编译/Rust 版本恒定,无新依赖)。
/// 不用 DefaultHasher:其结果不保证跨版本稳定,会孤立持久 tmux 会话。用全 64 位(不截断)
/// 把碰撞概率压到可忽略 —— 碰撞会导致两个 chat 撞同名 tmux 会话、上下文串台。
fn fnv1a_hex(s: &str) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

/// per-chat 落盘/命名用的安全短 tag:群=`g`+8hex,私聊=`p`+8hex。绝不含 tmux 危险字符。
pub fn chat_tag(chat_type: &str, chat_id: &str) -> String {
    let prefix = if matches!(chat_type, "group" | "topic_group") {
        'g'
    } else {
        'p'
    };
    format!("{prefix}{}", fnv1a_hex(chat_id))
}

/// 机器状态子目录名(迁移与回退判定共用这一份清单)。
/// authored 文件(agent.yaml / SOUL.md / .env / memory/)不在此列 —— 它们永远留在 agent 目录。
pub const STATE_SUBDIRS: &[&str] = &[
    "runtime",
    "sessions",
    "threads",
    "inbound",
    "cursor-config",
    "cursor-acp-sessions",
];

/// 机器状态**文件**(与 [`STATE_SUBDIRS`] 的目录对应):agent 根目录下的散落状态文件,
/// 迁移时一并挪到 `state/<alias>/`。目前只有 cursor 参数 bundle。
pub const STATE_FILES: &[&str] = &["cursor-parameters.json"];

/// 机器状态根目录:`~/.summon/state`(agents_dir 的兄弟)。
pub fn state_root(agents_dir: &Path) -> PathBuf {
    agents_dir.parent().unwrap_or(agents_dir).join("state")
}

/// 某 agent 的机器状态目录,**带回退**(迁移失败/未迁移时旧路径仍可读):
///
/// - 新位置 `state/<alias>/` 存在 → 用它(已迁移);
/// - 旧位置 `agents/<alias>/` 有任一机器状态子目录 → 回退旧位置(未迁移/迁移失败);
/// - 否则(全新 agent)→ 新位置(直接写新布局,不再产生旧布局)。
///
/// 为什么带回退:daemon 启动迁移是「移新位置」,但迁移可能失败(权限/跨设备)或还没跑;
/// 回退保证旧布局在任何时候都可读。漏改一个消费方也只是读到旧位置(见 `migrate_agent_state`)。
pub fn agent_state_dir(agents_dir: &Path, alias: &str) -> PathBuf {
    let new = state_root(agents_dir).join(alias);
    if new.exists() {
        return new;
    }
    let old = agents_dir.join(alias);
    if STATE_SUBDIRS.iter().any(|s| old.join(s).exists()) {
        return old;
    }
    new
}

/// daemon 启动时一次性迁移:把每个 agent 目录下的机器状态(子目录 + 散落文件)挪到 `state/<alias>/`。
/// **per-agent 全有或全无**:一个 agent 的多个状态项,要么全移成功,要么回滚到旧布局 ——
/// 避免「runtime 移走了、sessions 还在」的劈叉状态(那比不迁移更糟)。
/// 失败不致命:返回错误,调用方记日志后继续(旧布局照旧工作,`agent_state_dir` 回退读旧)。
pub fn migrate_agent_state(agents_dir: &Path) -> Result<(), RegistryError> {
    let state_root = state_root(agents_dir);
    let mut migrated = 0usize;
    let mut skipped = 0usize;
    for entry in std::fs::read_dir(agents_dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let old_agent = entry.path();
        let alias = match entry.file_name().to_str() {
            Some(a) => a.to_string(),
            None => continue,
        };
        // 只迁有机器状态的 agent;纯 authored 目录跳过。
        let to_move: Vec<&str> = STATE_SUBDIRS
            .iter()
            .copied()
            .filter(|s| old_agent.join(s).exists())
            .chain(
                STATE_FILES
                    .iter()
                    .copied()
                    .filter(|s| old_agent.join(s).exists()),
            )
            .collect();
        if to_move.is_empty() {
            continue;
        }
        let new_agent = state_root.join(&alias);
        std::fs::create_dir_all(&new_agent)?;
        // 逐个 rename(同盘原子);任一失败则把已移的挪回去,保证该 agent 不劈叉。
        let mut moved: Vec<&str> = Vec::new();
        for item in &to_move {
            let from = old_agent.join(item);
            let to = new_agent.join(item);
            if to.exists() {
                // 目标已存在(上次迁移残留?)—— 不覆盖,跳过这个项。
                continue;
            }
            if let Err(e) = std::fs::rename(&from, &to) {
                // 回滚已移的项
                for m in &moved {
                    let _ = std::fs::rename(new_agent.join(m), old_agent.join(m));
                }
                return Err(RegistryError::Migration(summon_i18n::format!(
                    "r76c3925462"
                )));
            }
            moved.push(item);
        }
        if moved.is_empty() {
            skipped += 1;
        } else {
            migrated += 1;
        }
    }
    if migrated > 0 {
        eprintln!(
            "{}",
            summon_i18n::format!("r63a6697d7d", state_root.display())
        );
    }
    Ok(())
}

impl Agent {
    pub fn tmux_name(&self) -> String {
        self.runtime
            .as_ref()
            .and_then(|r| r.tmux_name.clone())
            .unwrap_or_else(|| format!("soul-{}", self.name))
    }

    /// per-chat tmux 会话名。owner 私聊保留裸名(向后兼容、复用现存会话);
    /// 其余 = `<base>-<tag>`。base 取 `tmux_name()`(可被 [runtime].tmux_name 覆盖)。
    pub fn tmux_name_for_chat(&self, chat_type: &str, chat_id: &str, is_owner_p2p: bool) -> String {
        if is_owner_p2p {
            return self.tmux_name();
        }
        format!("{}-{}", self.tmux_name(), chat_tag(chat_type, chat_id))
    }

    pub fn working_dir(&self) -> Option<String> {
        self.runtime
            .as_ref()
            .and_then(|r| r.working_dir.as_deref())
            .map(expand_tilde)
    }

    /// 是否显式允许 Codex 在非 Git 工作目录运行。其它 CLI 不消费此配置。
    pub fn codex_skip_git_repo_check(&self) -> bool {
        self.runtime
            .as_ref()
            .map(|runtime| runtime.codex_skip_git_repo_check)
            .unwrap_or(false)
    }

    /// per-agent 配置的额外环境变量(`runtime.env`),注入到子进程/tmux 会话。
    /// 返回 `Vec<(String, String)>` 便于直接拼进 `SpawnSpec.env` / `Session::with_run_env`。
    /// 未配置则返回空 vec。
    pub fn extra_env(&self) -> Vec<(String, String)> {
        self.runtime
            .as_ref()
            .and_then(|r| r.env.as_ref())
            .map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
            .unwrap_or_default()
    }

    /// 按 chat_id 查 `runtime.chat_working_dirs` 映射配置的工作目录(已展开 ~/)。
    /// 优先级链:per-chat .cwd(/cd) > 此映射 > agent 级 working_dir。
    /// 未配置或该 chat_id 不在映射里 → 返回 None。
    pub fn chat_working_dir(&self, chat_id: &str) -> Option<String> {
        self.runtime
            .as_ref()
            .and_then(|r| r.chat_working_dirs.as_ref())
            .and_then(|m| m.get(chat_id))
            .map(|s| expand_tilde(s))
    }

    /// 是否走无头结构化捕获(stream-json)。默认按 CLI 决定:
    /// cursor/codex 默认无头(参 seedclaw,各自计费、结构化捕获更稳);
    /// claude 默认走 tmux 交互——2026-06-15 起 `claude -p`/Agent SDK 独立计费、脱离订阅制,
    /// 故 Claude 留在订阅制覆盖的 TUI 路径。`runtime.headless` 显式配置则覆盖该默认(知情逃生舱)。
    pub fn headless(&self) -> bool {
        if let Some(explicit) = self.runtime.as_ref().and_then(|r| r.headless) {
            return explicit;
        }
        matches!(
            CliDriver::from_kind(self.cli_kind()),
            CliDriver::Cursor | CliDriver::Codex | CliDriver::Traex
        )
    }

    /// resume 记录里 `cli` 字段该写什么。**它是「通道」标识,不等于 `cli_kind()`**。
    ///
    /// cursor 的两条通道用的是**两个不互通的命名空间**(真机已证)—— ACP 的 `sessionId` 与
    /// `cursor -p --resume` 认的 `~/.cursor/chats` id 互相喂过去都拿不到会话。每个 chat 的
    /// resume 记录只有一条,只能靠 `cli` 字段把两条隔开:切了 `runtime.capture` 之后读不到
    /// 记录 —— 那正是要的语义,**干净冷启**,而不是拿着 acp-uuid 去 `--resume`。
    ///
    /// ACP 键名 `"cursor-acp"` 是盘上兼容契约,不能改成 `"cursor"`。
    ///
    /// codex 不分:`app-server` 的 thread id 与 `codex exec` 的 rollout 是同一套 id,
    /// 两条通道互通,分开反而会白丢上下文。
    pub fn resume_cli_kind(&self) -> String {
        match (CliDriver::from_kind(self.cli_kind()), self.capture_mode()) {
            (CliDriver::Cursor, CaptureMode::Rpc) => "cursor-acp".to_string(),
            _ => self.cli_kind().to_string(),
        }
    }

    /// `runtime.capture` 的**读时**解析,**不吞诊断**:配了但值非法时返回 `Err`(带原始值)。
    pub fn capture_override_checked(
        &self,
    ) -> Result<Option<CaptureOverride>, CaptureOverrideError> {
        match self.runtime.as_ref().and_then(|r| r.capture.as_deref()) {
            None => Ok(None),
            Some(value) => CaptureOverride::parse(value).map(Some),
        }
    }

    /// 同上,但坏值降级成 None(按默认走)+ 一条 stderr 诊断。
    pub fn capture_override(&self) -> Option<CaptureOverride> {
        match self.capture_override_checked() {
            Ok(value) => value,
            Err(error) => {
                eprintln!("{}", summon_i18n::format!("r34074202c5", self.name));
                None
            }
        }
    }

    /// 捕获源选择。两级决策,顺序不可换:
    /// 1. **非 headless 恒 Transcript**(claude tmux 交互 / Other)—— `runtime.capture` 管不着它;
    /// 2. headless 内部:codex/traex 恒 Rpc;cursor 缺省 Rpc(ACP),显式 `capture: cli` → CliSession。
    ///
    /// claude 的 `runtime.headless: true` 仍没有独立落点(没有恢复 Subprocess),掉回 Transcript。
    pub fn capture_mode(&self) -> CaptureMode {
        if !self.headless() {
            return CaptureMode::Transcript;
        }
        let driver = CliDriver::from_kind(self.cli_kind());
        match driver {
            CliDriver::Codex | CliDriver::Traex => CaptureMode::Rpc,
            CliDriver::Cursor => match self.capture_override() {
                Some(CaptureOverride::Cli) => CaptureMode::CliSession,
                Some(CaptureOverride::Rpc) | None => CaptureMode::Rpc,
            },
            _ => CaptureMode::Transcript,
        }
    }

    /// 可路由 = active 且有 feishu app_id_env 绑定
    pub fn is_routable(&self) -> bool {
        self.status == "active"
            && self
                .feishu
                .as_ref()
                .and_then(|f| f.app_id_env.as_ref())
                .is_some()
    }

    /// open_id → 配置的昵称(命中返回 Some;未命中由调用方回退真名解析)。
    pub fn alias(&self, open_id: &str) -> Option<&str> {
        self.aliases.get(open_id).map(|s| s.as_str())
    }

    /// 该 agent 的清单文件路径(`<dir>/agent.yaml`)。
    pub fn manifest_path(&self) -> PathBuf {
        self.dir.join("agent.yaml")
    }

    /// 该 agent 的**机器状态**目录(runtime/sessions/threads/inbound)。
    /// 新位置 `~/.summon/state/<alias>/`;未迁移/迁移失败时回退旧位置 `agents/<alias>/`。
    /// authored 文件(agent.yaml/SOUL.md/.env/memory)仍在 [`Self::dir`],别用这个方法取它们。
    pub fn state_dir(&self) -> PathBuf {
        let agents_dir = self.dir.parent().unwrap_or(&self.dir);
        agent_state_dir(agents_dir, &self.name)
    }

    /// agent.yaml 的 mtime;读不到(文件缺失等)返回 None。
    /// worker 拿它判断 aliases/messaging 是否需要热重载(改完即时生效,无需重启 daemon)。
    pub fn manifest_mtime(&self) -> Option<std::time::SystemTime> {
        std::fs::metadata(self.manifest_path())
            .and_then(|m| m.modified())
            .ok()
    }

    // 仅测试使用:生产路径未调用(2026-08-20 死代码盘点确认)。不进 release 二进制。
    #[cfg(test)]
    /// 重读 agent.yaml 的 `aliases:`(解析成功才返回 Some;读/解析失败如写到一半返回 None)。
    /// 不改 self —— Session 持有 `&Agent`,worker 把结果热替换进自己的可变副本(失败保留旧值)。
    pub(crate) fn read_aliases(&self) -> Option<HashMap<String, String>> {
        std::fs::read_to_string(self.manifest_path())
            .ok()
            .and_then(|s| serde_yaml::from_str::<Agent>(&s).ok())
            .map(|a| a.aliases)
    }

    /// 重读 agent.yaml 的 `messaging:` 覆盖(热重载;读/解析失败返回 None)。
    pub fn read_messaging_override(&self) -> Option<MessagingOverride> {
        std::fs::read_to_string(self.manifest_path())
            .ok()
            .and_then(|s| serde_yaml::from_str::<Agent>(&s).ok())
            .and_then(|a| a.messaging)
    }

    /// 整份重读 agent.yaml(热重载;读/解析失败返回 None,调用方保留旧值)。
    /// 与逐字段的 `read_aliases`/`read_messaging_override` 不同:这里返回完整 `Agent`,
    /// 用于刷新 `model`/`working_dir`/`operators` 等只在启动时加载的字段(飞书面板改了配置后下次冷启生效)。
    /// `dir` 与 `app_owner` 是 `#[serde(skip)]`,不从 yaml 读,故沿用当前这份(`app_owner` 共享同一个槽)。
    pub fn reread(&self) -> Option<Agent> {
        std::fs::read_to_string(self.manifest_path())
            .ok()
            .and_then(|s| serde_yaml::from_str::<Agent>(&s).ok())
            .map(|mut a| {
                a.dir = self.dir.clone();
                a.global_config_path = self.global_config_path.clone();
                a.app_owner = self.app_owner.clone();
                a
            })
    }

    /// 当前生效的操作名单。每次判权现读全局配置，撤权不依赖 daemon 的配置快照。
    /// 读不出或格式错误时返回错误，调用方必须拒绝操作。
    pub fn effective_operators(&self) -> Result<Vec<String>, String> {
        let Some(feishu) = &self.feishu else {
            return Ok(Vec::new());
        };
        if let Some(list) = &feishu.operators {
            return Ok(list.clone());
        }
        let path = self.global_config_path.clone().or_else(|| {
            self.dir
                .parent()?
                .parent()
                .map(|root| root.join("config.yaml"))
        });
        let Some(path) = path else {
            return Ok(Vec::new());
        };
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(summon_i18n::format!("r1c19c8ad71")),
        };
        let cfg: crate::config::Config =
            serde_yaml::from_str(&text).map_err(|e| summon_i18n::format!("r3d8ab008f2"))?;
        Ok(cfg.feishu.operators)
    }

    /// 名单命中:open_id 或 union_id 任一非空且在 `list` 内即算命中。open_id 按 app 隔离、
    /// union_id 同开发者下跨 app 稳定,二者前缀(`ou_`/`on_`)天然不冲突,故同一名单里可混存——
    /// 新装用户写稳定的 union_id、老配置里的 open_id 继续生效。
    fn id_in_list(list: &[String], open_id: &str, union_id: &str) -> bool {
        list.iter().any(|u| {
            (!open_id.is_empty() && u == open_id) || (!union_id.is_empty() && u == union_id)
        })
    }

    /// 白名单判定:allowed_users 为空 = 全放行;否则 open_id 或 union_id 任一命中即放行。
    pub fn allows(&self, open_id: &str, union_id: &str) -> bool {
        match &self.feishu {
            Some(f) if !f.allowed_users.is_empty() => {
                Self::id_in_list(&f.allowed_users, open_id, union_id)
            }
            _ => true,
        }
    }

    pub fn replies_without_mention(&self, chat_id: &str) -> bool {
        !chat_id.is_empty()
            && self
                .feishu
                .as_ref()
                .is_some_and(|feishu| feishu.chat_reply_without_mention.get(chat_id) == Some(&true))
    }

    /// 破坏性操作(经卡片按钮 kill/清理会话)的授权判定 —— 与 `allows`(谁能说话)**解耦**:
    /// 优先看专门的 `operators` 名单;没配 operators 就回退到 `allowed_users`(向后兼容)。
    /// 选定名单非空 → operator 的 open_id 或 union_id 任一在内即放行,应用所有者不另行放行;
    /// 两者皆空(allow-all)→ 私聊放行(裸会话即 owner,单人便利),群里 fail-closed(守"只有授权用户
    /// 才中断"),只有效果落在本群的动作另行放行已查到的飞书应用所有者。
    /// `scope` 为 [`OperateScope::Chat`] 时,该 chat 本群名单里的人另行放行。
    pub fn can_operate(
        &self,
        open_id: &str,
        union_id: &str,
        is_group: bool,
        scope: OperateScope<'_>,
    ) -> bool {
        if self.effective_operators().is_err() {
            return false;
        }
        let in_chat = match scope {
            OperateScope::Chat(chat_id) => {
                self.feishu
                    .as_ref()
                    .and_then(|f| f.chat_operators.get(chat_id))
                    .is_some_and(|list| Self::id_in_list(list, open_id, union_id))
                    // 名单皆空时私聊人人可操作、群里人人可对话,本群操作只多放行应用所有者,
                    // 不扩大授权面。跨 chat 的动作(Bridge、跨 agent 停会话)此时私聊也拒,不放。
                    || (is_group
                        && self.member_lists_empty()
                        && self.is_app_owner(open_id, union_id))
            }
            OperateScope::Agent => false,
        };
        in_chat || self.can_operate_agent_wide(open_id, union_id, is_group)
    }

    /// 有飞书配置,且 `operators` 与 `allowed_users` 都为空。
    fn member_lists_empty(&self) -> bool {
        self.feishu.as_ref().is_some_and(|f| {
            self.effective_operators().is_ok_and(|list| list.is_empty())
                && f.allowed_users.is_empty()
        })
    }

    /// [`Self::can_operate`] 拒绝时的原因;放行返回 None。是否放行只由 `can_operate` 判定,
    /// 这里只在它拒绝后按名单状态归类,不会放行任何人。
    pub fn operate_denial(
        &self,
        open_id: &str,
        union_id: &str,
        is_group: bool,
        scope: OperateScope<'_>,
    ) -> Option<OperateDenial> {
        if self.can_operate(open_id, union_id, is_group, scope) {
            return None;
        }
        let configured = |list: fn(&Feishu) -> &Vec<String>| {
            self.feishu.as_ref().is_some_and(|f| !list(f).is_empty())
        };
        Some(
            if self
                .effective_operators()
                .map_or(true, |list| !list.is_empty())
            {
                OperateDenial::NotListed(OperateList::Operators)
            } else if configured(|f| &f.allowed_users) {
                OperateDenial::NotListed(OperateList::AllowedUsers)
            } else {
                OperateDenial::NoListConfigured
            },
        )
    }

    fn can_operate_agent_wide(&self, open_id: &str, union_id: &str, is_group: bool) -> bool {
        match &self.feishu {
            Some(f) => {
                let Ok(operators) = self.effective_operators() else {
                    return false;
                };
                let list = if !operators.is_empty() {
                    &operators
                } else {
                    &f.allowed_users
                };
                if list.is_empty() {
                    !is_group
                } else {
                    Self::id_in_list(list, open_id, union_id)
                }
            }
            None => !is_group,
        }
    }

    /// 发起人是否已查到的飞书应用所有者:任一非空 id 相等即算,空串不算命中。
    pub fn is_app_owner(&self, open_id: &str, union_id: &str) -> bool {
        match self.app_owner.get() {
            AppOwnerState::Known(Some(owner)) => {
                (!open_id.is_empty() && owner.open_id == open_id)
                    || (!union_id.is_empty() && owner.union_id == union_id)
            }
            _ => false,
        }
    }

    /// agent.yaml 顶层 `model`(去空白后非空才返回)。启动命令按 driver 决定是否拼成 `--model <name>`。
    pub fn model(&self) -> Option<&str> {
        self.model
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
    }

    /// `runtime.rpc_model`(去空白后非空才返回)。见其字段注释:与顶层 `model` 分属两个
    /// 不通用的命名空间,**不互相回退** —— 回退就等于把 CLI 名喂给 `session/set_model`,
    /// 换来一个 -32602 和一次「配了但没生效」。
    pub fn rpc_model(&self) -> Option<&str> {
        self.runtime
            .as_ref()
            .and_then(|r| r.rpc_model.as_deref())
            .map(str::trim)
            .filter(|s| !s.is_empty())
    }

    /// agent.yaml 顶层 `effort`(去空白后非空才返回)。逻辑 reasoning 档位,只服务 claude/codex
    /// (cursor 的档位在 bind 时已烤进 model 名)。镜像 `model()` 的 trim+空→None 语义。
    pub fn effort(&self) -> Option<&str> {
        self.effort
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
    }

    /// agent.yaml 顶层 `reasoning_summary`(去空白后非空才返回)。仅 codex 用;空→None=回退用户
    /// config / 兜底,决策见 `driver::reasoning_summary_decision`。镜像 `effort()` 的语义。
    pub fn reasoning_summary(&self) -> Option<&str> {
        self.reasoning_summary
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
    }

    /// 这是管家身份吗。**判 role 而不是判「有没有配 cli」** —— 后者是实现细节:
    /// steward 的 agent.yaml 里可能因为复用 onboard 而留着 cli 字段,靠它猜会猜错;
    /// 而 role 是 owner 显式声明的意图。
    pub fn is_steward(&self) -> bool {
        matches!(self.role, AgentRole::Steward)
    }

    /// 启动 cli 种类(默认 claude)。
    pub fn cli_kind(&self) -> &str {
        self.runtime
            .as_ref()
            .and_then(|r| r.cli.as_deref())
            .unwrap_or("claude")
    }

    /// 实际启动二进制:toml cli_cmd 覆盖 > 按 kind 默认。
    pub fn cli_cmd(&self) -> String {
        if let Some(c) = self.runtime.as_ref().and_then(|r| r.cli_cmd.as_deref()) {
            return c.to_string();
        }
        crate::tools::Tool::for_cli_kind(self.cli_kind())
            .map(crate::tools::resolve)
            .unwrap_or_else(|| {
                std::path::PathBuf::from(CliDriver::from_kind(self.cli_kind()).default_cmd())
            })
            .to_string_lossy()
            .into_owned()
    }

    /// 支持自动抓取/daemon 回复的交互式 CLI。
    pub fn captures(&self) -> bool {
        CliDriver::from_kind(self.cli_kind()).captures()
    }

    /// 是否对该 agent 启用 session resume:agent.yaml `[runtime].resume` 覆盖 > 全局默认。
    /// 仍需底层 CLI 本身支持 resume(Other 永不支持,由调用方按 driver 再判)。
    pub fn resume_enabled(&self, global_default: bool) -> bool {
        self.runtime
            .as_ref()
            .and_then(|r| r.resume)
            .unwrap_or(global_default)
    }

    /// 本 agent 生效的 codex 权限策略:内置默认 → 全局 `codex:` → 本 agent `runtime.codex_*`。
    ///
    /// 一并返回被拒的非法值,调用方负责写日志 —— 静默降级会让「我明明配了」查不下去。
    pub fn codex_policy(
        &self,
        global: Option<&crate::codex_policy::CodexPolicyOverride>,
    ) -> (
        crate::codex_policy::CodexPolicy,
        Vec<crate::codex_policy::PolicyFallback>,
    ) {
        let runtime = self.runtime.as_ref();
        crate::codex_policy::resolve(
            global,
            runtime.and_then(|r| r.codex_sandbox.as_deref()),
            runtime.and_then(|r| r.codex_approval_policy.as_deref()),
        )
    }

    /// bootstrap seed 行:`runtime.seed` 覆盖 > 按 kind 默认。`None` = 本次冷启不发种子
    /// (claude 的默认,见 `CliDriver::default_seed`)。显式配的空串也当作不发。
    pub fn seed(&self) -> Option<String> {
        if let Some(s) = self.runtime.as_ref().and_then(|r| r.seed.as_deref()) {
            return (!s.trim().is_empty()).then(|| s.to_string());
        }
        // per-CLI 默认种子收口 CliDriver(A3),不在 registry 散落裸字符串 match。
        crate::driver::CliDriver::from_kind(self.cli_kind())
            .default_seed(&self.name, &self.dir.display().to_string())
    }

    // 仅测试使用:生产路径未调用(2026-08-20 死代码盘点确认)。不进 release 二进制。
    #[cfg(test)]
    /// 长期记忆文件路径。
    pub(crate) fn memory_file(&self) -> PathBuf {
        self.dir.join("memory/MEMORY.md")
    }
}

#[derive(Debug)]
pub struct Registry {
    pub agents: Vec<Agent>,
}

/// 单份 agent.yaml 的加载失败。strict / tolerant 两个加载器共用**同一次扫描**,再各自投影:
/// strict 取第一条转 `RegistryError`,tolerant 把全部转成人类可读 warning。
/// why 收成一份:此前两个加载器各写一遍扫描循环,已经跑偏过(strict 遇目录条目错直接 `?`,
/// tolerant 记 warning 继续),同一份盘上事实给出两种结论,排障时信号互相矛盾。
#[derive(Debug)]
enum ManifestFailure {
    /// `read_dir` 迭代本身失败(条目级 IO),拿不到路径。
    Entry(std::io::Error),
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    Parse {
        path: PathBuf,
        source: serde_yaml::Error,
    },
}

impl ManifestFailure {
    fn warning(&self) -> String {
        match self {
            Self::Entry(e) => summon_i18n::format!("raf7b0d9a38"),
            Self::Read { path, source } => summon_i18n::format!("r98cdcccede", path.display()),
            Self::Parse { path, source } => summon_i18n::format!("r51981a1b30", path.display()),
        }
    }

    fn into_error(self) -> RegistryError {
        match self {
            Self::Entry(e) => RegistryError::Io(e),
            Self::Read { source, .. } => RegistryError::Io(source),
            Self::Parse { path, source } => RegistryError::Parse { path, source },
        }
    }
}

impl Registry {
    pub fn with_global_config_path(mut self, path: &Path) -> Self {
        for agent in &mut self.agents {
            agent.global_config_path = Some(path.to_path_buf());
        }
        self
    }

    /// 扫一遍 agents 目录:解析成功的进 `Vec<Agent>`,失败的**整份隔离**进 `Vec<ManifestFailure>`。
    ///
    /// 坏清单绝不以任何形态进 `agents`(哪怕占位):`allows()` 在名单空时 `_ => true` 全放行、
    /// `cli_kind()` 缺省回落 claude —— 半解析的 agent 会被这些降级分支当成合法配置,
    /// 等于给坏文件发通行证。故 fail-closed:排除出路由表,只留诊断。
    fn scan(agents_dir: &Path) -> Result<(Vec<Agent>, Vec<ManifestFailure>), RegistryError> {
        if !agents_dir.is_dir() {
            return Err(RegistryError::NoDir(agents_dir.to_path_buf()));
        }
        let mut agents = Vec::new();
        let mut failures = Vec::new();
        for entry in std::fs::read_dir(agents_dir)? {
            let dir = match entry {
                Ok(entry) => entry.path(),
                Err(e) => {
                    failures.push(ManifestFailure::Entry(e));
                    continue;
                }
            };
            let manifest = dir.join("agent.yaml");
            if !manifest.is_file() {
                continue;
            }
            let source = match std::fs::read_to_string(&manifest) {
                Ok(source) => source,
                Err(source) => {
                    failures.push(ManifestFailure::Read {
                        path: manifest,
                        source,
                    });
                    continue;
                }
            };
            let mut agent: Agent = match serde_yaml::from_str(&source) {
                Ok(agent) => agent,
                Err(source) => {
                    failures.push(ManifestFailure::Parse {
                        path: manifest,
                        source,
                    });
                    continue;
                }
            };
            agent.dir = dir;
            agents.push(agent);
        }
        agents.sort_by(|a, b| a.name.cmp(&b.name));
        Ok((agents, failures))
    }

    /// fail-fast 加载:任何一份清单坏掉都整体报错。
    ///
    /// **生产路径已全部改走 `load_tolerant`**(CLI 入口 `load_reg`、daemon boot、alias 映射、
    /// 身份隔离键收集),这里只剩测试在用「坏清单必须指名道姓报错」这条诊断契约。
    ///
    /// 别把它接回生产:一份 agent.yaml 坏掉就让全体 agent 不可用,是过重的连坐。真需要
    /// 「registry 读得干净」当前提的判定(如 daemon 的 `feishu_storage_scope` 靠「恰好一个飞书
    /// agent」决定要不要导入旧的未分 scope 身份缓存),请用 `load_tolerant` 再显式要求
    /// `warnings.is_empty()` —— 那样才能把「目录读不到」和「某份清单坏了」分开处理,
    /// 而不是靠 `Err` 碰巧兜住。
    pub fn load(agents_dir: &Path) -> Result<Self, RegistryError> {
        let (agents, failures) = Self::scan(agents_dir)?;
        match failures.into_iter().next() {
            Some(failure) => Err(failure.into_error()),
            None => Ok(Registry { agents }),
        }
    }

    /// daemon 启动用的 fail-soft 加载：目录本身不可读仍报错；单个 agent.yaml 损坏只隔离该 agent，
    /// 其余 agent 继续可用，并把可操作诊断交给调用方统一记录。
    pub fn load_tolerant(agents_dir: &Path) -> Result<(Self, Vec<String>), RegistryError> {
        let (agents, failures) = Self::scan(agents_dir)?;
        let warnings = failures.iter().map(ManifestFailure::warning).collect();
        Ok((Registry { agents }, warnings))
    }

    pub fn get(&self, name: &str) -> Option<&Agent> {
        self.agents.iter().find(|a| a.name == name)
    }

    pub fn routable(&self) -> Vec<&Agent> {
        self.agents.iter().filter(|a| a.is_routable()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn global_operators_inherit_override_revoke_and_fail_closed() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("agents/example");
        std::fs::create_dir_all(&dir).unwrap();
        let mut agent: Agent =
            serde_yaml::from_str("name: example\nfeishu:\n  allowed_users: [on_talker]\n").unwrap();
        agent.dir = dir;
        let config = root.path().join("config.yaml");
        let can = |a: &Agent, id| a.can_operate("", id, true, OperateScope::Agent);
        assert!(can(&agent, "on_talker"));
        std::fs::write(&config, "feishu:\n  operators: [on_admin]\n").unwrap();
        assert!(can(&agent, "on_admin"));
        assert!(!can(&agent, "on_talker"));
        std::fs::write(&config, "feishu:\n  operators: [on_next]\n").unwrap();
        assert!(!can(&agent, "on_admin"));
        assert!(can(&agent, "on_next"));
        std::fs::write(&config, "feishu: [broken").unwrap();
        assert!(!can(&agent, "on_next"));
        assert!(!agent.can_operate("", "anyone", false, OperateScope::Chat("chat")));
        agent.feishu.as_mut().unwrap().operators = Some(vec!["on_local".into()]);
        assert!(can(&agent, "on_local"));
        agent.feishu.as_mut().unwrap().operators = Some(vec![]);
        assert!(can(&agent, "on_talker"));
    }

    #[test]
    fn collapse_tilde_folds_home_and_leaves_others_and_round_trips() {
        // 非 home 前缀原样(不误折)。
        assert_eq!(collapse_tilde("/etc/foo"), "/etc/foo");
        assert_eq!(
            collapse_tilde("/Users/someone-else/x"),
            "/Users/someone-else/x"
        );
        // round-trip:expand_tilde 展开后 collapse_tilde 折回 ~(home 存在时);home 缺失则两者皆 no-op。
        assert_eq!(collapse_tilde(&expand_tilde("~/projects")), "~/projects");
        // home 本身 → ~。
        if let Some(h) = dirs::home_dir() {
            assert_eq!(collapse_tilde(&h.to_string_lossy()), "~");
        }
    }

    fn fixtures() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/agents")
    }

    #[test]
    fn loads_starlight() {
        let reg = Registry::load(&fixtures()).unwrap();
        let a = reg.get("starlight").expect("starlight present");
        assert_eq!(a.display, "星光");
        assert_eq!(a.tmux_name(), "soul-starlight");
        assert!(a.is_routable());
        assert!(a.working_dir().unwrap().ends_with("Desktop/work/example"));
    }

    #[test]
    fn missing_dir_errors() {
        let err = Registry::load(Path::new("/no/such/souls")).unwrap_err();
        assert!(matches!(err, RegistryError::NoDir(_)));
    }

    /// 表驱动用的临时 agents 目录:`(目录名, agent.yaml 内容)`。
    fn temp_agents_dir(label: &str, entries: &[(&str, &str)]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "summon-registry-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        for (name, manifest) in entries {
            let agent_dir = dir.join(name);
            std::fs::create_dir_all(&agent_dir).unwrap();
            std::fs::write(agent_dir.join("agent.yaml"), manifest).unwrap();
        }
        dir
    }

    /// 好 / 坏 / 空 三种清单:坏与空都算加载失败,tolerant 只留好的,warning 逐个点名。
    fn mixed_manifests() -> [(&'static str, &'static str); 3] {
        [
            (
                "good",
                "name: good\nstatus: active\nfeishu:\n  app_id_env: GOOD_APP_ID\n",
            ),
            // 坏在 feishu 块:若被「半解析」放行,它看着就是个 active + 有 app_id_env 的可路由 agent。
            (
                "bad",
                "name: bad\nstatus: active\nfeishu:\n  app_id_env: [BAD_APP_ID\n",
            ),
            ("empty", ""),
        ]
    }

    #[test]
    fn tolerant_load_isolates_broken_and_empty_manifests() {
        let dir = temp_agents_dir("tolerant", &mixed_manifests());

        let (registry, warnings) = Registry::load_tolerant(&dir).unwrap();
        assert!(registry.get("good").is_some());
        assert_eq!(registry.agents.len(), 1);
        // 坏文件不许被静默吞掉:两份失败都得有可操作诊断(含具体路径)。
        assert_eq!(warnings.len(), 2);
        assert!(warnings.iter().any(|w| w.contains("bad/agent.yaml")));
        assert!(warnings.iter().any(|w| w.contains("empty/agent.yaml")));

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn strict_load_reports_the_broken_manifest_path() {
        let dir = temp_agents_dir("strict", &mixed_manifests());

        // strict:坏清单整体报错,且指名道姓是哪份文件(排障入口)。
        let err = Registry::load(&dir).unwrap_err();
        match err {
            RegistryError::Parse { ref path, .. } => {
                let path = path.display().to_string();
                assert!(path.ends_with("bad/agent.yaml") || path.ends_with("empty/agent.yaml"));
            }
            other => panic!("期望 Parse 错,实得 {other:?}"),
        }

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn broken_manifest_never_becomes_routable() {
        let dir = temp_agents_dir("fail-closed", &mixed_manifests());

        // 红线:解析失败的 agent 绝不能变成「合法但全放行」——它连 registry 都进不去,
        // 于是 allows()(名单空即全放行)/ cli_kind()(缺省 claude)这些降级分支根本够不着它。
        let (registry, _) = Registry::load_tolerant(&dir).unwrap();
        let routable: Vec<&str> = registry
            .routable()
            .iter()
            .map(|a| a.name.as_str())
            .collect();
        assert_eq!(routable, ["good"]);
        assert!(registry.get("bad").is_none());
        assert!(registry.get("empty").is_none());
        // strict 路径更不必说:整体报错,没有任何可路由集合产出。
        assert!(Registry::load(&dir).is_err());

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn broken_manifest_never_participates_in_authz() {
        // 把 tolerant 当默认加载器之后,唯一还能保证「坏配置不越权」的地方就是这条:
        // 坏清单根本不在 `agents` 里,于是任何遍历 registry 做鉴权的调用方都够不着它。
        // 这里把「鉴权面」逐个点名钉死,防止后来者往 scan 里加「半解析占位 Agent」的兜底。
        let dir = temp_agents_dir("authz", &mixed_manifests());
        let (registry, warnings) = Registry::load_tolerant(&dir).unwrap();
        assert!(!warnings.is_empty(), "坏清单必须留下可见诊断");

        // ① 名字查不到 —— 调用方拿到 None 只能 fail-closed,没有「默认 agent」可退。
        for name in ["bad", "empty"] {
            assert!(registry.get(name).is_none(), "{name} 不该进 registry");
        }
        // ② 遍历型鉴权(谁能说话 / 谁能做破坏性操作)只可能落在幸存的 good 上。
        // 若坏 agent 混进来,它名单空 → `allows` 的 `_ => true` 与 `can_operate` 的私聊分支
        // 会把它当成合法的 allow-all 配置,等于给坏文件发通行证。
        let talkers: Vec<&str> = registry
            .agents
            .iter()
            .filter(|a| a.allows("ou_anyone", "on_anyone"))
            .map(|a| a.name.as_str())
            .collect();
        assert_eq!(talkers, ["good"]);
        let operators: Vec<&str> = registry
            .agents
            .iter()
            .filter(|a| a.can_operate("ou_anyone", "on_anyone", false, OperateScope::Agent))
            .map(|a| a.name.as_str())
            .collect();
        assert_eq!(operators, ["good"]);
        // ③ 可路由集合同样只有 good;cli_kind 的 `unwrap_or("claude")` 缺省也只作用在 good 上。
        assert_eq!(
            registry
                .routable()
                .iter()
                .map(|a| a.name.as_str())
                .collect::<Vec<_>>(),
            ["good"]
        );
        assert!(registry.agents.iter().all(|a| a.name == "good"));

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn allowlist_gates_sender() {
        let reg = Registry::load(&fixtures()).unwrap();
        let a = reg.get("starlight").unwrap();
        assert!(a.allows("example-user", "")); // open_id 名单内
        assert!(!a.allows("someone_else", "")); // 名单外
        assert!(a.allows("other_open", "example-user")); // union_id 命中也放行
    }

    #[test]
    fn mentionless_configuration_is_explicit_and_agent_local() {
        let first: Agent = serde_yaml::from_str(
            "name: first\nfeishu:\n  chat_reply_without_mention:\n    oc_enabled: true\n    oc_disabled: false\n",
        ).unwrap();
        let second: Agent = serde_yaml::from_str("name: second\nfeishu: {}\n").unwrap();
        assert!(first.replies_without_mention("oc_enabled"));
        for chat in ["oc_disabled", "oc_enabled_suffix", "oc_missing", ""] {
            assert!(!first.replies_without_mention(chat));
        }
        assert!(!second.replies_without_mention("oc_enabled"));
        assert!(serde_yaml::from_str::<Agent>(
            "name: invalid\nfeishu:\n  chat_reply_without_mention: {oc_enabled: yes}\n"
        )
        .is_err());

        let root = tempfile::tempdir().unwrap();
        let mut agent = first;
        agent.dir = root.path().to_path_buf();
        std::fs::write(
            agent.manifest_path(),
            "name: first\nfeishu:\n  chat_reply_without_mention: {oc_enabled: false}\n",
        )
        .unwrap();
        assert!(!agent
            .reread()
            .unwrap()
            .replies_without_mention("oc_enabled"));
    }

    /// 缺省是 `agent`:**现有的 agent.yaml 一行都不用改**,加一个维度不该惊动既有身份。
    #[test]
    fn role缺省是普通agent() {
        let a: Agent = serde_yaml::from_str("name: x\n").expect("最小 yaml 应能加载");
        assert_eq!(a.role, AgentRole::Agent);
        assert!(!a.is_steward());
    }

    /// 显式 `role: steward` 才是管家。
    #[test]
    fn 显式声明才是管家() {
        let a: Agent = serde_yaml::from_str("name: x\nrole: steward\n").expect("应能加载");
        assert_eq!(a.role, AgentRole::Steward);
        assert!(a.is_steward());
    }

    /// **写错的 role 必须让加载失败,不能静默降级成普通 agent。**
    ///
    /// 这与 `capture` 存字符串的取舍相反,是刻意的:capture 写错只是退回默认通道,而 role
    /// 写错(想写 steward 拼成 stewart)会让一个本该只读的身份变成能起会话、能被 @ 干活的
    /// agent —— 那是权限方向的静默升级,宁可加载时就炸。
    #[test]
    fn 拼错的role不静默降级() {
        let err = serde_yaml::from_str::<Agent>("name: x\nrole: stewart\n");
        assert!(err.is_err(), "拼错的 role 必须报错,而不是回落成 agent");
    }

    #[test]
    fn can_operate_gates_destructive_actions() {
        // 显式白名单:名单内可操作(p2p 与群都行),名单外一律拒。
        let a: Agent = serde_yaml::from_str(
            "name: x\nfeishu:\n  app_id_env: A\n  allowed_users:\n    - ou_owner\n    - ou_mate\n",
        )
        .unwrap();
        assert!(a.can_operate("ou_owner", "", false, OperateScope::Agent));
        assert!(a.can_operate("ou_owner", "", true, OperateScope::Agent));
        assert!(a.can_operate("ou_mate", "", true, OperateScope::Agent));
        assert!(!a.can_operate("ou_stranger", "", true, OperateScope::Agent));
        assert!(!a.can_operate("ou_stranger", "", false, OperateScope::Agent));

        // 空白名单(allow-all):破坏性操作在群里 fail-closed,私聊放行(裸会话即 owner)。
        let open: Agent = serde_yaml::from_str("name: y\nfeishu:\n  app_id_env: A\n").unwrap();
        assert!(open.can_operate("anyone", "", false, OperateScope::Agent)); // 私聊放行
        assert!(!open.can_operate("anyone", "", true, OperateScope::Agent)); // 群里拒

        // 完全无 feishu 配置:等同 allow-all,同样群里 fail-closed。
        let none: Agent = serde_yaml::from_str("name: z").unwrap();
        assert!(none.can_operate("anyone", "", false, OperateScope::Agent));
        assert!(!none.can_operate("anyone", "", true, OperateScope::Agent));
    }

    #[test]
    fn app_owner_operates_in_groups_only_when_both_lists_are_empty() {
        let chat = OperateScope::Chat("oc_x");
        let owner = || {
            AppOwnerState::Known(Some(AppOwner {
                open_id: "ou_owner".into(),
                union_id: "on_owner".into(),
            }))
        };
        let open: Agent = serde_yaml::from_str("name: y\nfeishu:\n  app_id_env: A\n").unwrap();
        // 未查到所有者:群里 fail-closed。
        assert!(!open.can_operate("ou_owner", "on_owner", true, chat));
        open.app_owner.set(owner());
        assert!(open.can_operate("ou_owner", "", true, chat));
        assert!(open.can_operate("ou_other_app", "on_owner", true, chat));
        // 只放行本群范围:跨 chat 的动作在名单皆空时对谁都拒,所有者也不例外。
        assert!(!open.can_operate("ou_owner", "on_owner", true, OperateScope::Agent));
        assert!(!open.can_operate("ou_mate", "on_mate", true, chat));
        assert!(!open.can_operate("", "", true, chat), "空 id 不算命中");
        assert_eq!(
            open.operate_denial("ou_mate", "on_mate", true, chat),
            Some(OperateDenial::NoListConfigured)
        );
        assert!(
            open.can_operate("ou_mate", "on_mate", false, chat),
            "私聊不变"
        );

        // 所有者某条 id 为空时,空串不与发起人的空 id 相配。
        open.app_owner.set(AppOwnerState::Known(Some(AppOwner {
            open_id: "ou_owner".into(),
            union_id: String::new(),
        })));
        assert!(!open.can_operate("ou_other", "", true, chat));
        assert!(open.can_operate("ou_owner", "", true, chat));
        open.app_owner.set(AppOwnerState::Known(Some(AppOwner {
            open_id: String::new(),
            union_id: "on_owner".into(),
        })));
        assert!(!open.can_operate("", "on_other", true, chat));
        assert!(open.can_operate("", "on_owner", true, chat));

        // 所有者不是企业成员:没有可放行的人。
        open.app_owner.set(AppOwnerState::Known(None));
        assert!(!open.can_operate("ou_owner", "on_owner", true, chat));

        // 名单任一非空:所有者不获得隐含权限。
        for lists in ["  operators: [on_lead]\n", "  allowed_users: [on_mate]\n"] {
            let listed: Agent =
                serde_yaml::from_str(&format!("name: x\nfeishu:\n  app_id_env: A\n{lists}"))
                    .unwrap();
            listed.app_owner.set(owner());
            assert!(
                !listed.can_operate("ou_owner", "on_owner", true, chat),
                "{lists}"
            );
            assert!(
                !listed.can_operate("ou_owner", "on_owner", false, chat),
                "{lists}"
            );
        }

        // 没有 feishu 段就没有应用,群里照旧拒。
        let none: Agent = serde_yaml::from_str("name: z").unwrap();
        none.app_owner.set(owner());
        assert!(!none.can_operate("ou_owner", "on_owner", true, chat));
    }

    #[test]
    fn app_owner_slot_is_shared_by_clones_and_reread() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("agent.yaml"),
            "name: x\nfeishu:\n  app_id_env: A\n",
        )
        .unwrap();
        let mut agent: Agent = serde_yaml::from_str("name: x\nfeishu:\n  app_id_env: A\n").unwrap();
        agent.dir = dir.path().to_path_buf();
        let copy = agent.clone();
        let fresh = agent.reread().unwrap();
        agent.app_owner.set(AppOwnerState::Known(Some(AppOwner {
            open_id: "ou_owner".into(),
            union_id: String::new(),
        })));
        let chat = OperateScope::Chat("oc_x");
        assert!(copy.can_operate("ou_owner", "", true, chat));
        assert!(fresh.can_operate("ou_owner", "", true, chat));
        assert!(fresh
            .reread()
            .unwrap()
            .can_operate("ou_owner", "", true, chat));
        let parsed: Agent = serde_yaml::from_str("name: x\nfeishu:\n  app_id_env: A\n").unwrap();
        assert_eq!(
            parsed.app_owner.get(),
            AppOwnerState::Unknown,
            "新解析的副本各自独立"
        );
    }

    #[test]
    fn operate_denial_names_the_list_that_rejected() {
        let chat = OperateScope::Chat("oc_x");
        let open: Agent = serde_yaml::from_str("name: y\nfeishu:\n  app_id_env: A\n").unwrap();
        assert_eq!(
            open.operate_denial("ou_a", "on_a", true, chat),
            Some(OperateDenial::NoListConfigured)
        );
        assert_eq!(open.operate_denial("ou_a", "on_a", false, chat), None);

        let ops: Agent = serde_yaml::from_str(
            "name: x\nfeishu:\n  app_id_env: A\n  operators: [on_lead]\n  chat_operators: {oc_x: [on_mate]}\n",
        )
        .unwrap();
        assert_eq!(
            ops.operate_denial("ou_a", "on_a", true, chat),
            Some(OperateDenial::NotListed(OperateList::Operators))
        );
        assert_eq!(ops.operate_denial("ou_l", "on_lead", true, chat), None);
        assert_eq!(ops.operate_denial("ou_m", "on_mate", true, chat), None);
        assert_eq!(
            ops.operate_denial("ou_m", "on_mate", true, OperateScope::Agent),
            Some(OperateDenial::NotListed(OperateList::Operators))
        );

        let talk: Agent = serde_yaml::from_str(
            "name: x\nfeishu:\n  app_id_env: A\n  allowed_users: [on_owner]\n",
        )
        .unwrap();
        assert_eq!(
            talk.operate_denial("ou_a", "on_a", true, chat),
            Some(OperateDenial::NotListed(OperateList::AllowedUsers))
        );
        assert_eq!(talk.operate_denial("ou_o", "on_owner", true, chat), None);
    }

    #[test]
    fn gate_matches_union_id_across_apps() {
        // 名单里存稳定的 union_id(on_...):open_id 按 app 变、对不上也能凭 union_id 放行。
        // 这正是「同一人在不同 app 下 open_id 不同」导致点不动按钮的根因修复。
        let by_union: Agent = serde_yaml::from_str(
            "name: x\nfeishu:\n  app_id_env: A\n  operators:\n    - on_stable\n",
        )
        .unwrap();
        // 这个 app 下 open_id 是 ou_thisapp(与名单里的 on_stable 不同),但 union_id 命中 → 放行。
        assert!(by_union.can_operate("ou_thisapp", "on_stable", true, OperateScope::Agent));
        // union_id 也对不上 → 拒。
        assert!(!by_union.can_operate("ou_thisapp", "on_other", true, OperateScope::Agent));
        // 名单存 open_id(老配置):open_id 命中即可,union_id 空也行(向后兼容)。
        let by_open: Agent = serde_yaml::from_str(
            "name: y\nfeishu:\n  app_id_env: A\n  allowed_users:\n    - ou_legacy\n",
        )
        .unwrap();
        assert!(by_open.allows("ou_legacy", ""));
        assert!(!by_open.allows("ou_else", "on_other")); // 两种 id 都不在名单 → 拒
    }

    #[test]
    fn can_operate_prefers_operators_and_decouples_from_talk() {
        // operators 与 talk 名单(allowed_users)解耦:talk 全放行,但只有 operators 能破坏性操作。
        let split: Agent = serde_yaml::from_str(
            "name: x\nfeishu:\n  app_id_env: A\n  allowed_users: []\n  operators:\n    - ou_owner\n",
        )
        .unwrap();
        assert!(split.allows("anyone", "")); // talk:allowed_users 空 = 谁都能说话
        assert!(split.can_operate("ou_owner", "", true, OperateScope::Agent)); // operate:群里也只有 operators 放行
        assert!(!split.can_operate("stranger", "", true, OperateScope::Agent)); // 不在 operators → 拒
        assert!(!split.can_operate("stranger", "", false, OperateScope::Agent)); // operators 非空就只认它,私聊也拒

        // 没配 operators → 回退到 allowed_users(向后兼容旧行为)。
        let legacy: Agent = serde_yaml::from_str(
            "name: y\nfeishu:\n  app_id_env: A\n  allowed_users:\n    - ou_a\n",
        )
        .unwrap();
        assert!(legacy.can_operate("ou_a", "", true, OperateScope::Agent));
        assert!(!legacy.can_operate("ou_b", "", true, OperateScope::Agent));
    }

    /// 本群名单与 agent 级名单取并集,但只在那个 chat 里、且只对效果落在那个 chat 上的动作算数。
    #[test]
    fn chat_operators_extend_the_agent_list_only_inside_their_chat() {
        let agent: Agent = serde_yaml::from_str(
            "name: x\nfeishu:\n  app_id_env: A\n  operators: [ou_owner]\n  chat_operators:\n    oc_shared: [ou_colleague, on_colleague_union]\n",
        )
        .unwrap();
        let here = OperateScope::Chat("oc_shared");
        let elsewhere = OperateScope::Chat("oc_elsewhere");

        assert!(agent.can_operate("ou_colleague", "", true, here));
        assert!(
            agent.can_operate("ou_other_app", "on_colleague_union", true, here),
            "本群名单同样认 union_id"
        );
        assert!(
            !agent.can_operate("ou_colleague", "", true, elsewhere),
            "出了那个群不算数"
        );
        assert!(
            !agent.can_operate("ou_colleague", "", false, elsewhere),
            "私聊也不算数"
        );
        assert!(
            !agent.can_operate("ou_colleague", "", true, OperateScope::Agent),
            "效果跨 chat 的动作只认 agent 级名单"
        );
        // agent 级名单在任何范围下都照旧生效(并集,只加不减)。
        assert!(agent.can_operate("ou_owner", "", true, here));
        assert!(agent.can_operate("ou_owner", "", true, elsewhere));
        assert!(agent.can_operate("ou_owner", "", true, OperateScope::Agent));
        assert!(!agent.can_operate("ou_stranger", "", true, here));
    }

    /// agent 级名单皆空时的既有语义(群里 fail-closed、私聊放行)不因本群名单改变。
    #[test]
    fn chat_operators_leave_the_empty_list_fallback_unchanged() {
        let agent: Agent = serde_yaml::from_str(
            "name: x\nfeishu:\n  app_id_env: A\n  chat_operators:\n    oc_shared: [ou_colleague]\n",
        )
        .unwrap();
        let here = OperateScope::Chat("oc_shared");
        assert!(agent.can_operate("ou_colleague", "", true, here));
        assert!(
            !agent.can_operate("ou_stranger", "", true, here),
            "群里仍 fail-closed"
        );
        assert!(
            agent.can_operate("ou_stranger", "", false, here),
            "私聊仍放行"
        );
    }

    #[test]
    fn cli_defaults_and_overrides() {
        let reg = Registry::load(&fixtures()).unwrap();
        let sk = reg.get("starlight").unwrap();
        assert_eq!(sk.cli_kind(), "claude");
        assert_eq!(sk.cli_cmd(), "claude");
        assert!(sk.captures());
        // claude 不再发冷启 seed:身份已由 daemon 拼进 PRIMER,再派一轮模型去读是重复劳动
        // (实测 41s)。见 `CliDriver::default_seed`。
        assert_eq!(sk.seed(), None);
        assert!(sk.memory_file().ends_with("starlight/memory/MEMORY.md"));

        let co = reg.get("coder").unwrap();
        assert_eq!(co.cli_kind(), "codex");
        assert_eq!(co.cli_cmd(), "codex");
        assert!(!co.captures());
        assert!(co.seed().unwrap().contains("SOUL.md"));

        // cursor 的默认命令是 `cursor-agent` 而非泛化的 `agent`:官方安装器两个软链都建、指向
        // 同一个二进制,取更具名的那个避免和别的 `agent` 撞名。要改用别的路径走 `tools.cursor_agent`。
        let cursor: Agent =
            serde_yaml::from_str("name: blue-wing\nruntime:\n  cli: cursor\n").unwrap();
        assert_eq!(cursor.cli_kind(), "cursor");
        assert_eq!(cursor.cli_cmd(), "cursor-agent");
        assert!(cursor.captures());

        let agent_alias: Agent =
            serde_yaml::from_str("name: blue-wing\nruntime:\n  cli: agent\n").unwrap();
        assert_eq!(agent_alias.cli_cmd(), "cursor-agent");
        assert!(agent_alias.captures());
    }

    /// agent.yaml 里配的两个键要真的被读出来并参与分层 —— serde 字段名写错不会报错,
    /// 只会静默变成 None、然后落回全局/默认,配置看起来「没生效」。
    #[test]
    fn agent_yaml_codex_policy_keys_are_read_and_win_over_the_global() {
        let global = crate::codex_policy::CodexPolicyOverride {
            sandbox: Some("danger-full-access".into()),
            approval_policy: Some("never".into()),
        };
        let agent: Agent = serde_yaml::from_str(
            "name: x\nruntime:\n  codex_sandbox: workspace-write\n  codex_approval_policy: untrusted\n",
        )
        .unwrap();
        let (policy, fallbacks) = agent.codex_policy(Some(&global));
        assert_eq!(policy.sandbox, "workspace-write");
        assert_eq!(policy.approval_policy, "untrusted");
        assert!(fallbacks.is_empty());
    }

    #[test]
    fn an_agent_without_codex_keys_follows_the_global_config() {
        let global = crate::codex_policy::CodexPolicyOverride {
            sandbox: Some("read-only".into()),
            approval_policy: None,
        };
        let agent: Agent = serde_yaml::from_str("name: x").unwrap();
        let (policy, _) = agent.codex_policy(Some(&global));
        assert_eq!(policy.sandbox, "read-only");
        assert_eq!(
            policy.approval_policy,
            crate::codex_policy::DEFAULT_APPROVAL_POLICY,
            "全局没配这一项就该落到内置默认"
        );
    }

    #[test]
    fn resume_enabled_agent_overrides_global_default() {
        // 无 runtime / 无 resume → 跟随全局
        let bare: Agent = serde_yaml::from_str("name: x").unwrap();
        assert!(bare.resume_enabled(true));
        assert!(!bare.resume_enabled(false));
        // agent 显式开 → 即使全局关也开
        let on: Agent = serde_yaml::from_str("name: x\nruntime:\n  resume: true\n").unwrap();
        assert!(on.resume_enabled(false));
        // agent 显式关 → 即使全局开也关
        let off: Agent = serde_yaml::from_str("name: x\nruntime:\n  resume: false\n").unwrap();
        assert!(!off.resume_enabled(true));
    }

    #[test]
    fn model_reads_top_level_and_trims_empty() {
        let with: Agent = serde_yaml::from_str("name: x\nmodel: gpt-5.5-high\n").unwrap();
        assert_eq!(with.model(), Some("gpt-5.5-high"));
        // 缺省 / 空白 → None(用 CLI 默认模型)
        let bare: Agent = serde_yaml::from_str("name: x").unwrap();
        assert_eq!(bare.model(), None);
        let blank: Agent = serde_yaml::from_str("name: x\nmodel: \"  \"\n").unwrap();
        assert_eq!(blank.model(), None);
    }

    #[test]
    fn effort_reads_top_level_and_trims_empty() {
        let with: Agent = serde_yaml::from_str("name: x\neffort: high\n").unwrap();
        assert_eq!(with.effort(), Some("high"));
        // 缺省 / 空白 → None(用 CLI 默认)
        let bare: Agent = serde_yaml::from_str("name: x").unwrap();
        assert_eq!(bare.effort(), None);
        let blank: Agent = serde_yaml::from_str("name: x\neffort: \"  \"\n").unwrap();
        assert_eq!(blank.effort(), None);
    }

    #[test]
    fn reasoning_summary_reads_top_level_and_trims_empty() {
        let with: Agent = serde_yaml::from_str("name: x\nreasoning_summary: detailed\n").unwrap();
        assert_eq!(with.reasoning_summary(), Some("detailed"));
        // 缺省 / 空白 → None(回退用户 config / 兜底)
        let bare: Agent = serde_yaml::from_str("name: x").unwrap();
        assert_eq!(bare.reasoning_summary(), None);
        let blank: Agent = serde_yaml::from_str("name: x\nreasoning_summary: \"  \"\n").unwrap();
        assert_eq!(blank.reasoning_summary(), None);
    }

    #[test]
    fn model_only_file_parses_with_effort_none() {
        // 既有只含 model 的 agent.yaml 必须照旧解析,effort 走 serde(default) = None。
        let a: Agent = serde_yaml::from_str("name: x\nmodel: claude-opus-4-8\n").unwrap();
        assert_eq!(a.model(), Some("claude-opus-4-8"));
        assert_eq!(a.effort(), None);
    }

    #[test]
    fn alias_maps_open_id_to_nick() {
        let a: Agent = serde_yaml::from_str("name: x\naliases:\n  ou_abc: 老板").unwrap();
        assert_eq!(a.alias("ou_abc"), Some("老板"));
        assert_eq!(a.alias("ou_unknown"), None);
    }

    #[test]
    fn reload_aliases_picks_up_disk_edits() {
        // 临时 agent 目录,写初始 agent.yaml(无 aliases)
        let tmp = std::env::temp_dir().join(format!("summon-reload-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let manifest = tmp.join("agent.yaml");
        std::fs::write(&manifest, "name: x\n").unwrap();

        let s = std::fs::read_to_string(&manifest).unwrap();
        let mut a: Agent = serde_yaml::from_str(&s).unwrap();
        a.dir = tmp.clone();
        assert_eq!(a.alias("ou_abc"), None);
        assert!(a.manifest_mtime().is_some());

        // agent 自己改文件:新增昵称 → read_aliases 立即读到(无需重启)
        std::fs::write(&manifest, "name: x\naliases:\n  ou_abc: 老板\n").unwrap();
        let m = a.read_aliases().expect("parse ok");
        assert_eq!(m.get("ou_abc").map(String::as_str), Some("老板"));

        // 改名:再读覆盖旧值
        std::fs::write(&manifest, "name: x\naliases:\n  ou_abc: 船长\n").unwrap();
        let m = a.read_aliases().expect("parse ok");
        assert_eq!(m.get("ou_abc").map(String::as_str), Some("船长"));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn chat_tag_is_stable_safe_and_prefixed() {
        // 稳定:同 id 多次调用一致
        assert_eq!(chat_tag("p2p", "oc_abc"), chat_tag("p2p", "oc_abc"));
        // 群/私聊前缀不同
        assert!(chat_tag("group", "oc_abc").starts_with('g'));
        assert!(chat_tag("p2p", "oc_abc").starts_with('p'));
        assert!(chat_tag("topic_group", "oc_abc").starts_with('g'));
        // 不含 tmux 危险字符,长度有界(前缀1 + 16 hex)
        let t = chat_tag("p2p", "oc_xyz:.$ 怪");
        assert_eq!(t.len(), 17);
        assert!(!t.contains([':', '.', ' ']));
        // 不同 id → 不同 tag(极大概率)
        assert_ne!(chat_tag("p2p", "oc_a"), chat_tag("p2p", "oc_b"));
    }

    #[test]
    fn tmux_name_for_chat_owner_p2p_keeps_bare_name() {
        let a: Agent = serde_yaml::from_str("name: x").unwrap();
        // owner 私聊 → 裸名,复用现存会话
        assert_eq!(a.tmux_name_for_chat("p2p", "oc_abc", true), "soul-x");
        // 非 owner / 群 → 带后缀
        assert_eq!(
            a.tmux_name_for_chat("p2p", "oc_abc", false),
            format!("soul-x-{}", chat_tag("p2p", "oc_abc"))
        );
        assert_eq!(
            a.tmux_name_for_chat("group", "oc_g", false),
            format!("soul-x-{}", chat_tag("group", "oc_g"))
        );
    }

    #[test]
    fn headless_defaults_per_cli_and_explicit_override() {
        // claude(裸 agent 默认 cli=claude)→ 默认 tmux(false),因 claude -p 独立计费
        let claude: Agent = serde_yaml::from_str("name: x\n").unwrap();
        assert!(!claude.headless());
        // cursor/codex → 默认无头(true)
        let cursor: Agent = serde_yaml::from_str("name: x\nruntime:\n  cli: cursor\n").unwrap();
        assert!(cursor.headless());
        let codex: Agent = serde_yaml::from_str("name: x\nruntime:\n  cli: codex\n").unwrap();
        assert!(codex.headless());
        // traex(codex fork)→ 同样默认无头(true),走原生 app-server RPC
        let traex: Agent = serde_yaml::from_str("name: x\nruntime:\n  cli: traex\n").unwrap();
        assert!(traex.headless());
        // 显式 runtime.headless 覆盖默认(两个方向)
        let claude_on: Agent =
            serde_yaml::from_str("name: x\nruntime:\n  cli: claude\n  headless: true\n").unwrap();
        assert!(claude_on.headless());
        let cursor_off: Agent =
            serde_yaml::from_str("name: x\nruntime:\n  cli: cursor\n  headless: false\n").unwrap();
        assert!(!cursor_off.headless());
    }

    #[test]
    fn extra_env_parses_and_returns_vec() {
        // 配了 runtime.env → 返回键值对 vec
        let a: Agent = serde_yaml::from_str(
            "name: x\nruntime:\n  env:\n    ANTHROPIC_BASE_URL: https://proxy\n    HTTP_PROXY: http://gw:8080\n",
        )
        .unwrap();
        let env = a.extra_env();
        assert_eq!(env.len(), 2);
        assert!(env.contains(&(
            "ANTHROPIC_BASE_URL".to_string(),
            "https://proxy".to_string()
        )));
        assert!(env.contains(&("HTTP_PROXY".to_string(), "http://gw:8080".to_string())));
    }

    #[test]
    fn extra_env_empty_when_unconfigured() {
        // 不配 runtime.env → 返回空 vec
        let a: Agent = serde_yaml::from_str("name: x\n").unwrap();
        assert!(a.extra_env().is_empty());
    }

    #[test]
    fn chat_working_dir_looks_up_by_chat_id() {
        // 配了 chat_working_dirs → 按 chat_id 查
        let a: Agent = serde_yaml::from_str(
            "name: x\nruntime:\n  working_dir: /default\n  chat_working_dirs:\n    oc_a: /proj-a\n    oc_b: ~/proj-b\n",
        )
        .unwrap();
        assert_eq!(a.chat_working_dir("oc_a"), Some("/proj-a".to_string()));
        // ~/ 展开
        let b_dir = a.chat_working_dir("oc_b").unwrap();
        assert!(b_dir.ends_with("proj-b"));
        assert!(b_dir.starts_with('/')); // 已展开
                                         // 未命中 → None
        assert_eq!(a.chat_working_dir("oc_c"), None);
    }

    #[test]
    fn chat_working_dir_none_when_unconfigured() {
        let a: Agent = serde_yaml::from_str("name: x\n").unwrap();
        assert_eq!(a.chat_working_dir("oc_any"), None);
    }

    #[test]
    fn tmux_name_falls_back_to_soul_prefix() {
        let a: Agent = serde_yaml::from_str("name: x\nstatus: active").unwrap();
        assert_eq!(a.tmux_name(), "soul-x");
        assert!(!a.is_routable()); // 无 feishu 绑定
    }

    /// 捕获通道:缺省仍由 cli 决定;Cursor 可用 `runtime.capture: cli` 切到 `-p`。
    #[test]
    fn capture_mode_defaults_by_cli_and_cursor_can_opt_into_clisession() {
        for cli in ["claude", ""] {
            let a: Agent =
                serde_yaml::from_str(&format!("name: x\nruntime:\n  cli: {cli}\n")).unwrap();
            assert_eq!(a.capture_mode(), CaptureMode::Transcript, "cli={cli}");
        }
        for cli in ["cursor", "codex", "traex"] {
            let a: Agent =
                serde_yaml::from_str(&format!("name: x\nruntime:\n  cli: {cli}\n")).unwrap();
            assert_eq!(a.capture_mode(), CaptureMode::Rpc, "cli={cli}");
        }
        // claude + 显式 headless:true **没有独立落点** —— 没有恢复 Subprocess,掉回 Transcript。
        let claude_headless: Agent =
            serde_yaml::from_str("name: x\nruntime:\n  cli: claude\n  headless: true\n").unwrap();
        assert_eq!(claude_headless.capture_mode(), CaptureMode::Transcript);
        let cursor_rpc: Agent =
            serde_yaml::from_str("name: x\nruntime:\n  cli: cursor\n  capture: rpc\n").unwrap();
        assert_eq!(cursor_rpc.capture_mode(), CaptureMode::Rpc);
        let cursor_cli: Agent =
            serde_yaml::from_str("name: x\nruntime:\n  cli: cursor\n  capture: cli\n").unwrap();
        assert_eq!(cursor_cli.capture_mode(), CaptureMode::CliSession);
        let cursor_exec: Agent =
            serde_yaml::from_str("name: x\nruntime:\n  cli: cursor\n  capture: exec\n").unwrap();
        assert_eq!(cursor_exec.capture_mode(), CaptureMode::CliSession);
        assert_eq!(cursor_cli.resume_cli_kind(), "cursor");
        assert_eq!(cursor_rpc.resume_cli_kind(), "cursor-acp");
        // codex `capture: exec` 不许另开通道,仍 Rpc。
        let codex_exec: Agent =
            serde_yaml::from_str("name: x\nruntime:\n  cli: codex\n  capture: exec\n").unwrap();
        assert_eq!(codex_exec.capture_mode(), CaptureMode::Rpc);
        // claude 的 capture 键改不了 tmux 路径。
        let claude_cli: Agent =
            serde_yaml::from_str("name: x\nruntime:\n  cli: claude\n  capture: cli\n").unwrap();
        assert_eq!(claude_cli.capture_mode(), CaptureMode::Transcript);
    }

    /// 反过来:codex **不许**分通道。`app-server` 的 thread id 与 `codex exec` 的 rollout 是
    /// 同一套 id,分开只会让切逃生门白丢一次上下文。claude 同理(它压根没有第二条通道)。
    #[test]
    fn codex_and_claude_keep_one_resume_namespace_across_channels() {
        for capture in ["", "  capture: rpc\n", "  capture: exec\n"] {
            let agent: Agent =
                serde_yaml::from_str(&format!("name: x\nruntime:\n  cli: codex\n{capture}"))
                    .unwrap();
            assert_eq!(agent.resume_cli_kind(), "codex", "capture={capture:?}");
        }
        let claude: Agent = serde_yaml::from_str("name: x\nruntime:\n  cli: claude\n").unwrap();
        assert_eq!(claude.resume_cli_kind(), "claude");
    }

    #[test]
    fn clisession_and_rpc_are_headless() {
        assert!(CaptureMode::Rpc.is_headless());
        assert!(CaptureMode::CliSession.is_headless());
        assert!(!CaptureMode::Transcript.is_headless());
    }

    // ---- state 目录拆分(agent_state_dir / migrate_agent_state)----

    fn make_agent_dir(root: &Path, alias: &str) -> PathBuf {
        let dir = root.join(alias);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("agent.yaml"), format!("name: {alias}\n")).unwrap();
        std::fs::write(dir.join("SOUL.md"), "soul").unwrap();
        std::fs::write(dir.join(".env"), "FEISHU_APP_ID=x\n").unwrap();
        std::fs::create_dir_all(dir.join("memory")).unwrap();
        dir
    }

    #[test]
    fn state_dir_returns_new_when_state_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let agents_dir = tmp.path().join("agents");
        std::fs::create_dir_all(&agents_dir).unwrap();
        make_agent_dir(&agents_dir, "a1");
        // 新位置已存在(已迁移)
        std::fs::create_dir_all(state_root(&agents_dir).join("a1").join("runtime")).unwrap();
        let got = agent_state_dir(&agents_dir, "a1");
        assert_eq!(got, state_root(&agents_dir).join("a1"));
    }

    #[test]
    fn state_dir_falls_back_to_old_when_state_subdirs_exist() {
        let tmp = tempfile::tempdir().unwrap();
        let agents_dir = tmp.path().join("agents");
        std::fs::create_dir_all(&agents_dir).unwrap();
        let dir = make_agent_dir(&agents_dir, "a1");
        // 旧位置有状态子目录,新位置不存在 → 回退旧位置
        std::fs::create_dir_all(dir.join("runtime")).unwrap();
        let got = agent_state_dir(&agents_dir, "a1");
        assert_eq!(got, dir);
    }

    #[test]
    fn state_dir_returns_new_for_fresh_agent() {
        let tmp = tempfile::tempdir().unwrap();
        let agents_dir = tmp.path().join("agents");
        std::fs::create_dir_all(&agents_dir).unwrap();
        make_agent_dir(&agents_dir, "a1");
        // 全新 agent:新旧都没有状态子目录 → 直接用新位置
        let got = agent_state_dir(&agents_dir, "a1");
        assert_eq!(got, state_root(&agents_dir).join("a1"));
    }

    #[test]
    fn migrate_moves_state_subdirs_and_keeps_authored() {
        let tmp = tempfile::tempdir().unwrap();
        let agents_dir = tmp.path().join("agents");
        std::fs::create_dir_all(&agents_dir).unwrap();
        let dir = make_agent_dir(&agents_dir, "a1");
        // 造机器状态
        std::fs::create_dir_all(dir.join("runtime").join("chats")).unwrap();
        std::fs::create_dir_all(dir.join("threads").join("g123")).unwrap();
        std::fs::write(dir.join("runtime").join("chats").join("s.session"), "data").unwrap();
        // 造 authored(不该被搬)
        std::fs::write(dir.join("CLAUDE.md"), "instructions").unwrap();

        migrate_agent_state(&agents_dir).unwrap();

        let new_dir = state_root(&agents_dir).join("a1");
        // 状态搬到了新位置
        assert!(new_dir.join("runtime/chats/s.session").is_file());
        assert!(new_dir.join("threads/g123").is_dir());
        // 旧位置不再有状态子目录
        assert!(!dir.join("runtime").exists());
        assert!(!dir.join("threads").exists());
        // authored 留在旧位置
        assert!(dir.join("agent.yaml").is_file());
        assert!(dir.join("SOUL.md").is_file());
        assert!(dir.join(".env").is_file());
        assert!(dir.join("memory").is_dir());
        assert!(dir.join("CLAUDE.md").is_file());
        // 迁移后 state_dir() 应返回新位置
        assert_eq!(agent_state_dir(&agents_dir, "a1"), new_dir);
    }

    #[test]
    fn migrate_moves_state_files() {
        let tmp = tempfile::tempdir().unwrap();
        let agents_dir = tmp.path().join("agents");
        std::fs::create_dir_all(&agents_dir).unwrap();
        let dir = make_agent_dir(&agents_dir, "a1");
        std::fs::write(dir.join("cursor-parameters.json"), r#"{"model":"x"}"#).unwrap();

        migrate_agent_state(&agents_dir).unwrap();

        let new_dir = state_root(&agents_dir).join("a1");
        assert!(new_dir.join("cursor-parameters.json").is_file());
        assert!(!dir.join("cursor-parameters.json").exists());
    }

    #[test]
    fn migrate_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let agents_dir = tmp.path().join("agents");
        std::fs::create_dir_all(&agents_dir).unwrap();
        let dir = make_agent_dir(&agents_dir, "a1");
        std::fs::create_dir_all(dir.join("runtime")).unwrap();

        migrate_agent_state(&agents_dir).unwrap();
        // 第二次跑:旧位置已无状态,新位置已存在 → 不应报错,不应重复搬
        migrate_agent_state(&agents_dir).unwrap();

        let new_dir = state_root(&agents_dir).join("a1");
        assert!(new_dir.join("runtime").is_dir());
        assert!(!dir.join("runtime").exists());
    }

    #[test]
    fn migrate_skips_agents_without_state() {
        let tmp = tempfile::tempdir().unwrap();
        let agents_dir = tmp.path().join("agents");
        std::fs::create_dir_all(&agents_dir).unwrap();
        make_agent_dir(&agents_dir, "draft-only");
        // 没有状态子目录/文件 → 跳过,不建 state 目录
        migrate_agent_state(&agents_dir).unwrap();
        assert!(!state_root(&agents_dir).join("draft-only").exists());
    }

    #[test]
    fn migrate_skips_existing_target_does_not_overwrite() {
        let tmp = tempfile::tempdir().unwrap();
        let agents_dir = tmp.path().join("agents");
        std::fs::create_dir_all(&agents_dir).unwrap();
        let dir = make_agent_dir(&agents_dir, "a1");
        std::fs::create_dir_all(dir.join("runtime")).unwrap();
        std::fs::create_dir_all(dir.join("threads")).unwrap();
        // 预置新位置的 runtime(上次迁移残留)→ 跳过,不覆盖
        let new_dir = state_root(&agents_dir).join("a1");
        std::fs::create_dir_all(new_dir.join("runtime")).unwrap();
        std::fs::write(new_dir.join("runtime").join("old.session"), "old").unwrap();

        migrate_agent_state(&agents_dir).unwrap();

        // 旧 runtime 留在原地(目标已存在,跳过)
        assert!(dir.join("runtime").exists());
        // 新 runtime 的内容没被覆盖
        assert!(new_dir.join("runtime/old.session").is_file());
        // threads 正常搬过去了
        assert!(new_dir.join("threads").exists());
        assert!(!dir.join("threads").exists());
    }

    #[test]
    fn migrate_handles_multiple_agents_independently() {
        let tmp = tempfile::tempdir().unwrap();
        let agents_dir = tmp.path().join("agents");
        std::fs::create_dir_all(&agents_dir).unwrap();
        let dir_a = make_agent_dir(&agents_dir, "alpha");
        let dir_b = make_agent_dir(&agents_dir, "beta");
        std::fs::create_dir_all(dir_a.join("runtime")).unwrap();
        std::fs::create_dir_all(dir_b.join("threads")).unwrap();

        migrate_agent_state(&agents_dir).unwrap();

        let state = state_root(&agents_dir);
        assert!(state.join("alpha/runtime").is_dir());
        assert!(state.join("beta/threads").is_dir());
        assert!(!dir_a.join("runtime").exists());
        assert!(!dir_b.join("threads").exists());
        // authored 都在
        assert!(dir_a.join("agent.yaml").is_file());
        assert!(dir_b.join("agent.yaml").is_file());
    }
}
