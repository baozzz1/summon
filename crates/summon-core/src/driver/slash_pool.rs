//! **slash 准入**:哪些 provider slash 命令可以从飞书透传,以及每条的**收尾语义**。
//! 纯数据 + 纯判定,零 IO。设计见 `docs/spec/2026-08-14-transcript-slash-passthrough-design.md`
//! 第四、六、十一节。
//!
//! **准入模型是黑名单**(owner 2026-08-14 决策,反转了此前的白名单/采样池准入):
//! 默认放行,发现 badcase 录入 [`SlashBlacklist`] 即可。反转的前提是承重结构已经就位 ——
//! 注入前的输入态正向证明、受限 Esc、at-most-once、fail-closed 回执 —— 未知命令的最坏情形
//! 已经从「会话被面板锁死」降为「一份诚实的失败/不确定回执」。
//!
//! 四条形状上的硬约束,写在这里是因为它们决定了这个模块为什么长这样:
//!
//! 1. **透传的最小单位是「命令名 + 收尾语义」,不是命令名。** 分类错的代价不对称:把 panel 类
//!    当 print 类 = 面板留在屏上吞掉后续每一条注入(实测 `/cost` 面板吃掉紧跟的 `/compact`),
//!    而反过来只是多等一次。所以每条已知命令都带着自己的收尾方式入池。
//! 2. **采样池从「准入门」降级成「已知命令的优化层」。** 池里每一条仍记着真机采样凭据与调优过的
//!    时间窗/收尾语义,准入反转**一字没动**它们 —— 变的只是「不在池里」不再等于「拒绝」,
//!    而是走 [`classify_generic`] 的通用路。
//! 3. **未知命令的归类发生在注入之后**,由观测定(面板特征 / spinner+转录增长 / 回到空输入框),
//!    判不出就诚实说不确定。事前猜一个类别正是白名单时代要避免的那种不对称错误。
//! 4. **生命周期命令永不裸注入**,它们连同**同形变体**在准入函数里就被收口(见
//!    `lifecycle_lookalike`)—— 反转之后这是一道新增的必需闸:池外一切曾经免费地落在「拒绝」
//!    那一侧,现在默认是「注入」。

use serde::{Deserialize, Serialize};

use crate::config::registry::CaptureMode;
use crate::driver::CliDriver;
use crate::handoff::HANDOFF_COMMAND;
use crate::pane_input::{ModalKind, PaneState};

/// 一条池内命令对**参数**的态度。参数是语义注入面(`/compact <指令>` 实测会被当成压缩指令执行),
/// 所以默认答案是 `ExactOnly`;开放带参是 owner 级授权,不是实现侧的便利。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArgsPolicy {
    /// 只认 exact 无参形态,带参一律拒。**生命周期类与 denylist 恒为此值** —— 参数会改变
    /// 拆除语义,且形态不可采样穷尽。
    ///
    /// (`/compact` 曾经也在此列,理由是「参数就是压缩指令本身」。owner 2026-08-20 判定那正是
    /// 想要的能力 —— 让用户指定压缩时保留什么 —— 故改判 `Passthrough`。生命周期类不适用同一
    /// 推理:那里的参数改变的是**拆不拆、怎么拆**,不是内容取舍。)
    ExactOnly,
    /// 接受带参形态:首 token 命中本条目,就把**整行原文**(含参数)纳入注入计划,依旧零 marker。
    /// **owner 2026-08-14 显式授权,起初只开放给透传类(print/panel)**(直接诱因:`/context all` 被拒);
    /// **2026-08-20 扩到 Slow 类的 `/compact`** —— 那条的参数是压缩指令,正是用户要的能力。
    /// 收尾语义沿用该命令的基线类别(print 仍 print、panel 仍 panel)—— 参数变体若渲染出意外形态
    /// (弹了别的面板、迟迟不到收尾态),由既有 settle 超时 / 受限 Esc / fail-closed 机制兜底,
    /// 不为参数形态另设判据。
    ///
    /// 唯一例外是**双形态条目**(`args_class`,目前只有 `/goal`):owner 2026-08-14 另行授权
    /// `/goal <目标>` 为基础要求,其带参形态按 [`SlashClass::Turn`] 收尾 —— 那是一条真轮,
    /// 不沿用裸形态的 panel 判据。
    Passthrough,
}

/// 一条命令跑完之后,pane 会停在什么形态 —— 决定了怎么判「跑完了」、怎么取回显、要不要退出。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlashClass {
    /// 输出落进转录区,自己回到空输入框(`/context`)。收尾 = 空输入行;回显 = 新增转录。
    Print,
    /// 全屏面板,不会自己退(`/status` `/cost`)。收尾 = 认出面板 → 抓正文回传 → 受限 Esc →
    /// **验证回到空输入框**才算成功;退不出去按 fail-closed 如实相告。
    Panel,
    /// 慢命令(`/compact`),有自己的专用等待与卡片回执。宽窗口,走既有 `handle_compact`。
    Slow,
    /// **本质是 prompt** 的命令(`/goal <目标>`):注入后 CLI 当场开一轮真实流式轮(真机:
    /// `⎿ Goal set: …` 即时回执后 3s 内开始跑工具)。**没有抓屏收尾** —— 收尾语义就是普通轮:
    /// 执行器只证明「已提交」(命令离开输入行),不等收尾、不抓屏回执、更不 Esc(那会打断刚开
    /// 的轮);随后由**调用方持有真实轮生命周期直到 terminal**(与普通 prompt 轮同构的消费路:
    /// 流式卡、打断、收尾表情、完成总结;镜像归属由显式 markerless fence 按偏移裁决)。回执唯一
    /// 在这里指**判决唯一**:成功路的判决就是轮自己的投递,未交轮的结局(不确定/被拒)仍一份判决。
    Turn,
}

/// 采样池里的一条。生产路径上的条目**只**来自本文件的静态池 —— 挡住「配置新增命令」的不是这个
/// 类型的可见性,而是 [`SlashToggles`](它只认名字、只能启停)。类型本身是可构造的,测试会拿它
/// 造零窗口条目去验超时分支。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlashEntry {
    /// exact 命令名(含 `/`)。首 token 按它匹配;是否接受参数由 `args` 决定。
    pub name: &'static str,
    pub class: SlashClass,
    /// 参数策略,见 [`ArgsPolicy`]:透传类(print/panel)开放整行原文(owner 2026-08-14 授权),
    /// Slow 保持 exact-only。要改必须在本文件里对这条命令单独声明。
    pub args: ArgsPolicy,
    /// 带参形态的收尾类别,**仅当与裸形态不同才声明**(`None` = 沿用 `class`)。唯一样本
    /// `/goal`:裸形态弹目标面板(panel),带参形态开真轮(turn)—— 真机采样两种形态收尾
    /// 完全不同,一个 `class` 字段说不了两件事。配错任何一边都会锁死会话:拿 panel 判据等
    /// 一条真轮 = 必然超时;拿 turn 语义跑面板 = 面板留屏吞掉后续每一条注入。
    pub args_class: Option<SlashClass>,
    /// 收尾等待:画面静止多久算收尾 / 总时长硬顶(秒)。**快慢命令不共享时间窗** ——
    /// 用 compact 的 240s 等 `/status` 是白扣 4 分钟,用秒级窗等 compact 是必然误报。
    pub stall_secs: u64,
    pub cap_secs: u64,
    /// 默认是否开启。灰度期新形态可以先以 false 入池,由配置显式打开。
    pub default_enabled: bool,
    /// **入池凭据**:真机采样的时间与观察到的形态。没有这一栏就不该有这一条。
    pub evidence: &'static str,
}

impl SlashEntry {
    /// 这条注入原文对应的**有效收尾类别**:首 token 之外还有内容 = 带参形态。判据与
    /// [`plan_slash`] 的 `has_args` 同构(trim 后整行不等于命令名);调用方须先保证首 token
    /// 就是本条目(执行器的零按键守卫已钉住,配错的计划在发键之前就被拒)。
    pub fn class_for_line(&self, line: &str) -> SlashClass {
        if line.trim() == self.name {
            self.class
        } else {
            self.args_class.unwrap_or(self.class)
        }
    }
}

/// claude 桶。本期只填 claude(spec 第七节 non-goal:Cursor/Codex 的 TUI 采样另说)。
static CLAUDE_POOL: &[SlashEntry] = &[
    SlashEntry {
        name: "/compact",
        class: SlashClass::Slow,
        // owner 2026-08-20 授权带参:`/compact <指令>` 的参数就是给 claude 的压缩指令
        // (「只保留架构决策」这类)。这正是 `ArgsPolicy` 文档说的「开放带参是 owner 级授权」。
        // 收尾语义不变 —— 仍是 Slow,走 `handle_compact` 的宽窗口与卡片回执;参数只改变
        // claude 压缩时保留什么,不改变「这是一条慢命令」这件事。
        args: ArgsPolicy::Passthrough,
        args_class: None,
        // 实测 2.5 分钟量级;判据坏掉那阵 cap 是唯一出路,故留宽。沿用既有 compact 专用窗口。
        stall_secs: 240,
        cap_secs: 600,
        default_enabled: true,
        evidence: "r28c3626a34",
    },
    SlashEntry {
        name: "/context",
        class: SlashClass::Print,
        args: ArgsPolicy::Passthrough,
        args_class: None,
        // 真机 ~8s(要跑一遍 token 统计)。60s 硬顶足够,且远短于 compact —— 这正是分窗口的意义。
        stall_secs: 20,
        cap_secs: 60,
        default_enabled: true,
        evidence: "r0e11d4cdb3",
    },
    SlashEntry {
        name: "/status",
        class: SlashClass::Panel,
        args: ArgsPolicy::Passthrough,
        args_class: None,
        stall_secs: 20,
        cap_secs: 60,
        default_enabled: true,
        evidence: "r0135fbe6b8",
    },
    SlashEntry {
        name: "/cost",
        class: SlashClass::Panel,
        args: ArgsPolicy::Passthrough,
        args_class: None,
        stall_secs: 20,
        cap_secs: 60,
        default_enabled: true,
        evidence: "radb1403ed3",
    },
    // 池里第一条**双形态**命令(owner 2026-08-14 授权,`/goal <目标>` 是基础要求)。
    // stall/cap 只约束裸形态的 panel 收尾;turn 形态提交即返回,不消费时间窗。
    SlashEntry {
        name: "/goal",
        class: SlashClass::Panel,
        args: ArgsPolicy::Passthrough,
        args_class: Some(SlashClass::Turn),
        stall_secs: 20,
        cap_secs: 60,
        default_enabled: true,
        evidence: "rf1eb364a42",
    },
];

/// cursor 若将来跑 tmux:只沿用**既有**的受控压缩通道,不借这次开新命令(spec 第七节)。
static CURSOR_POOL: &[SlashEntry] = &[SlashEntry {
    name: "/compress",
    class: SlashClass::Slow,
    args: ArgsPolicy::ExactOnly,
    args_class: None,
    stall_secs: 240,
    cap_secs: 600,
    default_enabled: true,
    evidence: "r688dfeddbe",
}];

/// 被明确拒绝的命令,以及拒它的**理由**。理由要留着 —— 否则下一个人只会看到一句通用的「被拒」,
/// 于是把它当成漏了、顺手放开。
///
/// 准入反转之后这张表的角色变了:它不再是「池外样本」的注解,而是**出厂黑名单条目 + 生命周期
/// 变体**的用户可见文案。出厂黑名单的**清单**在 `messaging.default.yaml`(可配、可移除),
/// **理由**留在这里(那是给人看的说明,不是配置)—— 用户把某条从黑名单里删掉之后,这段文案
/// 自然也就不再出现,两边不会互相矛盾。
static REJECTED_SAMPLES: &[(&str, &str)] = &[
    ("/login", "r626d40a0ba"),
    ("/logout", "r1a2f8a9ff9"),
    ("/config", "rfb6c00c2e7"),
    ("/model", "rb0e4689ae8"),
    ("/effort", "rac913ae73b"),
    ("/exit", "rf735fb6f43"),
    ("/quit", "r1e05dcb975"),
    ("/reset", "rba50f7fe65"),
    ("/clear", "rbb6be77df2"),
];

/// **slash 准入黑名单**(owner 2026-08-14 决策:准入从白名单反转为可配置黑名单)。
///
/// 生效集 = `default` 段 **∪** 该 backend 段。纯并集,没有别的合成规则 ——
/// 「claude 拦 `/login`、其它后端放行」就是把 `/login` **只**写进 `claude` 段而不写 `default`;
/// 反过来 `default` 段对所有 backend 生效。段之间不做减法、不做优先级覆盖:黑名单只会变长,
/// 这样「加一条 badcase」永远不会意外放开另一条。
///
/// **出厂预置项来自 `messaging.default.yaml`,不硬编码在这里** —— owner 显式要求它们可配:
/// 用户配置整段替换,写 `default: []` 就是把出厂项全部移除,那是 owner 的权利。
/// 理由文案仍留在 [`REJECTED_SAMPLES`](说明归说明,清单归配置)。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SlashBlacklist {
    /// 对**所有** backend 生效的段。
    pub default: Vec<String>,
    pub claude: Vec<String>,
    pub codex: Vec<String>,
    pub cursor: Vec<String>,
}

impl SlashBlacklist {
    /// 该 backend 的**生效集**(`default` ∪ backend 段)。
    ///
    /// 用 `match` 而不是字符串查表:新增一个 [`CliDriver`] 变体时编译器在这里逼着表态,
    /// 而不是静默落进「没有这一段 = 什么都不拦」。
    pub fn effective_for(&self, cli: CliDriver) -> impl Iterator<Item = &str> + '_ {
        let bucket: &[String] = match cli {
            CliDriver::Claude => &self.claude,
            // traex 是 codex fork,slash 命令面同构,黑名单复用 codex 段。
            CliDriver::Codex | CliDriver::Traex => &self.codex,
            CliDriver::Cursor => &self.cursor,
            // 未知后端只吃 default 段:给它一个专属段等于凭空发明一个 backend 名。
            CliDriver::Other => &[],
        };
        self.default.iter().chain(bucket).map(String::as_str)
    }

    /// 整行的首 token 在该 backend 下是否被拦。
    ///
    /// 按**同形变体**判(见 [`command_lookalike`]):输入的首 token 去掉首尾非字母数字,黑名单条目
    /// 只去掉前导 `/`,两者大小写不敏感相等即拦。所以条目要写成命令本名(`/login`),写成带标点的
    /// `/login!` 匹配不到任何输入。这是 fail-closed 方向:黑名单收的是「已确认会出事的命令」,而
    /// `/Login`、`/login!` 与 `/login` 在 TUI 里未必是两条命令 —— 窄判的代价是整张黑名单被一个
    /// 大写字母或一个标点绕过,宽判的代价只是拦掉一条变体。
    pub fn blocks(&self, cli: CliDriver, head: &str) -> bool {
        self.effective_for(cli)
            .any(|name| command_lookalike(head, name))
    }
}

/// 黑名单的**逐段覆盖**(全局 config.yaml / agent.yaml 的 `messaging.slash_blacklist`)。
///
/// **按段整体替换,不做并集** —— owner 2026-08-14 的显式要求是「用户配置可以移除出厂项」。
/// 逐项 merge 会让出厂项永远删不掉,那就把「可配」偷换成了「只能加不能减」。
/// 没写的段保持出厂值:只想加一条 claude 规则的人不会顺手把 default 段清空。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SlashBlacklistOverride {
    pub default: Option<Vec<String>>,
    pub claude: Option<Vec<String>>,
    pub codex: Option<Vec<String>>,
    pub cursor: Option<Vec<String>>,
}

impl SlashBlacklistOverride {
    /// 逐段套到已 resolved 的黑名单上(`None` 段保持原值)。
    pub fn apply(&self, b: &mut SlashBlacklist) {
        if let Some(v) = &self.default {
            b.default = v.clone();
        }
        if let Some(v) = &self.claude {
            b.claude = v.clone();
        }
        if let Some(v) = &self.codex {
            b.codex = v.clone();
        }
        if let Some(v) = &self.cursor {
            b.cursor = v.clone();
        }
    }
}

/// 生命周期命令通过受控路径执行，文本不注入 provider。
/// `/clear` 走确认卡。它们是红线「会话中断只有五条合法途径」的文本入口。
const LIFECYCLE_COMMANDS: &[&str] = &[
    "/quit", "/exit", "/reset", "/clear", "/archive", "/arc", "/resume",
];

/// `/thinking` 是禁止注入的保留命令名，同形变体不能掉进通用执行器。
const THINKING_COMMAND: &str = "/thinking";
/// `/cot` —— 过程外显设置的历史命名空间。保留为 `/set cot.*` 的别名:它已经写进用户肌肉记忆,
/// 而改掉一条已在用的命令名换不来任何东西。
const COT_COMMAND: &str = "/cot";
/// `/stop` —— 打断当前轮。飞书原生 CoT 消息的停止键发的就是这条文本(它不产生卡片回调),
/// 所以它是**原生 CoT 形态下唯一的打断入口**,必须对每个 backend 都收口成受控打断。
const STOP_COMMAND: &str = "/stop";

/// `/new <内容>` —— 丢掉旧上下文,用这句话起一个新会话。summon 自己处理,不转发给 agent:
/// 按 `/reset` 同一段代码重置,再把内容作为新会话的第一条消息。exact 无参 `/new` 等同 `/reset`。
const NEW_COMMAND: &str = "/new";

