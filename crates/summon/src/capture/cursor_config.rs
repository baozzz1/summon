//! Cursor 参数化模型的意图存储:per-agent 默认 bundle 与按群的稀疏覆盖。
//!
//! Cursor 自己的 configProvider 会把同一 `CURSOR_CONFIG_DIR` 下的 session 当成共享状态；
//! summon 因而给**每个 ACP 进程**一个独立临时根，本模块存两层目标:per-agent 默认 bundle
//! (`<state>/cursor-parameters.json`)与本群覆盖(`<state>/threads/<tag>/cursor-parameters.json`,
//! 同一 schema、只记本群改过的轴)。每轮在 prompt 前重放两层合成的目标。盘上文件是唯一事实源，
//! `runtime.rpc_model` 仅在默认 bundle 尚不存在时作为一次性迁移 seed，读取 seed 本身绝不落盘。

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::Read as _;
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use summon_core::registry::Agent;

pub const CURSOR_CONFIG_BUNDLE_VERSION: u32 = 1;
pub const CURSOR_CONFIG_BUNDLE_FILE: &str = "cursor-parameters.json";
pub const CURSOR_CONFIG_DIR_NAME: &str = "cursor-config";
pub const CURSOR_ACP_SESSIONS_DIR_NAME: &str = "cursor-acp-sessions";

const MAX_BUNDLE_BYTES: u64 = 1024 * 1024;
const MAX_PROFILES: usize = 128;
const MAX_OPTIONS_PER_PROFILE: usize = 64;
const MAX_MODEL_BYTES: usize = 512;
const MAX_OPTION_ID_BYTES: usize = 128;
const MAX_OPTION_VALUE_BYTES: usize = 512;

// bundle 是 per-agent 共享意图。锁按 agent 物理根分片，既串住卡片 mutation，也让不同 chat
// 的「读 bundle → 应用全轴 → 代际复核」成为一个事务；不能只让 rename 原子，否则旧
// generation 的慢 ACP setter 仍可能最后提交旧意图。Cursor 进程自身另有独立临时配置根。
static AGENT_LOCKS: OnceLock<Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>> = OnceLock::new();

/// 一个 Cursor ACP 进程独占的 configProvider 根。
///
/// guard 必须至少与 ACP client 同寿命；最后一个持有者释放时，`TempDir` 才会回收目录。
/// 用进程实例而不是 chat tag 做边界，Bridge rekey 后环境变量不变也不会和新进程撞根。
#[derive(Debug)]
pub struct CursorProcessConfigDir {
    directory: tempfile::TempDir,
}

