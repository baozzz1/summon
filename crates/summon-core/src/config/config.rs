use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::num::NonZeroU64;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub language: crate::i18n::LanguagePreference,
    pub agents_dir: PathBuf,
    pub poll_ms: u64,
    pub debounce_ms: u64,
    pub default_domain: String,
    /// Go sidecar二进制路径;None = 不起sidecar(仅 inject 模式)
    pub sidecar_bin: Option<PathBuf>,
    /// summon 运行时依赖的外部可执行文件。每项留空时按稳定裸名走 PATH；daemon 会先清掉
    /// summon 自己注入的 lark-cli wrapper 目录，再允许这种回退。
    pub tools: Tools,
    /// 全局操作名单，未在 agent 层覆盖时继承。
    pub feishu: GlobalFeishu,
    /// 兼容别名；新配置写 `tools.tmux`。两者同时存在时新字段优先，doctor 会提示清理旧字段。
    pub tmux_bin: Option<PathBuf>,
    /// 全局是否给会话记录原生 session id 并在 tmux 会话死后 resume 续接(默认 true)。
    /// 可被某 agent.yaml `[runtime].resume` 覆盖(见 `Agent::resume_enabled`)。
    pub resume_sessions: bool,
    /// 对内置消息层默认的**全局覆盖**(config.yaml 的 `messaging:` 段,全字段可选),热重载。
    pub messaging: MessagingOverride,
    /// 原生 macOS Menu Bar。
    pub menubar: MenuBar,
    /// owner sentinel(任意会话召唤 agent)。默认关，必须由 owner 显式启用。
    pub sentinel: Sentinel,
    /// 远端 summon 机器(跨机 fleet 聚合)。空 = 只看本机,与改动前逐字节等同。
    pub remotes: Vec<Remote>,
    /// 允许绑定的远程工具执行目标；在线连接由执行机器通过出站 SSH 注册。
    pub execution_targets: HashMap<String, crate::remote_tools::ExecutionTarget>,
    /// claude 托管会话的行为开关。
    pub claude: Claude,
    /// codex 托管会话的权限策略(sandbox + approval)。两个键都可选,不写 = 内置默认。
    /// 可被 agent.yaml 的 `runtime.codex_sandbox` / `runtime.codex_approval_policy` 再覆盖。
    /// 判据与降级规则见 [`crate::codex_policy`]。
    pub codex: crate::codex_policy::CodexPolicyOverride,
    /// cursor 托管会话的行为开关。
    pub cursor: Cursor,
    /// 文件预览服务器(把本机文件经 HTTP 渲成浏览器页,回复里提到的本地文件出成卡片按钮)。
    /// 默认开且只听 127.0.0.1;要让别的机器读到,是机器所有者的显式决定。
    /// 设计见 `docs/spec/2026-08-25-file-preview-server-design.md`。
    pub preview_server: PreviewServer,
    /// 浏览器内终端。见 [`WebTerminal`]。
    pub web_terminal: WebTerminal,
    /// 配置控制台。见 [`Dashboard`]。
    pub dashboard: Dashboard,
    /// 自动升级。见 [`Upgrade`]。
    pub upgrade: Upgrade,
    /// 主 agent:控制台洞察页的分析由它的 CLI、模型与账号来跑。不写 = 自动(近 30 天 token 用量
    /// 最多的 agent,还没有用量时取最早配置的),见 `summon_core::insights_analysis::resolve_primary`;
    /// 写了但这个 agent 不在或不可用时同样回落到自动。
    pub primary_agent: Option<String>,
}

/// 一台跑着 summon 的远端机器。
///
/// **为什么只有 ssh 一条路**:daemon 的控制面是 unix socket(`control.sock`),跨机不开 HTTP
/// 控制面(控制台只管本机的 `~/.summon`,不是跨机接口);而 ssh 是这台机器上**已经存在**的、
/// 已鉴权的通道 —— 用它就不必为跨机再造一套认证和监听端口。代价是每次拉取要 fork 一个 ssh 进程,所以拉取只在后台线程做、
/// 结果进缓存(见 `observe::remote_fleet`)。
///
/// ```yaml
/// remotes:
///   - name: devbox          # 卡片上显示的名字
///     ssh_host: devbox      # ssh 配置里的 Host 别名
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Remote {
    /// 展示名(也是卡片 value 里的机器键)。
    pub name: String,
    /// `ssh` 的目标(`~/.ssh/config` 里的 Host 别名,或 `user@host`)。
    pub ssh_host: String,
    /// 远端 summon 可执行文件路径。留空 = 按 `summon` 走远端 PATH。
    ///
    /// **为什么需要这个出口**:ssh 非交互登录拿到的 PATH 常常不含 `~/.local/bin`
    /// (devbox 的 deploy 脚本每次都在提醒这件事)。写死路径比赌 PATH 可靠。
    pub summon_bin: String,
}

impl Remote {
    /// 远端要执行的 summon 可执行文件(未配则裸名走 PATH)。
    pub fn summon_command(&self) -> &str {
        let bin = self.summon_bin.trim();
        if bin.is_empty() {
            "summon"
        } else {
            bin
        }
    }

    /// 配置是否可用(名字与 ssh 目标都得有)。空配置直接跳过而不是拿去 ssh 一个空串。
    pub fn is_usable(&self) -> bool {
        !self.name.trim().is_empty() && !self.ssh_host.trim().is_empty()
    }
}

impl Default for Config {
    fn default() -> Self {
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
        Config {
            language: crate::i18n::LanguagePreference::default(),
            agents_dir: home.join(".summon/agents"),
            poll_ms: 400,
            debounce_ms: 1200,
            default_domain: "feishu.cn".to_string(),
            sidecar_bin: None,
            tools: Tools::default(),
            feishu: GlobalFeishu::default(),
            tmux_bin: None,
            resume_sessions: true,
            messaging: MessagingOverride::default(),
            menubar: MenuBar::default(),
            sentinel: Sentinel::default(),
            remotes: Vec::new(),
            execution_targets: HashMap::new(),
            claude: Claude::default(),
            codex: Default::default(),
            cursor: Cursor::default(),
            preview_server: PreviewServer::default(),
            web_terminal: WebTerminal::default(),
            dashboard: Dashboard::default(),
            upgrade: Upgrade::default(),
            primary_agent: None,
        }
    }
}

/// 供各 agent 继承的飞书权限默认值。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct GlobalFeishu {
    pub operators: Vec<String>,
}

/// 外部 CLI 的统一路径表。显式值应为绝对路径（`~/` 会在解析时展开）；单项配错只让该工具
/// fail-closed，不影响其它工具。`tmux_bin` / `sentinel.lark_cli_path` 由 resolver 兼容读取。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Tools {
    pub tmux: Option<PathBuf>,
    pub lark_cli: Option<PathBuf>,
    pub claude: Option<PathBuf>,
    pub codex: Option<PathBuf>,
    /// traex(traecli):codex 的 fork 的可执行路径,缺省走 PATH 解析。
    pub traex: Option<PathBuf>,
    pub cursor_agent: Option<PathBuf>,
    pub git: Option<PathBuf>,
    pub ssh: Option<PathBuf>,
    pub npx: Option<PathBuf>,
    pub pkill: Option<PathBuf>,
    pub open: Option<PathBuf>,
    pub xdg_open: Option<PathBuf>,
    pub launchctl: Option<PathBuf>,
    pub systemctl: Option<PathBuf>,
    pub loginctl: Option<PathBuf>,
    pub pbcopy: Option<PathBuf>,
    pub osascript: Option<PathBuf>,
}

/// Menu Bar 配置。
///
/// `terminal` 是 `summon agents open <target>`「在终端里打开会话」用的 argv 模板
/// (`{cmd}` / `{title}` 占位)。留空 = 系统 Terminal.app。终端是个人口味,不硬编码,
/// 换终端只改 config.yaml。
///
/// ```yaml
/// menubar:
///   terminal: ["cmux", "new-workspace", "--name", "{title}", "--command", "{cmd}"]
///   icon: xyz
/// ```
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct MenuBar {
    pub terminal: Vec<String>,
    /// 状态栏的 summon 图标名(见 `menubar::MENUBAR_ICONS`)。None = 还没选过:onboard 据此决定
    /// 要不要问,Menu Bar 按默认图标画。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    /// 状态栏在 summon 图标旁显示 working / waiting 计数;关掉只剩图标(计数照样在悬停提示里)。
    /// None = 没配,按开。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub show_counts: Option<bool>,
}

impl MenuBar {
    pub fn show_counts(&self) -> bool {
        self.show_counts.unwrap_or(true)
    }
}

/// claude 托管会话的行为开关(config.yaml 的 `claude:` 段)。**只在本机生效**:
/// statusLine wrapper 是 claude 的子进程,读的就是这台机器的 config.yaml。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct Claude {
    /// summon 起的会话里,要不要照常执行用户自己那条 statusLine 命令。**默认关**。
    ///
    /// 关掉的理由是显示:同样的信息已经透到飞书角标上,pane 里再画一遍占掉两行、还给抓屏
    /// 判据添噪音。但有的 statusLine 命令不只是画 —— 它同时把数据透传给统计平台,不跑就断了
    /// (owner 2026-09-01)。所以给它一个开关,由机器所有者按自己那条命令的性质决定。
    ///
    /// **额度/上下文采集与这个开关无关**:快照在委派之前就已经写完,角标该有的数据一项不少。
    pub status_line_in_managed_sessions: bool,
}

/// cursor 托管会话的行为开关(config.yaml 的 `cursor:` 段)。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct Cursor {
    /// 个人额度查询,供飞书角标的「quota N%」。
    pub quota: CursorQuota,
}

/// Cursor 个人额度查询。Cursor 没有任何通道把额度推给 summon,只能主动去 cursor.com 查,
/// 所以每一项都在回答「什么时候查、拿谁的凭据查」。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct CursorQuota {
    /// 整台机器的总开关,默认开。真正的开关是会话面板的「额度显示」:没打开的会话一次也不查,
    /// 所以默认开不会让谁的凭据被发出去。这里关掉 = 本机不再向 cursor.com 查额度。
    pub enabled: bool,
    /// 同一凭据来源两次查询的最小间隔(秒),默认 600。用同一份登录态的几个 agent 共用一次
    /// 查询;失败也计入间隔,凭据过期或接口改版时不重锤。低于 [`CursorQuota::MIN_INTERVAL_SECS`]
    /// 按它算。
    pub interval_secs: u64,
    /// 取 Cursor 登录凭据的命令,经 `sh -c` 执行,stdout 第一行即 access token。命令只拿到
    /// 系统基础环境(PATH、HOME 等),看不到 daemon 的其余环境。
    /// 不写 = 读 cursor-agent 自己的登录态(macOS 钥匙串,其余平台
    /// `$XDG_CONFIG_HOME/cursor/auth.json`)。登录态放在别处(IDE、密码管理器)时用它。
    /// 只作用于没配 `runtime.cli_cmd` / `runtime.env` 的 agent,理由见
    /// `Runtime::cursor_quota_token_command`。
    pub token_command: Option<String>,
}

impl CursorQuota {
    pub const MIN_INTERVAL_SECS: u64 = 60;

    pub fn interval(&self) -> Duration {
        Duration::from_secs(self.interval_secs.max(Self::MIN_INTERVAL_SECS))
    }
}

impl Default for CursorQuota {
    fn default() -> Self {
        Self {
            enabled: true,
            interval_secs: 600,
            token_command: None,
        }
    }
}

/// owner sentinel 配置。持 owner 用户身份轮询飞书并代 owner 执行拉群/建群,属于
/// 有对外副作用的长期行为,故**默认关闭**;设计与红线见
/// `docs/spec/2026-08-07-owner-sentinel-design.md`。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Sentinel {
    pub enabled: bool,
    /// 只报告命中、不执行任何对外动作(拉群/建群/发卡都改为日志)。新装/大改后先 dry 一轮。
    pub dry_run: bool,
    /// 慢速补偿队列刷新与 `/summon` 全文搜索的间隔(秒)。保留原字段名以兼容既有配置。
    pub interval_secs: u64,
    /// 快速通道间隔(秒)。快速通道只看未设为勿扰的活跃会话。
    pub fast_interval_secs: u64,
    /// 快速通道每轮扫描的未勿扰活跃会话数。
    ///
    /// **默认 20 而不是 5**(owner 2026-08-10):fast 覆盖不到的会话要等 slow 通道
    /// (`60s × slow_batch_size`,159 个会话时轮一圈最坏 25 分钟),而召唤的时效窗只有 300s ——
    /// 排不进 fast 的群,召唤基本必漏。把常用的十几个群都纳入 fast,命中延迟从"最坏 25 分钟"
    /// 降到"最坏一个 tick"。代价是每 tick 多十几次 lark-cli 调用,而 CPU 侧已不是约束
    /// (同日把 daemon 从 70% 降到 2%),真正的上限是飞书 API 频率,单用户下远未触及。
    pub fast_top_k: usize,
    /// 每个快速 tick 从慢速补偿队列取出的会话数。勿扰群与活跃头部之外的会话走这里。
    pub slow_batch_size: usize,
    /// 慢速补偿队列的活跃会话数上限(按 ByActiveTimeDesc 取头部)。
    pub top_k: usize,
    /// 触发消息的时效窗(秒):create_time 距今超过它就不动作(首启复放防御的一半;另一半是水位)。
    pub max_age_secs: i64,
    /// 每小时自动动作上限(拉群+建群合计),超出只记录并私聊告知。
    pub per_hour: usize,
    /// 「进来回答」动作词表(字面前缀)。
    pub join_verbs: Vec<String>,
    /// 「建群」动作词表(字面前缀)。
    pub create_verbs: Vec<String>,
    /// **只拉人、不建会话**的动作词(默认 `/invite`)。
    ///
    /// 与 join_verbs 的区别是意图而非程度:那是「进来并回答」,这是「先进来待命」。典型用法是
    /// 拉进群后改用 `/bridge` 桥接一个已有 CLI 会话 —— 此时 summon 自己起的会话纯属多余,
    /// 还会先占住该 chat 的会话位。
    ///
    /// 用斜杠命令而非自然语言词:它是明确指令,不该被「进来」这类口语误命中(匹配顺序上也排在最前)。
    pub invite_verbs: Vec<String>,
    /// 黑名单 chat_id:命中即忽略(敏感群兜底)。
    pub blocklist: Vec<String>,
    /// 自定义称呼映射:agent name → 额外触发称呼(如 arcadia → ["阿卡迪亚","Arcadia"])。
    /// 触发文本匹配的是 owner 的打字习惯,与飞书 bot 显示名无关(bot 不在会话时 @ 是纯文本)。
    pub aliases: HashMap<String, Vec<String>>,
    /// owner 的 open_id(触发 sender 过滤),lark-cli 那个应用里的。留空 = 取 `lark_profile`
    /// 那份 lark-cli 配置里登录的用户(`auth status`)。
    pub owner_open_id: Option<String>,
    /// lark-cli profile(用户身份通道),须是登录了 owner 用户身份的那个。留空 = 跟着 lark-cli
    /// 默认 profile 走;默认 profile 是全机共用的,别的操作把它改成只有 bot 身份(比如专给某个 bot
    /// 用)时 sentinel 会整体停摆 —— 所以建议显式写上。停摆时会私聊 owner 一张说明卡。
    pub lark_profile: Option<String>,
    /// 兼容别名；新配置写 `tools.lark_cli`。sentinel 每 tick 重读时仍会解析这两个字段。
    pub lark_cli_path: Option<String>,
}

impl Default for Sentinel {
    fn default() -> Self {
        Self {
            enabled: false,
            dry_run: false,
            interval_secs: 60,
            fast_interval_secs: 5,
            fast_top_k: 20,
            slow_batch_size: 2,
            top_k: 50,
            max_age_secs: 300,
            per_hour: 6,
            join_verbs: vec![summon_i18n::source!("r9a1f1cdb91").to_string()],
            create_verbs: vec![summon_i18n::source!("rdba68a7283").to_string()],
            invite_verbs: vec!["/invite".to_string()],
            blocklist: Vec::new(),
            aliases: HashMap::new(),
            owner_open_id: None,
            lark_profile: None,
            lark_cli_path: None,
        }
    }
}

/// serde `default` 钩子:bool 字段缺省为 true(老配置缺该键时按「开」解释,如 `session_mirror`)。
fn default_true() -> bool {
    true
}

/// 文件预览服务器配置。**默认开**:`bind` 默认 127.0.0.1,只有本机能连,
/// 开着不暴露任何局域网可达面;要让别的机器(如 Mac 浏览器访问 devbox)读到,
/// 必须显式 `bind: 0.0.0.0` + `public_base_url`,那是机器所有者的有意决定。
/// 设计与安全模型(token 链接 / fail-closed 识别判据 / 点击认证)见
/// `docs/spec/2026-08-25-file-preview-server-design.md` 与
/// `docs/spec/2026-10-20-link-click-auth-design.md`。
///
/// ```yaml
/// preview_server:
///   enabled: false                 # 显式关闭
///   bind: "0.0.0.0"                # 只给本机用则保持 127.0.0.1
///   port: 7765
///   public_base_url: "http://10.0.0.2:7765"   # 按钮链接用的地址;留空 = http://localhost:<port>
///   token_ttl_secs: 3600           # 链接有效期(秒)
///   max_file_mb: 10
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PreviewServer {
    pub enabled: bool,
    pub bind: String,
    pub port: u16,
    /// 按钮链接用的对外地址。**留空 = `http://localhost:<port>`**——只对「浏览器与 daemon
    /// 同机」成立;devbox 这类远端机器必须显式填使用者浏览器可达的地址(机器自己猜不到
    /// NAT/VPN 后的自己)。
    pub public_base_url: String,
    /// 预览链接自签发起的有效期(秒)。链接经明文 HTTP 在局域网可达,有效期就是暴露窗口。
    pub token_ttl_secs: u64,
    pub max_file_mb: u64,
}

