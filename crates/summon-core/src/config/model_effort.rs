//! model/effort 选择的**复用接缝**:把「某 CLI 能选哪些 model/effort」「(model,effort) 是否合法」
//! 收口成两个纯函数,供飞书会话控制面板与本机命令面共用,**不在各调用点重复 CLI 知识**。
//!
//! 设计点:
//! - 这里不持有任何 CLI 知识——effort 合法集来自 `CliDriver::legal_efforts`(P2 的单一发散点),
//!   候选 model 列表来自 messaging 配置(`Messaging::models_for_cli`),effort 候选来自 `messaging.efforts`。
//! - `validate` 是 `CliDriver::resolve_launch` 的**薄包装**(同一校验源),只把 Err 透出给调用方 match。
//! - 纯逻辑、无 IO：卡片与本机命令都拿 `&Messaging` + cli 串调用。

use crate::config::config::Messaging;
use crate::config::registry::CaptureMode;
use crate::driver::{CliDriver, ModelEffortError};

/// 某 CLI 在交互面上可暴露的 model/effort 候选。
/// - `models`:该 CLI 的模型预设(messaging 按 cli 分桶,命中桶否则回退扁平表)。
/// - `efforts`:该 CLI 的**独立** effort 档位候选 = `messaging.efforts` ∩ 该 CLI 的 `legal_efforts`。
///   cursor/Other 无独立 effort 轴(tier 烤进 model 名)→ 空,UI 据此不渲染独立 effort 行。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ModelEffortOptions {
    pub models: Vec<String>,
    pub efforts: Vec<String>,
}

/// 取某 CLI 的 model/effort 候选（给卡片或本机命令渲染选项）。effort 候选 = messaging 配置的
/// `efforts` 与该 CLI `legal_efforts` 的交集(保 messaging 里的顺序),cursor/Other 因 `legal_efforts`
/// 为空而落空集——这正是「cursor 无独立 effort 行」的统一判据,不在 UI 侧另写 cli 判断。
pub fn options_for(messaging: &Messaging, cli: &str) -> ModelEffortOptions {
    let driver = CliDriver::from_kind(cli);
    let legal = driver.legal_efforts();
    let efforts = messaging
        .efforts
        .iter()
        .filter(|e| legal.contains(&e.as_str()))
        .cloned()
        .collect();
    ModelEffortOptions {
        models: messaging.models_for_cli(cli).to_vec(),
        efforts,
    }
}

/// summon 这一侧算出来的默认模型:值 + 它落在哪一层。
///
/// 带上层次是因为面板要如实标来源:值来自 backend 自报的目录时,标「summon 默认」是假话 ——
/// summon 的配置里根本没有那个 id,用户照着去翻会一无所获。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DefaultModel {
    pub value: Option<String>,
    /// 值来自 backend 自报的 `isDefault`,而不是配置里钉的或候选桶首项。
    pub from_backend: bool,
}

impl DefaultModel {
    /// 配置里来的(或没有值)。非面板路径不关心层次,用它就够。
    pub fn from_config(value: Option<String>) -> Self {
        Self {
            value,
            from_backend: false,
        }
    }

    pub fn as_deref(&self) -> Option<&str> {
        self.value.as_deref()
    }
}

/// 该 CLI 的**默认** model,逐层非空回退:钉选 → backend 自报的默认 → 候选桶首项。
///
/// `upstream_default` 由 IO 层备好(backend 目录里 `isDefault` 的那个 id),拿不到就传 `None`。
/// 目录本身随上游走,唯独默认模型不能:上游换代时主力会被悄悄换掉,所以留 `default_model_by_cli`
/// 这一层给人钉。三层都空 → `None` = **不写 `model:` 行**,让 CLI 用自己的默认。
///
/// 那为什么不干脆一直靠这个 `None` 跟随上游?因为它落到 agent.yaml 上是「没有 model 行」,
/// 面板和文件都看不出实际跑的是哪个模型;写出显式 id 才能让 agent.yaml 自解释、面板能高亮
/// 当前项。代价是这个 id 会随上游换代而过期,换回来的是可读性 —— 这是个取舍,不是疏漏。
///
/// 各 CLI 的模型命名空间互不通用,所以每一层都按 cli 取,不跨桶回退。
pub fn default_model_for(
    messaging: &Messaging,
    cli: &str,
    upstream_default: Option<&str>,
) -> DefaultModel {
    fn clean(value: Option<&str>) -> Option<String> {
        value
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    }
    if let Some(pinned) = clean(messaging.default_model_by_cli.get(cli).map(String::as_str)) {
        return DefaultModel::from_config(Some(pinned));
    }
    if let Some(upstream) = clean(upstream_default) {
        return DefaultModel {
            value: Some(upstream),
            from_backend: true,
        };
    }
    DefaultModel::from_config(clean(
        messaging.models_for_cli(cli).first().map(String::as_str),
    ))
}

