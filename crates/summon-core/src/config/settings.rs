//! 分级开关的声明式注册表:一个开关 = 一条声明,解析/合并/传播/存储全部由声明派生。
//!
//! **边界**:开关的前三层(内置默认 → `~/.summon/config.yaml` → `agent.yaml`)由 [`Messaging`]
//! 的 serde + `apply()` 叠好,那条链不归本模块管。这里管的是第四层(per-chat)以及它带来的
//! 存储、合并、传播 —— 这三件事都随开关数增长,所以必须由声明派生而不是逐个手写。
//!
//! 四件事:
//! 1. 声明一张表([`SETTINGS`]),把「key ↔ [`Messaging`] 字段 ↔ 谁消费」三者绑在一处;
//! 2. 把第四层解析收成一个函数([`ResolvedSettings::resolve`]);
//! 3. 把 per-chat 覆盖收成一个值([`ChatOverrides`]),对应盘上一个文件;
//! 4. 把「改了之后什么时候生效」编进类型([`Consumer`])——这是唯一无法从字段本身推出来的信息。
//!
//! 加一个分级开关的完整代价:`Messaging` 加字段(serde 真相源仍是 `messaging.default.yaml`)、
//! `MessagingOverride` 加字段、`apply()` 加分支、本文件加一条声明。前三条是 serde 样板,
//! 由 [`tests::every_declared_key_is_actually_overridable`] 逐条验证「声明了就真能被覆盖」——
//! 漏写 `apply()` 分支不会编译报错,但会被那个测试逮住。

use super::config::{Messaging, PrimerSections};

/// 开关的下游消费者。**它编码的不是「谁去读」,而是「改了之后要做什么」** —— 这是同一张表里
/// 唯一无法从字段类型推出来的信息,也是最容易漏做的一环。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Consumer {
    /// daemon 自己读(渲染、调度)。改了下一轮读新快照即生效,不需要任何额外动作。
    Daemon,
    /// 值进了**注入给 agent 的 PRIMER 文本**。PRIMER 只在会话冷启/resume 时注入
    /// (Transcript 走 `--append-system-prompt-file` 启动期参数、Rpc 冷启首轮前置文本),
    /// 所以改了之后**当前会话内不生效**,要等下次会话重启。回执必须如实说明,
    /// 否则用户会以为开关坏了。
    AgentPrompt,
}

/// PRIMER 在什么时机会被重新注入。**按 CLI 分,不是按开关分** —— 同一个 `AgentPrompt` 开关
/// 在不同 CLI 上的生效条件不同,而用户看到的回执必须是他那个群里的真实条件。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrimerReinject {
    /// 每次会话启动都重注,**含接回上下文的 resume**。拆一次会话(`/quit` 后再说话、
    /// 空闲退出后续聊)即可生效。
    EveryStart,
    /// 只有全新会话才注:接回上下文的那种 resume 不重注(PRIMER 已经在历史里,重注既污染
    /// 对话又伤缓存),所以旧约定会一直留在上下文里 —— 必须新建会话才换得掉。
    ColdStartOnly,
}

impl Consumer {
    /// 切换成功后回执里那句「什么时候生效」。文案跟着消费者类型和 PRIMER 重注时机走,
    /// 不由各调用点自己编。
    pub fn effect_notice(self, reinject: PrimerReinject) -> &'static str {
        match (self, reinject) {
            (Consumer::Daemon, _) => summon_i18n::text!("rb28b7c1333"),
            (Consumer::AgentPrompt, PrimerReinject::EveryStart) => {
                summon_i18n::text!("r067cc42f2a")
            }
            (Consumer::AgentPrompt, PrimerReinject::ColdStartOnly) => {
                summon_i18n::text!("r956fc61784")
            }
        }
    }
}