impl Default for PreviewServer {
    fn default() -> Self {
        Self {
            enabled: true,
            bind: "127.0.0.1".to_string(),
            port: 7765,
            public_base_url: String::new(),
            token_ttl_secs: 3600,
            max_file_mb: 10,
        }
    }
}

impl PreviewServer {
    /// 按钮链接的 base URL(不带尾斜杠)。
    ///
    /// `public_base_url` 留空时用 `host` —— 由调用方现查的本机对外地址([`pick_public_host`])。
    /// 查不到就 `None`:**宁可不给链接,也不给一个注定点不开的链接**。localhost 不是兜底 ——
    /// 链接的收件人是飞书客户端,它可能在手机上,localhost 会打到收件设备自己。
    pub fn base_url_with_host(&self, host: Option<std::net::Ipv4Addr>) -> Option<String> {
        let trimmed = self.public_base_url.trim_end_matches('/');
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
        host.map(|ip| format!("http://{ip}:{}", self.port))
    }

    /// 单文件大小上限(字节)。
    pub fn max_file_bytes(&self) -> u64 {
        self.max_file_mb.saturating_mul(1024 * 1024)
    }
}

/// 浏览器内终端(只读查看 / 可操作)。**出厂开**(owner 2026-09-22):链接只经 operate-gate
/// 私聊签发、一次性,监听地址沿用 `preview_server.bind`。与预览服务一样只在 daemon 启动时读。
/// 设计见 `docs/spec/2026-09-22-web-terminal-design.md`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct WebTerminal {
    pub enabled: bool,
    pub port: u16,
}

impl Default for WebTerminal {
    fn default() -> Self {
        Self {
            enabled: true,
            port: 7766,
        }
    }
}

impl WebTerminal {
    /// 主机与预览服务同一套探测,端口是终端自己的。`public_base_url` 不参与:它写死了预览端口,
    /// 而需要它的 devbox 上没有 tmux。查不到可路由主机就不出链接。
    pub fn base_url_with_host(&self, host: Option<std::net::Ipv4Addr>) -> Option<String> {
        host.map(|ip| format!("http://{ip}:{}", self.port))
    }
}

/// 配置控制台(网页上查看与修改 `~/.summon/` 的配置)。**出厂开**:远程链接只经 operate-gate 私聊
/// 签发、点击认证后才能进入;本机经 `summon dashboard open` 的一次性本机码进入。监听地址沿用
/// `preview_server.bind`,只在 daemon 启动时读。设计见 `docs/spec/2026-10-20-dashboard-console-design.md`。
///
/// ```yaml
/// dashboard:
///   enabled: true
///   port: 7767
///   public_base_url: ""   # 远程链接的对外地址;留空 = 预览的对外主机 + 本端口
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Dashboard {
    pub enabled: bool,
    pub port: u16,
    /// 远程链接用的对外地址(反向代理、端口转发时填)。留空时主机取与文件预览相同的那个
    /// (预览配了 `public_base_url` 取它的主机,否则现查本机可路由地址),端口用 `port`。
    pub public_base_url: String,
}

impl Default for Dashboard {
    fn default() -> Self {
        Self {
            enabled: true,
            port: 7767,
            public_base_url: String::new(),
        }
    }
}

/// 自动升级(daemon 定期问发布源,有合适的新版就取到本机,等整机空闲后换代)。
///
/// ```yaml
/// upgrade:
///   auto: false               # 默认关
///   check_interval_hours: 6   # 至少 1
///   min_age_hours: 2          # 新版发布满这么久才采用
///   idle_minutes: 10          # 整机连续空闲这么久才重启
///   range: minor              # minor:跟到同一主版本内的次版本;patch:只跟同一 x.y 的补丁版本
///   channel: github           # 新版从哪个渠道取;不写用本构建的默认渠道
///   from: /path/to/release    # 演练用的 Release 来源;不写即 GitHub 上的 Release
/// ```
///
/// 主版本号变化一律不自动跟。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Upgrade {
    pub auto: bool,
    pub check_interval_hours: u64,
    pub min_age_hours: u64,
    pub idle_minutes: u64,
    pub range: UpgradeRange,
    /// 新版从哪个渠道取:`github` 是 GitHub Releases,其余取值看本构建带了哪些渠道。取值在用时
    /// 校验(写错只让自动升级不生效、doctor 说明,不影响别的配置)。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,
    /// 演练用的 Release 来源(只用于 github 渠道):绝对路径的本地目录,或本机回环地址上的明文
    /// `http://`;其下直接有 `manifest.json` 与各平台的包。别的写法不认、不自动升级。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
}

impl Default for Upgrade {
    fn default() -> Self {
        Self {
            auto: false,
            check_interval_hours: 6,
            min_age_hours: 2,
            idle_minutes: 10,
            range: UpgradeRange::Minor,
            channel: None,
            from: None,
        }
    }
}

/// 自动升级跟到哪一级版本。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UpgradeRange {
    /// 只跟同一 x.y 的补丁版本。
    Patch,
    /// 跟到同一主版本内的次版本。
    Minor,
}

/// 从本机地址里挑一个**收件设备能路由到**的 IPv4。
///
/// 排除法,每一条都是「填了也点不开」的地址:回环(打到收件设备自己)、链路本地、
/// 家用私网 `192.168/16`(只在同一个 WiFi 下成立)、容器网段 `172.16/12`、
/// 代理软件常占的 benchmark 网段 `198.18/15`。
///
/// 剩下的**优先 `10/8`** ——企业内网与 VPN 都落在这里,是唯一「换个网络挂上 VPN 还能到」的一档;
/// 没有就退而取公网地址。全都不合格返回 `None`,调用方据此不 mint。
pub fn pick_public_host(addrs: &[std::net::Ipv4Addr]) -> Option<std::net::Ipv4Addr> {
    let usable = |ip: &std::net::Ipv4Addr| {
        let o = ip.octets();
        !ip.is_loopback()
            && !ip.is_link_local()
            && !ip.is_broadcast()
            && !ip.is_unspecified()
            && !ip.is_multicast()
            && !(o[0] == 192 && o[1] == 168)
            && !(o[0] == 172 && (16..=31).contains(&o[1]))
            && !(o[0] == 198 && (o[1] == 18 || o[1] == 19))
    };
    let enterprise = |ip: &std::net::Ipv4Addr| ip.octets()[0] == 10;
    addrs
        .iter()
        .find(|ip| usable(ip) && enterprise(ip))
        .or_else(|| addrs.iter().find(|ip| usable(ip)))
        .copied()
}

/// serde `default` 钩子:老配置没写 `progress_max_steps` 时用 core 的默认值(不在这里另写数字,
/// 免得两处漂移)。
fn default_progress_max_steps() -> usize {
    crate::progress::DEFAULT_MAX_STEPS
}

/// `~/.summon/config.yaml` 路径(load + 热重载 mtime 跟踪用)。
pub fn config_path() -> PathBuf {
    dirs::home_dir()
        .map(|h| h.join(".summon/config.yaml"))
        .unwrap_or_default()
}

/// 「上次成功解析的 config」进程级缓存。热重载每轮调 `load()`,解析失败时沿用它,
/// 避免「编辑保存到一半正好来消息」把运行态配置瞬间打回默认。
static LAST_GOOD: Mutex<Option<Config>> = Mutex::new(None);

fn last_good() -> Option<Config> {
    LAST_GOOD.lock().ok().and_then(|g| g.clone())
}

fn set_last_good(c: Config) {
    if let Ok(mut g) = LAST_GOOD.lock() {
        *g = Some(c);
    }
}

/// 解析 config.yaml 文本。成功→`(值, None)`;失败→`(fallback, Some(错误信息))`。
/// 纯函数(不碰 IO / 全局态):把「解析失败即降级」的判定,与 last-good 缓存、日志解耦,便于单测。
/// **关键:解析失败绝不返回 `None` 式的静默默认——错误信息一定回传,由调用方大声报出来。**
/// tmux 可执行文件的候选安装位置(按优先级)。`doctor` 据此把「这台机器上能用的 tmux」列给用户挑,
/// 而不是让人自己去猜路径。
///
/// 覆盖三类装法:各平台 homebrew(macOS arm/intel、**Linuxbrew 装在家目录**——2026-08-07 devbox
/// 现场踩的就是这个:daemon PATH 里写着 macOS 的 `/opt/homebrew/bin`,而实际在 `~/homebrew/bin`)、
/// 系统包管理器、以及用户自装的 `~/.local/bin`。
pub const TMUX_CANDIDATE_DIRS: [&str; 6] = [
    "/opt/homebrew/bin",              // macOS Apple Silicon homebrew
    "/usr/local/bin",                 // macOS Intel homebrew / 通用自装
    "~/homebrew/bin",                 // Linuxbrew 装在家目录(devbox 就是这种)
    "~/.linuxbrew/bin",               // Linuxbrew 默认位置之一
    "/home/linuxbrew/.linuxbrew/bin", // Linuxbrew 系统级默认位置
    "~/.local/bin",                   // 用户自装
];

pub fn parse_or_fallback(yaml: &str, fallback: Config) -> (Config, Option<String>) {
    match serde_yaml::from_str::<Config>(yaml) {
        Ok(cfg) => (cfg, None),
        Err(e) => (fallback, Some(e.to_string())),
    }
}

impl Config {
    /// 从 `~/.summon/config.yaml` 读。三态区分,**失败绝不静默、也绝不打回默认**:
    /// - 文件不存在 → 纯默认(常态,唯一允许静默的分支);
    /// - 存在且合法 → 解析值,并缓存为「上次好配置」;
    /// - 存在但读不出(权限等)或解析失败 → stderr 大声报错(落 `~/.summon/run/daemon.log`)
    ///   + 沿用上次好配置(无则默认)。
    ///
    /// why 要 last-good 兜底而不是 `Config::default()`:热重载每轮调本函数,编辑 config.yaml
    /// 保存到一半正好来条消息，一个 YAML 笔误就会把运行态配置整套打回默认——
    /// 表现得像「用户主动重置了所有开关」。告警只让人事后知情,兜底才让运行态不塌。
    pub fn load() -> Self {
        let path = config_path();
        let fallback = last_good().unwrap_or_default();
        let s = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            // 文件不存在 = 常态(从没写过 config.yaml),静默用默认。
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Config::default(),
            // 存在但读不出(权限/IO)= 异常:报错并沿用上次好配置,不打回默认。
            Err(e) => {
                eprintln!("{}", summon_i18n::format!("rf772603d14", path.display()));
                return fallback;
            }
        };
        let (cfg, err) = parse_or_fallback(&s, fallback);
        match err {
            None => set_last_good(cfg.clone()),
            Some(e) => eprintln!("{}", summon_i18n::format!("r6afdf4a98b", path.display())),
        }
        cfg
    }
}