impl CursorProcessConfigDir {
    pub fn path(&self) -> &Path {
        self.directory.path()
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CursorModelProfile {
    #[serde(default)]
    pub options: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CursorConfigBundle {
    pub version: u32,
    pub generation: u64,
    pub selected_model: Option<String>,
    #[serde(default)]
    pub profiles: BTreeMap<String, CursorModelProfile>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorConfigView {
    pub bundle: CursorConfigBundle,
    /// `false` 表示只从 `runtime.rpc_model` 得到的迁移视图，尚未产生任何磁盘写入。
    pub persisted: bool,
}

impl CursorConfigView {
    pub fn selected_model(&self) -> Option<&str> {
        self.bundle.selected_model.as_deref()
    }

    #[cfg(test)]
    pub fn current_options(&self) -> Option<&BTreeMap<String, String>> {
        let model = self.selected_model()?;
        self.bundle
            .profiles
            .get(model)
            .map(|profile| &profile.options)
    }
}

/// 意图落在哪一层。本群覆盖与默认 bundle 同一 schema,读写校验共用一套。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorConfigLayer<'a> {
    Default,
    Chat(&'a str),
}

/// 两层各自的代际。收敛缓存按这一对判「这一代整向量是否已验收」:任一层变了都要重放。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CursorTargetGeneration {
    pub default: u64,
    pub chat: u64,
}

/// 本群该收敛到的目标:模型取本群、没有取默认;option 取默认 bundle 里该模型的 profile,
/// 再叠本群同一模型的 profile。按轴稀疏 —— 本群只钉了模型,默认的 reasoning 改动照样传进来。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorTarget {
    pub default: CursorConfigView,
    pub chat: CursorConfigView,
}

impl CursorTarget {
    pub fn model(&self) -> Option<&str> {
        self.chat
            .selected_model()
            .or_else(|| self.default.selected_model())
    }

    /// 模型来自本群覆盖。
    #[cfg(test)]
    pub fn model_overridden(&self) -> bool {
        self.chat.selected_model().is_some()
    }

    pub fn options(&self) -> BTreeMap<String, String> {
        let Some(model) = self.model() else {
            return BTreeMap::new();
        };
        let mut options = self
            .default
            .bundle
            .profiles
            .get(model)
            .map(|profile| profile.options.clone())
            .unwrap_or_default();
        if let Some(profile) = self.chat.bundle.profiles.get(model) {
            options.extend(profile.options.clone());
        }
        options
    }

    /// 本群覆盖了当前模型下的哪些 option(面板标「覆盖」用)。
    pub fn overridden_option(&self, option_id: &str) -> bool {
        self.model().is_some_and(|model| {
            self.chat
                .bundle
                .profiles
                .get(model)
                .is_some_and(|profile| profile.options.contains_key(option_id))
        })
    }

    /// 默认层在当前模型下的 option 值(面板「默认 Y」用)。
    pub fn default_option(&self, option_id: &str) -> Option<&str> {
        let model = self.model()?;
        self.default
            .bundle
            .profiles
            .get(model)?
            .options
            .get(option_id)
            .map(String::as_str)
    }

    pub fn generation(&self) -> CursorTargetGeneration {
        CursorTargetGeneration {
            default: self.default.bundle.generation,
            chat: self.chat.bundle.generation,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorConfigErrorKind {
    InvalidInput,
    Conflict,
    UnsafePath,
    Read,
    Write,
}

pub struct CursorConfigError {
    kind: CursorConfigErrorKind,
    detail: String,
}

impl CursorConfigError {
    fn new(kind: CursorConfigErrorKind, detail: impl Into<String>) -> Self {
        Self {
            kind,
            detail: detail.into(),
        }
    }

    pub fn kind(&self) -> CursorConfigErrorKind {
        self.kind
    }

    pub(crate) fn diagnostic(&self) -> &str {
        &self.detail
    }
}

impl fmt::Debug for CursorConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CursorConfigError")
            .field("kind", &self.kind)
            .finish_non_exhaustive()
    }
}

impl fmt::Display for CursorConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self.kind() {
            CursorConfigErrorKind::InvalidInput => summon_i18n::text!("ra86a4f555d"),
            CursorConfigErrorKind::Conflict => summon_i18n::text!("ra8d38d104e"),
            CursorConfigErrorKind::UnsafePath => {
                summon_i18n::text!("r94a88d2342")
            }
            CursorConfigErrorKind::Read => {
                summon_i18n::text!("r0ebefcb8d1")
            }
            CursorConfigErrorKind::Write => {
                summon_i18n::text!("r8ab010b2a8")
            }
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for CursorConfigError {}

pub fn bundle_path(agent: &Agent) -> PathBuf {
    agent.state_dir().join(CURSOR_CONFIG_BUNDLE_FILE)
}

/// 读取 per-agent Cursor 参数意图。
///
/// bundle 缺失时只把 `runtime.rpc_model` 解析成 generation=0 的迁移视图，不创建文件；一旦
/// bundle 存在，任何 IO/schema/version/invariant 错误都 fail-stop，绝不退回 seed 掩盖坏状态。
pub fn load(agent: &Agent) -> Result<CursorConfigView, CursorConfigError> {
    load_inner(agent)
}

/// 读取一层意图。本群覆盖缺失时是空覆盖(generation 0);存在时与默认 bundle 同样严格,
/// 任何 IO/schema/version/invariant 错误都 fail-stop —— 悄悄退回默认可能把这一轮跑在用户没选的
/// 计费档上。
pub fn load_layer(
    agent: &Agent,
    layer: CursorConfigLayer<'_>,
) -> Result<CursorConfigView, CursorConfigError> {
    load_store(agent, layer)
}

/// 本群该收敛到的两层目标。
pub fn load_target(agent: &Agent, tag: &str) -> Result<CursorTarget, CursorConfigError> {
    Ok(CursorTarget {
        default: load_store(agent, CursorConfigLayer::Default)?,
        chat: load_store(agent, CursorConfigLayer::Chat(tag))?,
    })
}

/// 设置或清除本群的模型覆盖(`None` 回到默认)。本群改过的 option 按模型保留:换回那个模型时
/// 照样生效。
pub fn set_chat_selected_model(
    agent: &Agent,
    tag: &str,
    expected_generation: u64,
    model: Option<&str>,
) -> Result<CursorConfigView, CursorConfigError> {
    let model = model
        .map(|model| normalize_input(model, "model", MAX_MODEL_BYTES))
        .transpose()?;
    mutate_store(
        agent,
        CursorConfigLayer::Chat(tag),
        expected_generation,
        move |bundle| {
            if let Some(model) = &model {
                bundle.profiles.entry(model.clone()).or_default();
            }
            bundle.selected_model = model;
            prune_empty_profiles(bundle);
            Ok(())
        },
    )
}

/// 设置或清除本群在某个模型下的一个 option(`None` 回到默认)。
pub fn set_chat_model_option(
    agent: &Agent,
    tag: &str,
    expected_generation: u64,
    model: &str,
    option_id: &str,
    value: Option<&str>,
) -> Result<CursorConfigView, CursorConfigError> {
    let model = normalize_input(model, "model", MAX_MODEL_BYTES)?;
    let option_id = normalize_input(option_id, "option id", MAX_OPTION_ID_BYTES)?;
    let value = value
        .map(|value| normalize_input(value, "option value", MAX_OPTION_VALUE_BYTES))
        .transpose()?;
    mutate_store(
        agent,
        CursorConfigLayer::Chat(tag),
        expected_generation,
        move |bundle| {
            let profile = bundle.profiles.entry(model.clone()).or_default();
            match value {
                Some(value) => {
                    profile.options.insert(option_id, value);
                }
                None => {
                    profile.options.remove(&option_id);
                }
            }
            prune_empty_profiles(bundle);
            Ok(())
        },
    )
}

/// 把本群合成目标写成默认:默认 bundle 选中这个模型,该模型的 profile 换成本群生效的那组参数。
/// 两层在同一把锁里读写;本群覆盖原样保留(改默认不动本群覆盖)。
pub fn promote_chat_target_to_default(
    agent: &Agent,
    tag: &str,
) -> Result<CursorConfigView, CursorConfigError> {
    with_agent_lock(agent, || {
        let target = CursorTarget {
            default: load_store(agent, CursorConfigLayer::Default)?,
            chat: load_store(agent, CursorConfigLayer::Chat(tag))?,
        };
        let model = target.model().map(str::to_string).ok_or_else(|| {
            CursorConfigError::new(
                CursorConfigErrorKind::InvalidInput,
                summon_i18n::text!("rbd8b275e7c"),
            )
        })?;
        let options = target.options();
        mutate_locked(
            agent,
            CursorConfigLayer::Default,
            target.default.bundle.generation,
            move |bundle| {
                bundle.selected_model = Some(model.clone());
                bundle
                    .profiles
                    .insert(model, CursorModelProfile { options });
                Ok(())
            },
        )
    })?
}

/// 本群覆盖只记改过的轴:空 profile 只在它是本群选中模型时保留(校验要求选中模型有 profile)。
fn prune_empty_profiles(bundle: &mut CursorConfigBundle) {
    let selected = bundle.selected_model.clone();
    bundle.profiles.retain(|model, profile| {
        !profile.options.is_empty() || selected.as_deref() == Some(model.as_str())
    });
}

/// 在该 agent 的 Cursor 参数事务锁内执行操作。
///
/// 返回双层 `Result` 是刻意的：外层只表达路径/锁错误，闭包自己的协议或业务错误保持原类型，
/// 调用方无需把网络错误硬塞进持久化错误枚举。
pub fn with_agent_lock<T>(
    agent: &Agent,
    operation: impl FnOnce() -> T,
) -> Result<T, CursorConfigError> {
    let root = canonical_agent_root(agent)?;
    let lock = {
        let mut locks = AGENT_LOCKS
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .map_err(|_| {
                CursorConfigError::new(
                    CursorConfigErrorKind::Write,
                    summon_i18n::text!("r9e41ce30e8"),
                )
            })?;
        Arc::clone(
            locks
                .entry(root)
                .or_insert_with(|| Arc::new(Mutex::new(()))),
        )
    };
    let _guard = lock.lock().map_err(|_| {
        CursorConfigError::new(
            CursorConfigErrorKind::Write,
            summon_i18n::text!("r9140427837"),
        )
    })?;
    Ok(operation())
}

pub fn set_selected_model(
    agent: &Agent,
    expected_generation: u64,
    model: &str,
) -> Result<CursorConfigView, CursorConfigError> {
    let model = normalize_input(model, "model", MAX_MODEL_BYTES)?;
    mutate(agent, expected_generation, move |bundle| {
        bundle.selected_model = Some(model.clone());
        bundle.profiles.entry(model).or_default();
        Ok(())
    })
}

/// 在一次 generation-CAS 里建立/复核默认 bundle 的当前模型并写入一个 option。生产里默认层的
/// option 只经 [`promote_chat_target_to_default`] 整组写入;这条给测试搭默认层用。
#[cfg(test)]
pub fn set_model_option_for_model(
    agent: &Agent,
    expected_generation: u64,
    model: &str,
    option_id: &str,
    value: &str,
) -> Result<CursorConfigView, CursorConfigError> {
    let model = normalize_input(model, "model", MAX_MODEL_BYTES)?;
    let option_id = normalize_input(option_id, "option id", MAX_OPTION_ID_BYTES)?;
    let value = normalize_input(value, "option value", MAX_OPTION_VALUE_BYTES)?;
    mutate(agent, expected_generation, move |bundle| {
        match bundle.selected_model.as_deref() {
            Some(selected) if selected != model => {
                return Err(CursorConfigError::new(
                    CursorConfigErrorKind::InvalidInput,
                    "live model 与 bundle selected_model 不一致",
                ));
            }
            None => bundle.selected_model = Some(model.clone()),
            Some(_) => {}
        }
        bundle
            .profiles
            .entry(model.clone())
            .or_default()
            .options
            .insert(option_id.clone(), value.clone());
        Ok(())
    })
}

/// 创建一个 ACP 进程独占的隔离配置根，即 `<state_dir>/cursor-config/acp-<random>`。
///
/// `cli-config.json` / `acp-config.json` 在此进程临时目录内隔离；ACP 会话库（`acp-sessions/<sid>/store.db`）
/// 同样由 Cursor 从 `CURSOR_CONFIG_DIR/acp-sessions` 读写，因此把进程目录内的 `acp-sessions`
/// 软链到跨进程持久化的 `<state_dir>/cursor-acp-sessions`，使进程重开后 `session/load` 与上下文采集仍能读回同一份会话库。
pub fn process_config_dir(agent: &Agent) -> Result<CursorProcessConfigDir, CursorConfigError> {
    let root = canonical_agent_root(agent)?;
    let canonical_target =
        ensure_private_agent_subdir(&root, &agent.state_dir().join(CURSOR_CONFIG_DIR_NAME))?;
    let canonical_sessions =
        ensure_private_agent_subdir(&root, &agent.state_dir().join(CURSOR_ACP_SESSIONS_DIR_NAME))?;
    let directory = tempfile::Builder::new()
        .prefix("acp-")
        .tempdir_in(&canonical_target)
        .map_err(|error| {
            CursorConfigError::new(
                CursorConfigErrorKind::UnsafePath,
                summon_i18n::format!("rafd9cc813b"),
            )
        })?;
    open_directory_no_follow(directory.path())?
        .set_permissions(fs::Permissions::from_mode(0o700))
        .map_err(|error| {
            CursorConfigError::new(
                CursorConfigErrorKind::UnsafePath,
                summon_i18n::format!("rc414e64a33"),
            )
        })?;
    let canonical_process_dir = fs::canonicalize(directory.path()).map_err(|error| {
        CursorConfigError::new(
            CursorConfigErrorKind::UnsafePath,
            summon_i18n::format!("rd59d4f84fd"),
        )
    })?;
    if canonical_process_dir.parent() != Some(canonical_target.as_path()) {
        return Err(CursorConfigError::new(
            CursorConfigErrorKind::UnsafePath,
            summon_i18n::text!("r042e3aa7d1"),
        ));
    }
    std::os::unix::fs::symlink(&canonical_sessions, directory.path().join("acp-sessions"))
        .map_err(|error| {
            CursorConfigError::new(
                CursorConfigErrorKind::UnsafePath,
                summon_i18n::format!("rc46e1ac775"),
            )
        })?;
    Ok(CursorProcessConfigDir { directory })
}

fn ensure_private_agent_subdir(root: &Path, target: &Path) -> Result<PathBuf, CursorConfigError> {
    let parent = target.parent().ok_or_else(|| {
        CursorConfigError::new(
            CursorConfigErrorKind::UnsafePath,
            summon_i18n::text!("r727314a91a"),
        )
    })?;
    let canonical_parent = fs::canonicalize(parent).map_err(|error| {
        CursorConfigError::new(
            CursorConfigErrorKind::UnsafePath,
            summon_i18n::format!("r0105144664"),
        )
    })?;
    if canonical_parent != root {
        return Err(CursorConfigError::new(
            CursorConfigErrorKind::UnsafePath,
            summon_i18n::text!("r7e2369d476"),
        ));
    }

    loop {
        match fs::symlink_metadata(target) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() || !metadata.file_type().is_dir() {
                    return Err(CursorConfigError::new(
                        CursorConfigErrorKind::UnsafePath,
                        summon_i18n::text!("r6c5db97849"),
                    ));
                }
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                match fs::create_dir(target) {
                    Ok(()) => break,
                    // 另一个线程/进程恰好创建时回到 metadata 闸，不能直接当成可信目录。
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                    Err(error) => {
                        return Err(CursorConfigError::new(
                            CursorConfigErrorKind::UnsafePath,
                            summon_i18n::format!("rae129cf6cd"),
                        ));
                    }
                }
            }
            Err(error) => {
                return Err(CursorConfigError::new(
                    CursorConfigErrorKind::UnsafePath,
                    summon_i18n::format!("rb887dfc5a2"),
                ));
            }
        }
    }

    // O_NOFOLLOW 把 metadata 检查后的 symlink 竞态也收住；权限改在已打开 fd 上，不再跟路径走。
    let directory = open_directory_no_follow(target)?;
    directory
        .set_permissions(fs::Permissions::from_mode(0o700))
        .map_err(|error| {
            CursorConfigError::new(
                CursorConfigErrorKind::UnsafePath,
                summon_i18n::format!("rc65f99275e"),
            )
        })?;
    let canonical_target = fs::canonicalize(target).map_err(|error| {
        CursorConfigError::new(
            CursorConfigErrorKind::UnsafePath,
            summon_i18n::format!("r27403eb7eb"),
        )
    })?;
    if canonical_target.parent() != Some(root) {
        return Err(CursorConfigError::new(
            CursorConfigErrorKind::UnsafePath,
            summon_i18n::text!("rb47e53b9a0"),
        ));
    }
    Ok(canonical_target)
}

fn load_inner(agent: &Agent) -> Result<CursorConfigView, CursorConfigError> {
    load_store(agent, CursorConfigLayer::Default)
}

/// 本群覆盖文件。tag 不像 `registry::chat_tag` 的形态就不当路径用。
fn chat_overlay_path(agent: &Agent, tag: &str) -> Result<PathBuf, CursorConfigError> {
    crate::runtime::chat_model::chat_dir(agent, tag)
        .map(|dir| dir.join(CURSOR_CONFIG_BUNDLE_FILE))
        .ok_or_else(|| {
            CursorConfigError::new(
                CursorConfigErrorKind::UnsafePath,
                summon_i18n::format!("ra3aef982f8"),
            )
        })
}

fn store_path(agent: &Agent, layer: CursorConfigLayer<'_>) -> Result<PathBuf, CursorConfigError> {
    match layer {
        CursorConfigLayer::Default => Ok(bundle_path(agent)),
        CursorConfigLayer::Chat(tag) => chat_overlay_path(agent, tag),
    }
}

fn empty_view() -> CursorConfigView {
    CursorConfigView {
        bundle: CursorConfigBundle {
            version: CURSOR_CONFIG_BUNDLE_VERSION,
            generation: 0,
            selected_model: None,
            profiles: BTreeMap::new(),
        },
        persisted: false,
    }
}

fn load_store(
    agent: &Agent,
    layer: CursorConfigLayer<'_>,
) -> Result<CursorConfigView, CursorConfigError> {
    let Some(path) = existing_store_path(agent, layer)? else {
        return match layer {
            CursorConfigLayer::Default => seed_view(agent),
            CursorConfigLayer::Chat(_) => Ok(empty_view()),
        };
    };
    let bytes = read_regular_file_no_follow(&path)?;
    let bundle: CursorConfigBundle = serde_json::from_slice(&bytes).map_err(|error| {
        CursorConfigError::new(
            CursorConfigErrorKind::Read,
            summon_i18n::format!("r4a39d1cf57"),
        )
    })?;
    validate_bundle(&bundle, CursorConfigErrorKind::Read)?;
    Ok(CursorConfigView {
        bundle,
        persisted: true,
    })
}

fn seed_view(agent: &Agent) -> Result<CursorConfigView, CursorConfigError> {
    let mut bundle = CursorConfigBundle {
        version: CURSOR_CONFIG_BUNDLE_VERSION,
        generation: 0,
        selected_model: None,
        profiles: BTreeMap::new(),
    };
    if let Some(seed) = agent.rpc_model() {
        let (model, options) = parse_bracket_seed(seed)?;
        bundle.selected_model = Some(model.clone());
        bundle
            .profiles
            .insert(model, CursorModelProfile { options });
    }
    validate_bundle(&bundle, CursorConfigErrorKind::Read)?;
    Ok(CursorConfigView {
        bundle,
        persisted: false,
    })
}

fn parse_bracket_seed(seed: &str) -> Result<(String, BTreeMap<String, String>), CursorConfigError> {
    let seed = normalize_seed_token(seed, "runtime.rpc_model", MAX_MODEL_BYTES * 4)?;
    let Some(open) = seed.find('[') else {
        if seed.contains(']') {
            return Err(seed_error(summon_i18n::text!("r690b573293")));
        }
        return Ok((
            normalize_seed_token(&seed, "seed model", MAX_MODEL_BYTES)?,
            BTreeMap::new(),
        ));
    };
    if !seed.ends_with(']') {
        return Err(seed_error(summon_i18n::text!("r3d71f5a9d8")));
    }
    let body = &seed[open + 1..seed.len() - 1];
    if body.contains('[') || body.contains(']') {
        return Err(seed_error(summon_i18n::text!("r3d71f5a9d8")));
    }
    let model = normalize_seed_token(&seed[..open], "seed model", MAX_MODEL_BYTES)?;
    let mut options = BTreeMap::new();
    if body.trim().is_empty() {
        return Ok((model, options));
    }
    for item in body.split(',') {
        let Some((id, value)) = item.split_once('=') else {
            return Err(seed_error(summon_i18n::text!("r9c42d930ae")));
        };
        let id = normalize_seed_token(id, "seed option id", MAX_OPTION_ID_BYTES)?;
        let value = normalize_seed_token(value, "seed option value", MAX_OPTION_VALUE_BYTES)?;
        if options.insert(id, value).is_some() {
            return Err(seed_error(summon_i18n::text!("r005c6aaeca")));
        }
        if options.len() > MAX_OPTIONS_PER_PROFILE {
            return Err(seed_error(summon_i18n::text!("rfd2c82fe9f")));
        }
    }
    Ok((model, options))
}

fn mutate(
    agent: &Agent,
    expected_generation: u64,
    mutation: impl FnOnce(&mut CursorConfigBundle) -> Result<(), CursorConfigError>,
) -> Result<CursorConfigView, CursorConfigError> {
    mutate_store(
        agent,
        CursorConfigLayer::Default,
        expected_generation,
        mutation,
    )
}

/// 两层共用一把 agent 级事务锁:收敛时同一事务里读两层、重放、复核两层代际。
fn mutate_store(
    agent: &Agent,
    layer: CursorConfigLayer<'_>,
    expected_generation: u64,
    mutation: impl FnOnce(&mut CursorConfigBundle) -> Result<(), CursorConfigError>,
) -> Result<CursorConfigView, CursorConfigError> {
    with_agent_lock(agent, || {
        mutate_locked(agent, layer, expected_generation, mutation)
    })?
}

fn mutate_locked(
    agent: &Agent,
    layer: CursorConfigLayer<'_>,
    expected_generation: u64,
    mutation: impl FnOnce(&mut CursorConfigBundle) -> Result<(), CursorConfigError>,
) -> Result<CursorConfigView, CursorConfigError> {
    let mut view = load_store(agent, layer)?;
    if view.bundle.generation != expected_generation {
        return Err(CursorConfigError::new(
            CursorConfigErrorKind::Conflict,
            summon_i18n::format!("rd3a5ced136", view.bundle.generation),
        ));
    }
    mutation(&mut view.bundle)?;
    view.bundle.generation = view.bundle.generation.checked_add(1).ok_or_else(|| {
        CursorConfigError::new(
            CursorConfigErrorKind::Write,
            summon_i18n::text!("r49096c5388"),
        )
    })?;
    validate_bundle(&view.bundle, CursorConfigErrorKind::Write)?;
    persist_bundle(agent, layer, &view.bundle, view.persisted)?;
    view.persisted = true;
    Ok(view)
}

fn persist_bundle(
    agent: &Agent,
    layer: CursorConfigLayer<'_>,
    bundle: &CursorConfigBundle,
    previously_persisted: bool,
) -> Result<(), CursorConfigError> {
    let observed_persisted = existing_store_path(agent, layer)?.is_some();
    if observed_persisted != previously_persisted {
        return Err(CursorConfigError::new(
            CursorConfigErrorKind::Conflict,
            summon_i18n::text!("r5fd0fab6d6"),
        ));
    }
    let path = store_path(agent, layer)?;
    if let CursorConfigLayer::Chat(_) = layer {
        let parent = path.parent().ok_or_else(|| {
            CursorConfigError::new(
                CursorConfigErrorKind::UnsafePath,
                summon_i18n::text!("rbc273db330"),
            )
        })?;
        fs::create_dir_all(parent).map_err(|error| {
            CursorConfigError::new(
                CursorConfigErrorKind::Write,
                summon_i18n::format!("r57dea7d508"),
            )
        })?;
        expected_parent(agent, layer)?;
    }
    let mut bytes = serde_json::to_vec_pretty(bundle).map_err(|error| {
        CursorConfigError::new(
            CursorConfigErrorKind::Write,
            summon_i18n::format!("r817fd9772c"),
        )
    })?;
    bytes.push(b'\n');
    if bytes.len() as u64 > MAX_BUNDLE_BYTES {
        return Err(CursorConfigError::new(
            CursorConfigErrorKind::Write,
            summon_i18n::text!("r1a5ffd490a"),
        ));
    }
    crate::runtime::fsio::atomic_write_mode(&path, &bytes, 0o600).map_err(|error| {
        CursorConfigError::new(
            CursorConfigErrorKind::Write,
            summon_i18n::format!("r96dba36d37"),
        )
    })
}

fn canonical_agent_root(agent: &Agent) -> Result<PathBuf, CursorConfigError> {
    let state_dir = agent.state_dir();
    if state_dir.as_os_str().is_empty() {
        return Err(CursorConfigError::new(
            CursorConfigErrorKind::UnsafePath,
            summon_i18n::text!("r1ae54b06fd"),
        ));
    }
    // state_dir 可能还不存在(全新 agent);canonicalize 要求路径存在,先建再解析。
    fs::create_dir_all(&state_dir).map_err(|error| {
        CursorConfigError::new(
            CursorConfigErrorKind::UnsafePath,
            summon_i18n::format!("r3b309d4573"),
        )
    })?;
    let root = fs::canonicalize(&state_dir).map_err(|error| {
        CursorConfigError::new(
            CursorConfigErrorKind::UnsafePath,
            summon_i18n::format!("r08bff1511c"),
        )
    })?;
    if !root.is_dir() {
        return Err(CursorConfigError::new(
            CursorConfigErrorKind::UnsafePath,
            summon_i18n::text!("ra135a8b694"),
        ));
    }
    Ok(root)
}

/// 该层文件应在的物理目录:默认 bundle 在 agent 根,本群覆盖在 `<根>/threads/<tag>`。
/// 本群目录还不存在时返回它将会在的位置(由已解析的根拼出,不跟随任何 symlink)。
fn expected_parent(
    agent: &Agent,
    layer: CursorConfigLayer<'_>,
) -> Result<PathBuf, CursorConfigError> {
    let root = canonical_agent_root(agent)?;
    let CursorConfigLayer::Chat(tag) = layer else {
        return Ok(root);
    };
    let expected = root.join("threads").join(tag);
    match fs::symlink_metadata(&expected) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.file_type().is_dir() => {
            Err(CursorConfigError::new(
                CursorConfigErrorKind::UnsafePath,
                summon_i18n::text!("r31cb64c06f"),
            ))
        }
        Ok(_) => match fs::canonicalize(&expected) {
            Ok(canonical) if canonical == expected => Ok(expected),
            _ => Err(CursorConfigError::new(
                CursorConfigErrorKind::UnsafePath,
                summon_i18n::text!("r53a8164a3e"),
            )),
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(expected),
        Err(error) => Err(CursorConfigError::new(
            CursorConfigErrorKind::Read,
            summon_i18n::format!("r8d2ad702d7"),
        )),
    }
}

/// `None` 只表示文件确实不存在；其它任何不确定状态都 fail-stop。
fn existing_store_path(
    agent: &Agent,
    layer: CursorConfigLayer<'_>,
) -> Result<Option<PathBuf>, CursorConfigError> {
    let root = expected_parent(agent, layer)?;
    let path = store_path(agent, layer)?;
    match fs::symlink_metadata(&path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
                return Err(CursorConfigError::new(
                    CursorConfigErrorKind::UnsafePath,
                    summon_i18n::text!("rd36b5a25da"),
                ));
            }
            let canonical = fs::canonicalize(&path).map_err(|error| {
                CursorConfigError::new(
                    CursorConfigErrorKind::UnsafePath,
                    summon_i18n::format!("r183b5f08bb"),
                )
            })?;
            if canonical.parent() != Some(root.as_path()) {
                return Err(CursorConfigError::new(
                    CursorConfigErrorKind::UnsafePath,
                    summon_i18n::text!("r4025a034a5"),
                ));
            }
            Ok(Some(path))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(CursorConfigError::new(
            CursorConfigErrorKind::Read,
            summon_i18n::format!("r093708748f"),
        )),
    }
}

fn read_regular_file_no_follow(path: &Path) -> Result<Vec<u8>, CursorConfigError> {
    let mut options = OpenOptions::new();
    options.read(true).custom_flags(
        // O_NONBLOCK 防 metadata 后被换成 FIFO 时卡死；普通文件上无行为差异。
        nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK,
    );
    let file = options.open(path).map_err(|error| {
        CursorConfigError::new(
            CursorConfigErrorKind::Read,
            summon_i18n::format!("r1881e1bc01"),
        )
    })?;
    let metadata = file.metadata().map_err(|error| {
        CursorConfigError::new(
            CursorConfigErrorKind::Read,
            summon_i18n::format!("r603392f9b9"),
        )
    })?;
    if !metadata.file_type().is_file() || metadata.len() > MAX_BUNDLE_BYTES {
        return Err(CursorConfigError::new(
            CursorConfigErrorKind::Read,
            summon_i18n::text!("rc38df0c425"),
        ));
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(MAX_BUNDLE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| {
            CursorConfigError::new(
                CursorConfigErrorKind::Read,
                summon_i18n::format!("rfcabc49a46"),
            )
        })?;
    if bytes.len() as u64 > MAX_BUNDLE_BYTES {
        return Err(CursorConfigError::new(
            CursorConfigErrorKind::Read,
            summon_i18n::text!("r0dc67f3e59"),
        ));
    }
    Ok(bytes)
}

fn open_directory_no_follow(path: &Path) -> Result<File, CursorConfigError> {
    let mut options = OpenOptions::new();
    options
        .read(true)
        .custom_flags(nix::libc::O_DIRECTORY | nix::libc::O_NOFOLLOW);
    let directory = options.open(path).map_err(|error| {
        CursorConfigError::new(
            CursorConfigErrorKind::UnsafePath,
            summon_i18n::format!("r8f36cf6a12"),
        )
    })?;
    let metadata = directory.metadata().map_err(|error| {
        CursorConfigError::new(
            CursorConfigErrorKind::UnsafePath,
            summon_i18n::format!("rf1639f28ca"),
        )
    })?;
    if !metadata.file_type().is_dir() {
        return Err(CursorConfigError::new(
            CursorConfigErrorKind::UnsafePath,
            summon_i18n::text!("r5fc9624c88"),
        ));
    }
    Ok(directory)
}

fn validate_bundle(
    bundle: &CursorConfigBundle,
    error_kind: CursorConfigErrorKind,
) -> Result<(), CursorConfigError> {
    if bundle.version != CURSOR_CONFIG_BUNDLE_VERSION {
        return Err(CursorConfigError::new(
            error_kind,
            summon_i18n::format!("r1d884eb1db", bundle.version),
        ));
    }
    if bundle.profiles.len() > MAX_PROFILES {
        return Err(CursorConfigError::new(
            error_kind,
            summon_i18n::text!("r45b89da9a9"),
        ));
    }
    if let Some(selected) = &bundle.selected_model {
        validate_stored_token(selected, "selected_model", MAX_MODEL_BYTES, error_kind)?;
        if !bundle.profiles.contains_key(selected) {
            return Err(CursorConfigError::new(
                error_kind,
                summon_i18n::text!("r3e2e8d49c7"),
            ));
        }
    }
    for (model, profile) in &bundle.profiles {
        validate_stored_token(model, "profile model", MAX_MODEL_BYTES, error_kind)?;
        if profile.options.len() > MAX_OPTIONS_PER_PROFILE {
            return Err(CursorConfigError::new(
                error_kind,
                summon_i18n::text!("r887b1ce3c7"),
            ));
        }
        for (id, value) in &profile.options {
            validate_stored_token(id, "option id", MAX_OPTION_ID_BYTES, error_kind)?;
            validate_stored_token(value, "option value", MAX_OPTION_VALUE_BYTES, error_kind)?;
        }
    }
    Ok(())
}

fn validate_stored_token(
    value: &str,
    label: &str,
    max_bytes: usize,
    kind: CursorConfigErrorKind,
) -> Result<(), CursorConfigError> {
    if value.is_empty()
        || value.len() > max_bytes
        || value.trim() != value
        || value.chars().any(char::is_control)
    {
        return Err(CursorConfigError::new(
            kind,
            summon_i18n::format!("r6e13d15568"),
        ));
    }
    Ok(())
}

fn normalize_input(
    value: &str,
    label: &str,
    max_bytes: usize,
) -> Result<String, CursorConfigError> {
    validate_stored_token(value, label, max_bytes, CursorConfigErrorKind::InvalidInput)?;
    Ok(value.to_string())
}

fn normalize_seed_token(
    value: &str,
    label: &str,
    max_bytes: usize,
) -> Result<String, CursorConfigError> {
    let value = value.trim();
    validate_stored_token(value, label, max_bytes, CursorConfigErrorKind::Read)?;
    Ok(value.to_string())
}

fn seed_error(detail: impl Into<String>) -> CursorConfigError {
    CursorConfigError::new(CursorConfigErrorKind::Read, detail)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::mpsc;
    use std::time::Duration;

    fn make_agent(dir: &Path, rpc_model: Option<&str>) -> Agent {
        let rpc_model = rpc_model
            .map(|value| format!("  rpc_model: {}\n", serde_json::to_string(value).unwrap()))
            .unwrap_or_default();
        let mut agent: Agent = serde_yaml::from_str(&format!(
            "name: cursor-test\nruntime:\n  cli: cursor\n  capture: rpc\n{rpc_model}"
        ))
        .unwrap();
        // agent.dir 必须是 tempdir 的子目录而非根:state_dir() 从 dir.parent() 推导 agents_dir,
        // 用根会让 state_dir 落到系统临时目录的共享路径,跨测试串味。
        let agent_dir = dir.join("agents").join("cursor-test");
        fs::create_dir_all(&agent_dir).unwrap();
        agent.dir = agent_dir;
        // 预建 state_dir:symlink 测试要在里面建链接,父目录不存在会 NotFound。
        fs::create_dir_all(agent.state_dir()).unwrap();
        agent
    }

    /// 本群覆盖按轴稀疏:只钉了模型的群跟随默认的 option 改动;本群改过的 option 压过默认;
    /// 别的群不受影响;清掉覆盖回到默认。
    #[test]
    fn a_chat_overlay_overrides_only_the_axes_it_sets() {
        let temp = tempfile::tempdir().unwrap();
        let agent = make_agent(temp.path(), None);
        let base =
            set_model_option_for_model(&agent, 0, "gpt-5.6-sol", "reasoning", "high").unwrap();
        set_model_option_for_model(
            &agent,
            base.bundle.generation,
            "gpt-5.6-sol",
            "fast",
            "false",
        )
        .unwrap();

        let untouched = load_target(&agent, "g-a").unwrap();
        assert_eq!(untouched.model(), Some("gpt-5.6-sol"));
        assert!(!untouched.model_overridden());
        assert_eq!(untouched.generation().chat, 0);

        let overlay =
            set_chat_model_option(&agent, "g-a", 0, "gpt-5.6-sol", "reasoning", Some("xhigh"))
                .unwrap();
        let a = load_target(&agent, "g-a").unwrap();
        assert_eq!(
            a.options(),
            BTreeMap::from([
                ("fast".to_string(), "false".to_string()),
                ("reasoning".to_string(), "xhigh".to_string()),
            ])
        );
        assert!(a.overridden_option("reasoning"));
        assert!(!a.overridden_option("fast"));
        assert_eq!(a.default_option("reasoning"), Some("high"));
        let b = load_target(&agent, "g-b").unwrap();
        assert_eq!(
            b.options().get("reasoning").map(String::as_str),
            Some("high")
        );

        // 默认改 fast,只钉了 reasoning 的群照样跟随。
        let default_now = load(&agent).unwrap();
        set_model_option_for_model(
            &agent,
            default_now.bundle.generation,
            "gpt-5.6-sol",
            "fast",
            "true",
        )
        .unwrap();
        assert_eq!(
            load_target(&agent, "g-a")
                .unwrap()
                .options()
                .get("fast")
                .map(String::as_str),
            Some("true")
        );

        // 本群换模型:该模型在默认里没有 profile,只用本群那份。
        let chat = set_chat_selected_model(
            &agent,
            "g-a",
            overlay.bundle.generation,
            Some("claude-opus-5-5"),
        )
        .unwrap();
        let switched = load_target(&agent, "g-a").unwrap();
        assert_eq!(switched.model(), Some("claude-opus-5-5"));
        assert!(switched.model_overridden());
        assert!(switched.options().is_empty());
        assert_eq!(
            load_target(&agent, "g-b").unwrap().model(),
            Some("gpt-5.6-sol")
        );

        // 清掉模型覆盖:回到默认模型,本群在那个模型下改过的 reasoning 仍在。
        let cleared = set_chat_selected_model(&agent, "g-a", chat.bundle.generation, None).unwrap();
        let back = load_target(&agent, "g-a").unwrap();
        assert_eq!(back.model(), Some("gpt-5.6-sol"));
        assert_eq!(
            back.options().get("reasoning").map(String::as_str),
            Some("xhigh")
        );
        set_chat_model_option(
            &agent,
            "g-a",
            cleared.bundle.generation,
            "gpt-5.6-sol",
            "reasoning",
            None,
        )
        .unwrap();
        let inherited = load_target(&agent, "g-a").unwrap();
        assert_eq!(
            inherited.options().get("reasoning").map(String::as_str),
            Some("high")
        );
        assert!(inherited.chat.bundle.profiles.is_empty());
        // 代际只增不减:清空的覆盖仍留着文件,收敛缓存不会把它认成从没覆盖过的那一代。
        assert_eq!(inherited.generation().chat, 4);
    }

    /// 「本群这套设为默认」:默认 bundle 换成本群合成目标,本群覆盖原样保留。
    #[test]
    fn promoting_a_chat_target_rewrites_the_default_and_keeps_the_overlay() {
        let temp = tempfile::tempdir().unwrap();
        let agent = make_agent(temp.path(), None);
        set_model_option_for_model(&agent, 0, "gpt-5.6-sol", "fast", "false").unwrap();
        let chat = set_chat_selected_model(&agent, "g-a", 0, Some("claude-opus-5-5")).unwrap();
        set_chat_model_option(
            &agent,
            "g-a",
            chat.bundle.generation,
            "claude-opus-5-5",
            "reasoning",
            Some("high"),
        )
        .unwrap();
        promote_chat_target_to_default(&agent, "g-a").unwrap();
        let default = load(&agent).unwrap();
        assert_eq!(default.selected_model(), Some("claude-opus-5-5"));
        assert_eq!(
            default.current_options().unwrap(),
            &BTreeMap::from([("reasoning".to_string(), "high".to_string())])
        );
        assert_eq!(
            default.bundle.profiles["gpt-5.6-sol"].options["fast"], "false",
            "别的模型的 profile 不动"
        );
        assert!(load_target(&agent, "g-a").unwrap().model_overridden());
        assert_eq!(
            load_target(&agent, "g-b").unwrap().model(),
            Some("claude-opus-5-5")
        );
    }

    #[test]
    fn an_unreadable_chat_overlay_fails_stop_instead_of_falling_back() {
        let temp = tempfile::tempdir().unwrap();
        let agent = make_agent(temp.path(), None);
        let path = chat_overlay_path(&agent, "g-a").unwrap();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"{not json").unwrap();
        assert_eq!(
            load_target(&agent, "g-a").unwrap_err().kind(),
            CursorConfigErrorKind::Read
        );
        assert!(load_target(&agent, "g-b").is_ok(), "别的群不受影响");
    }

    #[test]
    fn a_chat_overlay_rejects_stale_generations_and_unsafe_paths() {
        let temp = tempfile::tempdir().unwrap();
        let agent = make_agent(temp.path(), None);
        set_chat_selected_model(&agent, "g-a", 0, Some("gpt-5.6-sol")).unwrap();
        assert_eq!(
            set_chat_selected_model(&agent, "g-a", 0, Some("composer-2.5"))
                .unwrap_err()
                .kind(),
            CursorConfigErrorKind::Conflict
        );
        assert_eq!(
            load_target(&agent, "../escape").unwrap_err().kind(),
            CursorConfigErrorKind::UnsafePath
        );
        let outside = temp.path().join("outside");
        fs::create_dir_all(&outside).unwrap();
        let threads = agent.state_dir().join("threads");
        fs::create_dir_all(&threads).unwrap();
        std::os::unix::fs::symlink(&outside, threads.join("g-link")).unwrap();
        assert_eq!(
            set_chat_selected_model(&agent, "g-link", 0, Some("gpt-5.6-sol"))
                .unwrap_err()
                .kind(),
            CursorConfigErrorKind::UnsafePath
        );
        assert!(!outside.join(CURSOR_CONFIG_BUNDLE_FILE).exists());
    }

    #[test]
    fn missing_bundle_uses_bracket_seed_without_writing() {
        let temp = tempfile::tempdir().unwrap();
        let agent = make_agent(
            temp.path(),
            Some("gpt-5.6-sol[context=1m,reasoning=xhigh,fast=false]"),
        );

        let view = load(&agent).unwrap();

        assert!(!view.persisted);
        assert_eq!(view.bundle.version, CURSOR_CONFIG_BUNDLE_VERSION);
        assert_eq!(view.bundle.generation, 0);
        assert_eq!(view.selected_model(), Some("gpt-5.6-sol"));
        assert_eq!(
            view.current_options().unwrap(),
            &BTreeMap::from([
                ("context".to_string(), "1m".to_string()),
                ("fast".to_string(), "false".to_string()),
                ("reasoning".to_string(), "xhigh".to_string()),
            ])
        );
        assert!(!bundle_path(&agent).exists(), "只读 seed 不得产生文件");
    }

    #[test]
    fn mutation_waits_for_the_same_agents_apply_transaction() {
        let temp = tempfile::tempdir().unwrap();
        let agent = make_agent(temp.path(), None);
        let holder_agent = agent.clone();
        let mutator_agent = agent.clone();
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let holder = std::thread::spawn(move || {
            with_agent_lock(&holder_agent, || {
                entered_tx.send(()).unwrap();
                release_rx.recv().unwrap();
            })
            .unwrap();
        });
        entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();

        let (started_tx, started_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let mutator = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            let result = set_selected_model(&mutator_agent, 0, "gpt-5.6-sol");
            done_tx.send(result.is_ok()).unwrap();
        });
        started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(
            matches!(
                done_rx.recv_timeout(Duration::from_millis(100)),
                Err(mpsc::RecvTimeoutError::Timeout)
            ),
            "同 agent 的 bundle mutation 不得穿过正在应用 ACP 参数的事务锁"
        );

        release_tx.send(()).unwrap();
        holder.join().unwrap();
        assert!(done_rx.recv_timeout(Duration::from_secs(2)).unwrap());
        mutator.join().unwrap();
    }

