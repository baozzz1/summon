//! `config.*`:字段表、三个层级的取值与来源、按键保存、YAML 原文模式。
//!
//! 取值口径:
//! - `value` 是本层文件里写的值(没写为 null);`effective` 是这一层的生效值;`source` 是生效值
//!   来自哪一层。前端拿到的永远是页面语义:`hide_thinking` 这类取反项由这里取反。
//! - 按群开关直接存页面语义(`/set` 写的 on/off),不取反。
//! - 敏感值(`runtime.env` 的字面值)换成占位符下发;保存时占位符原样带回就还原原值。

use std::collections::{BTreeMap, BTreeSet};

use serde::Deserialize;
use serde_json::Value;
use serde_yaml::Value as Yaml;
use summon_core::config::{builtin_messaging, Config, Messaging};
use summon_core::dashboard::authz::{self, AgentLists, Decision, Target};
use summon_core::dashboard::fields::{
    self, FieldDecl, FieldKind, FieldOption, Layer, Write, FIELDS,
};
use summon_core::dashboard::raw_check::{self, RawCheck, RawIssue};
use summon_core::dashboard::{redact, RawKind};
use summon_core::registry::Agent;
use summon_core::settings::{ChatOverrides, ResolvedSettings, SettingKey};
use summon_core::yaml_edit::{self, Edit};

use super::types::{
    ConfigChange, ConfigView, FieldValue, KeyEffect, RawFile, RawView, SaveResult, Scope,
};
use super::{
    authz_principal, internal, invalid, outcome, params, reply, shown_path, ApiEnv, ConfigFile,
};
use crate::runtime::daemon::dashboard_port;
use crate::runtime::dashboard::server::{ApiCtx, ApiError, ErrorCode};
use crate::runtime::dashboard::sessions::Principal;
use crate::runtime::dashboard::writer::{self, Change, WriteError};

// ── 公共工具 ────────────────────────────────────────────────────────────

/// 点分路径在 YAML 值里的位置。
pub(crate) fn yaml_at<'v>(root: &'v Yaml, dotted: &str) -> Option<&'v Yaml> {
    dotted
        .split('.')
        .try_fold(root, |cur, seg| cur.as_mapping()?.get(seg))
}

pub(crate) fn yaml_to_json(value: &Yaml) -> Value {
    serde_json::to_value(value).unwrap_or(Value::Null)
}

fn json_at(root: &Value, dotted: &str) -> Value {
    crate::runtime::dashboard::boot_snapshot::json_at(root, dotted)
}

/// `scope` 落在哪一层,改它要过哪道判权。
pub(crate) fn target_of(scope: &Scope) -> Target<'_> {
    match scope {
        Scope::Global => Target::Machine,
        Scope::Agent { agent } => Target::Agent(agent),
        Scope::Chat { agent, chat_id } => Target::Chat { agent, chat_id },
    }
}

fn layer_of(scope: &Scope) -> Layer {
    match scope {
        Scope::Global => Layer::Global,
        Scope::Agent { .. } => Layer::Agent,
        Scope::Chat { .. } => Layer::Chat,
    }
}

/// 审计与 `StatusView.revisions` 里这份文件的写法。
pub(crate) fn file_key(scope: &Scope) -> String {
    match scope {
        Scope::Global => "global".to_string(),
        Scope::Agent { agent } => format!("agent:{agent}"),
        Scope::Chat { agent, chat_id } => format!("chat:{agent}:{chat_id}"),
    }
}

pub(crate) fn global_messaging(cfg: &ConfigFile) -> Messaging {
    let mut m = builtin_messaging();
    m.apply(&cfg.parsed.messaging);
    m
}

pub(crate) fn agent_messaging(cfg: &ConfigFile, agent: &Agent) -> Messaging {
    let mut m = global_messaging(cfg);
    if let Some(o) = &agent.messaging {
        m.apply(o);
    }
    m
}

/// 页面语义:取反项把布尔翻过来。
fn to_page(field: &FieldDecl, value: Value) -> Value {
    match value {
        Value::Bool(b) if field.inverted => Value::Bool(!b),
        other => other,
    }
}

/// 敏感映射的值换成占位符。
fn masked(field: &FieldDecl, value: Value) -> Value {
    if !field.sensitive {
        return value;
    }
    match value {
        Value::Object(map) => Value::Object(
            map.into_iter()
                .map(|(k, v)| {
                    let shown = match &v {
                        Value::String(s) => Value::from(redact::mask(s)),
                        _ => Value::from(redact::PLACEHOLDER),
                    };
                    (k, shown)
                })
                .collect(),
        ),
        Value::Null => Value::Null,
        _ => Value::from(redact::PLACEHOLDER),
    }
}

/// 生效值的人话写法:`idle_retire_secs` 没开时是 null,页面上就是 0(= 不自动退出)。
fn normalized(key: &str, value: Value) -> Value {
    match (key, &value) {
        ("messaging.idle_retire_secs", Value::Null) => Value::from(0),
        _ => value,
    }
}

/// config.yaml 里存成「没写 = 由代码决定」的项,它们的内置值(结构体里是 `None`,序列化出来是
/// null)。只列默认值写在代码里、且有明确取值的几项;`tools.*` 这类「没写 = 按命令名找」照实给 null。
fn global_default(key: &str) -> Value {
    match key {
        "menubar.show_counts" => Value::from(summon_core::config::MenuBar::default().show_counts()),
        "menubar.icon" => Value::from(summon_core::config::menubar::DEFAULT_MENUBAR_ICON),
        "codex.sandbox" => Value::from(summon_core::codex_policy::DEFAULT_SANDBOX),
        "codex.approval_policy" => Value::from(summon_core::codex_policy::DEFAULT_APPROVAL_POLICY),
        _ => Value::Null,
    }
}

/// agent.yaml 里没写时的内置值(只列 serde 默认值确定、页面需要显示的几项)。
fn agent_default(key: &str) -> Value {
    match key {
        "runtime.cli" => Value::from("claude"),
        "role" => Value::from("agent"),
        "status" => Value::from("active"),
        "runtime.codex_skip_git_repo_check" => Value::from(false),
        _ => Value::Null,
    }
}

// ── 取值模型 ────────────────────────────────────────────────────────────

struct AgentLayer {
    agent: Agent,
    raw: Yaml,
    revision: String,
    messaging: Messaging,
}

pub(crate) struct ChatLayer {
    pub(crate) overrides: ChatOverrides,
    resolved: ResolvedSettings,
    pub(crate) revision: String,
}

pub(crate) fn chat_layer(env: &ApiEnv, agent: &Agent, chat_id: &str, m: &Messaging) -> ChatLayer {
    let chat_type = super::chats::chat_type_of(agent, chat_id);
    let session = env.port.session_name(agent, &chat_type, chat_id);
    let overrides = dashboard_port::read_chat_settings(agent, &session);
    let bytes = std::fs::read(dashboard_port::chat_settings_path(agent, &session)).ok();
    ChatLayer {
        resolved: ResolvedSettings::resolve(m, &overrides),
        overrides,
        revision: writer::revision(bytes.as_deref()),
    }
}

struct Model<'a> {
    cfg: &'a ConfigFile,
    global_json: Value,
    global_m: Messaging,
    agent: Option<AgentLayer>,
    chat: Option<ChatLayer>,
}

fn load_agent_layer(env: &ApiEnv, cfg: &ConfigFile, alias: &str) -> Result<AgentLayer, ApiError> {
    let agent = env.agent(alias)?;
    let text = std::fs::read_to_string(agent.manifest_path()).map_err(internal)?;
    let raw: Yaml = serde_yaml::from_str(&text).unwrap_or(Yaml::Null);
    Ok(AgentLayer {
        messaging: agent_messaging(cfg, &agent),
        revision: writer::revision(Some(text.as_bytes())),
        raw,
        agent,
    })
}

impl<'a> Model<'a> {
    /// 这一层合并后的 messaging(群这一层用它所属 agent 的)。
    fn messaging(&self) -> &Messaging {
        self.agent.as_ref().map_or(&self.global_m, |a| &a.messaging)
    }

    fn load(env: &ApiEnv, cfg: &'a ConfigFile, scope: &Scope) -> Result<Self, ApiError> {
        let mut model = Model {
            cfg,
            global_json: serde_json::to_value(&cfg.parsed).unwrap_or(Value::Null),
            global_m: global_messaging(cfg),
            agent: None,
            chat: None,
        };
        match scope {
            Scope::Global => {}
            Scope::Agent { agent } => model.agent = Some(load_agent_layer(env, cfg, agent)?),
            Scope::Chat { agent, chat_id } => {
                let layer = load_agent_layer(env, cfg, agent)?;
                model.chat = Some(chat_layer(env, &layer.agent, chat_id, &layer.messaging));
                model.agent = Some(layer);
            }
        }
        Ok(model)
    }

    fn revision(&self, layer: Layer) -> String {
        match layer {
            Layer::Chat => self.chat.as_ref().map(|c| c.revision.clone()),
            Layer::Agent => self.agent.as_ref().map(|a| a.revision.clone()),
            _ => None,
        }
        .unwrap_or_else(|| self.cfg.revision.clone())
    }

    fn agent_raw(&self, key: &str) -> Option<&Yaml> {
        self.agent.as_ref().and_then(|a| yaml_at(&a.raw, key))
    }

    fn global_raw(&self, key: &str) -> Option<&Yaml> {
        yaml_at(&self.cfg.raw, key)
    }

    /// 从 `from` 这一层往上,生效值来自哪一层(按群那一层另算)。
    fn source_above(&self, field: &FieldDecl, from: Layer) -> Layer {
        if from != Layer::Global
            && field.has_layer(Layer::Agent)
            && self.agent_raw(field.key).is_some()
        {
            return Layer::Agent;
        }
        if let Some(target) = field.inherits {
            if self.global_raw(target).is_some() {
                return Layer::Global;
            }
        }
        if field.has_layer(Layer::Global) && self.global_raw(field.key).is_some() {
            return Layer::Global;
        }
        Layer::Builtin
    }

    /// 盘上语义的生效值(按群那一层之外)。
    fn disk_effective(&self, field: &FieldDecl, layer: Layer) -> Value {
        if let Some(name) = field.key.strip_prefix("messaging.") {
            let m = match (&self.agent, layer) {
                (Some(a), Layer::Agent | Layer::Chat) => &a.messaging,
                _ => &self.global_m,
            };
            let json = serde_json::to_value(m).unwrap_or(Value::Null);
            return normalized(field.key, json_at(&json, name));
        }
        let global = |key: &str| match json_at(&self.global_json, key) {
            Value::Null => global_default(key),
            value => value,
        };
        if layer != Layer::Global && field.has_layer(Layer::Agent) {
            if let Some(raw) = self.agent_raw(field.key) {
                return yaml_to_json(raw);
            }
        }
        if field.has_layer(Layer::Global) {
            return global(field.key);
        }
        if let Some(raw) = self.agent_raw(field.key) {
            return yaml_to_json(raw);
        }
        match field.inherits {
            Some(target) => global(target),
            None => agent_default(field.key),
        }
    }