/// `/new <内容>` 的内容;不是这个形态(含 exact 无参 `/new`、同形变体)返回 `None`。
pub fn new_session_text(text: &str) -> Option<&str> {
    let text = text.trim();
    let rest = text.strip_prefix(NEW_COMMAND)?;
    if !rest.starts_with(char::is_whitespace) {
        return None;
    }
    let rest = rest.trim();
    (!rest.is_empty()).then_some(rest)
}

/// `/queue <内容>` —— 这条不插进正在跑的那一轮,排在它后面单独跑。summon 自己处理:dispatcher
/// 把它改写成以 `<内容>` 起头的普通对话,只是不参与轮内 steer;没有在跑的轮时就是一条普通消息。
const QUEUE_COMMAND: &str = "/queue";

/// `/queue <内容>` 的内容;不是这个形态(含 exact 无参 `/queue`、同形变体)返回 `None`。
pub fn queued_text(text: &str) -> Option<&str> {
    let rest = text.trim().strip_prefix(QUEUE_COMMAND)?;
    if !rest.starts_with(char::is_whitespace) {
        return None;
    }
    let rest = rest.trim();
    (!rest.is_empty()).then_some(rest)
}

/// `/set` —— 全部分级开关的统一入口。候选 key 与用法都从 `settings::SETTINGS` 派生,
/// 加一个开关不需要在这里改任何代码。
const SET_COMMAND: &str = "/set";

/// `/fork` —— 把本会话的上下文副本接到另一个群。带参(要指定目标),所以形态与生命周期那几条
/// exact 无参命令不同,但收口的理由一样:落进通用执行器就会被裸注入进 TUI,claude 会把它当
/// 一句不认识的命令,codex 会把它当 steer 素材喂给模型。
const FORK_COMMAND: &str = "/fork";

/// `/term` —— 私聊触发者一张浏览器终端卡。不注入按键、不作用于会话;收口理由同 `/fork`:
/// 落进通用执行器会被裸注入进 TUI。
const TERM_COMMAND: &str = "/term";

/// `/dash` —— 私聊触发者一张控制台卡。不注入按键、不作用于会话;收口理由同 `/term`。
const DASH_COMMAND: &str = "/dash";

/// `/summon-export` —— 私聊触发者本会话的诊断包,`/summon-export full` 另附对话内容。不注入按键、不作用于
/// 会话;收口理由同 `/term`。
const EXPORT_COMMAND: &str = "/summon-export";

/// head 是否是某条受控命令名的**同形变体**(去掉首尾非字母数字后按小写相等)。
///
/// 为什么要比 exact 宽 —— 这是准入反转**新增**的一道闸:反转之前池外的一切都被拒,
/// 大写/带标点的形态落在「拒绝」那一侧是免费的;反转之后它们会掉进通用执行器被**裸注入进
/// TUI**。宽拒的代价只是没法有一条字面叫那个大写形态的自定义命令,窄拒的代价是受控命令被
/// 一个大写字母或一个标点绕开。
fn command_lookalike(head: &str, name: &str) -> bool {
    let core = head.trim_matches(|c: char| !c.is_ascii_alphanumeric());
    core.eq_ignore_ascii_case(name.trim_start_matches('/'))
}

fn lifecycle_lookalike(head: &str) -> bool {
    LIFECYCLE_COMMANDS
        .iter()
        .any(|name| command_lookalike(head, name))
}

fn thinking_lookalike(head: &str) -> bool {
    command_lookalike(head, THINKING_COMMAND)
}

fn cot_lookalike(head: &str) -> bool {
    command_lookalike(head, COT_COMMAND)
}

fn set_lookalike(head: &str) -> bool {
    command_lookalike(head, SET_COMMAND)
}

fn fork_lookalike(head: &str) -> bool {
    command_lookalike(head, FORK_COMMAND)
}

/// 配置层能对采样池做的**全部**事情:启停池内条目(准入反转后保留兼容,语义一字未改)。
///
/// **它管的是「优化层」而不是「准入门」**:两张名单里认不出的名字一律被忽略,所以「往这里写一个
/// 池外命令名」既不会打开它、也不会关掉它 —— 池外命令的开关在 [`SlashBlacklist`] 那边。
/// 显式 `disabled` 一条池内命令 = 拒绝执行它(不是降级到通用路):那是一句「别执行它」,
/// 改判成「用没调优过的判据执行」是背着用户放宽。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SlashToggles {
    /// 显式打开(只对池内 `default_enabled: false` 的条目有意义)。
    pub enabled: Vec<String>,
    /// 显式关闭。**关的优先级高于开** —— 两张名单同时命中时按关处理(fail-closed)。
    pub disabled: Vec<String>,
}

impl SlashToggles {
    fn allows(&self, entry: &SlashEntry) -> bool {
        if self.disabled.iter().any(|d| d == entry.name) {
            return false;
        }
        entry.default_enabled || self.enabled.iter().any(|e| e == entry.name)
    }
}

/// 生命周期类命令的**受控映射**。裸透传它们等于从飞书文本直接触发会话拆除,
/// 所以文本只产生「意图」,真正动手的仍是既有白名单路径。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecycleIntent {
    /// exact `/quit` **或 exact `/exit`** → 一步生效的受控拆除(保留自动续聊入口)。
    ///
    /// **这是 owner 2026-08-13 显式授权的第五条会话中断合法途径**(裁决 spec 第十节第 1 问:
    /// 要的就是「一句 /quit 直接生效」,不走确认卡)。授权改的是「要不要动手」,没改「谁动手」:
    /// 这里只产出意图,拆除仍由白名单内的 `teardown.rs` 执行。
    ///
    /// **`/exit` 是 owner 2026-08-14 对这条授权的显式扩展**:此前冻结的原文只认 exact `/quit`,
    /// `/exit` 被当成「不自行扩大授权」挡在外面;现在别名由 owner 亲自扩入,两条走**完全相同**
    /// 的受控路径(`Control::Clean`、一步生效、不确认、保留 resume)。带参形态照旧不算。
    Quit,
    /// 归档当前会话；普通消息新建，`/resume` 可恢复。
    Archive,
    /// 恢复本 chat 最近结束的原生会话。
    Resume,
    /// exact `/reset` → 一步生效的受控重置(清 resume 记录 = 下次全新会话)。
    ///
    /// **它是 `/session reset` 的 exact 单词别名,不是新授权**:那条文本命令本来就一步生效、
    /// 过同一道 operate-gate、投同一个 `Control::Reset`。owner 2026-09-01 要一个单词形态,
    /// 因为这是他最常用的一条。走 `plan_slash` 而不是 dispatcher 的 `/session` 分支,是为了
    /// 拿到同形变体收口 —— 否则 `/RESET` `/reset!` 会掉进通用执行器被裸注入进 TUI。
    Reset,
    /// exact `/clear` → 确认卡。它轮换原生 session id,绕过 reset 会让 resume/mirror 继续盯旧
    /// 文件,**不在第五条授权之列**,仍走「卡片确认」这条既有合法途径。
    Clear,
}

/// app-server RPC 控制面中的目标操作。把参数解析留在 core，dispatcher 与 worker 共用同一份
/// 结论；否则一边 claim、一边按另一种参数语义执行，Bridge 所有权会再次错位。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RpcGoalCommand {
    Get,
    Set(String),
    Clear,
}

/// 可以从飞书安全映射到 Codex 家族 app-server 的控制指令。
///
/// 这不是 app-server 全量方法表：文件写入、账号、插件安装、thread 删除等方法虽存在，
/// 但不等于应该暴露成聊天命令。这里只收三类：summon 本地生命周期、只读查询、以及作用域
/// 严格限定在当前 thread 且不需要额外交互确认的状态变更。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RpcSlashCommand {
    Help,
    Compact,
    Status,
    Context,
    Rename(String),
    Goal(RpcGoalCommand),
    Mcp {
        verbose: bool,
    },
    Skills,
    Hooks,
    Plugins,
    Experimental,
    Processes,
    /// 停止当前 thread 的后台终端。**当前没有文本入口** —— `/stop` 那个名字归打断当前轮
    /// (原生 CoT 停止键发的就是它),这里保留 app-server 侧的能力映射。
    StopBackground,
}