    #[test]
    fn first_option_mutation_persists_the_whole_seed() {
        use std::os::unix::fs::MetadataExt;

        let temp = tempfile::tempdir().unwrap();
        let agent = make_agent(
            temp.path(),
            Some("gpt-5.6-sol[context=1m,reasoning=xhigh,fast=false]"),
        );

        let view = set_model_option_for_model(&agent, 0, "gpt-5.6-sol", "fast", "true").unwrap();

        assert!(view.persisted);
        assert_eq!(view.bundle.generation, 1);
        assert_eq!(
            view.current_options().unwrap().get("context").unwrap(),
            "1m"
        );
        assert_eq!(
            view.current_options().unwrap().get("reasoning").unwrap(),
            "xhigh"
        );
        assert_eq!(view.current_options().unwrap().get("fast").unwrap(), "true");
        assert!(bundle_path(&agent).is_file());
        assert_eq!(
            fs::metadata(bundle_path(&agent)).unwrap().mode() & 0o777,
            0o600
        );

        let changed_seed = make_agent(temp.path(), Some("other-model[fast=false]"));
        let reloaded = load(&changed_seed).unwrap();
        assert!(reloaded.persisted);
        assert_eq!(reloaded.selected_model(), Some("gpt-5.6-sol"));
        assert_eq!(
            reloaded.current_options().unwrap().get("fast").unwrap(),
            "true"
        );
    }

