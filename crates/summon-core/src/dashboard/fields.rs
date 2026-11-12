//! 控制台的字段表:每一项可在页面上设置的配置只在这里声明一次 —— 键路径、类型与取值范围、
//! 可设置的层、生效时机、名称与说明、是否敏感、写入方式。前端只按 [`schema`] 渲染通用表单,
//! 不知道哪一层生效、什么时候生效、哪一项要取反。
//!
//! **不复制别处已有的事实**:
//! - 默认值不在这里:内置层来自 `messaging.default.yaml` 与各结构体的 `Default`;
//! - 分级开关的说明直接取 [`SETTINGS`] 的 `description`,取反、按群可设、消费者也逐条对上
//!   (由测试钉住),这里只多一个面向页面的名称;
//! - `CONFIG_FIELD_META`、Menu Bar 白名单里的每一项都要在这里有对应(或登记为只在原文模式编辑),
//!   同样由测试钉住。
//!
//! 页面归属:全局设置页列出 `layers` 含 global 的全部字段,按 `group` 分组;agent 详情页的
//! 页签就是组的 `section`(identity / runtime / permissions / messaging),只列 `layers` 含 agent
//! 的字段;`system` 组只在全局设置页出现。

use super::RawKind;
use crate::config::settings::{decl_by_id, Consumer, SettingKey};
use serde::{Deserialize, Serialize};

/// 改了之后什么时候生效。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Effect {
    /// 当场(判权现读盘、或每次用到时都现读文件)。
    Immediate,
    /// 下一轮 / 下一次用到时(daemon 在两轮之间重读配置)。
    NextTurn,
    /// 新会话:进了 PRIMER、启动参数或会话建立时下发的项。
    NewSession,
    /// 下一次轮询(sentinel、额度查询这类自己定时重读的后台循环)。
    NextPoll,
    /// 重启 daemon:启动期参数、飞书身份、网页服务监听。
    Restart,
}

impl Effect {
    /// 分级开关的消费者 → 生效时机。`AgentPrompt` 进了 PRIMER,当前会话里不生效。
    pub fn of_consumer(consumer: Consumer) -> Effect {
        match consumer {
            Consumer::Daemon => Effect::NextTurn,
            Consumer::AgentPrompt => Effect::NewSession,
        }
    }
}

/// 配置的层。字段表里的 `layers` 只会出现 global / agent / chat;builtin 只作为生效值的来源。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Layer {
    Builtin,
    Global,
    Agent,
    Chat,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FieldKind {
    Bool,
    Enum,
    Int,
    DurationSecs,
    String,
    Path,
    StringList,
    StringMap,
}

/// 字段出现在哪一页、哪个页签。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Section {
    Identity,
    Runtime,
    Permissions,
    Messaging,
    System,
}

/// 写入方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Write {
    /// 通用按行改写。
    Form,
    /// 有专门校验或流程(model / effort 校验、名单防锁死、sentinel 身份核验、按群工作目录的活会话检查)。
    Guarded,
    /// 只显示。
    Readonly,
}

/// 飞书效果预览「这一项管哪一块」的对照取值:把这一项换成它再渲染一次,两次不同的地方就是它管的区块。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Contrast {
    /// 布尔取反。
    Flip,
    /// 取第一个值;当前值恰好就是它时取第二个。两个都是 YAML 字面量。
    Alt(&'static str, &'static str),
}

impl Contrast {
    /// 当前取值 `current` 的对照值。
    pub fn against(&self, current: &serde_yaml::Value) -> serde_yaml::Value {
        match self {
            Contrast::Flip => serde_yaml::Value::Bool(!current.as_bool().unwrap_or(false)),
            Contrast::Alt(first, second) => {
                let parse = |lit: &str| {
                    serde_yaml::from_str(lit)
                        .unwrap_or_else(|_| serde_yaml::Value::String(lit.to_string()))
                };
                let first = parse(first);
                if &first == current {
                    parse(second)
                } else {
                    first
                }
            }
        }
    }
}

/// 一项设置的声明。
#[derive(Debug, Clone, Copy)]
pub struct FieldDecl {
    /// YAML 键路径(在它所属的那份文件里)。
    pub key: &'static str,
    pub label: &'static str,
    /// 留空 = 取分级开关声明里的说明(只有 `setting` 有值的项这么做)。
    pub description: &'static str,
    pub group: &'static str,
    pub kind: FieldKind,
    /// `(取值, 名称)`。
    pub options: &'static [(&'static str, &'static str)],
    pub min: Option<i64>,
    pub max: Option<i64>,
    pub layers: &'static [Layer],
    pub effect: Effect,
    /// 值不进接口响应(按 `redact` 的规则换成占位符)。
    pub sensitive: bool,
    pub write: Write,
    pub preview_contrast: Option<Contrast>,
    /// 分级开关:按群那一层读写 per-chat 设置文件里的这一项。
    pub setting: Option<SettingKey>,
    /// 页面上的语义与 YAML 里存的相反(`hide_thinking` 存「隐藏」,页面显示「显示思考」)。
    /// 读写时由服务端取反,前端拿到的永远是页面语义。
    pub inverted: bool,
    /// 本层没写时继承哪个键(层与层键名不同的项,如 `runtime.resume` ← `resume_sessions`)。
    pub inherits: Option<&'static str>,
}

impl FieldDecl {
    /// 面向页面的说明。
    pub fn description(&self) -> &'static str {
        match self.setting {
            Some(id) if self.description.is_empty() => decl_by_id(id).description,
            _ => self.description,
        }
    }

    pub fn has_layer(&self, layer: Layer) -> bool {
        self.layers.contains(&layer)
    }

    /// 这一项写在哪份文件里:有 global 层的在 config.yaml,只有 agent 层的在 agent.yaml。
    /// 同一个键在两份文件里都能写的(`messaging.*`)两者都是。
    pub fn in_file(&self, kind: RawKind) -> bool {
        match kind {
            RawKind::Config => self.has_layer(Layer::Global),
            RawKind::Agent => self.has_layer(Layer::Agent),
        }
    }

    pub fn to_desc(&self) -> FieldDesc {
        FieldDesc {
            key: self.key.to_string(),
            label: crate::i18n::label(self.label).to_string(),
            description: crate::i18n::label(self.description()).to_string(),
            group: self.group.to_string(),
            kind: self.kind,
            options: (!self.options.is_empty()).then(|| {
                self.options
                    .iter()
                    .map(|(value, label)| FieldOption {
                        value: value.to_string(),
                        label: crate::i18n::label(label).to_string(),
                    })
                    .collect()
            }),
            min: self.min,
            max: self.max,
            layers: self.layers.to_vec(),
            effect: self.effect,
            sensitive: self.sensitive,
            write: self.write,
            preview: self.preview_contrast.is_some(),
            inverted: self.inverted,
        }
    }

    const fn new(
        key: &'static str,
        label: &'static str,
        group: &'static str,
        kind: FieldKind,
        layers: &'static [Layer],
        effect: Effect,
        description: &'static str,
    ) -> Self {
        FieldDecl {
            key,
            label,
            description,
            group,
            kind,
            options: &[],
            min: None,
            max: None,
            layers,
            effect,
            sensitive: false,
            write: Write::Form,
            preview_contrast: None,
            setting: None,
            inverted: false,
            inherits: None,
        }
    }

    const fn guarded(mut self) -> Self {
        self.write = Write::Guarded;
        self
    }

    const fn readonly(mut self) -> Self {
        self.write = Write::Readonly;
        self
    }

    const fn options(mut self, options: &'static [(&'static str, &'static str)]) -> Self {
        self.options = options;
        self
    }

    const fn min(mut self, min: i64) -> Self {
        self.min = Some(min);
        self
    }

    const fn range(mut self, min: i64, max: i64) -> Self {
        self.min = Some(min);
        self.max = Some(max);
        self
    }

    const fn contrast(mut self, contrast: Contrast) -> Self {
        self.preview_contrast = Some(contrast);
        self
    }

    const fn setting(mut self, id: SettingKey) -> Self {
        self.setting = Some(id);
        self
    }

    const fn inverted(mut self) -> Self {
        self.inverted = true;
        self
    }

    const fn sensitive(mut self) -> Self {
        self.sensitive = true;
        self
    }

    const fn inherits(mut self, key: &'static str) -> Self {
        self.inherits = Some(key);
        self
    }
}