/// 开关的稳定标识。用枚举而不是字符串做索引:调用点写错 key 是编译错误,
/// 而 [`ResolvedSettings`] 可以用定长数组存值,解析一次后读取零查找。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SettingKey {
    /// CoT 过程整体外显。
    CotVisible,
    /// 思考正文是否上卡(注意与 `Messaging::hide_thinking` **语义相反**:
    /// 这里以用户可见语义为准,「显示」= true)。
    CotThinking,
    /// CoT 是否展示工具调用(命令、参数、结果);关时 CoT 只有思考正文。
    CotToolDetails,
    /// 定时会话空闲归档时是否重置(清 resume 记录)。
    TimerIdleRetireReset,
    /// 是否给 agent 注入「贴语义表情」那段约定。关 → 该段整段不进 PRIMER。
    /// **只隔离 prompt**:`summon react` 命令本身不加闸,系统自动的 OnIt/DONE 工程表情不受影响。
    AgentReactions,
    /// 回复卡角标是否显示思考档位。
    BadgeEffort,
    /// 是否给 agent 注入「回复末尾写 feed 总结标签」那段约定。**只隔离 prompt**:回复里的标签
    /// 无论开关如何都会被摘掉。
    AgentFeed,
    /// 文档评论自动起草与审批发布。
    DocumentComments,
}

/// 一条开关声明。`get` 把 key 绑到 [`Messaging`] 的对应字段上 —— 默认值的真相源因此仍是
/// `messaging.default.yaml`,本表不复制任何默认值(复制一份就会有两个版本,而过期的那份
/// 照样会被读到)。
pub struct SettingDecl {
    /// 盘上/命令行里的稳定名字。per-chat 文件与 `/set` 命令都用它。
    pub key: &'static str,
    /// 同一个开关在 `config.yaml` / `agent.yaml` 里的字段名(历史上是扁平命名,与点分 `key`
    /// 不同形)。**映射只能有这一份** —— `MessagingOverride` 没有 `deny_unknown_fields`,
    /// 用户照着 `/set` 的名字往 yaml 里写会被静默忽略,所以这两个名字必须能被一起打出来。
    pub config_key: &'static str,
    pub id: SettingKey,
    /// 面向用户的中文名,切换回执用。
    pub label: &'static str,
    /// 从前三层已解析的 [`Messaging`] 取值(**用户可见语义**,`true` = 开)。
    pub get: fn(&Messaging) -> bool,
    /// 能不能在 per-chat 那一层设置。前三层(内置 / config.yaml / agent.yaml)对所有开关都开放,
    /// 只有会话级需要逐个声明 —— `/set` 的候选集与 per-chat 文件的合法 key 都由它决定。
    pub chat_scoped: bool,
    pub consumer: Consumer,
    /// 一句话说明这个开关管什么。`/set` 的列表会把它打出来 —— key 是点分命名空间,
    /// 只给 key 和一个「开/关」等于要用户自己猜它管什么。
    pub description: &'static str,
    /// yaml 字段的语义是否与 `key` **相反**(目前只有 `hide_thinking` ↔ `cot.thinking`:
    /// 前者存「隐藏」,后者按「显示」暴露)。写进声明而不是散在各处取反,是因为取反漏一次
    /// 就是开关行为整个反过来,而那种 bug 在读代码时看不出来。
    pub config_inverted: bool,
}