/// 生效**模型**落在解析链的哪一层。面板灰字后缀据此标注,让 user 知道当前跑的模型「哪来的」。
/// 链:会话实时(RPC)→ agent.yaml → 默认值(配置里的 / backend 报的)→ CLI 自决。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelSource {
    /// 本群覆盖(`threads/<tag>/model`,cursor 参数化 ACP 是本群稀疏覆盖的 `selected_model`)。
    ChatOverride,
    /// RPC 会话此刻自报的当前模型(cursor ACP `session/set_model` 后的真值)。最权威。
    SessionLive,
    /// agent.yaml 的 `model` / `runtime.rpc_model`(用户显式配的)。
    AgentConfig,
    /// summon 配置里算出来的:`default_model_by_cli` 钉选,或候选桶首项。
    SummonDefault,
    /// backend 自报的默认(codex 目录里 `isDefault` 的那个)。summon 的配置里没有这个 id。
    BackendDefault,
    /// 桶也空:summon 不写 `model:` 行,交给 CLI 用它自己的默认——summon 拿不到具体 id。
    CliDefault,
}

impl ModelSource {
    /// 面板灰字后缀里给人看的来源名(短,指向「哪一层」)。
    pub fn label(self) -> &'static str {
        match self {
            ModelSource::ChatOverride => summon_i18n::text!("rbba20088ac"),
            ModelSource::SessionLive => summon_i18n::text!("r7a4fccfe8e"),
            ModelSource::AgentConfig => summon_i18n::text!("rf518cb5c46"),
            ModelSource::SummonDefault => summon_i18n::text!("re03f375a8d"),
            ModelSource::BackendDefault => summon_i18n::text!("r9235e13c07"),
            ModelSource::CliDefault => summon_i18n::text!("r949b4ccf02"),
        }
    }
}

/// 解析后的生效模型:值 + 来源。`value=None` 只在 `CliDefault`(桶空、无具体 id)时出现——
/// 面板据此渲染「由 CLI 自定」而非旧的假「未指定」。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedModel {
    pub value: Option<String>,
    pub source: ModelSource,
}

/// 逐层非空回退解析生效模型 + 来源。**纯函数**:IO 层(summon)按通道备好三样原料再喂进来——
/// - `live`:仅 cursor-RPC 传 `current_model_shared`(会话实时);其余通道恒传 `None`(不存在实时轴)。
/// - `agent`:cursor-RPC 传 `rpc_model`,否则传顶层 `model`(两个命名空间不通用,不互相回退)。
/// - `fallback`:`default_model_for` 的结果(带层次:配置里来的还是 backend 报的)。
///
/// **不再返回「无值 = 未指定」**:回退到链上实际 fallback 值并标来源;只有 `CliDefault` 这最后一层
/// 因 summon 根本不知道 CLI 的内置默认 id 才 `value=None`。
pub fn resolve_model(
    live: Option<&str>,
    agent: Option<&str>,
    fallback: &DefaultModel,
) -> ResolvedModel {
    fn clean(s: Option<&str>) -> Option<&str> {
        s.map(str::trim).filter(|v| !v.is_empty())
    }
    if let Some(v) = clean(live) {
        return ResolvedModel {
            value: Some(v.to_string()),
            source: ModelSource::SessionLive,
        };
    }
    if let Some(v) = clean(agent) {
        return ResolvedModel {
            value: Some(v.to_string()),
            source: ModelSource::AgentConfig,
        };
    }
    if let Some(v) = clean(fallback.as_deref()) {
        return ResolvedModel {
            value: Some(v.to_string()),
            source: if fallback.from_backend {
                ModelSource::BackendDefault
            } else {
                ModelSource::SummonDefault
            },
        };
    }
    ResolvedModel {
        value: None,
        source: ModelSource::CliDefault,
    }
}

/// 生效 **effort** 的来源。cursor/Other 无独立 effort 轴(档位烤在 model 名里),单列一个「默认」
/// 是假信息——单独用 `ModelInherent` 表达「随模型」,面板据此标注而非显示假默认。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffortSource {
    /// 本会话覆盖(`threads/<tag>/effort`)。优先级高于 agent.yaml。
    ///
    /// **why 有这一档**:同一个 agent 会同时在多个群里跑,闲聊群和硬活群该用不同思考档。
    /// 2026-08-18 之前 effort 只有 agent.yaml 一个全局落点,在 A 群点一次改的是所有群 ——
    /// 而注入只发给当前 chat,于是「配置说的」和「B 群实际在跑的」不一致且无处可见。
    ChatOverride,
    /// agent.yaml 顶层 `effort`(claude/codex 用户显式配的)。
    AgentConfig,
    /// 有独立 effort 轴但没配 → CLI 用自己的默认档位(summon 不猜具体值)。
    CliDefault,
    /// cursor/Other 的 legacy 形态:effort 烤进 model 名,无独立轴 → 「随模型」,面板不显示假默认。
    ModelInherent,
    /// cursor 参数化 ACP:思考档位是 Cursor 参数里的 reasoning / effort / thinking 轴。`value` 是
    /// 那个轴此刻的生效值;当前模型没有这类轴时为 None。
    CursorParameter,
    /// cursor 参数化 ACP,但会话还没公布参数目录:说不清当前模型有没有思考档位。
    CursorParameterUnknown,
}