/// 一个分组的声明。
#[derive(Debug, Clone, Copy)]
pub struct GroupDecl {
    pub id: &'static str,
    pub label: &'static str,
    pub description: Option<&'static str>,
    pub section: Section,
}

impl GroupDecl {
    pub fn to_group(&self) -> FieldGroup {
        FieldGroup {
            id: self.id.to_string(),
            label: crate::i18n::label(self.label).to_string(),
            description: self.description.map(|s| crate::i18n::label(s).to_string()),
            section: self.section,
        }
    }
}

// ── 契约结构(与 dashboard/src/api/types.ts 逐字段对应) ────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct FieldOption {
    pub value: String,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct FieldDesc {
    pub key: String,
    pub label: String,
    pub description: String,
    pub group: String,
    pub kind: FieldKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub options: Option<Vec<FieldOption>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<i64>,
    pub layers: Vec<Layer>,
    pub effect: Effect,
    pub sensitive: bool,
    pub write: Write,
    pub preview: bool,
    /// 页面上的开关显示的是 YAML 里那个键取反后的含义(`hide_thinking` 显示为「显示思考」)。
    /// 取值由服务端换算好,前端只用它给键路径旁加提示。
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub inverted: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct FieldGroup {
    pub id: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub section: Section,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct Schema {
    pub groups: Vec<FieldGroup>,
    pub fields: Vec<FieldDesc>,
}

/// `config.schema` 的响应。
pub fn schema() -> Schema {
    Schema {
        groups: GROUPS.iter().map(GroupDecl::to_group).collect(),
        fields: FIELDS.iter().map(FieldDecl::to_desc).collect(),
    }
}

/// 按键路径查声明。
pub fn field(key: &str) -> Option<&'static FieldDecl> {
    FIELDS.iter().find(|f| f.key == key)
}

pub fn group(id: &str) -> Option<&'static GroupDecl> {
    GROUPS.iter().find(|g| g.id == id)
}

// ── 不进表单的键 ──────────────────────────────────────────────────────────

/// 不进表单的键为什么不进。原文校验与覆盖测试都按这张表认。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoteKind {
    /// 结构复杂,只在原文模式编辑(长文本、嵌套映射、严格子结构)。
    RawOnly,
    /// 有专门的页面与接口(按群名单、免 @、按群工作目录)。
    Dedicated,
    /// 不归 daemon 读、有别的读者:原文校验不报未知键。
    External,
    /// 已废弃:原文校验给警告。`CONFIG_FIELD_META` 里登记过的废弃键不在这里重复。
    Deprecated,
}

#[derive(Debug, Clone, Copy)]
pub struct KeyNote {
    pub file: RawKind,
    pub path: &'static str,
    pub kind: NoteKind,
    pub note: &'static str,
}

const fn note(file: RawKind, path: &'static str, kind: NoteKind, note: &'static str) -> KeyNote {
    KeyNote {
        file,
        path,
        kind,
        note,
    }
}

/// 按路径找登记:`path` 等于登记的键或在它下面。
pub fn key_note(file: RawKind, path: &str) -> Option<&'static KeyNote> {
    KEY_NOTES.iter().find(|n| {
        n.file == file
            && (n.path == path
                || path
                    .strip_prefix(n.path)
                    .is_some_and(|rest| rest.starts_with('.')))
    })
}

const MESSAGING_RAW_ONLY: [(&str, &str); 8] = [
    ("messaging.system_prompt", "r0cd6c63d53"),
    ("messaging.prompt_template", "r6cfe0d20cb"),
    ("messaging.reactions", "r342919226a"),
    ("messaging.reaction_meanings", "ra13c3a0058"),
    ("messaging.session_exited", "r41102f201a"),
    ("messaging.slash_blacklist", "ra6f49702dd"),
    ("messaging.models", "r89e27f95a6"),
    ("messaging.models_by_cli", "reb43f84ffe"),
];