impl RpcSlashCommand {
    /// 成功结果只是复述这条指令(改名、设定或清掉目标),回执是纯确认。
    pub fn result_only_confirms(&self) -> bool {
        matches!(
            self,
            RpcSlashCommand::Rename(_)
                | RpcSlashCommand::Goal(RpcGoalCommand::Set(_) | RpcGoalCommand::Clear)
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlashHelpEntry {
    pub head: &'static str,
    pub usage: &'static str,
    pub description: &'static str,
}

const CONTROL_HELP: &[SlashHelpEntry] = &[
    SlashHelpEntry {
        head: "/quit",
        usage: "/quit",
        description: "r34e8d20eff",
    },
    SlashHelpEntry {
        head: "/exit",
        usage: "/exit",
        description: "r34e8d20eff",
    },
    SlashHelpEntry {
        head: "/archive",
        usage: "/archive",
        description: "r4d52d1b879",
    },
    SlashHelpEntry {
        head: "/arc",
        usage: "/arc",
        description: "r4d52d1b879",
    },
    SlashHelpEntry {
        head: "/resume",
        usage: "/resume",
        description: "rab84d15aa1",
    },
    SlashHelpEntry {
        head: "/reset",
        usage: "/reset",
        description: "r4d52d1b879",
    },
    SlashHelpEntry {
        head: "/new",
        usage: "rd6499229ea",
        description: "r86b3f76e83",
    },
    SlashHelpEntry {
        head: "/handoff",
        usage: "r9992719a20",
        description: "r5b7af16958",
    },
    SlashHelpEntry {
        head: "/stop",
        usage: "/stop",
        description: "ra7d8113836",
    },
    SlashHelpEntry {
        head: "/queue",
        usage: "r99b6159318",
        description: "r1aa1fe4033",
    },
    SlashHelpEntry {
        head: "/fork",
        usage: "r7259e9d113",
        description: "rb40a9f1b32",
    },
    SlashHelpEntry {
        head: "/delegate",
        usage: "/delegate auto [on|off]",
        description: "rd1bc8b26a4",
    },
    SlashHelpEntry {
        head: "/term",
        usage: "/term",
        description: "r40875d6a7b",
    },
    SlashHelpEntry {
        head: "/dash",
        usage: "/dash",
        description: "r4a7be74cb9",
    },
    SlashHelpEntry {
        head: "/summon-export",
        usage: "/summon-export [full]",
        description: "r6db9556ef4",
    },
    SlashHelpEntry {
        head: "/clear",
        usage: "/clear",
        description: "r1e43082ab9",
    },
    SlashHelpEntry {
        head: "/cot",
        usage: "/cot on|off",
        description: "r2051a0903f",
    },
    SlashHelpEntry {
        head: "/cot",
        usage: "/cot thinking on|off",
        description: "r987dec3d3b",
    },
    SlashHelpEntry {
        head: "/cot",
        usage: "/cot tools on|off",
        description: "r3c0aee1a3e",
    },
];

const RPC_HELP: &[SlashHelpEntry] = &[
    SlashHelpEntry {
        head: "/compact",
        usage: "/compact",
        description: "rf8c07b7a73",
    },
    SlashHelpEntry {
        head: "/status",
        usage: "/status",
        description: "rcf15a16f1f",
    },
    SlashHelpEntry {
        head: "/context",
        usage: "/context",
        description: "r4570937300",
    },
    SlashHelpEntry {
        head: "/rename",
        usage: "r7c5821bd5b",
        description: "r1394c18425",
    },
    SlashHelpEntry {
        head: "/goal",
        usage: "r5855ad05dc",
        description: "r872582463c",
    },
    SlashHelpEntry {
        head: "/mcp",
        usage: "/mcp [verbose]",
        description: "r6778f388c5",
    },
    SlashHelpEntry {
        head: "/skills",
        usage: "/skills",
        description: "re5a6f7ef76",
    },
    SlashHelpEntry {
        head: "/hooks",
        usage: "/hooks",
        description: "r324471cd28",
    },
    SlashHelpEntry {
        head: "/plugins",
        usage: "/plugins",
        description: "r87f1982b9b",
    },
    SlashHelpEntry {
        head: "/experimental",
        usage: "/experimental",
        description: "r323e7b599a",
    },
    SlashHelpEntry {
        head: "/ps",
        usage: "/ps",
        description: "r5cec90097f",
    },
];

fn transcript_help_entry(entry: &SlashEntry) -> SlashHelpEntry {
    let usage = match entry.name {
        "/compact" => summon_i18n::text!("r0dd0b4f87e"),
        "/context" => "/context [all]",
        "/status" => summon_i18n::text!("r1414d82835"),
        "/cost" => summon_i18n::text!("r5d5628c7c5"),
        "/goal" => summon_i18n::text!("r8820d9bb7b"),
        other => other,
    };
    let description = match entry.name {
        "/compact" => summon_i18n::text!("racf3a91e67"),
        "/context" => summon_i18n::text!("r77e39889a8"),
        "/status" => summon_i18n::text!("rd14fd0831c"),
        "/cost" => summon_i18n::text!("rde56c3d4cd"),
        "/goal" => summon_i18n::text!("rb5ea7e2147"),
        "/compress" => summon_i18n::text!("racf3a91e67"),
        _ => summon_i18n::text!("r676b216f87"),
    };
    SlashHelpEntry {
        head: entry.name,
        usage,
        description,
    }
}

pub fn summon_control_help_entries(
    cli: CliDriver,
    blacklist: &SlashBlacklist,
) -> Vec<SlashHelpEntry> {
    CONTROL_HELP
        .iter()
        .copied()
        .filter(|entry| !blacklist.blocks(cli, entry.head))
        .collect()
}

pub fn backend_help_entries(
    cli: CliDriver,
    mode: CaptureMode,
    blacklist: &SlashBlacklist,
    toggles: &SlashToggles,
) -> Vec<SlashHelpEntry> {
    let mut entries = Vec::new();
    if mode == CaptureMode::Rpc && matches!(cli, CliDriver::Codex | CliDriver::Traex) {
        entries.extend(
            RPC_HELP
                .iter()
                .copied()
                .filter(|entry| !blacklist.blocks(cli, entry.head)),
        );
    } else if has_slash_channel(cli, mode) {
        entries.extend(
            pool_for(cli, mode)
                .iter()
                .filter(|entry| toggles.allows(entry) && !blacklist.blocks(cli, entry.name))
                .map(transcript_help_entry),
        );
    }
    entries
}

/// 当前 CLI × 捕获通道真正承认的已知命令。黑名单与池内开关在这里和 [`plan_slash`] 用同一判据。
pub fn slash_help_entries(
    cli: CliDriver,
    mode: CaptureMode,
    blacklist: &SlashBlacklist,
    toggles: &SlashToggles,
) -> Vec<SlashHelpEntry> {
    let mut entries = summon_control_help_entries(cli, blacklist);
    entries.extend(backend_help_entries(cli, mode, blacklist, toggles));
    entries
}

pub fn render_help_items(entries: &[SlashHelpEntry]) -> String {
    let mut text = String::new();
    for entry in entries {
        text.push_str(&format!(
            "\n\n- `{}`：{}",
            crate::i18n::label(entry.usage),
            crate::i18n::label(entry.description)
        ));
    }
    text
}

/// 管理帮助按操作合并可用别名；黑名单过滤后的命令目录决定每行列出的入口。
pub fn render_control_help_items(entries: &[SlashHelpEntry]) -> String {
    let mut text = String::new();
    for (index, entry) in entries.iter().enumerate() {
        let group: &[&str] = match entry.head {
            "/quit" | "/exit" => &["/quit", "/exit"],
            "/reset" | "/archive" | "/arc" => &["/reset", "/archive", "/arc"],
            _ => {
                text.push_str(&render_help_items(&[*entry]));
                continue;
            }
        };
        if entries[..index]
            .iter()
            .any(|prior| group.contains(&prior.head))
        {
            continue;
        }
        let usages = group
            .iter()
            .filter_map(|head| entries.iter().find(|alias| alias.head == *head))
            .map(|alias| format!("`{}`", crate::i18n::label(alias.usage)))
            .collect::<Vec<_>>()
            .join(" · ");
        text.push_str(&format!(
            "\n\n- {usages}：{}",
            crate::i18n::label(entry.description)
        ));
    }
    text
}

pub fn backend_help_markdown(
    cli: CliDriver,
    mode: CaptureMode,
    blacklist: &SlashBlacklist,
    toggles: &SlashToggles,
) -> String {
    render_help_items(&backend_help_entries(cli, mode, blacklist, toggles))
}

/// 命令清单末尾那句注脚的**原文**,不带任何标签。
///
/// 与清单分开返回,是因为两者的归宿不同:清单是正文,注脚要渲成灰字。灰字模板 `<font>` 只能由
/// 卡片层套在自己的元素上 —— 拼进这段纯文本里,整段会被出站通道当外部文本消毒,模板本身以
/// 明文出现在用户眼前(2026-08-28 `/help` 卡实测)。渲不出灰字的通道原样追加即可,信息不丢。
pub fn backend_help_note(
    cli: CliDriver,
    mode: CaptureMode,
    blacklist: &SlashBlacklist,
    toggles: &SlashToggles,
) -> Option<&'static str> {
    if has_slash_channel(cli, mode) {
        Some(summon_i18n::text!("rb0904b2c3c"))
    } else if backend_help_entries(cli, mode, blacklist, toggles).is_empty() {
        Some(summon_i18n::text!("r492a3ae1b4"))
    } else {
        None
    }
}

/// 为什么拒绝。每一条都要能翻译成一句用户看得懂的回执 —— 拒绝必须外显,不许静默丢。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlashRefusal {
    /// 命中 [`SlashBlacklist`] 的生效集(`default` ∪ 该 backend 段)。带参形态一并拦
    /// (首 token 命中即拦):黑名单收的是「这条命令本身不该从飞书跑」。
    Denylisted,
    /// 这条**形态**不可执行。准入反转后只剩三个来源:① 生命周期命令的变体/带参形态
    /// (`/quit now` `/QUIT`——绝不能掉进通用路);② 池内条目被配置显式关停;
    /// ③ 首 token 不是 `/` 开头(根本不是一条命令)。名字保留是为了不动既有红线断言。
    NotInPool,
    /// 命令在池里,但这条池内条目没声明可以带参(`ArgsPolicy::ExactOnly`)。
    ArgsNotDeclared,
    /// 命令在池里且已开放带参,但参数里有换行/控制字符 —— 终端会把它们当按键解释
    /// (换行会触发多行粘贴的补 Enter 路径,补的 Enter 打在面板上;Tab 是补全键),
    /// 整行原文透传只收**单行可打印**形态。
    ArgsNotInjectable,
    /// 这个 CLI / 捕获模式根本没有交互式命令通道。
    NoChannel,
    /// app-server 不消费 TUI slash 文本，且 summon 没有为这条命令声明原生 RPC 映射。
    NoRpcMapping,
    /// 命令有原生 RPC 映射，但本条参数形状不在冻结的安全契约内。
    InvalidRpcArgs { usage: &'static str },
    /// 命令参数形状不在冻结的安全契约内(与 RPC 路径无关的通用形态,如 `/thinking foo`)。
    InvalidArgs { usage: &'static str },
}

/// 一条飞书文本落到透传路径上的**全部可能结局**。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlashPlan {
    /// 白名单透传,按 entry 的类别收尾。`line` 是**注入计划里的整行原文**(trim 外围空白后):
    /// `ExactOnly` 条目恒等于 `entry.name`;`Passthrough` 条目保留参数原文,零 marker。
    Passthrough {
        entry: &'static SlashEntry,
        line: String,
    },
    /// **未知命令**(不在黑名单、不在采样池):默认放行,走通用执行器。
    ///
    /// 与 [`Passthrough`](SlashPlan::Passthrough) 的唯一区别是**收尾类别事前未知** ——
    /// 注入依旧过同一道输入态闸、依旧整行原文零 marker、依旧 at-most-once,只是「跑完了没有、
    /// 怎么取回显」改由注入后的观测归类(见 [`classify_generic`]),判不出就诚实回「不确定」。
    Generic {
        line: String,
    },
    /// 生命周期命令的受控映射(不注入任何按键)。
    Lifecycle(LifecycleIntent),
    /// 打断当前这一轮(不拆会话、不注入任何按键)。**全 backend 通用** —— 飞书原生 CoT 消息上的
    /// 停止键不产生卡片回调,它发的是一条文本 `/stop`,所以这条文本必须等价于控制卡的「⏸ 打断」。
    Interrupt,
    /// 分级开关的受控映射(`/set` 与其别名 `/cot`)。不触发轮、不注入任何按键。
    Setting(SettingAction),
    /// 跨群 fork:把本会话的上下文副本接到目标群。不触发轮、不注入任何按键 —— 它只是发起一次
    /// 需要目标群授权的请求,真正落盘要等目标群 operator 点确认。
    ///
    /// `target` 是用户给的原文(群名片段或 chat id),**这里不做解析** —— 解析要读盘上的 `.meta`,
    /// 那是 IO,不属于纯判据层。歧义命中多个群时由解析侧报错列候选,绝不猜。
    Fork {
        target: String,
    },
    /// Owner 对本群自主任务分流的持久授权。
    DelegateAuto {
        enabled: Option<bool>,
    },
    /// 私聊触发者一张浏览器终端卡(只读 / 可操作两条一次性链接)。不触发轮、不注入任何按键;
    /// 无头 backend 上由处理侧回复「没有终端」。
    Terminal,
    /// 私聊触发者一张控制台卡。不触发轮、不注入任何按键,与 backend 无关。
    Dashboard,
    /// 导出本会话的排查材料,只私聊触发者:默认是不含对话内容的诊断包,`full` 另附对话内容。
    /// 不触发轮、不注入任何按键,与 backend 无关。
    Export {
        full: bool,
    },
    /// `/new <内容>`:重置会话后以 `text` 起新会话。准入通过时 dispatcher 已把它改写成一条
    /// 要求新上下文的普通对话,走到 worker 的只会是未获准的那条。
    NewSession {
        text: String,
    },
    /// `/handoff [@目标] [备注]`:写交接文件后交给目标 agent 或本 agent 的新会话。准入通过时
    /// dispatcher 已把它改写成写交接的普通对话,走到 worker 的只会是未获准的那条。
    Handoff,
    /// app-server 原生控制方法。与 TUI 透传互斥：这里的文本不会进入 `turn/start`。
    Rpc(RpcSlashCommand),
    Refuse(SlashRefusal),
}

/// `/set` 的两种意图。列出候选是必要的一半 —— key 是点分命名空间,不给列表用户只能靠记。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingAction {
    /// `/set` 无参:列出本会话可设的开关与各自当前值。
    List,
    /// 设为显式值,或 `inherit` 恢复继承上一层。
    Change {
        key: &'static str,
        value: Option<bool>,
    },
}

impl SlashPlan {
    /// 这条命令会不会真正碰到本地会话。dispatcher 据此决定是否 claim —— 被拒绝的命令**不能**
    /// 把 chat 永久标成「本地已占用」(那会挡住 Bridge 接管)。
    ///
    /// `Clear` 只发一张卡,不碰会话;真正的拆除发生在用户点按钮之后,那条路径自己会 claim。
    /// `Generic` 会真注入,所以和 `Passthrough` 同判 —— dispatcher 不 claim 而 worker 注入,
    /// Bridge 就能在空窗内接管同一条会话。
    pub fn touches_local_session(&self) -> bool {
        matches!(
            self,
            SlashPlan::Passthrough { .. }
                | SlashPlan::Generic { .. }
                | SlashPlan::Lifecycle(
                    LifecycleIntent::Quit
                        | LifecycleIntent::Reset
                        | LifecycleIntent::Archive
                        | LifecycleIntent::Resume
                )
                | SlashPlan::NewSession { .. }
                | SlashPlan::Handoff
                | SlashPlan::Interrupt
                | SlashPlan::Rpc(
                    RpcSlashCommand::Compact
                        | RpcSlashCommand::Status
                        | RpcSlashCommand::Context
                        | RpcSlashCommand::Rename(_)
                        | RpcSlashCommand::Goal(_)
                        | RpcSlashCommand::Mcp { .. }
                        | RpcSlashCommand::Skills
                        | RpcSlashCommand::Hooks
                        | RpcSlashCommand::Plugins
                        | RpcSlashCommand::Experimental
                        | RpcSlashCommand::Processes
                        | RpcSlashCommand::StopBackground
                )
        )
    }
}

fn invalid_rpc_args(usage: &'static str) -> SlashPlan {
    SlashPlan::Refuse(SlashRefusal::InvalidRpcArgs { usage })
}

/// Traex 0.201.2 / Codex 0.146.0 TUI 的原生命令名快照，加上 summon 自己处理的命令(`/quit` 别名、`/queue` 等)。
/// dispatcher 只把这些名字送进控制规划器；其余 `/skill-name` 仍保留 Codex 家族既有的
/// `$skill-name` provider-prompt 语义。这样补控制面不会顺手切断动态 skill/plugin 命令。
const CODEX_FAMILY_TUI_COMMANDS: &[&str] = &[
    "/model",
    "/permissions",
    "/keymap",
    "/experimental",
    "/remote-control",
    "/tui",
    "/approve",
    "/memories",
    "/skills",
    "/review",
    "/rename",
    "/new",
    "/dashboard",
    "/peers",
    "/resume",
    "/fork",
    "/queue",
    "/handoff",
    "/archive",
    "/delete",
    "/init",
    "/compact",
    "/plan",
    "/goal",
    "/loop",
    "/collab",
    "/btw",
    "/side",
    "/rewind",
    "/undo-rewind",
    "/copy",
    "/raw",
    "/diff",
    "/mention",
    "/doctor",
    "/status",
    "/title",
    "/statusline",
    "/color",
    "/theme",
    "/mcp",
    "/plugins",
    "/logout",
    "/exit",
    "/quit",
    "/feedback",
    "/ps",
    "/stop",
    "/clear",
    "/subagents",
    "/help",
    "/context",
    "/memory",
    "/agents",
    "/agent-new",
    "/recap",
    "/vim",
    "/hooks",
    "/ide",
    "/add-dir",
    "/cost",
    "/trace",
    "/share",
    "/insights",
    "/terminal-setup",
    "/workflows",
    // summon 受控命令(非 TUI 原生,但必须进规划器,不能退成 provider prompt 发给模型)。
    "/thinking",
    // 旧版/黑名单仍认识的名字也必须走控制拒绝，不能因菜单下线而退成模型 prompt。
    "/login",
    "/config",
];

pub fn is_codex_family_tui_command(text: &str) -> bool {
    let head = text.split_whitespace().next().unwrap_or_default();
    lifecycle_lookalike(head)
        || thinking_lookalike(head)
        || cot_lookalike(head)
        || set_lookalike(head)
        || fork_lookalike(head)
        || command_lookalike(head, "/delegate")
        || command_lookalike(head, TERM_COMMAND)
        || command_lookalike(head, DASH_COMMAND)
        || command_lookalike(head, EXPORT_COMMAND)
        || command_lookalike(head, QUEUE_COMMAND)
        || command_lookalike(head, HANDOFF_COMMAND)
        || CODEX_FAMILY_TUI_COMMANDS
            .iter()
            .any(|command| command.eq_ignore_ascii_case(head))
}

/// Codex/Traex app-server 的 slash-facing 窄端口。RPC 协议接受方法帧，不解析 TUI 的 `/…`
/// 文本；因此未声明的命令必须在这里拒绝，不能降级成模型 prompt。
fn plan_rpc_slash(text: &str, head: &str, has_args: bool) -> SlashPlan {
    if text.chars().any(char::is_control) {
        return invalid_rpc_args(summon_i18n::text!("ra8cbde0833"));
    }
    let args = text.get(head.len()..).unwrap_or_default().trim();
    if head != "/help" && !RPC_HELP.iter().any(|entry| entry.head == head) {
        return SlashPlan::Refuse(SlashRefusal::NoRpcMapping);
    }
    let exact = |command| {
        if has_args {
            invalid_rpc_args(summon_i18n::text!("r54cbd476d7"))
        } else {
            SlashPlan::Rpc(command)
        }
    };
    match head {
        "/help" => exact(RpcSlashCommand::Help),
        "/compact" => exact(RpcSlashCommand::Compact),
        "/status" => exact(RpcSlashCommand::Status),
        "/context" => exact(RpcSlashCommand::Context),
        "/rename" if args.is_empty() => invalid_rpc_args(summon_i18n::text!("rb54a414603")),
        "/rename" if args.chars().count() > 100 => {
            invalid_rpc_args(summon_i18n::text!("r70b6103609"))
        }
        "/rename" => SlashPlan::Rpc(RpcSlashCommand::Rename(args.to_string())),
        "/goal" if args.is_empty() => SlashPlan::Rpc(RpcSlashCommand::Goal(RpcGoalCommand::Get)),
        "/goal" if args == "clear" => SlashPlan::Rpc(RpcSlashCommand::Goal(RpcGoalCommand::Clear)),
        "/goal" if args.chars().count() > 2_000 => {
            invalid_rpc_args(summon_i18n::text!("r2c26a5aa22"))
        }
        "/goal" => SlashPlan::Rpc(RpcSlashCommand::Goal(RpcGoalCommand::Set(args.to_string()))),
        "/mcp" if args.is_empty() => SlashPlan::Rpc(RpcSlashCommand::Mcp { verbose: false }),
        "/mcp" if args == "verbose" => SlashPlan::Rpc(RpcSlashCommand::Mcp { verbose: true }),
        "/mcp" => invalid_rpc_args(summon_i18n::text!("r3ee008dc7d")),
        "/skills" => exact(RpcSlashCommand::Skills),
        "/hooks" => exact(RpcSlashCommand::Hooks),
        "/plugins" => exact(RpcSlashCommand::Plugins),
        "/experimental" => exact(RpcSlashCommand::Experimental),
        "/ps" => exact(RpcSlashCommand::Processes),
        _ => SlashPlan::Refuse(SlashRefusal::NoRpcMapping),
    }
}

/// 该 CLI × 捕获模式下**有没有**可注入的交互式命令通道。
///
/// 准入反转之后这条判据必须独立于采样池:池空不再等于「没有通路」(未知命令走通用执行器)。
/// 通路的真实条件是「有一个我们能抓屏、能 send-keys 的 TUI」——
/// - Transcript = tmux 里的真 TUI(claude 今天在跑;cursor 若显式配 Transcript 也算);
/// - codex 从不跑 tmux(Rpc/Subprocess 常驻进程),即使配置里写了 Transcript 也没有 TUI 可注入;
/// - Subprocess/Rpc 一律没有交互命令通道,照旧诚实拒绝。
pub fn has_slash_channel(cli: CliDriver, mode: CaptureMode) -> bool {
    mode == CaptureMode::Transcript && matches!(cli, CliDriver::Claude | CliDriver::Cursor)
}

/// 该 CLI × 捕获模式下的采样池(未过滤配置)。
///
/// 准入反转后**池空不再等于「没有通路」**:通路由 [`has_slash_channel`] 单独判,池只回答
/// 「这条命令有没有调优过的收尾语义」。
pub fn pool_for(cli: CliDriver, mode: CaptureMode) -> &'static [SlashEntry] {
    match (cli, mode) {
        (CliDriver::Claude, CaptureMode::Transcript) => CLAUDE_POOL,
        (CliDriver::Cursor, CaptureMode::Transcript) => CURSOR_POOL,
        _ => &[],
    }
}

/// 池内某条命令被挡在外面的理由(只用于回执与文档,不参与判定)。同形变体与黑名单同样按
/// [`command_lookalike`] 认,拒绝回执里带上同一句理由。
pub fn rejection_reason(name: &str) -> Option<&'static str> {
    REJECTED_SAMPLES
        .iter()
        .find(|(n, _)| command_lookalike(name, n))
        .map(|(_, why)| crate::i18n::label(why))
}