impl EffortSource {
    /// 面板灰字后缀里给人看的来源名。
    pub fn label(self) -> &'static str {
        match self {
            Self::ChatOverride => summon_i18n::text!("rbba20088ac"),
            EffortSource::AgentConfig => summon_i18n::text!("rf518cb5c46"),
            EffortSource::CliDefault => summon_i18n::text!("r949b4ccf02"),
            EffortSource::ModelInherent => summon_i18n::text!("rb102c6388f"),
            EffortSource::CursorParameter | EffortSource::CursorParameterUnknown => {
                summon_i18n::text!("r6b36c8016a")
            }
        }
    }
}

/// 解析后的生效 effort:值 + 来源。`value` 仅 `AgentConfig` 时为 `Some`;`CliDefault`/`ModelInherent`
/// 无具体值(前者交给 CLI、后者烤在模型名里)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedEffort {
    pub value: Option<String>,
    pub source: EffortSource,
}

/// 解析生效 effort + 来源。**纯函数**:有无独立 effort 轴由 `CliDriver::legal_efforts()` 判定
/// (cursor/Other 空 → `ModelInherent`),不在 UI 侧另写 cli 分支。
pub fn resolve_effort(cli: &str, agent_effort: Option<&str>) -> ResolvedEffort {
    if CliDriver::from_kind(cli).legal_efforts().is_empty() {
        return ResolvedEffort {
            value: None,
            source: EffortSource::ModelInherent,
        };
    }
    match agent_effort.map(str::trim).filter(|s| !s.is_empty()) {
        Some(v) => ResolvedEffort {
            value: Some(v.to_string()),
            source: EffortSource::AgentConfig,
        },
        None => ResolvedEffort {
            value: None,
            source: EffortSource::CliDefault,
        },
    }
}

/// agent 的**通道**(面板「通道」行):从 `capture_mode()` + cli 映射成人话。RPC 迁移后这是最需要
/// 却完全看不到的信息。RPC 子标签按 cli 分叉(cursor=ACP、codex=app-server),两者协议不同,
/// 一律标 ACP 会误导 codex 用户。
pub fn channel_label(cli: &str, mode: CaptureMode) -> &'static str {
    match mode {
        CaptureMode::Transcript => summon_i18n::text!("rbf8a617f0e"),
        CaptureMode::CliSession => match CliDriver::from_kind(cli) {
            CliDriver::Cursor => summon_i18n::text!("r20b1f1c199"),
            _ => summon_i18n::text!("r84201ec7ea"),
        },
        CaptureMode::Rpc => match CliDriver::from_kind(cli) {
            CliDriver::Cursor => summon_i18n::text!("r91edd257e5"),
            CliDriver::Codex => summon_i18n::text!("rdb8f69b4f2"),
            _ => summon_i18n::text!("r487d9e4d4a"),
        },
    }
}

/// 校验 (model, effort) 在该 CLI 上是否合法(fail-closed 写入前调)。这是 `resolve_launch` 的薄包装,
/// 同一校验源：各入口拿到 `Err` 后各自渲染友好回执（IO 层不复制 CLI 合法集）。
pub fn validate(
    cli: &str,
    model: Option<&str>,
    effort: Option<&str>,
) -> Result<(), ModelEffortError> {
    CliDriver::from_kind(cli).resolve_launch(model, effort)?;
    Ok(())
}

/// 模型 id 的长度上限(字节)。云厂商带区域与账号的 ARN 形态也在这以内。
pub const MODEL_ID_MAX_LEN: usize = 200;

/// 要写进配置的模型名不像模型 id。`shown` 是截短后的输入，可以直接放进回执。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "{}", summon_i18n::format!("r1c6ef7e3af", shown = .shown)
)]
pub struct InvalidModelId {
    pub shown: String,
}

/// 模型 id 的形态检查，写进配置之前调用：一段话、带空白或全角标点、超长的输入都不是 id。
/// 只看形态，不判这个模型在 backend 那里存不存在。
///
/// 用白名单而不是只拦空白与非 ASCII:agent.yaml 里它是 `model:` 后的 YAML 裸值，`#` `{` `|` `&`
/// `*` `!` 这类字符会改变 YAML 的含义;白名单只放已知 id 用到的字符——云厂商 ARN 的 `: /`、
/// Vertex 的 `@`、claude 的 `[1m]`、cursor 参数化 id 的 `[context=1m,effort=high,fast=false]`。
/// 首字符限定字母数字，以 `[` `@` 开头的裸值会被解析成别的东西。
pub fn check_model_id(model: &str) -> Result<(), InvalidModelId> {
    let id_char = |c: char| c.is_ascii_alphanumeric() || "-._:/@+=,[]".contains(c);
    if model.len() <= MODEL_ID_MAX_LEN
        && model.starts_with(|c: char| c.is_ascii_alphanumeric())
        && model.chars().all(id_char)
        && reads_back_as_string(model)
    {
        return Ok(());
    }
    Err(InvalidModelId {
        shown: shorten(model, 24),
    })
}