/// 完整(resolved)消息层配置。内置默认来自 `assets/messaging.default.yaml`,**不在代码里 hardcode**;
/// 用 `builtin_messaging()` 取默认,再用 `apply()` 叠加全局/每-agent 覆盖。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Messaging {
    /// 冷启教给 agent 的系统输出约定(PRIMER)。summon 直接抓结构化输出
    /// (子进程 stdout / 转录 jsonl),只讲称呼/@/附件约定 + 「自然作答」。
    /// **可覆盖的 base**(tier2):用户/agent 可在 `~/.summon` 的 `messaging.system_prompt` 整个改写,
    /// 作为自定义开场白。默认空——所有运行时协议都在不可覆盖的 `system_prompt_core` 里。
    pub system_prompt: String,
    /// **内置 app-core 协议**(tier1,优先级高):summon 运行时依赖 agent 知道的协议(自然作答/排版、
    /// 称呼&@递归防护、附件、preview/remind 等动作通道)。**不在 `MessagingOverride` 里
    /// → 用户覆盖不掉**,随二进制升级、每次注入,堵住「升级后 ~/.summon 旧模板拿不到新协议」的黑洞。
    /// 只写机制事实,做事偏好归用户层(见 `messaging.default.yaml` 头部注释)。
    /// 含 `{ask_protocol}`、`{reactions_protocol}`、`{feed_protocol}`、`{background_protocol}`
    /// 等占位,后三者按 [`PrimerSections`] 填对应段或空。
    #[serde(default)]
    pub system_prompt_core: String,
    /// **表情约定段**(tier1,与 `system_prompt_core` 同属不可覆盖的 app-core)。
    /// 填进 core 的 `{reactions_protocol}` 占位;`agent_reactions` 关掉时占位填空,整段不进 PRIMER。
    #[serde(default)]
    pub system_prompt_reactions: String,
    /// 是否给 agent 注入表情约定段(**默认开**)。分级开关,四层可配(内置 → config.yaml →
    /// agent.yaml → per-chat),声明见 `settings::SETTINGS`。
    ///
    /// **只隔离 PRIMER**:关掉后 `summon react` 命令照常可用(agent 若从别处知道它仍能贴),
    /// 系统自动的 OnIt/DONE 工程表情也一个字不动 —— 那是运行状态指示,不是「agent 回复表情」。
    #[serde(default = "default_true")]
    pub agent_reactions: bool,
    /// **feed 总结段**(tier1,不可覆盖)。填进 core 的 `{feed_protocol}` 占位;`agent_feed` 关掉时
    /// 占位填空。
    #[serde(default)]
    pub system_prompt_feed: String,
    /// 是否给 agent 注入 feed 总结段(**默认开**)。分级开关,声明见 `settings::SETTINGS`。
    /// **只隔离 PRIMER**:回复末尾的标签无论开关如何都会从正文摘掉。
    #[serde(default = "default_true")]
    pub agent_feed: bool,
    /// 是否处理文档评论并起草回复；全局配置可由单个 agent 覆盖。
    #[serde(default)]
    pub document_comments_enabled: bool,
    /// 是否处理非 owner 发出的文档评论；默认关闭，支持单 agent 覆盖。
    #[serde(default)]
    pub document_comments_non_owner_enabled: bool,
    /// Owner 主动提及可使用用户权限读取其它文档；默认关闭。
    #[serde(default)]
    pub document_comments_owner_mentions_enabled: bool,
    /// 单次文档评论起草的总时限，单位秒；全局配置可由单 agent 覆盖。
    #[serde(default = "default_document_comment_timeout_secs")]
    pub document_comment_timeout_secs: NonZeroU64,
    /// agent 可选 + daemon 校验的安全表情集。
    pub reactions: Vec<String>,
    /// 表情含义映射(KEY → 中文含义)。写 emoji 文件时,有含义的输出「KEY(含义)」。
    #[serde(default)]
    pub reaction_meanings: HashMap<String, String>,
    /// 兜底表情。
    pub default_reaction: String,
    /// 收到消息即贴的表情(daemon 直连)。
    pub working_reaction: String,
    /// 任务完成补的表情。
    pub done_reaction: String,
    /// 本轮被「⏸ 打断」按钮中止时补的表情(任务4):撤掉 working 后贴它,标记「已打断/暂停」。
    /// 取值须在 `reactions` 安全集内(见 messaging.default.yaml)。
    pub interrupted_reaction: String,
    /// 每轮注入模板,占位 `{source}` `{who}` `{text}` `{chat}`。
    pub prompt_template: String,
    /// 会话空闲多久发提示卡片(秒)。
    pub idle_ttl_secs: u64,
    /// 无头轮真静默上限（正整数秒），在轮启动时冻结。
    #[serde(default = "default_headless_stall_secs")]
    pub headless_stall_secs: NonZeroU64,
    /// 会话空闲多久**自动退出**(秒);`None` = 永不自动退出(内置默认)。
    ///
    /// 退出语义是「退出续聊」——保留 resume 记录、只释放会话进程(claude 的 tmux / codex、
    /// cursor 跨轮存活的 RPC 会话),下条消息按原生 resume 接回上下文,不是重置。
    /// **这是唯一由扫描线程发起的会话终止**,与 CLAUDE.md 第一红线「回收/空闲扫描从不擅杀」
    /// 是 owner 2026-08-12 显式授权的例外,因此:
    /// - 默认 `None`(fail-closed):不写这行的机器行为逐字节不变;
    /// - 判据全部为真才动手,任一条证不了就跳过(见 `should_retire_idle_session`)。
    pub idle_retire_secs: Option<u64>,
    /// **定时任务尾轮**会话空闲多久静默自动退出(秒);`0` = 关。
    ///
    /// 针对「最后一轮入站是定时器触发」的会话:定时轮之后这么久没有任何新消息,就**不发**
    /// 空闲提醒卡,直接走与 `idle_retire_secs` 完全相同的「退出续聊」退出通路(回执文案也
    /// 复用 `session_exited.idle_retired`)——定时任务会话安静地走,不打扰群里的人。
    /// 人类消息一到,该会话立刻回到普通规则(idle_ttl 提醒卡 + idle_retire)。
    ///
    /// **默认开(3300 = 55min)**:这是 owner 2026-08-13 显式授权的默认值,是本仓「回收类
    /// 机制默认关(fail-closed)」惯例的一次显式例外,不是随手改的。要关在 `messaging:` 下写
    /// `timer_idle_retire_secs: 0`(写 `null` 不算显式关闭,只会落回内置默认,见 override 注释)。
    ///
    /// 三种捕获模式同一条通路,判据与 `idle_retire_secs` 相同:默认退出续聊,
    /// `timer_idle_retire_reset` 打开才重置。都要求没有活跃 turn/subagent,且盘上有
    /// session id;结构条件不可证时保持会话不动。
    #[serde(default = "default_timer_idle_retire_secs")]
    pub timer_idle_retire_secs: u64,
    /// 定时任务尾轮会话空闲归档时是否**重置**(清 resume 记录,下次触发全新 session)。
    ///
    /// 默认 false = 退出续聊(保留 resume 记录,下次定时器触发接回原上下文)。置 true 后,
    /// 定时会话到点静默退出时走 Reset 而非 Clean:resume 记录一并清掉,下次触发零历史。
    /// 仅当尾轮确实是定时器合成轮时生效;人类消息尾轮的空闲归档永远走续聊。
    ///
    /// per-chat 可在 schedules 面板单独开关,覆盖此 agent 维度默认。
    #[serde(default = "default_timer_idle_retire_reset")]
    pub timer_idle_retire_reset: bool,
    /// dispatcher 空闲扫描节拍(秒)。
    pub idle_scan_secs: u64,
    /// 跨后台任务续跑的时间上限(秒)。一轮 `end_turn` 时仍有 `Bash(run_in_background)` 未完成,
    /// 主路径不收尾、续跑到 task-notification 到达;超过它就强制收尾,防止后台任务永不完成时
    /// 主路径永久挂起。**默认 3600(一小时)**(owner 2026-08-17 决策一天,Arcadia review 后
    /// 降到一小时:后台工具失败/被杀时会话冻结时长与 cap 等长,一天太久)。**0 = 关**(不续跑,
    /// 与 `timer_idle_retire_secs` 的 0=关 约定一致;worker 临时会话恒传 ZERO)。只对 claude/Transcript 生效。
    #[serde(default = "default_background_continuation_max_secs")]
    pub background_continuation_max_secs: u64,
    /// claude/Transcript 路径的**轮内 steer** 开关(**默认开**)。summon 不再自己排队,轮内到达的
    /// 普通消息直接注入 claude、由它自己的队列在下一个工具边界消费,N 条消息可能被合成一轮回答。
    ///
    /// 它打破了「一条飞书消息 ↔ 一轮 ↔ 一张卡」的 1:1 模型,代价由**过程展示**承担:
    /// 追加消息被真正消费的那一刻,卡片路径切卡、CoT 路径打段边界(见 `content_boundary`),
    /// 用户因此看得见「从这里起是我那条追加」。消费判据本身是白名单式的严格判定,
    /// 入队不算送达(见设计文档),所以这些边界落在真实的消费时刻上而不是猜测的时刻。
    /// 见 `docs/spec/2026-08-04-claude-steer-recognition-design.md`。
    #[serde(default = "default_true")]
    pub steer_transcript: bool,
    /// RPC(codex app-server)路径的**轮内 steer** 开关(**默认开**)。**刻意与 `steer_transcript`
    /// 分开而不是复用一个开关**:两条路的注入机制毫无共同点 —— tmux 那条是往 TUI 里打字、
    /// 由 claude 自己的队列消费,这条是往 app-server 发 `turn/steer` 帧、由 codex 在当前 turn 里
    /// 追加输入。风险面不同,必须能各自 kill。
    ///
    /// 送达确认也弱一档:`turn/steer` 只保证帧写进了 stdin,server 仍可能因 `expectedTurnId`
    /// 过期而拒(见 `RpcSteerOutcome::Sent`)。cursor 的 ACP 没有对应能力,开了对它是空操作
    /// (见 `SoftInterrupt::steer`)。
    #[serde(default = "default_true")]
    pub steer_rpc: bool,
    /// Cursor Headless CLI Session 的 steer 开关。**默认开**：当前 Cursor CLI 没有结构化的
    /// mid-turn 输入接口，summon 采用「SIGINT 当前 `-p` → 保留 session id → `--resume` 并重喂
    /// 原提示/部分输出/新消息」的可恢复语义。它不是伪装成原生 steer：当前轮会重启，卡片明确
    /// 显示“收到新消息，续跑中”。与 `steer_rpc` 分开，切到 ACP 降级通道后不会误用 CLI 语义。
    #[serde(default)]
    pub steer_cli_session: bool,
    /// 会话镜像开关(**默认开**)。仅 claude/Transcript:开则起 per-session 常驻 tailer 把转录里**任何**
    /// assistant turn(含用户 attach 终端直敲触发的)也推回飞书(RPC→pub-sub)。可在 config/agent.yaml 的
    /// `messaging:` 下显式 `session_mirror: false` 关掉(回退现有 turn-scoped 路径)。见
    /// `docs/spec/2026-06-05-session-mirror-and-reactions-design.md`。
    #[serde(default = "default_true")]
    pub session_mirror: bool,
    /// Claude 授权请求 / AskUserQuestion 走 PermissionRequest hook 转飞书卡片(**默认开**)。
    /// 关掉后 hook 一律不作答,请求落回终端对话框与抓屏中介;判定在 daemon 侧逐次做,
    /// 在跑的会话即刻生效。见 `docs/spec/2026-10-16-claude-permission-hook-design.md`。
    #[serde(default = "default_true")]
    pub claude_permission_hook: bool,
    /// 邮箱打码重发(**默认开**):消息被飞书内容审核以含邮箱整条拒发时,把其中的邮箱打成只留
    /// 用户名首字母(`p***@example.com`)再发一次;没被拒的原样送达。被拒时按配置文件当前内容
    /// 判定,改了即生效。
    #[serde(default = "default_true")]
    pub mask_email: bool,
    /// turn 完成「双卡」开关(**默认开**):每轮跑完把过程卡(流式卡)定格成「✅ 已完成 + 折叠完整
    /// 回复」,并另发一张「📋 总结」卡作为完成信号(新气泡)。`messaging:` 下设
    /// `completion_summary_card: false` 关掉(回退单卡:流式卡末态即最终回复)。
    #[serde(default = "default_true")]
    pub completion_summary_card: bool,
    /// live 卡工具轨迹保留的最大步数(**默认 3**)。只对「思考内容不可见」那一档(claude 自家模型)
    /// 有渲染效果 —— 别的模型的过程由 thinking 正文承担,不走轨迹。取小值是因为轨迹回答的是
    /// 「刚做完什么、正在做什么」,不是完整日志(完整的在 events.jsonl)。`0` 按 1 处理:一步不留
    /// 会连「🔧 执行 X 中…」状态行一起抹掉。
    #[serde(default = "default_progress_max_steps")]
    pub progress_max_steps: usize,
    /// 是否隐藏 thinking 块正文(**默认 false**)。开 → 有明文 thinking 的模型(经网关接入的模型)
    /// 表现等同 claude 自家模型:正文不上卡,同时工具轨迹(progress trail)自动启用 —— 复用
    /// 同一条 `redacted_thinking` 轴,不另搞一套。关 → 现状(thinking 正文照渲,轨迹关)。
    /// 按 bot 配:agent.yaml 的 `messaging.hide_thinking: true` 只影响该 agent。
    #[serde(default)]
    pub hide_thinking: bool,
    /// 是否外显 CoT 过程内容(思考、工具、过程回复)。关时只保留运行状态、呼吸效果与暂停按钮,
    /// 只看终态回复。
    #[serde(default = "default_true")]
    pub cot_visible: bool,
    /// CoT 是否展示工具调用(命令、参数、结果;**默认关**)。关时 CoT 只有思考正文。
    #[serde(default)]
    pub cot_tool_details: bool,
    /// 回复卡角标是否显示思考档位(**默认开**)。分级开关,四层可配(内置 → config.yaml →
    /// agent.yaml → per-chat),声明见 `settings::SETTINGS`。
    ///
    /// 只管渲染:关掉后档位照常采集,重新打开当轮就有值可显。
    #[serde(default = "default_true")]
    pub badge_effort: bool,
    /// 是否强制显示工具轨迹(三行状态栏:**默认 false**)。开 → 即使 thinking 可见(有明文)
    /// 也渲染工具轨迹;关 → 现状(只有 thinking 不可见时才渲染,给看不到思考的模型补形状)。
    /// 按 bot 配:agent.yaml 的 `messaging.tool_trail: true` 只影响该 agent。
    #[serde(default)]
    pub tool_trail: bool,
    /// 会话控制面板上可一键切换的模型预设(扁平回退表;各 CLI 命名空间混在一起,优先用 `models_by_cli`)。
    #[serde(default)]
    pub models: Vec<String>,
    /// 按 CLI 种类(key=cli kind,如 claude/cursor/codex)分桶的模型候选;渲染/picker 优先查它,
    /// 命中即用,否则回退扁平 `models`。让 claude 的 `claude-opus-5` 与 codex 的
    /// `gpt-5.6-sol` 不混。
    #[serde(default)]
    pub models_by_cli: HashMap<String, Vec<String>>,
    /// codex/traex 的会话是否把 summon 的平台工具(preview / react / send_image)注册成
    /// app-server dynamic tools,让模型直接调用。**出厂关**:这三个能力经 CLI 一直可用,
    /// 打开只是换一条更顺手的通道,而其中两个直接往群里发东西、发出去撤不回来 —— 要不要
    /// 默认开是 owner 的灰度决定,不是实现的默认值。
    ///
    /// 工具定义只在 `thread/start` 注册(`thread/resume` 依赖 rollout 恢复),所以改了它
    /// **要新建会话**才生效:`/quit` 后接回的那条老 thread 上,工具该在的在、该没有的没有。
    #[serde(default)]
    pub codex_platform_tools: bool,
    /// 按 CLI 种类钉选的**默认**模型(key=cli kind)。候选目录由 backend 自报后,这里是唯一
    /// 还需要人来定的一项:上游换默认模型时不该悄悄换掉主力,钉了就以钉的为准。
    /// 没钉 → 用 backend 自报的默认 → 再没有才落到 `models_by_cli` 桶首项。
    #[serde(default)]
    pub default_model_by_cli: HashMap<String, String>,
    /// 会话控制面板/onboard picker 上可选的逻辑 effort 档位(规范全集的子集);最终再与各 CLI 的
    /// 合法集求交。只服务 claude/codex(cursor 档位在 model 名里)。
    #[serde(default)]
    pub efforts: Vec<String>,
    /// 会话被强制退出时回执给用户的文案(按 `ExitReason` 选串)。不在代码里 hardcode。
    #[serde(default)]
    pub session_exited: SessionExitedText,
    /// **slash 准入黑名单**(owner 2026-08-14:准入从白名单反转为可配置黑名单)。默认放行,
    /// 发现 badcase 录进来即可。生效集 = `default` 段 ∪ 该 backend 段(纯并集)。
    ///
    /// 出厂预置项(`/login` `/logout` `/config` `/model`)的真相源是 `messaging.default.yaml`,
    /// **不硬编码** —— owner 显式要求可配:用户在 `messaging.slash_blacklist.default` 下写一份
    /// 新清单就整段替换,写 `[]` 就是把出厂项全部移除。那是 owner 的权利。
    #[serde(default)]
    pub slash_blacklist: crate::driver::slash_pool::SlashBlacklist,
}

/// 「会话已退出」回执文案,按退出原因分别可配(messaging 覆盖、热重载)。
/// 缺省值来自 `messaging.default.yaml` 的 `session_exited:` 段。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionExitedText {
    /// `agents kill` / `down`(不带 `--reset`),以及外部销毁 tmux 的兜底合成。**续上下文**。
    pub manual_kill: String,
    /// `agents kill --reset`:额外清 resume 记录,**不续上下文**。
    pub manual_kill_reset: String,
    /// 面板「退出(退出续聊)」。
    pub panel_clean: String,
    /// 面板「📦 归档」。
    pub panel_reset: String,
    /// 空闲扫描自动退出(退出续聊)。
    pub idle_retired: String,
    /// agent 自行退出(退出续聊)。
    pub agent_archived: String,
    /// 群解散事件。
    pub group_disbanded: String,
    /// bot 被移出群事件。
    pub bot_removed: String,
    /// daemon 重启停机打断在跑无头一轮(outbox NotifyChat 用)。
    pub shutdown_interrupt: String,
}

impl Default for SessionExitedText {
    /// serde 在缺字段/整段时用它。内置默认值的真相源是 `messaging.default.yaml`(总会提供全部字段),
    /// 故这里给空串占位即可——绝不能回头调 `builtin_messaging()`(那会在解析时无限递归)。
    fn default() -> Self {
        SessionExitedText {
            manual_kill: String::new(),
            manual_kill_reset: String::new(),
            panel_clean: String::new(),
            panel_reset: String::new(),
            idle_retired: String::new(),
            agent_archived: String::new(),
            group_disbanded: String::new(),
            bot_removed: String::new(),
            shutdown_interrupt: String::new(),
        }
    }
}

fn default_document_comment_timeout_secs() -> NonZeroU64 {
    NonZeroU64::new(300).expect(summon_i18n::text!("r34b52e7840"))
}

/// 无头轮静默上限的反序列化兜底，与内置YAML一致。
fn default_headless_stall_secs() -> NonZeroU64 {
    NonZeroU64::new(1800).expect(summon_i18n::text!("r2051d621d2"))
}

/// `timer_idle_retire_secs` 的兜底默认(55min)。真相源仍是 `messaging.default.yaml`(3300);
/// 这里再兜一层是防某处对 `Messaging` 的部分反序列化把默认「静默变关」——默认开是 owner
/// 2026-08-13 的显式授权,两处必须一致。
fn default_timer_idle_retire_secs() -> u64 {
    3300
}

/// `timer_idle_retire_reset` 的兜底默认(false = 退出续聊)。与 `messaging.default.yaml` 一致。
fn default_timer_idle_retire_reset() -> bool {
    false
}

/// `background_continuation_max_secs` 的兜底默认(3600 = 一小时)。owner 2026-08-17 初定
/// 一天,Arcadia review 后降到一小时:后台工具失败/被杀时会话冻结时长与 cap 等长,一天太久。
fn default_background_continuation_max_secs() -> u64 {
    3600
}

/// 内置消息层默认:解析编译期打进来的 `messaging.default.yaml`(字段必须齐全,由测试保证)。
pub fn builtin_messaging() -> Messaging {
    serde_yaml::from_str(summon_i18n::include_text!("messaging.default.yaml"))
        .expect(summon_i18n::text!("rc4f4b673a4"))
}

/// `{ask_protocol}` 的两种填法 —— **提问出口按捕获形态分叉**,不是文风差异:
///
/// - 无头(cursor/codex):Codex RPC 的原生 requestUserInput 会被桥到飞书；其它无头形态未必有
///   原生桥，统一保留 `summon ask` 作为可验证的后备出口。工具不可用/自动 skipped 不能冒充人答过。
/// - claude:走 tmux,自己弹的 TUI 菜单已由 `parse::interactive` 抓屏中介到飞书,不需要命令;
///   而且在 tmux 里调阻塞命令会让转录长时间无新字节,撞上「静默 = 轮结束」的既有误判风险。
fn ask_protocol_text(headless: bool) -> &'static str {
    if headless {
        summon_i18n::text!("r5e8095db7c")
    } else {
        summon_i18n::text!("r287748e24e")
    }
}

/// `{background_protocol}` 的填法。agent 只知道「结束本轮 = 交付」时,想等后台任务就只能在前台
/// sleep,而 sleep 期间完成通知进不来,跑完了也要白等到 sleep 结束。
const BACKGROUND_PROTOCOL: &str = "r81fc214422";

/// PRIMER 里随分级开关注入或不注入的段。按段给布尔而不是传整份开关快照:worker 的表情段还取决于
/// 候选文件能不能落盘,同一个开关值在不同调用点可能对应不同的注入结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PrimerSections {
    /// 表情约定段(`{reactions_protocol}`)。
    pub reactions: bool,
    /// feed 总结段(`{feed_protocol}`)。
    pub feed: bool,
    /// 跨后台任务续跑段(`{background_protocol}`):会话的捕获形态会在 end_turn 时等后台任务的
    /// 完成通知(claude/Transcript 主路径)。续跑上限写 0 时这一段同样不注入。
    pub background_continuation: bool,
}

impl PrimerSections {
    /// 全部可选段都注入,即内置默认。
    pub const ALL: PrimerSections = PrimerSections {
        reactions: true,
        feed: true,
        background_continuation: true,
    };
}

/// 把三个及以上的连续换行压成两个(= 一个空行)。可选 PRIMER 段被关掉时,占位符前后各自
/// 保留的空行会连成一片,压掉多出来的那些;段内的单空行不动。
fn collapse_blank_runs(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut newline_run = 0usize;
    for ch in text.chars() {
        if ch == '\n' {
            newline_run += 1;
            if newline_run <= 2 {
                out.push(ch);
            }
        } else {
            newline_run = 0;
            out.push(ch);
        }
    }
    out
}