    #[test]
    fn first_live_option_write_commits_model_and_value_in_one_generation() {
        let temp = tempfile::tempdir().unwrap();
        let agent = make_agent(temp.path(), None);

        let view = set_model_option_for_model(&agent, 0, "gpt-5.6-sol", "fast", "true").unwrap();

        assert_eq!(view.bundle.generation, 1);
        assert_eq!(view.selected_model(), Some("gpt-5.6-sol"));
        assert_eq!(view.current_options().unwrap()["fast"], "true");
        let reloaded = load(&agent).unwrap();
        assert_eq!(reloaded.bundle.generation, 1);
        assert_eq!(reloaded.current_options().unwrap()["fast"], "true");
    }

    #[test]
    fn live_option_write_rejects_a_different_selected_model_without_partial_commit() {
        let temp = tempfile::tempdir().unwrap();
        let agent = make_agent(temp.path(), Some("model-a[fast=false]"));

        let error = set_model_option_for_model(&agent, 0, "model-b", "fast", "true")
            .expect_err("旧模型目录的参数不能写进另一个 profile");

        assert_eq!(error.kind(), CursorConfigErrorKind::InvalidInput);
        assert!(!bundle_path(&agent).exists());
        assert_eq!(load(&agent).unwrap().selected_model(), Some("model-a"));
    }