/// 全部分级开关。加开关在这里加一行。
pub const SETTINGS: &[SettingDecl] = &[
    SettingDecl {
        key: "document_comments.enabled",
        config_key: "document_comments_enabled",
        id: SettingKey::DocumentComments,
        label: "r2549c41532",
        get: |m| m.document_comments_enabled,
        chat_scoped: false,
        consumer: Consumer::Daemon,
        config_inverted: false,
        description: "r8261f74e52",
    },
    SettingDecl {
        key: "cot.visible",
        config_key: "cot_visible",
        id: SettingKey::CotVisible,
        label: "re4efd66267",
        get: |m| m.cot_visible,
        chat_scoped: true,
        consumer: Consumer::Daemon,
        config_inverted: false,
        description: "r304faa67b1",
    },
    SettingDecl {
        key: "cot.thinking",
        config_key: "hide_thinking",
        id: SettingKey::CotThinking,
        label: "r3de8c943a2",
        // `hide_thinking` 存的是「隐藏」,这里取反成「显示」——盘上与命令行一律用用户可见语义,
        // 免得 `/set cot.thinking off` 这种写法还要用户自己在脑子里做一次取反。
        get: |m| !m.hide_thinking,
        chat_scoped: true,
        consumer: Consumer::Daemon,
        config_inverted: true,
        description: "raf3b4b2e3a",
    },
    SettingDecl {
        key: "cot.tools",
        config_key: "cot_tool_details",
        id: SettingKey::CotToolDetails,
        label: "r009ad24e28",
        get: |m| m.cot_tool_details,
        chat_scoped: true,
        consumer: Consumer::Daemon,
        config_inverted: false,
        description: "rd5b3943071",
    },
    SettingDecl {
        key: "timer.idle_retire_reset",
        config_key: "timer_idle_retire_reset",
        id: SettingKey::TimerIdleRetireReset,
        label: "r68b988a7f9",
        get: |m| m.timer_idle_retire_reset,
        chat_scoped: true,
        consumer: Consumer::Daemon,
        config_inverted: false,
        description: "r1a7ca0ac60",
    },
    SettingDecl {
        key: "badge.effort",
        config_key: "badge_effort",
        id: SettingKey::BadgeEffort,
        label: "r3dbd9d268d",
        get: |m| m.badge_effort,
        chat_scoped: true,
        consumer: Consumer::Daemon,
        config_inverted: false,
        description: "rf1b8002dee",
    },
    SettingDecl {
        key: "agent.reactions",
        config_key: "agent_reactions",
        id: SettingKey::AgentReactions,
        label: "r5dd04b0f9d",
        get: |m| m.agent_reactions,
        chat_scoped: true,
        consumer: Consumer::AgentPrompt,
        config_inverted: false,
        description: "r1cc2e1bd84",
    },
    SettingDecl {
        key: "agent.feed",
        config_key: "agent_feed",
        id: SettingKey::AgentFeed,
        label: "r8a0d818df0",
        get: |m| m.agent_feed,
        chat_scoped: true,
        consumer: Consumer::AgentPrompt,
        config_inverted: false,
        description: "r254d704450",
    },
];

/// 按 key 查声明。`/set` 与配置校验用。
pub fn decl_by_key(key: &str) -> Option<&'static SettingDecl> {
    SETTINGS.iter().find(|d| d.key == key)
}

/// 按 id 查声明。
pub fn decl_by_id(id: SettingKey) -> &'static SettingDecl {
    SETTINGS
        .iter()
        .find(|d| d.id == id)
        .expect(summon_i18n::text!("r1a61f38e04"))
}

/// 可以在 per-chat 那一层设置的开关。`/set` 的候选集与用法提示都从它派生。
pub fn chat_scoped() -> impl Iterator<Item = &'static SettingDecl> {
    SETTINGS.iter().filter(|d| d.chat_scoped)
}

/// per-chat 覆盖:每个 key 要么显式 `Some(值)`,要么缺省 `None`(继承上一层)。
///
/// 全部 key 落在同一个文件(`runtime/chats/<session>.settings`),因此有唯一的原子提交点 ——
/// 多个入口(`/set`、会话面板按钮)并发改不同的键时,不会各写各的文件而失去一致性。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ChatOverrides {
    values: [Option<bool>; SETTING_COUNT],
}

/// 开关数组容量；每条声明的槽位与可覆盖行为由配置测试验证。
pub const SETTING_COUNT: usize = 8;

/// [`SettingKey`] 到定长数组下标的映射。新增 key 时编译器会在这里报 non-exhaustive。
const fn slot(id: SettingKey) -> usize {
    match id {
        SettingKey::CotVisible => 0,
        SettingKey::CotThinking => 1,
        SettingKey::CotToolDetails => 2,
        SettingKey::TimerIdleRetireReset => 3,
        SettingKey::AgentReactions => 4,
        SettingKey::BadgeEffort => 5,
        SettingKey::AgentFeed => 6,
        SettingKey::DocumentComments => 7,
    }
}

impl ChatOverrides {
    pub fn get(&self, id: SettingKey) -> Option<bool> {
        self.values[slot(id)]
    }

    /// 设为显式值。
    pub fn set(&mut self, id: SettingKey, value: bool) {
        self.values[slot(id)] = Some(value);
    }

    /// 恢复继承(`inherit`)。
    pub fn clear(&mut self, id: SettingKey) {
        self.values[slot(id)] = None;
    }

    /// 渲染成盘上文本。**全量输出所有 key**(含 `inherit`),这样文件本身就是一份完整的
    /// 当前状态,读的人不必去比对哪些 key 没写。
    pub fn render(&self) -> String {
        let mut out = String::new();
        for decl in chat_scoped() {
            let value = match self.get(decl.id) {
                Some(true) => "on",
                Some(false) => "off",
                None => "inherit",
            };
            out.push_str(decl.key);
            out.push('=');
            out.push_str(value);
            out.push('\n');
        }
        out
    }