    /// 页面语义的生效值与来源。
    fn effective(&self, field: &FieldDecl, layer: Layer) -> (Value, Layer) {
        if layer == Layer::Chat {
            if let (Some(id), Some(chat)) = (field.setting, &self.chat) {
                if let Some(v) = chat.overrides.get(id) {
                    return (Value::Bool(v), Layer::Chat);
                }
                // 分级开关声明里的取值就是页面语义。
                return (
                    Value::Bool(chat.resolved.get(id)),
                    self.source_above(field, Layer::Agent),
                );
            }
        }
        (
            to_page(field, self.disk_effective(field, layer)),
            self.source_above(field, layer),
        )
    }

    /// 本层写的值(页面语义)。
    fn own(&self, field: &FieldDecl, layer: Layer) -> Value {
        match layer {
            Layer::Chat => field
                .setting
                .zip(self.chat.as_ref())
                .and_then(|(id, chat)| chat.overrides.get(id))
                .map(Value::Bool)
                .unwrap_or(Value::Null),
            Layer::Agent => self
                .agent_raw(field.key)
                .map(|v| to_page(field, yaml_to_json(v)))
                .unwrap_or(Value::Null),
            _ => self
                .global_raw(field.key)
                .map(|v| to_page(field, yaml_to_json(v)))
                .unwrap_or(Value::Null),
        }
    }
}

/// 下层覆盖了哪些项:agent.yaml 里写了同一个键(或继承它的键)的 agent,与按群开关有本群值的
/// 「agent · 群名」。
struct Overrides {
    agents: Vec<(String, Yaml)>,
    chats: Vec<(String, ChatOverrides)>,
}

impl Overrides {
    fn build(env: &ApiEnv, scope: &Scope) -> Self {
        let agents: Vec<Agent> = match scope {
            Scope::Global => env
                .agents()
                .into_iter()
                .filter(|a| !a.is_steward())
                .collect(),
            Scope::Agent { agent } => env.agent(agent).into_iter().collect(),
            Scope::Chat { .. } => Vec::new(),
        };
        let mut out = Overrides {
            agents: Vec::new(),
            chats: Vec::new(),
        };
        for agent in &agents {
            if matches!(scope, Scope::Global) {
                let raw = std::fs::read_to_string(agent.manifest_path())
                    .ok()
                    .and_then(|t| serde_yaml::from_str(&t).ok())
                    .unwrap_or(Yaml::Null);
                out.agents.push((agent.name.clone(), raw));
            }
            for (label, overrides) in super::chats::chat_overrides(agent) {
                out.chats
                    .push((format!("{} · {label}", agent.name), overrides));
            }
        }
        out
    }

    fn of(&self, field: &FieldDecl) -> Vec<String> {
        let mut names = Vec::new();
        let inheriting: Vec<&str> = FIELDS
            .iter()
            .filter(|f| f.inherits == Some(field.key))
            .map(|f| f.key)
            .collect();
        for (alias, raw) in &self.agents {
            let writes = |key: &str| yaml_at(raw, key).is_some();
            if (field.has_layer(Layer::Agent) && writes(field.key))
                || inheriting.iter().any(|k| writes(k))
            {
                names.push(alias.clone());
            }
        }
        if let Some(id) = field.setting.filter(|_| field.has_layer(Layer::Chat)) {
            for (label, overrides) in &self.chats {
                if overrides.get(id).is_some() {
                    names.push(label.clone());
                }
            }
        }
        names
    }
}

const READONLY_REASON: &str = "r64c24644da";

pub(crate) fn view(env: &ApiEnv, ctx: &ApiCtx, scope: &Scope) -> Result<ConfigView, ApiError> {
    let cfg = env.read_config()?;
    let model = Model::load(env, &cfg, scope)?;
    let layer = layer_of(scope);
    let decision = env.decide(ctx, &target_of(scope));
    let overrides = match scope {
        Scope::Chat { .. } => None,
        _ => Some(Overrides::build(env, scope)),
    };
    let reactions = (layer != Layer::Chat).then(|| reaction_options(model.messaging()));
    let values = FIELDS
        .iter()
        .filter(|f| f.has_layer(layer))
        .map(|field| {
            let (effective, source) = model.effective(field, layer);
            let (editable, deny_reason) = match (&decision, field.write) {
                (_, Write::Readonly) => (
                    false,
                    Some(::summon_core::i18n::label(READONLY_REASON).to_string()),
                ),
                (Decision::Allow, _) => (true, None),
                (Decision::Deny(why), _) => (false, Some(why.clone())),
            };
            FieldValue {
                key: field.key.to_string(),
                value: masked(field, model.own(field, layer)),
                effective: masked(field, effective),
                source,
                overridden_by: overrides.as_ref().map(|o| o.of(field)).unwrap_or_default(),
                editable,
                deny_reason,
            }
        })
        .collect();
    Ok(ConfigView {
        scope: scope.clone(),
        revision: model.revision(layer),
        values,
        reactions,
    })
}

/// 表情盘的候选:安全集按原顺序,含义取 `reaction_meanings`(没写含义的留空)。
fn reaction_options(m: &Messaging) -> Vec<FieldOption> {
    m.reactions
        .iter()
        .map(|key| FieldOption {
            value: key.clone(),
            label: m.reaction_meanings.get(key).cloned().unwrap_or_default(),
        })
        .collect()
}

// ── 方法:读 ────────────────────────────────────────────────────────────

pub(crate) fn schema(_ctx: &ApiCtx, _params: Value) -> Result<Value, ApiError> {
    reply(&fields::schema())
}

#[derive(Deserialize)]
struct GetParams {
    scope: Scope,
}

pub(crate) fn get(ctx: &ApiCtx, p: Value) -> Result<Value, ApiError> {
    let env = ctx.env()?;
    let GetParams { scope } = params(p)?;
    reply(&view(env, ctx, &scope)?)
}

#[derive(Deserialize)]
struct LarkUserParams {
    /// 页面上此刻填的 lark-cli 配置名(可能还没保存);空 = 默认配置。
    #[serde(default)]
    profile: Option<String>,
}

/// 「你的 open_id」旁的读取按钮:问 sentinel 会用的那份 lark-cli 配置里登录的是谁。
/// 结果只用来填全局设置,所以与改全局设置同一道判权。
pub(crate) fn lark_user(ctx: &ApiCtx, p: Value) -> Result<Value, ApiError> {
    let env = ctx.env()?;
    let LarkUserParams { profile } = params(p)?;
    env.require(ctx, &Target::Machine)?;
    let cfg = env.read_config()?;
    let profile = profile.as_deref().map(str::trim).filter(|s| !s.is_empty());
    let user = env.port.lark_user(&cfg.parsed, profile).map_err(|why| {
        ApiError::new(ErrorCode::Unavailable, why).with_key("sentinel.owner_open_id")
    })?;
    reply(&user)
}

// ── 方法:按键保存 ──────────────────────────────────────────────────────

#[derive(Deserialize)]
struct PatchParams {
    scope: Scope,
    revision: String,
    changes: Vec<ConfigChange>,
}

pub(crate) fn patch(ctx: &ApiCtx, p: Value) -> Result<Value, ApiError> {
    let env = ctx.env()?;
    let req: PatchParams = params(p)?;
    let keys: Vec<String> = req.changes.iter().map(|c| c.key.clone()).collect();
    let result = do_patch(env, ctx, &req);
    env.audit(ctx, &file_key(&req.scope), &keys, &outcome(&result));
    let saved = result?;
    if matches!(req.scope, Scope::Global)
        && req.changes.iter().any(|change| change.key == "language")
    {
        if let Ok(config) = env.read_config() {
            crate::runtime::localization::configure(&config.parsed);
        }
    }
    reply(&saved)
}

fn do_patch(env: &ApiEnv, ctx: &ApiCtx, req: &PatchParams) -> Result<SaveResult, ApiError> {
    env.require(ctx, &target_of(&req.scope))?;
    if req.changes.is_empty() {
        return Err(invalid(summon_i18n::text!("raf6a59ad07")));
    }
    let mut seen = BTreeSet::new();
    for change in &req.changes {
        if !seen.insert(change.key.as_str()) {
            return Err(invalid(summon_i18n::text!("r83957c4ac3")).with_key(change.key.clone()));
        }
    }
    match &req.scope {
        Scope::Chat { agent, chat_id } => patch_chat(env, agent, chat_id, req),
        Scope::Global => patch_yaml(env, ctx, None, req),
        Scope::Agent { agent } => patch_yaml(env, ctx, Some(agent), req),
    }
}

fn field_for(key: &str, layer: Layer) -> Result<&'static FieldDecl, ApiError> {
    let field = fields::field(key)
        .filter(|f| f.has_layer(layer))
        .ok_or_else(|| invalid(summon_i18n::format!("r5fa4b102b3")).with_key(key))?;
    if field.write == Write::Readonly {
        return Err(ApiError::new(
            ErrorCode::Forbidden,
            ::summon_core::i18n::label(READONLY_REASON),
        )
        .with_key(key));
    }
    Ok(field)
}

fn shape_error(field: &FieldDecl, what: &str) -> ApiError {
    invalid(summon_i18n::format!(
        "rec46f7d44f",
        summon_core::i18n::label(field.label)
    ))
    .with_key(field.key)
}