impl Messaging {
    /// 合成注入用 PRIMER = 用户可覆盖 base ⊕ 内置 app-core(权威层、置后)。app-core 总叠加
    /// (用户即便整个改写 base 也丢不掉),由此随二进制升级把新协议带给所有人。daemon 再在其上叠
    /// CLAUDE.md 运营层(`system_prompt_with_instructions`)。
    ///
    /// - `headless` = 该 agent 走无头一次性子进程(cursor/codex),决定 `{ask_protocol}` 填什么。
    /// - `sections` = 可选段填不填。`{emoji_file}` 占位只存在于表情段里,
    ///   所以表情段关掉时连它一起消失,`emoji_file` 参数不再影响输出。
    pub fn system_prompt_filled(
        &self,
        emoji_file: &str,
        headless: bool,
        sections: PrimerSections,
    ) -> String {
        let inject_reactions = sections.reactions;
        let reactions = if inject_reactions {
            self.system_prompt_reactions.trim_end()
        } else {
            ""
        };
        let feed = if sections.feed {
            self.system_prompt_feed.trim_end()
        } else {
            ""
        };
        let background =
            if sections.background_continuation && self.background_continuation_max_secs > 0 {
                ::summon_core::i18n::label(BACKGROUND_PROTOCOL)
            } else {
                ""
            };
        // 先把可选段填进去,再统一填占位 —— 反过来的话段内的 `{emoji_file}` 不会被替换,
        // 而那个漏填只在「段被注入」时才显形,测试里很容易看不见。
        let core = self
            .system_prompt_core
            .replace("{reactions_protocol}", reactions)
            .replace("{feed_protocol}", feed)
            .replace(
                "{message_protocol}",
                crate::messaging::prompt_protocol::instructions(inject_reactions).trim_end(),
            )
            .replace("{emoji_file}", emoji_file)
            .replace("{ask_protocol}", ask_protocol_text(headless))
            .replace("{background_protocol}", background);
        // 可选段留空后,占位前后的空行会连成一片。规整只做「三个及以上连续换行压成两个」,
        // 不碰段内单空行 —— PRIMER 的段落结构靠空行表达,压过头会让相邻两段粘在一起。
        let core = collapse_blank_runs(&core);
        compose_system_prompt(
            &self.system_prompt,
            &[(summon_i18n::text!("r14ef7bd659"), &core)],
        )
    }

    /// 渲染每轮注入 prompt。
    /// - `chat=None` 表示私聊,`Some(chat_name)` 表示群聊。
    /// - `addressed=true` 表示本 bot 被直接 @(消息开头的 self-mention 已剥离),或经一人一 Bot 验证
    ///   放行的免 @ 普通对话,用户在对本 bot 说话。
    /// - `addressed=false` 表示本 bot 只是被 CC 或没被 @(消息里非开头位置提到或完全没提),agent 自行判断是否响应。
    ///
    /// 群名不再每轮重复注入(会话级 PRIMER/compact 声明一次即可,保 token)。
    /// - `sender_is_bot=true` 表示发信人是另一个 bot(sidecar 透传的 `sender_type == "app"`)。
    ///   **必须在 prompt 里标出来**:agent 那套「多 bot @ 时判断自己是不是主要收件人、拿不准
    ///   就静默」的规则,前提是分得清对面是人还是 bot;不标的话两者在 prompt 里完全同形,
    ///   防回音就没有依据。真人保持逐字节不变,不给正常对话加噪音。
    /// - `is_external=true` 表示发起人**没过 operate-gate**,本轮已被切成只读(见 `runtime::tool_guard`)。
    ///   标出来是给 agent 判断依据;**真正的边界是 PreToolUse 闸,这个标签不是安全机制**。
    ///   全权发起人不加任何标签 —— 常见路径逐字节不变,不给正常对话加噪音,也不扰动既有会话上下文。
    pub fn render_prompt(
        &self,
        who: &str,
        text: &str,
        chat: Option<&str>,
        addressed: bool,
        sender_is_bot: bool,
        is_external: bool,
    ) -> String {
        let mut who_label = if sender_is_bot {
            format!("bot「{who}」")
        } else {
            who.to_string()
        };
        if is_external {
            who_label.push_str(summon_i18n::text!("r162de42ba0"));
        }
        let (source, chat_name) = match chat {
            Some(chat_name) => {
                let src = if addressed {
                    summon_i18n::format!("r88d6ae4999")
                } else {
                    summon_i18n::format!("r0d00463b78")
                };
                (src, chat_name)
            }
            None => (summon_i18n::format!("r6a09a7eaee"), ""),
        };
        self.prompt_template
            .replace("{source}", &source)
            .replace("{who}", who)
            .replace("{text}", text)
            .replace("{chat}", chat_name)
    }

    /// 按退出原因取回执文案(messaging 配置驱动,不 hardcode)。
    pub fn session_exited_text(&self, reason: crate::session::exit::ExitReason) -> &str {
        use crate::session::exit::ExitReason;
        match reason {
            ExitReason::ManualKill => &self.session_exited.manual_kill,
            ExitReason::ManualKillReset => &self.session_exited.manual_kill_reset,
            ExitReason::PanelClean => &self.session_exited.panel_clean,
            ExitReason::PanelReset => &self.session_exited.panel_reset,
            ExitReason::IdleRetired => &self.session_exited.idle_retired,
            ExitReason::AgentArchived => &self.session_exited.agent_archived,
            ExitReason::GroupDisbanded => &self.session_exited.group_disbanded,
            ExitReason::BotRemoved => &self.session_exited.bot_removed,
            ExitReason::ShutdownInterrupt => &self.session_exited.shutdown_interrupt,
        }
    }

    /// 渲染退出回执。群解散 / bot 被移除必须带可辨认来源：优先使用盘上群名，拿不到时回退
    /// 稳定短 tag。旧的自定义文案若没有 `{chat}` 占位符，也自动在末尾补来源，避免升级后继续
    /// 出现“那个群”但用户无法判断是哪个群。
    pub fn render_session_exited_text(
        &self,
        reason: crate::session::exit::ExitReason,
        chat_name: Option<&str>,
        chat_id: &str,
    ) -> String {
        let template = self.session_exited_text(reason);
        if template.trim().is_empty() || !reason.group_gone() {
            return template.to_string();
        }
        let chat = exit_chat_label(chat_name, chat_id);
        if template.contains("{chat}") {
            template.replace("{chat}", &chat)
        } else {
            summon_i18n::format!("r5e5ef7ca80")
        }
    }

    pub fn headless_stall(&self) -> Duration {
        Duration::from_secs(self.headless_stall_secs.get())
    }

    pub fn idle_ttl(&self) -> Duration {
        Duration::from_secs(self.idle_ttl_secs)
    }
    /// 空闲自动退出阈值;`None` = 该机器没开这个功能。**0 也当没开** —— 配 0 的语义是
    /// 「立刻退出所有会话」,那多半是手误而不是本意,不替用户执行这种解释。
    pub fn idle_retire(&self) -> Option<Duration> {
        self.idle_retire_secs
            .filter(|s| *s > 0)
            .map(Duration::from_secs)
    }
    /// 定时任务尾轮的静默退出阈值;`None` = 关(配 0)。0 当关与 `idle_retire` 同款:
    /// 0 的字面语义是「定时轮一结束立刻退」,更可能是手误,不替用户执行这种解释。
    pub fn timer_idle_retire(&self) -> Option<Duration> {
        (self.timer_idle_retire_secs > 0).then(|| Duration::from_secs(self.timer_idle_retire_secs))
    }
    pub fn idle_scan(&self) -> Duration {
        Duration::from_secs(self.idle_scan_secs)
    }

    /// 跨后台任务续跑的时间上限。**0 = 关**(不续跑,首个 end_turn 即收尾,与 worker 行为一致)——
    /// 与 `timer_idle_retire_secs` 的 0=关 约定一致。非 0 时等通知期间转录静默是正常的,
    /// stall 看门狗让位给它,到点强制收尾。
    pub fn background_continuation_cap(&self) -> Duration {
        Duration::from_secs(self.background_continuation_max_secs)
    }

    /// 逐字段套用覆盖(None 字段保持原值)。
    pub fn apply(&mut self, o: &MessagingOverride) {
        if let Some(v) = &o.system_prompt {
            self.system_prompt = v.clone();
        }
        if let Some(v) = &o.reactions {
            self.reactions = v.clone();
        }
        if let Some(v) = &o.reaction_meanings {
            self.reaction_meanings = v.clone();
        }
        if let Some(v) = &o.default_reaction {
            self.default_reaction = v.clone();
        }
        if let Some(v) = &o.working_reaction {
            self.working_reaction = v.clone();
        }
        if let Some(v) = &o.done_reaction {
            self.done_reaction = v.clone();
        }
        if let Some(v) = &o.interrupted_reaction {
            self.interrupted_reaction = v.clone();
        }
        if let Some(v) = &o.prompt_template {
            self.prompt_template = v.clone();
        }
        if let Some(v) = o.headless_stall_secs {
            self.headless_stall_secs = v;
        }
        if let Some(v) = o.idle_ttl_secs {
            self.idle_ttl_secs = v;
        }
        // 覆盖只在**显式写了**这行时生效(内层 Option 也是 Some);写了才等于 owner 在这台机器上
        // 显式开启自动退出。没写 → 保持内置的「关」。
        if let Some(v) = o.idle_retire_secs {
            self.idle_retire_secs = v;
        }
        if let Some(v) = o.timer_idle_retire_secs {
            self.timer_idle_retire_secs = v;
        }
        if let Some(v) = o.timer_idle_retire_reset {
            self.timer_idle_retire_reset = v;
        }
        if let Some(v) = o.idle_scan_secs {
            self.idle_scan_secs = v;
        }
        if let Some(v) = o.background_continuation_max_secs {
            self.background_continuation_max_secs = v;
        }
        if let Some(v) = o.steer_transcript {
            self.steer_transcript = v;
        }
        if let Some(v) = o.steer_rpc {
            self.steer_rpc = v;
        }
        if let Some(v) = o.steer_cli_session {
            self.steer_cli_session = v;
        }
        if let Some(v) = o.session_mirror {
            self.session_mirror = v;
        }
        if let Some(v) = o.claude_permission_hook {
            self.claude_permission_hook = v;
        }
        if let Some(v) = o.mask_email {
            self.mask_email = v;
        }
        if let Some(v) = o.completion_summary_card {
            self.completion_summary_card = v;
        }
        if let Some(v) = o.progress_max_steps {
            self.progress_max_steps = v;
        }
        if let Some(v) = o.hide_thinking {
            self.hide_thinking = v;
        }
        if let Some(v) = o.cot_visible {
            self.cot_visible = v;
        }
        if let Some(v) = o.cot_tool_details {
            self.cot_tool_details = v;
        }
        if let Some(v) = o.badge_effort {
            self.badge_effort = v;
        }
        if let Some(v) = o.tool_trail {
            self.tool_trail = v;
        }
        if let Some(v) = o.agent_reactions {
            self.agent_reactions = v;
        }
        if let Some(v) = o.agent_feed {
            self.agent_feed = v;
        }
        if let Some(v) = o.document_comments_enabled {
            self.document_comments_enabled = v;
        }
        if let Some(v) = o.document_comments_non_owner_enabled {
            self.document_comments_non_owner_enabled = v;
        }
        if let Some(v) = o.document_comments_owner_mentions_enabled {
            self.document_comments_owner_mentions_enabled = v;
        }
        if let Some(v) = o.document_comment_timeout_secs {
            self.document_comment_timeout_secs = v;
        }
        if let Some(v) = &o.models {
            self.models = v.clone();
        }
        if let Some(v) = &o.models_by_cli {
            // per-key merge(**不整表替换**):用户只需写要改的 CLI 桶,没提的桶继承内置默认。
            // why:整表替换会让「用户在 config.yaml 配了 models_by_cli」= 锁死自己那一版——summon
            // 升级新增 CLI 桶 / 换 model id 时,用户拿不到(面板按钮点了起不来)。这与
            // system_prompt_core「随二进制升级、覆盖不掉」的取舍同源:合法值集由二进制定,不该被用户旧配置压死。
            for (cli, list) in v {
                self.models_by_cli.insert(cli.clone(), list.clone());
            }
        }
        if let Some(v) = o.codex_platform_tools {
            self.codex_platform_tools = v;
        }
        if let Some(v) = &o.default_model_by_cli {
            // 与 models_by_cli 同一套 per-key merge 理由:只钉自己在乎的那个 CLI,其余跟随升级。
            for (cli, model) in v {
                self.default_model_by_cli.insert(cli.clone(), model.clone());
            }
        }
        if let Some(v) = &o.efforts {
            self.efforts = v.clone();
        }
        if let Some(v) = &o.session_exited {
            v.apply(&mut self.session_exited);
        }
        // 逐**段**替换(不是逐项 merge):owner 要的是「用户配置可以移除出厂项」,
        // 逐项 merge 会让出厂项永远删不掉。没写的段保持出厂值。
        if let Some(v) = &o.slash_blacklist {
            v.apply(&mut self.slash_blacklist);
        }
    }

    /// 某 CLI 的模型候选:优先按 cli kind 查 `models_by_cli`,命中即用,否则回退扁平 `models`。
    pub fn models_for_cli(&self, cli: &str) -> &[String] {
        self.models_by_cli
            .get(cli)
            .map(Vec::as_slice)
            .unwrap_or(&self.models)
    }
}

fn exit_chat_label(chat_name: Option<&str>, chat_id: &str) -> String {
    let name = chat_name
        .map(str::trim)
        .filter(|name| !name.is_empty() && *name != summon_i18n::source!("r35b49ee58a"));
    match name {
        Some(name) => summon_i18n::format!("rc25b6cc4cb", crate::feishu::escape_lark_md(name)),
        None => {
            let tag: String = crate::registry::chat_tag("group", chat_id)
                .chars()
                .take(9)
                .collect();
            summon_i18n::format!("re419e853c8")
        }
    }
}

/// 运营「常驻要求」文件(CLAUDE.md / 软链 AGENTS.md)里允许写 `<!-- ... -->` 注释给人看,
/// 注入 system_prompt 前剥掉。未闭合的注释从 `<!--` 起把剩余整段丢弃。纯函数,便于单测。
pub fn strip_md_comments(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(start) = rest.find("<!--") {
        out.push_str(&rest[..start]);
        match rest[start..].find("-->") {
            Some(end) => rest = &rest[start + end + 3..],
            None => return out,
        }
    }
    out.push_str(rest);
    out
}

/// 把若干「常驻要求」层 `(标题, 正文)` 叠到基础 system_prompt(PRIMER)末尾:逐层先剥注释
/// 再 trim,空层跳过,非空层以 `# 标题` 分节追加。base 即「summon 提供的开发者意图」,layers
/// 为运营要求(全局 ~/.summon/CLAUDE.md + per-agent <dir>/CLAUDE.md)。纯函数,便于单测。
pub fn compose_system_prompt(base: &str, layers: &[(&str, &str)]) -> String {
    let mut out = base.trim_end().to_string();
    for (title, body) in layers {
        let body = strip_md_comments(body);
        let body = body.trim();
        if body.is_empty() {
            continue;
        }
        out.push_str("\n\n# ");
        out.push_str(title);
        out.push('\n');
        out.push_str(body);
    }
    out
}

/// 把多行文本压成单行(空白游程并成一个空格、去首尾空白)。注入 system_prompt 前用:
/// CLI 的 TUI 输入框里裸换行会被逐行提交 / 打断(Claude 多行 + 末尾 Enter 提交不掉,会把
/// PRIMER 残留在输入框、和下一条消息拼一起),故发送前先压平成一行(YAML 里仍可多行写、好读)。
/// 纯函数,便于单测。
pub fn to_single_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// 消息层覆盖(全局 config.yaml 或某 agent.yaml 的 `messaging:` 段,全字段可选)。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct MessagingOverride {
    pub system_prompt: Option<String>,
    pub reactions: Option<Vec<String>>,
    pub reaction_meanings: Option<HashMap<String, String>>,
    pub default_reaction: Option<String>,
    pub working_reaction: Option<String>,
    pub done_reaction: Option<String>,
    pub interrupted_reaction: Option<String>,
    pub prompt_template: Option<String>,
    pub idle_ttl_secs: Option<u64>,
    pub headless_stall_secs: Option<NonZeroU64>,
    /// 双层 Option:外层「这台机器写没写这行」,内层「写的是数还是显式 null(=关)」。
    /// 少一层就没法表达「显式关掉」,只能靠 0 这种魔数,而 0 在这里另有含义(见 `idle_retire`)。
    pub idle_retire_secs: Option<Option<u64>>,
    /// 单层 Option + **0 表示关**(不学 `idle_retire_secs` 的双层 Option):该字段默认**开**
    /// (3300),需要一个能落到「关」的显式写法;而 YAML 的 `null` 经 serde 双层 Option 实际
    /// 落 outer `None`(= 没写),表达不了「显式关」。`0` 走 `timer_idle_retire()` 的过滤,
    /// 语义干净且解析无歧义。
    pub timer_idle_retire_secs: Option<u64>,
    /// 定时会话空闲归档是否重置。`None` = 没写,跟内置默认(false)。
    pub timer_idle_retire_reset: Option<bool>,
    pub idle_scan_secs: Option<u64>,
    /// 跨后台任务续跑上限(秒)。`None` = 没写,保持内置默认(一天)。
    pub background_continuation_max_secs: Option<u64>,
    pub steer_transcript: Option<bool>,
    pub steer_rpc: Option<bool>,
    pub steer_cli_session: Option<bool>,
    pub session_mirror: Option<bool>,
    pub claude_permission_hook: Option<bool>,
    pub mask_email: Option<bool>,
    pub completion_summary_card: Option<bool>,
    pub progress_max_steps: Option<usize>,
    pub hide_thinking: Option<bool>,
    pub cot_visible: Option<bool>,
    pub cot_tool_details: Option<bool>,
    /// 角标思考档位显示开关。`None` = 没写,跟上一层。
    pub badge_effort: Option<bool>,
    pub tool_trail: Option<bool>,
    /// 表情约定段注入开关。`None` = 没写,跟上一层。
    pub agent_reactions: Option<bool>,
    /// feed 总结段注入开关。`None` = 没写,跟上一层。
    pub agent_feed: Option<bool>,
    pub document_comments_enabled: Option<bool>,
    pub document_comments_non_owner_enabled: Option<bool>,
    pub document_comments_owner_mentions_enabled: Option<bool>,
    pub document_comment_timeout_secs: Option<NonZeroU64>,
    pub models: Option<Vec<String>>,
    pub models_by_cli: Option<HashMap<String, Vec<String>>>,
    pub codex_platform_tools: Option<bool>,
    pub default_model_by_cli: Option<HashMap<String, String>>,
    pub efforts: Option<Vec<String>>,
    pub session_exited: Option<SessionExitedOverride>,
    /// slash 准入黑名单的逐**段**覆盖。段级 `Option` 是必需的:少了它,只想加一条 claude 规则
    /// 的人会把出厂的 `default` 段静默清空。
    pub slash_blacklist: Option<crate::driver::slash_pool::SlashBlacklistOverride>,
}