/// 把一条飞书文本判成 [`SlashPlan`]。**顺序即优先级,不可调换**:
///
/// ① 首 token 必须 `/` 开头 —— 少了这条,通用路会把任意一句话当命令注进 TUI;
/// ② [`SlashBlacklist`] 钉死在最上面(带参也一样,首 token 命中即拦);
/// ③ 生命周期命令**及其同形变体**在这里收口:exact 无参的 `/quit` `/exit` → 一步生效的受控
///    拆除,exact 无参 `/reset` → 一步生效的受控重置,exact 无参 `/clear` → 确认卡,
///    **其余形态一律拒**(绝不落进 ⑤ 被裸注入);
/// ④ Codex 家族 RPC → 映射到原生方法，未声明的命令明确拒绝；
/// ⑤ 没有交互通道 → 拒;
/// ⑥ 池内条目走**调优执行器**(exact / `ArgsPolicy::Passthrough` 的单行带参形态),
///    判据与准入反转之前逐字节相同;
/// ⑦ 其余(未知命令)**默认放行**,走通用执行器 —— 这是 2026-08-14 反转的核心增量。
///
/// ②③ 排在 ④ 之前是有意的：`/quit` 拆除不依赖 TUI，在 RPC 下也必须先产生命周期意图；
/// dispatcher 再用 fresh operate-gate 冻结准入位，worker 只消费这个意图、不注入文本。
pub fn plan_slash(
    cli: CliDriver,
    mode: CaptureMode,
    text: &str,
    blacklist: &SlashBlacklist,
    toggles: &SlashToggles,
) -> SlashPlan {
    let text = text.trim();
    let head = text.split_whitespace().next().unwrap_or_default();
    let has_args = text != head;
    // ① 不是 `/` 开头就不是一条命令。反转之前这条由「不在池里」顺带兜住,现在必须显式写出来。
    if !head.starts_with('/') {
        return SlashPlan::Refuse(SlashRefusal::NotInPool);
    }
    // ② 黑名单:任何池、任何形态都翻不过去。
    if blacklist.blocks(cli, head) {
        return SlashPlan::Refuse(SlashRefusal::Denylisted);
    }
    // ③ 受控映射只认 exact 无参命令。带参的
    // `/quit --now`、大小写变体 `/QUIT`、带标点的 `/quit!` 都不是「那句命令」——
    // 它们必须在这里被拒,**绝不能**掉进 ⑥ 的通用执行器(裸注入一个可能被 CLI 认成退出的
    // 形态 = 绕过红线的五条合法会话中断途径)。红线里那句「带参形态不算」也在这里落地。
    if lifecycle_lookalike(head) {
        return match (has_args, head) {
            (false, "/quit" | "/exit") => SlashPlan::Lifecycle(LifecycleIntent::Quit),
            (false, "/archive" | "/arc") => SlashPlan::Lifecycle(LifecycleIntent::Archive),
            (false, "/resume") => SlashPlan::Lifecycle(LifecycleIntent::Resume),
            (false, "/reset") => SlashPlan::Lifecycle(LifecycleIntent::Reset),
            (false, "/clear") => SlashPlan::Lifecycle(LifecycleIntent::Clear),
            _ => SlashPlan::Refuse(SlashRefusal::NotInPool),
        };
    }
    // ③.2 `/new` 与生命周期命令同一套收口:exact 拼写才算,同形变体拒,绝不落进通用执行器。
    if command_lookalike(head, NEW_COMMAND) {
        return match (new_session_text(text), head) {
            (Some(rest), NEW_COMMAND) => SlashPlan::NewSession {
                text: rest.to_string(),
            },
            (None, NEW_COMMAND) if !has_args => SlashPlan::Lifecycle(LifecycleIntent::Reset),
            _ => SlashPlan::Refuse(SlashRefusal::NotInPool),
        };
    }
    // ③.3 `/handoff` 同一套收口:exact 拼写(带不带目标与备注都算)才是它,同形变体拒。
    if command_lookalike(head, HANDOFF_COMMAND) {
        return if head == HANDOFF_COMMAND {
            SlashPlan::Handoff
        } else {
            SlashPlan::Refuse(SlashRefusal::NotInPool)
        };
    }
    // ③.4 `/stop` 是**打断当前轮**,不是别的:飞书原生 CoT 的停止键就发这条文本(那条链路没有
    // 卡片回调),所以它必须与控制卡「⏸ 打断」同义,且对每个 backend 都成立 —— 落进通用执行器
    // 被注入进 TUI 的话,claude 会把它当一句不认识的命令,codex 会把它当 steer 素材喂给模型。
    // 同形变体与带参形态照生命周期那套规矩拒:停一轮是破坏性动作,只认 owner 冻结的那个拼写。
    if command_lookalike(head, STOP_COMMAND) {
        return match (has_args, head) {
            (false, STOP_COMMAND) => SlashPlan::Interrupt,
            _ => SlashPlan::Refuse(SlashRefusal::NotInPool),
        };
    }
    // ③.45 `/queue <内容>` 由 dispatcher 改写成普通对话,走到这里的只剩无参与同形变体:
    // 回用法,绝不落进通用执行器被当成一句命令注入。
    if command_lookalike(head, QUEUE_COMMAND) {
        return SlashPlan::Refuse(SlashRefusal::InvalidArgs {
            usage: summon_i18n::text!("r99b6159318"),
        });
    }
    // ③.5 `/thinking` 是禁止注入的保留命令，同形变体在这里收口。
    if thinking_lookalike(head) {
        return SlashPlan::Refuse(SlashRefusal::NotInPool);
    }
    // ③.6 分级开关的两个入口。都不注入任何按键,只产生 per-chat 状态变更意图。
    if cot_lookalike(head) {
        // `/cot` 是 `/set cot.*` 的历史别名:三个维度共用一个命名空间,无子命令时指 visible。
        let parts: Vec<_> = text.split_whitespace().collect();
        let change = match parts.as_slice() {
            [_, value] => parse_on_off(value).map(|v| ("cot.visible", v)),
            [_, "thinking", value] => parse_on_off(value).map(|v| ("cot.thinking", v)),
            [_, "tools", value] => parse_on_off(value).map(|v| ("cot.tools", v)),
            _ => None,
        };
        return change
            .map(|(key, value)| {
                SlashPlan::Setting(SettingAction::Change {
                    key,
                    value: Some(value),
                })
            })
            .unwrap_or_else(|| {
                SlashPlan::Refuse(SlashRefusal::InvalidArgs {
                    usage: "/cot on|off | /cot thinking on|off | /cot tools on|off",
                })
            });
    }
    if set_lookalike(head) {
        // 与 `/cot` `/quit` 同规矩:只有 exact 拼写走受控映射,`/SET` `/set!` 这类同形变体拒绝。
        if head != SET_COMMAND {
            return SlashPlan::Refuse(SlashRefusal::NotInPool);
        }
        let parts: Vec<_> = text.split_whitespace().collect();
        return match parts.as_slice() {
            // 无参 = 列出候选。key 是点分命名空间,不给列表等于要求用户背下来。
            [_] => SlashPlan::Setting(SettingAction::List),
            [_, key, value] => {
                // 候选集来自注册表且只认**会话级可设**的那些 —— 拒绝一个只到 agent 级的 key,
                // 好过让用户以为自己在这个群里关掉了某个其实是全局的东西。
                match crate::config::settings::chat_scoped().find(|d| d.key == *key) {
                    Some(decl) => match parse_on_off_inherit(value) {
                        Some(value) => SlashPlan::Setting(SettingAction::Change {
                            key: decl.key,
                            value,
                        }),
                        None => SlashPlan::Refuse(SlashRefusal::InvalidArgs {
                            usage: summon_i18n::text!("r1367fde4ef"),
                        }),
                    },
                    None => SlashPlan::Refuse(SlashRefusal::InvalidArgs {
                        usage: summon_i18n::text!("r1367fde4ef"),
                    }),
                }
            }
            _ => SlashPlan::Refuse(SlashRefusal::InvalidArgs {
                usage: summon_i18n::text!("r1367fde4ef"),
            }),
        };
    }
    if command_lookalike(head, "/delegate") {
        if head != "/delegate" {
            return SlashPlan::Refuse(SlashRefusal::NotInPool);
        }
        return match text[head.len()..]
            .split_whitespace()
            .collect::<Vec<_>>()
            .as_slice()
        {
            [] | ["auto"] => SlashPlan::DelegateAuto { enabled: None },
            ["auto", "on"] => SlashPlan::DelegateAuto {
                enabled: Some(true),
            },
            ["auto", "off"] => SlashPlan::DelegateAuto {
                enabled: Some(false),
            },
            _ => SlashPlan::Refuse(SlashRefusal::InvalidArgs {
                usage: "/delegate auto on|off",
            }),
        };
    }
    // ③.7 `/fork` 把本会话的上下文副本接到另一个群。**先于 ④ 判**:它与生命周期那几条同理,
    // 不依赖 TUI(不注入任何按键),在 RPC 下也必须产生同一个意图。同形变体照那套规矩拒 ——
    // 跨群带走上下文是破坏性动作,只认 exact 拼写。
    if fork_lookalike(head) {
        if head != FORK_COMMAND {
            return SlashPlan::Refuse(SlashRefusal::NotInPool);
        }
        // 目标是整条命令名之后的剩余原文(不按空白切):群名里有空格是常态,
        // 切掉再拼回来只会把「Demo 项目 Alpha」这类名字弄坏。
        let target = text[head.len()..].trim();
        // **无参 = 列候选**,而不是报用法。用户在群里看不到那份会话索引(它在 agent 的 PRIMER 里),
        // 所以「有哪些群可选」除了让他自己记住之外没有别的入口 —— 报一句用法等于让他去猜。
        return SlashPlan::Fork {
            target: target.to_string(),
        };
    }
    // ③.8 `/term` 与 `/fork` 同理先于 ④⑤ 判:codex/cursor 上也要走到受控分支回一句「没有终端」。
    // 只认 exact 无参拼写,同形变体与带参形态拒绝。
    if command_lookalike(head, TERM_COMMAND) {
        return match (has_args, head) {
            (false, TERM_COMMAND) => SlashPlan::Terminal,
            _ => SlashPlan::Refuse(SlashRefusal::NotInPool),
        };
    }
    // ③.9 `/dash` 同理:出卡与会话无关,每个 backend 都走到受控分支;只认 exact 无参拼写。
    if command_lookalike(head, DASH_COMMAND) {
        return match (has_args, head) {
            (false, DASH_COMMAND) => SlashPlan::Dashboard,
            _ => SlashPlan::Refuse(SlashRefusal::NotInPool),
        };
    }
    // ③.10 `/summon-export` 同理:导出与 backend 无关;只认 exact 的 `/summon-export` 与 `/summon-export full`。
    if command_lookalike(head, EXPORT_COMMAND) {
        if head != EXPORT_COMMAND {
            return SlashPlan::Refuse(SlashRefusal::NotInPool);
        }
        let args: Vec<&str> = text.split_whitespace().skip(1).collect();
        return match args.as_slice() {
            [] => SlashPlan::Export { full: false },
            ["full"] => SlashPlan::Export { full: true },
            _ => SlashPlan::Refuse(SlashRefusal::InvalidArgs {
                usage: summon_i18n::text!("rcaee700a58"),
            }),
        };
    }
    // ④ app-server 不解析 TUI slash 文本：安全子集逐条映射，未知命令明确拒绝，绝不把
    // `/foo` 转义成普通 prompt 交给模型“回答”。
    if mode == CaptureMode::Rpc && matches!(cli, CliDriver::Codex | CliDriver::Traex) {
        return plan_rpc_slash(text, head, has_args);
    }
    // ⑤ 没有可注入的 TUI。
    if !has_slash_channel(cli, mode) {
        return SlashPlan::Refuse(SlashRefusal::NoChannel);
    }
    // ⑥ 采样池 = 已知命令的**优化层**(调优过的时间窗与收尾语义)。这一段判据在准入反转里
    // 一字未动 —— 变的只是下面 `None` 那一支不再是「拒绝」。
    match pool_for(cli, mode).iter().find(|e| e.name == head) {
        // 启停对整条命令生效:关掉的条目,带参形态一并关掉(先于参数判定,fail-closed)。
        // **关掉 = 拒绝,不是降级走通用路** —— 显式关停是一句「别执行它」,把它改判成
        // 「用没调优过的判据执行」是背着用户放宽。
        Some(e) if !toggles.allows(e) => SlashPlan::Refuse(SlashRefusal::NotInPool),
        Some(e) if has_args && e.args == ArgsPolicy::ExactOnly => {
            SlashPlan::Refuse(SlashRefusal::ArgsNotDeclared)
        }
        // 换行/控制字符不是「参数」,是按键:换行会触发多行粘贴的补 Enter 路径(补的 Enter
        // 打在面板上),Tab 是补全键。整行原文透传只收单行可打印形态。
        Some(_) if has_args && text.chars().any(char::is_control) => {
            SlashPlan::Refuse(SlashRefusal::ArgsNotInjectable)
        }
        // `/goal clear` 的 `clear` 是参数,不是受控命令 `/clear`:必须执行而不是拒绝。
        // 但它的 stop-early 收尾未采样,不能谎称与普通 `/goal <目标>` 一样必定开真轮;
        // 走通用执行器原样注入,再从真实画面判断 print/panel/turn,避免空等假轮。
        Some(e) if has_args && needs_observed_args_class(e, text) => SlashPlan::Generic {
            line: text.to_string(),
        },
        Some(e) => SlashPlan::Passthrough {
            entry: e,
            line: text.to_string(),
        },
        // ⑦ 未知命令:默认放行。控制字符仍是按键不是参数,这条守卫对通用路同样有效
        // (且不再要求 `has_args` —— `/foo\u{7}` 这种没有空白分隔的形态也要挡住)。
        None if text.chars().any(char::is_control) => {
            SlashPlan::Refuse(SlashRefusal::ArgsNotInjectable)
        }
        None => SlashPlan::Generic {
            line: text.to_string(),
        },
    }
}

/// `on`/`off`/`inherit` → `Some(Some(true))` / `Some(Some(false))` / `Some(None)`;
/// 其它 → `None`(非法)。**两层 Option 是必需的**:少一层就无法区分「设为关」与「恢复继承」,
/// 而这两者语义完全不同 —— 前者钉死在关,后者跟随上层配置变化。
fn parse_on_off_inherit(arg: &str) -> Option<Option<bool>> {
    if arg.eq_ignore_ascii_case("inherit") {
        Some(None)
    } else {
        parse_on_off(arg).map(Some)
    }
}

fn parse_on_off(arg: &str) -> Option<bool> {
    if arg.eq_ignore_ascii_case("on") {
        Some(true)
    } else if arg.eq_ignore_ascii_case("off") {
        Some(false)
    } else {
        None
    }
}

/// 已知命令中需要在注入后观察收尾类别的参数形态。它们依旧是**放行并原样透传**,
/// 只是不借用同一命令其它参数的预采样类别。
fn needs_observed_args_class(entry: &SlashEntry, text: &str) -> bool {
    entry.name == "/goal"
        && text
            .split_whitespace()
            .nth(1)
            .is_some_and(|arg| arg.eq_ignore_ascii_case("clear"))
}

/// 拒绝回执的正文。必须说清**没执行**和**下一步**;含糊或只说「失败」都不合格。
///
/// `command` 是用户发的原文:被明确挡在池外的样本([`REJECTED_SAMPLES`])带着**为什么**挡它,
/// 那句话比通用的「不在池里」有用得多 —— 否则用户(和下一个 agent)只会以为是漏了、要求加进去。
pub fn refusal_reason(command: &str, refusal: &SlashRefusal, agent: &str) -> String {
    let head = command.split_whitespace().next().unwrap_or_default();
    if let Some(why) = rejection_reason(head) {
        return summon_i18n::format!("rf011a490f0");
    }
    match refusal {
        // 通用文案:黑名单是**可配的**,用户自己加进去的条目没有理由文案,不能替他编一个
        // (旧文案写死「登录/登出」,用户拦了 `/foo` 也会看到那句,那是谎报)。
        SlashRefusal::Denylisted => summon_i18n::text!("r23a3d3241c").to_string(),
        SlashRefusal::NoChannel => summon_i18n::text!("r1913e32b82").to_string(),
        SlashRefusal::NoRpcMapping => summon_i18n::format!("r82d4b7b186"),
        SlashRefusal::InvalidRpcArgs { usage } => {
            summon_i18n::format!("r2b2fc8205a")
        }
        SlashRefusal::InvalidArgs { usage } => {
            summon_i18n::format!("rcd43172160")
        }
        SlashRefusal::ArgsNotDeclared => summon_i18n::text!("rc7b6a0f9a1").to_string(),
        SlashRefusal::ArgsNotInjectable => summon_i18n::text!("rde53657ecd").to_string(),
        // 准入反转后这一支只剩「形态不可执行」:生命周期变体/带参形态、被显式关停的池内条目、
        // 不是 `/` 开头的文本。普通未知命令**不再**走到这里(它们默认放行)。
        SlashRefusal::NotInPool => summon_i18n::format!("rc5c4f89a24"),
    }
}

/// 该类别的命令,pane 停在这个状态时算不算「跑完了」。
///
/// 这是 `/compact` 那条「无 spinner + 空输入行」判据的泛化:print/slow 收在空输入框,
/// panel 收在「已认出的面板」上(面板不会自己退,等它变空输入框等于等到 cap)。
pub fn is_settled(class: SlashClass, state: PaneState) -> bool {
    match class {
        SlashClass::Print | SlashClass::Slow => state == PaneState::InputReady,
        SlashClass::Panel => state == PaneState::Modal(ModalKind::Panel),
        // turn 类不以抓屏收尾:注入即开一轮真轮,收尾在轮机制里(调用方以与普通轮同构的
        // 消费路持有到 terminal)。任何 pane 形态都不构成它的「跑完了」——
        // 拿抓屏判据等一条真轮,等到的只会是超时误报。
        SlashClass::Turn => false,
    }
}

/// 未知命令的收尾等待窗:静止窗 / 总时长硬顶(秒)。
///
/// 与池内条目**分开**取值,理由和「快慢命令不共享时间窗」同源:池里的窗口是按各命令的真机
/// 采样调出来的,未知命令没有采样,只能取一个「够慢的命令跑完、又不至于把 chat 线程钉死」的
/// 折中。静止窗只在画面**真的一帧不动**时才走(转录区含 spinner 那行的走秒计时,轮在跑时
/// 它每拍都在变,所以这道窗不会误伤思考中的轮)。
pub const GENERIC_STALL_SECS: u64 = 20;
pub const GENERIC_CAP_SECS: u64 = 180;

/// 判成 turn 的第一条证据:**连续**这么多拍观测到「spinner 在跑 + 转录区行数在涨」。
/// 取 2 而不是 1 是为了排开一次性抖动(命令输出落屏的那一拍,spinner 可能还没撤)。
pub const GENERIC_TURN_GROWTH_TICKS: u32 = 2;

/// 判成 turn 的第二条证据:spinner **独自**连续挂满这么久(还在思考、一个字都没吐)。
///
/// 为什么必须有这条兜底:第一条证据要求转录区长出新行,而 claude 思考期间只有 spinner 那行
/// 在原地走秒 —— 只认第一条会让「想得久的轮」一直判不出,最后撞 cap 报不确定,而那条轮其实
/// 跑得好好的。取 15s 是因为真机采样过的打印类命令里最慢的 `/context` 也只有 ~8s spinner。
pub const GENERIC_TURN_PATIENCE_SECS: u64 = 15;

/// 未知命令注入后的**一拍观测**。四个事实全部来自既有机件,不新造判据:
/// `classify_pane` 给状态,`pane_output::transcript_region` / `panel_body` 给正文与行数。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GenericTick {
    /// `classify_pane` 给出的此刻 pane 状态。
    pub state: PaneState,
    /// 该类别相关的正文与上一拍逐字节一致(Modal 看面板正文,其余看转录区)。
    pub stable: bool,
    /// **连续**观测到「spinner 在跑且转录区行数在涨」的拍数。
    pub growth_streak: u32,
    /// spinner 已经连续挂了多久(秒)。状态一离开 `Running` 就归零。
    pub running_secs: u64,
}

