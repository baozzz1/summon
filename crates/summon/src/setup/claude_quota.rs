//! Claude CLI 全机额度观察接入。
//!
//! Claude 只允许一个 `statusLine.command`，因此安装不能直接覆盖用户已有命令：summon 先把原
//! block 完整保存到自己的安装状态，再换成 wrapper；wrapper 记录结构化额度快照后，以同一份
//! stdin 链式调用原命令。
//! 本模块只负责配置与观察，不依赖 daemon 或菜单栏。

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const STATE_VERSION: u32 = 1;

/// 安装时由上层解析好真实二进制及 wrapper 命令；本模块不假设 summon 安装在哪个目录。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallSpec {
    /// 写入 Claude `statusLine.command` 的完整 shell 命令。
    pub status_line_wrapper_command: String,
}

/// Claude 全局接入与 summon 私有状态的固定路径。测试与迁移工具可通过 `at_home` 注入根目录，
/// 生产入口统一走 `for_current_user`，避免 daemon 和手动命令各自拼一套路径。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntegrationPaths {
    pub settings: PathBuf,
    pub state: PathBuf,
    pub snapshot: PathBuf,
}

impl IntegrationPaths {
    pub fn at_home(home: &Path) -> Self {
        Self {
            settings: home.join(".claude/settings.json"),
            state: home.join(".summon/quota-recovery/claude-integration.json"),
            // 多个 Claude CLI 可并行刷新 statusLine；每 session 独立文件，避免全局 last-writer 覆盖。
            snapshot: home.join(".summon/quota-recovery/claude-status"),
        }
    }

