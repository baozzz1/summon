//! 走 CLI 通道(`runtime.capture: cli`)的 Cursor 参数化模型：参数目录与每轮 `--model` 的合成。
//!
//! `cursor-agent models` 只列平铺 id,没有参数化基座模型，也没有参数轴;`-p --model` 却接受
//! `base[k=v,...]`。cursor-agent 2026.11.22 真机验证的规则：
//! - 括号里必须是该模型的**全部**参数轴，并按目录顺序；缺一个轴或换个顺序都被拒(`Cannot use this model`)。
//! - 不带括号的 base id 也能跑，但参数取 CLI 配置根里上一次留下的值，不确定。
//!
//! 所以轴与合法取值只能来自真实目录，而目录只有 ACP 给：后台一次性起 `cursor-agent acp`(独立临时
//! 配置根),initialize + `session/new` 拿模型列表，再对每个模型 `session/set_config_option model=<id>`
//! 读它的参数轴与默认值，不发 prompt。解析复用 [`CursorAcpClient`] 的严格解析。目录缓存在内存并
//! 落盘到 agent 状态目录，读路径只读这份缓存，不算环境、不起进程；只有探测成功才替换它。这个 agent
//! 从没探测成功过时，面板列平铺模型，每轮按平铺模型启动。
//!
//! 意图存储复用 [`super::cursor_config`] 的两层 bundle。一轮的模型按这个顺序取：本群 bundle 模型 >
//! 本群平铺模型(`threads/<tag>/model`) > 默认 bundle 模型 > agent.yaml `model`;取到 bundle 模型时
//! 拼成完整参数向量传给 `--model`,拼不出合法值就拒绝这一轮，不换成别的模型。

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use summon_core::registry::Agent;

use super::cursor_acp::{CursorAcpClient, CursorAcpConfig, CursorAcpConfigOption};
use super::cursor_config::CursorTarget;
use crate::observe::log::log;
use crate::runtime::executor::ChildKiller;

/// 落盘的参数目录，在 agent 状态目录下。
pub const CATALOG_FILE: &str = "cursor-cli-parameter-catalog.json";
const CATALOG_VERSION: u32 = 1;
const MAX_CATALOG_BYTES: u64 = 4 * 1024 * 1024;
const MAX_MODELS: usize = 512;
/// 每个 ACP 请求的上限。真机 `session/new` 约 7s、每次切模型约 1.3s;探测跑在后台线程里，给足余量。
const PROBE_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// 不是参数轴的配置项：模型是括号外的 base id,模式是 ACP 会话级状态。
const NON_PARAMETER_IDS: [&str; 2] = ["model", "mode"];

/// CLI 通道的参数目录。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CursorParameterCatalog {
    /// 参数化 base model id,server 原序。
    pub models: Vec<String>,
    /// 全新配置根下 `session/new` 报的当前模型。
    pub default_model: Option<String>,
    /// 每个模型的参数轴(不含 model / mode)。顺序就是 CLI 括号里要求的顺序;`current_value` 是该模型
    /// 在全新配置根下的默认值。没读到轴的模型不在这里。
    pub axes: BTreeMap<String, Vec<CursorAcpConfigOption>>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredCatalog {
    version: u32,
    account: String,
    catalog: CursorParameterCatalog,
}

/// 决定 cursor-agent 连哪个账号、哪个服务端的环境变量(cursor-agent 2026.11.22 的读取点):认证文件在
/// `$XDG_CONFIG_HOME/cursor` 或 `$HOME/.config/cursor`,另有令牌与服务端地址覆盖。PATH、语言、代理等
/// 与账号无关，不进账号作用域。
const ACCOUNT_ENV_KEYS: [&str; 5] = [
    "HOME",
    "XDG_CONFIG_HOME",
    "CURSOR_API_KEY",
    "CURSOR_AUTH_TOKEN",
    "CURSOR_API_ENDPOINT",
];

/// 账号作用域摘要：程序路径 + [`ACCOUNT_ENV_KEYS`] 的取值，SHA-256 前 16 字节的十六进制。
/// 跨进程、跨 Rust 版本稳定，只落摘要不落令牌原文。
pub(crate) fn account_key(program: &str, env: &[(String, String)]) -> String {
    let mut context = ring::digest::Context::new(&ring::digest::SHA256);
    let mut field = |value: &str| {
        context.update(&(value.len() as u64).to_le_bytes());
        context.update(value.as_bytes());
    };
    field(program);
    for key in ACCOUNT_ENV_KEYS {
        let value = env
            .iter()
            .rev()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.as_str());
        field(key);
        match value {
            Some(value) => field(value),
            None => field("\0unset"),
        }
    }
    context
        .finish()
        .as_ref()
        .iter()
        .take(16)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