    #[test]
    fn live_tokens_are_opaque_and_never_trimmed_into_a_different_value() {
        let temp = tempfile::tempdir().unwrap();
        let agent = make_agent(temp.path(), None);

        for (model, id, value) in [
            (" model-a", "fast", "true"),
            ("model-a", " fast", "true"),
            ("model-a", "fast", " true "),
        ] {
            let error = set_model_option_for_model(&agent, 0, model, id, value)
                .expect_err("opaque token 不得 trim 后冒充目录中的另一个值");
            assert_eq!(error.kind(), CursorConfigErrorKind::InvalidInput);
            assert!(!bundle_path(&agent).exists());
        }
    }

    #[test]
    fn model_switch_preserves_each_models_profile() {
        let temp = tempfile::tempdir().unwrap();
        let agent = make_agent(temp.path(), Some("model-a[context=1m,fast=false]"));

        let on_b = set_selected_model(&agent, 0, "model-b").unwrap();
        assert_eq!(on_b.bundle.generation, 1);
        let configured_b =
            set_model_option_for_model(&agent, 1, "model-b", "reasoning", "high").unwrap();
        assert_eq!(configured_b.bundle.generation, 2);
        let back_on_a = set_selected_model(&agent, 2, "model-a").unwrap();

        assert_eq!(back_on_a.bundle.generation, 3);
        assert_eq!(
            back_on_a.current_options().unwrap().get("context").unwrap(),
            "1m"
        );
        assert_eq!(
            back_on_a.current_options().unwrap().get("fast").unwrap(),
            "false"
        );
        assert_eq!(
            back_on_a.bundle.profiles["model-b"].options["reasoning"],
            "high"
        );
    }