    pub fn for_current_user() -> Result<Self> {
        let home = dirs::home_dir().context(summon_i18n::text!("ra1b3a366ca"))?;
        Ok(Self::at_home(&home))
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InstallState {
    version: u32,
    previous_status_line: Option<Value>,
    installed_status_line: Value,
    delegate_command: Option<String>,
    status_line_wrapper_command: String,
}

impl InstallState {
    pub fn delegate_command(&self) -> Option<&str> {
        self.delegate_command.as_deref()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IntegrationStatus {
    NotInstalled,
    Installed,
    /// 安装状态还在，settings 里整个没有 statusLine。这是整份 settings 被重置(重新登录、清理
    /// `~/.claude`)的结果：没有可被覆盖的用户值，按安装状态原样重建，链在后面的原命令随之恢复。
    Missing,
    /// 安装状态还在，但用户或别的工具改动了 Claude settings；调用方应展示修复提示，不能覆盖。
    Drifted {
        status_line_owned: bool,
    },
}

/// 用当前 summon 可执行文件生成 Claude statusLine wrapper。
/// statusLine.command 必须经过 POSIX 单引号转义，路径含空格或单引号时仍只执行这一份二进制。
pub fn install_spec_for_exe(executable: &Path) -> Result<InstallSpec> {
    let executable = executable
        .to_str()
        .context(summon_i18n::text!("rd29f1c6c2e"))?;
    Ok(InstallSpec {
        status_line_wrapper_command: format!(
            "{} daemon claude-quota-statusline",
            shell_quote(executable)
        ),
    })
}

/// daemon 启动、运行期复查与 `claude-quota install` 共用的保守幂等入口。
///
/// 未安装就装(settings.json 不存在时一并新建)；statusLine 缺失时等 settings.json 存在再按安装
/// 状态重建，此前返回 `Missing`、不写；已安装不动；statusLine 被改成别的值都拒绝自动修复，
/// 要求 owner 先查看 status 并显式 uninstall/install。
pub fn ensure_installed(paths: &IntegrationPaths, spec: &InstallSpec) -> Result<IntegrationStatus> {
    purge_legacy_stop_failure_hook(&paths.settings)?;
    match status(&paths.settings, &paths.state)? {
        IntegrationStatus::NotInstalled => install(&paths.settings, &paths.state, spec),
        // 有安装状态却连 settings.json 都没有，说明 `~/.claude` 正处在整体重置的过程中(整体
        // 挪走、清空后重新登录、迁移)：此刻写入会混进那个过程，抢在 Claude Code 初始化之前造出
        // 一份只有 statusLine 的 settings。首次安装没有这样一个进行中的过程，不必等。
        IntegrationStatus::Missing if !paths.settings.is_file() => Ok(IntegrationStatus::Missing),
        IntegrationStatus::Missing => install(&paths.settings, &paths.state, spec),
        IntegrationStatus::Installed => {
            let state = read_install_state(&paths.state)?;
            if state.version != STATE_VERSION
                || state.status_line_wrapper_command != spec.status_line_wrapper_command
            {
                bail!(summon_i18n::format!("r82db9bdd07"));
            }
            Ok(IntegrationStatus::Installed)
        }
        IntegrationStatus::Drifted { status_line_owned } => {
            bail!(summon_i18n::format!("r8cb4394915"))
        }
    }
}

/// 旧版在 `hooks.StopFailure` 里装过 `claude-quota-hook` 子命令；该子命令已删，
/// 留着只会让 Claude 每次 rate_limit 停止都调一个不存在的命令。daemon 启动时顺手清掉。
fn purge_legacy_stop_failure_hook(settings_path: &Path) -> Result<()> {
    let mut settings = read_settings(settings_path)?;
    if purge_legacy_hook(&mut settings) {
        write_json_atomic(settings_path, &settings)?;
    }
    Ok(())
}

fn purge_legacy_hook(settings: &mut Value) -> bool {
    let legacy_args = serde_json::json!(["daemon", "claude-quota-hook"]);

    let removed = {
        let Some(groups) = settings
            .pointer_mut("/hooks/StopFailure")
            .and_then(|v| v.as_array_mut())
        else {
            return false;
        };
        let before = groups.len();
        groups.retain(|group| {
            !group
                .get("hooks")
                .and_then(|h| h.as_array())
                .is_some_and(|handlers| {
                    handlers.iter().any(|h| h.get("args") == Some(&legacy_args))
                })
        });
        groups.len() < before
    };

    if removed {
        let stop_failure_empty = settings
            .pointer("/hooks/StopFailure")
            .and_then(|v| v.as_array())
            .is_some_and(|a| a.is_empty());
        if stop_failure_empty {
            if let Some(hooks) = settings
                .pointer_mut("/hooks")
                .and_then(|v| v.as_object_mut())
            {
                hooks.remove("StopFailure");
                if hooks.is_empty() {
                    if let Some(root) = settings.as_object_mut() {
                        root.remove("hooks");
                    }
                }
            }
        }
    }
    removed
}

/// 本机接入的落点：settings/state 路径与 wrapper 命令。
///
/// daemon 启动时解析一次，运行期复查沿用同一份：落点二进制在运行期被替换后，Linux 上
/// `current_exe()` 读出的路径带 ` (deleted)` 后缀，重新解析会拼出与安装状态对不上的 wrapper。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallTarget {
    pub paths: IntegrationPaths,
    pub spec: InstallSpec,
}

pub fn current_exe_install_target() -> Result<InstallTarget> {
    let home = dirs::home_dir().context(summon_i18n::text!("ra1b3a366ca"))?;
    let executable = std::env::current_exe().context(summon_i18n::text!("r3f14f2b415"))?;
    install_target_for_exe(&home, &executable)
}

fn install_target_for_exe(home: &Path, executable: &Path) -> Result<InstallTarget> {
    let executable = stable_entrypoint_for_exe(home, executable);
    Ok(InstallTarget {
        paths: IntegrationPaths::at_home(home),
        spec: install_spec_for_exe(&executable)?,
    })
}

/// `summon daemon claude-quota install` 的入口。显式命令只有两种结果：装好，或说明为什么没装。
pub fn install_current_exe() -> Result<()> {
    install_target(&current_exe_install_target()?)
}

fn install_target(target: &InstallTarget) -> Result<()> {
    match ensure_installed(&target.paths, &target.spec)? {
        IntegrationStatus::Installed => Ok(()),
        IntegrationStatus::Missing => bail!(summon_i18n::format!(
            "rc192286a62",
            target.paths.settings.display()
        )),
        status => bail!(summon_i18n::format!("rfbe306ca87")),
    }
}

#[cfg(test)]
fn ensure_exe_installed_at(home: &Path, executable: &Path) -> Result<IntegrationStatus> {
    let target = install_target_for_exe(home, executable)?;
    ensure_installed(&target.paths, &target.spec)
}

/// daemon 运行期复查入口，返回这次是否重建了 statusLine。
///
/// 首次安装只在 daemon 启动时做；运行期只修 [`IntegrationStatus::Missing`]，未安装与 statusLine
/// 被改成别的值都不碰。
pub fn restore_if_missing(target: &InstallTarget) -> Result<bool> {
    if status(&target.paths.settings, &target.paths.state)? != IntegrationStatus::Missing {
        return Ok(false);
    }
    Ok(ensure_installed(&target.paths, &target.spec)? == IntegrationStatus::Installed)
}

/// 部署后 daemon 从真实安装路径启动，而用户通常经 `~/.local/bin/summon`
/// 调用 CLI。两者若指向同一文件，统一记录稳定入口，避免重启时把自己误判为外部安装。
fn stable_entrypoint_for_exe(home: &Path, executable: &Path) -> PathBuf {
    let stable = home.join(".local/bin/summon");
    match (
        std::fs::canonicalize(&stable),
        std::fs::canonicalize(executable),
    ) {
        (Ok(stable_target), Ok(executable_target)) if stable_target == executable_target => stable,
        _ => executable.to_path_buf(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UninstallOutcome {
    pub status_line_restored: bool,
    pub status_line_drifted: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClaudeQuotaWindow {
    pub used_percentage: f64,
    /// Unix epoch seconds。
    pub resets_at: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClaudeQuotaSnapshot {
    pub session_id: String,
    pub transcript_path: Option<String>,
    pub cwd: Option<String>,
    pub five_hour: Option<ClaudeQuotaWindow>,
    pub seven_day: Option<ClaudeQuotaWindow>,
    /// 以下三项供**飞书角标**用,与额度恢复无关 —— 它们搭的是同一趟车:statusLine 每次刷新都把
    /// 会话状态喂给本 wrapper,而这些正是转录里拿不到的东西(claude 转录不产 usage,所以上下文
    /// 用量此前在角标上恒缺)。全部 `Option` + `#[serde(default)]`:旧快照读进来仍合法。
    /// 模型显示名(`model.display_name`,如 `Opus 5`)。
    #[serde(default)]
    pub model_display_name: Option<String>,
    /// 思考档位(`effort.level`,如 `xhigh`)。
    #[serde(default)]
    pub effort_level: Option<String>,
    /// 上下文窗口已用百分比 0..=100(`context_window.used_percentage`)。冷启时 claude 给 null。
    #[serde(default)]
    pub context_used_percentage: Option<f64>,
    /// 上下文窗口**总大小**(token,`context_window.context_window_size`)。
    ///
    /// 2026-08-18 接上:这个字段 payload 里一直都有,只是此前没解析。它是「上下文过重提醒」的
    /// 判据来源 —— 提醒与否只看窗口大小(见 `summon_core::context_budget::WARN_MIN_WINDOW`)。
    #[serde(default)]
    pub context_window_size: Option<u64>,
    /// 快照落盘时刻(Unix 秒)。角标的账号级额度缓存靠它在同一 reset 窗口内分辨快照新旧。
    #[serde(default)]
    pub observed_at: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct RawStatusLine {
    session_id: String,
    transcript_path: Option<String>,
    cwd: Option<String>,
    workspace: Option<RawWorkspace>,
    rate_limits: Option<RawRateLimits>,
    model: Option<RawModel>,
    effort: Option<RawEffort>,
    context_window: Option<RawContextWindow>,
}

#[derive(Debug, Deserialize)]
struct RawModel {
    display_name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawEffort {
    level: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawContextWindow {
    /// claude 在冷启(还没发过请求)时给 `null`,跑起来才有值。
    used_percentage: Option<f64>,
    /// 该模型的上下文窗口总 token 数。与 `used_percentage` 不同,它冷启时就有。
    context_window_size: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct RawWorkspace {
    current_dir: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawRateLimits {
    five_hour: Option<RawQuotaWindow>,
    seven_day: Option<RawQuotaWindow>,
}

#[derive(Debug, Deserialize)]
struct RawQuotaWindow {
    used_percentage: Option<f64>,
    resets_at: Option<i64>,
}

impl RawQuotaWindow {
    fn complete(self) -> Option<ClaudeQuotaWindow> {
        Some(ClaudeQuotaWindow {
            used_percentage: self.used_percentage?,
            resets_at: self.resets_at?,
        })
    }
}

/// 安装 Claude 全局观察入口。写入次序是 state → settings：若中途崩溃，下一次 install 能从 state
/// 重试；反过来会让 wrapper 已生效却找不到被保存的原 statusLine 命令。
pub fn install(
    settings_path: &Path,
    state_path: &Path,
    spec: &InstallSpec,
) -> Result<IntegrationStatus> {
    if spec.status_line_wrapper_command.trim().is_empty() {
        bail!(summon_i18n::format!("rffd55a9588"));
    }

    let mut settings = read_settings(settings_path)?;
    let existing_state = read_install_state_optional(state_path)?;

    let state = if let Some(state) = existing_state {
        if state.version != STATE_VERSION
            || state.status_line_wrapper_command != spec.status_line_wrapper_command
        {
            bail!(summon_i18n::format!("r9550185ac1"));
        }
        let current = current_status_line(&settings)?;
        if current.is_some()
            && current != Some(state.installed_status_line.clone())
            && current != state.previous_status_line
        {
            bail!(summon_i18n::format!("r0102cc4ba2"));
        }
        state
    } else {
        let previous_status_line = current_status_line(&settings)?;
        if previous_status_line
            .as_ref()
            .and_then(status_line_command)
            .is_some_and(|command| command == spec.status_line_wrapper_command)
        {
            bail!(summon_i18n::format!("r65dd7afd5a"));
        }
        let delegate_command = match previous_status_line.as_ref() {
            Some(value) => Some(validate_status_line_command(value)?.to_string()),
            None => None,
        };
        let installed_status_line = wrapped_status_line(
            previous_status_line.as_ref(),
            &spec.status_line_wrapper_command,
        )?;
        InstallState {
            version: STATE_VERSION,
            previous_status_line,
            installed_status_line,
            delegate_command,
            status_line_wrapper_command: spec.status_line_wrapper_command.clone(),
        }
    };

    set_status_line(&mut settings, state.installed_status_line.clone())?;
    write_json_atomic(state_path, &state)?;
    write_json_atomic(settings_path, &settings)?;
    status(settings_path, state_path)
}

/// statusLine 仍等于安装时 wrapper 才恢复；若用户后来改过，保留用户的新值并报告 drift，
/// 绝不拿旧备份反向覆盖。
pub fn uninstall(settings_path: &Path, state_path: &Path) -> Result<UninstallOutcome> {
    let Some(state) = read_install_state_optional(state_path)? else {
        return Ok(UninstallOutcome {
            status_line_restored: false,
            status_line_drifted: false,
        });
    };
    let mut settings = read_settings(settings_path)?;

    let current = current_status_line(&settings)?;
    let (status_line_restored, status_line_drifted) =
        if current == Some(state.installed_status_line.clone()) {
            restore_status_line(&mut settings, state.previous_status_line.clone())?;
            (true, false)
        } else if current == state.previous_status_line {
            (false, false)
        } else {
            (false, true)
        };

    write_json_atomic(settings_path, &settings)?;
    match std::fs::remove_file(state_path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(e)
                .with_context(|| summon_i18n::format!("rbe2d634e79", state_path.display()))
        }
    }
    Ok(UninstallOutcome {
        status_line_restored,
        status_line_drifted,
    })
}

pub fn status(settings_path: &Path, state_path: &Path) -> Result<IntegrationStatus> {
    // status 也必须先验证 settings；损坏 JSON 不能被“未安装”掩盖，否则后续修复可能误覆盖。
    let settings = read_settings(settings_path)?;
    let Some(state) = read_install_state_optional(state_path)? else {
        return Ok(IntegrationStatus::NotInstalled);
    };
    match current_status_line(&settings)? {
        None => Ok(IntegrationStatus::Missing),
        Some(current) if current == state.installed_status_line => Ok(IntegrationStatus::Installed),
        Some(_) => Ok(IntegrationStatus::Drifted {
            status_line_owned: false,
        }),
    }
}

pub fn parse_status_line_snapshot(input: &[u8]) -> Result<ClaudeQuotaSnapshot> {
    let raw: RawStatusLine =
        serde_json::from_slice(input).context(summon_i18n::text!("rd837471572"))?;
    if raw.session_id.trim().is_empty() {
        bail!(summon_i18n::format!("re606a42994"));
    }
    let cwd = raw
        .cwd
        .or_else(|| raw.workspace.and_then(|workspace| workspace.current_dir));
    let (five_hour, seven_day) = match raw.rate_limits {
        Some(rate_limits) => (
            rate_limits.five_hour.and_then(RawQuotaWindow::complete),
            rate_limits.seven_day.and_then(RawQuotaWindow::complete),
        ),
        None => (None, None),
    };
    Ok(ClaudeQuotaSnapshot {
        session_id: raw.session_id,
        transcript_path: raw.transcript_path,
        cwd,
        five_hour,
        seven_day,
        model_display_name: raw
            .model
            .and_then(|m| m.display_name)
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty()),
        effort_level: raw
            .effort
            .and_then(|e| e.level)
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty()),
        // 只收 0..=100 的有限值:角标要拿它渲百分比,越界或 NaN 宁可缺项也不渲一个荒唐数字。
        context_used_percentage: raw
            .context_window
            .as_ref()
            .and_then(|c| c.used_percentage)
            .filter(|p| p.is_finite() && (0.0..=100.0).contains(p)),
        // 0 视同没给:除零在 ContextBudget 里已挡,这里就地当「不知道」更贴语义。
        context_window_size: raw
            .context_window
            .and_then(|c| c.context_window_size)
            .filter(|&n| n > 0),
        observed_at: None,
    })
}

pub fn write_snapshot(root: &Path, snapshot: &ClaudeQuotaSnapshot) -> Result<()> {
    write_json_atomic(&snapshot_file(root, &snapshot.session_id)?, snapshot)
}

pub fn read_snapshot(root: &Path, session_id: &str) -> Result<ClaudeQuotaSnapshot> {
    let path = snapshot_file(root, session_id)?;
    let bytes = std::fs::read(&path)
        .with_context(|| summon_i18n::format!("r6da9ed443d", path.display()))?;
    let mut snapshot: ClaudeQuotaSnapshot = serde_json::from_slice(&bytes)
        .with_context(|| summon_i18n::format!("r7a19386521", path.display()))?;
    if snapshot.observed_at.is_none() {
        // 没盖戳的快照以文件 mtime 为落盘时刻:wrapper 原子写入,mtime 就是落盘那一刻。
        snapshot.observed_at = file_mtime_secs(&path);
    }
    Ok(snapshot)
}

fn file_mtime_secs(path: &Path) -> Option<i64> {
    std::fs::metadata(path)
        .and_then(|meta| meta.modified())
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|elapsed| elapsed.as_secs() as i64)
}

pub fn read_install_state(path: &Path) -> Result<InstallState> {
    let bytes =
        std::fs::read(path).with_context(|| summon_i18n::format!("r6da9ed443d", path.display()))?;
    serde_json::from_slice(&bytes)
        .with_context(|| summon_i18n::format!("r7a19386521", path.display()))
}

/// 用同一份 statusLine stdin 调用安装前保存的命令。返回完整 Output，由 CLI bridge 原样转发
/// stdout/stderr/exit code；没有原命令时返回 `None`（summon 只采集快照，不画状态栏）。
pub fn run_saved_status_line(state_path: &Path, input: &[u8]) -> Result<Option<Output>> {
    let state = read_install_state(state_path)?;
    let Some(delegate) = state.delegate_command() else {
        return Ok(None);
    };
    let mut child = Command::new("sh")
        // statusLine 原命令本就是 shell command；用非 login shell，避免额外读取 profile 改 PATH/
        // 输出，从而保持 wrapper 对原行为透明。
        .args(["-c", delegate])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| summon_i18n::format!("r7fd2198007"))?;
    child
        .stdin
        .take()
        .context(summon_i18n::text!("r65eb8cc604"))?
        .write_all(input)
        .context(summon_i18n::text!("rdddc60a57c"))?;
    child
        .wait_with_output()
        .context(summon_i18n::text!("ra505f7f6a8"))
        .map(Some)
}

/// anthropic.com 之外的端点(自建或第三方网关)才算「内部路由」。显式指向官方端点视同未改路由。
pub(crate) fn is_internal_routing_url(base_url: &str) -> bool {
    let raw = base_url.trim();
    if raw.is_empty() {
        return false;
    }
    // 不引 url 依赖:statusLine 给的就是 http(s)://host[:port]/... 形态,手剥 host 足够。
    let after_scheme = raw.split("://").nth(1).unwrap_or(raw);
    // host 只到路径/查询/fragment 为止。
    let host_port = after_scheme.split(['/', '?', '#']).next().unwrap_or("");
    // 去端口,小写(DNS 大小写不敏感),去 FQDN 尾点。
    let host = host_port
        .split(':')
        .next()
        .unwrap_or("")
        .trim()
        .to_lowercase();
    let host = host.strip_suffix('.').unwrap_or(&host);
    if host.is_empty() {
        return false;
    }
    // 按**标签边界**匹配,避免 notanthropic.com 这类后缀碰撞。
    host != "anthropic.com" && !host.ends_with(".anthropic.com")
}

/// 从转录末尾倒着找当前上下文的真实占用。
///
/// 三种结果:
/// - 当前(或最近)压缩区域内的最近一条 assistant usage → 三项 token 之和(input + 两块缓存);
/// - 尾部落在「压缩边界行已落盘、压缩后首轮 assistant 还没落盘」的短窗口 → 边界行的
///   `compactMetadata.postTokens`(压缩摘要本身的占用);
/// - 证不出来 → None(调用方保留上游值,绝不发明数字)。
///
/// 已知限制:压缩**请求进行中**(从自动压缩触发到边界行落盘,实测约一个 summarize 请求时长、
/// 量级分钟)转录尾部只有压缩前的 assistant 记录,此函数返回的是压缩前占用 —— 转录里没有
/// 「压缩开始」标记,无法可靠识别该窗口,不做猜测性启发式。该窗口内角标沿用压缩前值,
/// 边界行落盘后立即纠正。
///
/// 只读尾部窗口:转录会长到几十 MB,statusLine 又是秒级刷新,全扫不起。首读 2MB;窗口内
/// 没有可用记录且文件更长时,用 8MB 复读一次 —— 单条 assistant 行(带大工具结果)实测可接近
/// 2MB,被腰斩时扩窗才能读到;超过 8MB 的巨行仍返回 None。
fn last_transcript_context_usage(transcript_path: &Path) -> Option<u64> {
    const TAIL_FIRST: u64 = 2 * 1024 * 1024;
    const TAIL_MAX: u64 = 8 * 1024 * 1024;
    read_tail_usage(transcript_path, TAIL_FIRST).or_else(|| {
        let len = std::fs::metadata(transcript_path).ok()?.len();
        (len > TAIL_FIRST).then(|| read_tail_usage(transcript_path, TAIL_MAX))?
    })
}

fn read_tail_usage(transcript_path: &Path, window: u64) -> Option<u64> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(transcript_path).ok()?;
    let len = file.metadata().ok()?.len();
    let start = len.saturating_sub(window);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = Vec::with_capacity((len - start) as usize);
    file.read_to_end(&mut buf).ok()?;
    // 倒序遍历:先看文件尾部。文件末尾若不是换行符,最后一段可能是 Claude Code 正在追加的
    // 半行(完整记录的写入是「内容+换行」),丢掉等下次刷新;末尾有换行时最后一段是空串,
    // 丢弃无意义且可能误伤恰好无尾换行的完整记录,所以以 ends_with 换行作为唯一判据。
    // 起点处腰斩的半段在倒序里最后才遇到,JSON 解析必败被自然跳过,无需特判。
    let mut lines = buf.split(|&b| b == b'\n').rev();
    if !buf.ends_with(b"\n") {
        lines.next();
    }
    // 倒序越过压缩摘要行之后,更旧方向的 assistant 记录全是压缩前的脏数据,一律不可用;
    // 只有 compact_boundary 行的 postTokens 能代表压缩后起点。
    let mut crossed_compact_summary = false;
    for line in lines {
        let Ok(value) = serde_json::from_slice::<Value>(line) else {
            // 写入中的半行 / 坏行:跳过,不影响取更早的完整记录。
            continue;
        };
        match value.get("type").and_then(Value::as_str) {
            Some("assistant") if !crossed_compact_summary => {
                if let Some(total) = assistant_context_tokens(&value) {
                    return Some(total);
                }
            }
            Some("user")
                if value.get("isCompactSummary").and_then(Value::as_bool) == Some(true) =>
            {
                crossed_compact_summary = true;
            }
            Some("system")
                if value.get("subtype").and_then(Value::as_str) == Some("compact_boundary") =>
            {
                // 边界行在压缩**完成**时才落盘(postTokens 此刻才可知);压缩请求本身进行中的
                // 窗口无法从转录识别,见函数文档的已知限制。这里覆盖边界行落盘到新轮 assistant
                // 落盘之间的短窗口,postTokens 是该窗口内唯一可信占用。
                return value
                    .pointer("/compactMetadata/postTokens")
                    .and_then(Value::as_u64)
                    .filter(|&n| n > 0);
            }
            _ => {}
        }
    }
    None
}

fn assistant_context_tokens(value: &Value) -> Option<u64> {
    let usage = value.pointer("/message/usage")?;
    let pick = |key: &str| usage.get(key).and_then(Value::as_u64).unwrap_or(0);
    let total = pick("input_tokens")
        + pick("cache_read_input_tokens")
        + pick("cache_creation_input_tokens");
    (total > 0).then_some(total)
}

/// 内部路由模型的上下文百分比兜底。
///
/// Claude Code 的 statusLine `context_window.used_percentage` 在会话自动压缩后续跑会钉死在 0
/// (2026-09-17 在 CC 2.1.274 上仍可复现,官方模型同版本正常)。**只在上游报 0 或缺值时**启用:
/// 上游给了健康非零值就以它为准 —— 它的口径与 CC 自己的压缩策略一致,转录反算只反映最近一次
/// 请求的输入侧,不该在正常值时去覆盖。经网关接入的模型的转录每条 assistant 消息都带真实 API usage,
/// 0 时倒读转录反算;转录取不到(首轮还没出过 assistant 消息)就保留上游值。
fn enrich_context_usage(snapshot: &mut ClaudeQuotaSnapshot, internal: bool) {
    if !internal {
        return;
    }
    if snapshot.context_used_percentage.is_some_and(|p| p > 0.0) {
        return;
    }
    let Some(window) = snapshot.context_window_size else {
        return;
    };
    let Some(transcript) = snapshot.transcript_path.as_ref() else {
        return;
    };
    let Some(used) = last_transcript_context_usage(Path::new(transcript)) else {
        return;
    };
    let pct = (used as f64 * 100.0 / window as f64).clamp(0.0, 100.0);
    snapshot.context_used_percentage = Some(pct);
}

/// wrapper 的组合入口：快照落盘失败不能让用户原状态栏消失，因此观察写是显式 best-effort；原命令
/// 的执行错误仍向上传递。调用方负责把 Output 原样写回 stdout/stderr 并使用同一退出码。
pub fn bridge_status_line(
    state_path: &Path,
    snapshot_path: &Path,
    input: &[u8],
    managed_session: bool,
    delegate_in_managed_session: bool,
    internal: bool,
) -> Result<Option<Output>> {
    if let Ok(mut snapshot) = parse_status_line_snapshot(input) {
        snapshot.observed_at = Some(unix_now_secs());
        enrich_context_usage(&mut snapshot, internal);
        // 额度观察是旁路；磁盘瞬时故障不应阻断既有 statusLine 渲染。
        let _ = write_snapshot(snapshot_path, &snapshot);
    }
    // summon 起的会话默认不画 statusLine:同样的信息已经透到飞书角标上,pane 里再画一遍只是占掉
    // 两行、给抓屏判据添噪音。但有的 statusLine 命令不只是画 —— 它同时把数据透传给统计平台,
    // 不跑就断了。所以这一步由机器所有者用 `claude.status_line_in_managed_sessions` 决定。
    //
    // **采集不受任何一侧影响** —— 快照上面已经写完了,这里决定的只是「要不要把原 statusLine
    // 命令跑起来并转发它的输出」。用户自己开的 claude 一如既往。
    if managed_session && !delegate_in_managed_session {
        return Ok(None);
    }
    run_saved_status_line(state_path, input)
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn unix_now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or(0)
}

fn snapshot_file(root: &Path, session_id: &str) -> Result<PathBuf> {
    if session_id.is_empty()
        || session_id.len() > 128
        || !session_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        bail!(summon_i18n::format!("r88c8110296"))
    }
    Ok(root.join(format!("{session_id}.json")))
}

fn read_settings(path: &Path) -> Result<Value> {
    if !path.exists() {
        return Ok(Value::Object(Map::new()));
    }
    let bytes =
        std::fs::read(path).with_context(|| summon_i18n::format!("r6da9ed443d", path.display()))?;
    let value: Value = serde_json::from_slice(&bytes)
        .with_context(|| summon_i18n::format!("rcddfa85ce9", path.display()))?;
    if !value.is_object() {
        bail!(summon_i18n::format!("r848da7a17c", path.display()));
    }
    Ok(value)
}

fn read_install_state_optional(path: &Path) -> Result<Option<InstallState>> {
    if !path.exists() {
        return Ok(None);
    }
    read_install_state(path).map(Some)
}

fn write_json_atomic(path: &Path, value: &impl Serialize) -> Result<()> {
    let mut bytes = serde_json::to_vec_pretty(value).context(summon_i18n::text!("r15c25d663e"))?;
    bytes.push(b'\n');
    crate::runtime::fsio::atomic_write(path, &bytes)
}

fn settings_object(settings: &Value) -> Result<&Map<String, Value>> {
    settings
        .as_object()
        .context(summon_i18n::text!("rc736a494ad"))
}

fn settings_object_mut(settings: &mut Value) -> Result<&mut Map<String, Value>> {
    settings
        .as_object_mut()
        .context(summon_i18n::text!("rc736a494ad"))
}

fn current_status_line(settings: &Value) -> Result<Option<Value>> {
    match settings_object(settings)?.get("statusLine") {
        None => Ok(None),
        Some(value) if value.is_object() => Ok(Some(value.clone())),
        Some(_) => bail!(summon_i18n::format!("r4bb15c2c98")),
    }
}

fn status_line_command(value: &Value) -> Option<&str> {
    value.get("command").and_then(Value::as_str)
}

fn validate_status_line_command(value: &Value) -> Result<&str> {
    let object = value
        .as_object()
        .context(summon_i18n::text!("r0261264429"))?;
    if object.get("type").and_then(Value::as_str) != Some("command") {
        bail!(summon_i18n::format!("r8476d69e19"));
    }
    object
        .get("command")
        .and_then(Value::as_str)
        .filter(|command| !command.trim().is_empty())
        .context(summon_i18n::text!("rd5bff95cb3"))
}

fn wrapped_status_line(previous: Option<&Value>, wrapper: &str) -> Result<Value> {
    let mut object = match previous {
        Some(value) => {
            validate_status_line_command(value)?;
            value
                .as_object()
                .cloned()
                .context(summon_i18n::text!("r45ab0d210d"))?
        }
        None => Map::new(),
    };
    object.insert("type".into(), Value::String("command".into()));
    object.insert("command".into(), Value::String(wrapper.to_string()));
    Ok(Value::Object(object))
}

fn set_status_line(settings: &mut Value, status_line: Value) -> Result<()> {
    settings_object_mut(settings)?.insert("statusLine".into(), status_line);
    Ok(())
}

fn restore_status_line(settings: &mut Value, previous: Option<Value>) -> Result<()> {
    let object = settings_object_mut(settings)?;
    match previous {
        Some(value) => {
            object.insert("statusLine".into(), value);
        }
        None => {
            object.remove("statusLine");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn spec() -> InstallSpec {
        InstallSpec {
            status_line_wrapper_command: "'/opt/summon' daemon claude-statusline".into(),
        }
    }

    fn paths(tmp: &tempfile::TempDir) -> (std::path::PathBuf, std::path::PathBuf) {
        (
            tmp.path().join("settings.json"),
            tmp.path().join("state.json"),
        )
    }

    fn read_value(path: &Path) -> Value {
        serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
    }

    #[test]
    fn install_is_idempotent_and_preserves_existing_settings() {
        let tmp = tempfile::tempdir().unwrap();
        let (settings, state) = paths(&tmp);
        let original_status = json!({
            "type": "command",
            "command": "statusline-tool --json",
            "padding": 0,
            "futureField": true,
        });
        std::fs::write(
            &settings,
            serde_json::to_vec_pretty(&json!({
                "hooks": {
                    "SessionEnd": [{"hooks": [{"type": "command", "command": "archive"}]}]
                },
                "statusLine": original_status,
                "theme": "dark",
            }))
            .unwrap(),
        )
        .unwrap();

        assert_eq!(
            install(&settings, &state, &spec()).unwrap(),
            IntegrationStatus::Installed
        );
        assert_eq!(
            install(&settings, &state, &spec()).unwrap(),
            IntegrationStatus::Installed
        );
        let installed = read_value(&settings);
        assert_eq!(installed["theme"], "dark");
        assert_eq!(
            installed["hooks"]["SessionEnd"][0]["hooks"][0]["command"],
            "archive"
        );
        assert_eq!(installed["statusLine"]["padding"], 0);
        assert_eq!(installed["statusLine"]["futureField"], true);
        assert_eq!(
            read_install_state(&state).unwrap().delegate_command(),
            Some("statusline-tool --json")
        );
    }

    #[test]
    fn uninstall_restores_exact_status_line_and_keeps_other_hooks() {
        let tmp = tempfile::tempdir().unwrap();
        let (settings, state) = paths(&tmp);
        let original = json!({
            "hooks": {"SessionEnd": [{"hooks": [{"type": "command", "command": "archive"}]}]},
            "statusLine": {"type": "command", "command": "draw", "padding": 7},
        });
        std::fs::write(&settings, serde_json::to_vec(&original).unwrap()).unwrap();
        install(&settings, &state, &spec()).unwrap();

        let outcome = uninstall(&settings, &state).unwrap();
        assert_eq!(
            outcome,
            UninstallOutcome {
                status_line_restored: true,
                status_line_drifted: false,
            }
        );
        assert_eq!(read_value(&settings), original);
        assert!(!state.exists());
    }

    #[test]
    fn uninstall_never_overwrites_external_status_line_change() {
        let tmp = tempfile::tempdir().unwrap();
        let (settings, state) = paths(&tmp);
        std::fs::write(
            &settings,
            serde_json::to_vec(&json!({
                "statusLine": {"type": "command", "command": "old"}
            }))
            .unwrap(),
        )
        .unwrap();
        install(&settings, &state, &spec()).unwrap();
        let mut changed = read_value(&settings);
        changed["statusLine"]["command"] = Value::String("new-owner-command".into());
        std::fs::write(&settings, serde_json::to_vec(&changed).unwrap()).unwrap();

        let outcome = uninstall(&settings, &state).unwrap();
        assert!(outcome.status_line_drifted);
        assert_eq!(
            read_value(&settings)["statusLine"]["command"],
            "new-owner-command"
        );
    }

    #[test]
    fn corrupt_settings_fail_closed() {
        let tmp = tempfile::tempdir().unwrap();
        let (settings, state) = paths(&tmp);
        std::fs::write(&settings, b"{broken").unwrap();
        assert!(install(&settings, &state, &spec()).is_err());
        assert_eq!(std::fs::read(&settings).unwrap(), b"{broken");
        assert!(!state.exists());
    }

    #[test]
    fn status_line_snapshot_round_trips_complete_windows() {
        let input = json!({
            "session_id": "sid-2",
            "transcript_path": "/tmp/sid-2.jsonl",
            "workspace": {"current_dir": "/repo"},
            "rate_limits": {
                "five_hour": {"used_percentage": 100.0, "resets_at": 1783855800},
                "seven_day": {"used_percentage": 67.0, "resets_at": 1784185200}
            }
        });
        let mut snapshot =
            parse_status_line_snapshot(&serde_json::to_vec(&input).unwrap()).unwrap();
        assert_eq!(snapshot.cwd.as_deref(), Some("/repo"));
        assert_eq!(snapshot.five_hour.as_ref().unwrap().used_percentage, 100.0);

        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("snapshots");
        snapshot.observed_at = Some(1_783_855_000);
        write_snapshot(&path, &snapshot).unwrap();
        assert_eq!(read_snapshot(&path, "sid-2").unwrap(), snapshot);
    }

    /// **真机 fixture**(2026-08-07 探针:`claude --settings` 把 statusLine 换成 `cat` 抓下来的
    /// 原样 stdin,只删了与本测试无关的字段)。角标要的三项都在这一份 JSON 里 —— 这也是为什么
    /// 不必另开数据源:wrapper 每次刷新都收到它。
    #[test]
    fn status_line_snapshot_carries_badge_fields() {
        let input = json!({
            "session_id": "sid-badge",
            "transcript_path": "/tmp/sid-badge.jsonl",
            "cwd": "/repo",
            "effort": {"level": "xhigh"},
            "model": {"id": "claude-opus-5", "display_name": "Opus 5"},
            "context_window": {
                "context_window_size": 1_000_000,
                "used_percentage": 5,
                "remaining_percentage": 95
            },
            "rate_limits": {
                "five_hour": {"used_percentage": 1.0, "resets_at": 1786097400},
                "seven_day": {"used_percentage": 14.0, "resets_at": 1786604400}
            }
        });
        let snap = parse_status_line_snapshot(&serde_json::to_vec(&input).unwrap()).unwrap();
        assert_eq!(snap.model_display_name.as_deref(), Some("Opus 5"));
        assert_eq!(snap.effort_level.as_deref(), Some("xhigh"));
        assert_eq!(snap.context_used_percentage, Some(5.0));
        assert_eq!(snap.seven_day.as_ref().unwrap().used_percentage, 14.0);
    }

    /// 冷启那一拍 claude 给 `used_percentage: null`(实测),不能因此解析失败或渲成 0%。
    #[test]
    fn cold_start_null_context_usage_is_absent_not_zero() {
        let input = json!({
            "session_id": "sid-cold",
            "model": {"display_name": "Opus 5"},
            "context_window": {"context_window_size": 1_000_000, "used_percentage": null}
        });
        let snap = parse_status_line_snapshot(&serde_json::to_vec(&input).unwrap()).unwrap();
        assert_eq!(snap.context_used_percentage, None);
        assert_eq!(snap.model_display_name.as_deref(), Some("Opus 5"));
    }

    /// 越界/非有限的百分比宁可缺项也不渲荒唐数字。
    #[test]
    fn out_of_range_context_usage_is_dropped() {
        for bad in [-1.0, 100.5, f64::NAN, f64::INFINITY] {
            let input = json!({
                "session_id": "sid-bad",
                "context_window": {"used_percentage": bad}
            });
            let snap = parse_status_line_snapshot(&serde_json::to_vec(&input).unwrap_or_default())
                .unwrap();
            assert_eq!(snap.context_used_percentage, None, "{bad} 应被丢弃");
        }
    }

    /// 旧快照(没有这三个字段)必须仍能读进来 —— 升级前落的盘不能让读取整个失败。
    #[test]
    fn legacy_snapshot_without_badge_fields_still_reads() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("snapshots");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("sid-legacy.json"),
            br#"{"session_id":"sid-legacy","transcript_path":null,"cwd":null,"five_hour":null,"seven_day":null}"#,
        )
        .unwrap();
        let snap = read_snapshot(&root, "sid-legacy").unwrap();
        assert_eq!(snap.session_id, "sid-legacy");
        assert_eq!(snap.model_display_name, None);
        assert!(
            snap.observed_at.is_some(),
            "没盖戳的快照以文件 mtime 为落盘时刻"
        );
    }

    /// summon 起的会话**不渲染** statusLine(不调 delegate),但快照照写不误 —— 采集与显示是
    /// 两件事,关掉显示不能把数据源一起关掉。
    #[test]
    fn managed_session_delegates_only_when_opted_in_and_always_writes_the_snapshot() {
        let tmp = tempfile::tempdir().unwrap();
        let (settings, state) = paths(&tmp);
        std::fs::create_dir_all(settings.parent().unwrap()).unwrap();
        std::fs::write(
            &settings,
            serde_json::to_vec(&json!({
                "statusLine": {"type": "command", "command": "cat"}
            }))
            .unwrap(),
        )
        .unwrap();
        install(&settings, &state, &spec()).unwrap();
        let snapshot = tmp.path().join("snapshots");
        let input = br#"{"session_id":"managed-sid","model":{"display_name":"Opus 5"}}"#;
        let before = unix_now_secs();

        // 受管 + 开关关(出厂):不返回任何 delegate 输出。
        assert!(
            bridge_status_line(&state, &snapshot, input, true, false, false)
                .unwrap()
                .is_none()
        );
        // 但采集照常,并盖上落盘时刻。
        let written = read_snapshot(&snapshot, "managed-sid").unwrap();
        assert_eq!(written.model_display_name.as_deref(), Some("Opus 5"));
        assert!(written.observed_at.is_some_and(|at| at >= before));

        // 受管 + 开关开:原命令照常跑、输出照常转发 —— 有的 statusLine 同时在往统计平台透传
        // 数据,不跑就断了(owner 2026-09-01)。
        let managed_on = bridge_status_line(&state, &snapshot, input, true, true, false)
            .unwrap()
            .expect("开关打开后受管会话也要委派");
        assert_eq!(managed_on.stdout, input);

        // 非受管(用户自己开的 claude):delegate 照常跑、输出照常转发,与开关无关。
        for delegate_in_managed in [false, true] {
            let out =
                bridge_status_line(&state, &snapshot, input, false, delegate_in_managed, false)
                    .unwrap()
                    .unwrap();
            assert_eq!(out.stdout, input);
        }
    }

    #[test]
    fn saved_status_line_receives_exact_same_stdin() {
        let tmp = tempfile::tempdir().unwrap();
        let (settings, state) = paths(&tmp);
        std::fs::write(
            &settings,
            serde_json::to_vec(&json!({
                "statusLine": {"type": "command", "command": "cat"}
            }))
            .unwrap(),
        )
        .unwrap();
        install(&settings, &state, &spec()).unwrap();
        let input = br#"{"session_id":"same-bytes"}"#;
        let snapshot = tmp.path().join("snapshots");
        let output = bridge_status_line(&state, &snapshot, input, false, false, false)
            .unwrap()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, input);
        assert_eq!(
            read_snapshot(&snapshot, "same-bytes").unwrap().session_id,
            "same-bytes"
        );
    }

    #[test]
    fn concurrent_sessions_keep_independent_status_snapshots() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("snapshots");
        for session_id in ["session-a", "session-b"] {
            write_snapshot(
                &root,
                &ClaudeQuotaSnapshot {
                    session_id: session_id.into(),
                    transcript_path: None,
                    cwd: None,
                    five_hour: None,
                    seven_day: None,
                    model_display_name: None,
                    effort_level: None,
                    context_used_percentage: None,
                    context_window_size: None,
                    observed_at: None,
                },
            )
            .unwrap();
        }
        assert_eq!(
            read_snapshot(&root, "session-a").unwrap().session_id,
            "session-a"
        );
        assert_eq!(
            read_snapshot(&root, "session-b").unwrap().session_id,
            "session-b"
        );
    }

    #[test]
    fn install_spec_quotes_executable_path() {
        let spec = install_spec_for_exe(Path::new("/tmp/a b/it's summon")).unwrap();
        assert_eq!(
            spec.status_line_wrapper_command,
            "'/tmp/a b/it'\"'\"'s summon' daemon claude-quota-statusline"
        );
    }

    #[cfg(unix)]
    #[test]
    fn stable_entrypoint_treats_deployed_symlink_as_the_same_executable() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let executable = tmp.path().join(".summon/bin/summon");
        let stable = tmp.path().join(".local/bin/summon");
        std::fs::create_dir_all(executable.parent().unwrap()).unwrap();
        std::fs::create_dir_all(stable.parent().unwrap()).unwrap();
        std::fs::write(&executable, b"summon").unwrap();
        symlink(&executable, &stable).unwrap();

        assert_eq!(stable_entrypoint_for_exe(tmp.path(), &executable), stable);
    }

    #[cfg(unix)]
    #[test]
    fn ensure_entrypoint_is_idempotent_across_cli_symlink_and_daemon_target() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let executable = tmp.path().join(".summon/bin/summon");
        let stable = tmp.path().join(".local/bin/summon");
        std::fs::create_dir_all(executable.parent().unwrap()).unwrap();
        std::fs::create_dir_all(stable.parent().unwrap()).unwrap();
        std::fs::create_dir_all(tmp.path().join(".claude")).unwrap();
        std::fs::write(&executable, b"summon").unwrap();
        symlink(&executable, &stable).unwrap();

        let paths = IntegrationPaths::at_home(tmp.path());
        let stable_spec = install_spec_for_exe(&stable).unwrap();
        assert_eq!(
            ensure_installed(&paths, &stable_spec).unwrap(),
            IntegrationStatus::Installed
        );
        assert_eq!(
            ensure_exe_installed_at(tmp.path(), &executable).unwrap(),
            IntegrationStatus::Installed
        );
    }

    #[cfg(unix)]
    #[test]
    fn ensure_entrypoint_rejects_a_stable_symlink_to_another_executable() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let executable = tmp.path().join("target/debug/summon");
        let other_executable = tmp.path().join(".summon/bin/summon");
        let stable = tmp.path().join(".local/bin/summon");
        std::fs::create_dir_all(executable.parent().unwrap()).unwrap();
        std::fs::create_dir_all(other_executable.parent().unwrap()).unwrap();
        std::fs::create_dir_all(stable.parent().unwrap()).unwrap();
        std::fs::create_dir_all(tmp.path().join(".claude")).unwrap();
        std::fs::write(&executable, b"current").unwrap();
        std::fs::write(&other_executable, b"other").unwrap();
        symlink(&other_executable, &stable).unwrap();

        let paths = IntegrationPaths::at_home(tmp.path());
        let current_spec = install_spec_for_exe(&executable).unwrap();
        assert_eq!(
            ensure_installed(&paths, &current_spec).unwrap(),
            IntegrationStatus::Installed
        );
        assert_eq!(
            ensure_exe_installed_at(tmp.path(), &executable).unwrap(),
            IntegrationStatus::Installed
        );
    }

    #[test]
    fn stable_entrypoint_keeps_unrelated_executable() {
        let tmp = tempfile::tempdir().unwrap();
        let executable = tmp.path().join("target/debug/summon");
        std::fs::create_dir_all(executable.parent().unwrap()).unwrap();
        std::fs::write(&executable, b"summon").unwrap();

        assert_eq!(
            stable_entrypoint_for_exe(tmp.path(), &executable),
            executable
        );
    }

    #[test]
    fn ensure_installed_refuses_to_repair_drift() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = IntegrationPaths {
            settings: tmp.path().join("settings.json"),
            state: tmp.path().join("state.json"),
            snapshot: tmp.path().join("snapshot.json"),
        };
        ensure_installed(&paths, &spec()).unwrap();
        let mut changed = read_value(&paths.settings);
        changed["statusLine"]["command"] = Value::String("external-owner".into());
        std::fs::write(&paths.settings, serde_json::to_vec(&changed).unwrap()).unwrap();

        let error = ensure_installed(&paths, &spec()).unwrap_err().to_string();
        assert!(error.contains("配置漂移"));
        assert_eq!(
            read_value(&paths.settings)["statusLine"]["command"],
            "external-owner"
        );
    }