/// 未知命令的**归类判据** —— 通用执行器的核心增量。
///
/// 白名单时代的分类发生在**注入之前**(靠真机采样),黑名单时代没有这个前提,所以归类改由
/// **注入之后的观测**定。三条都取正向特征,判不出就返回 `None`(继续等,等到窗口用尽即
/// 「不确定」)—— 与整条 slash 路径一样,「画面静止」永远不单独构成任何结论。
///
/// - **panel**:命中已注册面板特征库(`classify_pane` 判出 `Modal(Panel)`)且面板正文两拍一致
///   → 走 panel 语义(抓正文 → 全文私聊 → 受限 Esc → 验证回到空输入框);
/// - **turn**:CLI 自己开了一轮 —— spinner + 转录区持续增长(已在吐字),或 spinner 独自挂满
///   耐心窗(还在思考)。交给与 `/goal <目标>` 完全同一条路(`consume_transcript_turn`);
/// - **print**:回到空输入框且转录区两拍一致 → 走 print 语义(锚定输出进群,锚失配降级私聊)。
///
/// 各分支的 fail-closed 方向与既有约定一致:`Draft`(用户草稿)、`Modal(Menu)`(菜单可能与
/// 工具授权提示同形,Esc 不授权)、`Unknown` 一律**不**归类 —— 宁可回「不确定」让人 attach
/// 看一眼,也不在一个认不出的界面上按键。
pub fn classify_generic(tick: GenericTick) -> Option<SlashClass> {
    match tick.state {
        PaneState::Modal(ModalKind::Panel) if tick.stable => Some(SlashClass::Panel),
        PaneState::Running
            if tick.growth_streak >= GENERIC_TURN_GROWTH_TICKS
                || tick.running_secs >= GENERIC_TURN_PATIENCE_SECS =>
        {
            Some(SlashClass::Turn)
        }
        PaneState::InputReady if tick.stable => Some(SlashClass::Print),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `/new` 在通用层收口,先于任何 backend 的规划:exact 无参等同 `/reset`,带内容起新会话,
    /// 同形变体拒绝。
    #[test]
    fn new_is_planned_before_any_backend() {
        let blacklist = SlashBlacklist::default();
        let toggles = SlashToggles::default();
        for (cli, mode) in [
            (CliDriver::Claude, CaptureMode::Transcript),
            (CliDriver::Codex, CaptureMode::Rpc),
            (CliDriver::Cursor, CaptureMode::CliSession),
        ] {
            let plan = |text: &str| plan_slash(cli, mode, text, &blacklist, &toggles);
            assert_eq!(plan("/new"), SlashPlan::Lifecycle(LifecycleIntent::Reset));
            assert_eq!(
                plan("/new  看下这个实验 "),
                SlashPlan::NewSession {
                    text: "看下这个实验".into()
                }
            );
            for lookalike in ["/NEW", "/new!", "/New 内容"] {
                assert_eq!(
                    plan(lookalike),
                    SlashPlan::Refuse(SlashRefusal::NotInPool),
                    "{lookalike}"
                );
            }
            assert!(plan("/new 内容").touches_local_session());
        }
        assert_eq!(new_session_text("/new 内容"), Some("内容"));
        assert_eq!(new_session_text("/new"), None);
        assert_eq!(new_session_text("/newer 内容"), None);
    }

    /// `/handoff` 带不带目标与备注都是同一个计划;同形变体拒,不进通用执行器被当命令注入。
    #[test]
    fn handoff_plans_only_the_exact_spelling() {
        let blacklist = SlashBlacklist::default();
        let toggles = SlashToggles::default();
        for (cli, mode) in [
            (CliDriver::Claude, CaptureMode::Transcript),
            (CliDriver::Codex, CaptureMode::Rpc),
            (CliDriver::Cursor, CaptureMode::CliSession),
        ] {
            let plan = |text: &str| plan_slash(cli, mode, text, &blacklist, &toggles);
            for exact in ["/handoff", "/handoff 把 MR 合了", "  /handoff  "] {
                assert_eq!(plan(exact), SlashPlan::Handoff, "{cli:?} {exact}");
            }
            for lookalike in ["/HANDOFF", "/handoff!", "/Handoff 内容"] {
                assert_eq!(
                    plan(lookalike),
                    SlashPlan::Refuse(SlashRefusal::NotInPool),
                    "{cli:?} {lookalike}"
                );
            }
            assert!(plan("/handoff").touches_local_session());
        }
        for form in ["/handoff", "/HANDOFF 内容", "/handoff!"] {
            assert!(
                is_codex_family_tui_command(form),
                "codex 家族里 {form} 要进规划器,不当 $skill 提示词"
            );
        }
    }

    /// `/queue <内容>` 由 dispatcher 改写成普通对话;落到规划器的只剩无参与同形变体,一律回用法,
    /// 不进通用执行器。
    #[test]
    fn queue_content_is_parsed_and_leftover_forms_get_the_usage() {
        assert_eq!(queued_text("/queue 跑完再看日志"), Some("跑完再看日志"));
        assert_eq!(queued_text("  /queue  多行\n内容 "), Some("多行\n内容"));
        for not_content in [
            "/queue",
            "/queue   ",
            "/queued 内容",
            "/QUEUE 内容",
            "/queue!",
        ] {
            assert_eq!(queued_text(not_content), None, "{not_content}");
        }
        let blacklist = SlashBlacklist::default();
        let toggles = SlashToggles::default();
        for (cli, mode) in [
            (CliDriver::Claude, CaptureMode::Transcript),
            (CliDriver::Codex, CaptureMode::Rpc),
            (CliDriver::Cursor, CaptureMode::CliSession),
        ] {
            for leftover in ["/queue", "/QUEUE", "/queue!"] {
                assert_eq!(
                    plan_slash(cli, mode, leftover, &blacklist, &toggles),
                    SlashPlan::Refuse(SlashRefusal::InvalidArgs {
                        usage: "/queue <内容>"
                    }),
                    "{cli:?} {leftover}"
                );
            }
        }
        for form in ["/queue", "/QUEUE 内容", "/queue!", "/Queue"] {
            assert!(
                is_codex_family_tui_command(form),
                "codex 家族里 {form} 要进规划器回用法,不当 $skill 提示词 steer 进当前轮"
            );
        }
    }

    /// **出厂**黑名单(真相源是 `messaging.default.yaml`)。测试一律拿它跑,这样「出厂项生效」
    /// 不是另写一份常量自证,而是真的在验用户拿到的那份配置。
    #[test]
    fn archive_and_resume_are_exact_controlled_commands_for_every_backend() {
        for (cli, mode) in [
            (CliDriver::Claude, CaptureMode::Transcript),
            (CliDriver::Codex, CaptureMode::Rpc),
            (CliDriver::Cursor, CaptureMode::Rpc),
            (CliDriver::Cursor, CaptureMode::CliSession),
        ] {
            for (command, intent) in [
                ("/archive", LifecycleIntent::Archive),
                ("/arc", LifecycleIntent::Archive),
                ("/resume", LifecycleIntent::Resume),
            ] {
                let plan = plan_slash(cli, mode, command, &factory(), &SlashToggles::default());
                assert_eq!(plan, SlashPlan::Lifecycle(intent));
                assert!(plan.touches_local_session());
                for bad in [
                    format!("{command} now"),
                    format!("{command}!"),
                    command.to_uppercase(),
                ] {
                    assert!(
                        matches!(
                            plan_slash(cli, mode, &bad, &factory(), &SlashToggles::default()),
                            SlashPlan::Refuse(_)
                        ),
                        "{bad}"
                    );
                }
            }
        }
    }

    fn factory() -> SlashBlacklist {
        crate::config::builtin_messaging().slash_blacklist
    }

    fn claude(text: &str) -> SlashPlan {
        claude_with(text, &factory())
    }

    fn claude_with(text: &str, blacklist: &SlashBlacklist) -> SlashPlan {
        plan_slash(
            CliDriver::Claude,
            CaptureMode::Transcript,
            text,
            blacklist,
            &SlashToggles::default(),
        )
    }

    /// 采样池从「准入门」降级成「已知命令的优化层」:池内命令仍拿到自己那条调优过的条目
    /// (类别 / 时间窗 / 收尾语义一字未动),池外命令**不再被拒**,而是走通用路。
    #[test]
    fn sampled_commands_keep_their_tuned_entry_and_unknown_ones_go_generic() {
        for name in ["/status", "/cost", "/context", "/compact", "/goal"] {
            assert!(
                matches!(claude(name), SlashPlan::Passthrough { entry, line }
                    if entry.name == name && line == name),
                "{name} 在池里,应拿到自己的调优条目且注入原文即命令本身"
            );
        }
        for outsider in ["/help", "/agents", "/mcp", "/doctor", "/rename"] {
            assert_eq!(
                claude(outsider),
                SlashPlan::Generic {
                    line: outsider.to_string()
                },
                "{outsider} 不在黑名单里,准入反转后默认放行、走通用执行器"
            );
        }
    }

    /// **准入反转本身**(owner 2026-08-14):默认放行,黑名单说了才拒。
    /// 反转之前这条测试是不可能通过的 —— 那时「不在池里」就是终局拒绝。
    #[test]
    fn admission_is_a_blacklist_so_unknown_commands_are_allowed_by_default() {
        // 一张空黑名单 = 什么都不拦:连出厂项都放行(那正是「出厂项可移除」的含义)。
        let empty = SlashBlacklist::default();
        for text in [
            "/whatever",
            "/config",
            "/login",
            "/some-custom-command 带参数",
        ] {
            assert!(
                matches!(claude_with(text, &empty), SlashPlan::Generic { line } if line == text),
                "{text} 在空黑名单下应默认放行"
            );
        }
        // 录一条 badcase 进去就立刻生效,不必重编译、不必进采样池。
        let one = SlashBlacklist {
            default: vec!["/whatever".into()],
            ..Default::default()
        };
        assert_eq!(
            claude_with("/whatever", &one),
            SlashPlan::Refuse(SlashRefusal::Denylisted)
        );
        // 带参形态一并拦(首 token 命中即拦):黑名单收的是「这条命令本身不该跑」。
        assert_eq!(
            claude_with("/whatever 参数", &one),
            SlashPlan::Refuse(SlashRefusal::Denylisted)
        );
        // 大小写变体也拦 —— 窄判的代价是整张黑名单被一个大写字母绕过。
        assert_eq!(
            claude_with("/WhatEver", &one),
            SlashPlan::Refuse(SlashRefusal::Denylisted)
        );
        // 不是 `/` 开头的文本根本不是命令,不能被通用路当命令注进 TUI。
        assert_eq!(
            claude_with("随便说句话", &empty),
            SlashPlan::Refuse(SlashRefusal::NotInPool)
        );
    }

    /// 生效集是 **default ∪ backend 段**,纯并集 —— 三种写法各验一遍:
    /// 只写 default(对所有 backend 生效)、只写某个 backend(只拦它)、两边都写(相加)。
    #[test]
    fn the_effective_blacklist_is_the_union_of_default_and_the_backend_section() {
        let bl = SlashBlacklist {
            default: vec!["/everywhere".into()],
            claude: vec!["/login".into()],
            codex: vec!["/only-codex".into()],
            cursor: vec![],
        };
        let plan = |cli: CliDriver, text: &str| {
            plan_slash(
                cli,
                CaptureMode::Transcript,
                text,
                &bl,
                &SlashToggles::default(),
            )
        };
        // default 段对每个 backend 都生效。
        for cli in [CliDriver::Claude, CliDriver::Cursor] {
            assert_eq!(
                plan(cli, "/everywhere"),
                SlashPlan::Refuse(SlashRefusal::Denylisted),
                "{cli:?} 应吃到 default 段"
            );
        }
        // owner 要的那句话:「claude 拦 /login 其它放行」= 只写进 claude 段。
        assert_eq!(
            plan(CliDriver::Claude, "/login"),
            SlashPlan::Refuse(SlashRefusal::Denylisted)
        );
        assert!(
            matches!(plan(CliDriver::Cursor, "/login"), SlashPlan::Generic { .. }),
            "别的 backend 段没写 /login,不该被 claude 段殃及"
        );
        // 段之间不串:codex 段的条目对 claude/cursor 无效。
        for cli in [CliDriver::Claude, CliDriver::Cursor] {
            assert!(
                matches!(plan(cli, "/only-codex"), SlashPlan::Generic { .. }),
                "{cli:?} 不该吃到 codex 段"
            );
        }
        // 生效集就是两段拼起来,不多不少。
        let mut got: Vec<&str> = bl.effective_for(CliDriver::Claude).collect();
        got.sort_unstable();
        assert_eq!(got, vec!["/everywhere", "/login"]);
        assert_eq!(
            bl.effective_for(CliDriver::Cursor).collect::<Vec<_>>(),
            vec!["/everywhere"]
        );
    }

    /// 出厂默认项(`messaging.default.yaml`)**确实生效**,而且**可以被用户移除** ——
    /// owner 2026-08-14 显式要求:不再硬编码钉死,那是 owner 的权利。
    #[test]
    fn factory_blacklist_items_take_effect_and_stay_removable() {
        for text in [
            "/login", "/logout", "/config", "/model", "/effort", "/effort!", "/effort:", "/EFFORT",
            "/model!",
        ] {
            assert_eq!(
                claude(text),
                SlashPlan::Refuse(SlashRefusal::Denylisted),
                "{text} 是出厂预置的 badcase,开箱即拦"
            );
            // 每一条的**理由**都要能送到用户面前(否则下一个人只会以为是漏了)。
            assert!(
                rejection_reason(text).is_some_and(|why| !why.trim().is_empty()),
                "{text} 缺拒绝理由文案"
            );
        }
        // 用户把 default 段整段换掉(甚至清空)→ 出厂项随之失效。
        let mut removed = factory();
        SlashBlacklistOverride {
            default: Some(vec![]),
            ..Default::default()
        }
        .apply(&mut removed);
        for text in ["/login", "/config"] {
            assert!(
                matches!(claude_with(text, &removed), SlashPlan::Generic { .. }),
                "{text} 已被 owner 从配置里移除,不该还拦着"
            );
        }
    }

    /// 覆盖是**逐段整体替换**:没写的段保持出厂值,写了的段整段换掉。
    /// 逐项 merge 会让出厂项永远删不掉 —— 那就把「可配」偷换成了「只能加不能减」。
    #[test]
    fn a_blacklist_override_replaces_whole_sections_and_leaves_the_rest_alone() {
        let mut b = factory();
        let before_default = b.default.clone();
        SlashBlacklistOverride {
            claude: Some(vec!["/only-claude".into()]),
            ..Default::default()
        }
        .apply(&mut b);
        assert_eq!(b.default, before_default, "没写的段必须原样保留");
        assert_eq!(b.claude, vec!["/only-claude".to_string()]);
        // 再写一次 claude 段 = 整段替换,不是追加。
        SlashBlacklistOverride {
            claude: Some(vec!["/another".into()]),
            ..Default::default()
        }
        .apply(&mut b);
        assert_eq!(b.claude, vec!["/another".to_string()], "段是替换不是并集");
        // YAML 形态就是 owner 要求的那个 schema。
        let parsed: SlashBlacklistOverride =
            serde_yaml::from_str("default:\n  - /login\nclaude:\n  - /config\n").unwrap();
        assert_eq!(parsed.default, Some(vec!["/login".to_string()]));
        assert_eq!(parsed.claude, Some(vec!["/config".to_string()]));
        assert_eq!(parsed.codex, None, "没写的段是 None(= 继承),不是空表");
        assert_eq!(parsed.cursor, None);
    }

    /// 每条入池命令都必须带着真机采样凭据 —— 没有 evidence 就不该有这一条。
    #[test]
    fn every_pool_entry_carries_its_real_machine_evidence() {
        for e in CLAUDE_POOL.iter().chain(CURSOR_POOL) {
            assert!(e.name.starts_with('/'), "{}", e.name);
            assert!(!e.evidence.trim().is_empty(), "{} 缺采样凭据", e.name);
            assert!(e.cap_secs > e.stall_secs, "{} 的硬顶必须比静止窗宽", e.name);
        }
        // 被挡在池外的样本要留着理由,否则下一个人只会以为是漏了。
        assert!(rejection_reason("/config").is_some_and(|r| r.contains("Esc")));
        assert!(rejection_reason("/status").is_none());
    }

    /// 池里那条 Slow 必须与 driver 已声明的 compact 命令**同名**:两处写死同一个字符串,
    /// 改了一处不改另一处,受控 compact 会静默从池里消失(变成 NotInPool 被拒)。
    #[test]
    fn slow_entry_matches_the_driver_declared_compact_command() {
        for (cli, expect) in [
            (CliDriver::Claude, "/compact"),
            (CliDriver::Cursor, "/compress"),
        ] {
            let declared = match cli.compact_spec(CaptureMode::Transcript) {
                crate::driver::CompactSpec::Tmux { command } => command,
                crate::driver::CompactSpec::Unsupported => panic!("{cli:?} 应有 tmux compact 通道"),
            };
            assert_eq!(declared, expect);
            let slow = pool_for(cli, CaptureMode::Transcript)
                .iter()
                .find(|e| e.class == SlashClass::Slow)
                .expect("池里必须有一条 Slow");
            assert_eq!(
                slow.name, declared,
                "{cli:?} 的 Slow 条目与 compact_spec 脱钩了"
            );
        }
    }

    /// 启停名单管的是**优化层**,不是准入门(准入反转后保留兼容,语义一字未改):
    /// 池外名字写进 `enabled` 既打不开黑名单里的条目,也不会凭空造出一条池内条目。
    #[test]
    fn toggles_only_touch_pool_entries_and_never_override_the_blacklist() {
        let opened = SlashToggles {
            enabled: vec!["/config".into(), "/login".into(), "/anything".into()],
            disabled: vec![],
        };
        let plan = |name: &str| {
            plan_slash(
                CliDriver::Claude,
                CaptureMode::Transcript,
                name,
                &factory(),
                &opened,
            )
        };
        // 黑名单钉在启停之上:`enabled` 打不开它。
        for name in ["/config", "/login"] {
            assert_eq!(
                plan(name),
                SlashPlan::Refuse(SlashRefusal::Denylisted),
                "{name} 被配置「打开」了 —— 启停名单不该翻得过黑名单"
            );
        }
        // 池外名字写进 enabled 不会让它变成池内条目(它照旧走通用路,没有调优过的收尾语义)。
        assert!(
            matches!(plan("/anything"), SlashPlan::Generic { .. }),
            "启停名单不该能新增池内条目"
        );
        // 启停对池内条目正常生效,且「关」压过「开」。关掉 = 拒绝,**不是**降级走通用路。
        let off = SlashToggles {
            enabled: vec!["/status".into()],
            disabled: vec!["/status".into()],
        };
        assert_eq!(
            plan_slash(
                CliDriver::Claude,
                CaptureMode::Transcript,
                "/status",
                &factory(),
                &off
            ),
            SlashPlan::Refuse(SlashRefusal::NotInPool),
            "同时出现在两张名单里时必须 fail-closed,且不许悄悄改判成通用路"
        );
    }

    /// 黑名单钉在白名单之上:配置打不开,带不带参都拒。
    #[test]
    fn credential_commands_are_refused_under_every_configuration() {
        for text in ["/login", "/logout", "/login foo", "/logout --force"] {
            assert_eq!(
                claude(text),
                SlashPlan::Refuse(SlashRefusal::Denylisted),
                "{text}"
            );
        }
    }

    /// 生命周期命令永不裸透传:`/quit` 一步生效(owner 2026-08-13 授权),`/clear` 走确认卡。
    #[test]
    fn lifecycle_commands_map_to_controlled_paths_instead_of_being_injected() {
        assert_eq!(claude("/quit"), SlashPlan::Lifecycle(LifecycleIntent::Quit));
        assert_eq!(
            claude("  /quit  "),
            SlashPlan::Lifecycle(LifecycleIntent::Quit)
        );
        assert_eq!(
            claude("/clear"),
            SlashPlan::Lifecycle(LifecycleIntent::Clear)
        );
        // 三条都不该出现在采样池里(池 = 可注入的命令)。
        for e in CLAUDE_POOL {
            assert!(
                !matches!(e.name, "/quit" | "/clear" | "/exit"),
                "{} 不该在池里",
                e.name
            );
        }
    }

    /// **owner 2026-08-14 对第五条授权的显式扩展**:`/exit` 是 `/quit` 的别名,exact 无参形态走
    /// **完全相同**的受控拆除(同一个 `LifecycleIntent::Quit` → 同一条 `Control::Clean` → 同一份
    /// 回执)。此前冻结的原文只认 exact `/quit`,`/exit` 被当成「不自行扩大授权」挡着;
    /// 现在别名由 owner 亲自扩入,那条拒绝理由随之退役。
    ///
    /// 边界一条不放松:**带参照样拒**(参数会改变拆除语义,红线原文「带参形态不算」一字不动),
    /// 且**绝不能**掉进准入反转后的通用执行器 —— 裸注入一条会杀掉 provider 进程的命令,
    /// 等于绕过红线的五条合法会话中断途径。
    #[test]
    fn exit_is_the_authorized_alias_of_quit_and_takes_the_same_controlled_path() {
        assert_eq!(claude("/exit"), claude("/quit"), "别名必须与本体同语义");
        assert_eq!(claude("/exit"), SlashPlan::Lifecycle(LifecycleIntent::Quit));
        assert_eq!(
            claude("  /exit  "),
            SlashPlan::Lifecycle(LifecycleIntent::Quit),
            "外围空白不改变 exact 形态"
        );
        assert!(claude("/exit").touches_local_session(), "它真会拆会话");
        // 带参形态:拒,且不 claim 本地会话、更不许落进通用路被裸注入。
        for text in ["/exit now", "/exit --force", "/exit 顺便清一下"] {
            assert_eq!(
                claude(text),
                SlashPlan::Refuse(SlashRefusal::NotInPool),
                "{text} 不在授权范围内"
            );
            assert!(!claude(text).touches_local_session(), "{text}");
        }
        // 拒绝理由要说清它是别名、且只认 exact 无参 —— 通用文案在这里没有用。
        let why = refusal_reason("/exit now", &SlashRefusal::NotInPool, "a");
        assert!(why.contains("别名"), "{why}");
        assert!(why.contains("exact"), "{why}");
    }

    /// `/stop` = 打断当前轮,**对每个 backend 都一样**。它不是普通命令:飞书原生 CoT 消息上的
    /// 停止键不产生卡片回调,而是发这条文本 —— 所以它是原生 CoT 形态下唯一的打断入口。
    /// 落进通用执行器会被裸注入进 TUI(claude 收到一条不认识的命令,codex 把它当 steer 素材),
    /// 那正是 owner 2026-09-01 报的现象:点了停止,agent 反而多收到一句 `/stop`。
    #[test]
    fn stop_means_interrupt_this_turn_on_every_backend() {
        for cli in [CliDriver::Claude, CliDriver::Codex, CliDriver::Cursor] {
            let plan = plan_slash(
                cli,
                CaptureMode::Rpc,
                "/stop",
                &SlashBlacklist::default(),
                &SlashToggles::default(),
            );
            assert_eq!(plan, SlashPlan::Interrupt, "{cli:?} 的 /stop 必须是打断");
            assert!(
                plan.touches_local_session(),
                "{cli:?}:打断作用于本地会话,准入位必须带上它(否则非 operate 也能停别人的轮)"
            );
        }
        // 同形变体与带参形态照生命周期那套规矩拒:停一轮是破坏性动作,不认模糊拼写。
        for text in ["/STOP", "/stop!", "/stop now", "stop"] {
            assert_eq!(
                claude(text),
                SlashPlan::Refuse(SlashRefusal::NotInPool),
                "{text} 不是那条停止键发出来的文本"
            );
            assert!(!claude(text).touches_local_session(), "{text}");
        }
    }

    /// `/fork <目标>` 跨群带走上下文。它带参,所以形态与生命周期那几条 exact 无参命令不同,
    /// 但收口的理由一样:落进通用执行器就会被裸注入进 TUI —— claude 把它当一句不认识的命令,
    /// codex 把它当 steer 素材喂给模型,而用户以为自己发起了一次 fork。
    #[test]
    fn fork_carries_its_target_verbatim_and_never_reaches_the_generic_executor() {
        assert_eq!(
            claude("/fork 目标群"),
            SlashPlan::Fork {
                target: "目标群".into()
            }
        );
        // 群名里有空格是常态。按空白切再拼回来会把「Demo 项目 Alpha」这类名字弄坏,
        // 所以目标取的是命令名之后的**整段剩余原文**。
        assert_eq!(
            claude("/fork  Demo 项目 Alpha 的 讨论组  "),
            SlashPlan::Fork {
                target: "Demo 项目 Alpha 的 讨论组".into()
            }
        );
        // 无参 = 列候选(空 target),不是拒绝:用户在群里看不到会话索引,得有个入口看可选项。
        assert_eq!(
            claude("/fork"),
            SlashPlan::Fork {
                target: String::new()
            }
        );
        // 同形变体照生命周期那套规矩拒。**每一条都必须不是 Generic/Passthrough** ——
        // 那才是这条测试真正要守的东西。
        for text in ["/FORK 目标群", "/fork! 目标群", "/Fork x"] {
            assert_eq!(
                claude(text),
                SlashPlan::Refuse(SlashRefusal::NotInPool),
                "{text} 不是那条受控命令的拼写"
            );
        }
        for text in ["/fork", "/fork 目标群", "/FORK 目标群", "/fork! x"] {
            assert!(
                !matches!(
                    claude(text),
                    SlashPlan::Generic { .. } | SlashPlan::Passthrough { .. }
                ),
                "{text} 绝不能被裸注入进 TUI"
            );
        }
    }

    /// fork 不碰本地会话:它只落一封邀请函或写目标 chat 的记录,当前这条会话一个字节都不动。
    ///
    /// 准入位跟着这个判断走,所以搞错的后果是实打实的 —— 报 true 会让一条 `/fork` 把本 chat
    /// 永久标成「本地已占用」,挡住 Bridge 接管。
    #[test]
    fn fork_does_not_touch_the_local_session() {
        assert!(!claude("/fork 目标群").touches_local_session());
    }

    #[test]
    fn delegate_auto_commands_stay_out_of_the_provider() {
        for cli in [CliDriver::Claude, CliDriver::Codex, CliDriver::Cursor] {
            for (text, enabled) in [
                ("/delegate auto", None),
                ("/delegate auto on", Some(true)),
                ("/delegate auto off", Some(false)),
            ] {
                let plan = plan_slash(
                    cli,
                    CaptureMode::Rpc,
                    text,
                    &factory(),
                    &SlashToggles::default(),
                );
                assert_eq!(plan, SlashPlan::DelegateAuto { enabled });
                assert!(!plan.touches_local_session());
            }
            for text in [
                "/DELEGATE auto on",
                "/delegate! auto on",
                "/delegate auto maybe",
                "/delegate auto on extra",
            ] {
                assert!(matches!(
                    plan_slash(
                        cli,
                        CaptureMode::Rpc,
                        text,
                        &factory(),
                        &SlashToggles::default()
                    ),
                    SlashPlan::Refuse(_)
                ));
            }
        }
        assert!(is_codex_family_tui_command("/delegate auto on"));
    }

    /// RPC 那侧同样收口:`/fork` 必须在 `plan_rpc_slash` **之前**判掉。掉进去的话,
    /// codex 的安全子集里没有这条,它会被当成未知命令拒绝 —— 那时用户看到的是「不支持」,
    /// 而 fork 本来与 provider 通道无关(它不注入任何按键)。
    #[test]
    fn fork_works_on_the_rpc_path_too() {
        for cli in [CliDriver::Codex, CliDriver::Traex] {
            assert_eq!(
                plan_slash(
                    cli,
                    CaptureMode::Rpc,
                    "/fork 目标群",
                    &SlashBlacklist::default(),
                    &SlashToggles::default(),
                ),
                SlashPlan::Fork {
                    target: "目标群".into()
                },
                "{cli:?} 的 /fork 与 TUI 路同一个意图"
            );
        }
    }

    /// codex/traex 的**分流**也必须认 `/fork`。这条与上面那条不是重复:`plan_slash` 判得对,
    /// 但 dispatcher 的 `chat_task_kind` 先用本函数决定「这条文本要不要进规划器」——
    /// 漏在这里,`/fork 目标群` 会被判成 `ProviderPrompt`,整行当普通 prompt 喂给模型,
    /// 而规划器压根没被调用过。上面那条测试**测不到**这一步。
    #[test]
    fn the_codex_family_router_sends_fork_into_the_planner() {
        for text in ["/fork 目标群", "/FORK x", "/fork! x", "/fork"] {
            assert!(
                is_codex_family_tui_command(text),
                "{text} 必须进受控规划器,不能落成 provider prompt"
            );
        }
    }

    /// `/fork` 要出现在 `/help` 里。一条只给人用的命令,发现不了等于没有。
    #[test]
    fn fork_is_listed_in_the_control_help() {
        let entries = summon_control_help_entries(CliDriver::Claude, &SlashBlacklist::default());
        let fork = entries
            .iter()
            .find(|entry| entry.head == "/fork")
            .expect("/help 的控制命令清单里必须有 /fork");
        assert!(
            crate::i18n::label(fork.usage).contains('<'),
            "它带参,用法要把参数写出来:{}",
            fork.usage
        );
    }

    /// `/term` 私聊一张终端卡。它不注入任何按键,每个 backend 都要走到受控分支:codex/cursor 上
    /// 得回一句「无头 agent 没有终端」,不能落成 provider prompt 或 NoChannel。
    #[test]
    fn term_is_a_controlled_command_on_every_backend() {
        for (cli, mode) in [
            (CliDriver::Claude, CaptureMode::Transcript),
            (CliDriver::Codex, CaptureMode::Rpc),
            (CliDriver::Cursor, CaptureMode::Rpc),
        ] {
            let plan = plan_slash(cli, mode, "/term", &factory(), &SlashToggles::default());
            assert_eq!(plan, SlashPlan::Terminal, "{cli:?}");
            assert!(!plan.touches_local_session(), "出卡不作用于会话");
        }
        for text in ["/TERM", "/term!", "/term now"] {
            assert_eq!(
                claude(text),
                SlashPlan::Refuse(SlashRefusal::NotInPool),
                "{text}"
            );
        }
        assert_ne!(
            claude("/terminal-setup"),
            SlashPlan::Terminal,
            "前缀相同的别的命令不受影响"
        );
    }

    #[test]
    fn the_codex_family_router_sends_term_into_the_planner() {
        for text in ["/term", "/TERM", "/term x"] {
            assert!(is_codex_family_tui_command(text), "{text}");
        }
    }

    #[test]
    fn term_is_listed_in_the_control_help() {
        let entries = summon_control_help_entries(CliDriver::Claude, &SlashBlacklist::default());
        assert!(entries.iter().any(|e| e.head == "/term"));
    }

    /// `/dash` 私聊一张控制台卡。与会话无关,每个 backend 都走受控分支;只认 exact 无参拼写,
    /// 同形变体与带参形态拒绝 —— 落进通用执行器就会被裸注入进 provider。
    #[test]
    fn plan_slash_routes_exact_dash_only() {
        for (cli, mode) in [
            (CliDriver::Claude, CaptureMode::Transcript),
            (CliDriver::Codex, CaptureMode::Rpc),
            (CliDriver::Traex, CaptureMode::Rpc),
            (CliDriver::Cursor, CaptureMode::Rpc),
            (CliDriver::Cursor, CaptureMode::CliSession),
        ] {
            let plan = plan_slash(cli, mode, "/dash", &factory(), &SlashToggles::default());
            assert_eq!(plan, SlashPlan::Dashboard, "{cli:?}/{mode:?}");
            assert!(!plan.touches_local_session(), "出卡不作用于会话");
            for text in ["/DASH", "/dash!", "/dash x", "/Dash", "/dash."] {
                assert_eq!(
                    plan_slash(cli, mode, text, &factory(), &SlashToggles::default()),
                    SlashPlan::Refuse(SlashRefusal::NotInPool),
                    "{cli:?}/{mode:?} {text}"
                );
            }
        }
        assert_ne!(
            claude("/dashboard"),
            SlashPlan::Dashboard,
            "前缀相同的别的命令不受影响"
        );
    }

    /// `/summon-export` 与 backend 无关,每个 backend 都走受控分支;只认 exact 的 `/summon-export` 与
    /// `/summon-export full`,同形变体与别的参数拒绝。
    #[test]
    fn plan_slash_routes_exact_export_and_export_full_only() {
        for (cli, mode) in [
            (CliDriver::Claude, CaptureMode::Transcript),
            (CliDriver::Codex, CaptureMode::Rpc),
            (CliDriver::Traex, CaptureMode::Rpc),
            (CliDriver::Cursor, CaptureMode::Rpc),
            (CliDriver::Cursor, CaptureMode::CliSession),
        ] {
            let plan =
                |text: &str| plan_slash(cli, mode, text, &factory(), &SlashToggles::default());
            assert_eq!(
                plan("/summon-export"),
                SlashPlan::Export { full: false },
                "{cli:?}/{mode:?}"
            );
            assert_eq!(
                plan("/summon-export full"),
                SlashPlan::Export { full: true },
                "{cli:?}/{mode:?}"
            );
            assert!(
                !plan("/summon-export").touches_local_session(),
                "导出不作用于会话"
            );
            for text in ["/SUMMON-EXPORT", "/summon-export!", "/Summon-Export full"] {
                assert_eq!(
                    plan(text),
                    SlashPlan::Refuse(SlashRefusal::NotInPool),
                    "{cli:?}/{mode:?} {text}"
                );
            }
            for text in [
                "/summon-export FULL",
                "/summon-export full x",
                "/summon-export 2h",
            ] {
                assert!(
                    matches!(
                        plan(text),
                        SlashPlan::Refuse(SlashRefusal::InvalidArgs { .. })
                    ),
                    "{cli:?}/{mode:?} {text}"
                );
            }
        }
        assert!(is_codex_family_tui_command("/summon-export full"));
        assert_ne!(
            claude("/export"),
            SlashPlan::Export { full: false },
            "CLI 自带的 /export 照常透传,不被 summon 接走"
        );
        assert!(
            summon_control_help_entries(CliDriver::Codex, &SlashBlacklist::default())
                .iter()
                .any(|e| e.head == "/summon-export")
        );
    }

    #[test]
    fn the_codex_family_router_sends_dash_into_the_planner() {
        for text in ["/dash", "/DASH", "/dash x", "/dash!"] {
            assert!(is_codex_family_tui_command(text), "{text}");
        }
    }

    #[test]
    fn dash_is_listed_in_the_control_help() {
        let entries = summon_control_help_entries(CliDriver::Claude, &SlashBlacklist::default());
        let dash = entries
            .iter()
            .find(|e| e.head == "/dash")
            .expect("/help 的控制命令清单里必须有 /dash");
        assert!(
            crate::i18n::label(dash.description).contains("控制台"),
            "{}",
            dash.description
        );
    }

    /// `/reset` 是 `/session reset` 的 exact 单词别名(owner 2026-09-01:那是他最常用的一条)。
    /// 它与 `/quit` 走同一段受控拆除,差别只有一个 —— 清 resume 记录而不是保留。
    ///
    /// 边界与 `/quit` 一字不差:带参拒、同形变体拒,且**绝不能**掉进通用执行器 ——
    /// 裸注入一条会重置会话的命令等于绕过红线。
    #[test]
    fn reset_is_the_word_alias_of_session_reset_and_stays_exact_only() {
        assert_eq!(
            claude("/reset"),
            SlashPlan::Lifecycle(LifecycleIntent::Reset)
        );
        assert_eq!(
            claude("  /reset  "),
            SlashPlan::Lifecycle(LifecycleIntent::Reset),
            "外围空白不改变 exact 形态"
        );
        assert!(claude("/reset").touches_local_session(), "它真会拆会话");
        assert_ne!(
            claude("/reset"),
            claude("/quit"),
            "两条的拆除语义不同(清记录 vs 留记录),不能合成一个意图"
        );
        for text in [
            "/reset now",
            "/reset --hard",
            "/RESET",
            "/Reset",
            "/reset!",
            "reset",
        ] {
            assert_eq!(
                claude(text),
                SlashPlan::Refuse(SlashRefusal::NotInPool),
                "{text} 不是被授权的那句 /reset"
            );
            assert!(
                !claude(text).touches_local_session(),
                "{text} 连本地会话都不该 claim —— 尤其不许被通用执行器裸注入进 TUI"
            );
        }
        // 拒绝理由要说清它是别名、只认 exact —— 用户看到通用文案只会以为是漏配了。
        let why = refusal_reason("/reset now", &SlashRefusal::NotInPool, "a");
        assert!(why.contains("别名"), "{why}");
        assert!(why.contains("exact"), "{why}");
    }

    /// 红线原文:「带参形态(`/quit <任何东西>`)不算」。它必须落回普通拒绝,不能沾第五条授权。
    #[test]
    fn quit_with_arguments_is_not_the_authorized_one_step_teardown() {
        for text in ["/quit now", "/quit --force", "/clear all"] {
            assert_eq!(
                claude(text),
                SlashPlan::Refuse(SlashRefusal::NotInPool),
                "{text} 不在授权范围内"
            );
        }
    }

    /// owner 2026-08-14 授权:透传类(print/panel)池内命令开放有参形态 —— 首 token 定命令,
    /// **整行原文**(含参数、保留内部空白)进注入计划,零 marker;收尾语义沿用基线类别。
    #[test]
    fn print_and_panel_args_forms_pass_through_with_the_full_original_line() {
        for (text, name, class) in [
            ("/context all", "/context", SlashClass::Print),
            ("/status foo", "/status", SlashClass::Panel),
            // 外围空白 trim 掉,参数内部的空白按原文保留 —— 「整行原文」不是「规范化后的原文」。
            ("  /cost --breakdown  x  ", "/cost", SlashClass::Panel),
        ] {
            match claude(text) {
                SlashPlan::Passthrough { entry, line } => {
                    assert_eq!(entry.name, name, "{text}");
                    assert_eq!(entry.class, class, "{text} 的收尾语义必须沿用基线类别");
                    assert_eq!(line, text.trim(), "注入计划必须是整行原文");
                    assert!(!line.contains("summon-transport"), "零 marker: {line}");
                }
                other => panic!("{text} 应透传,实得 {other:?}"),
            }
        }
    }

    /// `/compact <指令>` 带参放行(owner 2026-08-20):参数就是压缩指令,那正是想要的能力。
    /// **但授权只给了 claude 的 `/compact` 这一条** —— cursor 桶的 `/compress` 没被授权,
    /// 仍 exact-only、带参一律拒(而不是静默丢掉参数,那会让用户以为指令生效了)。
    #[test]
    fn compact_takes_args_but_other_slow_commands_stay_exact_only() {
        assert_eq!(
            claude("/compact 只保留结论"),
            SlashPlan::Passthrough {
                entry: CLAUDE_POOL.iter().find(|e| e.name == "/compact").unwrap(),
                line: "/compact 只保留结论".to_string(),
            },
            "参数即压缩指令,必须整行透传"
        );
        assert_eq!(
            plan_slash(
                CliDriver::Cursor,
                CaptureMode::Transcript,
                "/compress now",
                &factory(),
                &SlashToggles::default(),
            ),
            SlashPlan::Refuse(SlashRefusal::ArgsNotDeclared)
        );
    }

    /// 带参 `/compact` 的**收尾语义不变**:仍是 Slow(宽窗口 + 卡片回执),参数只改变 claude
    /// 压缩时保留什么,不改变「这是一条慢命令」。若哪天它被误判成 print/panel,收尾窗口会从
    /// 240s 缩到秒级,压缩还没跑完就被当成结束 —— 那是静默的错误结果,不是报错。
    #[test]
    fn compact_with_args_keeps_slow_settle_semantics() {
        let entry = CLAUDE_POOL.iter().find(|e| e.name == "/compact").unwrap();
        assert_eq!(
            entry.class_for_line("/compact 只保留架构决策"),
            SlashClass::Slow
        );
        assert_eq!(entry.class_for_line("/compact"), SlashClass::Slow);
        assert_eq!(entry.stall_secs, 240, "宽窗口不能因带参而变");
    }

    /// 控制字符仍然拒:那不是「参数」是按键(换行会触发补 Enter 打在面板上)。
    /// 带参放行**不放宽这道守卫**。
    #[test]
    fn compact_with_control_chars_is_still_refused() {
        assert_eq!(
            claude("/compact 保留\n结论"),
            SlashPlan::Refuse(SlashRefusal::ArgsNotInjectable)
        );
    }

    /// 授权的覆盖面 = print/panel 两桶(2026-08-14)**加上 claude 的 `/compact` 这一条例外**
    /// (2026-08-20)。桶与策略脱钩就是在扩大授权,这条会红,提醒去找 owner 再裁决 ——
    /// 它在 2026-08-20 确实红过一次,那次的结论是 owner 认可扩权,于是把例外显式列进来。
    /// **例外必须逐条列名,不能放宽成「Slow 桶都行」** —— 那等于把一次性授权变成永久口子。
    #[test]
    fn the_args_grant_covers_print_panel_buckets_plus_compact() {
        for e in CLAUDE_POOL.iter().chain(CURSOR_POOL) {
            let expect = match e.class {
                SlashClass::Print | SlashClass::Panel => ArgsPolicy::Passthrough,
                // 唯一被授权带参的 Slow;cursor 的 /compress 等仍是 ExactOnly。
                SlashClass::Slow if e.name == "/compact" => ArgsPolicy::Passthrough,
                SlashClass::Slow => ArgsPolicy::ExactOnly,
                SlashClass::Turn => unreachable!("基线 Turn 由下面的钉子测试挡住"),
            };
            assert_eq!(
                e.args, expect,
                "{} 的参数策略越出了 2026-08-14 的授权",
                e.name
            );
        }
    }

    /// Turn 桶的授权钉死在 `/goal` 一条上(owner 2026-08-14:`/goal <目标>` 是基础要求):
    /// 别的条目声明 `args_class`、或把**基线** class 写成 Turn(裸形态开真轮,没有任何采样),
    /// 都是在扩大「注入即开真轮」的授权面 —— 这条会红,提醒去找 owner 再裁决。
    #[test]
    fn the_turn_bucket_grant_is_pinned_to_goal_alone() {
        for e in CLAUDE_POOL.iter().chain(CURSOR_POOL) {
            assert_ne!(
                e.class,
                SlashClass::Turn,
                "{} 的基线 class 不该是 Turn(裸形态无「开真轮」采样)",
                e.name
            );
            match e.name {
                "/goal" => assert_eq!(
                    e.args_class,
                    Some(SlashClass::Turn),
                    "/goal 的带参形态就是 turn —— 改掉它请先重新采样并找 owner"
                ),
                _ => assert_eq!(e.args_class, None, "{} 不在 turn 授权之列", e.name),
            }
        }
    }

    /// `/goal` 是池里第一条**双形态**命令:裸形态弹目标面板(panel 收尾),带参形态开真轮
    /// (turn 收尾)。`class_for_line` 按注入原文分派 —— 配错任何一边都会锁死会话
    /// (拿 panel 判据等真轮 = 必然超时;拿 turn 语义跑面板 = 面板留屏吞注入)。
    #[test]
    fn goal_settles_as_panel_when_bare_and_as_turn_with_args() {
        let goal = CLAUDE_POOL
            .iter()
            .find(|e| e.name == "/goal")
            .expect("池里应有 /goal");
        assert_eq!(goal.class_for_line("/goal"), SlashClass::Panel);
        assert_eq!(goal.class_for_line("  /goal  "), SlashClass::Panel);
        assert_eq!(
            goal.class_for_line("/goal 完成一个测试目标"),
            SlashClass::Turn
        );
        // 计划层:整行原文进注入计划,零 marker;带参形态照常被控制字符守卫拦下。
        match claude("/goal 完成一个测试目标") {
            SlashPlan::Passthrough { entry, line } => {
                assert_eq!(entry.name, "/goal");
                assert_eq!(line, "/goal 完成一个测试目标");
                assert!(!line.contains("summon-transport"), "零 marker: {line}");
            }
            other => panic!("/goal 带参应透传,实得 {other:?}"),
        }
        assert_eq!(
            claude("/goal 完成\n再多做一步"),
            SlashPlan::Refuse(SlashRefusal::ArgsNotInjectable),
            "换行是按键不是参数,turn 形态也不例外"
        );
        // 启停对命令生效:关掉 /goal,面板形态与 turn 形态一并关掉。
        let off = SlashToggles {
            enabled: vec![],
            disabled: vec!["/goal".into()],
        };
        for text in ["/goal", "/goal 完成一个测试目标"] {
            assert_eq!(
                plan_slash(
                    CliDriver::Claude,
                    CaptureMode::Transcript,
                    text,
                    &factory(),
                    &off
                ),
                SlashPlan::Refuse(SlashRefusal::NotInPool),
                "{text}"
            );
        }
    }

    /// 受控命令只按消息的首命令 token 判定:只有 exact `/clear` 进入生命周期受控路径。
    /// `/goal clear` 的 `clear` 是 `/goal` 的参数,不能越权复用 `/clear` 的控制边界。
    #[test]
    fn goal_clear_is_a_goal_args_form_not_the_controlled_clear_command() {
        for text in [
            "/goal clear",
            "/goal clear 现在",
            "/goal Clear",
            "/goal CLEAR",
            "/goal   clear  ",
        ] {
            let SlashPlan::Generic { line } = claude(text) else {
                panic!("{text} 应原样透传并在注入后观测收尾")
            };
            assert_eq!(line, text.trim());
            assert!(claude(text).touches_local_session());
        }
        assert_eq!(
            claude("/clear"),
            SlashPlan::Lifecycle(LifecycleIntent::Clear),
            "只有首命令 token 为 exact /clear 才进入受控路径"
        );
    }

    /// 换行/控制字符不是参数是按键(换行触发多行提交补 Enter、Tab 是补全键)——
    /// 带参形态只收单行可打印原文,其余明确拒绝,不静默清洗。
    #[test]
    fn args_with_control_characters_are_refused_not_injected() {
        for text in ["/context all\n再帮我看看", "/status a\tb", "/cost a\rb"] {
            assert_eq!(
                claude(text),
                SlashPlan::Refuse(SlashRefusal::ArgsNotInjectable),
                "{text:?}"
            );
        }
    }

    /// 配置停用一条命令 = 它的带参形态一并停用(启停对命令生效,不对形态生效)。
    #[test]
    fn disabling_a_command_also_disables_its_args_form() {
        let off = SlashToggles {
            enabled: vec![],
            disabled: vec!["/context".into()],
        };
        for text in ["/context", "/context all"] {
            assert_eq!(
                plan_slash(
                    CliDriver::Claude,
                    CaptureMode::Transcript,
                    text,
                    &factory(),
                    &off
                ),
                SlashPlan::Refuse(SlashRefusal::NotInPool),
                "{text}"
            );
        }
    }

    /// 无交互通道的 capture 一条都不放行 —— 准入反转**不碰**这道闸:通用执行器要的是一个能
    /// 抓屏、能 send-keys 的 TUI,没有 TUI 就没有「默认放行」可言。
    #[test]
    fn headless_capture_has_no_slash_channel_at_all() {
        let mode = CaptureMode::Rpc;
        assert!(!has_slash_channel(CliDriver::Claude, mode));
        assert!(pool_for(CliDriver::Claude, mode).is_empty());
        for text in ["/status", "/whatever"] {
            assert_eq!(
                plan_slash(
                    CliDriver::Claude,
                    mode,
                    text,
                    &factory(),
                    &SlashToggles::default()
                ),
                SlashPlan::Refuse(SlashRefusal::NoChannel),
                "{text}"
            );
        }
        // codex 从不跑 tmux(Rpc/Subprocess 常驻进程):即便配置里写了 Transcript 也没有 TUI。
        // 反转后这条尤其重要 —— 少了它,codex 会凭空获得一条「默认放行」的裸注入通道。
        assert!(!has_slash_channel(
            CliDriver::Codex,
            CaptureMode::Transcript
        ));
        assert!(pool_for(CliDriver::Codex, CaptureMode::Transcript).is_empty());
        assert_eq!(
            plan_slash(
                CliDriver::Codex,
                CaptureMode::Transcript,
                "/whatever",
                &factory(),
                &SlashToggles::default()
            ),
            SlashPlan::Refuse(SlashRefusal::NoChannel)
        );
    }

    /// 只有会真正碰本地会话的结局才允许 claim;拒绝与「只发一张卡」都不许占住 chat。
    #[test]
    fn only_real_injections_and_quit_claim_the_local_session() {
        assert!(claude("/status").touches_local_session());
        assert!(claude("/goal 完成一个测试目标").touches_local_session());
        assert!(claude("/quit").touches_local_session());
        assert!(claude("/exit").touches_local_session());
        // 未知命令走通用执行器 —— 它会真注入,所以和池内透传同判。
        assert!(claude("/whatever").touches_local_session());
        assert!(!claude("/clear").touches_local_session());
        assert!(!claude("/login").touches_local_session());
        assert!(!claude("/quit now").touches_local_session());
        // 分级开关只改 per-chat 状态,不碰会话 —— 被它 claim 会挡住 Bridge 接管。
        assert!(!SlashPlan::Setting(SettingAction::List).touches_local_session());
        for key in ["cot.visible", "cot.thinking", "agent.reactions"] {
            assert!(!SlashPlan::Setting(SettingAction::Change {
                key,
                value: Some(true)
            })
            .touches_local_session());
        }
    }

    /// Codex 家族的 app-server 不解析 TUI slash 文本；飞书控制指令必须先落成一个原生 RPC
    /// 意图，未知命令则明确拒绝，绝不能再退回普通 prompt 让模型“回答这条命令”。
    #[test]
    fn codex_family_rpc_commands_never_fall_back_to_model_prompts() {
        let plan = |cli, text| {
            plan_slash(
                cli,
                CaptureMode::Rpc,
                text,
                &factory(),
                &SlashToggles::default(),
            )
        };

        for cli in [CliDriver::Codex, CliDriver::Traex] {
            assert_eq!(
                plan(cli, "/quit"),
                SlashPlan::Lifecycle(LifecycleIntent::Quit)
            );
            assert_eq!(
                plan(cli, "/compact"),
                SlashPlan::Rpc(RpcSlashCommand::Compact)
            );
            assert_eq!(
                plan(cli, "/status"),
                SlashPlan::Rpc(RpcSlashCommand::Status)
            );
            assert_eq!(
                plan(cli, "/rename 新名字"),
                SlashPlan::Rpc(RpcSlashCommand::Rename("新名字".into()))
            );
            assert_eq!(
                plan(cli, "/goal"),
                SlashPlan::Rpc(RpcSlashCommand::Goal(RpcGoalCommand::Get))
            );
            assert_eq!(
                plan(cli, "/goal clear"),
                SlashPlan::Rpc(RpcSlashCommand::Goal(RpcGoalCommand::Clear))
            );
            assert_eq!(
                plan(cli, "/goal 把 RPC 控制面补齐"),
                SlashPlan::Rpc(RpcSlashCommand::Goal(RpcGoalCommand::Set(
                    "把 RPC 控制面补齐".into()
                )))
            );
            assert_eq!(
                plan(cli, "/not-an-app-server-command"),
                SlashPlan::Refuse(SlashRefusal::NoRpcMapping)
            );
            assert_eq!(
                plan(cli, "/review"),
                SlashPlan::Refuse(SlashRefusal::NoRpcMapping),
                "会启动模型流但尚未接投递契约的命令必须明确拒绝"
            );
        }
    }

    /// RPC 控制面只接受冻结的参数形状：无参命令不能借参数扩语义，重命名不能为空，控制字符
    /// 不能穿过飞书文本边界进入协议参数。
    #[test]
    fn rpc_control_argument_shapes_are_exact_and_printable() {
        let plan = |text| {
            plan_slash(
                CliDriver::Traex,
                CaptureMode::Rpc,
                text,
                &factory(),
                &SlashToggles::default(),
            )
        };
        for text in ["/compact now", "/status verbose", "/rename", "/goal x\ny"] {
            assert!(
                matches!(
                    plan(text),
                    SlashPlan::Refuse(SlashRefusal::InvalidRpcArgs { .. })
                ),
                "{text:?} 必须在发送 RPC 前被拒"
            );
        }
        let long_name = format!("/rename {}", "x".repeat(101));
        assert!(matches!(
            plan(&long_name),
            SlashPlan::Refuse(SlashRefusal::InvalidRpcArgs { .. })
        ));
        for lookalike in ["/QUIT", "/quit!", "/Exit."] {
            assert!(
                is_codex_family_tui_command(lookalike),
                "生命周期同形变体必须进入拒绝规划器，不能退成 provider prompt"
            );
            assert!(matches!(
                plan(lookalike),
                SlashPlan::Refuse(SlashRefusal::NotInPool)
            ));
        }
    }

    /// 保留命令必须明确拒绝，不能落入 provider。
    #[test]
    fn reserved_thinking_toggle_is_refused_without_injection() {
        let plan =
            |cli, mode, text| plan_slash(cli, mode, text, &factory(), &SlashToggles::default());
        for cli in [
            CliDriver::Claude,
            CliDriver::Codex,
            CliDriver::Traex,
            CliDriver::Cursor,
            CliDriver::Other,
        ] {
            for mode in [CaptureMode::Transcript, CaptureMode::Rpc] {
                for bad in [
                    "/thinking",
                    "/thinking on",
                    "/thinking off",
                    "/thinking foo",
                ] {
                    assert!(
                        matches!(
                            plan(cli, mode, bad),
                            SlashPlan::Refuse(SlashRefusal::NotInPool)
                        ),
                        "{cli:?} {mode:?} {bad} 必须被拒"
                    );
                }
            }
        }
        // 同形变体必须进规划器(lookalike 闸),不能退成 provider prompt。
        for lookalike in ["/THINKING", "/thinking!", "/Thinking."] {
            assert!(
                is_codex_family_tui_command(lookalike),
                "{lookalike} 必须进入规划器，不能退成 provider prompt"
            );
            assert!(matches!(
                plan_slash(
                    CliDriver::Codex,
                    CaptureMode::Rpc,
                    lookalike,
                    &factory(),
                    &SlashToggles::default()
                ),
                SlashPlan::Refuse(SlashRefusal::NotInPool)
            ));
        }
    }

    #[test]
    fn cot_toggle_is_planned_per_chat_without_injection() {
        let plan =
            |cli, mode, text| plan_slash(cli, mode, text, &factory(), &SlashToggles::default());
        let change = |key, value| {
            SlashPlan::Setting(SettingAction::Change {
                key,
                value: Some(value),
            })
        };
        for cli in [
            CliDriver::Claude,
            CliDriver::Codex,
            CliDriver::Traex,
            CliDriver::Cursor,
            CliDriver::Other,
        ] {
            for mode in [CaptureMode::Transcript, CaptureMode::Rpc] {
                // `/cot` 是 `/set cot.*` 的别名,两条路必须落到同一个意图上。
                assert_eq!(plan(cli, mode, "/cot on"), change("cot.visible", true));
                assert_eq!(plan(cli, mode, "/cot off"), change("cot.visible", false));
                assert_eq!(plan(cli, mode, "/COT OFF"), change("cot.visible", false));
                assert_eq!(
                    plan(cli, mode, "/cot thinking off"),
                    change("cot.thinking", false)
                );
                assert_eq!(plan(cli, mode, "/cot tools on"), change("cot.tools", true));
                assert_eq!(
                    plan(cli, mode, "/set cot.visible off"),
                    plan(cli, mode, "/cot off")
                );

                // `/set` 自己的形态。
                assert_eq!(
                    plan(cli, mode, "/set"),
                    SlashPlan::Setting(SettingAction::List)
                );
                assert_eq!(
                    plan(cli, mode, "/set agent.reactions off"),
                    change("agent.reactions", false)
                );
                // `inherit` 与 `off` 必须是两个不同的意图 —— 前者跟随上层,后者钉死在关。
                assert_eq!(
                    plan(cli, mode, "/set agent.reactions inherit"),
                    SlashPlan::Setting(SettingAction::Change {
                        key: "agent.reactions",
                        value: None
                    })
                );
                for bad in [
                    "/cot",
                    "/cot on off",
                    "/cot maybe",
                    "/set agent.reactions",
                    "/set agent.reactions maybe",
                    "/set no.such.key on",
                    "/set a b c",
                ] {
                    assert!(
                        matches!(
                            plan(cli, mode, bad),
                            SlashPlan::Refuse(SlashRefusal::InvalidArgs { .. })
                        ),
                        "{bad} 应被拒绝"
                    );
                }
            }
        }
        // 同形变体的**唯一**硬要求:不掉进通用执行器被裸注入 —— 那等于把一条控制命令原样
        // 打进 TUI。落成受控意图还是被拒都可以,落成 Generic/Passthrough 不行。
        for lookalike in ["/COT", "/cot!", "/Cot.", "/SET", "/set!", "/Set."] {
            assert!(is_codex_family_tui_command(lookalike));
            let planned = plan_slash(
                CliDriver::Codex,
                CaptureMode::Rpc,
                lookalike,
                &factory(),
                &SlashToggles::default(),
            );
            assert!(
                !matches!(
                    planned,
                    SlashPlan::Generic { .. } | SlashPlan::Passthrough { .. }
                ),
                "{lookalike} 不得裸注入,实到 {planned:?}"
            );
        }
    }

    #[test]
    fn help_entries_match_the_current_planner_and_resolved_blacklist() {
        let blacklist = SlashBlacklist::default();
        let toggles = SlashToggles::default();
        let claude = slash_help_entries(
            CliDriver::Claude,
            CaptureMode::Transcript,
            &blacklist,
            &toggles,
        );
        for head in [
            "/quit", "/clear", "/cot", "/compact", "/context", "/status", "/cost", "/goal",
        ] {
            assert!(claude.iter().any(|entry| entry.head == head), "漏列 {head}");
        }
        assert!(!claude.iter().any(|entry| entry.head == "/mode"));

        let rpc = slash_help_entries(CliDriver::Codex, CaptureMode::Rpc, &blacklist, &toggles);
        for entry in &rpc {
            // `/queue <内容>` 在 dispatcher 就改写成普通对话,到不了规划器;它可执行的证据是改写那一步。
            if entry.head == "/queue" {
                assert!(queued_text("/queue 内容").is_some());
                continue;
            }
            let sample = match crate::i18n::label(entry.usage) {
                "/cot on|off" => "/cot on",
                "/cot thinking on|off" => "/cot thinking on",
                "/cot tools on|off" => "/cot tools on",
                "/rename <名称>" => "/rename title",
                "/goal [目标|clear]" => "/goal",
                "/mcp [verbose]" => "/mcp",
                "/summon-export [full]" => "/summon-export",
                "/fork [new|to <完整群名>]" => "/fork to target group",
                "/delegate auto [on|off]" => "/delegate auto",
                other => other,
            };
            assert!(
                !matches!(
                    plan_slash(
                        CliDriver::Codex,
                        CaptureMode::Rpc,
                        sample,
                        &blacklist,
                        &toggles,
                    ),
                    SlashPlan::Refuse(_)
                ),
                "帮助列出的命令不可执行:{}",
                entry.usage
            );
        }

        let blocked = SlashBlacklist {
            claude: vec!["/status".into(), "/cot".into()],
            ..Default::default()
        };
        let filtered = slash_help_entries(
            CliDriver::Claude,
            CaptureMode::Transcript,
            &blocked,
            &toggles,
        );
        assert!(!filtered
            .iter()
            .any(|entry| matches!(entry.head, "/status" | "/cot")));

        let summon = summon_control_help_entries(CliDriver::Claude, &blacklist);
        assert!(summon.iter().any(|entry| entry.head == "/cot"));
        assert!(summon.iter().any(|entry| entry.head == "/quit"));
        assert!(!summon.iter().any(|entry| entry.head == "/status"));
        let backend = backend_help_entries(
            CliDriver::Claude,
            CaptureMode::Transcript,
            &blacklist,
            &toggles,
        );
        assert!(backend.iter().any(|entry| entry.head == "/status"));
        assert!(!backend.iter().any(|entry| entry.head == "/cot"));
        assert!(!backend.iter().any(|entry| entry.head == "/quit"));
    }

    #[test]
    fn control_help_groups_aliases_after_individual_blacklist_filtering() {
        let aliases = ["/quit", "/exit", "/reset", "/archive", "/arc"];
        for cli in [
            CliDriver::Claude,
            CliDriver::Codex,
            CliDriver::Traex,
            CliDriver::Cursor,
        ] {
            for mask in 0..32 {
                let blacklist = SlashBlacklist {
                    default: aliases
                        .iter()
                        .enumerate()
                        .filter(|(index, _)| mask & (1 << index) != 0)
                        .map(|(_, alias)| (*alias).into())
                        .collect(),
                    ..Default::default()
                };
                let entries = summon_control_help_entries(cli, &blacklist);
                let help = render_control_help_items(&entries);
                for (index, alias) in aliases.iter().enumerate() {
                    assert_eq!(
                        help.contains(&format!("`{alias}`")),
                        mask & (1 << index) == 0
                    );
                }
                for group in [&aliases[..2], &aliases[2..]] {
                    let rows = help
                        .lines()
                        .filter(|row| {
                            group
                                .iter()
                                .any(|alias| row.contains(&format!("`{alias}`")))
                        })
                        .collect::<Vec<_>>();
                    assert_eq!(
                        rows.len(),
                        usize::from(group.iter().any(|alias| !blacklist.blocks(cli, alias)))
                    );
                }
                assert!(help.contains("`/resume`"));
                if mask == 0 {
                    assert!(help.contains("- `/quit` · `/exit`：退出，下条消息接着聊"));
                    assert!(help.contains(
                        "- `/reset` · `/archive` · `/arc`：📦 归档，下条消息新建；/resume 可恢复"
                    ));
                }
            }
        }
    }

    /// 收尾判据:print/slow 收在空输入框(泛化自 `/compact`),panel 收在被认出的面板上。
    /// 「画面静止」永远不单独构成完成 —— 未知态在任何类别下都不算收尾。
    #[test]
    fn settled_states_are_positive_proofs_not_just_stillness() {
        assert!(is_settled(SlashClass::Print, PaneState::InputReady));
        assert!(is_settled(SlashClass::Slow, PaneState::InputReady));
        assert!(!is_settled(SlashClass::Panel, PaneState::InputReady));
        assert!(is_settled(
            SlashClass::Panel,
            PaneState::Modal(ModalKind::Panel)
        ));
        for class in [SlashClass::Print, SlashClass::Panel, SlashClass::Slow] {
            for state in [
                PaneState::Running,
                PaneState::Draft,
                PaneState::Unknown,
                PaneState::Modal(ModalKind::Menu),
            ] {
                assert!(
                    !is_settled(class, state),
                    "{class:?} 不该把 {state:?} 当收尾"
                );
            }
        }
        // turn 类没有抓屏收尾态:任何 pane 形态都不算「跑完了」,收尾在轮机制里。
        for state in [
            PaneState::InputReady,
            PaneState::Running,
            PaneState::Draft,
            PaneState::Unknown,
            PaneState::Modal(ModalKind::Panel),
            PaneState::Modal(ModalKind::Menu),
        ] {
            assert!(
                !is_settled(SlashClass::Turn, state),
                "Turn 不该把 {state:?} 当收尾"
            );
        }
    }

    /// 通用执行器的归类判据:三类都取**正向特征**,判不出就 `None`(继续等 → 最终「不确定」)。
    /// 「画面静止」在这里同样不单独构成任何结论。
    #[test]
    fn generic_classification_takes_positive_evidence_and_admits_when_it_cannot_tell() {
        let tick = |state, stable, growth_streak, running_secs| GenericTick {
            state,
            stable,
            growth_streak,
            running_secs,
        };
        // panel:命中已注册面板特征 + 面板正文两拍一致。
        assert_eq!(
            classify_generic(tick(PaneState::Modal(ModalKind::Panel), true, 0, 0)),
            Some(SlashClass::Panel)
        );
        assert_eq!(
            classify_generic(tick(PaneState::Modal(ModalKind::Panel), false, 0, 0)),
            None,
            "面板还在重绘,不算收尾"
        );
        // turn:spinner + 转录持续增长(快路),或 spinner 独自挂满耐心窗(慢路,还在思考)。
        assert_eq!(
            classify_generic(tick(
                PaneState::Running,
                false,
                GENERIC_TURN_GROWTH_TICKS,
                0
            )),
            Some(SlashClass::Turn)
        );
        assert_eq!(
            classify_generic(tick(
                PaneState::Running,
                false,
                0,
                GENERIC_TURN_PATIENCE_SECS
            )),
            Some(SlashClass::Turn)
        );
        assert_eq!(
            classify_generic(tick(PaneState::Running, false, 1, 0)),
            None,
            "一拍增长是抖动(输出落屏那一拍 spinner 可能还没撤),不足以判成开轮"
        );
        // print:回到空输入框 + 转录区两拍一致。
        assert_eq!(
            classify_generic(tick(PaneState::InputReady, true, 0, 0)),
            Some(SlashClass::Print)
        );
        assert_eq!(
            classify_generic(tick(PaneState::InputReady, false, 0, 0)),
            None,
            "还在往转录区吐字,不算收尾"
        );
        // fail-closed:草稿 / 菜单 / 认不出的画面一律不归类 —— 宁可回「不确定」让人 attach,
        // 也不在一个认不出的界面上按键(Esc 在菜单上是替用户取消工具调用)。
        for state in [
            PaneState::Draft,
            PaneState::Modal(ModalKind::Menu),
            PaneState::Unknown,
        ] {
            for stable in [true, false] {
                assert_eq!(
                    classify_generic(tick(state, stable, 9, 999)),
                    None,
                    "{state:?} 永远不该被归类"
                );
            }
        }
        // 通用路结构上不产出 Slow:慢命令的宽窗口只对采样过的命令有意义。
        assert_ne!(
            classify_generic(tick(PaneState::InputReady, true, 0, 0)),
            Some(SlashClass::Slow)
        );
    }

    /// 授权是**冻结**的:owner 冻结的拼写是 exact `/quit` 与(2026-08-14 扩入的)exact `/exit`。
    /// **大小写变体与带标点的同形变体都不沾这条授权**,而且准入反转之后它们连「落进通用执行器
    /// 被裸注入」这条路都必须被堵死 —— 反转之前池外一切被拒是免费的,现在默认是放行,
    /// 所以这道闸是新增的必需品(`lifecycle_lookalike`)。
    #[test]
    fn only_the_exact_frozen_spelling_of_quit_tears_the_session_down() {
        for text in [
            "/EXIT", "/Quit", "/QUIT", "/Exit", "/quit!", "/exit.", "quit",
        ] {
            assert!(
                !matches!(claude(text), SlashPlan::Lifecycle(_)),
                "{text} 不是被授权的那句 /quit"
            );
            assert!(
                !claude(text).touches_local_session(),
                "{text} 连本地会话都不该 claim —— 尤其不许被通用执行器裸注入进 TUI"
            );
            assert_eq!(
                claude(text),
                SlashPlan::Refuse(SlashRefusal::NotInPool),
                "{text} 必须明确拒绝"
            );
        }
        // 反过来:真正被授权的那两句仍然一步生效(别把边界收得连它们也拒了)。
        assert_eq!(claude("/quit"), SlashPlan::Lifecycle(LifecycleIntent::Quit));
        assert_eq!(claude("/exit"), SlashPlan::Lifecycle(LifecycleIntent::Quit));
    }

    #[test]
    fn refusal_text_tells_the_user_what_to_do_next() {
        let notice = refusal_reason("/whatever", &SlashRefusal::NotInPool, "starlight");
        assert!(notice.contains("attach starlight"), "{notice}");
        assert!(refusal_reason("/login", &SlashRefusal::Denylisted, "x").contains("账号"));
        assert!(
            refusal_reason("/compact 只保留结论", &SlashRefusal::ArgsNotDeclared, "x")
                .contains("参数")
        );
        assert!(
            refusal_reason("/context a\nb", &SlashRefusal::ArgsNotInjectable, "x").contains("换行")
        );
    }

    /// 被挡在池外的样本,其**理由**必须真的送到用户面前 —— 只留在源码注释里等于没写:
    /// 用户看到通用的「不在池里」只会认为是漏了,然后要求加进去(而那正是分类错误的来源)。
    #[test]
    fn a_sampled_rejection_explains_itself_instead_of_saying_only_not_in_pool() {
        let generic = refusal_reason("/help", &SlashRefusal::NotInPool, "a");
        for (command, must_contain) in [
            ("/config", "Esc"),
            ("/model", "菜单"),
            ("/exit", "别名"),
            ("/quit now", "带参"),
            ("/clear all", "确认卡"),
        ] {
            let notice = refusal_reason(command, &SlashRefusal::NotInPool, "a");
            assert!(notice.contains(must_contain), "{command}: {notice}");
            assert_ne!(notice, generic, "{command} 不该退回通用文案");
        }
    }

    /// 只复述指令的结果是纯确认;查询类、压缩的结果都是信息。
    #[test]
    fn only_rename_and_goal_changes_count_as_pure_confirmations() {
        for command in [
            RpcSlashCommand::Rename("x".into()),
            RpcSlashCommand::Goal(RpcGoalCommand::Set("x".into())),
            RpcSlashCommand::Goal(RpcGoalCommand::Clear),
        ] {
            assert!(command.result_only_confirms(), "{command:?}");
        }
        for command in [
            RpcSlashCommand::Help,
            RpcSlashCommand::Compact,
            RpcSlashCommand::Status,
            RpcSlashCommand::Context,
            RpcSlashCommand::Goal(RpcGoalCommand::Get),
            RpcSlashCommand::Mcp { verbose: false },
            RpcSlashCommand::Skills,
            RpcSlashCommand::Hooks,
            RpcSlashCommand::Plugins,
            RpcSlashCommand::Experimental,
            RpcSlashCommand::Processes,
            RpcSlashCommand::StopBackground,
        ] {
            assert!(!command.result_only_confirms(), "{command:?}");
        }
    }
}