/// 页面提交的值 → 写进 YAML 的值(盘上语义)。`None` = 删键(恢复继承)。
fn to_disk(
    field: &FieldDecl,
    value: &Value,
    original: Option<&Yaml>,
) -> Result<Option<Yaml>, ApiError> {
    if value.is_null() {
        return Ok(None);
    }
    let yaml = match field.kind {
        FieldKind::Bool => {
            let b = value
                .as_bool()
                .ok_or_else(|| shape_error(field, summon_i18n::text!("r46c6d88cf3")))?;
            Yaml::Bool(if field.inverted { !b } else { b })
        }
        FieldKind::Enum => {
            let s = value
                .as_str()
                .ok_or_else(|| shape_error(field, summon_i18n::text!("r0095e8425d")))?;
            if !field.options.iter().any(|(v, _)| *v == s) {
                let all: Vec<&str> = field.options.iter().map(|(v, _)| *v).collect();
                return Err(shape_error(
                    field,
                    &summon_i18n::format!("r9f7b1811d2", all.join(" / ")),
                ));
            }
            Yaml::String(s.to_string())
        }
        FieldKind::Int | FieldKind::DurationSecs => {
            let n = value
                .as_i64()
                .ok_or_else(|| shape_error(field, summon_i18n::text!("r67e2bb6ef6")))?;
            if field.min.is_some_and(|min| n < min) || field.max.is_some_and(|max| n > max) {
                let range = match (field.min, field.max) {
                    (Some(min), Some(max)) => summon_i18n::format!("r409e51a937"),
                    (Some(min), None) => summon_i18n::format!("r4051c2b303"),
                    (None, Some(max)) => summon_i18n::format!("r1dca3e82c7"),
                    (None, None) => summon_i18n::text!("r67e2bb6ef6").to_string(),
                };
                return Err(shape_error(field, &range));
            }
            Yaml::Number(n.into())
        }
        FieldKind::String | FieldKind::Path => {
            let s = value
                .as_str()
                .ok_or_else(|| shape_error(field, summon_i18n::text!("rdd81861811")))?;
            if s.trim().is_empty() {
                return Ok(None);
            }
            Yaml::String(s.to_string())
        }
        FieldKind::StringList => {
            let items = value
                .as_array()
                .ok_or_else(|| shape_error(field, summon_i18n::text!("raedd6814ff")))?;
            let mut out = Vec::new();
            for item in items {
                let s = item
                    .as_str()
                    .ok_or_else(|| shape_error(field, summon_i18n::text!("rab422e9177")))?
                    .trim();
                if !s.is_empty() && !out.iter().any(|v: &Yaml| v.as_str() == Some(s)) {
                    out.push(Yaml::String(s.to_string()));
                }
            }
            Yaml::Sequence(out)
        }
        FieldKind::StringMap => {
            let map = value
                .as_object()
                .ok_or_else(|| shape_error(field, summon_i18n::text!("r0001890550")))?;
            let mut out = serde_yaml::Mapping::new();
            for (k, v) in map {
                let k = k.trim();
                let v = v
                    .as_str()
                    .ok_or_else(|| shape_error(field, summon_i18n::text!("rbc9fc287f7")))?;
                if k.is_empty() {
                    continue;
                }
                let v = if field.sensitive {
                    let before = original
                        .and_then(|o| o.as_mapping())
                        .and_then(|m| m.get(k))
                        .and_then(Yaml::as_str);
                    redact::unmask(&format!("{}.{k}", field.key), v, before)
                        .map_err(|e| invalid(e.to_string()).with_key(field.key))?
                } else {
                    v.to_string()
                };
                out.insert(Yaml::String(k.to_string()), Yaml::String(v));
            }
            Yaml::Mapping(out)
        }
    };
    Ok(Some(yaml))
}

/// 这份文件里字段表标成只读的键(`name`、`status`、`role`、飞书身份、tmux 会话名前缀……)。
/// 它们决定 daemon 认不认这个 agent、会话叫什么,一变就要重启 daemon,期间入站与控制台会话
/// 一律失效 —— 所以只有本机 owner 能改,表单与原文模式同一条。由字段表派生,加一个只读项不用
/// 改这里。`raw_check` 只管写法合不合法(对谁都一样),谁能改哪些键在这里判。
fn readonly_keys(kind: RawKind) -> impl Iterator<Item = &'static str> {
    FIELDS
        .iter()
        .filter(move |f| f.write == Write::Readonly && f.in_file(kind))
        .map(|f| f.key)
}

const READONLY_CHANGE_REASON: &str = "r5c67c0c062";

fn readonly_values(kind: RawKind, text: &str) -> Option<Vec<Option<Yaml>>> {
    let value: Yaml = serde_yaml::from_str(text).ok()?;
    Some(
        readonly_keys(kind)
            .map(|key| yaml_at(&value, key).cloned())
            .collect(),
    )
}

/// 非 owner 的写入不得改动只读项;改动前的内容读不出时证明不了没碰,也不让写。
fn check_readonly(
    ctx: &ApiCtx,
    kind: RawKind,
    old: Option<&str>,
    after: &str,
) -> Result<(), WriteError> {
    if matches!(ctx.principal, Principal::Owner) {
        return Ok(());
    }
    // 文件原本不存在 = 一项只读值也没有。
    let before = readonly_values(kind, old.unwrap_or(""));
    let unchanged = match (before, readonly_values(kind, after)) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    };
    if unchanged {
        Ok(())
    } else {
        Err(WriteError::Refused(
            ::summon_core::i18n::label(READONLY_CHANGE_REASON).to_string(),
        ))
    }
}

/// config.yaml 的锁内检查:非 owner 不得改动只读项。
pub(crate) fn config_guard(
    ctx: &ApiCtx,
) -> impl Fn(Option<&str>, &str) -> Result<(), WriteError> + '_ {
    move |old, after| {
        check_readonly(ctx, RawKind::Config, old, after)?;
        if matches!(ctx.principal, Principal::Owner) {
            return Ok(());
        }
        let next: Config =
            serde_yaml::from_str(after).map_err(|e| WriteError::Refused(e.to_string()))?;
        let env = ctx.env().map_err(|e| WriteError::Refused(e.message))?;
        for before in env.agents() {
            let mut changed = before.clone();
            if let Some(f) = &mut changed.feishu {
                if f.operators.is_none() {
                    f.operators = Some(next.feishu.operators.clone());
                }
            }
            if authz::would_lock_out(
                &authz_principal(&ctx.principal),
                &AgentLists::from_agent(&before, true),
                &AgentLists::from_agent(&changed, true),
            ) {
                return Err(WriteError::Refused(
                    summon_i18n::text!("r0cb911d882").into(),
                ));
            }
        }
        Ok(())
    }
}

/// 一切写 agent.yaml 的接口共用的锁内检查(本机 owner 不受限):
/// - 只读项(含飞书身份)只能在本机改,见 [`readonly_keys`];
/// - 改签发 agent 的名单,改完还得能操作它(`before` 是改动前解析出的 agent;读不出时没有)。
pub(crate) fn agent_guard<'a>(
    ctx: &'a ApiCtx,
    before: Option<&'a Agent>,
) -> impl Fn(Option<&str>, &str) -> Result<(), WriteError> + 'a {
    move |old, after| {
        if matches!(ctx.principal, Principal::Owner) {
            return Ok(());
        }
        check_readonly(ctx, RawKind::Agent, old, after)?;
        let Some(before) = before else {
            return Ok(());
        };
        let Ok(mut next) = serde_yaml::from_str::<Agent>(after) else {
            return Ok(());
        };
        next.dir = before.dir.clone();
        next.global_config_path = before.global_config_path.clone();
        let before_lists = AgentLists::from_agent(before, true);
        let after_lists =
            AgentLists::from_agent(&next, true).with_app_owner(before.app_owner.get());
        if authz::would_lock_out(
            &authz_principal(&ctx.principal),
            &before_lists,
            &after_lists,
        ) {
            return Err(WriteError::Refused(summon_i18n::format!(
                "r995b0a3ee5",
                before.name
            )));
        }
        Ok(())
    }
}

fn patch_yaml(
    env: &ApiEnv,
    ctx: &ApiCtx,
    alias: Option<&String>,
    req: &PatchParams,
) -> Result<SaveResult, ApiError> {
    let layer = if alias.is_some() {
        Layer::Agent
    } else {
        Layer::Global
    };
    let cfg = env.read_config()?;
    let agent = alias.map(|a| env.agent(a)).transpose()?;
    let (path, kind) = match &agent {
        Some(agent) => (agent.manifest_path(), RawKind::Agent),
        None => (env.config_path(), RawKind::Config),
    };
    let current_text = writer::read_optional(&path).map_err(internal)?;
    let current: Yaml = current_text
        .as_deref()
        .and_then(|t| serde_yaml::from_str(t).ok())
        .unwrap_or(Yaml::Null);
    let mut edits = Vec::new();
    let mut effects = Vec::new();
    let mut next: BTreeMap<&str, Option<Yaml>> = BTreeMap::new();
    for change in &req.changes {
        let field = field_for(&change.key, layer)?;
        let value = to_disk(field, &change.value, yaml_at(&current, field.key))?;
        edits.push(match &value {
            Some(v) => Edit::Set(yaml_edit::path(field.key), v.clone()),
            None => Edit::Remove(yaml_edit::path(field.key)),
        });
        effects.push(KeyEffect {
            key: field.key.to_string(),
            effect: field.effect,
        });
        next.insert(field.key, value);
    }
    if let Some(agent) = &agent {
        check_model_effort(agent, &next)?;
        super::chats::check_work_dir_map(env, agent, &current, &next)?;
    } else {
        check_sentinel(env, &cfg, &next)?;
        check_primary_agent(env, &next)?;
    }
    let saved = match &agent {
        Some(agent) => writer::write_yaml(
            &path,
            kind,
            Some(&req.revision),
            Change::Edits(edits),
            &agent_guard(ctx, Some(agent)),
        )?,
        None => writer::write_yaml(
            &path,
            kind,
            Some(&req.revision),
            Change::Edits(edits),
            &config_guard(ctx),
        )?,
    };
    if let Some(agent) = &agent {
        if !saved.unchanged {
            env.port.agent_written(&agent.name);
            if next.contains_key("runtime.chat_working_dirs") {
                super::chats::sync_resume_cwd_all(env, &agent.name, &current, &saved.after);
            }
        }
    }
    Ok(SaveResult {
        revision: saved.revision,
        effects,
        restart_pending: env.restart_pending(),
    })
}

/// model / effort / cli 改了就过与 `summon agents model|effort` 同一道校验。
fn check_model_effort(agent: &Agent, next: &BTreeMap<&str, Option<Yaml>>) -> Result<(), ApiError> {
    let touched = ["model", "effort", "runtime.cli"]
        .iter()
        .find(|k| next.contains_key(**k));
    let Some(key) = touched else {
        return Ok(());
    };
    let pick = |key: &str, current: Option<&str>| -> Option<String> {
        match next.get(key) {
            Some(v) => v.as_ref().and_then(Yaml::as_str).map(str::to_string),
            None => current.map(str::to_string),
        }
    };
    let cli = pick("runtime.cli", Some(agent.cli_kind())).unwrap_or_else(|| "claude".to_string());
    let model = pick("model", agent.model.as_deref());
    let effort = pick("effort", agent.effort.as_deref());
    summon_core::model_effort::validate(&cli, model.as_deref(), effort.as_deref()).map_err(|e| {
        let key = if next.contains_key("effort") && !next.contains_key("model") {
            "effort"
        } else {
            key
        };
        invalid(format!("{e}")).with_key(key)
    })
}