/// 「会话已退出」文案的逐字段覆盖(全字段可选)。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionExitedOverride {
    pub manual_kill: Option<String>,
    pub manual_kill_reset: Option<String>,
    pub idle_retired: Option<String>,
    pub agent_archived: Option<String>,
    pub panel_clean: Option<String>,
    pub panel_reset: Option<String>,
    pub group_disbanded: Option<String>,
    pub bot_removed: Option<String>,
    pub shutdown_interrupt: Option<String>,
}

impl SessionExitedOverride {
    /// 逐字段套到已 resolved 的文案上(None 字段保持原值)。
    fn apply(&self, t: &mut SessionExitedText) {
        if let Some(v) = &self.manual_kill {
            t.manual_kill = v.clone();
        }
        if let Some(v) = &self.manual_kill_reset {
            t.manual_kill_reset = v.clone();
        }
        if let Some(v) = &self.panel_clean {
            t.panel_clean = v.clone();
        }
        if let Some(v) = &self.panel_reset {
            t.panel_reset = v.clone();
        }
        if let Some(v) = &self.idle_retired {
            t.idle_retired = v.clone();
        }
        if let Some(v) = &self.agent_archived {
            t.agent_archived = v.clone();
        }
        if let Some(v) = &self.group_disbanded {
            t.group_disbanded = v.clone();
        }
        if let Some(v) = &self.bot_removed {
            t.bot_removed = v.clone();
        }
        if let Some(v) = &self.shutdown_interrupt {
            t.shutdown_interrupt = v.clone();
        }
    }
}

/// 配置字段的元信息:用于 doctor 检查时告知用户新增/废弃字段。
#[derive(Debug, Clone)]
pub struct ConfigFieldMeta {
    /// 点分隔的字段路径,如 "messaging.session_mirror"
    pub path: &'static str,
    /// 引入时的版本描述(如 "2026-06-05"),用于显示
    pub since: &'static str,
    /// 简短功能说明
    pub description: &'static str,
    /// 如果已废弃,填废弃说明;None 表示在用
    pub deprecated_since: Option<&'static str>,
}

/// 所有已知配置字段的元信息表。新增字段时在此追加一条。
///
/// 只列"值得让用户知道存在"的字段(功能开关/行为变更相关),不必列所有。
/// doctor 检查时读用户 config.yaml 的原始 Value,按 path 逐层查 key 是否存在。
pub const CONFIG_FIELD_META: &[ConfigFieldMeta] = &[
    ConfigFieldMeta {
        path: "language",
        since: "2026-11-08",
        description: "language.description",
        deprecated_since: None,
    },
    ConfigFieldMeta {
        path: "primary_agent",
        since: "2026-11-11",
        description: "r351a275214",
        deprecated_since: None,
    },
    ConfigFieldMeta {
        path: "codex",
        since: "2026-08-21",
        description: "rf2b14037f1",
        deprecated_since: None,
    },
    ConfigFieldMeta {
        path: "tools",
        since: "2026-08-10",
        description: "r47acf0c043",
        deprecated_since: None,
    },
    ConfigFieldMeta {
        path: "tmux_bin",
        since: "2026-08-07",
        description: "rcbf037687e",
        deprecated_since: Some("rb81027e668"),
    },
    ConfigFieldMeta {
        path: "messaging.working_reaction",
        since: "initial",
        description: "rd840cbd74c",
        deprecated_since: None,
    },
    ConfigFieldMeta {
        path: "messaging.done_reaction",
        since: "initial",
        description: "r479cadec35",
        deprecated_since: None,
    },
    ConfigFieldMeta {
        path: "messaging.interrupted_reaction",
        since: "2026-06-05",
        description: "r960d3c3e6f",
        deprecated_since: None,
    },
    ConfigFieldMeta {
        path: "preview_server.token_ttl_days",
        since: "2026-08-25",
        description: "rc74efb4bf6",
        deprecated_since: Some("r07a4465b6e"),
    },
    ConfigFieldMeta {
        path: "messaging.interrupt_enabled",
        since: "2026-06-05",
        description: "r3c83d77072",
        deprecated_since: Some("r454ef9b873"),
    },
    ConfigFieldMeta {
        path: "messaging.interrupt_transcript",
        since: "2026-06-05",
        description: "r3c83d77072",
        deprecated_since: Some("r454ef9b873"),
    },
    ConfigFieldMeta {
        path: "messaging.steer_transcript",
        since: "2026-08-04",
        description: "rc22517658f",
        deprecated_since: None,
    },
    ConfigFieldMeta {
        path: "messaging.steer_rpc",
        since: "2026-08-08",
        description: "rf70d9d032e",
        deprecated_since: None,
    },
    ConfigFieldMeta {
        path: "messaging.steer_cli_session",
        since: "2026-08-12",
        description: "r156250a40c",
        deprecated_since: None,
    },
    ConfigFieldMeta {
        path: "messaging.session_mirror",
        since: "2026-06-05",
        description: "r521411ac4d",
        deprecated_since: None,
    },
    ConfigFieldMeta {
        path: "messaging.claude_permission_hook",
        since: "2026-10-16",
        description: "r07dc13553a",
        deprecated_since: None,
    },
    ConfigFieldMeta {
        path: "messaging.mask_email",
        since: "2026-10-16",
        description: "rd608724ade",
        deprecated_since: None,
    },
    ConfigFieldMeta {
        path: "messaging.completion_summary_card",
        since: "2026-06-05",
        description: "radebdfe293",
        deprecated_since: None,
    },
    ConfigFieldMeta {
        path: "messaging.progress_max_steps",
        since: "2026-08-13",
        description: "rb3bf52b3f4",
        deprecated_since: None,
    },
    ConfigFieldMeta {
        path: "messaging.models_by_cli",
        since: "2026-06-03",
        description: "rdd9c8e4ff9",
        deprecated_since: None,
    },
    ConfigFieldMeta {
        path: "messaging.efforts",
        since: "2026-06-03",
        description: "reb84dffe03",
        deprecated_since: None,
    },
    ConfigFieldMeta {
        path: "messaging.codex_platform_tools",
        since: "2026-09-05",
        description: "r3b172ea99a",
        deprecated_since: None,
    },
    ConfigFieldMeta {
        path: "messaging.default_model_by_cli",
        since: "2026-09-05",
        description: "r90e9fafc9e",
        deprecated_since: None,
    },
    ConfigFieldMeta {
        path: "messaging.slash_blacklist",
        since: "2026-08-14",
        description: "r8e4f337edd",
        deprecated_since: None,
    },
    ConfigFieldMeta {
        path: "messaging.badge_effort",
        since: "2026-09-01",
        description: "r45741001d9",
        deprecated_since: None,
    },
    ConfigFieldMeta {
        path: "messaging.agent_reactions",
        since: "2026-08-28",
        description: "rff6a81756c",
        deprecated_since: None,
    },
    ConfigFieldMeta {
        path: "messaging.document_comment_timeout_secs",
        since: "2026-10-22",
        description: "r274de273ca",
        deprecated_since: None,
    },
    ConfigFieldMeta {
        path: "messaging.document_comments_non_owner_enabled",
        since: "2026-11-13",
        description: "r1e5e19fd0a",
        deprecated_since: None,
    },
    ConfigFieldMeta {
        path: "messaging.document_comments_owner_mentions_enabled",
        since: "2026-11-12",
        description: "rd9493bfc14",
        deprecated_since: None,
    },
    ConfigFieldMeta {
        path: "messaging.document_comments_enabled",
        since: "2026-10-22",
        description: "r7a38b29bb8",
        deprecated_since: None,
    },
    ConfigFieldMeta {
        path: "messaging.agent_feed",
        since: "2026-10-15",
        description: "r3396d10fbc",
        deprecated_since: None,
    },
    ConfigFieldMeta {
        path: "messaging.reaction_meanings",
        since: "2026-07-06",
        description: "r6ccc5ae717",
        deprecated_since: None,
    },
    ConfigFieldMeta {
        path: "messaging.interrupt_prefix",
        since: "initial",
        description: "re6d3665a2b",
        deprecated_since: Some("r454ef9b873"),
    },
    ConfigFieldMeta {
        path: "claude.status_line_in_managed_sessions",
        since: "2026-09-01",
        description: "r55aece3019",
        deprecated_since: None,
    },
    ConfigFieldMeta {
        path: "cursor.quota",
        since: "2026-09-18",
        description: "r5c07b8b656",
        deprecated_since: None,
    },
    ConfigFieldMeta {
        path: "menubar.terminal",
        since: "2026-07-14",
        description: "r01c3fcabec",
        deprecated_since: None,
    },
    ConfigFieldMeta {
        path: "menubar.icon",
        since: "2026-10-15",
        description: "raa3fdbe213",
        deprecated_since: None,
    },
    ConfigFieldMeta {
        path: "sentinel.enabled",
        since: "2026-08-07",
        description: "rcedebd04f8",
        deprecated_since: None,
    },
    ConfigFieldMeta {
        path: "sentinel.dry_run",
        since: "2026-08-07",
        description: "r672e9c247c",
        deprecated_since: None,
    },
    ConfigFieldMeta {
        path: "sentinel.aliases",
        since: "2026-08-07",
        description: "r181b33dae5",
        deprecated_since: None,
    },
    ConfigFieldMeta {
        path: "sentinel.fast_interval_secs",
        since: "2026-08-08",
        description: "re230ee5c3b",
        deprecated_since: None,
    },
    ConfigFieldMeta {
        path: "sentinel.fast_top_k",
        since: "2026-08-08",
        description: "r80cd4ab5e8",
        deprecated_since: None,
    },
    ConfigFieldMeta {
        path: "sentinel.slow_batch_size",
        since: "2026-08-08",
        description: "rcc21547b6a",
        deprecated_since: None,
    },
    ConfigFieldMeta {
        path: "sentinel.lark_cli_path",
        since: "2026-08-08",
        description: "raf4ea3f4fb",
        deprecated_since: Some("r17e626316e"),
    },
    ConfigFieldMeta {
        path: "web_terminal.enabled",
        since: "2026-09-22",
        description: "r2181ec2e74",
        deprecated_since: None,
    },
    ConfigFieldMeta {
        path: "web_terminal.port",
        since: "2026-09-22",
        description: "r03a35e066c",
        deprecated_since: None,
    },
    ConfigFieldMeta {
        path: "dashboard.enabled",
        since: "2026-10-20",
        description: "r54b470bdff",
        deprecated_since: None,
    },
    ConfigFieldMeta {
        path: "dashboard.port",
        since: "2026-10-20",
        description: "r77009f14f2",
        deprecated_since: None,
    },
    ConfigFieldMeta {
        path: "dashboard.public_base_url",
        since: "2026-10-20",
        description: "raa6396845f",
        deprecated_since: None,
    },
    ConfigFieldMeta {
        path: "upgrade",
        since: "2026-10-29",
        description: "rd4b103881a",
        deprecated_since: None,
    },
    ConfigFieldMeta {
        path: "gateway",
        since: "2026-06-04",
        description: "rbb75b8b08c",
        deprecated_since: Some("2026-07-13"),
    },
];