type Store = Mutex<HashMap<PathBuf, (String, CursorParameterCatalog)>>;

fn store() -> &'static Store {
    static STORE: OnceLock<Store> = OnceLock::new();
    STORE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn catalog_path(agent: &Agent) -> PathBuf {
    agent.state_dir().join(CATALOG_FILE)
}

/// 这个 agent 最近一次探测成功的参数目录。内存没有就读盘；从未探测成功 → `None`。
///
/// 跑在 chat 线程上(面板渲染、每轮启动),所以只读缓存，不解析环境。账号作用域只在探测线程里比对：
/// 账号变了也沿用旧目录，直到新探测成功替换它 —— 目录只决定参数轴与默认值，旧目录拼出的值不合法时
/// CLI 会当场拒绝；丢掉它却会让用户存的参数悄悄失效，换成平铺模型跑。
pub fn cached(agent: &Agent) -> Option<CursorParameterCatalog> {
    stored(agent).map(|(_, catalog)| catalog)
}

fn stored(agent: &Agent) -> Option<(String, CursorParameterCatalog)> {
    let key = agent.state_dir();
    if let Some(entry) = store()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(&key)
    {
        return Some(entry.clone());
    }
    let loaded = load_from_disk(agent)?;
    store()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(key, loaded.clone());
    Some(loaded)
}

fn load_from_disk(agent: &Agent) -> Option<(String, CursorParameterCatalog)> {
    use std::io::Read as _;
    let path = catalog_path(agent);
    let metadata = std::fs::symlink_metadata(&path).ok()?;
    if !metadata.file_type().is_file() || metadata.len() > MAX_CATALOG_BYTES {
        return None;
    }
    let mut bytes = Vec::new();
    std::fs::File::open(&path)
        .ok()?
        .take(MAX_CATALOG_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    let stored: StoredCatalog = serde_json::from_slice(&bytes).ok()?;
    (stored.version == CATALOG_VERSION && catalog_is_usable(&stored.catalog))
        .then_some((stored.account, stored.catalog))
}

fn catalog_is_usable(catalog: &CursorParameterCatalog) -> bool {
    !catalog.models.is_empty() && catalog.models.len() <= MAX_MODELS
}

/// 记一份探测结果：内存立即生效，盘上原子替换。空目录不覆盖已有的。
pub(crate) fn record(agent: &Agent, account: &str, catalog: CursorParameterCatalog) {
    if !catalog_is_usable(&catalog) {
        return;
    }
    let stored = StoredCatalog {
        version: CATALOG_VERSION,
        account: account.to_string(),
        catalog,
    };
    match serde_json::to_vec_pretty(&stored) {
        Ok(bytes) if bytes.len() as u64 <= MAX_CATALOG_BYTES => {
            if let Err(error) =
                crate::runtime::fsio::atomic_write_mode(&catalog_path(agent), &bytes, 0o600)
            {
                log(&summon_i18n::format!(
                    "cursor_cli_params.store_failed",
                    agent.name,
                    error = error
                ));
            }
        }
        _ => log(&summon_i18n::format!(
            "cursor_cli_params.store_failed",
            agent.name,
            error = "catalog exceeds size budget"
        )),
    }
    store()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(agent.state_dir(), (account.to_string(), stored.catalog));
}

/// 探测并记下参数目录。`Ok(false)` 表示停机打断，不算失败。
pub(crate) fn refresh(
    agent: &Agent,
    program: &str,
    env: &[(String, String)],
    stopped: &AtomicBool,
    publish_killer: &mut dyn FnMut(Option<ChildKiller>),
) -> Result<bool, String> {
    let account = account_key(program, env);
    if stored(agent).is_some_and(|(previous, _)| previous != account) {
        log(&summon_i18n::format!(
            "cursor_cli_params.account_changed",
            agent.name
        ));
    }
    match probe(agent, program, env, stopped, publish_killer)? {
        Some(catalog) => {
            let models = catalog.models.len();
            record(agent, &account, catalog);
            log(&summon_i18n::format!(
                "cursor_cli_params.probe_recorded",
                agent.name,
                models
            ));
            Ok(true)
        }
        None => Ok(false),
    }
}

/// 起一次性 `cursor-agent acp` 读参数目录。配置根与工作目录都是探测专用的临时目录，不碰 owner 的
/// `~/.cursor`,也不在 agent 的会话库里留下会话。
fn probe(
    agent: &Agent,
    program: &str,
    env: &[(String, String)],
    stopped: &AtomicBool,
    publish_killer: &mut dyn FnMut(Option<ChildKiller>),
) -> Result<Option<CursorParameterCatalog>, String> {
    let config_dir = super::cursor_config::probe_config_dir(agent)
        .map_err(|error| error.diagnostic().to_string())?;
    let mut client = CursorAcpClient::spawn(CursorAcpConfig {
        program: program.to_string(),
        args: CursorAcpConfig::acp_args(),
        work_dir: Some(config_dir.path().to_path_buf()),
        config_dir: config_dir.path().to_path_buf(),
        request_timeout: PROBE_REQUEST_TIMEOUT,
        env: env.to_vec(),
    })
    .map_err(|error| error.to_string())?;
    publish_killer(Some(client.killer()));
    let result = probe_with(
        &mut client,
        config_dir.path().to_str(),
        stopped,
        &agent.name,
    );
    publish_killer(None);
    drop(client);
    drop(config_dir);
    result
}

fn parameter_axes(options: &[CursorAcpConfigOption]) -> Vec<CursorAcpConfigOption> {
    options
        .iter()
        .filter(|option| !NON_PARAMETER_IDS.contains(&option.id.as_str()))
        .cloned()
        .collect()
}

fn probe_with(
    client: &mut CursorAcpClient,
    cwd: Option<&str>,
    stopped: &AtomicBool,
    agent_name: &str,
) -> Result<Option<CursorParameterCatalog>, String> {
    let session = client
        .session_new(cwd, None)
        .map_err(|error| error.to_string())?;
    let mut models: Vec<String> = Vec::new();
    for model in &session.available_models {
        if !models.contains(model) {
            models.push(model.clone());
        }
    }
    if models.is_empty() {
        return Err("session/new reported no models".into());
    }
    if models.len() > MAX_MODELS {
        return Err(format!("session/new reported {} models", models.len()));
    }
    let mut axes = BTreeMap::new();
    if let Some(current) = session
        .current_model_id
        .as_ref()
        .filter(|_| session.config_options_trusted)
    {
        axes.insert(current.clone(), parameter_axes(&session.config_options));
    }
    for model in &models {
        if stopped.load(Ordering::Acquire) {
            return Ok(None);
        }
        if axes.contains_key(model) {
            continue;
        }
        match client.set_config_option(&session.session_id, "model", model, None) {
            Ok(options) => {
                axes.insert(model.clone(), parameter_axes(&options));
            }
            Err(error) => log(&summon_i18n::format!(
                "cursor_cli_params.probe_axes_skipped",
                agent_name,
                model,
                error = error
            )),
        }
    }
    Ok(Some(CursorParameterCatalog {
        models,
        default_model: session.current_model_id,
        axes,
    }))
}

/// 本群在参数目录命名空间里的选择。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliSelection {
    pub model: String,
    /// 两层合成后用户显式设过的轴(本群覆盖叠在默认 bundle 上)。
    pub options: BTreeMap<String, String>,
}