    #[test]
    fn stale_generation_cannot_overwrite_newer_state() {
        let temp = tempfile::tempdir().unwrap();
        let agent = make_agent(temp.path(), Some("model-a[fast=false]"));
        set_model_option_for_model(&agent, 0, "model-a", "fast", "true").unwrap();

        let error = set_model_option_for_model(&agent, 0, "model-a", "fast", "false").unwrap_err();

        assert_eq!(error.kind(), CursorConfigErrorKind::Conflict);
        let current = load(&agent).unwrap();
        assert_eq!(current.bundle.generation, 1);
        assert_eq!(current.current_options().unwrap()["fast"], "true");
    }

    #[test]
    fn concurrent_same_generation_has_exactly_one_winner() {
        use std::sync::{Arc, Barrier};

        let temp = tempfile::tempdir().unwrap();
        let agent = make_agent(temp.path(), Some("model-a[fast=false]"));
        let barrier = Arc::new(Barrier::new(3));
        let mut handles = Vec::new();
        for value in ["true", "false"] {
            let agent = agent.clone();
            let barrier = Arc::clone(&barrier);
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                set_model_option_for_model(&agent, 0, "model-a", "fast", value)
            }));
        }
        barrier.wait();
        let results: Vec<_> = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect();

        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        assert_eq!(
            results
                .iter()
                .filter_map(|result| result.as_ref().err())
                .filter(|error| error.kind() == CursorConfigErrorKind::Conflict)
                .count(),
            1
        );
        assert_eq!(load(&agent).unwrap().bundle.generation, 1);
    }

    #[test]
    fn malformed_bundle_fails_stop_and_public_error_is_redacted() {
        let temp = tempfile::tempdir().unwrap();
        let agent = make_agent(temp.path(), Some("fallback[fast=false]"));
        fs::write(bundle_path(&agent), br#"{"secret":"TOP_SECRET"}"#).unwrap();

        let error = load(&agent).unwrap_err();

        assert_eq!(error.kind(), CursorConfigErrorKind::Read);
        let public = error.to_string();
        assert!(!public.contains("TOP_SECRET"));
        assert!(!public.contains(temp.path().to_string_lossy().as_ref()));
        assert!(error.diagnostic().contains("解析"));
        assert!(set_model_option_for_model(&agent, 0, "fallback", "fast", "true").is_err());
        assert_eq!(
            fs::read(bundle_path(&agent)).unwrap(),
            br#"{"secret":"TOP_SECRET"}"#
        );
    }

    #[test]
    fn malformed_seed_fails_stop_without_creating_bundle() {
        let temp = tempfile::tempdir().unwrap();
        let agent = make_agent(temp.path(), Some("model-a[fast]"));

        let error = load(&agent).unwrap_err();

        assert_eq!(error.kind(), CursorConfigErrorKind::Read);
        assert!(!bundle_path(&agent).exists());
    }

    #[test]
    fn unknown_bundle_version_and_fields_fail_stop() {
        let temp = tempfile::tempdir().unwrap();
        let agent = make_agent(temp.path(), None);
        for content in [
            br#"{"version":2,"generation":0,"selected_model":null,"profiles":{}}"#.as_slice(),
            br#"{"version":1,"generation":0,"selected_model":null,"profiles":{},"future":true}"#
                .as_slice(),
        ] {
            fs::write(bundle_path(&agent), content).unwrap();
            assert_eq!(
                load(&agent).unwrap_err().kind(),
                CursorConfigErrorKind::Read
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn process_config_dirs_share_persistent_acp_sessions_across_restarts() {
        use std::os::unix::fs::MetadataExt;

        let temp = tempfile::tempdir().unwrap();
        let agent = make_agent(temp.path(), None);

        let first = process_config_dir(&agent).unwrap();
        let session_dir = first.path().join("acp-sessions").join("sid-1");
        fs::create_dir_all(&session_dir).unwrap();
        let db = session_dir.join("store.db");
        fs::write(&db, b"sqlite-bytes").unwrap();
        drop(first);

        let second = process_config_dir(&agent).unwrap();
        let second_db = second
            .path()
            .join("acp-sessions")
            .join("sid-1")
            .join("store.db");
        assert_eq!(fs::read(&second_db).unwrap(), b"sqlite-bytes");
        let shared = agent.state_dir().join(CURSOR_ACP_SESSIONS_DIR_NAME);
        assert!(shared.is_dir());
        assert_eq!(fs::metadata(&shared).unwrap().mode() & 0o777, 0o700);
    }

    #[cfg(unix)]
    #[test]
    fn acp_sessions_symlink_escape_is_rejected() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let agent = make_agent(temp.path(), None);
        symlink(
            outside.path(),
            agent.state_dir().join(CURSOR_ACP_SESSIONS_DIR_NAME),
        )
        .unwrap();

        let error = process_config_dir(&agent).unwrap_err();
        assert_eq!(error.kind(), CursorConfigErrorKind::UnsafePath);
    }

    #[test]
    fn process_config_dirs_are_private_unique_and_reclaimed() {
        use std::os::unix::fs::MetadataExt;

        let temp = tempfile::tempdir().unwrap();
        let agent = make_agent(temp.path(), None);

        let first = process_config_dir(&agent).unwrap();
        let second = process_config_dir(&agent).unwrap();
        let first_path = first.path().to_path_buf();
        let expected_parent =
            fs::canonicalize(agent.state_dir().join(CURSOR_CONFIG_DIR_NAME)).unwrap();

        assert_ne!(first.path(), second.path());
        assert_eq!(first.path().parent(), Some(expected_parent.as_path()));
        assert!(first.path().is_absolute());
        assert!(first.path().is_dir());
        assert_eq!(fs::metadata(first.path()).unwrap().mode() & 0o777, 0o700);
        drop(first);
        assert!(!first_path.exists());
        assert!(second.path().is_dir());
    }

    #[cfg(unix)]
    #[test]
    fn config_dir_symlink_escape_is_rejected() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let agent = make_agent(temp.path(), None);
        symlink(
            outside.path(),
            agent.state_dir().join(CURSOR_CONFIG_DIR_NAME),
        )
        .unwrap();

        let error = process_config_dir(&agent).unwrap_err();

        assert_eq!(error.kind(), CursorConfigErrorKind::UnsafePath);
        assert!(fs::read_dir(outside.path()).unwrap().next().is_none());
        assert!(!error
            .to_string()
            .contains(outside.path().to_string_lossy().as_ref()));
    }

    #[cfg(unix)]
    #[test]
    fn bundle_symlink_is_never_followed() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();
        fs::write(outside.path(), b"TOP_SECRET_OUTSIDE").unwrap();
        let agent = make_agent(temp.path(), Some("model-a[fast=false]"));
        symlink(outside.path(), bundle_path(&agent)).unwrap();

        let error = load(&agent).unwrap_err();

        assert_eq!(error.kind(), CursorConfigErrorKind::UnsafePath);
        assert!(!error.to_string().contains("TOP_SECRET_OUTSIDE"));
        assert_eq!(fs::read(outside.path()).unwrap(), b"TOP_SECRET_OUTSIDE");
    }
}
