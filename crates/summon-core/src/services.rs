//! 通用前置依赖服务（Managed Prerequisite Services）配置模型与静态校验。
//!
//! 设计目标：适用于任何需要前置常驻服务（本地模型代理、MCP 服务、内网隧道等）的 agent。
//! 偏好由 launchd / systemd 等外部服务管理器主托管；summon 侧重于探活、环境变量自动注入
//! 以及探活失败时的按需拉起兜底。进程生命周期与探针执行位于应用层 `runtime::services`。

use crate::config::agent_identity::is_runtime_owned_env_key;
use crate::session::launch_env::is_env_name;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::Duration;

pub const DEFAULT_PROBE_TIMEOUT_SECS: u64 = 2;
pub const MIN_PROBE_TIMEOUT_SECS: u64 = 1;
pub const MAX_PROBE_TIMEOUT_SECS: u64 = 30;

pub const DEFAULT_READY_TIMEOUT_SECS: u64 = 15;
pub const MIN_READY_TIMEOUT_SECS: u64 = 1;
pub const MAX_READY_TIMEOUT_SECS: u64 = 120;

fn default_probe_timeout_secs() -> u64 {
    DEFAULT_PROBE_TIMEOUT_SECS
}

fn default_ready_timeout_secs() -> u64 {
    DEFAULT_READY_TIMEOUT_SECS
}

fn default_true() -> bool {
    true
}

/// 服务健康探测配置：必须在 `http`、`tcp`、`command` 三者中恰好指定一项。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServiceHealthcheck {
    /// HTTP GET 探测 URL（2xx 视为健康，不跟随重定向）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub http: Option<String>,
    /// TCP 连通性探测地址（`host:port` 或 `[ipv6]:port`）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tcp: Option<String>,
    /// 自定义探测命令（argv 列表，不经过 shell；退出码 0 视为健康）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<Vec<String>>,
    /// 单次探测超时（秒），默认 2，有效范围 1..=30。
    #[serde(default = "default_probe_timeout_secs")]
    pub timeout_secs: u64,
    /// 按需拉起后等待就绪的总超时（秒），默认 15，有效范围 1..=120。
    #[serde(default = "default_ready_timeout_secs")]
    pub ready_timeout_secs: u64,
}