/// `true` `null` `123` `1e3` 这类值写成 YAML 裸值，读回来是布尔、空值或数字，agent.yaml 的
/// `model` 字段随之读不出来。按 summon 读 agent.yaml 用的 YAML 解析判;YAML 1.1 读成布尔的
/// `yes` `no` `on` `off` `y` `n` 一并拒——agent.yaml 不只被 summon 读。
fn reads_back_as_string(model: &str) -> bool {
    const YAML11_BOOLS: &[&str] = &["y", "yes", "n", "no", "on", "off"];
    !YAML11_BOOLS.iter().any(|b| model.eq_ignore_ascii_case(b))
        && matches!(
            serde_yaml::from_str::<serde_yaml::Value>(model),
            Ok(serde_yaml::Value::String(_))
        )
}

fn shorten(text: &str, max_chars: usize) -> String {
    let mut chars = text.chars();
    let head: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{head}…")
    } else {
        head
    }
}

/// 飞书文字命令 `/model <id>` 的去向。人很难手打对模型名，文字命令只认候选目录里的 id;
/// 对不上就不改，回几个候选让人点。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelTextCommand {
    /// 候选目录里有它(大小写不敏感),或输入恰是唯一一个候选的一截;值取目录里的写法。
    Switch(String),
    /// 输入不像模型 id,附上候选目录开头的几个。
    NotAModelId {
        error: InvalidModelId,
        suggestions: Vec<String>,
    },
    /// 像 id,但候选目录里没有，附上最接近的几个。
    NotInCatalog { suggestions: Vec<String> },
    /// 没有候选目录可对照。
    NoCatalog,
}

/// 文字命令 `/model <id>` 对照的目录：候选之外，再认该 CLI 自己接受的模型别名和 agent 当前配置的
/// 模型。前者是 CLI 内置的、不必列成按钮;后者让当前模型不在候选里的 agent 换走之后还换得回来。
/// 按大小写不敏感去重，候选在前。
pub fn text_command_catalog(
    cli: &str,
    candidates: &[String],
    current: Option<&str>,
) -> Vec<String> {
    let mut catalog: Vec<String> = Vec::new();
    let extras = CliDriver::from_kind(cli)
        .model_aliases()
        .iter()
        .copied()
        .chain(current);
    for model in candidates.iter().map(String::as_str).chain(extras) {
        let model = model.trim();
        if !model.is_empty() && !catalog.iter().any(|seen| seen.eq_ignore_ascii_case(model)) {
            catalog.push(model.to_string());
        }
    }
    catalog
}

/// `/model` 与 `/effort` 的参数形态。改本群是主路径;`default` 改 bot 默认,`inherit` 清本群覆盖。
/// 两个保留字都放得过模型 id 的形态校验,所以在这里先认。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayeredCommand<'a> {
    /// 无参:发选择卡。
    Picker,
    /// `<值>`:改本群。
    Chat(&'a str),
    /// `default <值>`:改 bot 默认。
    Default(&'a str),
    /// `default` 后面没给值。
    DefaultWithoutValue,
    /// `inherit`:清本群覆盖，回到默认。
    Inherit,
}

pub fn layered_command(argument: &str) -> LayeredCommand<'_> {
    let argument = argument.trim();
    if argument.is_empty() {
        return LayeredCommand::Picker;
    }
    if argument.eq_ignore_ascii_case("inherit") {
        return LayeredCommand::Inherit;
    }
    let mut parts = argument.splitn(2, char::is_whitespace);
    let head = parts.next().unwrap_or_default();
    if head.eq_ignore_ascii_case("default") {
        return match parts
            .next()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            Some(value) => LayeredCommand::Default(value),
            None => LayeredCommand::DefaultWithoutValue,
        };
    }
    LayeredCommand::Chat(argument)
}

/// `/effort <档位>` 对照该 CLI 可选的档位(大小写不敏感),返回规范写法;不在里面返回 None。
pub fn effort_text_command(input: &str, efforts: &[String]) -> Option<String> {
    efforts
        .iter()
        .find(|effort| effort.eq_ignore_ascii_case(input.trim()))
        .cloned()
}

/// 文字命令回执里最多列几个候选。
pub const MODEL_SUGGESTION_CAP: usize = 6;