/// 读本群的参数化选择。`Ok(None)`:没有参数化选择，按平铺模型(本群 `/model` 或 agent.yaml)跑。
pub fn selection(
    agent: &Agent,
    tag: &str,
) -> Result<Option<CliSelection>, super::cursor_config::CursorConfigError> {
    let target = super::cursor_config::load_persisted_target(agent, tag)?;
    let chat_flat = crate::runtime::chat_model::chat_model(agent, tag).is_some();
    Ok(selection_from(&target, chat_flat))
}

/// 本群 bundle 模型 > 本群平铺模型 > 默认 bundle 模型。本群平铺模型胜出时返回 `None`。
fn selection_from(target: &CursorTarget, chat_flat_override: bool) -> Option<CliSelection> {
    if target.chat.selected_model().is_none() && chat_flat_override {
        return None;
    }
    let model = target.model()?.to_string();
    Some(CliSelection {
        model,
        options: target.options(),
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ComposeError {
    UnknownModel { model: String },
    MissingAxes { model: String },
    UnsupportedOption { axis: String, value: String },
    MissingDefault { axis: String },
    UnsafeToken,
}

impl fmt::Display for ComposeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            Self::UnknownModel { model } => {
                summon_i18n::format!("cursor_cli_params.unknown_model", model = md(model))
            }
            Self::MissingAxes { model } => {
                summon_i18n::format!("cursor_cli_params.missing_axes", model = md(model))
            }
            Self::UnsupportedOption { axis, value } => summon_i18n::format!(
                "cursor_cli_params.unsupported_option",
                axis = md(axis),
                value = md(value)
            ),
            Self::MissingDefault { axis } => {
                summon_i18n::format!("cursor_cli_params.missing_default", axis = md(axis))
            }
            Self::UnsafeToken => summon_i18n::text!("cursor_cli_params.unsafe_token").to_string(),
        };
        formatter.write_str(&text)
    }
}