pub const KEY_NOTES: &[KeyNote] = &[
    note(
        RawKind::Config,
        MESSAGING_RAW_ONLY[0].0,
        NoteKind::RawOnly,
        MESSAGING_RAW_ONLY[0].1,
    ),
    note(
        RawKind::Config,
        MESSAGING_RAW_ONLY[1].0,
        NoteKind::RawOnly,
        MESSAGING_RAW_ONLY[1].1,
    ),
    note(
        RawKind::Config,
        MESSAGING_RAW_ONLY[2].0,
        NoteKind::RawOnly,
        MESSAGING_RAW_ONLY[2].1,
    ),
    note(
        RawKind::Config,
        MESSAGING_RAW_ONLY[3].0,
        NoteKind::RawOnly,
        MESSAGING_RAW_ONLY[3].1,
    ),
    note(
        RawKind::Config,
        MESSAGING_RAW_ONLY[4].0,
        NoteKind::RawOnly,
        MESSAGING_RAW_ONLY[4].1,
    ),
    note(
        RawKind::Config,
        MESSAGING_RAW_ONLY[5].0,
        NoteKind::RawOnly,
        MESSAGING_RAW_ONLY[5].1,
    ),
    note(
        RawKind::Config,
        MESSAGING_RAW_ONLY[6].0,
        NoteKind::RawOnly,
        MESSAGING_RAW_ONLY[6].1,
    ),
    note(
        RawKind::Config,
        MESSAGING_RAW_ONLY[7].0,
        NoteKind::RawOnly,
        MESSAGING_RAW_ONLY[7].1,
    ),
    note(
        RawKind::Agent,
        MESSAGING_RAW_ONLY[0].0,
        NoteKind::RawOnly,
        MESSAGING_RAW_ONLY[0].1,
    ),
    note(
        RawKind::Agent,
        MESSAGING_RAW_ONLY[1].0,
        NoteKind::RawOnly,
        MESSAGING_RAW_ONLY[1].1,
    ),
    note(
        RawKind::Agent,
        MESSAGING_RAW_ONLY[2].0,
        NoteKind::RawOnly,
        MESSAGING_RAW_ONLY[2].1,
    ),
    note(
        RawKind::Agent,
        MESSAGING_RAW_ONLY[3].0,
        NoteKind::RawOnly,
        MESSAGING_RAW_ONLY[3].1,
    ),
    note(
        RawKind::Agent,
        MESSAGING_RAW_ONLY[4].0,
        NoteKind::RawOnly,
        MESSAGING_RAW_ONLY[4].1,
    ),
    note(
        RawKind::Agent,
        MESSAGING_RAW_ONLY[5].0,
        NoteKind::RawOnly,
        MESSAGING_RAW_ONLY[5].1,
    ),
    note(
        RawKind::Agent,
        MESSAGING_RAW_ONLY[6].0,
        NoteKind::RawOnly,
        MESSAGING_RAW_ONLY[6].1,
    ),
    note(
        RawKind::Agent,
        MESSAGING_RAW_ONLY[7].0,
        NoteKind::RawOnly,
        MESSAGING_RAW_ONLY[7].1,
    ),
    note(
        RawKind::Config,
        "sentinel.aliases",
        NoteKind::RawOnly,
        "r44587c303b",
    ),
    note(RawKind::Config, "remotes", NoteKind::RawOnly, "r54d9a39eec"),
    note(
        RawKind::Config,
        "execution_targets",
        NoteKind::RawOnly,
        "rfc86c51b11",
    ),
    note(RawKind::Config, "upgrade", NoteKind::RawOnly, "r57dd131753"),
    note(
        RawKind::Agent,
        "runtime.execution",
        NoteKind::RawOnly,
        "r65bc01b049",
    ),
    note(
        RawKind::Agent,
        "runtime.seed",
        NoteKind::RawOnly,
        "r943a7f7b79",
    ),
    note(
        RawKind::Agent,
        "runtime.rpc_model",
        NoteKind::RawOnly,
        "r42b4176904",
    ),
    note(
        RawKind::Agent,
        "runtime.headless",
        NoteKind::RawOnly,
        "r1429578e50",
    ),
    note(
        RawKind::Agent,
        "feishu.chat_operators",
        NoteKind::Dedicated,
        "re8b90d774a",
    ),
    note(
        RawKind::Agent,
        "feishu.chat_reply_without_mention",
        NoteKind::Dedicated,
        "rf0173d1e0a",
    ),
    note(
        RawKind::Agent,
        "cognition",
        NoteKind::External,
        "ra6891d016b",
    ),
    note(
        RawKind::Config,
        "quota_recovery",
        NoteKind::Deprecated,
        "rf50aa6db38",
    ),
];

// ── 分组 ──────────────────────────────────────────────────────────────────

const fn grp(
    id: &'static str,
    label: &'static str,
    section: Section,
    description: &'static str,
) -> GroupDecl {
    GroupDecl {
        id,
        label,
        description: Some(description),
        section,
    }
}

/// 全局设置页的分组按这里的顺序排;「全局说明」(`~/.summon/CLAUDE.md`)是说明文件编辑器,
/// 走 `docs.*`,不在字段表里。
pub const GROUPS: &[GroupDecl] = &[
    grp("cot", "r0f1e62f967", Section::Messaging, "re465592dcf"),
    grp(
        "lifecycle",
        "rf736c54622",
        Section::Messaging,
        "r53edfdb453",
    ),
    grp("steer", "r1d089bea55", Section::Messaging, "r383652b9ca"),
    grp(
        "document_comments",
        "r2549c41532",
        Section::Messaging,
        "r99ff6258d4",
    ),
    grp("behavior", "r222f9427fd", Section::Messaging, "r2f07adcd00"),
    grp("models", "rc98e118e0a", Section::Messaging, "ref93b786d0"),
    grp("sentinel", "Sentinel", Section::System, "ra60e6efc44"),
    grp("codex", "r6f17b319d9", Section::System, "r858cd0fa2c"),
    grp(
        "cursor_quota",
        "rcd4519ad7a",
        Section::System,
        "ra394b4edb8",
    ),
    grp("web", "r4e3e084fad", Section::System, "ra158411c45"),
    grp("tools", "r39213ec38c", Section::System, "r52a5568d92"),
    grp("menubar", "Menu Bar", Section::System, "rf55ab70a71"),
    grp("insights", "r9d7407c8a3", Section::System, "r56a25114fe"),
    grp("advanced", "rd6e61888b0", Section::System, "r8ad35b8d90"),
    grp(
        "agent_identity",
        "raaaf449bd4",
        Section::Identity,
        "r384062001c",
    ),
    grp(
        "agent_runtime",
        "rc3bdfbfc5c",
        Section::Runtime,
        "rb3670df695",
    ),
    grp(
        "agent_codex",
        "r29a82f2da8",
        Section::Runtime,
        "radcdba98e5",
    ),
    grp(
        "agent_permissions",
        "r52798775c2",
        Section::Permissions,
        "r3211fafcbc",
    ),
];

// ── 字段 ──────────────────────────────────────────────────────────────────

use Effect::{Immediate, NewSession, NextPoll, NextTurn, Restart};
use FieldKind::{Bool, DurationSecs, Int, Path, StringList, StringMap};

const G: &[Layer] = &[Layer::Global];
const GA: &[Layer] = &[Layer::Global, Layer::Agent];
const GAC: &[Layer] = &[Layer::Global, Layer::Agent, Layer::Chat];
const A: &[Layer] = &[Layer::Agent];

const SANDBOX_OPTIONS: &[(&str, &str)] = &[
    ("read-only", "r3b5ec3533b"),
    ("workspace-write", "r789a8ee665"),
    ("danger-full-access", "re7adfca832"),
];
const APPROVAL_OPTIONS: &[(&str, &str)] = &[
    ("untrusted", "ra4b773490d"),
    ("on-request", "r40101f3dee"),
    ("never", "rcda21b3b88"),
];
const STR: FieldKind = FieldKind::String;
const ENUM: FieldKind = FieldKind::Enum;

const fn tool(key: &'static str, label: &'static str) -> FieldDecl {
    FieldDecl::new(key, label, "tools", Path, G, NextTurn, "r7750aa2e28")
}