/// 开 sentinel 前与 Menu Bar 一样先核验 lark-cli 的用户身份。
fn check_sentinel(
    env: &ApiEnv,
    cfg: &ConfigFile,
    next: &BTreeMap<&str, Option<Yaml>>,
) -> Result<(), ApiError> {
    let enabling = next
        .get("sentinel.enabled")
        .is_some_and(|v| v.as_ref().and_then(Yaml::as_bool) == Some(true));
    if !enabling || cfg.parsed.sentinel.enabled {
        return Ok(());
    }
    let mut check = cfg.parsed.clone();
    if let Some(profile) = next.get("sentinel.lark_profile") {
        check.sentinel.lark_profile = profile.as_ref().and_then(Yaml::as_str).map(str::to_string);
    }
    env.port
        .sentinel_identity(&check)
        .map_err(|why| invalid(summon_i18n::format!("rfcab411aa7")).with_key("sentinel.enabled"))
}

/// 主 agent 只能指向此刻可用的 agent(与解析时的「可用」同一个判据);清空即自动。
fn check_primary_agent(env: &ApiEnv, next: &BTreeMap<&str, Option<Yaml>>) -> Result<(), ApiError> {
    let Some(Some(value)) = next.get("primary_agent") else {
        return Ok(());
    };
    let name = value.as_str().unwrap_or_default();
    let usable = env.agents().iter().any(|a| {
        a.name == name && crate::runtime::insights_analysis::usable(a, env.enabled(&a.name))
    });
    if usable {
        Ok(())
    } else {
        Err(invalid(summon_i18n::format!("r20e2772c71")).with_key("primary_agent"))
    }
}

fn patch_chat(
    env: &ApiEnv,
    alias: &str,
    chat_id: &str,
    req: &PatchParams,
) -> Result<SaveResult, ApiError> {
    let agent = env.agent(alias)?;
    let mut changes: Vec<(SettingKey, Option<bool>)> = Vec::new();
    let mut effects = Vec::new();
    for change in &req.changes {
        let field = field_for(&change.key, Layer::Chat)?;
        let id = field
            .setting
            .ok_or_else(|| invalid(summon_i18n::text!("r1b4a66eca9")).with_key(field.key))?;
        let value = match &change.value {
            Value::Null => None,
            Value::Bool(b) => Some(*b),
            _ => return Err(shape_error(field, summon_i18n::text!("r3a21bbdc7b"))),
        };
        changes.push((id, value));
        effects.push(KeyEffect {
            key: field.key.to_string(),
            effect: field.effect,
        });
    }
    let chat_type = super::chats::chat_type_of(&agent, chat_id);
    let session = env.port.session_name(&agent, &chat_type, chat_id);
    let expected = req.revision.clone();
    let updated = dashboard_port::update_chat_settings_if(&agent, &session, &changes, |current| {
        writer::revision(current) == expected
    })
    .map_err(|e| invalid(summon_i18n::format!("r1cb62757a1")))?;
    if updated.is_none() {
        return Err(WriteError::Conflict.into());
    }
    let bytes = std::fs::read(dashboard_port::chat_settings_path(&agent, &session)).ok();
    Ok(SaveResult {
        revision: writer::revision(bytes.as_deref()),
        effects,
        restart_pending: env.restart_pending(),
    })
}

// ── 方法:原文模式 ──────────────────────────────────────────────────────

struct RawTarget {
    kind: RawKind,
    path: std::path::PathBuf,
    agent: Option<Agent>,
    scope: Scope,
}

fn raw_target(env: &ApiEnv, file: &RawFile) -> Result<RawTarget, ApiError> {
    Ok(match file {
        RawFile::Global => RawTarget {
            kind: RawKind::Config,
            path: env.config_path(),
            agent: None,
            scope: Scope::Global,
        },
        RawFile::Agent { agent } => RawTarget {
            kind: RawKind::Agent,
            path: env.manifest_path(agent)?,
            // 读不出的那份也要能在原文模式里修:这时没有解析好的 agent。
            agent: env.agent(agent).ok(),
            scope: Scope::Agent {
                agent: agent.clone(),
            },
        },
    })
}

fn label(kind: RawKind) -> &'static str {
    match kind {
        RawKind::Config => "config.yaml",
        RawKind::Agent => "agent.yaml",
    }
}

fn redacted(kind: RawKind, text: &str) -> Result<(String, Vec<redact::Hidden>), ApiError> {
    redact::redact(kind, text).map_err(|e| invalid(summon_i18n::format!("r4fcd7b3c04")))
}

#[derive(Deserialize)]
struct RawGetParams {
    #[serde(flatten)]
    file: RawFile,
}

pub(crate) fn raw_get(ctx: &ApiCtx, p: Value) -> Result<Value, ApiError> {
    let env = ctx.env()?;
    let RawGetParams { file } = params(p)?;
    let target = raw_target(env, &file)?;
    let text = writer::read_optional(&target.path).map_err(internal)?;
    if text.is_none() && target.kind == RawKind::Agent {
        return Err(ApiError::new(
            ErrorCode::NotFound,
            summon_i18n::text!("r7939e67685"),
        ));
    }
    // 版本与正文必须出自同一次读取:否则两次读之间别处的改动会被这一页静默覆盖。
    let revision = writer::revision(text.as_deref().map(str::as_bytes));
    let original = text.unwrap_or_default();
    let (shown, _) = redacted(target.kind, &original)?;
    let editable = env.decide(ctx, &target_of(&target.scope)).is_allowed();
    reply(&RawView {
        revision,
        text: shown,
        path: shown_path(&target.path),
        editable,
    })
}

#[derive(Deserialize)]
struct RawCheckParams {
    #[serde(flatten)]
    file: RawFile,
    text: String,
}

/// 校验一份要保存的全文:错误与警告按还原了密钥的真实内容算,差异按占位符算(密钥不进页面)。
fn check_text(
    kind: RawKind,
    current: &str,
    submitted: &str,
) -> Result<(RawCheck, String), ApiError> {
    let (shown, hidden) = redacted(kind, current)?;
    let diff = raw_check::unified_diff(&shown, submitted, label(kind));
    match redact::restore(kind, submitted, &hidden) {
        Ok(restored) => {
            let mut check = raw_check::check(kind, current, &restored);
            check.diff = diff;
            Ok((check, restored))
        }
        Err(e) => Ok((
            RawCheck {
                diff,
                errors: vec![RawIssue {
                    key: Some("runtime.env".into()),
                    message: e.to_string(),
                    ..RawIssue::default()
                }],
                warnings: Vec::new(),
            },
            String::new(),
        )),
    }
}

pub(crate) fn raw_check(ctx: &ApiCtx, p: Value) -> Result<Value, ApiError> {
    let env = ctx.env()?;
    let req: RawCheckParams = params(p)?;
    let target = raw_target(env, &req.file)?;
    let current = writer::read_optional(&target.path)
        .map_err(internal)?
        .unwrap_or_default();
    reply(&check_text(target.kind, &current, &req.text)?.0)
}

#[derive(Deserialize)]
struct RawPutParams {
    #[serde(flatten)]
    file: RawFile,
    revision: String,
    text: String,
}

pub(crate) fn raw_put(ctx: &ApiCtx, p: Value) -> Result<Value, ApiError> {
    let env = ctx.env()?;
    let req: RawPutParams = params(p)?;
    let target = raw_target(env, &req.file)?;
    let mut keys = Vec::new();
    let result = (|| {
        env.require(ctx, &target_of(&target.scope))?;
        let current = writer::read_optional(&target.path)
            .map_err(internal)?
            .unwrap_or_default();
        let (check, restored) = check_text(target.kind, &current, &req.text)?;
        if let Some(first) = check.errors.first() {
            let error = invalid(summon_i18n::format!("r2afcb886cd", first.message));
            return Err(match &first.key {
                Some(key) => error.with_key(key.clone()),
                None => error,
            });
        }
        keys = changed_keys(&current, &restored);
        let saved = match target.kind {
            // 读不出的 agent.yaml 也走同一道检查(没有改动前的名单,只核对飞书身份)。
            RawKind::Agent => writer::write_yaml(
                &target.path,
                target.kind,
                Some(&req.revision),
                Change::Full(restored),
                &agent_guard(ctx, target.agent.as_ref()),
            )?,
            RawKind::Config => writer::write_yaml(
                &target.path,
                target.kind,
                Some(&req.revision),
                Change::Full(restored),
                &config_guard(ctx),
            )?,
        };
        if matches!(target.scope, Scope::Global) && keys.iter().any(|key| key == "language") {
            if let Ok(config) = env.read_config() {
                crate::runtime::localization::configure(&config.parsed);
            }
        }
        if let (Scope::Agent { agent }, false) = (&target.scope, saved.unchanged) {
            env.port.agent_written(agent);
        }
        Ok(SaveResult {
            revision: saved.revision,
            effects: effects_of(&keys),
            restart_pending: env.restart_pending(),
        })
    })();
    env.audit(ctx, &file_key(&target.scope), &keys, &outcome(&result));
    reply(&result?)
}

/// 两份全文之间改了哪些键(叶子或整段替换的那一级)。只给审计与生效时机用,不含值。
fn changed_keys(old: &str, new: &str) -> Vec<String> {
    fn walk(prefix: &str, a: &Yaml, b: &Yaml, out: &mut Vec<String>) {
        match (a.as_mapping(), b.as_mapping()) {
            (Some(ma), Some(mb)) => {
                let keys: BTreeSet<String> = ma
                    .keys()
                    .chain(mb.keys())
                    .filter_map(|k| k.as_str().map(str::to_string))
                    .collect();
                for k in keys {
                    let path = if prefix.is_empty() {
                        k.clone()
                    } else {
                        format!("{prefix}.{k}")
                    };
                    let null = Yaml::Null;
                    walk(
                        &path,
                        ma.get(k.as_str()).unwrap_or(&null),
                        mb.get(k.as_str()).unwrap_or(&null),
                        out,
                    );
                }
            }
            _ if a != b => out.push(prefix.to_string()),
            _ => {}
        }
    }
    let parse = |t: &str| serde_yaml::from_str::<Yaml>(t).unwrap_or(Yaml::Null);
    let mut out = Vec::new();
    walk("", &parse(old), &parse(new), &mut out);
    out.retain(|k| !k.is_empty());
    out
}