    /// 整份 settings 被重置时 statusLine 与其它键一起消失：按安装状态原样重建，原命令照旧链在后面。
    #[test]
    fn ensure_installed_rebuilds_a_missing_status_line_with_its_saved_delegate() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = IntegrationPaths {
            settings: tmp.path().join("settings.json"),
            state: tmp.path().join("state.json"),
            snapshot: tmp.path().join("snapshot.json"),
        };
        std::fs::write(
            &paths.settings,
            serde_json::to_vec(&json!({
                "statusLine": {"type": "command", "command": "user-statusline", "padding": 0}
            }))
            .unwrap(),
        )
        .unwrap();
        ensure_installed(&paths, &spec()).unwrap();
        let installed = read_value(&paths.settings)["statusLine"].clone();

        std::fs::write(
            &paths.settings,
            serde_json::to_vec(&json!({"theme": "dark"})).unwrap(),
        )
        .unwrap();
        assert_eq!(
            status(&paths.settings, &paths.state).unwrap(),
            IntegrationStatus::Missing
        );

        assert_eq!(
            ensure_installed(&paths, &spec()).unwrap(),
            IntegrationStatus::Installed
        );
        let rebuilt = read_value(&paths.settings);
        assert_eq!(rebuilt["statusLine"], installed);
        assert_eq!(rebuilt["theme"], "dark");
        assert_eq!(
            read_install_state(&paths.state).unwrap().delegate_command(),
            Some("user-statusline")
        );
    }

    /// 运行期复查只修「安装状态在、statusLine 整个没了」：别的值、uninstall 之后、settings 还没被
    /// Claude Code 建出来，一律不写。
    #[test]
    fn runtime_restore_only_rebuilds_a_missing_status_line() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let executable = home.join("bin/summon");
        std::fs::create_dir_all(executable.parent().unwrap()).unwrap();
        std::fs::create_dir_all(home.join(".claude")).unwrap();
        std::fs::write(&executable, b"summon").unwrap();
        let target = install_target_for_exe(home, &executable).unwrap();
        let paths = &target.paths;
        ensure_installed(paths, &target.spec).unwrap();
        let installed = read_value(&paths.settings)["statusLine"].clone();

        assert!(!restore_if_missing(&target).unwrap());

        std::fs::write(&paths.settings, b"{}").unwrap();
        assert!(restore_if_missing(&target).unwrap());
        assert_eq!(read_value(&paths.settings)["statusLine"], installed);

        let external = json!({"statusLine": {"type": "command", "command": "external-owner"}});
        std::fs::write(&paths.settings, serde_json::to_vec(&external).unwrap()).unwrap();
        assert!(!restore_if_missing(&target).unwrap());
        assert_eq!(read_value(&paths.settings), external);

        std::fs::remove_file(&paths.settings).unwrap();
        assert!(!restore_if_missing(&target).unwrap());
        assert!(!paths.settings.exists());

        std::fs::write(
            &paths.settings,
            serde_json::to_vec(&json!({"statusLine": installed})).unwrap(),
        )
        .unwrap();
        uninstall(&paths.settings, &paths.state).unwrap();
        std::fs::write(&paths.settings, b"{}").unwrap();
        assert!(!restore_if_missing(&target).unwrap());
        assert_eq!(read_value(&paths.settings), json!({}));
    }

    /// 启动入口与运行期复查同一条规则：settings.json 还没被 Claude Code 建出来时不写，
    /// 连 `~/.claude` 目录都不造。
    #[test]
    fn ensure_installed_waits_for_claude_code_to_create_settings() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        std::fs::create_dir_all(home.join(".claude")).unwrap();
        let paths = IntegrationPaths::at_home(home);
        ensure_installed(&paths, &spec()).unwrap();

        std::fs::remove_dir_all(home.join(".claude")).unwrap();
        assert_eq!(
            ensure_installed(&paths, &spec()).unwrap(),
            IntegrationStatus::Missing
        );
        assert!(!home.join(".claude").exists());
    }

    #[test]
    fn explicit_install_reports_missing_settings_instead_of_success() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let executable = home.join("bin/summon");
        std::fs::create_dir_all(executable.parent().unwrap()).unwrap();
        std::fs::create_dir_all(home.join(".claude")).unwrap();
        std::fs::write(&executable, b"summon").unwrap();
        let target = install_target_for_exe(home, &executable).unwrap();
        install_target(&target).unwrap();

        std::fs::remove_dir_all(home.join(".claude")).unwrap();
        let error = install_target(&target).unwrap_err().to_string();
        assert!(error.contains("还不存在"), "{error}");
        assert!(!home.join(".claude").exists());

        std::fs::create_dir_all(home.join(".claude")).unwrap();
        std::fs::write(&target.paths.settings, b"{}").unwrap();
        install_target(&target).unwrap();
        assert_eq!(
            status(&target.paths.settings, &target.paths.state).unwrap(),
            IntegrationStatus::Installed
        );
    }

    #[test]
    fn ensure_installed_purges_legacy_stop_failure_hook() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = IntegrationPaths {
            settings: tmp.path().join("settings.json"),
            state: tmp.path().join("state.json"),
            snapshot: tmp.path().join("snapshot.json"),
        };
        std::fs::write(
            &paths.settings,
            serde_json::to_vec_pretty(&json!({
                "hooks": {
                    "StopFailure": [
                        {
                            "matcher": "rate_limit",
                            "hooks": [{
                                "type": "command",
                                "command": "/opt/summon",
                                "args": ["daemon", "claude-quota-hook"],
                                "timeout": 5
                            }]
                        }
                    ],
                    "SessionEnd": [
                        {"hooks": [{"type": "command", "command": "archive"}]}
                    ]
                },
                "statusLine": {"type": "command", "command": "draw"}
            }))
            .unwrap(),
        )
        .unwrap();

        ensure_installed(&paths, &spec()).unwrap();

        let settings = read_value(&paths.settings);
        assert!(
            settings["hooks"].get("StopFailure").is_none(),
            "legacy StopFailure hook 必须被清掉"
        );
        assert_eq!(
            settings["hooks"]["SessionEnd"][0]["hooks"][0]["command"], "archive",
            "不相关的 hook 不能动"
        );
        assert_eq!(
            settings["statusLine"]["command"],
            spec().status_line_wrapper_command,
            "NotInstalled 时正常装 wrapper"
        );
    }

    #[test]
    fn purge_legacy_hook_is_idempotent_and_preserves_unrelated_stop_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let settings_path = tmp.path().join("settings.json");
        std::fs::write(
            &settings_path,
            serde_json::to_vec_pretty(&json!({
                "hooks": {
                    "StopFailure": [
                        {
                            "matcher": "rate_limit",
                            "hooks": [{
                                "type": "command",
                                "command": "/opt/summon",
                                "args": ["daemon", "claude-quota-hook"],
                                "timeout": 5
                            }]
                        },
                        {
                            "matcher": "",
                            "hooks": [{"type": "command", "command": "other-tool"}]
                        }
                    ]
                }
            }))
            .unwrap(),
        )
        .unwrap();

        // 第一次清掉 legacy 组，保留不相关的组
        purge_legacy_stop_failure_hook(&settings_path).unwrap();
        let after_first = read_value(&settings_path);
        let groups = after_first["hooks"]["StopFailure"].as_array().unwrap();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0]["hooks"][0]["command"], "other-tool");

        // 第二次幂等：不再改动
        let before = std::fs::read_to_string(&settings_path).unwrap();
        purge_legacy_stop_failure_hook(&settings_path).unwrap();
        let after = std::fs::read_to_string(&settings_path).unwrap();
        assert_eq!(before, after, "没有 legacy hook 时不应写 settings");
    }

    #[test]
    fn internal_routing_detects_only_non_anthropic_endpoints() {
        // 内部端点:各种 host/端口/路径/查询形态。
        assert!(is_internal_routing_url("https://relay.example.org"));
        assert!(is_internal_routing_url("http://relay.internal:8080/proxy"));
        assert!(is_internal_routing_url("https://relay.internal?x=1#f"));
        // 官方端点:大小写、FQDN 尾点、带子路径、apex 域,都必须识别为官方。
        assert!(!is_internal_routing_url("https://API.ANTHROPIC.COM/v1"));
        assert!(!is_internal_routing_url("https://api.anthropic.com."));
        assert!(!is_internal_routing_url("https://api.anthropic.com/"));
        assert!(!is_internal_routing_url("https://anthropic.com"));
        // 标签边界:notanthropic.com 不是 anthropic.com 的子域,按内部处理而不是后缀碰撞放过。
        assert!(is_internal_routing_url("https://notanthropic.com"));
        assert!(!is_internal_routing_url(""));
        assert!(!is_internal_routing_url("   "));
    }

    /// 三项 token 各自不同的非零值,防止某项漏加时测试仍绿。返回 (行内容, 期望总和)。
    fn transcript_line3(input: u64, cache_read: u64, cache_write: u64) -> (String, u64) {
        (
            serde_json::to_string(&json!({
                "type": "assistant",
                "message": {"usage": {
                    "input_tokens": input,
                    "cache_read_input_tokens": cache_read,
                    "cache_creation_input_tokens": cache_write,
                }},
            }))
            .unwrap(),
            input + cache_read + cache_write,
        )
    }

    #[test]
    fn transcript_usage_takes_last_assistant_record_and_sums_all_three_fields() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("t.jsonl");
        let (old_line, old_total) = transcript_line3(1_000, 80_000, 19_000);
        let (new_line, new_total) = transcript_line3(2_000, 130_000, 8_000);
        assert_ne!(old_total, new_total);
        std::fs::write(
            &path,
            format!(
                "{}\n{}\n{}\n",
                serde_json::to_string(&json!({"type": "user"})).unwrap(),
                old_line,
                new_line,
            ),
        )
        .unwrap();
        assert_eq!(last_transcript_context_usage(&path), Some(new_total));
        assert_ne!(
            last_transcript_context_usage(&path),
            Some(new_total - 8_000),
            "cache_creation_input_tokens 必须计入"
        );
    }

    #[test]
    fn transcript_usage_skips_a_half_written_line_at_the_very_tail() {
        // 真实并发形态:wrapper 在 Claude Code 追加写一行到一半时读到文件,半行就在最尾。
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("t.jsonl");
        let (line, total) = transcript_line3(5_000, 90_000, 5_000);
        let half = "{\"type\":\"assistant\",\"message\":{\"usa";
        std::fs::write(&path, format!("{line}\n{half}")).unwrap();
        assert_eq!(last_transcript_context_usage(&path), Some(total));
    }

    #[test]
    fn transcript_usage_discards_half_line_at_tail_window_boundary() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("t.jsonl");
        // 噪声前缀超过 2MB 首读窗口,起点落在噪声行中间(下一字节不是换行),验证半行被丢弃。
        let noise = "x".repeat(3_000_000);
        let (line, total) = transcript_line3(10_000, 30_000, 10_000);
        std::fs::write(&path, format!("{noise}\n{line}\n")).unwrap();
        assert_eq!(last_transcript_context_usage(&path), Some(total));
    }

    fn compact_boundary_line(post: u64) -> String {
        serde_json::to_string(&json!({
            "type": "system",
            "subtype": "compact_boundary",
            "compactMetadata": {"trigger": "auto", "preTokens": 220_000, "postTokens": post},
        }))
        .unwrap()
    }

    fn compact_summary_line() -> String {
        // 真实形态:摘要挂在 user 消息里,顶层带 isCompactSummary。
        serde_json::to_string(&json!({
            "type": "user",
            "isCompactSummary": true,
            "message": {"role": "user", "content": "This session is being continued..."},
        }))
        .unwrap()
    }

    #[test]
    fn transcript_usage_uses_post_tokens_while_post_compact_turn_is_in_flight() {
        // 压缩已完成但压缩后首轮 assistant 还没落盘:旧 assistant(220k)在更旧方向,
        // 绝不能取它 —— 只能取 compact_boundary 的 postTokens。
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("t.jsonl");
        let (old_line, _) = transcript_line3(10_000, 205_000, 5_000);
        std::fs::write(
            &path,
            format!(
                "{old_line}\n{boundary}\n{summary}\n",
                boundary = compact_boundary_line(7_423),
                summary = compact_summary_line(),
            ),
        )
        .unwrap();
        assert_eq!(last_transcript_context_usage(&path), Some(7_423));
    }

    #[test]
    fn transcript_usage_prefers_new_assistant_over_pre_compact_history() {
        // 压缩后首轮已落盘:取新轮 usage,不能越过摘要行去取压缩前的 220k。
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("t.jsonl");
        let (old_line, _) = transcript_line3(10_000, 205_000, 5_000);
        let (new_line, new_total) = transcript_line3(20_000, 25_000, 1_000);
        std::fs::write(
            &path,
            format!(
                "{old_line}\n{boundary}\n{summary}\n{new_line}\n",
                boundary = compact_boundary_line(7_423),
                summary = compact_summary_line(),
                new_line = new_line,
            ),
        )
        .unwrap();
        assert_eq!(last_transcript_context_usage(&path), Some(new_total));
    }

    fn snapshot_with(
        transcript: Option<&str>,
        pct: Option<f64>,
        window: Option<u64>,
    ) -> ClaudeQuotaSnapshot {
        ClaudeQuotaSnapshot {
            session_id: "s1".into(),
            transcript_path: transcript.map(|s| s.to_string()),
            cwd: None,
            five_hour: None,
            seven_day: None,
            model_display_name: Some("ark/seed-evolving".into()),
            effort_level: None,
            context_used_percentage: pct,
            context_window_size: window,
            observed_at: None,
        }
    }

    #[test]
    fn enrich_only_fills_missing_or_zero_and_never_touches_healthy_or_official() {
        let tmp = tempfile::tempdir().unwrap();
        let transcript = tmp.path().join("t.jsonl");
        let (line, total) = transcript_line3(50_000, 90_000, 10_000);
        std::fs::write(&transcript, format!("{line}\n")).unwrap();
        let tp = transcript.to_string_lossy();
        let expected = total as f64 * 100.0 / 1_000_000.0;

        // 内部 + 上游钉 0:反算。
        let mut snap = snapshot_with(Some(&tp), Some(0.0), Some(1_000_000));
        enrich_context_usage(&mut snap, true);
        assert_eq!(snap.context_used_percentage, Some(expected));

        // 内部 + 上游缺值:同样反算。
        let mut snap = snapshot_with(Some(&tp), None, Some(1_000_000));
        enrich_context_usage(&mut snap, true);
        assert_eq!(snap.context_used_percentage, Some(expected));

        // 内部 + 上游健康非零:保留上游值,不用转录盖。
        let mut snap = snapshot_with(Some(&tp), Some(42.0), Some(1_000_000));
        enrich_context_usage(&mut snap, true);
        assert_eq!(snap.context_used_percentage, Some(42.0));

        // 官方端点 + 上游 0:逐字节保留。
        let mut snap = snapshot_with(Some(&tp), Some(0.0), Some(1_000_000));
        enrich_context_usage(&mut snap, false);
        assert_eq!(snap.context_used_percentage, Some(0.0));

        // 内部但转录读不到 / 窗口未知:保留上游值,不发明数字。
        let mut snap = snapshot_with(Some("/no/such/file"), Some(0.0), Some(1_000_000));
        enrich_context_usage(&mut snap, true);
        assert_eq!(snap.context_used_percentage, Some(0.0));
        let mut snap = snapshot_with(Some(&tp), Some(0.0), None);
        enrich_context_usage(&mut snap, true);
        assert_eq!(snap.context_used_percentage, Some(0.0));

        // 越界钳到 100,不能渲出荒唐百分比。
        let big = tmp.path().join("big.jsonl");
        let (big_line, _) = transcript_line3(500_000, 2_000_000, 0);
        std::fs::write(&big, format!("{big_line}\n")).unwrap();
        let mut snap = snapshot_with(Some(&big.to_string_lossy()), Some(0.0), Some(1_000_000));
        enrich_context_usage(&mut snap, true);
        assert_eq!(snap.context_used_percentage, Some(100.0));
    }

    #[test]
    fn transcript_usage_returns_none_when_boundary_is_half_written_past_summary() {
        // 摘要行已完整落盘、边界行还在写一半(两行真实相隔仅百毫秒级):没有闸门就会倒读越过
        // 摘要取回压缩前的 220k 旧值;闸门存在时必须返回 None,保留上游值。
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("t.jsonl");
        let (old_line, _) = transcript_line3(10_000, 205_000, 5_000);
        let half_boundary = "{\"type\":\"system\",\"subtype\":\"compact_boundary\",\"compactMeta";
        std::fs::write(
            &path,
            format!(
                "{old_line}\n{half}\n{summary}\n",
                half = half_boundary,
                summary = compact_summary_line(),
            ),
        )
        .unwrap();
        assert_eq!(last_transcript_context_usage(&path), None);
    }

    #[test]
    fn transcript_usage_expands_tail_window_when_latest_assistant_line_is_huge() {
        // 最近一条 assistant 行本身超过 2MB 首读窗口:首读被腰斩返回 None,必须扩到 8MB 命中。
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("big.jsonl");
        let padding = "z".repeat(2_100_000);
        // 行总长约 2.15MB,且文件整体大于 2MB,保证首读起点落在该行中间。
        let big_line = serde_json::to_string(&json!({
            "type": "assistant",
            "message": {"usage": {
                "input_tokens": 30_000,
                "cache_read_input_tokens": 2_000_000,
                "cache_creation_input_tokens": 70_000,
            }},
            "padding": padding,
        }))
        .unwrap();
        assert!(big_line.len() > 2 * 1024 * 1024);
        std::fs::write(&path, format!("{big_line}\n")).unwrap();
        assert_eq!(
            last_transcript_context_usage(&path),
            Some(2_100_000),
            "删除扩窗分支此断言必失败"
        );
    }

    #[test]
    fn tail_window_discards_unterminated_last_segment_and_keeps_terminated_one() {
        // 末段无尾换行时无法与正在写的半行区分,策略是丢弃(下次刷新再取);有尾换行则必须保留。
        // 直接钉 read_tail_usage 这一层,避开 8MB 扩窗的掩盖。
        let tmp = tempfile::tempdir().unwrap();
        let (line, total) = transcript_line3(10_000, 30_000, 10_000);
        // 前缀保证 start>0 且起点落在前缀中间,排除起点边界因素。
        let prefix = "p".repeat(10_000);

        let no_newline = tmp.path().join("no_nl.jsonl");
        std::fs::write(&no_newline, format!("{prefix}\n{line}")).unwrap();
        assert_eq!(read_tail_usage(&no_newline, 2 * 1024 * 1024), None);

        let with_newline = tmp.path().join("with_nl.jsonl");
        std::fs::write(&with_newline, format!("{prefix}\n{line}\n")).unwrap();
        assert_eq!(read_tail_usage(&with_newline, 2 * 1024 * 1024), Some(total));
    }

    #[test]
    fn tail_window_starting_exactly_on_newline_keeps_the_following_line() {
        // 窗口起点恰好落在 '\n' 上:紧随其后的完整行不能丢。buf = "\n<line>\n",
        // 倒序首段是空串(尾换行),丢弃策略不碰真实记录。
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("boundary.jsonl");
        let target_line_len = 2 * 1024 * 1024 - 2;
        let mut line = serde_json::to_string(&json!({
            "type": "assistant",
            "message": {"usage": {
                "input_tokens": 10_000,
                "cache_read_input_tokens": 30_000,
                "cache_creation_input_tokens": 10_000,
            }},
            "pad": "",
        }))
        .unwrap();
        let pad_need = target_line_len - line.len();
        line = line.replace(
            "\"pad\":\"\"",
            &format!("\"pad\":\"{}\"", "q".repeat(pad_need)),
        );
        let prefix = "p".repeat(100);
        let content = format!("{prefix}\n{line}\n");
        assert_eq!(content.as_bytes()[content.len() - 2 * 1024 * 1024], b'\n');
        std::fs::write(&path, &content).unwrap();
        assert_eq!(read_tail_usage(&path, 2 * 1024 * 1024), Some(50_000));
    }

    #[test]
    fn bridge_overrides_stale_zero_end_to_end_only_for_internal_routing() {
        // 接线测试:env 判定在 CLI 层,这里直接验证 bridge 的 internal 开关贯通到落盘快照。
        let tmp = tempfile::tempdir().unwrap();
        let transcript = tmp.path().join("t.jsonl");
        let (line, _) = transcript_line3(50_000, 90_000, 10_000);
        std::fs::write(&transcript, format!("{line}\n")).unwrap();
        let snapshot_dir = tmp.path().join("snapshots");
        let state = tmp.path().join("state.json"); // managed + 不 delegate 时不会读它

        let payload = json!({
            "session_id": "sess-e2e",
            "transcript_path": transcript.to_string_lossy(),
            "context_window": {"used_percentage": 0.0, "context_window_size": 1_000_000},
        })
        .to_string();

        // 内部路由:落盘快照应被反算为 15%。
        bridge_status_line(&state, &snapshot_dir, payload.as_bytes(), true, false, true).unwrap();
        let written = read_value(&snapshot_dir.join("sess-e2e.json"));
        assert_eq!(written["context_used_percentage"], json!(15.0));

        // 官方路由:同一 payload,落盘快照保持 0.0。
        bridge_status_line(
            &state,
            &snapshot_dir,
            payload.as_bytes(),
            true,
            false,
            false,
        )
        .unwrap();
        let written = read_value(&snapshot_dir.join("sess-e2e.json"));
        assert_eq!(written["context_used_percentage"], json!(0.0));
    }
}