    /// 严格解析:未知 key、非法值、重复 key 一律报错。
    ///
    /// **写路径用它**——写之前必须能读懂现状,否则全量覆盖会把用户已设的其它开关清零;
    /// 而用户手误若被静默跳过,就成了「设置看起来生效了其实没有」,没有任何外部表征。
    ///
    /// 读路径用 [`Self::parse_lenient`],理由见那里。
    pub fn parse(text: &str) -> Result<ChatOverrides, SettingsParseError> {
        let mut out = ChatOverrides::default();
        let mut seen: Vec<&'static str> = Vec::new();
        for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
            let (key, value) = line
                .split_once('=')
                .ok_or_else(|| SettingsParseError::MalformedLine(line.to_string()))?;
            let key = key.trim();
            let decl =
                decl_by_key(key).ok_or_else(|| SettingsParseError::UnknownKey(key.to_string()))?;
            if !decl.chat_scoped {
                return Err(SettingsParseError::NotChatScoped(decl.key));
            }
            if seen.contains(&decl.key) {
                return Err(SettingsParseError::DuplicateKey(decl.key));
            }
            seen.push(decl.key);
            match value.trim() {
                "on" => out.set(decl.id, true),
                "off" => out.set(decl.id, false),
                "inherit" => out.clear(decl.id),
                other => {
                    return Err(SettingsParseError::BadValue {
                        key: decl.key,
                        value: other.to_string(),
                    })
                }
            }
        }
        Ok(out)
    }

    /// 宽容解析:认不出的 key **跳过并报告**,其余照常严格。
    ///
    /// **只给读路径用**。文件内容由写它的那个版本的 [`SETTINGS`] 决定,而 key 会随版本改名或
    /// 删除 —— 用严格解析去读上一个版本写下的文件,一次改名就会让所有已有 `.settings` 的会话
    /// 集体失效:读退成全继承(用户的设置静默消失)、写被拒(`/set` 与面板按钮双双报「落盘失败」),
    /// 且没有自愈路径。用户手误与「自己上个版本写的文件」不是同一件事,不该同一个判据。
    ///
    /// 返回被跳过的 key,调用方据此留日志 —— 跳过必须留痕,否则又成了静默。
    pub fn parse_lenient(text: &str) -> (ChatOverrides, Vec<String>) {
        let mut out = ChatOverrides::default();
        let mut unknown = Vec::new();
        for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
            let Some((key, value)) = line.split_once('=') else {
                unknown.push(line.to_string());
                continue;
            };
            let key = key.trim();
            let Some(decl) = decl_by_key(key).filter(|d| d.chat_scoped) else {
                unknown.push(key.to_string());
                continue;
            };
            match value.trim() {
                "on" => out.set(decl.id, true),
                "off" => out.set(decl.id, false),
                "inherit" => out.clear(decl.id),
                _ => unknown.push(key.to_string()),
            }
        }
        (out, unknown)
    }
}