fn md(value: &str) -> String {
    summon_core::feishu::escape_lark_md(value)
}

fn safe_token(value: &str) -> bool {
    !value.is_empty()
        && !value
            .chars()
            .any(|c| matches!(c, '[' | ']' | ',' | '=') || c.is_whitespace() || c.is_control())
}

/// 把模型与显式设过的轴拼成 CLI `--model` 的值。
///
/// 模型有参数轴时一律拼出完整向量(未设的轴取目录默认值，按目录顺序):CLI 只认完整有序的向量，
/// 裸 base id 又会沿用 CLI 配置根里上一次的参数。没有参数轴的模型传裸 id。
pub fn compose_model_id(
    catalog: &CursorParameterCatalog,
    model: &str,
    overrides: &BTreeMap<String, String>,
) -> Result<String, ComposeError> {
    if !catalog.models.iter().any(|candidate| candidate == model) {
        return Err(ComposeError::UnknownModel {
            model: model.to_string(),
        });
    }
    if !safe_token(model) {
        return Err(ComposeError::UnsafeToken);
    }
    let Some(axes) = catalog.axes.get(model) else {
        return if overrides.is_empty() {
            Ok(model.to_string())
        } else {
            Err(ComposeError::MissingAxes {
                model: model.to_string(),
            })
        };
    };
    for (id, value) in overrides {
        let supported = axes
            .iter()
            .find(|axis| axis.id == *id)
            .is_some_and(|axis| axis.options.iter().any(|option| option.value == *value));
        if !supported {
            return Err(ComposeError::UnsupportedOption {
                axis: id.clone(),
                value: value.clone(),
            });
        }
    }
    if axes.is_empty() {
        return Ok(model.to_string());
    }
    let mut parts = Vec::with_capacity(axes.len());
    for axis in axes {
        let value = overrides
            .get(&axis.id)
            .or(axis.current_value.as_ref())
            .ok_or_else(|| ComposeError::MissingDefault {
                axis: axis.id.clone(),
            })?;
        if !safe_token(&axis.id) || !safe_token(value) {
            return Err(ComposeError::UnsafeToken);
        }
        parts.push(format!("{}={value}", axis.id));
    }
    Ok(format!("{model}[{}]", parts.join(",")))
}

/// 本轮 `--model` 的值。`Ok(None)`:没有参数目录或没有参数化选择，调用方照旧用平铺模型。
/// `Err` 是给用户看的原因：选择存在但拼不出合法值，这一轮不该启动。
pub fn model_for_turn(agent: &Agent, tag: &str) -> Result<Option<String>, String> {
    let Some(catalog) = cached(agent) else {
        return Ok(None);
    };
    model_for_turn_with(agent, tag, &catalog)
}

fn model_for_turn_with(
    agent: &Agent,
    tag: &str,
    catalog: &CursorParameterCatalog,
) -> Result<Option<String>, String> {
    let selection = selection(agent, tag).map_err(|error| {
        log(&summon_i18n::format!(
            "cursor_cli_params.selection_unreadable",
            agent.name,
            error.diagnostic()
        ));
        error.to_string()
    })?;
    let Some(selection) = selection else {
        return Ok(None);
    };
    compose_model_id(catalog, &selection.model, &selection.options)
        .map(Some)
        .map_err(|error| error.to_string())
}

/// 面板用的「当前会话目录」:`model` 项 + 该模型的参数轴，`current_value` 是下一轮会带上的值
/// (显式设过的取设定值，否则取目录默认)。形状与 ACP 会话下发的 `configOptions` 相同，面板复用同一套渲染。
pub fn session_options(
    catalog: &CursorParameterCatalog,
    model: &str,
    chosen: &BTreeMap<String, String>,
) -> Vec<CursorAcpConfigOption> {
    let mut options = vec![CursorAcpConfigOption {
        id: "model".into(),
        name: "Model".into(),
        description: None,
        category: Some("model".into()),
        option_type: Some("select".into()),
        current_value: Some(model.to_string()),
        options: catalog
            .models
            .iter()
            .map(|id| super::cursor_acp::CursorAcpConfigValue {
                value: id.clone(),
                name: id.clone(),
                description: None,
                group: None,
                group_name: None,
            })
            .collect(),
    }];
    if let Some(axes) = catalog.axes.get(model) {
        options.extend(axes.iter().cloned().map(|mut axis| {
            if let Some(value) = chosen.get(&axis.id) {
                axis.current_value = Some(value.clone());
            }
            axis
        }));
    }
    options
}