/// 改动的键各自什么时候生效(字段表里认得的那些;整段替换时取段下各项)。
fn effects_of(keys: &[String]) -> Vec<KeyEffect> {
    let mut out: Vec<KeyEffect> = Vec::new();
    for key in keys {
        for field in FIELDS.iter().filter(|f| {
            f.key == key.as_str()
                || key.starts_with(&format!("{}.", f.key))
                || f.key.starts_with(&format!("{key}."))
        }) {
            if !out.iter().any(|e| e.key == field.key) {
                out.push(KeyEffect {
                    key: field.key.to_string(),
                    effect: field.effect,
                });
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::super::testkit::*;
    use super::*;
    use crate::runtime::dashboard::sessions::ChatKind;
    use serde_json::json;

    fn value<'v>(view: &'v Value, key: &str) -> &'v Value {
        view["values"]
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["key"] == key)
            .unwrap_or_else(|| panic!("没有 {key}"))
    }

    fn kit_with_agent() -> (Kit, Agent) {
        let kit = Kit::new();
        let agent = kit.add_agent(
            "starlight",
            &agent_yaml(
                "starlight",
                "星光",
                "claude",
                "model: claude-opus-4-8\nmessaging:\n  hide_thinking: true\n",
            ),
        );
        (kit, agent)
    }

    #[test]
    fn operators_show_inherited_source_and_global_edit_cannot_lock_out_issuer() {
        let kit = Kit::new();
        kit.add_agent(
            "starlight",
            &agent_yaml("starlight", "星光", "claude", "")
                .replace(&format!("  operators: [{ME_UNION}]\n"), ""),
        );
        std::fs::write(
            kit.home().join("config.yaml"),
            format!("feishu:\n  operators: [{ME_UNION}]\n"),
        )
        .unwrap();
        let view = kit
            .call(
                &kit.owner(),
                get,
                json!({"scope": {"level":"agent", "agent":"starlight"}}),
            )
            .unwrap();
        assert_eq!(value(&view, "feishu.operators")["source"], "global");
        assert_eq!(
            value(&view, "feishu.operators")["effective"],
            json!([ME_UNION])
        );
        let ctx = kit.feishu(issuer("starlight", P2P, ChatKind::P2p));
        assert!(config_guard(&ctx)(Some(""), "feishu:\n  operators: [on_else]\n").is_err());
        let agent = kit.env().agent("starlight").unwrap();
        assert_eq!(agent.effective_operators().unwrap(), [ME_UNION]);
    }

    #[test]
    fn layers_report_value_effective_and_source() {
        let (kit, agent) = kit_with_agent();
        kit.add_chat(&agent, "group", GROUP, "示例群");
        let ctx = kit.owner();
        let global = kit
            .call(&ctx, get, json!({"scope": {"level": "global"}}))
            .unwrap();
        let cot = value(&global, "messaging.cot_visible");
        assert_eq!(cot["value"], true);
        assert_eq!(cot["source"], "global");
        let thinking = value(&global, "messaging.hide_thinking");
        assert_eq!(thinking["value"], Value::Null, "整机没写");
        assert_eq!(thinking["source"], "builtin");
        assert_eq!(
            thinking["overridden_by"],
            json!(["starlight"]),
            "agent.yaml 覆盖了它"
        );
        assert_eq!(cot["overridden_by"], json!([]));

        let agent_view = kit
            .call(
                &ctx,
                get,
                json!({"scope": {"level": "agent", "agent": "starlight"}}),
            )
            .unwrap();
        let thinking = value(&agent_view, "messaging.hide_thinking");
        assert_eq!(
            thinking["value"], false,
            "存的是隐藏,页面语义是「显示思考」= 关"
        );
        assert_eq!(thinking["effective"], false);
        assert_eq!(thinking["source"], "agent");
        let cli = value(&agent_view, "runtime.cli");
        assert_eq!(
            (cli["value"].clone(), cli["source"].clone()),
            (json!("claude"), json!("agent"))
        );
        let resume = value(&agent_view, "runtime.resume");
        assert_eq!(resume["value"], Value::Null);
        assert_eq!(resume["effective"], true, "继承整机的 resume_sessions");
        assert_eq!(resume["source"], "builtin");
        assert_eq!(
            value(&agent_view, "runtime.codex_sandbox")["effective"],
            summon_core::codex_policy::DEFAULT_SANDBOX,
            "代码里的默认值也要显示出来"
        );
        assert_eq!(
            value(&agent_view, "name")["editable"],
            false,
            "只读项不可改"
        );
        assert!(agent_view["revision"].as_str().unwrap().len() == 16);
    }

    #[test]
    fn chat_scope_values_are_tristate() {
        let (kit, agent) = kit_with_agent();
        kit.add_chat(&agent, "group", GROUP, "示例群");
        let ctx = kit.owner();
        let scope = json!({"level": "chat", "agent": "starlight", "chat_id": GROUP});
        let view = kit.call(&ctx, get, json!({ "scope": scope })).unwrap();
        let keys: Vec<&str> = view["values"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v["key"].as_str().unwrap())
            .collect();
        assert_eq!(keys.len(), summon_core::settings::chat_scoped().count());
        let thinking = value(&view, "messaging.hide_thinking");
        assert_eq!(thinking["value"], Value::Null, "继承");
        assert_eq!(thinking["effective"], false);
        assert_eq!(thinking["source"], "agent");
        assert_eq!(view["revision"], "absent");

        // 开、关、继承各写一次,读回三态。
        let saved = kit
            .call(
                &ctx,
                patch,
                json!({"scope": scope, "revision": "absent", "changes": [
                    {"key": "messaging.hide_thinking", "value": true},
                    {"key": "messaging.badge_effort", "value": false},
                ]}),
            )
            .unwrap();
        let view = kit.call(&ctx, get, json!({ "scope": scope })).unwrap();
        assert_eq!(view["revision"], saved["revision"]);
        let thinking = value(&view, "messaging.hide_thinking");
        assert_eq!(
            (thinking["value"].clone(), thinking["source"].clone()),
            (json!(true), json!("chat"))
        );
        assert_eq!(value(&view, "messaging.badge_effort")["value"], false);
        kit.call(
            &ctx,
            patch,
            json!({"scope": scope, "revision": saved["revision"], "changes": [
                {"key": "messaging.badge_effort", "value": null},
            ]}),
        )
        .unwrap();
        let view = kit.call(&ctx, get, json!({ "scope": scope })).unwrap();
        assert_eq!(value(&view, "messaging.badge_effort")["value"], Value::Null);
        // 群层的覆盖在 agent 页与整机页上都看得到。
        let agent_view = kit
            .call(
                &ctx,
                get,
                json!({"scope": {"level": "agent", "agent": "starlight"}}),
            )
            .unwrap();
        assert_eq!(
            value(&agent_view, "messaging.hide_thinking")["overridden_by"],
            json!(["starlight · 示例群"])
        );
        // 盘上就是 `/set` 写的那份文件。
        let session = agent.tmux_name_for_chat("group", GROUP, false);
        let text =
            std::fs::read_to_string(dashboard_port::chat_settings_path(&agent, &session)).unwrap();
        assert!(text.contains("cot.thinking=on\n") && text.contains("badge.effort=inherit\n"));
        // 旧版本号再写被拒,文件不动。
        let err = kit
            .call(
                &ctx,
                patch,
                json!({"scope": scope, "revision": "absent", "changes": [
                    {"key": "messaging.cot_visible", "value": false},
                ]}),
            )
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::Conflict);
        assert_eq!(
            std::fs::read_to_string(dashboard_port::chat_settings_path(&agent, &session)).unwrap(),
            text
        );
    }

    #[test]
    fn patch_global_writes_config_and_reports_effects() {
        let (kit, _agent) = kit_with_agent();
        let ctx = kit.owner();
        let view = kit
            .call(&ctx, get, json!({"scope": {"level": "global"}}))
            .unwrap();
        let saved = kit
            .call(
                &ctx,
                patch,
                json!({"scope": {"level": "global"}, "revision": view["revision"], "changes": [
                    {"key": "messaging.hide_thinking", "value": false},
                    {"key": "dashboard.port", "value": 7800},
                ]}),
            )
            .unwrap();
        let text = kit.read("config.yaml");
        assert!(
            text.contains("hide_thinking: true"),
            "页面「显示思考=关」存成隐藏:{text}"
        );
        assert!(text.contains("cot_visible: true  # 显示过程"), "注释原样");
        assert_eq!(
            saved["effects"],
            json!([{"key": "messaging.hide_thinking", "effect": "next_turn"},
                   {"key": "dashboard.port", "effect": "restart"}])
        );
        assert_eq!(
            saved["restart_pending"],
            json!([{"key": "dashboard.port", "file": "global"}])
        );
        // 恢复继承 = 删键。
        kit.call(
            &ctx,
            patch,
            json!({"scope": {"level": "global"}, "revision": saved["revision"], "changes": [
                {"key": "dashboard.port", "value": null},
            ]}),
        )
        .unwrap();
        assert!(!kit.read("config.yaml").contains("port: 7800"));
    }

    #[test]
    fn document_comment_timeout_config_obeys_scope_inheritance_and_validation() {
        let (kit, _) = kit_with_agent();
        let ctx = kit.owner();
        let key = "messaging.document_comment_timeout_secs";
        let global = json!({"level":"global"});
        let agent = json!({"level":"agent","agent":"starlight"});
        let read = |scope: Value| kit.call(&ctx, get, json!({"scope":scope})).unwrap();
        let write = |scope: Value, new: Value| {
            let view = read(scope.clone());
            kit.call(&ctx, patch, json!({"scope":scope,"revision":view["revision"],"changes":[{"key":key,"value":new}]}))
        };
        assert_eq!(value(&read(global.clone()), key)["effective"], 300);
        write(global.clone(), json!(120)).unwrap();
        let inherited = read(agent.clone());
        assert_eq!(value(&inherited, key)["source"], "global");
        assert_eq!(value(&inherited, key)["effective"], 120);
        write(agent.clone(), json!(600)).unwrap();
        assert_eq!(value(&read(agent.clone()), key)["source"], "agent");
        assert_eq!(value(&read(agent.clone()), key)["effective"], 600);
        assert_eq!(value(&read(global.clone()), key)["effective"], 120);
        write(agent.clone(), Value::Null).unwrap();
        assert_eq!(value(&read(agent.clone()), key)["effective"], 120);
        let before = kit.read("config.yaml");
        for bad in [json!(0), json!(-1), json!(1.5)] {
            let err = write(global.clone(), bad).unwrap_err();
            assert_eq!(err.code, ErrorCode::Invalid);
            assert_eq!(err.key.as_deref(), Some(key));
        }
        assert_eq!(before, kit.read("config.yaml"));
        let remote = kit.feishu(crate::runtime::dashboard::sessions::Issuer {
            open_id: "ou_00000000000000000000000000000004".into(),
            union_id: PEER_UNION.into(),
            ..issuer("starlight", GROUP, ChatKind::Group)
        });
        let view = read(global.clone());
        let err = kit.call(&remote, patch, json!({"scope":global,"revision":view["revision"],"changes":[{"key":key,"value":1}]})).unwrap_err();
        assert_eq!(err.code, ErrorCode::Forbidden);
        let field = summon_core::dashboard::fields::field(key).unwrap();
        assert!(!field.has_layer(summon_core::dashboard::fields::Layer::Chat));
    }

    #[test]
    fn patch_rejects_bad_values_with_the_key() {
        let (kit, _agent) = kit_with_agent();
        let ctx = kit.owner();
        let rev = kit
            .call(&ctx, get, json!({"scope": {"level": "global"}}))
            .unwrap()["revision"]
            .clone();
        for (key, value) in [
            ("messaging.idle_ttl_secs", json!(5)),
            ("codex.sandbox", json!("everything")),
            ("messaging.cot_visible", json!("yes")),
            ("nonexistent.key", json!(1)),
            ("runtime.cli", json!("codex")),
        ] {
            let err = kit
                .call(
                    &ctx,
                    patch,
                    json!({"scope": {"level": "global"}, "revision": rev, "changes": [{"key": key, "value": value}]}),
                )
                .unwrap_err();
            assert_eq!(err.code, ErrorCode::Invalid, "{key}");
            assert_eq!(err.key.as_deref(), Some(key), "{key}");
        }
        // model 非法:与 `summon agents model` 同一道校验。
        let arev = kit
            .call(
                &ctx,
                get,
                json!({"scope": {"level": "agent", "agent": "starlight"}}),
            )
            .unwrap()["revision"]
            .clone();
        let err = kit
            .call(
                &ctx,
                patch,
                json!({"scope": {"level": "agent", "agent": "starlight"}, "revision": arev, "changes": [
                    {"key": "effort", "value": "ultra"}
                ]}),
            )
            .unwrap_err();
        assert_eq!(
            (err.code, err.key.as_deref()),
            (ErrorCode::Invalid, Some("effort"))
        );
        let err = kit
            .call(
                &ctx,
                patch,
                json!({"scope": {"level": "agent", "agent": "starlight"}, "revision": arev, "changes": [
                    {"key": "name", "value": "x"}
                ]}),
            )
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::Forbidden, "只读项");
    }

    #[test]
    fn agent_patch_wakes_the_dispatcher_and_keeps_comments() {
        let (kit, _agent) = kit_with_agent();
        let ctx = kit.owner();
        let scope = json!({"level": "agent", "agent": "starlight"});
        let rev = kit.call(&ctx, get, json!({ "scope": scope })).unwrap()["revision"].clone();
        kit.call(
            &ctx,
            patch,
            json!({"scope": scope, "revision": rev, "changes": [
                {"key": "effort", "value": "high"},
                {"key": "messaging.cot_tool_details", "value": false},
            ]}),
        )
        .unwrap();
        let text = kit.read("agents/starlight/agent.yaml");
        assert!(text.starts_with("# 星光 的配置\n"));
        assert!(text.contains("effort: high"));
        assert_eq!(kit.port.written.lock().unwrap().as_slice(), ["starlight"]);
    }

    #[test]
    fn sensitive_env_round_trips_through_placeholders() {
        let kit = Kit::new();
        kit.add_agent(
            "starlight",
            &agent_yaml("starlight", "星光", "claude", "").replace(
                "  working_dir: /tmp\n",
                "  working_dir: /tmp\n  env:\n    API_TOKEN: sk-literal-secret-0001\n    PROXY: ${CORP_PROXY}\n",
            ),
        );
        let ctx = kit.owner();
        let scope = json!({"level": "agent", "agent": "starlight"});
        let view = kit.call(&ctx, get, json!({ "scope": scope })).unwrap();
        let env = value(&view, "runtime.env");
        assert_eq!(
            env["value"],
            json!({"API_TOKEN": redact::PLACEHOLDER, "PROXY": "${CORP_PROXY}"})
        );
        assert!(!view.to_string().contains("sk-literal-secret"));
        // 占位符原样带回 = 不改;改了引用的那一项照写。
        kit.call(
            &ctx,
            patch,
            json!({"scope": scope, "revision": view["revision"], "changes": [
                {"key": "runtime.env", "value": {"API_TOKEN": redact::PLACEHOLDER, "PROXY": "${OTHER}"}},
            ]}),
        )
        .unwrap();
        let text = kit.read("agents/starlight/agent.yaml");
        let saved: Yaml = serde_yaml::from_str(&text).unwrap();
        assert_eq!(
            yaml_at(&saved, "runtime.env.API_TOKEN").and_then(Yaml::as_str),
            Some("sk-literal-secret-0001"),
            "{text}"
        );
        assert_eq!(
            yaml_at(&saved, "runtime.env.PROXY").and_then(Yaml::as_str),
            Some("${OTHER}")
        );
        let raw = kit
            .call(
                &ctx,
                raw_get,
                json!({"file": "agent", "agent": "starlight"}),
            )
            .unwrap();
        assert!(!raw["text"].as_str().unwrap().contains("sk-literal-secret"));
    }

    #[test]
    fn raw_get_hides_literal_env() {
        let kit = Kit::new();
        kit.add_agent(
            "starlight",
            "name: starlight\nruntime:\n  env:\n    KEY: literal-value-123\n",
        );
        let raw = kit
            .call(
                &kit.owner(),
                raw_get,
                json!({"file": "agent", "agent": "starlight"}),
            )
            .unwrap();
        let text = raw["text"].as_str().unwrap();
        assert!(!text.contains("literal-value-123") && text.contains(redact::PLACEHOLDER));
        assert_eq!(
            raw["revision"],
            writer::revision(Some(kit.read("agents/starlight/agent.yaml").as_bytes()))
        );
    }

    #[test]
    fn raw_check_and_put_keep_secrets_and_report_issues() {
        let kit = Kit::new();
        kit.add_agent(
            "starlight",
            "name: starlight\nruntime:\n  cli: claude\n  env:\n    KEY: literal-value-123\n",
        );
        let ctx = kit.owner();
        let raw = kit
            .call(
                &ctx,
                raw_get,
                json!({"file": "agent", "agent": "starlight"}),
            )
            .unwrap();
        let shown = raw["text"].as_str().unwrap().to_string();
        let edited = format!("{shown}display: 新名字\nrutime:\n  cli: codex\n");
        let check = kit
            .call(
                &ctx,
                super::raw_check,
                json!({"file": "agent", "agent": "starlight", "text": edited}),
            )
            .unwrap();
        assert!(check["diff"].as_str().unwrap().contains("+display: 新名字"));
        assert!(!check.to_string().contains("literal-value-123"));
        assert!(
            check["warnings"]
                .as_array()
                .unwrap()
                .iter()
                .any(|w| w["key"] == "rutime"),
            "未知键给警告:{check}"
        );
        let saved = kit
            .call(
                &ctx,
                raw_put,
                json!({"file": "agent", "agent": "starlight", "revision": raw["revision"], "text": edited}),
            )
            .unwrap();
        let text = kit.read("agents/starlight/agent.yaml");
        assert!(text.contains("KEY: literal-value-123") && text.contains("display: 新名字"));
        assert_eq!(saved["revision"], writer::revision(Some(text.as_bytes())));
        assert!(saved["effects"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["key"] == "display"));
        // 解析失败不写。
        let err = kit
            .call(
                &ctx,
                raw_put,
                json!({"file": "agent", "agent": "starlight", "revision": saved["revision"], "text": "name: [\n"}),
            )
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::Invalid);
        assert_eq!(kit.read("agents/starlight/agent.yaml"), text);
    }

    #[test]
    fn broken_manifest_is_listed_and_raw_editable() {
        let kit = Kit::new();
        kit.add_agent("starlight", &agent_yaml("starlight", "星光", "claude", ""));
        let broken_dir = kit.agents_dir().join("saturn");
        std::fs::create_dir_all(&broken_dir).unwrap();
        let bad = "name: saturn\nruntime:\n  cli: claude\n   broken: [\n";
        std::fs::write(broken_dir.join("agent.yaml"), bad).unwrap();
        let ctx = kit.owner();
        let list = kit
            .call(&ctx, super::super::agents::list, json!({}))
            .unwrap();
        let rows = list.as_array().unwrap();
        assert_eq!(rows.len(), 2);
        let saturn = rows.iter().find(|r| r["alias"] == "saturn").unwrap();
        assert!(saturn["load_error"].as_str().unwrap().len() > 3);
        assert_eq!(
            rows.iter().find(|r| r["alias"] == "starlight").unwrap()["routable"],
            true
        );
        // 原文模式打得开、修得好。
        let raw = kit
            .call(&ctx, raw_get, json!({"file": "agent", "agent": "saturn"}))
            .unwrap();
        assert_eq!(raw["text"], bad);
        kit.call(
            &ctx,
            raw_put,
            json!({"file": "agent", "agent": "saturn", "revision": raw["revision"], "text": "name: saturn\nruntime:\n  cli: claude\n"}),
        )
        .unwrap();
        let list = kit
            .call(&ctx, super::super::agents::list, json!({}))
            .unwrap();
        assert!(list
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r.get("load_error").is_none()));
    }

    #[test]
    fn overridden_by_names_agents_and_chats() {
        let kit = Kit::new();
        let a = kit.add_agent(
            "starlight",
            &agent_yaml(
                "starlight",
                "星光",
                "claude",
                "messaging:\n  badge_effort: false\n",
            ),
        );
        kit.add_agent(
            "arcadia",
            &agent_yaml("arcadia", "阿卡迪亚", "codex", "").replace(
                "  working_dir: /tmp\n",
                "  working_dir: /tmp\n  resume: false\n",
            ),
        );
        kit.add_chat(&a, "group", GROUP, "示例群");
        let session = a.tmux_name_for_chat("group", GROUP, false);
        dashboard_port::update_chat_settings_if(
            &a,
            &session,
            &[(SettingKey::BadgeEffort, Some(true))],
            |_| true,
        )
        .unwrap();
        let view = kit
            .call(&kit.owner(), get, json!({"scope": {"level": "global"}}))
            .unwrap();
        assert_eq!(
            value(&view, "messaging.badge_effort")["overridden_by"],
            json!(["starlight", "starlight · 示例群"])
        );
        assert_eq!(
            value(&view, "resume_sessions")["overridden_by"],
            json!(["arcadia"]),
            "agent 用 runtime.resume 覆盖整机的 resume_sessions"
        );
    }

    #[test]
    fn feishu_identity_edits_follow_the_issuer_chat() {
        let (kit, agent) = kit_with_agent();
        kit.add_chat(&agent, "group", GROUP, "示例群");
        // 只在本群操作名单里的人:改得了本群、改不了 agent 级与整机。
        let yaml = agent_yaml("starlight", "星光", "claude", "").replace(
            &format!("operators: [{ME_UNION}]"),
            &format!("operators: [{PEER_UNION}]\n  chat_operators:\n    {GROUP}: [{ME_UNION}]"),
        );
        std::fs::write(agent.manifest_path(), yaml).unwrap();
        let ctx = kit.feishu(issuer("starlight", GROUP, ChatKind::Group));
        let chat = json!({"level": "chat", "agent": "starlight", "chat_id": GROUP});
        let view = kit.call(&ctx, get, json!({ "scope": chat })).unwrap();
        assert_eq!(value(&view, "messaging.cot_visible")["editable"], true);
        kit.call(
            &ctx,
            patch,
            json!({"scope": chat, "revision": view["revision"], "changes": [{"key": "messaging.cot_visible", "value": false}]}),
        )
        .unwrap();
        let agent_scope = json!({"level": "agent", "agent": "starlight"});
        let view = kit
            .call(&ctx, get, json!({ "scope": agent_scope }))
            .unwrap();
        let row = value(&view, "messaging.cot_visible");
        assert_eq!(row["editable"], false);
        assert!(row["deny_reason"]
            .as_str()
            .unwrap()
            .contains("本群操作名单"));
        let err = kit
            .call(
                &ctx,
                patch,
                json!({"scope": agent_scope, "revision": view["revision"], "changes": [{"key": "display", "value": "x"}]}),
            )
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::Forbidden);
        let err = kit
            .call(
                &ctx,
                patch,
                json!({"scope": {"level": "global"}, "revision": "x", "changes": [{"key": "poll_ms", "value": 500}]}),
            )
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::Forbidden);
    }

    /// 别的 agent:两份名单都空时,只有它的飞书应用所有者(按 union_id 认)改得了。
    #[test]
    fn other_agent_needs_explicit_grant_or_app_ownership() {
        let (kit, _agent) = kit_with_agent();
        let open = "name: arcadia\nruntime:\n  cli: codex\nfeishu:\n  app_id_env: DASH_TEST_ARCADIA_APP_ID\n";
        kit.add_agent("arcadia", open);
        let ctx = kit.feishu(issuer("starlight", P2P, ChatKind::P2p));
        let scope = json!({"level": "agent", "agent": "arcadia"});
        let view = kit.call(&ctx, get, json!({ "scope": scope })).unwrap();
        assert_eq!(
            value(&view, "display")["editable"],
            false,
            "私聊人人可操作那条不算数"
        );
        kit.set_owner("arcadia", ME_UNION);
        let view = kit.call(&ctx, get, json!({ "scope": scope })).unwrap();
        assert_eq!(value(&view, "display")["editable"], true);
        kit.call(
            &ctx,
            patch,
            json!({"scope": scope, "revision": view["revision"], "changes": [{"key": "display", "value": "二号"}]}),
        )
        .unwrap();
    }

    /// 飞书身份(name 与 feishu.app_id_env / app_secret_env / scope)一改,daemon 就不认这个 agent 了:
    /// 只有本机 owner 改得了,任何写 agent.yaml 的接口都一样。
    #[test]
    fn identity_fields_change_only_from_this_machine() {
        let (kit, agent) = kit_with_agent();
        let ctx = kit.feishu(issuer("starlight", P2P, ChatKind::P2p));
        let before = kit.read("agents/starlight/agent.yaml");
        let raw = kit
            .call(
                &ctx,
                raw_get,
                json!({"file": "agent", "agent": "starlight"}),
            )
            .unwrap();
        let shown = raw["text"].as_str().unwrap().to_string();
        for (what, edited) in [
            ("name", shown.replace("name: starlight", "name: starlight2")),
            (
                "app_id_env",
                shown.replace("DASH_TEST_STARLIGHT_APP_ID", "OTHER_APP_ID"),
            ),
            (
                "app_secret_env",
                shown.replace("DASH_TEST_STARLIGHT_APP_SECRET", "OTHER_SECRET"),
            ),
            ("scope", shown.replace("scope: thread", "scope: chat")),
            ("feishu 段", shown.replace("feishu:", "feishu_off:")),
            // 字段表里别的只读项同样只在本机改:status 不是 active 时 daemon 同样不认它。
            ("status", shown.replace("status: active", "status: draft")),
            ("role", format!("{shown}role: steward\n")),
            (
                "tmux_name",
                shown.replace("runtime:\n", "runtime:\n  tmux_name: soul-other\n"),
            ),
        ] {
            assert_ne!(edited, shown, "{what}");
            let err = kit
                .call(
                    &ctx,
                    raw_put,
                    json!({"file": "agent", "agent": "starlight", "revision": raw["revision"], "text": edited}),
                )
                .unwrap_err();
            assert_eq!(err.code, ErrorCode::Forbidden, "{what}: {}", err.message);
            assert!(
                err.message.contains("summon bind"),
                "{what}: {}",
                err.message
            );
            assert_eq!(kit.read("agents/starlight/agent.yaml"), before, "{what}");
        }
        // 别的内容照常能改。
        kit.call(
            &ctx,
            raw_put,
            json!({"file": "agent", "agent": "starlight", "revision": raw["revision"],
                   "text": shown.replace("display: 星光", "display: 新名字")}),
        )
        .unwrap();
        // 本机 owner 改得了,改完列为「重启 daemon 后生效」(改名也认得出是同一个 agent)。
        let owner = kit.owner();
        let raw = kit
            .call(
                &owner,
                raw_get,
                json!({"file": "agent", "agent": "starlight"}),
            )
            .unwrap();
        let renamed = raw["text"]
            .as_str()
            .unwrap()
            .replace("name: starlight", "name: starlight2")
            .replace("DASH_TEST_STARLIGHT_APP_ID", "OTHER_APP_ID");
        let saved = kit
            .call(
                &owner,
                raw_put,
                json!({"file": "agent", "agent": "starlight", "revision": raw["revision"], "text": renamed}),
            )
            .unwrap();
        let pending: Vec<(String, String)> = saved["restart_pending"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| {
                (
                    i["file"].as_str().unwrap().to_string(),
                    i["key"].as_str().unwrap().to_string(),
                )
            })
            .collect();
        assert!(
            pending.contains(&("agent:starlight2".into(), "name".into())),
            "{pending:?}"
        );
        assert!(
            pending.contains(&("agent:starlight2".into(), "feishu.app_id_env".into())),
            "{pending:?}"
        );
        let _ = agent;
    }

    /// 有读不出的 agent.yaml 时证明不了对它也有授权:飞书身份的整机改动一律拒绝,本机 owner 照常。
    #[test]
    fn broken_agent_blocks_machine_edits_for_feishu() {
        let (kit, _agent) = kit_with_agent();
        let broken = kit.agents_dir().join("saturn");
        std::fs::create_dir_all(&broken).unwrap();
        std::fs::write(broken.join("agent.yaml"), "name: [\n").unwrap();
        let ctx = kit.feishu(issuer("starlight", P2P, ChatKind::P2p));
        let view = kit
            .call(&ctx, get, json!({"scope": {"level": "global"}}))
            .unwrap();
        let row = value(&view, "poll_ms");
        assert_eq!(row["editable"], false);
        assert!(row["deny_reason"].as_str().unwrap().contains("saturn"));
        let change = json!({"scope": {"level": "global"}, "revision": view["revision"],
                            "changes": [{"key": "poll_ms", "value": 500}]});
        let err = kit.call(&ctx, patch, change.clone()).unwrap_err();
        assert_eq!(err.code, ErrorCode::Forbidden);
        assert!(err.message.contains("saturn"), "{}", err.message);
        let doc = kit
            .call(
                &ctx,
                super::super::docs::put,
                json!({"doc": {"level": "global"}, "revision": null, "text": "x"}),
            )
            .unwrap_err();
        assert_eq!(doc.code, ErrorCode::Forbidden, "全局说明同是整机");
        // agent 自己的设置不受影响;本机 owner 照常改整机。
        let agent_view = kit
            .call(
                &ctx,
                get,
                json!({"scope": {"level": "agent", "agent": "starlight"}}),
            )
            .unwrap();
        assert_eq!(value(&agent_view, "display")["editable"], true);
        kit.call(&kit.owner(), patch, change).unwrap();
    }

    /// 停用一个 agent 不会让整机判权变松:被判权的人能停用签发 agent,却不能借此改整机。
    #[test]
    fn disabling_an_agent_does_not_open_machine_settings() {
        let kit = Kit::new();
        // 签发 agent 的名单都空:私聊里人人可操作它(包括停用它),但这不是明确授权。
        kit.add_agent(
            "starlight",
            "name: starlight\nruntime:\n  cli: claude\nfeishu:\n  app_id_env: DASH_TEST_STARLIGHT_APP_ID\n",
        );
        let ctx = kit.feishu(issuer("starlight", P2P, ChatKind::P2p));
        kit.call(
            &ctx,
            super::super::agents::set_enabled,
            json!({"agent": "starlight", "enabled": false}),
        )
        .unwrap();
        let rev = kit
            .call(&ctx, get, json!({"scope": {"level": "global"}}))
            .unwrap()["revision"]
            .clone();
        let err = kit
            .call(
                &ctx,
                patch,
                json!({"scope": {"level": "global"}, "revision": rev, "changes": [{"key": "poll_ms", "value": 500}]}),
            )
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::Forbidden, "{}", err.message);
        assert!(!kit.read("config.yaml").contains("poll_ms"));
    }

    #[test]
    fn lockout_is_refused() {
        let (kit, agent) = kit_with_agent();
        let ctx = kit.feishu(issuer("starlight", P2P, ChatKind::P2p));
        let scope = json!({"level": "agent", "agent": "starlight"});
        let view = kit.call(&ctx, get, json!({ "scope": scope })).unwrap();
        let before = kit.read("agents/starlight/agent.yaml");
        let err = kit
            .call(
                &ctx,
                patch,
                json!({"scope": scope, "revision": view["revision"], "changes": [
                    {"key": "feishu.operators", "value": [PEER_UNION]},
                ]}),
            )
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::Forbidden, "{}", err.message);
        assert!(err.message.contains("失去"));
        assert_eq!(kit.read("agents/starlight/agent.yaml"), before);
        // 原文模式同一道闸。
        let raw = kit
            .call(
                &ctx,
                raw_get,
                json!({"file": "agent", "agent": "starlight"}),
            )
            .unwrap();
        let edited = raw["text"].as_str().unwrap().replace(
            &format!("operators: [{ME_UNION}]"),
            &format!("operators: [{PEER_UNION}]"),
        );
        let err = kit
            .call(
                &ctx,
                raw_put,
                json!({"file": "agent", "agent": "starlight", "revision": raw["revision"], "text": edited}),
            )
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::Forbidden, "{}", err.message);
        assert_eq!(kit.read("agents/starlight/agent.yaml"), before);
        // 没有 agent 级权限的人连原文都存不了。
        let chat_only = kit.feishu(issuer("starlight", GROUP, ChatKind::Group));
        std::fs::write(
            agent.manifest_path(),
            before.replace(
                &format!("operators: [{ME_UNION}]"),
                &format!("operators: [{PEER_UNION}]\n  chat_operators:\n    {GROUP}: [{ME_UNION}]"),
            ),
        )
        .unwrap();
        let raw = kit
            .call(
                &chat_only,
                raw_get,
                json!({"file": "agent", "agent": "starlight"}),
            )
            .unwrap();
        assert_eq!(raw["editable"], false);
        let err = kit
            .call(
                &chat_only,
                raw_put,
                json!({"file": "agent", "agent": "starlight", "revision": raw["revision"], "text": raw["text"]}),
            )
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::Forbidden);
        std::fs::write(agent.manifest_path(), &before).unwrap();
        // 本机 owner 不受此限。
        kit.call(
            &kit.owner(),
            patch,
            json!({"scope": scope, "revision": view["revision"], "changes": [
                {"key": "feishu.operators", "value": [PEER_UNION]},
            ]}),
        )
        .unwrap();
        let _ = agent;
    }

    #[test]
    fn sentinel_enable_checks_identity_first() {
        let (kit, _agent) = kit_with_agent();
        let ctx = kit.owner();
        let rev = kit
            .call(&ctx, get, json!({"scope": {"level": "global"}}))
            .unwrap()["revision"]
            .clone();
        kit.port
            .sentinel_refused
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let err = kit
            .call(
                &ctx,
                patch,
                json!({"scope": {"level": "global"}, "revision": rev, "changes": [{"key": "sentinel.enabled", "value": true}]}),
            )
            .unwrap_err();
        assert_eq!(
            (err.code, err.key.as_deref()),
            (ErrorCode::Invalid, Some("sentinel.enabled"))
        );
        assert!(!kit.read("config.yaml").contains("sentinel"));
        kit.port
            .sentinel_refused
            .store(false, std::sync::atomic::Ordering::SeqCst);
        kit.call(
            &ctx,
            patch,
            json!({"scope": {"level": "global"}, "revision": rev, "changes": [{"key": "sentinel.enabled", "value": true}]}),
        )
        .unwrap();
        assert!(kit.read("config.yaml").contains("enabled: true"));
    }

    #[test]
    fn views_carry_the_reaction_set_of_their_layer() {
        let (kit, _agent) = kit_with_agent();
        let ctx = kit.owner();
        let global = kit
            .call(&ctx, get, json!({"scope": {"level": "global"}}))
            .unwrap();
        let builtin = builtin_messaging();
        let keys = |v: &Value| -> Vec<String> {
            v["reactions"]
                .as_array()
                .unwrap()
                .iter()
                .map(|o| o["value"].as_str().unwrap().to_string())
                .collect()
        };
        assert_eq!(keys(&global), builtin.reactions);
        let ok = global["reactions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|o| o["value"] == "OK")
            .unwrap();
        assert_eq!(ok["label"], builtin.reaction_meanings["OK"].as_str());

        // agent 自己收窄了安全集:它那一层的候选跟着变,整机那一层不变。
        let yaml = kit.agents_dir().join("starlight/agent.yaml");
        let text = std::fs::read_to_string(&yaml).unwrap();
        // 样例 agent.yaml 以 messaging 段结尾,接着往这一段里加。
        std::fs::write(&yaml, format!("{text}  reactions: [OK, DONE]\n")).unwrap();
        let agent = kit
            .call(
                &ctx,
                get,
                json!({"scope": {"level": "agent", "agent": "starlight"}}),
            )
            .unwrap();
        assert_eq!(keys(&agent), ["OK", "DONE"]);
        let chat = kit
            .call(
                &ctx,
                get,
                json!({"scope": {"level": "chat", "agent": "starlight", "chat_id": GROUP}}),
            )
            .unwrap();
        assert!(chat.get("reactions").is_none());
    }

    #[test]
    fn lark_user_reads_the_profile_on_the_page_and_needs_machine_rights() {
        let (kit, _agent) = kit_with_agent();
        let owner = kit.owner();
        let got = kit
            .call(&owner, lark_user, json!({"profile": " me "}))
            .unwrap();
        assert_eq!(got["open_id"], LARK_OWNER_OPEN);
        // 页面上把配置名清空 = 用默认配置,不回落到盘上存的那个。
        kit.call(&owner, lark_user, json!({"profile": ""})).unwrap();
        assert_eq!(
            *kit.port.lark_profiles.lock().unwrap(),
            [Some("me".to_string()), None]
        );

        kit.port
            .sentinel_refused
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let err = kit.call(&owner, lark_user, json!({})).unwrap_err();
        assert_eq!(
            (err.code, err.key.as_deref()),
            (ErrorCode::Unavailable, Some("sentinel.owner_open_id"))
        );

        // 改不了全局设置的人也读不了(这里:有个 agent 读不出来,飞书身份一律不给改整机)。
        let broken = kit.agents_dir().join("saturn");
        std::fs::create_dir_all(&broken).unwrap();
        std::fs::write(broken.join("agent.yaml"), "name: [\n").unwrap();
        let ctx = kit.feishu(issuer("starlight", P2P, ChatKind::P2p));
        let err = kit.call(&ctx, lark_user, json!({})).unwrap_err();
        assert_eq!(err.code, ErrorCode::Forbidden);
    }

    #[test]
    fn audit_line_has_keys_not_values() {
        let (kit, _agent) = kit_with_agent();
        let ctx = kit.feishu(issuer("starlight", P2P, ChatKind::P2p));
        let scope = json!({"level": "agent", "agent": "starlight"});
        let rev = kit.call(&ctx, get, json!({ "scope": scope })).unwrap()["revision"].clone();
        kit.call(
            &ctx,
            patch,
            json!({"scope": scope, "revision": rev, "changes": [{"key": "display", "value": "独一无二的名字"}]}),
        )
        .unwrap();
        let _ = kit.call(
            &ctx,
            patch,
            json!({"scope": scope, "revision": "stale", "changes": [{"key": "display", "value": "另一个"}]}),
        );
        let lines = kit.audit_lines();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["keys"], json!(["display"]));
        assert_eq!(lines[0]["file"], "agent:starlight");
        assert_eq!(lines[0]["result"], "ok");
        assert_eq!(lines[0]["issuer_agent"], "starlight");
        assert_eq!(lines[0]["identity"]["union_id"], ME_UNION);
        assert_eq!(lines[1]["result"], "conflict");
        let text = std::fs::read_to_string(kit.home().join("run/dashboard-audit.ndjson")).unwrap();
        assert!(
            !text.contains("独一无二") && !text.contains("另一个"),
            "{text}"
        );
    }

    /// Menu Bar 设置白名单(`setup::menubar_settings`)的每一项在字段表里都有对应,且两边读出的
    /// 整机值一致(`hide_thinking` 在 Menu Bar 存原义、在控制台是页面语义)。summon-core 的同名测试
    /// 看不见这个 crate,这里补跨 crate 的那一半。
    #[test]
    fn every_menubar_setting_matches_the_field_table() {
        let mapping: &[(&str, &[&str])] = &[
            ("sentinel", &["sentinel.enabled", "sentinel.dry_run"]),
            ("show_counts", &["menubar.show_counts"]),
            ("launch_at_login", &[]),
            ("cot_visible", &["messaging.cot_visible"]),
            ("hide_thinking", &["messaging.hide_thinking"]),
            ("cot_tool_details", &["messaging.cot_tool_details"]),
            ("badge_effort", &["messaging.badge_effort"]),
            ("idle_ttl_secs", &["messaging.idle_ttl_secs"]),
            ("idle_retire_secs", &["messaging.idle_retire_secs"]),
        ];
        let source = include_str!("../../../setup/menubar_settings.rs");
        let parse = &source[source.find("pub(crate) fn parse(key: &str)").unwrap()..];
        let parse = &parse[..parse.find("other => bail!").unwrap()];
        let listed: Vec<&str> = parse
            .lines()
            .filter_map(|l| l.trim().strip_prefix('"')?.split('"').next())
            .collect();
        let mapped: Vec<&str> = mapping.iter().map(|(k, _)| *k).collect();
        assert_eq!(listed, mapped, "Menu Bar 白名单变了,这张对照表跟着改");
        for (name, keys) in mapping {
            crate::setup::menubar_settings::SettingKey::parse(name).unwrap();
            for key in *keys {
                let f = fields::field(key).unwrap_or_else(|| panic!("{name} → {key} 不在字段表里"));
                assert!(
                    f.has_layer(Layer::Global) && f.write != Write::Readonly,
                    "{key}"
                );
            }
        }
        let kit = Kit::new();
        std::fs::write(
            kit.home().join("config.yaml"),
            "messaging:\n  hide_thinking: true\n  badge_effort: false\n  idle_retire_secs: 3600\n",
        )
        .unwrap();
        let cfg = kit.env().read_config().unwrap().parsed;
        let menubar = crate::setup::menubar_settings::read(
            &cfg,
            &summon_core::registry::Registry { agents: Vec::new() },
            false,
        );
        let view = kit
            .call(&kit.owner(), get, json!({"scope": {"level": "global"}}))
            .unwrap();
        let effective = |key: &str| value(&view, key)["effective"].clone();
        assert_eq!(
            effective("messaging.hide_thinking"),
            json!(!menubar.hide_thinking.value)
        );
        assert_eq!(
            effective("messaging.badge_effort"),
            json!(menubar.badge_effort.value)
        );
        assert_eq!(
            effective("messaging.cot_visible"),
            json!(menubar.cot_visible.value)
        );
        assert_eq!(
            effective("messaging.idle_ttl_secs"),
            json!(menubar.idle_ttl_secs.value)
        );
        assert_eq!(
            effective("messaging.idle_retire_secs"),
            json!(menubar.idle_retire_secs.value)
        );
        assert_eq!(effective("menubar.show_counts"), json!(menubar.show_counts));
    }

    #[test]
    fn changed_keys_and_effects_cover_nested_edits() {
        let keys = changed_keys(
            "a: 1\nmessaging:\n  cot_visible: true\n",
            "a: 1\nmessaging:\n  cot_visible: false\npreview_server:\n  port: 1\n",
        );
        assert_eq!(keys, vec!["messaging.cot_visible", "preview_server"]);
        let effects = effects_of(&keys);
        assert!(effects.iter().any(|e| e.key == "messaging.cot_visible"));
        assert!(effects.iter().any(|e| e.key == "preview_server.port"));
    }
}