/// per-chat 设置文件的解析失败。库层用 thiserror(应用层再包 `anyhow`),
/// 每一种失败都带上出问题的那个 key —— 回执要能直接告诉用户改哪一行。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SettingsParseError {
    #[error("{}", summon_i18n::format!("r34abdc5063", _0 = .0))]
    MalformedLine(String),
    #[error("{}", summon_i18n::format!("r5a2346eef0", _0 = .0))]
    UnknownKey(String),
    /// 写路径遇到只到 agent 级的 key。不许把它写进 per-chat 文件 —— `render()` 只遍历会话级
    /// key,写进去的值会被下一次全量写悄悄丢掉。
    #[error("{}", summon_i18n::format!("r74c27610c6", _0 = .0))]
    NotChatScoped(&'static str),
    #[error("{}", summon_i18n::format!("rb425cc809c", _0 = .0))]
    DuplicateKey(&'static str),
    #[error("{}", summon_i18n::format!("r1df3c8ba5d", key = .key, value = .value))]
    BadValue { key: &'static str, value: String },
}

/// 一轮内不可变的解析结果。**快照边界必须是一轮**:配置热重载与 `/set` 都只影响下一轮,
/// 同一轮的前半段和后半段绝不能看到不同的值(渲染会自相矛盾,而这种错位极难复现)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedSettings {
    values: [bool; SETTING_COUNT],
}

impl ResolvedSettings {
    /// 四层合并:per-chat 显式值优先,否则用前三层叠好的 [`Messaging`]。
    pub fn resolve(messaging: &Messaging, chat: &ChatOverrides) -> ResolvedSettings {
        let mut values = [false; SETTING_COUNT];
        for decl in SETTINGS {
            let inherited = (decl.get)(messaging);
            values[slot(decl.id)] = chat.get(decl.id).unwrap_or(inherited);
        }
        ResolvedSettings { values }
    }

    pub fn get(&self, id: SettingKey) -> bool {
        self.values[slot(id)]
    }

    /// CoT 过程是否外显。
    pub fn cot_visible(&self) -> bool {
        self.get(SettingKey::CotVisible)
    }

    /// 是否**隐藏**思考正文。注册表按「显示」存,消费点历史上按「隐藏」问,这里做唯一一次取反。
    pub fn hide_thinking(&self) -> bool {
        !self.get(SettingKey::CotThinking)
    }

    /// 是否展示工具命令与结果详情。
    pub fn cot_tool_details(&self) -> bool {
        self.get(SettingKey::CotToolDetails)
    }

    /// 定时会话空闲归档时是否重置。
    pub fn timer_idle_retire_reset(&self) -> bool {
        self.get(SettingKey::TimerIdleRetireReset)
    }

    /// 是否给 agent 注入表情约定段。
    pub fn agent_reactions(&self) -> bool {
        self.get(SettingKey::AgentReactions)
    }

    /// 回复卡角标是否显示思考档位。
    pub fn badge_effort(&self) -> bool {
        self.get(SettingKey::BadgeEffort)
    }

    /// 是否给 agent 注入 feed 总结段。
    pub fn agent_feed(&self) -> bool {
        self.get(SettingKey::AgentFeed)
    }

    /// PRIMER 的可选段按本会话开关怎么注入。
    pub fn primer_sections(&self) -> PrimerSections {
        PrimerSections {
            reactions: self.agent_reactions(),
            feed: self.agent_feed(),
            // 取决于会话的捕获形态,不是开关;知道形态的调用点自己置位。
            background_continuation: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::config::{builtin_messaging, MessagingOverride};

    #[test]
    fn keys_are_unique_and_slots_are_distinct() {
        let mut keys: Vec<_> = SETTINGS.iter().map(|d| d.key).collect();
        keys.sort_unstable();
        let before = keys.len();
        keys.dedup();
        assert_eq!(before, keys.len(), "key 必须唯一");

        let mut slots: Vec<_> = SETTINGS.iter().map(|d| slot(d.id)).collect();
        slots.sort_unstable();
        slots.dedup();
        assert_eq!(slots.len(), SETTINGS.len(), "每条声明必须占独立槽位");
    }

    /// 声明表里每个 key 都必须真的能被 `agent.yaml` 覆盖。
    ///
    /// **为什么需要这个测试**:`Messaging` 字段 / `MessagingOverride` 字段 / `apply()` 分支是三张
    /// 平行表,漏写 `apply()` 那条分支**不会编译报错** —— 结果是配置写了没反应,而这种失败
    /// 只在真机上、只在有人真去配它的时候才暴露。这里逐条翻转默认值再套 override,
    /// 值没变就说明那条 `apply()` 分支不存在。
    #[test]
    fn every_declared_key_is_actually_overridable() {
        for decl in SETTINGS {
            let base = builtin_messaging();
            let want = !(decl.get)(&base);
            let (field, inverted) = (decl.config_key, decl.config_inverted);
            // `hide_thinking` 在 yaml 里存的是「隐藏」,声明表按「显示」暴露 —— 写进 yaml 前
            // 要还原成字段自己的语义,否则这个测试会把一次正确的取反误判成缺失的 apply 分支。
            let written = if inverted { !want } else { want };
            let yaml = format!("{field}: {written}");
            let o: MessagingOverride = serde_yaml::from_str(&yaml)
                .unwrap_or_else(|e| panic!("{} 的 override yaml 解析失败: {e}", decl.key));
            let mut m = base;
            m.apply(&o);
            assert_eq!(
                (decl.get)(&m),
                want,
                "{} 声明了却没有生效的 apply() 分支",
                decl.key
            );
        }
    }

    #[test]
    fn chat_layer_wins_over_inherited() {
        let m = builtin_messaging();
        let mut chat = ChatOverrides::default();
        let inherited = ResolvedSettings::resolve(&m, &chat);
        assert!(inherited.agent_reactions(), "表情约定注入默认开");

        chat.set(SettingKey::AgentReactions, false);
        let overridden = ResolvedSettings::resolve(&m, &chat);
        assert!(!overridden.agent_reactions());

        chat.clear(SettingKey::AgentReactions);
        assert_eq!(ResolvedSettings::resolve(&m, &chat), inherited);
    }

    /// 出厂默认:思考正文上卡,工具调用不上 CoT(owner 2026-09-17)。
    #[test]
    fn builtin_defaults_think_on_tools_off() {
        let m = builtin_messaging();
        let resolved = ResolvedSettings::resolve(&m, &ChatOverrides::default());
        assert!(resolved.get(SettingKey::CotVisible), "CoT 整体默认外显");
        assert!(resolved.get(SettingKey::CotThinking), "思考正文默认上卡");
        assert!(!resolved.get(SettingKey::CotToolDetails), "工具详情默认关");
    }

    /// `hide_thinking` 是全表唯一语义反转的字段,取反只允许发生在一处。
    #[test]
    fn thinking_key_is_stored_as_display_semantics() {
        let mut m = builtin_messaging();
        m.hide_thinking = true;
        let resolved = ResolvedSettings::resolve(&m, &ChatOverrides::default());
        assert!(resolved.hide_thinking());
        assert!(!resolved.get(SettingKey::CotThinking));

        let mut chat = ChatOverrides::default();
        chat.set(SettingKey::CotThinking, true);
        assert!(!ResolvedSettings::resolve(&m, &chat).hide_thinking());
    }

    #[test]
    fn render_round_trips_through_parse() {
        let mut chat = ChatOverrides::default();
        chat.set(SettingKey::CotVisible, false);
        chat.set(SettingKey::AgentReactions, false);
        chat.set(SettingKey::CotThinking, true);
        let parsed = ChatOverrides::parse(&chat.render()).expect("自产文本必须能读回");
        assert_eq!(parsed, chat);
    }

    #[test]
    fn render_covers_every_chat_scoped_key() {
        let text = ChatOverrides::default().render();
        for decl in chat_scoped() {
            assert!(
                text.contains(&format!("{}=inherit", decl.key)),
                "{} 未出现在全量渲染里",
                decl.key
            );
        }
    }

    /// 手误必须报错而不是静默跳过 —— 静默跳过的失败没有任何外部表征。
    #[test]
    fn parse_rejects_malformed_input() {
        for bad in [
            "cot.visible",
            "cot.visible=maybe",
            "cot.unknown=on",
            "cot.visible=on\ncot.visible=off",
        ] {
            assert!(ChatOverrides::parse(bad).is_err(), "{bad} 应被拒绝");
        }
        assert_eq!(
            ChatOverrides::parse("").expect("空文件 = 全继承"),
            ChatOverrides::default()
        );
    }

    /// PRIMER 类开关的回执必须说清「当前会话不生效」,而且两种重注时机要给出**不同**的动作 ——
    /// 对接回上下文时不重注的 CLI 说「/quit 就行」是假的,用户照做后开关不生效,
    /// 只会认为开关坏了。
    #[test]
    fn agent_prompt_notice_matches_how_the_primer_is_actually_reinjected() {
        let decl = decl_by_id(SettingKey::AgentReactions);
        assert_eq!(decl.consumer, Consumer::AgentPrompt);

        let every = decl.consumer.effect_notice(PrimerReinject::EveryStart);
        let cold = decl.consumer.effect_notice(PrimerReinject::ColdStartOnly);
        assert!(every.contains("重启"));
        assert!(cold.contains("新建会话"));
        assert!(cold.contains("`/quit` 不够"), "必须点破 /quit 不管用");
        assert_ne!(every, cold);

        for reinject in [PrimerReinject::EveryStart, PrimerReinject::ColdStartOnly] {
            let daemon = Consumer::Daemon.effect_notice(reinject);
            assert!(!daemon.contains("重启"), "daemon 类下一轮就生效");
            assert!(!daemon.contains("新建会话"));
        }
    }
}