#[cfg(test)]
pub(crate) fn clear_memory_for_test(agent: &Agent) {
    store()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .remove(&agent.state_dir());
}

#[cfg(test)]
pub(crate) fn record_for_test(agent: &Agent, catalog: CursorParameterCatalog) {
    record(agent, "test-account", catalog);
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::capture::cursor_acp::CursorAcpConfigValue;
    use crate::capture::cursor_config::{self, CursorConfigLayer};

    fn axis(id: &str, values: &[&str], default: &str) -> CursorAcpConfigOption {
        CursorAcpConfigOption {
            id: id.into(),
            name: id.into(),
            description: None,
            category: None,
            option_type: Some("select".into()),
            current_value: Some(default.into()),
            options: values
                .iter()
                .map(|value| CursorAcpConfigValue {
                    value: (*value).into(),
                    name: (*value).into(),
                    description: None,
                    group: None,
                    group_name: None,
                })
                .collect(),
        }
    }

    /// 与真机目录同形：sol 三个轴(context, reasoning, fast)按这个顺序;opus 四个轴 thinking 在前;
    /// gemini-3.1-pro 没有轴;muse 的轴没读到。
    pub(crate) fn sample_catalog() -> CursorParameterCatalog {
        let mut axes = BTreeMap::new();
        axes.insert(
            "gpt-5.6-sol".to_string(),
            vec![
                axis("context", &["272k", "1m"], "272k"),
                axis(
                    "reasoning",
                    &["none", "low", "medium", "high", "xhigh"],
                    "medium",
                ),
                axis("fast", &["false", "true"], "false"),
            ],
        );
        axes.insert(
            "claude-opus-5".to_string(),
            vec![
                axis("thinking", &["false", "true"], "true"),
                axis("context", &["300k", "1m"], "300k"),
                axis("effort", &["low", "medium", "high"], "high"),
                axis("fast", &["false", "true"], "false"),
            ],
        );
        axes.insert("gemini-3.1-pro".to_string(), Vec::new());
        CursorParameterCatalog {
            models: vec![
                "auto-smart".into(),
                "gpt-5.6-sol".into(),
                "claude-opus-5".into(),
                "gemini-3.1-pro".into(),
                "muse-wonder-1.0".into(),
            ],
            default_model: Some("auto-smart".into()),
            axes,
        }
    }

    fn overrides(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    #[test]
    fn compose_emits_every_axis_in_catalog_order_with_defaults_filled_in() {
        let catalog = sample_catalog();
        assert_eq!(
            compose_model_id(
                &catalog,
                "gpt-5.6-sol",
                &overrides(&[("reasoning", "low"), ("context", "1m")])
            )
            .unwrap(),
            "gpt-5.6-sol[context=1m,reasoning=low,fast=false]"
        );
        // thinking 排在最前面:按目录顺序，不按字母序。
        assert_eq!(
            compose_model_id(&catalog, "claude-opus-5", &overrides(&[("fast", "true")])).unwrap(),
            "claude-opus-5[thinking=true,context=300k,effort=high,fast=true]"
        );
        // 没设任何轴也拼完整向量：裸 id 会沿用 CLI 配置根里上次的参数。
        assert_eq!(
            compose_model_id(&catalog, "gpt-5.6-sol", &BTreeMap::new()).unwrap(),
            "gpt-5.6-sol[context=272k,reasoning=medium,fast=false]"
        );
        assert_eq!(
            compose_model_id(&catalog, "gemini-3.1-pro", &BTreeMap::new()).unwrap(),
            "gemini-3.1-pro"
        );
    }

    #[test]
    fn compose_fails_closed_instead_of_switching_model_or_dropping_an_axis() {
        let catalog = sample_catalog();
        assert_eq!(
            compose_model_id(&catalog, "gpt-9", &BTreeMap::new()),
            Err(ComposeError::UnknownModel {
                model: "gpt-9".into()
            })
        );
        assert_eq!(
            compose_model_id(&catalog, "gpt-5.6-sol", &overrides(&[("context", "2m")])),
            Err(ComposeError::UnsupportedOption {
                axis: "context".into(),
                value: "2m".into()
            })
        );
        assert_eq!(
            compose_model_id(&catalog, "gemini-3.1-pro", &overrides(&[("fast", "true")])),
            Err(ComposeError::UnsupportedOption {
                axis: "fast".into(),
                value: "true".into()
            })
        );
        assert_eq!(
            compose_model_id(
                &catalog,
                "muse-wonder-1.0",
                &overrides(&[("context", "1m")])
            ),
            Err(ComposeError::MissingAxes {
                model: "muse-wonder-1.0".into()
            })
        );
        assert_eq!(
            compose_model_id(&catalog, "muse-wonder-1.0", &BTreeMap::new()).unwrap(),
            "muse-wonder-1.0"
        );
        let mut no_default = catalog.clone();
        no_default.axes.get_mut("gpt-5.6-sol").unwrap()[2].current_value = None;
        assert_eq!(
            compose_model_id(&no_default, "gpt-5.6-sol", &BTreeMap::new()),
            Err(ComposeError::MissingDefault {
                axis: "fast".into()
            })
        );
        let mut bracket = catalog;
        bracket.axes.get_mut("gpt-5.6-sol").unwrap()[0].current_value = Some("1m]".into());
        assert_eq!(
            compose_model_id(&bracket, "gpt-5.6-sol", &BTreeMap::new()),
            Err(ComposeError::UnsafeToken)
        );
    }

    fn cli_agent() -> (Agent, tempfile::TempDir) {
        crate::test_support::agent_dir("cursor", "  capture: cli\nmodel: auto\n")
    }

    #[test]
    fn turn_model_follows_chat_overlay_over_default_bundle() {
        let (agent, _tmp) = cli_agent();
        let catalog = sample_catalog();
        assert_eq!(
            model_for_turn_with(&agent, "g-a", &catalog),
            Ok(None),
            "没有 bundle 时保持平铺模型"
        );
        let view = cursor_config::load_layer(&agent, CursorConfigLayer::Default).unwrap();
        cursor_config::set_selected_model(&agent, view.bundle.generation, "claude-opus-5").unwrap();
        assert_eq!(
            model_for_turn_with(&agent, "g-a", &catalog)
                .unwrap()
                .as_deref(),
            Some("claude-opus-5[thinking=true,context=300k,effort=high,fast=false]")
        );
        let view =
            cursor_config::set_chat_selected_model(&agent, "g-a", 0, Some("gpt-5.6-sol")).unwrap();
        cursor_config::set_chat_model_option(
            &agent,
            "g-a",
            view.bundle.generation,
            "gpt-5.6-sol",
            "context",
            Some("1m"),
        )
        .unwrap();
        assert_eq!(
            model_for_turn_with(&agent, "g-a", &catalog)
                .unwrap()
                .as_deref(),
            Some("gpt-5.6-sol[context=1m,reasoning=medium,fast=false]")
        );
        // 别的群不受本群覆盖影响。
        assert_eq!(
            model_for_turn_with(&agent, "g-b", &catalog)
                .unwrap()
                .as_deref(),
            Some("claude-opus-5[thinking=true,context=300k,effort=high,fast=false]")
        );
    }

    #[test]
    fn chat_flat_model_beats_the_default_bundle_but_not_the_chat_bundle() {
        let (agent, _tmp) = cli_agent();
        let catalog = sample_catalog();
        let view = cursor_config::load_layer(&agent, CursorConfigLayer::Default).unwrap();
        cursor_config::set_selected_model(&agent, view.bundle.generation, "claude-opus-5").unwrap();
        crate::runtime::chat_model::set_chat_model(&agent, "g-a", Some("gpt-5.6-sol-high"))
            .unwrap();
        assert_eq!(model_for_turn_with(&agent, "g-a", &catalog), Ok(None));
        cursor_config::set_chat_selected_model(&agent, "g-a", 0, Some("gemini-3.1-pro")).unwrap();
        assert_eq!(
            model_for_turn_with(&agent, "g-a", &catalog)
                .unwrap()
                .as_deref(),
            Some("gemini-3.1-pro")
        );
    }

    #[test]
    fn turn_refuses_a_selection_the_catalog_cannot_express() {
        let (agent, _tmp) = cli_agent();
        cursor_config::set_chat_selected_model(&agent, "g-a", 0, Some("gpt-9-retired")).unwrap();
        let error = model_for_turn_with(&agent, "g-a", &sample_catalog()).unwrap_err();
        assert!(error.contains("gpt-9-retired"), "{error}");
    }

    #[test]
    fn the_acp_rpc_model_seed_is_not_a_cli_selection() {
        let (agent, _tmp) = crate::test_support::agent_dir(
            "cursor",
            "  capture: cli\n  rpc_model: \"gpt-5.6-sol[context=1m]\"\n",
        );
        assert_eq!(
            model_for_turn_with(&agent, "g-a", &sample_catalog()),
            Ok(None)
        );
    }

    #[test]
    fn without_a_catalog_the_turn_keeps_the_flat_model() {
        let (agent, _tmp) = cli_agent();
        cursor_config::set_chat_selected_model(&agent, "g-a", 0, Some("gpt-5.6-sol")).unwrap();
        clear_memory_for_test(&agent);
        assert_eq!(cached(&agent), None);
        assert_eq!(model_for_turn(&agent, "g-a"), Ok(None));
    }

    #[test]
    fn a_recorded_catalog_survives_a_restart_and_an_account_change() {
        let (agent, _tmp) = cli_agent();
        record(&agent, "old-account", sample_catalog());
        clear_memory_for_test(&agent);
        assert_eq!(cached(&agent), Some(sample_catalog()), "盘上那份要读得回来");
        assert_eq!(stored(&agent).unwrap().0, "old-account");
        // 读路径不比账号：盘上目录一直沿用到新探测成功。
        cursor_config::set_chat_selected_model(&agent, "g-a", 0, Some("gpt-5.6-sol")).unwrap();
        clear_memory_for_test(&agent);
        assert_eq!(
            model_for_turn(&agent, "g-a").unwrap().as_deref(),
            Some("gpt-5.6-sol[context=272k,reasoning=medium,fast=false]")
        );
        // 空目录不覆盖好目录。
        record(&agent, "new-account", CursorParameterCatalog::default());
        assert_eq!(cached(&agent), Some(sample_catalog()));
    }

    fn env(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    #[test]
    fn the_account_key_ignores_launch_environment_noise_but_tracks_the_account() {
        let base = env(&[
            ("PATH", "/usr/bin"),
            ("HOME", "/srv/account-a"),
            ("LANG", "C"),
            ("TMPDIR", "/tmp/a"),
        ]);
        let key = account_key("/bin/cursor-agent", &base);
        assert_eq!(key.len(), 32);
        assert_eq!(
            key,
            account_key(
                "/bin/cursor-agent",
                &env(&[
                    ("HOME", "/srv/account-a"),
                    ("PATH", "/opt/homebrew/bin:/usr/bin"),
                    ("TERM", "xterm"),
                    ("HTTPS_PROXY", "http://proxy"),
                    ("XDG_CACHE_HOME", "/c"),
                ])
            ),
            "launchd / systemd / shell 起的 daemon 只在这些变量上不同，账号是同一个"
        );
        // 固定值：跨进程、跨 Rust 版本都不能变。
        assert_eq!(key, "9ae93c8d5b00d344654efe7494e0ee2b");
        for changed in [
            account_key("/other/cursor-agent", &base),
            account_key("/bin/cursor-agent", &env(&[("HOME", "/srv/account-b")])),
            account_key(
                "/bin/cursor-agent",
                &env(&[("HOME", "/srv/account-a"), ("CURSOR_API_KEY", "k")]),
            ),
            account_key(
                "/bin/cursor-agent",
                &env(&[("HOME", "/srv/account-a"), ("XDG_CONFIG_HOME", "/cfg")]),
            ),
        ] {
            assert_ne!(changed, key);
        }
    }

    #[test]
    fn session_options_report_the_values_the_next_turn_will_send() {
        let options = session_options(
            &sample_catalog(),
            "gpt-5.6-sol",
            &overrides(&[("fast", "true")]),
        );
        let ids: Vec<_> = options.iter().map(|option| option.id.as_str()).collect();
        assert_eq!(ids, ["model", "context", "reasoning", "fast"]);
        assert_eq!(options[0].current_value.as_deref(), Some("gpt-5.6-sol"));
        assert_eq!(options[1].current_value.as_deref(), Some("272k"));
        assert_eq!(options[3].current_value.as_deref(), Some("true"));
    }

    /// 替身 `cursor-agent acp`:session/new 报两个模型与当前模型的轴，切模型时回该模型的轴。
    const FAKE_ACP: &str = r#"#!/bin/sh
cd "$(dirname "$0")"
printf '%s\n' "$CURSOR_CONFIG_DIR" >> config-dirs
while IFS= read -r line; do
  printf '%s\n' "$line" >> requests.jsonl
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
  case "$line" in
    *'"method":"initialize"'*) printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":1}}\n' "$id" ;;
    *'"method":"session/new"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"sessionId":"probe-1","models":{"currentModelId":"auto-smart","availableModels":[{"modelId":"auto-smart","name":"Auto"},{"modelId":"gpt-5.6-sol","name":"Sol"}]},"configOptions":[{"id":"model","name":"Model","type":"select","currentValue":"auto-smart","options":[{"value":"auto-smart","name":"Auto"},{"value":"gpt-5.6-sol","name":"Sol"}]},{"id":"optimize_for","name":"Optimize","type":"select","currentValue":"balanced","options":[{"value":"balanced","name":"Balanced"},{"value":"cost","name":"Cost"}]}]}}\n' "$id" ;;
    *'"method":"session/set_config_option"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"configOptions":[{"id":"mode","name":"Mode","type":"select","currentValue":"agent","options":[{"value":"agent","name":"Agent"}]},{"id":"model","name":"Model","type":"select","currentValue":"gpt-5.6-sol","options":[{"value":"auto-smart","name":"Auto"},{"value":"gpt-5.6-sol","name":"Sol"}]},{"id":"context","name":"Context","type":"select","currentValue":"272k","options":[{"value":"272k","name":"272K"},{"value":"1m","name":"1M"}]},{"id":"fast","name":"Fast","type":"select","currentValue":"false","options":[{"value":"false","name":"Off"},{"value":"true","name":"On"}]}]}}\n' "$id" ;;
  esac
done
"#;

    #[test]
    fn probe_reads_every_models_axes_without_prompting() {
        let (agent, tmp) = cli_agent();
        let script = tmp.path().join("fake-cursor-agent");
        crate::test_support::write_test_executable(&script, FAKE_ACP);
        let stopped = AtomicBool::new(false);
        let mut killers = Vec::new();
        let catalog = probe(
            &agent,
            script.to_str().unwrap(),
            &crate::runtime::daemon::essential_system_env(),
            &stopped,
            &mut |killer| killers.push(killer.is_some()),
        )
        .unwrap()
        .unwrap();
        assert_eq!(killers, [true, false], "停机路径要能杀掉在跑的探测进程");
        assert_eq!(catalog.models, ["auto-smart", "gpt-5.6-sol"]);
        assert_eq!(catalog.default_model.as_deref(), Some("auto-smart"));
        let ids = |model: &str| -> Vec<String> {
            catalog.axes[model]
                .iter()
                .map(|axis| axis.id.clone())
                .collect()
        };
        assert_eq!(ids("auto-smart"), ["optimize_for"]);
        assert_eq!(ids("gpt-5.6-sol"), ["context", "fast"]);
        let requests = std::fs::read_to_string(tmp.path().join("requests.jsonl")).unwrap();
        assert!(!requests.contains("session/prompt"), "探测不发 prompt");
        assert_eq!(requests.matches("session/set_config_option").count(), 1);
        let config_dir = std::fs::read_to_string(tmp.path().join("config-dirs")).unwrap();
        let config_dir = std::path::Path::new(config_dir.trim());
        assert!(config_dir.starts_with(agent.state_dir().join("cursor-config")));
        assert!(!config_dir.exists(), "探测用的配置根用完即删");
    }

    #[test]
    fn a_failed_probe_leaves_no_catalog_behind() {
        let (agent, tmp) = cli_agent();
        let script = tmp.path().join("broken-cursor-agent");
        crate::test_support::write_test_executable(&script, "#!/bin/sh\nexit 3\n");
        let stopped = AtomicBool::new(false);
        let outcome = refresh(
            &agent,
            script.to_str().unwrap(),
            &crate::runtime::daemon::essential_system_env(),
            &stopped,
            &mut |_| {},
        );
        assert!(outcome.is_err());
        assert!(!catalog_path(&agent).exists());
    }

    /// 真机：一次性 ACP 探测能拿到参数化目录(不发 prompt,不耗 token)。
    #[test]
    #[ignore = "需要本机已登录的 cursor-agent;起 ACP 读目录，不发 prompt"]
    fn real_cursor_cli_parameter_probe_reports_parameterized_axes() {
        let (agent, _tmp) = cli_agent();
        let program = summon_core::tools::resolve(summon_core::tools::Tool::CursorAgent)
            .to_string_lossy()
            .into_owned();
        let stopped = AtomicBool::new(false);
        let catalog = probe(
            &agent,
            &program,
            &crate::runtime::daemon::essential_system_env(),
            &stopped,
            &mut |_| {},
        )
        .expect("探测应成功")
        .expect("未被打断");
        assert!(!catalog.models.is_empty());
        let open =
            |id: &str| matches!(id, "context" | "reasoning" | "effort" | "thinking" | "fast");
        assert!(
            catalog
                .axes
                .values()
                .any(|axes| axes.iter().any(|axis| open(&axis.id))),
            "至少一个模型要带可调的参数轴: {:?}",
            catalog.axes.keys().collect::<Vec<_>>()
        );
        eprintln!(
            "models={} with_axes={}",
            catalog.models.len(),
            catalog.axes.len()
        );
    }
}