impl Default for ServiceHealthcheck {
    fn default() -> Self {
        Self {
            http: None,
            tcp: None,
            command: None,
            timeout_secs: DEFAULT_PROBE_TIMEOUT_SECS,
            ready_timeout_secs: DEFAULT_READY_TIMEOUT_SECS,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HealthcheckKind<'a> {
    Http(&'a str),
    Tcp(&'a str),
    Command(&'a [String]),
}

impl ServiceHealthcheck {
    pub fn timeout(&self) -> Duration {
        Duration::from_secs(
            self.timeout_secs
                .clamp(MIN_PROBE_TIMEOUT_SECS, MAX_PROBE_TIMEOUT_SECS),
        )
    }

    pub fn ready_timeout(&self) -> Duration {
        Duration::from_secs(
            self.ready_timeout_secs
                .clamp(MIN_READY_TIMEOUT_SECS, MAX_READY_TIMEOUT_SECS),
        )
    }

    /// 恰好配置了一种合法探测形态时返回该形态，否则返回 `None`。
    pub fn kind(&self) -> Option<HealthcheckKind<'_>> {
        let count = usize::from(self.http.is_some())
            + usize::from(self.tcp.is_some())
            + usize::from(self.command.is_some());
        if count != 1 {
            return None;
        }
        if let Some(url) = self.http.as_deref() {
            return Some(HealthcheckKind::Http(url.trim()));
        }
        if let Some(addr) = self.tcp.as_deref() {
            return Some(HealthcheckKind::Tcp(addr.trim()));
        }
        self.command
            .as_deref()
            .filter(|cmd| !cmd.is_empty())
            .map(HealthcheckKind::Command)
    }
}

/// 单个前置依赖服务定义（`config.yaml` 中 `services.<name>`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServiceSpec {
    /// 健康探测定义（必填）。
    pub healthcheck: ServiceHealthcheck,
    /// 可选的按需兜底启动命令（argv 列表，不经 shell 解析；留空表示纯探活，不自动拉起）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<Vec<String>>,
    /// 启动命令与 command 探针的工作目录（支持 `~/`）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub working_dir: Option<String>,
    /// 注入给服务自身进程的额外环境变量（支持从全局 `~/.summon/.env` 与进程环境展开 `${VAR}`）。
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub env: HashMap<String, String>,
    /// 自动注入给通过 `runtime.requires` 声明依赖此服务的 agent 的环境变量。
    /// 优先级低于 agent 自己的 `runtime.env`，且禁止覆盖运行时保留键。
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub agent_env: HashMap<String, String>,
    /// 由 summon 按需拉起的服务进程是否在 daemon 重启/停机时继续存活（默认 true）。
    /// 遵循 daemon 重启不影响外部会话的红线：默认脱离 daemon 进程组（setsid）。
    #[serde(default = "default_true")]
    pub survive_daemon_restart: bool,
    /// `command` 是否为一次性交接启动命令（如 `launchctl kickstart` / `systemctl --user start`，默认 false）。
    /// 为 true 时 summon 不写自管租约，必须同时确认命令以 0 退出且探活转绿才视为就绪，并按外部托管处理。
    #[serde(default)]
    pub oneshot: bool,
}

impl Default for ServiceSpec {
    fn default() -> Self {
        Self {
            healthcheck: ServiceHealthcheck::default(),
            command: None,
            working_dir: None,
            env: HashMap::new(),
            agent_env: HashMap::new(),
            survive_daemon_restart: true,
            oneshot: false,
        }
    }
}

impl ServiceSpec {
    /// 是否配置了按需兜底启动命令。
    pub fn start_command(&self) -> Option<&[String]> {
        self.command.as_deref().filter(|cmd| !cmd.is_empty())
    }

    /// 静态校验服务名与字段语义。空 Vec 表示合法。
    pub fn validate(&self, name: &str) -> Vec<String> {
        let mut issues = Vec::new();
        if !is_valid_service_name(name) {
            issues.push(summon_i18n::text!("services.name_invalid").to_string());
        }

        let probe_count = usize::from(self.healthcheck.http.is_some())
            + usize::from(self.healthcheck.tcp.is_some())
            + usize::from(self.healthcheck.command.is_some());
        if probe_count == 0 {
            issues.push(summon_i18n::text!("services.healthcheck_missing").to_string());
        } else if probe_count > 1 {
            issues.push(summon_i18n::text!("services.healthcheck_multiple").to_string());
        } else {
            if let Some(url) = self.healthcheck.http.as_deref() {
                if !is_valid_http_url(url) {
                    issues
                        .push(summon_i18n::text!("services.healthcheck_http_invalid").to_string());
                }
            }
            if let Some(addr) = self.healthcheck.tcp.as_deref() {
                if parse_tcp_host_port(addr).is_none() {
                    issues.push(summon_i18n::text!("services.healthcheck_tcp_invalid").to_string());
                }
            }
            if let Some(cmd) = self.healthcheck.command.as_deref() {
                if cmd.first().is_none_or(|p| p.trim().is_empty()) {
                    issues.push(summon_i18n::text!("services.healthcheck_cmd_empty").to_string());
                }
            }
        }

        if let Some(cmd) = self.start_command() {
            if cmd.first().is_none_or(|p| p.trim().is_empty()) {
                issues.push(summon_i18n::text!("services.command_empty_program").to_string());
            }
        }

        let mut env_keys: Vec<&str> = self.env.keys().map(String::as_str).collect();
        env_keys.sort_unstable();
        for key in env_keys {
            if !is_env_name(key) {
                issues.push(summon_i18n::format!("services.env_invalid_name", key = key));
            }
        }

        let mut agent_env_keys: Vec<&str> = self.agent_env.keys().map(String::as_str).collect();
        agent_env_keys.sort_unstable();
        for key in agent_env_keys {
            if !is_env_name(key) {
                issues.push(summon_i18n::format!(
                    "services.agent_env_invalid_name",
                    key = key
                ));
            } else if is_runtime_owned_env_key(key) {
                issues.push(summon_i18n::format!(
                    "services.agent_env_runtime_owned",
                    key = key
                ));
            }
        }

        issues
    }
}

/// 服务名校验：仅允许 1..=48 字节的 ASCII 字母、数字、`-`、`_`。
/// 服务名直接用于 `~/.summon/run/services/<name>.{json,log,lock}` 文件名，
/// 严格收窄字符集防止路径穿越与特殊字符注入。
pub fn is_valid_service_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 48
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn is_valid_http_url(raw: &str) -> bool {
    let trimmed = raw.trim();
    let rest = trimmed
        .strip_prefix("http://")
        .or_else(|| trimmed.strip_prefix("https://"));
    match rest {
        Some(after) => {
            let host = after.split('/').next().unwrap_or("");
            !host.is_empty() && !trimmed.chars().any(char::is_whitespace)
        }
        None => false,
    }
}

/// 解析 `host:port` 或 `[ipv6]:port`，要求 host 非空且端口为 1..=65535。
pub fn parse_tcp_host_port(raw: &str) -> Option<(&str, u16)> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.chars().any(char::is_whitespace) {
        return None;
    }
    let (host, port_str) = if let Some(rest) = trimmed.strip_prefix('[') {
        let (h, tail) = rest.split_once("]:")?;
        (h, tail)
    } else {
        let (h, p) = trimmed.rsplit_once(':')?;
        if h.contains(':') {
            return None;
        }
        (h, p)
    };
    if host.is_empty() {
        return None;
    }
    let port: u16 = port_str.parse().ok()?;
    if port == 0 {
        return None;
    }
    Some((host, port))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_name_rejects_traversal_and_spaces() {
        for bad in [
            "",
            "../proxy",
            "a/b",
            "my proxy",
            ".hidden",
            &"a".repeat(49),
        ] {
            assert!(!is_valid_service_name(bad), "should reject {bad:?}");
        }
        for good in ["claude-cursor-proxy", "mcp_local_1", "A1"] {
            assert!(is_valid_service_name(good), "should accept {good:?}");
        }
    }

    #[test]
    fn parse_tcp_handles_ipv4_hostname_and_ipv6() {
        assert_eq!(
            parse_tcp_host_port("127.0.0.1:8080"),
            Some(("127.0.0.1", 8080))
        );
        assert_eq!(
            parse_tcp_host_port("localhost:3000"),
            Some(("localhost", 3000))
        );
        assert_eq!(parse_tcp_host_port("[::1]:8080"), Some(("::1", 8080)));
        assert_eq!(parse_tcp_host_port("127.0.0.1:0"), None);
        assert_eq!(parse_tcp_host_port(":8080"), None);
        assert_eq!(parse_tcp_host_port("127.0.0.1"), None);
        assert_eq!(parse_tcp_host_port("::1:8080"), None);
    }

    #[test]
    fn spec_yaml_defaults_and_unknown_fields_are_strict() {
        let spec: ServiceSpec = serde_yaml::from_str(
            "healthcheck:\n  http: http://127.0.0.1:8080/health\nagent_env:\n  ANTHROPIC_BASE_URL: http://127.0.0.1:8080\n",
        )
        .unwrap();
        assert!(spec.survive_daemon_restart);
        assert_eq!(spec.healthcheck.timeout(), Duration::from_secs(2));
        assert_eq!(spec.healthcheck.ready_timeout(), Duration::from_secs(15));
        assert!(spec.start_command().is_none());
        assert!(spec.validate("claude-cursor-proxy").is_empty());

        let unknown = "healthcheck:\n  http: http://127.0.0.1:8080\nbogus: 1\n";
        assert!(serde_yaml::from_str::<ServiceSpec>(unknown).is_err());

        let unknown_hc = "healthcheck:\n  http: http://127.0.0.1:8080\n  bogus: 1\n";
        assert!(serde_yaml::from_str::<ServiceSpec>(unknown_hc).is_err());
    }

    #[test]
    fn validate_flags_missing_multiple_and_runtime_owned_agent_env() {
        let empty = ServiceSpec::default();
        assert!(!empty.validate("svc").is_empty());

        let multi: ServiceSpec = serde_yaml::from_str(
            "healthcheck:\n  http: http://127.0.0.1:8080\n  tcp: 127.0.0.1:8080\n",
        )
        .unwrap();
        assert!(!multi.validate("svc").is_empty());

        let bad_env: ServiceSpec = serde_yaml::from_str(
            "healthcheck:\n  tcp: 127.0.0.1:8080\nagent_env:\n  SUMMON_AGENT: evil\n  BAD-KEY: x\n",
        )
        .unwrap();
        let issues = bad_env.validate("svc");
        assert_eq!(issues.len(), 2, "{issues:?}");
    }

    #[test]
    fn strict_config_load_never_leaks_scalar_values_on_parse_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        let bad_yaml = "services:\n  synth:\n    healthcheck:\n      tcp: 127.0.0.1:9999\n    survive_daemon_restart: synth-config-secret-4242\n";
        std::fs::write(&path, bad_yaml).unwrap();

        let err_en = crate::i18n::with_language(crate::i18n::Language::English, || {
            crate::config::Config::load_strict_at(&path).unwrap_err()
        });
        assert!(
            !err_en.contains("synth-config-secret-4242"),
            "strict load (en) must not leak scalar values: {err_en}"
        );
        assert!(
            err_en.contains("line 5"),
            "en error should include line: {err_en}"
        );

        let err_zh = crate::i18n::with_language(crate::i18n::Language::Chinese, || {
            crate::config::Config::load_strict_at(&path).unwrap_err()
        });
        assert!(
            !err_zh.contains("synth-config-secret-4242"),
            "strict load (zh) must not leak scalar values: {err_zh}"
        );
        assert!(
            err_zh.contains("第 5 行"),
            "zh error should include localized line: {err_zh}"
        );

        let (_, fallback_err_en) =
            crate::i18n::with_language(crate::i18n::Language::English, || {
                crate::config::parse_or_fallback(bad_yaml, crate::config::Config::default())
            });
        let fallback_err_en = fallback_err_en.expect("parse_or_fallback must report error");
        assert!(
            !fallback_err_en.contains("synth-config-secret-4242"),
            "tolerant load must not leak scalar values: {fallback_err_en}"
        );
        assert!(fallback_err_en.contains("line 5"), "{fallback_err_en}");
    }
}