/// 按候选目录判 `/model <id>` 的去向，见 [`ModelTextCommand`]。
pub fn model_text_command(input: &str, candidates: &[String]) -> ModelTextCommand {
    let candidates: Vec<&String> = candidates.iter().filter(|c| !c.trim().is_empty()).collect();
    if let Err(error) = check_model_id(input) {
        return ModelTextCommand::NotAModelId {
            error,
            suggestions: candidates
                .iter()
                .take(MODEL_SUGGESTION_CAP)
                .map(|c| c.to_string())
                .collect(),
        };
    }
    if candidates.is_empty() {
        return ModelTextCommand::NoCatalog;
    }
    if let Some(hit) = candidates.iter().find(|c| c.eq_ignore_ascii_case(input)) {
        return ModelTextCommand::Switch(hit.to_string());
    }
    let wanted = input.to_ascii_lowercase();
    // 手打了半个名字:恰好只有一个候选包含它,就是它;不止一个就不猜,回候选让人点。
    let mut partial = candidates
        .iter()
        .filter(|candidate| candidate.to_ascii_lowercase().contains(&wanted));
    if let (Some(only), None) = (partial.next(), partial.next()) {
        return ModelTextCommand::Switch(only.to_string());
    }
    let mut ranked: Vec<(bool, usize, usize, &String)> = candidates
        .iter()
        .enumerate()
        .map(|(order, candidate)| {
            // 输入是候选的一截(手打了半个名字)时排前面，其余按编辑距离。
            let have = candidate.to_ascii_lowercase();
            (
                !have.contains(&wanted),
                edit_distance(&wanted, &have),
                order,
                *candidate,
            )
        })
        .collect();
    ranked.sort();
    ModelTextCommand::NotInCatalog {
        suggestions: ranked
            .into_iter()
            .take(MODEL_SUGGESTION_CAP)
            .map(|(_, _, _, candidate)| candidate.clone())
            .collect(),
    }
}

/// 字符级编辑距离，候选排序用。输入已过形态检查(ASCII、至多 200 字节)。
fn edit_distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut diagonal = row[0];
        row[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let replaced = diagonal + usize::from(ca != *cb);
            diagonal = row[j + 1];
            row[j + 1] = replaced.min(row[j] + 1).min(row[j + 1] + 1);
        }
    }
    row[b.len()]
}