pub const FIELDS: &[FieldDecl] = &[
    // ── 飞书过程展示 ──
    FieldDecl::new(
        "language",
        "language.label",
        "web",
        ENUM,
        G,
        Immediate,
        "language.description",
    )
    .options(&[
        ("auto", "language.auto"),
        ("zh-CN", "language.zh-CN"),
        ("en", "language.en"),
    ]),
    FieldDecl::new(
        "messaging.cot_visible",
        "r0544dcc527",
        "cot",
        Bool,
        GAC,
        NextTurn,
        "",
    )
    .setting(SettingKey::CotVisible)
    .contrast(Contrast::Flip),
    FieldDecl::new(
        "messaging.hide_thinking",
        "rcd42d145dc",
        "cot",
        Bool,
        GAC,
        NextTurn,
        "",
    )
    .setting(SettingKey::CotThinking)
    .inverted()
    .contrast(Contrast::Flip),
    FieldDecl::new(
        "messaging.cot_tool_details",
        "r165e4bc9e1",
        "cot",
        Bool,
        GAC,
        NextTurn,
        "",
    )
    .setting(SettingKey::CotToolDetails)
    .contrast(Contrast::Flip),
    FieldDecl::new(
        "messaging.badge_effort",
        "r32f3e3871d",
        "cot",
        Bool,
        GAC,
        NextTurn,
        "",
    )
    .setting(SettingKey::BadgeEffort)
    .contrast(Contrast::Flip),
    FieldDecl::new(
        "messaging.tool_trail",
        "rcdc4ded33f",
        "cot",
        Bool,
        GA,
        NextTurn,
        "r2acd95878b",
    )
    .contrast(Contrast::Flip),
    FieldDecl::new(
        "messaging.progress_max_steps",
        "ra21a0c55a6",
        "cot",
        Int,
        GA,
        NextTurn,
        "r69a175535c",
    )
    .range(0, 50)
    .contrast(Contrast::Alt("1", "6")),
    FieldDecl::new(
        "messaging.completion_summary_card",
        "r45c2e30421",
        "cot",
        Bool,
        GA,
        NextTurn,
        "r05622b1728",
    )
    .contrast(Contrast::Flip),
    FieldDecl::new(
        "messaging.mask_email",
        "rfad9b4d948",
        "cot",
        Bool,
        GA,
        Immediate,
        "rab232611c8",
    ),
    // ── 会话生命周期 ──
    FieldDecl::new(
        "messaging.idle_ttl_secs",
        "ra833904037",
        "lifecycle",
        DurationSecs,
        GA,
        NextTurn,
        "re82321f746",
    )
    .min(60),
    FieldDecl::new(
        "messaging.idle_retire_secs",
        "ra6ec78fd4f",
        "lifecycle",
        DurationSecs,
        GA,
        NextTurn,
        "r6cd570df19",
    )
    .min(0),
    FieldDecl::new(
        "messaging.timer_idle_retire_secs",
        "r2ba1ceca8a",
        "lifecycle",
        DurationSecs,
        GA,
        NextTurn,
        "re950cef32a",
    )
    .min(0),
    FieldDecl::new(
        "messaging.timer_idle_retire_reset",
        "r45867ed53d",
        "lifecycle",
        Bool,
        GAC,
        NextTurn,
        "",
    )
    .setting(SettingKey::TimerIdleRetireReset),
    FieldDecl::new(
        "resume_sessions",
        "r02ab8908cd",
        "lifecycle",
        Bool,
        G,
        NewSession,
        "rb276669bb3",
    ),
    FieldDecl::new(
        "messaging.headless_stall_secs",
        "rf4d9dc2085",
        "lifecycle",
        DurationSecs,
        GA,
        NextTurn,
        "r724c0cdd16",
    )
    .min(1),
    FieldDecl::new(
        "messaging.background_continuation_max_secs",
        "raf9cd7ec65",
        "lifecycle",
        DurationSecs,
        GA,
        NextTurn,
        "r38a8647b2b",
    )
    .min(0),
    FieldDecl::new(
        "messaging.idle_scan_secs",
        "rc28a114b5b",
        "lifecycle",
        DurationSecs,
        GA,
        NextTurn,
        "r56c6336073",
    )
    .min(1),
    // ── 轮内追加 ──
    FieldDecl::new(
        "messaging.steer_transcript",
        "re507c1c804",
        "steer",
        Bool,
        GA,
        NextTurn,
        "r450ff7d6a2",
    ),
    FieldDecl::new(
        "messaging.steer_rpc",
        "r0720daf756",
        "steer",
        Bool,
        GA,
        NextTurn,
        "rd1e570e2fc",
    ),
    FieldDecl::new(
        "messaging.steer_cli_session",
        "r2ff78bcd10",
        "steer",
        Bool,
        GA,
        NextTurn,
        "r0a12602bc4",
    ),
    // ── agent 行为 ──
    FieldDecl::new(
        "messaging.document_comments_enabled",
        "r818d20a8f7",
        "document_comments",
        Bool,
        GA,
        NextTurn,
        "",
    )
    .setting(SettingKey::DocumentComments),
    FieldDecl::new(
        "messaging.document_comment_timeout_secs",
        "r8d1ff99074",
        "document_comments",
        DurationSecs,
        GA,
        NextTurn,
        "r02c1313489",
    )
    .min(1),
    FieldDecl::new(
        "messaging.agent_reactions",
        "rbd14d043c2",
        "behavior",
        Bool,
        GAC,
        NewSession,
        "",
    )
    .setting(SettingKey::AgentReactions)
    .contrast(Contrast::Flip),
    FieldDecl::new(
        "messaging.agent_feed",
        "r584ac8506d",
        "behavior",
        Bool,
        GAC,
        NewSession,
        "",
    )
    .setting(SettingKey::AgentFeed)
    .contrast(Contrast::Flip),
    FieldDecl::new(
        "messaging.claude_permission_hook",
        "r367c68ffe1",
        "behavior",
        Bool,
        GA,
        Immediate,
        "r63f78c5ce1",
    ),
    FieldDecl::new(
        "messaging.session_mirror",
        "ra9f2407558",
        "behavior",
        Bool,
        GA,
        NextTurn,
        "rad4eac3fc0",
    ),
    FieldDecl::new(
        "messaging.codex_platform_tools",
        "r4d307cea0b",
        "behavior",
        Bool,
        GA,
        NewSession,
        "r24c5586a70",
    ),
    FieldDecl::new(
        "messaging.working_reaction",
        "r55b92ffd51",
        "behavior",
        STR,
        GA,
        NextTurn,
        "reebad262a3",
    ),
    FieldDecl::new(
        "messaging.done_reaction",
        "r8a00842619",
        "behavior",
        STR,
        GA,
        NextTurn,
        "r9a2b8686c1",
    )
    .contrast(Contrast::Alt("THUMBSUP", "DONE")),
    FieldDecl::new(
        "messaging.interrupted_reaction",
        "r08cffa1950",
        "behavior",
        STR,
        GA,
        NextTurn,
        "r95a0166733",
    ),
    FieldDecl::new(
        "messaging.default_reaction",
        "r2e53d922ea",
        "behavior",
        STR,
        GA,
        NextTurn,
        "ra91fcb8309",
    ),
    FieldDecl::new(
        "claude.status_line_in_managed_sessions",
        "rbb9dc34b10",
        "behavior",
        Bool,
        G,
        Immediate,
        "re83fa81f99",
    ),
    // ── 模型 ──
    FieldDecl::new(
        "messaging.default_model_by_cli",
        "r24b92d9fc2",
        "models",
        StringMap,
        GA,
        NextTurn,
        "r3392c1020e",
    ),
    FieldDecl::new(
        "messaging.efforts",
        "rca1cc972b0",
        "models",
        StringList,
        GA,
        NextTurn,
        "r368dde7f8f",
    ),
    // ── Sentinel ──
    FieldDecl::new(
        "sentinel.enabled",
        "rf4f0ead111",
        "sentinel",
        Bool,
        G,
        NextPoll,
        "r0fea773c72",
    )
    .guarded(),
    FieldDecl::new(
        "sentinel.dry_run",
        "r9718eb7ca2",
        "sentinel",
        Bool,
        G,
        NextPoll,
        "r2edb09541a",
    ),
    FieldDecl::new(
        "sentinel.fast_interval_secs",
        "r68fdb896a3",
        "sentinel",
        DurationSecs,
        G,
        NextPoll,
        "r6818d6ec3a",
    )
    .min(1),
    FieldDecl::new(
        "sentinel.fast_top_k",
        "r64a82de06b",
        "sentinel",
        Int,
        G,
        NextPoll,
        "rd645e6e68b",
    )
    .min(1),
    FieldDecl::new(
        "sentinel.interval_secs",
        "r362ce661ea",
        "sentinel",
        DurationSecs,
        G,
        NextPoll,
        "r4551d78d0c",
    )
    .min(1),
    FieldDecl::new(
        "sentinel.slow_batch_size",
        "r1206a53eb0",
        "sentinel",
        Int,
        G,
        NextPoll,
        "r6034eb05ef",
    )
    .min(1),
    FieldDecl::new(
        "sentinel.top_k",
        "r1de322a572",
        "sentinel",
        Int,
        G,
        NextPoll,
        "r14c626a37e",
    )
    .min(1),
    FieldDecl::new(
        "sentinel.max_age_secs",
        "r09d0f03e12",
        "sentinel",
        DurationSecs,
        G,
        NextPoll,
        "r00184c192b",
    )
    .min(1),
    FieldDecl::new(
        "sentinel.per_hour",
        "rcfcdfd781c",
        "sentinel",
        Int,
        G,
        NextPoll,
        "rede2a40190",
    )
    .min(0),
    FieldDecl::new(
        "sentinel.join_verbs",
        "ra4e7d248dc",
        "sentinel",
        StringList,
        G,
        NextPoll,
        "rb5d91b1c93",
    ),
    FieldDecl::new(
        "sentinel.create_verbs",
        "r33e1aa4e23",
        "sentinel",
        StringList,
        G,
        NextPoll,
        "r64e7e0d19b",
    ),
    FieldDecl::new(
        "sentinel.invite_verbs",
        "r26fdb95f19",
        "sentinel",
        StringList,
        G,
        NextPoll,
        "r356a20101d",
    ),
    FieldDecl::new(
        "sentinel.blocklist",
        "r180c0541c2",
        "sentinel",
        StringList,
        G,
        NextPoll,
        "r552ec2eada",
    ),
    FieldDecl::new(
        "sentinel.owner_open_id",
        "rdd73e9a56b",
        "sentinel",
        STR,
        G,
        NextPoll,
        "r1996be7e20",
    ),
    FieldDecl::new(
        "sentinel.lark_profile",
        "r8ede57e02c",
        "sentinel",
        STR,
        G,
        NextPoll,
        "r6349bec1f1",
    ),
    // ── Codex 策略 ──
    FieldDecl::new(
        "codex.sandbox",
        "sandbox",
        "codex",
        ENUM,
        G,
        NewSession,
        "r556b5368b7",
    )
    .options(SANDBOX_OPTIONS),
    FieldDecl::new(
        "codex.approval_policy",
        "r6952a625ca",
        "codex",
        ENUM,
        G,
        NewSession,
        "r7c0bb800b8",
    )
    .options(APPROVAL_OPTIONS),
    // ── Cursor 额度 ──
    FieldDecl::new(
        "cursor.quota.enabled",
        "r31c63fafbb",
        "cursor_quota",
        Bool,
        G,
        NextPoll,
        "re0807ed4c7",
    ),
    FieldDecl::new(
        "cursor.quota.interval_secs",
        "r4bd581fb4b",
        "cursor_quota",
        DurationSecs,
        G,
        NextPoll,
        "rcf3b27f6ca",
    )
    .min(60),
    FieldDecl::new(
        "cursor.quota.token_command",
        "re70b03909b",
        "cursor_quota",
        STR,
        G,
        NextPoll,
        "r6b06eb2277",
    ),
    // ── 网页服务 ──
    FieldDecl::new(
        "preview_server.enabled",
        "rf3c5fe4dd2",
        "web",
        Bool,
        G,
        Restart,
        "r44ea252127",
    ),
    FieldDecl::new(
        "preview_server.bind",
        "r6b2caa10c8",
        "web",
        STR,
        G,
        Restart,
        "rf5c090f354",
    ),
    FieldDecl::new(
        "preview_server.port",
        "re2a07bff13",
        "web",
        Int,
        G,
        Restart,
        "r4668843caa",
    )
    .range(1, 65535),
    FieldDecl::new(
        "preview_server.public_base_url",
        "r70ef02de78",
        "web",
        STR,
        G,
        Restart,
        "r19307506eb",
    ),
    FieldDecl::new(
        "preview_server.token_ttl_secs",
        "r2a8f59e6c7",
        "web",
        DurationSecs,
        G,
        Restart,
        "r92f683b0ca",
    )
    .min(60),
    FieldDecl::new(
        "preview_server.max_file_mb",
        "rf3a1b7151a",
        "web",
        Int,
        G,
        Restart,
        "rc5de6a3027",
    )
    .min(1),
    FieldDecl::new(
        "web_terminal.enabled",
        "rec6063164f",
        "web",
        Bool,
        G,
        Restart,
        "rc808e14ac8",
    ),
    FieldDecl::new(
        "web_terminal.port",
        "r3220d161b7",
        "web",
        Int,
        G,
        Restart,
        "rd1b056098a",
    )
    .range(1, 65535),
    FieldDecl::new(
        "dashboard.enabled",
        "rd0d07d1531",
        "web",
        Bool,
        G,
        Restart,
        "r10db8fdf3a",
    ),
    FieldDecl::new(
        "dashboard.port",
        "r7de1bdc2fd",
        "web",
        Int,
        G,
        Restart,
        "rddbcb4907d",
    )
    .range(1, 65535),
    FieldDecl::new(
        "dashboard.public_base_url",
        "r2342136548",
        "web",
        STR,
        G,
        Restart,
        "ra689258fac",
    ),
    // ── 外部工具 ──
    tool("tools.tmux", "tmux"),
    tool("tools.lark_cli", "lark-cli"),
    tool("tools.claude", "claude"),
    tool("tools.codex", "codex"),
    tool("tools.traex", "traex"),
    tool("tools.cursor_agent", "cursor-agent"),
    tool("tools.git", "git"),
    tool("tools.ssh", "ssh"),
    tool("tools.npx", "npx"),
    tool("tools.pkill", "pkill"),
    tool("tools.open", "open"),
    tool("tools.xdg_open", "xdg-open"),
    tool("tools.launchctl", "launchctl"),
    tool("tools.systemctl", "systemctl"),
    tool("tools.loginctl", "loginctl"),
    tool("tools.pbcopy", "pbcopy"),
    tool("tools.osascript", "osascript"),
    // ── Menu Bar ──
    FieldDecl::new(
        "menubar.show_counts",
        "r41b513e74a",
        "menubar",
        Bool,
        G,
        Immediate,
        "r2288cd943d",
    ),
    FieldDecl::new(
        "menubar.icon",
        "r0d720eeea2",
        "menubar",
        ENUM,
        G,
        Immediate,
        "r650f3d41a6",
    )
    .options(&[
        ("xyz", "r357c359a24"),
        ("millennium-eye", "rea3e1df9ff"),
        ("link", "r91afe57d54"),
        ("synchro", "r4921c33aea"),
        ("card", "r97e13b5d2d"),
        ("fusion", "r0d0066249e"),
        ("ring-star", "r1297a4b570"),
        ("hex-star", "r908bb5862b"),
    ]),
    FieldDecl::new(
        "menubar.terminal",
        "r3678deca0e",
        "menubar",
        StringList,
        G,
        Immediate,
        "rccf5326c5a",
    ),
    // ── 洞察 ──
    // 写入时校验是一个此刻可用的 agent;手改成不在的名字时运行期回落到自动。
    FieldDecl::new(
        "primary_agent",
        "r584edc12fd",
        "insights",
        STR,
        G,
        Immediate,
        "r351a275214",
    )
    .guarded(),
    // ── 高级 ──
    FieldDecl::new(
        "agents_dir",
        "r95ce60047f",
        "advanced",
        Path,
        G,
        Restart,
        "r4658df574c",
    ),
    FieldDecl::new(
        "default_domain",
        "r3dbd0f5b7b",
        "advanced",
        STR,
        G,
        Restart,
        "r6f4e057ae4",
    ),
    FieldDecl::new(
        "poll_ms",
        "rf5e59c4cef",
        "advanced",
        Int,
        G,
        Restart,
        "re92551d5f9",
    )
    .min(10),
    FieldDecl::new(
        "debounce_ms",
        "r8604f3ebd0",
        "advanced",
        Int,
        G,
        Restart,
        "r06c35a5c8f",
    )
    .min(0),
    FieldDecl::new(
        "sidecar_bin",
        "r0546969638",
        "advanced",
        Path,
        G,
        Restart,
        "r0a463864ab",
    ),
    // ── agent 概要 ──
    FieldDecl::new(
        "name",
        "rd44e9b3d3b",
        "agent_identity",
        STR,
        A,
        Restart,
        "r53ec7a824f",
    )
    .readonly(),
    FieldDecl::new(
        "display",
        "ra98585871c",
        "agent_identity",
        STR,
        A,
        NextTurn,
        "r9213a37f39",
    ),
    FieldDecl::new(
        "status",
        "r6320b4a872",
        "agent_identity",
        STR,
        A,
        Restart,
        "r8ec53f25f8",
    )
    .readonly(),
    FieldDecl::new(
        "role",
        "rc47b54e84e",
        "agent_identity",
        ENUM,
        A,
        Restart,
        "re00fa43e67",
    )
    .options(&[("agent", "agent"), ("steward", "rdebccd161c")])
    .readonly(),
    FieldDecl::new(
        "feishu.app_id_env",
        "r47128c38c0",
        "agent_identity",
        STR,
        A,
        Restart,
        "r0953e2c07a",
    )
    .readonly(),
    FieldDecl::new(
        "feishu.app_secret_env",
        "r94a83c20d2",
        "agent_identity",
        STR,
        A,
        Restart,
        "r5601fb3a18",
    )
    .readonly(),
    FieldDecl::new(
        "feishu.scope",
        "r479b9a7987",
        "agent_identity",
        STR,
        A,
        Restart,
        "r289e1c75c6",
    )
    .readonly(),
    // ── agent 运行时 ──
    FieldDecl::new(
        "runtime.cli",
        "CLI",
        "agent_runtime",
        ENUM,
        A,
        NewSession,
        "rd1e7cbbf26",
    )
    .options(&[
        ("claude", "Claude Code"),
        ("codex", "Codex"),
        ("cursor", "Cursor"),
        ("traex", "traex"),
    ]),
    FieldDecl::new(
        "runtime.capture",
        "re72a3dc256",
        "agent_runtime",
        ENUM,
        A,
        NewSession,
        "r6a74402315",
    )
    .options(&[("rpc", "rd8f05517c0"), ("cli", "rff707e4ddd")]),
    FieldDecl::new(
        "model",
        "rc98e118e0a",
        "agent_runtime",
        STR,
        A,
        NextTurn,
        "rfc2d1d25a5",
    )
    .guarded(),
    FieldDecl::new(
        "effort",
        "rc8c14507b2",
        "agent_runtime",
        STR,
        A,
        NewSession,
        "ra3e94c5470",
    )
    .guarded(),
    FieldDecl::new(
        "reasoning_summary",
        "rc71d5942eb",
        "agent_runtime",
        ENUM,
        A,
        NewSession,
        "rb20375dc11",
    )
    .options(&[
        ("none", "r5cd6f073af"),
        ("concise", "r09e62bcda7"),
        ("auto", "r7eb336e42c"),
        ("detailed", "rca3634c751"),
    ]),
    FieldDecl::new(
        "runtime.working_dir",
        "r3db7b06b5f",
        "agent_runtime",
        Path,
        A,
        NewSession,
        "r1cd9294f6e",
    ),
    FieldDecl::new(
        "runtime.chat_working_dirs",
        "r6a2bf16868",
        "agent_runtime",
        StringMap,
        A,
        NewSession,
        "re87cdd23d9",
    )
    .guarded(),
    FieldDecl::new(
        "runtime.resume",
        "r02ab8908cd",
        "agent_runtime",
        Bool,
        A,
        NewSession,
        "rae310f8cba",
    )
    .inherits("resume_sessions"),
    FieldDecl::new(
        "runtime.env",
        "rae27b474ea",
        "agent_runtime",
        StringMap,
        A,
        NewSession,
        "r9f16825c52",
    )
    .sensitive(),
    FieldDecl::new(
        "runtime.cli_cmd",
        "r2910c1f56d",
        "agent_runtime",
        STR,
        A,
        NewSession,
        "r806d6daa58",
    ),
    FieldDecl::new(
        "runtime.model_context_window",
        "rbb074b86a9",
        "agent_runtime",
        Int,
        A,
        NewSession,
        "rea08054b76",
    )
    .min(1),
    FieldDecl::new(
        "runtime.codex_skip_git_repo_check",
        "r74828f8fdd",
        "agent_runtime",
        Bool,
        A,
        NewSession,
        "r0f101847bd",
    ),
    FieldDecl::new(
        "runtime.cursor_quota_token_command",
        "r067f0b4bf0",
        "agent_runtime",
        STR,
        A,
        NextPoll,
        "r5a1b223545",
    ),
    FieldDecl::new(
        "runtime.tmux_name",
        "rc0ec0879ce",
        "agent_runtime",
        STR,
        A,
        Restart,
        "r03e01d0298",
    )
    .readonly(),
    FieldDecl::new(
        "runtime.codex_sandbox",
        "sandbox",
        "agent_codex",
        ENUM,
        A,
        NewSession,
        "rc23346662d",
    )
    .options(SANDBOX_OPTIONS)
    .inherits("codex.sandbox"),
    FieldDecl::new(
        "runtime.codex_approval_policy",
        "r6952a625ca",
        "agent_codex",
        ENUM,
        A,
        NewSession,
        "r8f2fcc868b",
    )
    .options(APPROVAL_OPTIONS)
    .inherits("codex.approval_policy"),
    // ── agent 名单 ──
    FieldDecl::new(
        "feishu.allowed_users",
        "r7532a3f520",
        "agent_permissions",
        StringList,
        A,
        Immediate,
        "r5df21218b6",
    )
    .guarded(),
    FieldDecl::new(
        "feishu.operators",
        "rfdeb19d6f5",
        "agent_permissions",
        StringList,
        GA,
        Immediate,
        "re9c6c6143a",
    )
    .guarded(),
    FieldDecl::new(
        "aliases",
        "r27521b16c7",
        "agent_permissions",
        StringMap,
        A,
        NextTurn,
        "r897d5fe06d",
    ),
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::codex_policy::{APPROVAL_VALUES, SANDBOX_VALUES};
    use crate::config::config::CONFIG_FIELD_META;
    use crate::config::menubar::MENUBAR_ICONS;
    use crate::config::registry::CaptureOverride;
    use crate::config::settings::SETTINGS;
    use crate::driver::CliDriver;
    use std::collections::HashSet;

    fn must(key: &str) -> &'static FieldDecl {
        field(key).unwrap_or_else(|| panic!("字段表里没有 {key}"))
    }

    #[test]
    fn every_setting_decl_has_a_field() {
        for decl in SETTINGS {
            let key = format!("messaging.{}", decl.config_key);
            let f = must(&key);
            assert_eq!(f.kind, FieldKind::Bool, "{key}");
            assert_eq!(f.setting, Some(decl.id), "{key} 要绑到同一条分级开关");
            assert_eq!(
                f.has_layer(Layer::Chat),
                decl.chat_scoped,
                "{key} 按群可设与声明一致"
            );
            assert_eq!(f.inverted, decl.config_inverted, "{key} 取反与声明一致");
            assert_eq!(
                f.effect,
                Effect::of_consumer(decl.consumer),
                "{key} 生效时机"
            );
            assert_eq!(
                f.description(),
                decl.description,
                "{key} 说明引用声明,不另写"
            );
            assert!(
                f.has_layer(Layer::Global) && f.has_layer(Layer::Agent),
                "{key}"
            );
        }
        for f in FIELDS {
            if let Some(id) = f.setting {
                assert_eq!(f.key, format!("messaging.{}", decl_by_id(id).config_key));
            }
            if f.has_layer(Layer::Chat) {
                assert!(f.setting.is_some(), "{} 按群可设,必须是分级开关", f.key);
            }
        }
        assert_eq!(must("messaging.hide_thinking").to_desc().label, "显示思考");
    }

    /// Menu Bar 白名单(`crates/summon/src/setup/menubar_settings.rs` 的 `SettingKey::parse`)
    /// 每一项在字段表里的对应。summon-core 看不见那个 crate,所以名单写在这里;那边加一项,
    /// 这里跟着加一行。
    #[test]
    fn every_menubar_key_has_a_field() {
        let mapping: &[(&str, &[&str])] = &[
            ("sentinel", &["sentinel.enabled", "sentinel.dry_run"]),
            ("show_counts", &["menubar.show_counts"]),
            // 开机自启不在 config.yaml 里(autostart 单元),概览里只读显示。
            ("launch_at_login", &[]),
            ("cot_visible", &["messaging.cot_visible"]),
            ("hide_thinking", &["messaging.hide_thinking"]),
            ("cot_tool_details", &["messaging.cot_tool_details"]),
            ("badge_effort", &["messaging.badge_effort"]),
            ("idle_ttl_secs", &["messaging.idle_ttl_secs"]),
            ("idle_retire_secs", &["messaging.idle_retire_secs"]),
        ];
        assert_eq!(mapping.len(), 9);
        for (menubar_key, keys) in mapping {
            for key in *keys {
                let f = must(key);
                assert!(
                    f.has_layer(Layer::Global),
                    "{menubar_key} → {key} 要能在整机层改"
                );
                assert_ne!(f.write, Write::Readonly, "{menubar_key} → {key}");
            }
        }
        assert_eq!(
            must("sentinel.enabled").write,
            Write::Guarded,
            "开启前核验身份"
        );
        assert_eq!(
            must("messaging.idle_retire_secs").min,
            Some(0),
            "0 = 关,与 Menu Bar 一致"
        );
    }

    #[test]
    fn config_field_meta_paths_are_known_or_raw_only() {
        for meta in CONFIG_FIELD_META {
            let path = meta.path;
            let as_field = FIELDS.iter().any(|f| {
                f.has_layer(Layer::Global)
                    && (f.key == path
                        || f.key.strip_prefix(path).is_some_and(|r| r.starts_with('.')))
            });
            let noted = key_note(RawKind::Config, path).is_some();
            assert!(
                as_field || noted || meta.deprecated_since.is_some(),
                "CONFIG_FIELD_META 的 {path} 既不在字段表里,也没登记为原文模式 / 已废弃"
            );
        }
    }

    #[test]
    fn field_keys_are_unique() {
        let mut seen = HashSet::new();
        for f in FIELDS {
            assert!(seen.insert(f.key), "{} 声明了两次", f.key);
        }
        let mut groups = HashSet::new();
        for g in GROUPS {
            assert!(groups.insert(g.id), "分组 {} 声明了两次", g.id);
        }
        for n in KEY_NOTES {
            assert!(
                !FIELDS.iter().any(|f| f.key == n.path && f.in_file(n.file)),
                "{} 既在字段表里又登记为不进表单",
                n.path
            );
        }
    }

    #[test]
    fn preview_fields_have_contrast() {
        let mut previewed = 0;
        for f in FIELDS {
            let Some(contrast) = f.preview_contrast else {
                continue;
            };
            previewed += 1;
            assert!(f.to_desc().preview, "{}", f.key);
            match contrast {
                Contrast::Flip => assert_eq!(f.kind, FieldKind::Bool, "{} 只有布尔能取反", f.key),
                Contrast::Alt(a, b) => {
                    assert_ne!(f.kind, FieldKind::Bool, "{} 布尔用 Flip", f.key);
                    assert_ne!(a, b, "{}", f.key);
                    for lit in [a, b] {
                        let v: serde_yaml::Value = serde_yaml::from_str(lit).unwrap();
                        if let (Some(n), Some(min)) = (v.as_i64(), f.min) {
                            assert!(n >= min, "{} 的对照值 {lit} 小于下限", f.key);
                        }
                        if !f.options.is_empty() {
                            assert!(f.options.iter().any(|(o, _)| *o == lit), "{} {lit}", f.key);
                        }
                    }
                    let first: serde_yaml::Value = serde_yaml::from_str(a).unwrap();
                    assert_ne!(
                        contrast.against(&first),
                        first,
                        "{} 对照值要和当前值不同",
                        f.key
                    );
                }
            }
        }
        assert!(previewed >= 8, "影响飞书显示的开关都要能在预览里指出区块");
        let flip = Contrast::Flip.against(&serde_yaml::Value::Bool(true));
        assert_eq!(flip, serde_yaml::Value::Bool(false));
    }

    #[test]
    fn groups_sections_and_layers_fit_together() {
        for f in FIELDS {
            let g = group(f.group).unwrap_or_else(|| panic!("{} 的分组 {} 不存在", f.key, f.group));
            assert!(!f.layers.is_empty(), "{}", f.key);
            assert!(!f.has_layer(Layer::Builtin), "{} 内置层不可设置", f.key);
            assert!(
                !f.label.is_empty() && !f.description().is_empty(),
                "{}",
                f.key
            );
            match g.section {
                Section::System => assert_eq!(f.layers, &[Layer::Global], "{}", f.key),
                Section::Identity | Section::Runtime => {
                    assert_eq!(f.layers, &[Layer::Agent], "{}", f.key)
                }
                Section::Permissions => {
                    let expected = if f.key == "feishu.operators" { GA } else { A };
                    assert_eq!(f.layers, expected, "{}", f.key);
                }
                Section::Messaging => assert!(f.has_layer(Layer::Global), "{}", f.key),
            }
            if f.kind == FieldKind::Enum {
                assert!(!f.options.is_empty(), "{} 枚举要列出取值", f.key);
            } else {
                assert!(f.options.is_empty(), "{} 只有枚举带取值", f.key);
            }
            if f.write == Write::Readonly {
                assert!(f.preview_contrast.is_none(), "{}", f.key);
            }
        }
        for g in GROUPS {
            assert!(
                FIELDS.iter().any(|f| f.group == g.id),
                "分组 {} 是空的",
                g.id
            );
        }
        assert!(must("runtime.env").sensitive, "环境变量的字面值是密钥");
    }

    #[test]
    fn options_match_their_source_lists() {
        let values =
            |key: &str| -> Vec<&str> { must(key).options.iter().map(|(v, _)| *v).collect() };
        assert_eq!(values("codex.sandbox"), SANDBOX_VALUES);
        assert_eq!(values("runtime.codex_sandbox"), SANDBOX_VALUES);
        assert_eq!(values("codex.approval_policy"), APPROVAL_VALUES);
        assert_eq!(values("runtime.codex_approval_policy"), APPROVAL_VALUES);
        let icons: Vec<&str> = MENUBAR_ICONS.iter().map(|i| i.name).collect();
        assert_eq!(values("menubar.icon"), icons);
        // codex 的合法集是私有常量:用「user_config 已声明时只有合法值会被采用」来比对。
        for summary in values("reasoning_summary") {
            let taken = crate::driver::reasoning_summary_decision(Some(summary), true);
            assert_eq!(taken.as_deref(), Some(summary), "{summary}");
        }
        assert_eq!(values("reasoning_summary").len(), 4);
        for cli in values("runtime.cli") {
            assert_ne!(CliDriver::from_kind(cli), CliDriver::Other, "{cli}");
        }
        for capture in values("runtime.capture") {
            assert!(CaptureOverride::parse(capture).is_ok(), "{capture}");
        }
    }

    #[test]
    fn inherited_fields_point_to_a_global_field_of_the_same_kind() {
        for f in FIELDS {
            let Some(target) = f.inherits else { continue };
            let t = must(target);
            assert_eq!(
                t.layers,
                &[Layer::Global],
                "{} 继承的 {target} 只在整机层",
                f.key
            );
            assert_eq!(f.layers, &[Layer::Agent], "{}", f.key);
            assert_eq!(t.kind, f.kind, "{}", f.key);
            assert_eq!(t.options, f.options, "{}", f.key);
        }
        assert_eq!(must("runtime.resume").inherits, Some("resume_sessions"));
    }

    #[test]
    fn schema_serializes_like_the_contract() {
        let json = serde_json::to_value(schema()).unwrap();
        let fields = json["fields"].as_array().unwrap();
        let thinking = fields
            .iter()
            .find(|f| f["key"] == "messaging.hide_thinking")
            .unwrap();
        let mut keys: Vec<&str> = thinking
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "description",
                "effect",
                "group",
                "inverted",
                "key",
                "kind",
                "label",
                "layers",
                "preview",
                "sensitive",
                "write"
            ]
        );
        assert_eq!(thinking["inverted"], true);
        assert_eq!(thinking["kind"], "bool");
        assert_eq!(
            thinking["layers"],
            serde_json::json!(["global", "agent", "chat"])
        );
        assert_eq!(thinking["effect"], "next_turn");
        assert_eq!(thinking["write"], "form");
        assert_eq!(thinking["preview"], true);
        let ttl = fields
            .iter()
            .find(|f| f["key"] == "messaging.idle_ttl_secs")
            .unwrap();
        assert_eq!(ttl["kind"], "duration_secs");
        assert!(ttl.get("inverted").is_none(), "不取反的项不写这个键");
        assert!(ttl["min"].is_i64());
        let sandbox = fields.iter().find(|f| f["key"] == "codex.sandbox").unwrap();
        assert_eq!(sandbox["options"][0]["value"], "read-only");
        let group = &json["groups"][0];
        assert!(group["id"].is_string() && group["section"].is_string());
        let back: Schema = serde_json::from_value(json).unwrap();
        assert_eq!(back, schema());
    }
}
