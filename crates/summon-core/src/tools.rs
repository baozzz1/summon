//! 运行时外部可执行文件的单一解析入口。
//!
//! 普通运行路径读取进程启动时安装的内存快照，不在 spawn 热路径反复读 `config.yaml`；需要热语义
//! 的 sentinel 显式把每 tick 已加载的 [`Config`] 传给 [`resolve_from`]。配置解析失败仍由
//! `Config::load` 的 last-good 处理；单个显式路径非法则只让该工具落到不可执行哨兵路径。

use crate::config::{Config, Tools};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::{OnceLock, RwLock};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Tool {
    Tmux,
    LarkCli,
    Claude,
    Codex,
    Traex,
    CursorAgent,
    Git,
    Ssh,
    Npx,
    Pkill,
    Open,
    XdgOpen,
    Launchctl,
    Systemctl,
    Loginctl,
    Pbcopy,
    Osascript,
}

impl Tool {
    pub const ALL: [Self; 17] = [
        Self::Tmux,
        Self::LarkCli,
        Self::Claude,
        Self::Codex,
        Self::Traex,
        Self::CursorAgent,
        Self::Git,
        Self::Ssh,
        Self::Npx,
        Self::Pkill,
        Self::Open,
        Self::XdgOpen,
        Self::Launchctl,
        Self::Systemctl,
        Self::Loginctl,
        Self::Pbcopy,
        Self::Osascript,
    ];