/// 测试夹具：一整句说明文字被当成模型名(中文，夹着空格、全角标点和引号)。两个 crate 的测试共用。
#[doc(hidden)]
pub const PROSE_AS_MODEL_FIXTURE: &str = "rcb6ecf5d8d";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_ids_pass_the_shape_check_and_sentences_do_not() {
        for id in [
            "opus",
            "claude-opus-4-8",
            "claude-opus-4-8[1m]",
            "gpt-5.6-sol",
            "gpt-5.5-high",
            "claude-3-5-sonnet-v2@20241022",
            "openai/gpt-oss-120b",
            "us.anthropic.claude-opus-4-8:0",
            "claude-opus-5-5[context=1m,effort=high,fast=false]",
            "gpt-5.6-sol[context=1m,reasoning=xhigh,fast=false]",
        ] {
            assert_eq!(check_model_id(id), Ok(()), "{id}");
        }
        for bad in [
            ::summon_core::i18n::label(PROSE_AS_MODEL_FIXTURE),
            "",
            "gpt 5.6",
            "gpt-5.6\n",
            "ｇｐｔ-5",
            "opus，",
            "`opus`",
            "opus\"",
            "[1m]opus",
            "@opus",
            "-opus",
            "true",
            "yes",
            "On",
            "null",
            "123",
            "1.5",
            "1e3",
            "0x1f",
        ] {
            assert!(check_model_id(bad).is_err(), "{bad:?}");
        }
        assert!(check_model_id(&"a".repeat(MODEL_ID_MAX_LEN)).is_ok());
        assert!(check_model_id(&"a".repeat(MODEL_ID_MAX_LEN + 1)).is_err());
        let shown = check_model_id(::summon_core::i18n::label(PROSE_AS_MODEL_FIXTURE))
            .unwrap_err()
            .shown;
        assert_eq!(shown, "模型名应当是一个标识符，例如 “claude-o…");
    }

    #[test]
    fn the_text_command_switches_only_to_a_catalog_id() {
        let catalog: Vec<String> = ["opus", "sonnet", "claude-opus-4-8", "claude-sonnet-4-6"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(
            model_text_command("Opus", &catalog),
            ModelTextCommand::Switch("opus".into())
        );
        match model_text_command(::summon_core::i18n::label(PROSE_AS_MODEL_FIXTURE), &catalog) {
            ModelTextCommand::NotAModelId { suggestions, .. } => {
                assert_eq!(suggestions, catalog)
            }
            other => panic!("{other:?}"),
        }
        match model_text_command("claude-opus-4-9", &catalog) {
            ModelTextCommand::NotInCatalog { suggestions } => {
                assert_eq!(suggestions[0], "claude-opus-4-8");
                assert_eq!(suggestions.len(), catalog.len());
            }
            other => panic!("{other:?}"),
        }
        match model_text_command("son", &catalog) {
            ModelTextCommand::NotInCatalog { suggestions } => {
                assert_eq!(&suggestions[..2], ["sonnet", "claude-sonnet-4-6"])
            }
            other => panic!("{other:?}"),
        }
        // 恰好只有一个候选包含输入:切到它。
        assert_eq!(
            model_text_command("SONNET-4", &catalog),
            ModelTextCommand::Switch("claude-sonnet-4-6".into())
        );
        assert_eq!(model_text_command("opus", &[]), ModelTextCommand::NoCatalog);
        assert!(matches!(
            model_text_command("gpt 5", &[]),
            ModelTextCommand::NotAModelId { .. }
        ));
    }

    /// claude 的文字命令还认它内置的别名和当前配置的模型：当前模型不在候选里也换得回来。
    #[test]
    fn the_text_command_catalog_adds_cli_aliases_and_the_current_model() {
        let bucket: Vec<String> = ["claude-opus-5-5", "claude-sonnet-5-5"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let catalog = text_command_catalog("claude", &bucket, Some("claude-opus-4-8"));
        assert_eq!(&catalog[..2], bucket.as_slice(), "候选在前");
        for model in ["opus", "fable", "opus[1m]", "opusplan", "claude-opus-4-8"] {
            assert_eq!(
                model_text_command(model, &catalog),
                ModelTextCommand::Switch(model.into()),
                "{model}"
            );
        }
        // 去重不分大小写;别的 CLI 没有别名。
        let again = text_command_catalog("claude", &bucket, Some("Claude-Opus-5-5"));
        assert_eq!(
            again.len(),
            bucket.len() + CliDriver::Claude.model_aliases().len()
        );
        assert_eq!(
            text_command_catalog("codex", &bucket, None),
            bucket,
            "codex 只认候选"
        );
    }

    #[test]
    fn layered_arguments_split_chat_default_inherit_and_picker() {
        assert_eq!(layered_command("  "), LayeredCommand::Picker);
        assert_eq!(layered_command("opus"), LayeredCommand::Chat("opus"));
        assert_eq!(layered_command("Inherit"), LayeredCommand::Inherit);
        assert_eq!(
            layered_command("default  claude-opus-5-5 "),
            LayeredCommand::Default("claude-opus-5-5")
        );
        assert_eq!(
            layered_command("DEFAULT"),
            LayeredCommand::DefaultWithoutValue
        );
        // 整段话照样落到「本群」,由形态校验拦下。
        assert_eq!(
            layered_command(::summon_core::i18n::label(PROSE_AS_MODEL_FIXTURE)),
            LayeredCommand::Chat(::summon_core::i18n::label(PROSE_AS_MODEL_FIXTURE))
        );
    }

    /// 整段说明文字落到「改本群」,由形态校验拦下,不切换。
    #[test]
    fn a_long_sentence_is_never_a_model() {
        assert_eq!(
            layered_command(::summon_core::i18n::label(PROSE_AS_MODEL_FIXTURE)),
            LayeredCommand::Chat(::summon_core::i18n::label(PROSE_AS_MODEL_FIXTURE))
        );
        let catalog: Vec<String> = ["opus", "sonnet"].iter().map(|s| s.to_string()).collect();
        assert!(matches!(
            model_text_command(::summon_core::i18n::label(PROSE_AS_MODEL_FIXTURE), &catalog),
            ModelTextCommand::NotAModelId { .. }
        ));
    }

    #[test]
    fn effort_text_matches_only_the_offered_levels() {
        let efforts: Vec<String> = ["low", "high"].iter().map(|s| s.to_string()).collect();
        assert_eq!(
            effort_text_command("HIGH", &efforts).as_deref(),
            Some("high")
        );
        assert_eq!(effort_text_command("max", &efforts), None);
    }

    #[test]
    fn suggestions_are_capped() {
        let catalog: Vec<String> = (0..20).map(|i| format!("model-{i}")).collect();
        match model_text_command("model-x", &catalog) {
            ModelTextCommand::NotInCatalog { suggestions } => {
                assert_eq!(suggestions.len(), MODEL_SUGGESTION_CAP)
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn edit_distance_counts_single_character_edits() {
        assert_eq!(edit_distance("", "abc"), 3);
        assert_eq!(edit_distance("kitten", "sitting"), 3);
        assert_eq!(edit_distance("opus", "opus"), 0);
    }

    fn messaging_with(efforts: &[&str], models: &[&str]) -> Messaging {
        let mut m = crate::config::config::builtin_messaging();
        m.efforts = efforts.iter().map(|s| s.to_string()).collect();
        m.models = models.iter().map(|s| s.to_string()).collect();
        m.models_by_cli.clear();
        m
    }

    #[test]
    fn options_for_intersects_efforts_with_cli_legal_set() {
        // messaging 列了 minimal(claude 非法)+ high/max(claude 合法)→ 交集只保 claude 合法的。
        let m = messaging_with(&["minimal", "high", "max"], &["claude-opus-4-8"]);
        let opt = options_for(&m, "claude");
        assert_eq!(opt.efforts, vec!["high", "max"]); // minimal 被滤掉,顺序保 messaging
        assert_eq!(opt.models, vec!["claude-opus-4-8"]);

        // codex 合法集含 minimal/none/max → 三者都能按 messaging 顺序参与候选。
        let m = messaging_with(&["minimal", "high", "max"], &["gpt-5.5-codex"]);
        let opt = options_for(&m, "codex");
        assert_eq!(opt.efforts, vec!["minimal", "high", "max"]);
    }

    #[test]
    fn options_for_cursor_and_other_have_no_effort_axis() {
        let m = messaging_with(&["low", "high"], &["gpt-5.5-high"]);
        // cursor:tier 烤进 model 名,无独立 effort 行 → 空 efforts(legal_efforts 为空)。
        assert!(options_for(&m, "cursor").efforts.is_empty());
        // Other:同样空。
        assert!(options_for(&m, "nonesuch").efforts.is_empty());
    }

    #[test]
    fn default_model_takes_bucket_head_per_cli_and_none_when_empty() {
        let mut m = messaging_with(&["high"], &[]);
        m.models_by_cli
            .insert("claude".into(), vec!["claude-opus-5".into()]);
        m.models_by_cli
            .insert("codex".into(), vec!["gpt-5.6-sol".into(), "gpt-5.5".into()]);
        // 各 CLI 拿自己桶的首项——不再有「claude 的 id 当全局默认」这种跨命名空间串味。
        assert_eq!(
            default_model_for(&m, "claude", None).value,
            Some("claude-opus-5".into())
        );
        assert_eq!(
            default_model_for(&m, "codex", None).value,
            Some("gpt-5.6-sol".into())
        );
        // 无桶且扁平表空 → None = 不写 model 行,让 CLI 用自己的默认(不猜 id)。
        assert_eq!(default_model_for(&m, "cursor", None).value, None);
    }

    #[test]
    fn default_model_falls_back_to_flat_table_for_unknown_cli() {
        let m = messaging_with(&["high"], &["flat-a", "flat-b"]);
        assert_eq!(
            default_model_for(&m, "nonesuch", None).value,
            Some("flat-a".into())
        );
    }

    #[test]
    fn pinned_default_model_wins_over_upstream_and_bucket() {
        let mut m = messaging_with(&["high"], &[]);
        m.models_by_cli
            .insert("codex".into(), vec!["gpt-5.5".into()]);
        m.default_model_by_cli
            .insert("codex".into(), "gpt-5.6-terra".into());
        assert_eq!(
            default_model_for(&m, "codex", Some("gpt-6-astra")).value,
            Some("gpt-5.6-terra".into())
        );
    }

    #[test]
    fn upstream_default_wins_over_bucket_head_when_nothing_pinned() {
        // 桶只是冷启兜底,backend 自报的默认比它新,所以压过它。
        let mut m = messaging_with(&["high"], &[]);
        m.models_by_cli.insert(
            "codex".into(),
            vec!["gpt-5.5".into(), "gpt-5.4-mini".into()],
        );
        assert_eq!(
            default_model_for(&m, "codex", Some("gpt-6-astra")).value,
            Some("gpt-6-astra".into())
        );
    }

    /// 面板要如实标来源:钉选与桶首项都是 summon 配置里的,只有中间那层是 backend 报的。
    #[test]
    fn only_the_upstream_layer_is_marked_as_coming_from_the_backend() {
        let mut m = messaging_with(&["high"], &[]);
        m.models_by_cli
            .insert("codex".into(), vec!["gpt-5.5".into()]);

        assert!(default_model_for(&m, "codex", Some("gpt-6-astra")).from_backend);
        assert!(!default_model_for(&m, "codex", None).from_backend);

        m.default_model_by_cli
            .insert("codex".into(), "gpt-5.6-terra".into());
        assert!(
            !default_model_for(&m, "codex", Some("gpt-6-astra")).from_backend,
            "钉选压过上游时,来源是配置不是 backend"
        );

        // 面板文案据此分叉,所以把两层各自落到哪个 source 一起钉住。
        let backend = default_model_for(
            &messaging_with(&["high"], &[]),
            "codex",
            Some("gpt-6-astra"),
        );
        assert_eq!(
            resolve_model(None, None, &backend).source,
            ModelSource::BackendDefault
        );
        assert_eq!(
            resolve_model(None, None, &DefaultModel::from_config(Some("x".into()))).source,
            ModelSource::SummonDefault
        );
    }

    #[test]
    fn blank_pinned_and_upstream_values_fall_through_to_the_next_layer() {
        // 配置里留了空串 / backend 回了空 id,都不算「有值」,否则会写出一个起不来的空 model 行。
        let mut m = messaging_with(&["high"], &[]);
        m.models_by_cli
            .insert("codex".into(), vec!["gpt-5.5".into()]);
        m.default_model_by_cli.insert("codex".into(), "  ".into());
        assert_eq!(
            default_model_for(&m, "codex", Some("  ")).value,
            Some("gpt-5.5".into())
        );
    }

    #[test]
    fn builtin_defaults_are_self_consistent_per_cli() {
        // 内置默认表给了桶的 CLI 都要给得出默认 model,且该 model 在自己 CLI 上校验得过——
        // 否则 onboard 一回车就写进一个起不来的 agent.yaml。cursor 没有内置桶,默认是不写
        // model 行、交给 cursor-agent 自己选。
        let m = crate::config::config::builtin_messaging();
        assert_eq!(default_model_for(&m, "cursor", None).value, None);
        for cli in ["claude", "codex"] {
            let model = default_model_for(&m, cli, None)
                .value
                .unwrap_or_else(|| panic!("内置 messaging 缺 {cli} 的默认 model"));
            assert!(
                validate(cli, Some(&model), None).is_ok(),
                "内置默认 model {model} 在 {cli} 上不合法"
            );
        }
    }

    #[test]
    fn options_for_models_fall_back_to_flat_when_no_bucket() {
        let mut m = messaging_with(&["high"], &["flat-a", "flat-b"]);
        m.models_by_cli
            .insert("claude".into(), vec!["bucket-claude".into()]);
        // claude 命中桶。
        assert_eq!(options_for(&m, "claude").models, vec!["bucket-claude"]);
        // codex 无桶 → 回退扁平。
        assert_eq!(options_for(&m, "codex").models, vec!["flat-a", "flat-b"]);
    }

    #[test]
    fn validate_wraps_resolve_launch() {
        // 合法直通。
        assert!(validate("claude", Some("claude-opus-4-8"), Some("high")).is_ok());
        assert!(validate("claude", Some("claude-opus-4-8"), None).is_ok());
        // claude 非法 effort → InvalidEffort。
        assert_eq!(
            validate("claude", None, Some("minimal")),
            Err(ModelEffortError::InvalidEffort {
                cli: "claude",
                value: "minimal".into(),
                allowed: "low/medium/high/xhigh/max".into(),
            })
        );
        // cursor + effort → EffortUnsupported(tier 烤进 model 名)。
        assert_eq!(
            validate("cursor", Some("gpt-5.5"), Some("high")),
            Err(ModelEffortError::EffortUnsupported { cli: "cursor" })
        );
    }

    #[test]
    fn resolve_model_picks_highest_present_layer_with_source() {
        // 实时 > agent > 默认:每命中一层就停,并标对来源。
        assert_eq!(
            resolve_model(
                Some("live-m"),
                Some("agent-m"),
                &DefaultModel::from_config(Some("def-m".into()))
            ),
            ResolvedModel {
                value: Some("live-m".into()),
                source: ModelSource::SessionLive
            }
        );
        assert_eq!(
            resolve_model(
                None,
                Some("agent-m"),
                &DefaultModel::from_config(Some("def-m".into()))
            ),
            ResolvedModel {
                value: Some("agent-m".into()),
                source: ModelSource::AgentConfig
            }
        );
        assert_eq!(
            resolve_model(None, None, &DefaultModel::from_config(Some("def-m".into()))),
            ResolvedModel {
                value: Some("def-m".into()),
                source: ModelSource::SummonDefault
            }
        );
    }

    #[test]
    fn resolve_model_all_empty_is_cli_default_not_unspecified() {
        // 三层皆空 → CliDefault,value=None(面板渲染「由 CLI 自定」,绝不回到旧「未指定」)。
        let r = resolve_model(None, None, &DefaultModel::default());
        assert_eq!(r.source, ModelSource::CliDefault);
        assert_eq!(r.value, None);
        // 空白串等同没值,逐层跳过。
        assert_eq!(
            resolve_model(Some("  "), Some("agent-m"), &DefaultModel::default()).source,
            ModelSource::AgentConfig
        );
    }

    #[test]
    fn resolve_effort_codex_follows_chain_cursor_is_model_inherent() {
        // codex 有独立 effort 轴:配了 → AgentConfig,没配 → CliDefault(不显示假默认)。
        assert_eq!(
            resolve_effort("codex", Some("high")),
            ResolvedEffort {
                value: Some("high".into()),
                source: EffortSource::AgentConfig
            }
        );
        assert_eq!(
            resolve_effort("codex", None),
            ResolvedEffort {
                value: None,
                source: EffortSource::CliDefault
            }
        );
        // cursor/Other 无独立轴 → ModelInherent(随模型),即便传了 effort 也不当真值。
        assert_eq!(
            resolve_effort("cursor", Some("high")).source,
            EffortSource::ModelInherent
        );
        assert_eq!(resolve_effort("cursor", Some("high")).value, None);
        assert_eq!(
            resolve_effort("nonesuch", None).source,
            EffortSource::ModelInherent
        );
    }

    #[test]
    fn channel_label_maps_two_capture_modes_and_rpc_subvariant() {
        assert_eq!(
            channel_label("claude", CaptureMode::Transcript),
            "tmux 交互"
        );
        // RPC 子标签按 cli 分叉:cursor=ACP、codex=app-server、其余保守 "RPC 常驻"。
        assert_eq!(channel_label("cursor", CaptureMode::Rpc), "RPC · ACP 常驻");
        assert_eq!(
            channel_label("cursor", CaptureMode::CliSession),
            "无头 · cursor -p"
        );
        assert_eq!(
            channel_label("codex", CaptureMode::Rpc),
            "RPC · app-server 常驻"
        );
        assert_eq!(channel_label("nonesuch", CaptureMode::Rpc), "RPC 常驻");
    }
}