/// 检查用户 config.yaml 中是否存在某个点分隔的嵌套路径。
///
/// 按 "." 分割 path,逐层查 Value 的 map。任一层不是 map 或 key 不存在 → false。
/// 纯函数,便于单测。
pub fn path_exists_in_value(value: &serde_yaml::Value, path: &str) -> bool {
    let parts: Vec<&str> = path.split('.').collect();
    let mut current = value;
    for part in parts {
        match current.as_mapping() {
            Some(map) => match map.get(serde_yaml::Value::String(part.to_string())) {
                Some(next) => current = next,
                None => return false,
            },
            None => return false,
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn document_non_owner_comments_default_off_and_agent_override_wins() {
        let mut messaging = builtin_messaging();
        assert!(!messaging.document_comments_non_owner_enabled);
        messaging
            .apply(&serde_yaml::from_str("document_comments_non_owner_enabled: true").unwrap());
        assert!(messaging.document_comments_non_owner_enabled);
        messaging
            .apply(&serde_yaml::from_str("document_comments_non_owner_enabled: false").unwrap());
        assert!(!messaging.document_comments_non_owner_enabled);
        messaging
            .apply(&serde_yaml::from_str("document_comments_non_owner_enabled: null").unwrap());
        assert!(!messaging.document_comments_non_owner_enabled);
    }

    #[test]
    fn document_owner_mentions_default_off_and_agent_override_wins() {
        let mut messaging = builtin_messaging();
        assert!(!messaging.document_comments_owner_mentions_enabled);
        messaging.apply(
            &serde_yaml::from_str("document_comments_owner_mentions_enabled: true").unwrap(),
        );
        assert!(messaging.document_comments_owner_mentions_enabled);
        messaging.apply(
            &serde_yaml::from_str("document_comments_owner_mentions_enabled: false").unwrap(),
        );
        assert!(!messaging.document_comments_owner_mentions_enabled);
        messaging.apply(
            &serde_yaml::from_str("document_comments_owner_mentions_enabled: null").unwrap(),
        );
        assert!(!messaging.document_comments_owner_mentions_enabled);
    }

    #[test]
    fn builtin_reactions_include_raise_hand_with_searchable_meaning() {
        let m = builtin_messaging();
        assert!(m.reactions.iter().any(|k| k == "MeMeMe"));
        assert!(m.reaction_meanings["MeMeMe"].contains("举手"));
    }

    #[test]
    fn config_parse_failure_is_loud_and_keeps_last_good() {
        // 合法 → 解析值 + 无错。
        let (ok, err) = parse_or_fallback("sentinel:\n  enabled: true\n", Config::default());
        assert!(ok.sentinel.enabled);
        assert!(err.is_none(), "合法配置不应报错");

        // 非法(序列喂给期望 map 的 Config)→ 必带错误信息(不静默)+ 沿用 fallback 值(不退默认)。
        let mut good = Config::default();
        good.sentinel.enabled = true;
        let (bad, err) = parse_or_fallback("- 不是一个 map\n- b\n", good);
        assert!(err.is_some(), "解析失败必须可观测,绝不能静默吞掉");
        assert!(
            bad.sentinel.enabled,
            "解析失败应沿用上次好配置,而非打回默认"
        );
    }

    #[test]
    fn cursor_quota_defaults_to_ten_minutes_of_cursor_agent_login() {
        let (cfg, err) = parse_or_fallback("poll_ms: 400\n", Config::default());
        assert!(err.is_none());
        assert!(cfg.cursor.quota.enabled);
        assert_eq!(cfg.cursor.quota.interval(), Duration::from_secs(600));
        assert_eq!(cfg.cursor.quota.token_command, None);

        let (cfg, err) = parse_or_fallback(
            "cursor:\n  quota:\n    interval_secs: 5\n    token_command: pass show cursor\n",
            Config::default(),
        );
        assert!(err.is_none());
        assert!(cfg.cursor.quota.enabled, "只写部分字段时其余取默认");
        assert_eq!(
            cfg.cursor.quota.interval(),
            Duration::from_secs(CursorQuota::MIN_INTERVAL_SECS),
            "过小的间隔被抬到下限,不让一个笔误变成每轮都打外部接口"
        );
        assert_eq!(
            cfg.cursor.quota.token_command.as_deref(),
            Some("pass show cursor")
        );
    }

    #[test]
    fn sentinel_config_defaults_off_and_partial_yaml_fills_defaults() {
        // 无 sentinel 节 → 全默认(关)。
        let (cfg, err) = parse_or_fallback("poll_ms: 400\n", Config::default());
        assert!(err.is_none());
        assert!(!cfg.sentinel.enabled, "sentinel 必须默认关");
        assert_eq!(cfg.sentinel.interval_secs, 60);
        assert_eq!(cfg.sentinel.fast_interval_secs, 5);
        assert_eq!(cfg.sentinel.fast_top_k, 20);
        assert_eq!(cfg.sentinel.slow_batch_size, 2);
        assert_eq!(cfg.sentinel.join_verbs, vec!["进来回答".to_string()]);
        assert_eq!(cfg.sentinel.create_verbs, vec!["建群".to_string()]);
        assert_eq!(cfg.sentinel.lark_cli_path, None);

        // 只写 enabled → 其余字段补默认。
        let (cfg, err) = parse_or_fallback("sentinel:\n  enabled: true\n", Config::default());
        assert!(err.is_none());
        assert!(cfg.sentinel.enabled);
        assert!(!cfg.sentinel.dry_run);
        assert_eq!(cfg.sentinel.top_k, 50);
        assert_eq!(cfg.sentinel.max_age_secs, 300);
        assert_eq!(cfg.sentinel.per_hour, 6);

        // DevBox 这类 npm prefix 安装不一定落在 daemon PATH 里，必须能显式指定可执行文件。
        let (cfg, err) = parse_or_fallback(
            "sentinel:\n  lark_cli_path: /home/me/.npm-global/bin/lark-cli\n",
            Config::default(),
        );
        assert!(err.is_none());
        assert_eq!(
            cfg.sentinel.lark_cli_path.as_deref(),
            Some("/home/me/.npm-global/bin/lark-cli")
        );

        // 自定义称呼映射与多动作词热配置。
        let (cfg, err) = parse_or_fallback(
            "sentinel:\n  join_verbs: [进来回答, 进来说明, 进来]\n  aliases:\n    arcadia: [阿卡迪亚, Arcadia]\n",
            Config::default(),
        );
        assert!(err.is_none());
        assert_eq!(cfg.sentinel.join_verbs.len(), 3);
        assert_eq!(cfg.sentinel.aliases.get("arcadia").unwrap().len(), 2);
    }

    #[test]
    fn tools_table_parses_without_breaking_legacy_aliases() {
        let (cfg, err) = parse_or_fallback(
            r#"
tools:
  tmux: /new/tmux
  lark_cli: ~/.local/bin/lark-cli
  claude: /new/claude
  codex: /new/codex
  cursor_agent: /new/cursor-agent
  git: /usr/bin/git
tmux_bin: /legacy/tmux
sentinel:
  lark_cli_path: /legacy/lark-cli
"#,
            Config::default(),
        );
        assert!(err.is_none());
        assert_eq!(cfg.tools.tmux, Some(PathBuf::from("/new/tmux")));
        assert_eq!(
            cfg.tools.lark_cli,
            Some(PathBuf::from("~/.local/bin/lark-cli"))
        );
        assert_eq!(cfg.tmux_bin, Some(PathBuf::from("/legacy/tmux")));
        assert_eq!(
            cfg.sentinel.lark_cli_path.as_deref(),
            Some("/legacy/lark-cli")
        );
    }

    #[test]
    fn platform_tools_ship_disabled_and_stay_overridable() {
        let mut m = builtin_messaging();
        assert!(
            !m.codex_platform_tools,
            "出厂必须是关的 —— 这三个工具里两个直接往群里发东西,「要不要默认开」是 owner 的灰度决定"
        );

        m.apply(&MessagingOverride {
            codex_platform_tools: Some(true),
            ..Default::default()
        });
        assert!(
            m.codex_platform_tools,
            "配置里写了就得生效,否则开关等于不存在"
        );

        // 后一层没提这个键时,前一层的值要留着(全局开了、agent.yaml 没提 → 仍然开)。
        m.apply(&MessagingOverride::default());
        assert!(m.codex_platform_tools);
    }

    #[test]
    fn pinned_default_model_override_merges_per_key_and_ships_empty() {
        let mut m = builtin_messaging();
        assert!(
            m.default_model_by_cli.is_empty(),
            "出厂不钉任何默认模型 —— 钉了就等于把上游换代拦在外面,那是用户自己的决定"
        );

        let mut first = HashMap::new();
        first.insert("codex".to_string(), "gpt-5.6-sol".to_string());
        m.apply(&MessagingOverride {
            default_model_by_cli: Some(first),
            ..Default::default()
        });
        let mut second = HashMap::new();
        second.insert("claude".to_string(), "claude-opus-5".to_string());
        m.apply(&MessagingOverride {
            default_model_by_cli: Some(second),
            ..Default::default()
        });

        assert_eq!(
            m.default_model_by_cli.get("codex").map(String::as_str),
            Some("gpt-5.6-sol"),
            "只钉 claude 的那一层不该把已钉的 codex 抹掉"
        );
        assert_eq!(
            m.default_model_by_cli.get("claude").map(String::as_str),
            Some("claude-opus-5")
        );
    }

    #[test]
    fn models_by_cli_override_merges_per_key_not_whole_map() {
        // 内置默认给了 claude/codex 两桶(见 messaging.default.yaml);cursor、traex 不带内置候选。
        let mut m = builtin_messaging();
        let builtin_codex = m.models_by_cli.get("codex").cloned();
        assert!(builtin_codex.is_some(), "内置默认应含 codex 桶(前提)");

        // 用户只覆盖 claude 桶。
        let mut override_map = HashMap::new();
        override_map.insert("claude".to_string(), vec!["my-claude-model".to_string()]);
        m.apply(&MessagingOverride {
            models_by_cli: Some(override_map),
            ..Default::default()
        });

        // claude 桶被替换成用户的。
        assert_eq!(
            m.models_by_cli.get("claude").unwrap(),
            &vec!["my-claude-model".to_string()]
        );
        // 关键:用户没提的 codex 桶仍在(继承内置默认,不被整表替换清空)。
        assert_eq!(
            m.models_by_cli.get("codex").cloned(),
            builtin_codex,
            "未覆盖的 codex 桶应继承内置默认,而非被整表替换抹掉"
        );
    }

    #[test]
    fn both_steer_paths_default_on_and_can_each_be_killed() {
        // 轮内 steer 两条路都**默认开**:连发的消息不再干等前一轮。1:1 模型因此变成 N:1,
        // 而「哪段属于哪条消息」由过程展示的语义边界表达(卡片切卡 / CoT 段边界)。
        let m = builtin_messaging();
        assert!(m.steer_transcript, "claude 轮内 steer 默认开");
        assert!(m.steer_rpc, "codex 轮内 steer 默认开");

        // 两个 kill-switch 必须**各自**独立:两条路的注入机制毫无共同点,出问题要能只关一条。
        let mut only_transcript_off = builtin_messaging();
        only_transcript_off.apply(&MessagingOverride {
            steer_transcript: Some(false),
            ..Default::default()
        });
        assert!(!only_transcript_off.steer_transcript);
        assert!(only_transcript_off.steer_rpc, "关一条不该带倒另一条");

        let mut only_rpc_off = builtin_messaging();
        only_rpc_off.apply(&MessagingOverride {
            steer_rpc: Some(false),
            ..Default::default()
        });
        assert!(only_rpc_off.steer_transcript);
        assert!(!only_rpc_off.steer_rpc);
    }

    #[test]
    fn cursor_cli_session_steer_defaults_on_and_can_be_disabled() {
        let m = builtin_messaging();
        assert!(
            m.steer_cli_session,
            "Cursor Headless CLI Session 必须默认接收轮内追加消息"
        );
        let mut m2 = builtin_messaging();
        m2.apply(&MessagingOverride {
            steer_cli_session: Some(false),
            ..Default::default()
        });
        assert!(!m2.steer_cli_session, "必须保留逐 agent kill-switch");
        // cursor 那条是 SIGINT + resume 重喂,codex 那条是 `turn/steer` 帧 —— 关一条绝不能
        // 带倒另一条。
        assert!(m2.steer_rpc, "关 cursor 的开关不该动 Codex/ACP 那个");
    }

    #[test]
    fn session_mirror_defaults_true_and_overrides_off() {
        // 会话镜像默认**开**:claude 起常驻 tailer 把终端侧 turn 也推飞书。可显式覆盖关掉。
        let m = builtin_messaging();
        assert!(m.session_mirror, "会话镜像内置默认应为开");
        let mut m2 = builtin_messaging();
        m2.apply(&MessagingOverride {
            session_mirror: Some(false),
            ..Default::default()
        });
        assert!(!m2.session_mirror, "覆盖应能关掉会话镜像");
    }

    #[test]
    fn mask_email_defaults_on_and_overrides_off() {
        let m = builtin_messaging();
        assert!(m.mask_email, "邮箱打码内置默认应为开");
        let mut m2 = builtin_messaging();
        m2.apply(&MessagingOverride {
            mask_email: Some(false),
            ..Default::default()
        });
        assert!(!m2.mask_email, "覆盖应能关掉邮箱打码");
    }

    #[test]
    fn claude_permission_hook_defaults_true_and_overrides_off() {
        let m = builtin_messaging();
        assert!(m.claude_permission_hook, "授权 hook 内置默认应为开");
        let mut m2 = builtin_messaging();
        m2.apply(&MessagingOverride {
            claude_permission_hook: Some(false),
            ..Default::default()
        });
        assert!(!m2.claude_permission_hook, "覆盖应能关掉授权 hook");
    }

    #[test]
    fn strip_md_comments_removes_human_notes() {
        assert_eq!(strip_md_comments("a<!--x-->b"), "ab");
        // 多行注释
        assert_eq!(strip_md_comments("a<!--\n多行\n-->b"), "ab");
        // 多个注释
        assert_eq!(strip_md_comments("<!--1-->x<!--2-->y"), "xy");
        // 未闭合:从 <!-- 起丢弃剩余
        assert_eq!(strip_md_comments("keep<!--dangling"), "keep");
        // 无注释原样
        assert_eq!(strip_md_comments("plain"), "plain");
    }

    #[test]
    fn compose_system_prompt_layers_nonempty_only() {
        let base = "PRIMER 正文\n";
        // 全局有内容、per-agent 只有注释(应被剥成空 → 跳过)
        let out = compose_system_prompt(
            base,
            &[
                ("常驻要求(全局)", "  全局要求一行  "),
                ("常驻要求(本 agent)", "<!-- 还没写 -->"),
            ],
        );
        assert_eq!(out, "PRIMER 正文\n\n# 常驻要求(全局)\n全局要求一行");
        // 两层皆空 → 只剩 base(且 trim_end)
        assert_eq!(
            compose_system_prompt("base\n\n", &[("a", ""), ("b", "  ")]),
            "base"
        );
        // 两层都有 → 依次分节
        let both = compose_system_prompt("B", &[("G", "g 内容"), ("A", "a 内容")]);
        assert_eq!(both, "B\n\n# G\ng 内容\n\n# A\na 内容");
    }

    #[test]
    fn to_single_line_collapses_whitespace_and_dash_lines() {
        // 多行 + 空行 → 单行,空白游程并成一个空格
        assert_eq!(to_single_line("a\nb\n\nc"), "a b c");
        // 行首 `-` 列表压平后变成行内,不再处于行首(配合 send_text 的 -- 双保险)
        assert_eq!(to_single_line("  - x\n  - y  "), "- x - y");
        // 单行原样(仅 trim)
        assert_eq!(to_single_line("  plain  "), "plain");
    }

    #[test]
    fn builtin_system_prompt_single_line_has_no_leading_dash_after_collapse() {
        // 回归:多行 PRIMER(合成 base+app-core 后)压平应是一行、不以 `-` 起头(否则 tmux send-keys 当 flag)
        let one = to_single_line(&builtin_messaging().system_prompt_filled(
            "/tmp/emojis.md",
            false,
            PrimerSections::ALL,
        ));
        assert!(!one.contains('\n'));
        assert!(!one.starts_with('-'));
    }

    #[test]
    fn headless_prompt_allows_native_question_bridge_without_treating_skip_as_human_answer() {
        let prompt =
            builtin_messaging().system_prompt_filled("/tmp/emojis.md", true, PrimerSections::ALL);
        assert!(prompt.contains("原生提问工具"));
        assert!(prompt.contains("桥接成同类飞书卡片"));
        assert!(prompt.contains("不是**对方真的跳过"));
        assert!(prompt.contains("summon ask"), "无原生桥的通道仍需后备出口");
    }

    #[test]
    fn defaults_are_sane() {
        let c = Config::default();
        assert_eq!(c.poll_ms, 400);
        assert_eq!(c.debounce_ms, 1200);
        assert_eq!(c.default_domain, "feishu.cn");
        assert!(c.agents_dir.ends_with(".summon/agents"));
        assert!(c.sidecar_bin.is_none());
        assert!(c.resume_sessions); // 默认开启 session resume
        assert!(c.messaging.working_reaction.is_none()); // 全局覆盖默认空
    }

    #[test]
    fn menubar_terminal_defaults_empty_and_is_configurable() {
        // 没配 → 空模板(渲染时回落系统 Terminal.app),不强加任何第三方终端。
        assert!(Config::default().menubar.terminal.is_empty());

        let configured: Config = serde_yaml::from_str(
            "menubar:\n  terminal: [\"cmux\", \"new-workspace\", \"--command\", \"{cmd}\"]\n",
        )
        .unwrap();
        assert_eq!(
            configured.menubar.terminal,
            vec!["cmux", "new-workspace", "--command", "{cmd}"]
        );
    }

    /// 判据是排除法:每一条被排掉的都是「填进链接也点不开」的地址。10/8 优先,因为企业内网
    /// 与 VPN 都在那儿——那是唯一「换个网络挂上 VPN 还能到」的一档。
    #[test]
    fn pick_public_host_prefers_enterprise_over_public_and_rejects_unroutable() {
        use std::net::Ipv4Addr;
        let ip = |s: &str| s.parse::<Ipv4Addr>().unwrap();

        // Mac 的真实三地址:回环与家用段都被排掉,只剩 VPN 那个。
        assert_eq!(
            pick_public_host(&[ip("127.0.0.1"), ip("192.168.1.2"), ip("10.0.0.3")]),
            Some(ip("10.0.0.3"))
        );
        // devbox:单个企业内网地址。
        assert_eq!(pick_public_host(&[ip("10.0.0.2")]), Some(ip("10.0.0.2")));
        // 10/8 优先于公网地址,即便公网地址排在前面。
        assert_eq!(
            pick_public_host(&[ip("1.2.3.4"), ip("10.0.0.9")]),
            Some(ip("10.0.0.9"))
        );
        // 没有 10/8 时公网地址仍然可用。
        assert_eq!(pick_public_host(&[ip("1.2.3.4")]), Some(ip("1.2.3.4")));
        // 多个 10/8 取第一个,顺序稳定。
        assert_eq!(
            pick_public_host(&[ip("10.1.1.1"), ip("10.2.2.2")]),
            Some(ip("10.1.1.1"))
        );
        // 全是点不开的:回环、链路本地、家用段、容器段、代理占用的 benchmark 段。
        for addrs in [
            vec![ip("127.0.0.1")],
            vec![ip("169.254.1.1")],
            vec![ip("192.168.1.5")],
            vec![ip("172.17.0.1")],
            vec![ip("198.18.0.1"), ip("198.19.0.1")],
            vec![],
        ] {
            assert_eq!(pick_public_host(&addrs), None, "不该选中:{addrs:?}");
        }
    }

    #[test]
    fn preview_links_expire_after_an_hour_unless_configured() {
        assert_eq!(PreviewServer::default().token_ttl_secs, 3600);
        let cfg: Config = serde_yaml::from_str("preview_server:\n  token_ttl_secs: 600\n").unwrap();
        assert_eq!(cfg.preview_server.token_ttl_secs, 600);
    }

    /// 查不到地址就不给 base_url —— 宁可不 mint,也不给一个注定点不开的链接。
    #[test]
    fn base_url_falls_back_to_nothing_rather_than_localhost() {
        let cfg = PreviewServer::default();
        assert_eq!(cfg.base_url_with_host(None), None);
        assert_eq!(
            cfg.base_url_with_host(Some("10.0.0.9".parse().unwrap())),
            Some("http://10.0.0.9:7765".to_string())
        );
        // 显式配了对外地址就照用,不再探测。
        let pinned = PreviewServer {
            public_base_url: "http://example.internal:8080/".to_string(),
            ..PreviewServer::default()
        };
        assert_eq!(
            pinned.base_url_with_host(None),
            Some("http://example.internal:8080".to_string())
        );
    }

    #[test]
    fn resume_sessions_defaults_true_and_is_overridable() {
        // 缺省 → true(serde default 走 Config::default)
        let c: Config = serde_yaml::from_str("poll_ms: 250").unwrap();
        assert!(c.resume_sessions);
        // 显式关闭
        let off: Config = serde_yaml::from_str("resume_sessions: false").unwrap();
        assert!(!off.resume_sessions);
    }

    #[test]
    fn partial_yaml_merges_with_defaults() {
        let c: Config = serde_yaml::from_str("poll_ms: 250").unwrap();
        assert_eq!(c.poll_ms, 250);
        assert_eq!(c.debounce_ms, 1200); // 未指定 → 默认
    }

    #[test]
    fn feed_section_asks_for_the_feed_summary_tag() {
        let feed = builtin_messaging().system_prompt_feed;
        assert!(feed.contains(crate::feed_tag::FEED_TAG_OPEN));
        assert!(feed.contains(crate::feed_tag::FEED_TAG_CLOSE));
        assert!(feed.contains(&format!("{} 字以内", crate::feed_tag::FEED_TAG_MAX_CHARS)));
    }

    /// feed 开关只管 feed 段:关掉后标签约定整段不进 PRIMER、不留占位,表情段与其余段一字不动。
    #[test]
    fn feed_protocol_is_absent_when_the_switch_is_off() {
        let m = builtin_messaging();
        let on = m.system_prompt_filled("/tmp/e.md", false, PrimerSections::ALL);
        let off = m.system_prompt_filled(
            "/tmp/e.md",
            false,
            PrimerSections {
                feed: false,
                ..PrimerSections::ALL
            },
        );

        assert!(
            on.contains(crate::feed_tag::FEED_TAG_OPEN),
            "开着时该段必须在"
        );
        assert!(!on.contains("{feed_protocol}"));

        assert!(!off.contains(crate::feed_tag::FEED_TAG_OPEN));
        assert!(!off.contains("{feed_protocol}"));
        assert!(!off.contains("\n\n\n"), "接缝处不得出现连续空行");
        assert!(off.contains("给消息贴语义表情"), "表情段不随 feed 开关变");
        assert!(off.contains("以上是你的常驻约定"), "收尾句必须还在");
        assert_eq!(
            off.len() + m.system_prompt_feed.trim_end().len() + "\n\n".len(),
            on.len(),
            "关掉只少了 feed 段和它的一处段间空行"
        );
    }

    #[test]
    fn builtin_messaging_parses_and_is_complete() {
        let m = builtin_messaging(); // 不 panic = YAML 字段齐全
        assert!(m.system_prompt.is_empty()); // base 默认空,运行时协议都在 core
        assert!(!m.system_prompt_core.contains("{reactions}"));
        assert!(!m.system_prompt_core.contains("星光")); // 已删
        assert!(m.system_prompt_core.contains('\n')); // 多行块标量(可读),非单行
        assert!(m.system_prompt_core.contains("open_id")); // 声明:回复用名字、别回显 open_id
        assert!(m.system_prompt_core.contains("递归")); // 声明:谨慎 @ bot,防无穷递归
                                                        // 2026-08-13 修:旧礼仪把「任务回交」错划进「回音」(「汇报结果不要 @」),GPT 系照
                                                        // 字面执行,审完活不 @ 派活方,而 bot 收不到没 @ 它的消息 —— 交付永久丢失。新协议
                                                        // 按「谁需要这条消息才能继续」判 @,机制事实与回交义务必须钉在文本里。
        assert!(m.system_prompt_core.contains("唯一送达通道")); // 机制事实:@ 是 bot 间唯一送达通道
        assert!(m.system_prompt_core.contains("你回交给它的产出")); // 回交属于「它需要这条消息」,要 @
        assert!(
            m.reactions.len() > 50,
            "表情候选应覆盖飞书官方键集,数量={}",
            m.reactions.len()
        );
        // 官方键区分大小写，使用 ASCII 字母、数字或下划线（Status_PrivateMessage）。
        // 候选保持非空且唯一；BUG 不在官方键表中，API 返回 231001。
        assert!(m
            .reactions
            .iter()
            .all(|r| !r.is_empty() && r.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')));
        let unique: std::collections::HashSet<_> = m.reactions.iter().collect();
        assert_eq!(unique.len(), m.reactions.len(), "reactions 无重复");
        assert!(
            !m.reactions.iter().any(|r| r == "BUG"),
            "BUG 不是合法飞书反应键(231001),不得回流"
        );
        assert!(
            m.reactions.iter().any(|r| r == "Alarm") && m.reactions.iter().any(|r| r == "OnIt"),
            "2026-08-05 新增的官方键应在集合内"
        );
        assert_eq!(m.working_reaction, "OnIt");
        assert_eq!(m.done_reaction, "DONE");
        assert!(
            !m.system_prompt_core.contains("OnIt") && !m.system_prompt_core.contains("DONE"),
            "工程自动维护生命周期表情，agent-facing prompt 不应再介绍对应表情键"
        );
        assert_eq!(m.idle_ttl_secs, 86400);
    }

    #[test]
    fn builtin_system_prompt_present_and_natural_reply() {
        // app-core(内置协议)非空且含自然作答指引;base 默认空,内容都在 core。
        let m = builtin_messaging();
        assert!(!m.system_prompt_core.trim().is_empty());
        assert!(m.system_prompt_core.contains("自然作答"));
        assert!(m.system_prompt_core.contains("飞书"));
    }

    #[test]
    fn builtin_system_prompt_teaches_lazy_current_chat_history_lookup() {
        let core = builtin_messaging().system_prompt_core;

        // **chat_id 必须走凭证化通道**:`$SUMMON_CHAT_ID` 是进程 spawn 时钉死的,会话被 rekey
        // 迁移后指向上一个群 —— 照它读历史会读到别人的会话。协议里必须教 `summon chat-id`。
        assert!(
            core.contains("summon chat-id --turn-id"),
            "读历史前必须先用凭证取当前 chat_id"
        );
        assert!(
            !core.contains("--chat-id \"$SUMMON_CHAT_ID\""),
            "不得再教 agent 直接用环境变量里的 chat_id"
        );
        // 具体怎么读(命令、翻页、话题)已经搬进 `summon guide protocol`,常驻模板只留
        // 「chat_id 从哪来」这条禁令和一句索引 —— 判据跟着搬家,由
        // `guide::tests::guide_protocol_keeps_prohibitions_resident_and_details_out_of_line`
        // 两头对照着守(细节在 guide 里、且不许留在常驻)。
        assert!(
            core.contains("summon guide protocol"),
            "常驻模板必须留下取用细节的索引"
        );
    }

    #[test]
    fn system_prompt_filled_substitutes_emoji_placeholder() {
        // 占位 {emoji_file} 替换成路径后不再残留;填充只给路径、不展开表情全集(agent 自己读文件)。
        let m = builtin_messaging();
        let filled = m.system_prompt_filled("/tmp/summon-emojis.md", false, PrimerSections::ALL);
        assert!(!filled.contains("{emoji_file}"));
        assert!(!filled.contains(&m.reactions.join(" ")));
    }

    /// 开关关掉后表情约定**整段**不进 PRIMER,且不留占位残骸。
    ///
    /// 这条测试守的是本功能的**唯一效果**:开关的布尔值怎么合并有一整套测试,
    /// 但「布尔值 → PRIMER 文本」那一跳如果没人验,把 `inject_reactions` 整个忽略掉
    /// 也不会有测试变红 —— 交付的东西被废掉而门禁全绿。
    #[test]
    fn reactions_protocol_is_absent_when_the_switch_is_off() {
        let m = builtin_messaging();
        let on = m.system_prompt_filled("/tmp/summon-emojis.md", false, PrimerSections::ALL);
        let off = m.system_prompt_filled(
            "/tmp/summon-emojis.md",
            false,
            PrimerSections {
                reactions: false,
                ..PrimerSections::ALL
            },
        );

        assert!(on.contains("给消息贴语义表情"), "开着时该段必须在");
        assert!(
            on.contains("/tmp/summon-emojis.md"),
            "开着时 emoji 路径必须填进去"
        );

        assert!(!off.contains("给消息贴语义表情"));
        assert!(!off.contains("summon react"));
        assert!(!off.contains("THUMBSUP"), "速查表也不能剩");
        // `{emoji_file}` 现在只存在于被抽出的那段里 —— 关掉后它连同段落一起消失,
        // 而不是留下一个没被替换的占位。
        assert!(!off.contains("{emoji_file}"));
        assert!(!off.contains("/tmp/summon-emojis.md"));
        assert!(!off.contains("{reactions_protocol}"));
        assert!(off.len() < on.len());

        // 关段落不能破坏 PRIMER 的段落结构:接缝处只留一个空行,别的段一字不动。
        assert!(!off.contains("\n\n\n"), "接缝处不得出现连续空行");
        assert!(off.contains("以上是你的常驻约定"), "收尾句必须还在");
        assert!(off.contains("自然作答"), "其余段落不受影响");
    }

    /// 「直接结束本轮、summon 会等后台任务」只告诉真会续跑的会话:捕获形态不续跑、或续跑上限
    /// 写 0 时说了就是错的 —— agent 照做收尾,后台任务的结果就丢在了本轮之外。
    #[test]
    fn background_protocol_is_present_only_when_the_session_continues() {
        let m = builtin_messaging();
        let on = m.system_prompt_filled("/tmp/e.md", false, PrimerSections::ALL);
        let not_continuing = m.system_prompt_filled(
            "/tmp/e.md",
            false,
            PrimerSections {
                background_continuation: false,
                ..PrimerSections::ALL
            },
        );
        let mut capped_off = builtin_messaging();
        capped_off.background_continuation_max_secs = 0;
        let capped_off = capped_off.system_prompt_filled("/tmp/e.md", false, PrimerSections::ALL);

        assert!(
            on.contains(::summon_core::i18n::label(BACKGROUND_PROTOCOL)),
            "续跑的会话必须带这一段"
        );
        for off in [&not_continuing, &capped_off] {
            assert!(!off.contains("后台子 agent"));
            assert!(!off.contains("{background_protocol}"));
            assert!(!off.contains("\n\n\n"), "接缝处不得出现连续空行");
            assert!(off.contains("要问对方时"), "相邻段不受影响");
        }
    }

    /// 占位替换顺序:段落先填进去,`{emoji_file}` 才轮得到被替换。反过来的话段内那个占位
    /// 会原样留在 PRIMER 里,而这个漏填只在「段被注入」时才显形。
    #[test]
    fn emoji_placeholder_inside_the_reactions_section_is_still_substituted() {
        let m = builtin_messaging();
        assert!(
            m.system_prompt_reactions.contains("{emoji_file}"),
            "占位应当在被抽出的那段里(它换位置了,这条测试就该跟着改)"
        );
        assert!(!m.system_prompt_core.contains("{emoji_file}"));
        let filled = m.system_prompt_filled("/tmp/e.md", false, PrimerSections::ALL);
        assert!(filled.contains("/tmp/e.md"));
        assert!(!filled.contains("{emoji_file}"));
    }

    #[test]
    fn system_prompt_override_merges_onto_builtin() {
        let o: MessagingOverride =
            serde_yaml::from_str("system_prompt: \"自定义 PRIMER\"").unwrap();
        let mut m = builtin_messaging();
        m.apply(&o);
        assert_eq!(m.system_prompt, "自定义 PRIMER");
        assert_eq!(m.working_reaction, "OnIt"); // 未覆盖 → builtin 不变
    }

    #[test]
    fn app_core_survives_base_override_and_upgrades_with_binary() {
        // 升级安全不变量(本轮新增):用户在 ~/.summon 把可覆盖的 base 整个改写,内置 app-core
        // 协议仍每次注入——它根本不在 MessagingOverride 里,随二进制升级、覆盖不掉。
        let mut m = builtin_messaging();
        m.apply(&MessagingOverride {
            system_prompt: Some("我的自定义开场白".into()),
            ..Default::default()
        });
        let filled = m.system_prompt_filled("/tmp/emojis.md", false, PrimerSections::ALL);
        assert!(filled.contains("我的自定义开场白"), "用户 base 应在");
        assert!(filled.contains("summon react"), "内置 react 协议覆盖后仍在");
        assert!(filled.contains("递归"), "内置 @ 防护覆盖后仍在");
        assert!(filled.contains("唯一的提问出口"), "内置提问出口覆盖后仍在");
        assert!(
            !filled.contains("{emoji_file}"),
            "core 里的 {{emoji_file}} 占位被填"
        );
    }

    #[test]
    fn session_exited_text_picks_by_reason() {
        use crate::session::exit::ExitReason;
        let m = builtin_messaging();
        // 每个原因都出一段非空且互不相同的文案(默认 yaml 齐全)。
        let texts = [
            m.session_exited_text(ExitReason::ManualKill),
            m.session_exited_text(ExitReason::ManualKillReset),
            m.session_exited_text(ExitReason::PanelClean),
            m.session_exited_text(ExitReason::PanelReset),
            m.session_exited_text(ExitReason::AgentArchived),
            m.session_exited_text(ExitReason::GroupDisbanded),
            m.session_exited_text(ExitReason::BotRemoved),
            m.session_exited_text(ExitReason::ShutdownInterrupt),
        ];
        assert!(texts.iter().all(|t| !t.trim().is_empty()));
        // 停机打断有专属文案,提到「打断」。
        assert!(m
            .session_exited_text(ExitReason::ShutdownInterrupt)
            .contains("打断"));
        assert!(m
            .session_exited_text(ExitReason::ManualKill)
            .contains("关闭"));
        assert!(m
            .render_session_exited_text(ExitReason::GroupDisbanded, Some("研发群"), "oc_group",)
            .contains("研发群"));
    }

    /// 实测事故:`agents kill`(不带 `--reset`)与 `--reset` 共用一条「下次会开一段新的对话」,
    /// 但 `resume_store::clear_chat` 只在 `--reset` 时调用 —— 不带 reset 的 kill 实际是**续上下文**的,
    /// 回执把它说成新对话是错的。同一个洞也影响外部销毁 tmux(如 `/quit`)的兜底合成回执。
    #[test]
    fn manual_kill_text_distinguishes_reset_from_plain() {
        use crate::session::exit::{manual_kill_reason, ExitReason};
        let m = builtin_messaging();
        assert_eq!(manual_kill_reason(false), ExitReason::ManualKill);
        assert_eq!(manual_kill_reason(true), ExitReason::ManualKillReset);

        let plain = m.session_exited_text(ExitReason::ManualKill);
        let reset = m.session_exited_text(ExitReason::ManualKillReset);
        assert_ne!(plain, reset, "两者行为不同,文案不能共用");
        assert!(plain.contains("续聊"), "不带 reset = 续上下文: {plain}");
        assert!(
            !plain.contains("全新"),
            "不带 reset 绝不能说成全新对话: {plain}"
        );
        assert!(reset.contains("全新"), "带 reset = 不续旧上下文: {reset}");
    }

    #[test]
    fn session_exited_override_merges_per_field() {
        use crate::session::exit::ExitReason;
        let o: MessagingOverride =
            serde_yaml::from_str("session_exited:\n  manual_kill: \"自定义关闭文案\"").unwrap();
        let mut m = builtin_messaging();
        let before_clean = m.session_exited_text(ExitReason::PanelClean).to_string();
        m.apply(&o);
        assert_eq!(
            m.session_exited_text(ExitReason::ManualKill),
            "自定义关闭文案"
        );
        // 未覆盖字段保持 builtin
        assert_eq!(m.session_exited_text(ExitReason::PanelClean), before_clean);
    }

    #[test]
    fn group_exit_text_names_the_group_and_escapes_external_name() {
        use crate::session::exit::ExitReason;
        let m = builtin_messaging();
        // 群名是外部文本:`<Platform>` 是字母开头的标签形态,须拆散防飞书剥掉;`&` 与
        // 中文开头的 `<平台>` 本就安全,原样保留(飞书不解码实体,不能再转 `&amp;`/`&lt;`)。
        assert_eq!(
            m.render_session_exited_text(
                ExitReason::BotRemoved,
                Some("研发 & <Platform> <平台>"),
                "oc_group_a",
            ),
            "我已被移出群「研发 & <\u{200B}Platform> <平台>」，相关会话已退出。需要的话随时私聊我。"
        );
    }

    #[test]
    fn group_exit_text_falls_back_to_stable_tag_and_upgrades_old_override() {
        use crate::session::exit::ExitReason;
        let mut m = builtin_messaging();
        m.session_exited.bot_removed = "我已被移出那个群，相关会话已退出。".into();
        let rendered =
            m.render_session_exited_text(ExitReason::BotRemoved, Some("群聊"), "oc_group_a");
        assert!(rendered.starts_with("我已被移出那个群，相关会话已退出。\n\n来源：群聊（标识 g"));
        assert_eq!(
            rendered,
            m.render_session_exited_text(ExitReason::BotRemoved, None, "oc_group_a")
        );
        assert_ne!(
            rendered,
            m.render_session_exited_text(ExitReason::BotRemoved, None, "oc_group_b"),
            "不同群必须有不同兜底标识"
        );
    }

    /// bot 发来的消息必须在 prompt 里标出发送方是 bot。agent 的「多 bot @ 时判断自己是不是
    /// 主要收件人、拿不准就静默」规则,依赖这个信号;标不出来,bot 和真人长得一模一样。
    #[test]
    fn render_prompt_marks_bot_senders_and_leaves_humans_untouched() {
        let m = builtin_messaging();
        assert_eq!(
            m.render_prompt("sample测试助手", "看下", Some("研发群"), true, true, false),
            "bot「sample测试助手」 @你:「看下」"
        );
        assert_eq!(
            m.render_prompt(
                "sample测试助手",
                "顺带一提",
                Some("研发群"),
                false,
                true,
                false
            ),
            "bot「sample测试助手」 说:「顺带一提」"
        );
        assert_eq!(
            m.render_prompt("remote-cursor", "在么", None, true, true, false),
            "来自 bot「remote-cursor」 的飞书消息:「在么」"
        );
        // 真人保持逐字节不变,不加噪音。
        assert_eq!(
            m.render_prompt("老板", "看下", Some("研发群"), true, false, false),
            "老板 @你:「看下」"
        );
        // 名字解析不到时(该 app 没申请 im:chat.members:read),who 退成 open_id —— 仍然可区分,
        // 不能所有 bot 都糊成同一个「bot」。
        assert_eq!(
            m.render_prompt(
                "ou_22222222222222222222222222222222",
                "在么",
                Some("研发群"),
                true,
                true,
                false
            ),
            "bot「ou_22222222222222222222222222222222」 @你:「在么」"
        );
    }

    /// 外部发起人(没过 operate-gate)必须在 prompt 里标出来:agent 据此判断该不该照做。
    /// 真正的边界是 PreToolUse 闸,标签只是提示 —— 但没有标签,连提示都没有。
    #[test]
    fn external_sender_is_labelled_and_full_access_is_untouched() {
        let m = builtin_messaging();
        assert_eq!(
            m.render_prompt("李四", "看下", Some("研发群"), true, false, true),
            "李四(外部) @你:「看下」"
        );
        // 全权发起人:逐字节不变。常见路径不加噪音,也避免扰动既有会话上下文。
        assert_eq!(
            m.render_prompt("张三", "看下", Some("研发群"), true, false, false),
            "张三 @你:「看下」"
        );
        // 非 @ 的旁听消息同样带标签
        assert_eq!(
            m.render_prompt("李四", "顺带一提", Some("研发群"), false, false, true),
            "李四(外部) 说:「顺带一提」"
        );
    }

    #[test]
    fn external_bot_carries_both_labels() {
        let m = builtin_messaging();
        // bot 标记与外部标记正交:两个都要,agent 才能同时判断「是不是人」和「有没有权」。
        assert_eq!(
            m.render_prompt("remote-cursor", "在么", None, true, true, true),
            "来自 bot「remote-cursor」(外部) 的飞书消息:「在么」"
        );
    }

    #[test]
    fn render_templates_substitute_placeholders() {
        let m = builtin_messaging();
        // 私聊:不变
        assert_eq!(
            m.render_prompt("老板", "你好", None, true, false, false),
            "来自 老板 的飞书消息:「你好」"
        );
        // 群聊 + 被直接 @:显示「XX @你」,不再每轮重复群名(会话级 PRIMER 声明)
        assert_eq!(
            m.render_prompt("老板", "看下", Some("研发群"), true, false, false),
            "老板 @你:「看下」"
        );
        // 群聊 + 被 CC(非开头 @):显示「XX 说」,agent 自行判断是否响应
        assert_eq!(
            m.render_prompt(
                "老板",
                "帮我 @星光 看看",
                Some("研发群"),
                false,
                false,
                false
            ),
            "老板 说:「帮我 @星光 看看」"
        );
    }

    #[test]
    fn global_messaging_override_merges_onto_builtin() {
        let c: Config =
            serde_yaml::from_str("messaging:\n  working_reaction: HEART\n  idle_scan_secs: 60")
                .unwrap();
        assert_eq!(c.messaging.working_reaction.as_deref(), Some("HEART"));
        let mut m = builtin_messaging();
        m.apply(&c.messaging);
        assert_eq!(m.working_reaction, "HEART"); // 覆盖
        assert_eq!(m.done_reaction, "DONE"); // 未指定 → builtin
        assert_eq!(m.idle_scan_secs, 60);
        assert!(!m.reactions.is_empty()); // 未指定 → builtin 全集
    }

    #[test]
    fn builtin_messaging_has_efforts_and_models_by_cli() {
        // builtin asset 必须带 efforts(否则 builtin_messaging() expect 解析后字段空)+ per-CLI 模型桶。
        let m = builtin_messaging();
        assert!(m.efforts.contains(&"high".to_string()));
        assert!(m.efforts.contains(&"max".to_string()));
        assert!(m.efforts.iter().all(|e| !e.is_empty()));
        // ultra 是 codex 独有档位(gpt-5.6-sol/terra),内置表要带着它,交集那步才滤得出来。
        assert!(m.efforts.contains(&"ultra".to_string()));
        // 每桶都给多个候选:面板按钮只列桶内项,只放一个等于「切不了模型」。
        assert_eq!(
            m.models_for_cli("codex"),
            ["gpt-5.6-sol", "gpt-5.6-terra", "gpt-5.6-luna", "gpt-5.5"]
        );
        assert_eq!(
            m.models_for_cli("claude"),
            ["claude-opus-5", "claude-fable-5", "claude-sonnet-5"]
        );
        // cursor 只认 ACP 会话下发的目录,内置不给兜底:目录没拿到时要显出来,不拿猜的表顶上。
        assert!(!m.models_by_cli.contains_key("cursor"));
        assert!(m.models_for_cli("cursor").is_empty());
        for cli in ["claude", "codex"] {
            assert!(
                m.models_for_cli(cli).len() > 1,
                "{cli} 桶只有一个候选,面板上就切不了模型"
            );
        }
        // models_by_cli 命中 → 用桶;未命中 cli → 回退扁平 models。
        assert!(!m.models_for_cli("claude").is_empty());
        assert_eq!(m.models_for_cli("nonesuch"), m.models.as_slice());
    }

    /// 出厂 slash 黑名单的真相源是 `messaging.default.yaml`,**不硬编码在 Rust 里** ——
    /// owner 2026-08-14 显式要求这些项可配、可移除。这条钉住 schema 与出厂清单两件事。
    #[test]
    fn builtin_messaging_ships_the_known_slash_badcases_in_its_blacklist() {
        let m = builtin_messaging();
        for name in ["/login", "/logout", "/config", "/model"] {
            assert!(
                m.slash_blacklist.default.iter().any(|n| n == name),
                "{name} 是已知 badcase,出厂 default 段应带着它"
            );
        }
        // 出厂只填 default 段:per-backend 段结构先就位(codex/cursor 的 slash 通路还不存在),
        // 语义已定 —— 它们接入时不必再改判定。
        for bucket in [
            &m.slash_blacklist.claude,
            &m.slash_blacklist.codex,
            &m.slash_blacklist.cursor,
        ] {
            assert!(bucket.is_empty(), "出厂不预置 per-backend 项");
        }
        // 生命周期命令**不**在黑名单里:它们走受控映射(`/quit` `/exit` 一步生效、`/clear`
        // 确认卡),塞进黑名单等于把 owner 授权的第五条途径关掉。
        for name in ["/quit", "/exit", "/clear"] {
            assert!(
                !m.slash_blacklist.default.iter().any(|n| n == name),
                "{name} 走受控映射,不该出现在黑名单里"
            );
        }
    }

    /// messaging 层的热覆盖语义与其它项相同,只是粒度是**段**:写了的段整段替换
    /// (所以出厂项删得掉),没写的段继承出厂值(所以不会被顺手清空)。
    #[test]
    fn messaging_apply_replaces_slash_blacklist_sections_and_keeps_the_rest() {
        let mut m = builtin_messaging();
        let o: MessagingOverride =
            serde_yaml::from_str("slash_blacklist:\n  claude:\n    - /login\n").unwrap();
        m.apply(&o);
        assert_eq!(m.slash_blacklist.claude, vec!["/login".to_string()]);
        assert!(
            m.slash_blacklist.default.iter().any(|n| n == "/config"),
            "没写的 default 段必须保持出厂值"
        );
        // 出厂项可移除 —— 那是 owner 的权利,不再硬编码钉死。
        let mut m2 = builtin_messaging();
        m2.apply(&serde_yaml::from_str("slash_blacklist:\n  default: []\n").unwrap());
        assert!(m2.slash_blacklist.default.is_empty());
        // 整段不写 slash_blacklist → 出厂值一字不动(与其它 messaging 项同款)。
        let mut m3 = builtin_messaging();
        m3.apply(&serde_yaml::from_str("efforts: [low]\n").unwrap());
        assert_eq!(m3.slash_blacklist, builtin_messaging().slash_blacklist);
    }

    #[test]
    fn messaging_apply_overrides_efforts_and_models_by_cli() {
        let mut m = builtin_messaging();
        let o: MessagingOverride = serde_yaml::from_str(
            "efforts: [low, high]\nmodels_by_cli:\n  claude:\n    - my-claude\n",
        )
        .unwrap();
        m.apply(&o);
        assert_eq!(m.efforts, vec!["low".to_string(), "high".to_string()]);
        assert_eq!(m.models_for_cli("claude"), &["my-claude".to_string()]);
        // per-key merge:只覆盖 claude 桶,未提及的 codex 桶继承内置默认(不再被整表替换抹掉)。
        assert_eq!(
            m.models_for_cli("codex"),
            builtin_messaging().models_for_cli("codex"),
            "未覆盖的 codex 桶应保留内置默认,而非回退扁平 models"
        );
    }

    #[test]
    fn messaging_override_applies_only_set_fields() {
        let mut m = builtin_messaging();
        let o: MessagingOverride =
            serde_yaml::from_str("prompt_template: \"{source} -> {text}\"\nidle_ttl_secs: 3600")
                .unwrap();
        m.apply(&o);
        assert_eq!(
            m.render_prompt("A", "hi", Some("G"), true, false, false),
            "A @你 -> hi"
        ); // 覆盖
        assert_eq!(m.idle_ttl_secs, 3600); // 覆盖
        assert_eq!(m.working_reaction, "OnIt"); // 未覆盖 → builtin
    }

    #[test]
    fn document_comment_timeout_defaults_and_layered_positive_overrides() {
        let mut m = builtin_messaging();
        assert_eq!(m.document_comment_timeout_secs.get(), 300);
        let mut document = serde_yaml::to_value(&m).unwrap();
        document
            .as_mapping_mut()
            .unwrap()
            .remove(serde_yaml::Value::String(
                "document_comment_timeout_secs".into(),
            ));
        assert_eq!(
            serde_yaml::from_value::<Messaging>(document)
                .unwrap()
                .document_comment_timeout_secs
                .get(),
            300
        );
        let global: Config =
            serde_yaml::from_str("messaging:\n  document_comment_timeout_secs: 120\n").unwrap();
        m.apply(&global.messaging);
        assert_eq!(m.document_comment_timeout_secs.get(), 120);
        let agent: crate::registry::Agent =
            serde_yaml::from_str("name: probe\nmessaging:\n  document_comment_timeout_secs: 600\n")
                .unwrap();
        m.apply(agent.messaging.as_ref().unwrap());
        assert_eq!(m.document_comment_timeout_secs.get(), 600);
        m.apply(&serde_yaml::from_str("document_comment_timeout_secs: null").unwrap());
        assert_eq!(m.document_comment_timeout_secs.get(), 600);
        for value in ["0", "-1", "1.5", "bad"] {
            assert!(serde_yaml::from_str::<MessagingOverride>(&format!(
                "document_comment_timeout_secs: {value}"
            ))
            .is_err());
        }
    }

    #[test]
    fn headless_stall_parses_defaults_global_agent_and_positive_seconds() {
        let mut m = builtin_messaging();
        assert_eq!(m.headless_stall(), Duration::from_secs(1800));
        let mut document = serde_yaml::to_value(&m).unwrap();
        document
            .as_mapping_mut()
            .unwrap()
            .remove(serde_yaml::Value::String("headless_stall_secs".into()));
        let omitted: Messaging = serde_yaml::from_value(document).unwrap();
        assert_eq!(omitted.headless_stall(), Duration::from_secs(1800));
        let global: Config =
            serde_yaml::from_str("messaging:\n  headless_stall_secs: 900\n").unwrap();
        m.apply(&global.messaging);
        assert_eq!(m.headless_stall(), Duration::from_secs(900));
        let agent: crate::registry::Agent =
            serde_yaml::from_str("name: probe\nmessaging:\n  headless_stall_secs: 3600\n").unwrap();
        m.apply(agent.messaging.as_ref().unwrap());
        assert_eq!(m.headless_stall(), Duration::from_secs(3600));
        m.apply(&serde_yaml::from_str("headless_stall_secs: null").unwrap());
        assert_eq!(m.headless_stall(), Duration::from_secs(3600));
        for value in ["0", "-1", "1.5", "bad"] {
            assert!(serde_yaml::from_str::<MessagingOverride>(&format!(
                "headless_stall_secs: {value}"
            ))
            .is_err());
        }
    }

    /// timer_idle_retire_secs 的解析契约。默认**开**(3300 = 55min)是 owner 2026-08-13 显式
    /// 授权的(打破「回收类机制默认关」惯例);关闭的显式写法是 0,不是 null——
    /// YAML null 对 Option 字段落 outer None(= 没写),表达不了「显式关」。
    #[test]
    fn timer_idle_retire_secs_parses_default_override_and_off() {
        // 内置默认:开,55min。
        let m = builtin_messaging();
        assert_eq!(m.timer_idle_retire_secs, 3300);
        assert_eq!(m.timer_idle_retire(), Some(Duration::from_secs(3300)));

        // 覆盖成自定义窗口。
        let mut m = builtin_messaging();
        let o: MessagingOverride = serde_yaml::from_str("timer_idle_retire_secs: 7200").unwrap();
        m.apply(&o);
        assert_eq!(m.timer_idle_retire(), Some(Duration::from_secs(7200)));

        // 显式关:0。
        let mut m = builtin_messaging();
        let o: MessagingOverride = serde_yaml::from_str("timer_idle_retire_secs: 0").unwrap();
        m.apply(&o);
        assert_eq!(m.timer_idle_retire(), None, "0 = 关,回到旧行为");

        // 没写这行 → 保持默认开(其它覆盖不牵连它)。
        let mut m = builtin_messaging();
        let o: MessagingOverride = serde_yaml::from_str("idle_ttl_secs: 60").unwrap();
        m.apply(&o);
        assert_eq!(m.timer_idle_retire(), Some(Duration::from_secs(3300)));

        // 写 null:serde 落 outer None = 视同没写,**不是**关(钉死这个语义,文档也这么写)。
        let mut m = builtin_messaging();
        let o: MessagingOverride = serde_yaml::from_str("timer_idle_retire_secs: null").unwrap();
        m.apply(&o);
        assert_eq!(
            m.timer_idle_retire(),
            Some(Duration::from_secs(3300)),
            "null 落回内置默认(开),要关必须写 0"
        );
    }

    #[test]
    fn timer_idle_retire_reset_defaults_off_and_overrides_on() {
        // 内置默认:关(退出续聊,行为与本机制引入前一致)。
        let m = builtin_messaging();
        assert!(!m.timer_idle_retire_reset);

        // 覆盖成开。
        let mut m = builtin_messaging();
        let o: MessagingOverride = serde_yaml::from_str("timer_idle_retire_reset: true").unwrap();
        m.apply(&o);
        assert!(m.timer_idle_retire_reset);

        // 没写这行 → 保持默认(其它覆盖不牵连它)。
        let mut m = builtin_messaging();
        let o: MessagingOverride = serde_yaml::from_str("idle_ttl_secs: 60").unwrap();
        m.apply(&o);
        assert!(!m.timer_idle_retire_reset);
    }

    #[test]
    fn badge_effort_defaults_on_and_overrides_off() {
        // 内置默认:开(角标照渲思考档位)。
        let m = builtin_messaging();
        assert!(m.badge_effort);

        let mut m = builtin_messaging();
        let o: MessagingOverride = serde_yaml::from_str("badge_effort: false").unwrap();
        m.apply(&o);
        assert!(!m.badge_effort);

        // 没写这行 → 保持默认(其它覆盖不牵连它)。
        let mut m = builtin_messaging();
        let o: MessagingOverride = serde_yaml::from_str("cot_visible: false").unwrap();
        m.apply(&o);
        assert!(m.badge_effort);
    }

    #[test]
    fn path_exists_in_value_detects_nested_keys() {
        let v: serde_yaml::Value = serde_yaml::from_str(
            "messaging:\n  session_mirror: true\n  idle_ttl_secs: 3600\nresume_sessions: false\n",
        )
        .unwrap();
        // 顶层存在
        assert!(path_exists_in_value(&v, "resume_sessions"));
        // 嵌套存在
        assert!(path_exists_in_value(&v, "messaging.session_mirror"));
        assert!(path_exists_in_value(&v, "messaging.idle_ttl_secs"));
        // 嵌套不存在
        assert!(!path_exists_in_value(&v, "messaging.no_such_key"));
        // 顶层不存在
        assert!(!path_exists_in_value(&v, "missing_top_level"));
        // 中间层不是 map
        assert!(!path_exists_in_value(&v, "resume_sessions.x"));
    }

    #[test]
    fn path_exists_in_value_handles_empty_and_scalar() {
        // 空 map
        let v: serde_yaml::Value = serde_yaml::Value::Mapping(Default::default());
        assert!(!path_exists_in_value(&v, "anything"));
        // 标量根(非 map)
        let v: serde_yaml::Value = serde_yaml::Value::String("hello".into());
        assert!(!path_exists_in_value(&v, "foo"));
    }

    #[test]
    fn dashboard_defaults_on_at_7767_and_is_overridable() {
        let cfg = Config::default();
        assert!(cfg.dashboard.enabled);
        assert_eq!(cfg.dashboard.port, 7767);
        assert!(cfg.dashboard.public_base_url.is_empty());
        let (cfg, err) = parse_or_fallback(
            "dashboard:\n  port: 8000\n  public_base_url: https://dash.example.com\n",
            Config::default(),
        );
        assert!(err.is_none());
        assert!(cfg.dashboard.enabled, "只写部分字段时其余取默认");
        assert_eq!(cfg.dashboard.port, 8000);
        assert_eq!(cfg.dashboard.public_base_url, "https://dash.example.com");
    }

    #[test]
    fn web_terminal_defaults_on_at_7766_and_is_overridable() {
        let (cfg, err) = parse_or_fallback("poll_ms: 400\n", Config::default());
        assert!(err.is_none());
        assert!(cfg.web_terminal.enabled, "owner 2026-09-22:出厂开");
        assert_eq!(cfg.web_terminal.port, 7766);
        let (cfg, err) = parse_or_fallback("web_terminal:\n  enabled: false\n", Config::default());
        assert!(err.is_none());
        assert!(!cfg.web_terminal.enabled);
        assert_eq!(cfg.web_terminal.port, 7766, "只写部分字段时其余取默认");
    }

    #[test]
    fn web_terminal_base_url_uses_its_own_port_and_never_falls_back_to_localhost() {
        let wt = WebTerminal::default();
        let ip: std::net::Ipv4Addr = "10.1.2.3".parse().unwrap();
        assert_eq!(
            wt.base_url_with_host(Some(ip)).as_deref(),
            Some("http://10.1.2.3:7766")
        );
        assert_eq!(wt.base_url_with_host(None), None);
    }
}