    pub fn key(self) -> &'static str {
        match self {
            Self::Tmux => "tmux",
            Self::LarkCli => "lark_cli",
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Traex => "traex",
            Self::CursorAgent => "cursor_agent",
            Self::Git => "git",
            Self::Ssh => "ssh",
            Self::Npx => "npx",
            Self::Pkill => "pkill",
            Self::Open => "open",
            Self::XdgOpen => "xdg_open",
            Self::Launchctl => "launchctl",
            Self::Systemctl => "systemctl",
            Self::Loginctl => "loginctl",
            Self::Pbcopy => "pbcopy",
            Self::Osascript => "osascript",
        }
    }

    /// PATH 上的命令名。配置键用下划线,命令名里是连字符的要单独列出。
    pub fn command(self) -> &'static str {
        match self {
            Self::LarkCli => "lark-cli",
            Self::CursorAgent => "cursor-agent",
            Self::XdgOpen => "xdg-open",
            _ => self.key(),
        }
    }

    pub fn for_cli_kind(kind: &str) -> Option<Self> {
        match kind {
            "claude" => Some(Self::Claude),
            "codex" => Some(Self::Codex),
            "cursor" | "agent" => Some(Self::CursorAgent),
            _ => None,
        }
    }

    pub fn for_driver(driver: crate::driver::CliDriver) -> Option<Self> {
        match driver {
            crate::driver::CliDriver::Claude => Some(Self::Claude),
            crate::driver::CliDriver::Codex => Some(Self::Codex),
            // traex 是独立二进制(不是 codex),可用性检查必须查 traex 本身。
            crate::driver::CliDriver::Traex => Some(Self::Traex),
            crate::driver::CliDriver::Cursor => Some(Self::CursorAgent),
            crate::driver::CliDriver::Other => None,
        }
    }

    /// doctor 默认展示的运行时主依赖；平台 UI 工具只在对应系统出现，避免 Linux 因 launchctl
    /// 天然不存在而永远无法全绿。
    pub fn doctor_tools() -> Vec<Self> {
        let mut tools = vec![
            Self::Tmux,
            Self::LarkCli,
            Self::Claude,
            Self::Codex,
            Self::CursorAgent,
            Self::Git,
            Self::Ssh,
            Self::Npx,
            Self::Pkill,
        ];
        if cfg!(target_os = "macos") {
            tools.extend([Self::Open, Self::Launchctl, Self::Pbcopy, Self::Osascript]);
        } else if cfg!(target_os = "linux") {
            // systemctl/loginctl 是开机自启(systemd --user)那条路的依赖:前者装/起服务,
            // 后者开 linger(没有它,user service 只在有登录会话时活着,ssh 一断就停)。
            tools.extend([Self::XdgOpen, Self::Systemctl, Self::Loginctl]);
        }
        tools
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolSource {
    Tools,
    LegacyTmux,
    LegacyLarkCli,
    Path,
}

impl ToolSource {
    pub fn label(self) -> &'static str {
        match self {
            Self::Tools => "tools",
            Self::LegacyTmux => summon_i18n::text!("r06debf6907"),
            Self::LegacyLarkCli => summon_i18n::text!("rd248d7e3ad"),
            Self::Path => "PATH",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolInspection {
    pub tool: Tool,
    pub requested: PathBuf,
    pub actual: Option<PathBuf>,
    pub source: ToolSource,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Default)]
struct ResolverConfig {
    tools: Tools,
    legacy_tmux: Option<PathBuf>,
    legacy_lark_cli: Option<PathBuf>,
}

impl From<&Config> for ResolverConfig {
    fn from(config: &Config) -> Self {
        Self {
            tools: config.tools.clone(),
            legacy_tmux: config.tmux_bin.clone(),
            legacy_lark_cli: config.sentinel.lark_cli_path.as_deref().map(PathBuf::from),
        }
    }
}

fn active() -> &'static RwLock<ResolverConfig> {
    static ACTIVE: OnceLock<RwLock<ResolverConfig>> = OnceLock::new();
    ACTIVE.get_or_init(|| RwLock::new(ResolverConfig::default()))
}

/// 安装本进程的工具路径快照。CLI 在统一 `Config::load` 后调用一次；daemon 后续每次 spawn 只
/// 读这把内存锁。重复调用是确定性的覆盖，便于 daemon 启动入口与嵌入式调用各自保险。
pub fn configure(config: &Config) {
    if let Ok(mut current) = active().write() {
        *current = ResolverConfig::from(config);
    }
}

/// 解析普通运行路径使用的进程级快照。锁中毒时退回裸名；daemon PATH 已先净化，退回不会重新
/// 命中 summon 自己的 lark-cli wrapper。
pub fn resolve(tool: Tool) -> PathBuf {
    active()
        .read()
        .ok()
        .map(|config| resolve_from_snapshot(&config, tool))
        .unwrap_or_else(|| PathBuf::from(tool.command()))
}

/// 用调用方已经加载的 config 解析。sentinel 每 tick 用它保住 `tools.lark_cli` 与旧别名热生效，
/// 不让 resolver 自己再 `Config::load()` 扫盘。
pub fn resolve_from(config: &Config, tool: Tool) -> PathBuf {
    resolve_from_snapshot(&ResolverConfig::from(config), tool)
}

pub fn inspect(config: &Config, tool: Tool, path: &OsStr) -> ToolInspection {
    let snapshot = ResolverConfig::from(config);
    let selected = select(&snapshot, tool);
    let mut warnings = selected.warnings;
    let requested = match selected.path {
        Some(path) => match expand_explicit(&path) {
            Some(path) => path,
            None => {
                warnings.push(summon_i18n::format!("ra4d338e597", tool.key()));
                invalid_path(tool)
            }
        },
        None => PathBuf::from(tool.command()),
    };
    let actual = find_executable(&requested, path);
    ToolInspection {
        tool,
        requested,
        actual,
        source: selected.source,
        warnings,
    }
}

pub fn find_executable(command: &Path, path: &OsStr) -> Option<PathBuf> {
    if command.is_absolute() {
        return is_executable(command).then(|| command.to_path_buf());
    }
    // 带目录成分的相对路径不是 PATH 裸名；配置层会把它变成 invalid 哨兵，这里同样不猜。
    if command.components().count() != 1 {
        return None;
    }
    std::env::split_paths(path)
        .map(|dir| dir.join(command))
        .find(|candidate| is_executable(candidate))
}

pub fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// 在不经 serde 重写整份人工配置的前提下，合并 `tools:` 路径。`overwrite=false` 用于 onboard
/// 只补初值；`true` 用于显式的 doctor 修复命令。仅接受普通 block mapping，遇到 inline mapping
/// fail-closed，避免一个便利命令意外毁掉配置注释或结构。
pub fn patch_tools_yaml(
    raw: &str,
    entries: &[(Tool, PathBuf)],
    overwrite: bool,
) -> Result<String, String> {
    if entries.is_empty() {
        return Ok(raw.to_string());
    }
    let had_trailing_newline = raw.ends_with('\n');
    let mut lines: Vec<String> = raw.lines().map(str::to_string).collect();
    let tools_start = lines
        .iter()
        .position(|line| !line.starts_with([' ', '\t']) && line.trim_end() == "tools:");

    if tools_start.is_none()
        && lines.iter().any(|line| {
            !line.starts_with([' ', '\t'])
                && line
                    .trim_start()
                    .strip_prefix("tools:")
                    .is_some_and(|rest| !rest.trim().is_empty() && !rest.trim().starts_with('#'))
        })
    {
        return Err(summon_i18n::text!("rab8da066c8").to_string());
    }

    let start = if let Some(start) = tools_start {
        start
    } else {
        if lines.last().is_some_and(|line| !line.is_empty()) {
            lines.push(String::new());
        }
        lines.push("tools:".to_string());
        lines.len() - 1
    };
    let mut end = lines.len();
    for (index, line) in lines.iter().enumerate().skip(start + 1) {
        let trimmed = line.trim();
        if !trimmed.is_empty() && !trimmed.starts_with('#') && !line.starts_with([' ', '\t']) {
            end = index;
            break;
        }
    }

    let mut additions = Vec::new();
    for (tool, path) in entries {
        let key = tool.key();
        let rendered = yaml_single_quote(&path.to_string_lossy());
        let existing = (start + 1..end).find(|index| {
            let line = lines[*index].trim_start();
            !line.starts_with('#') && line.split_once(':').is_some_and(|(found, _)| found == key)
        });
        match existing {
            Some(index) if overwrite => lines[index] = format!("  {key}: {rendered}"),
            Some(_) => {}
            None => additions.push(format!("  {key}: {rendered}")),
        }
    }
    if !additions.is_empty() {
        lines.splice(end..end, additions);
    }
    let mut out = lines.join("\n");
    if had_trailing_newline || !out.is_empty() {
        out.push('\n');
    }
    Ok(out)
}

fn yaml_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn resolve_from_snapshot(config: &ResolverConfig, tool: Tool) -> PathBuf {
    match select(config, tool).path {
        Some(path) => expand_explicit(&path).unwrap_or_else(|| invalid_path(tool)),
        None => PathBuf::from(tool.command()),
    }
}

struct Selected {
    path: Option<PathBuf>,
    source: ToolSource,
    warnings: Vec<String>,
}

fn select(config: &ResolverConfig, tool: Tool) -> Selected {
    let current = configured(&config.tools, tool).filter(|path| !path.as_os_str().is_empty());
    let legacy = match tool {
        Tool::Tmux => config.legacy_tmux.clone(),
        Tool::LarkCli => config.legacy_lark_cli.clone(),
        _ => None,
    }
    .filter(|path| !path.as_os_str().is_empty());
    let mut warnings = Vec::new();
    if let (Some(current), Some(legacy)) = (&current, &legacy) {
        if current != legacy {
            warnings.push(summon_i18n::format!("r826334a648", tool.key(), tool.key()));
        } else {
            warnings.push(summon_i18n::format!("rceef30724b", tool.key()));
        }
    }
    if let Some(path) = current {
        return Selected {
            path: Some(path),
            source: ToolSource::Tools,
            warnings,
        };
    }
    if let Some(path) = legacy {
        warnings.push(summon_i18n::format!(
            "r35fc9b6447",
            match tool {
                Tool::Tmux => " tmux_bin",
                Tool::LarkCli => " sentinel.lark_cli_path",
                _ => summon_i18n::text!("r5ac130134b"),
            },
            tool.key()
        ));
        let source = match tool {
            Tool::Tmux => ToolSource::LegacyTmux,
            Tool::LarkCli => ToolSource::LegacyLarkCli,
            _ => ToolSource::Path,
        };
        return Selected {
            path: Some(path),
            source,
            warnings,
        };
    }
    Selected {
        path: None,
        source: ToolSource::Path,
        warnings,
    }
}

fn configured(tools: &Tools, tool: Tool) -> Option<PathBuf> {
    match tool {
        Tool::Tmux => tools.tmux.clone(),
        Tool::LarkCli => tools.lark_cli.clone(),
        Tool::Claude => tools.claude.clone(),
        Tool::Codex => tools.codex.clone(),
        Tool::Traex => tools.traex.clone(),
        Tool::CursorAgent => tools.cursor_agent.clone(),
        Tool::Git => tools.git.clone(),
        Tool::Ssh => tools.ssh.clone(),
        Tool::Npx => tools.npx.clone(),
        Tool::Pkill => tools.pkill.clone(),
        Tool::Open => tools.open.clone(),
        Tool::XdgOpen => tools.xdg_open.clone(),
        Tool::Launchctl => tools.launchctl.clone(),
        Tool::Systemctl => tools.systemctl.clone(),
        Tool::Loginctl => tools.loginctl.clone(),
        Tool::Pbcopy => tools.pbcopy.clone(),
        Tool::Osascript => tools.osascript.clone(),
    }
}

fn expand_explicit(path: &Path) -> Option<PathBuf> {
    if path.is_absolute() {
        return Some(path.to_path_buf());
    }
    let value = path.to_str()?;
    if value == "~" {
        return dirs::home_dir();
    }
    value
        .strip_prefix("~/")
        .and_then(|rest| dirs::home_dir().map(|home| home.join(rest)))
}

fn invalid_path(tool: Tool) -> PathBuf {
    Path::new("/__summon_invalid_tool_path__").join(tool.key())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 没配 `tools.*` 时按命令名在 PATH 上找:配置键 `lark_cli` / `cursor_agent` 不是命令名。
    #[test]
    fn unconfigured_tools_resolve_to_their_real_command_names() {
        let empty = ResolverConfig::default();
        assert_eq!(
            resolve_from_snapshot(&empty, Tool::LarkCli),
            PathBuf::from("lark-cli")
        );
        assert_eq!(
            resolve_from_snapshot(&empty, Tool::CursorAgent),
            PathBuf::from("cursor-agent")
        );
        assert_eq!(
            resolve_from_snapshot(&empty, Tool::XdgOpen),
            PathBuf::from("xdg-open")
        );
        assert_eq!(
            resolve_from_snapshot(&empty, Tool::Claude),
            PathBuf::from("claude")
        );
    }

    use std::os::unix::fs::PermissionsExt;

    fn executable(path: &Path) {
        std::fs::write(path, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn tools_value_wins_over_conflicting_legacy_alias() {
        let mut config = Config::default();
        config.tools.tmux = Some("/new/tmux".into());
        config.tmux_bin = Some("/old/tmux".into());
        let inspected = inspect(&config, Tool::Tmux, OsStr::new("/usr/bin"));
        assert_eq!(
            resolve_from(&config, Tool::Tmux),
            PathBuf::from("/new/tmux")
        );
        assert_eq!(inspected.source, ToolSource::Tools);
        assert!(inspected
            .warnings
            .iter()
            .any(|warning| warning.contains("冲突")));
    }

    #[test]
    fn legacy_alias_remains_effective_and_requests_migration() {
        let mut config = Config::default();
        config.sentinel.lark_cli_path = Some("/legacy/lark-cli".into());
        let inspected = inspect(&config, Tool::LarkCli, OsStr::new("/usr/bin"));
        assert_eq!(
            resolve_from(&config, Tool::LarkCli),
            PathBuf::from("/legacy/lark-cli")
        );
        assert_eq!(inspected.source, ToolSource::LegacyLarkCli);
        assert!(inspected
            .warnings
            .iter()
            .any(|warning| warning.contains("迁移")));
    }

    /// 迁移**完成**态:只写 `tools.lark_cli`、旧字段已删。此时不该再有任何迁移提示——否则
    /// 用户照着提示改无可改。2026-08-10 迁移本机配置时发现 `sentinel status` 在这个状态下
    /// 反而显示「<daemon PATH>」(它裸读旧字段),而 lark-cli 没钉死正是全盲事故的根因。
    #[test]
    fn migrated_tools_entry_resolves_without_migration_warning() {
        let mut config = Config::default();
        config.tools.lark_cli = Some("/migrated/lark-cli".into());
        config.sentinel.lark_cli_path = None;
        let inspected = inspect(&config, Tool::LarkCli, OsStr::new("/usr/bin"));
        assert_eq!(
            resolve_from(&config, Tool::LarkCli),
            PathBuf::from("/migrated/lark-cli")
        );
        assert_eq!(inspected.source, ToolSource::Tools);
        assert!(!inspected
            .warnings
            .iter()
            .any(|warning| warning.contains("迁移") || warning.contains("冗余")));
    }

    #[test]
    fn invalid_relative_explicit_path_fails_only_that_tool_closed() {
        let mut config = Config::default();
        config.tools.codex = Some("relative/codex".into());
        assert_eq!(
            resolve_from(&config, Tool::Codex),
            PathBuf::from("/__summon_invalid_tool_path__/codex")
        );
        assert_eq!(resolve_from(&config, Tool::Git), PathBuf::from("git"));
    }

    #[test]
    fn bare_fallback_reports_the_actual_executable_on_path() {
        let dir = std::env::temp_dir().join(format!(
            "summon-tools-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let bin = dir.join("codex");
        executable(&bin);
        let inspected = inspect(&Config::default(), Tool::Codex, dir.as_os_str());
        assert_eq!(inspected.requested, PathBuf::from("codex"));
        assert_eq!(inspected.actual, Some(bin));
        assert_eq!(inspected.source, ToolSource::Path);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn tools_yaml_patch_preserves_comments_and_is_idempotent() {
        let raw =
            "# keep\ntools:\n  codex: '/old/codex' # keep value\n\nsentinel:\n  enabled: true\n";
        let entries = vec![
            (Tool::Codex, PathBuf::from("/new/codex")),
            (Tool::Git, PathBuf::from("/usr/bin/git")),
        ];
        let once = patch_tools_yaml(raw, &entries, false).unwrap();
        assert!(once.contains("  codex: '/old/codex' # keep value"));
        assert!(once.contains("  git: '/usr/bin/git'"));
        assert!(once.contains("sentinel:\n  enabled: true"));
        assert_eq!(patch_tools_yaml(&once, &entries, false).unwrap(), once);

        let replaced = patch_tools_yaml(&once, &entries[..1], true).unwrap();
        assert!(replaced.contains("  codex: '/new/codex'"));
        assert!(!replaced.contains("'/old/codex'"));
    }

    #[test]
    fn tools_yaml_patch_rejects_inline_mapping() {
        assert!(patch_tools_yaml(
            "tools: { codex: /bin/codex }\n",
            &[(Tool::Git, "/usr/bin/git".into())],
            false
        )
        .is_err());
    }
}
