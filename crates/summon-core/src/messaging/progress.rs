//! live 卡的「进行中形状」侧信道:工具轨迹 + thinking 节奏(纯件,无 IO)。
//!
//! **为什么需要它。** Anthropic 自家模型从 opus-4-7 起不再在转录里留明文 thinking —— 实测本机
//! `~/.claude/projects/` 全量转录:claude-opus-4-7/4-8/5、claude-sonnet-5、claude-fable-5 合计 **36871 个 thinking 块,
//! 带正文的 0 个**,只剩加密 `signature`;而同样跑在 claude CLI 下的 `model_api/experimental_*`、
//! `ark/*`、`gpt` 仍然 100% 有正文。于是 `push_segment` 的空文本闸(见 `segment.rs`)把这些块整个丢掉,
//! `segments` 里一段 thinking 都不剩。叠加上「375 条 assistant 记录里只有 56 条含 text」这个事实,
//! live 卡在一轮里 **86% 的时间没有任何内容可刷**,只剩一个瞬态旋转指示 —— 与卡死无从区分。
//!
//! **它补的是「形状」不是「内容」。** 思考正文拿不到了,但「思考发生过、持续多久」「工具走到哪一步、
//! 跑了多久」都还在事件流里,现在被丢弃。本模块把这部分攒起来给 live 卡渲染。
//!
//! **边界(重要)。** 本模块的产物**只进 live 卡**,绝不进 `segments`、收尾卡、总结卡 —— 与既有的
//! 瞬态运行指示同一条不变量。这样 `has_thinking` 语义不变(自家模型仍不拆双卡)、收尾卡不会混入
//! 「思考 12s」这类占位噪音。
//!
//! **只对「思考内容不可见」那一档生效。** 判据不是模型名白名单(硬编码 model id 会过期、且同一个
//! claude CLI 下自家与第三方模型混跑,按 CLI 也分不开),而是**观测到的信号**:见到 thinking 块但
//! 正文为空。经网关接入的模型仍有明文 → 行为一个字不变;哪天明文放回来 → 自动退回原样。

/// live 卡上保留的最大步数的**默认值**。只为给人「走到哪了」的体感,不是完整日志(完整的在
/// events.jsonl)。取 3(owner 2026-08-13,原为 6):再多就把卡片撑成一屏流水账,而人真正在意的
/// 只有「刚做完什么、正在做什么」。要改按会话/全局配 `messaging.progress_max_steps`。
pub const DEFAULT_MAX_STEPS: usize = 3;

/// CoT 工具事件里命令原文的最大字符数。入口即截断，避免长内联脚本随每次快照反复克隆。
pub const MAX_COMMAND_CHARS: usize = 200;
pub const MAX_TOOL_RESULT_CHARS: usize = 512;

fn clip_command(command: &str) -> String {
    let redacted = crate::segment::redact_command(command);
    let mut chars = redacted.chars();
    let head: String = chars.by_ref().take(MAX_COMMAND_CHARS).collect();
    if chars.next().is_some() {
        format!("{head}…")
    } else {
        head
    }
}

fn clip_tool_result(result: &str) -> String {
    let redacted = crate::segment::redact_tool_output(result);
    let mut chars = redacted.chars();
    let head: String = chars.by_ref().take(MAX_TOOL_RESULT_CHARS).collect();
    if chars.next().is_some() {
        format!("{head}…")
    } else {
        head
    }
}

/// 子 agent 每段思考文本的最大字符数。它们整段进 CoT、还要随快照落盘,入口即截断。
pub const MAX_SUBAGENT_THINKING_CHARS: usize = 300;

fn clip_chars(text: &str, max: usize) -> String {
    let mut chars = text.chars();
    let head: String = chars.by_ref().take(max).collect();
    if chars.next().is_some() {
        format!("{head}…")
    } else {
        head
    }
}

/// 意图说明只做长度兜底,不脱敏:它是模型自己写的一句话,不是从环境里捞来的路径/参数
/// (与 `segment::clip_tool_label` 同一判据)。
fn clip_description(description: &str) -> String {
    clip_chars(description, MAX_COMMAND_CHARS)
}

/// 一步留痕。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProgressStep {
    /// 一次工具调用。`done=false` 表示此刻仍在跑(渲染成 ⟳ + 计时)。
    Tool {
        /// 轮内单调编号。CoT 增量投影据此区分连续两次同名工具调用。
        id: u64,
        label: String,
        secs: u64,
        done: bool,
        /// 仅命令类工具有值；展示标题仍使用人读 label。
        command: Option<String>,
        /// 工具返回的有界展示摘要；命令类由 CoT 以等宽块展示。
        result: Option<String>,
    },
    /// 一段不可见的思考。只有时长,没有内容 —— 内容侧已被上游抹掉。
    Thinking { secs: u64 },
}

/// CoT 协议使用的完整工具账本。它与 live 卡只保留最近 `max_steps` 条的展示窗口分离：
/// 展示裁剪不能改变已经开始的协议调用，也不能让晚到的结果失去归属。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtocolTool {
    pub id: u64,
    /// 工具名 + 对象(`Read lib.rs`、`Agent Explore`),不含 description。
    pub label: String,
    pub tool_name: String,
    pub secs: u64,
    pub done: bool,
    pub command: Option<String>,
    /// TOOL_CALL_RESULT 的 fenced code language；只存已证实的文件类型或命令类型。
    pub language: Option<String>,
    pub result: Option<String>,
    /// 模型自己写的这一步意图(claude 工具入参里的 `description`)。它属于思考正文那一档,
    /// 不属于工具详情:`tool_details` 关只收走 label / 命令 / 结果,这句照留,由 CoT 按
    /// 思考正文推进 reasoning 区。
    pub description: Option<String>,
    /// 这一步没有命令、只有 description:description 就是它唯一的「在做什么」,由 CoT 放进
    /// 标题;有命令的步骤标题是命令,description 留在 reasoning 区。判据看原始入参、不看
    /// `tool_details`:description 放在哪只由调用本身决定。
    pub titled_by_description: bool,
}

/// 子 agent 过程里的一项,按到达顺序排。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubagentItem {
    /// 一段思考文本(已截断)。可见与否由 CoT 按思考开关决定,账本照记。
    Thinking(String),
    /// 一次工具调用。`id` 是本 lane 内的序号(从 1 起),不占主 agent 工具的编号:接管重放时
    /// 主/子事件的到达顺序会变,共用计数器会让主工具换号。
    Tool(ProtocolTool),
}

/// 一个子 agent 在本轮里的过程账本。**只给 CoT**:live 卡的步骤窗口与状态行不读它
/// (子 agent 在 live 卡上只经 [`TurnProgress::subagent_tail`] 出现)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentLane {
    key: String,
    title: String,
    description: Option<String>,
    finished: bool,
    items: Vec<SubagentItem>,
    /// provider 的工具调用 id → 本 lane 的工具序号。结果只在本 lane 内按它配对:不同子 agent 的
    /// provider id 可能撞(codex 各线程各自编号),主 agent 的工具更不能被它碰到。
    provider_ids: Vec<(String, u64)>,
    tool_count: u64,
}

impl SubagentLane {
    /// 子 agent 标识(claude:父级 `Agent` 调用的 toolUseId;codex:子线程 id)。
    pub fn key(&self) -> &str {
        &self.key
    }

    /// 英文标题(`Agent Explore`、`child_probe`)。
    pub fn title(&self) -> &str {
        &self.title
    }

    /// 任务说明(已截断)。属于思考正文那一档,由 CoT 按思考开关决定出不出现。
    pub fn description(&self) -> Option<&str> {
        self.description.as_deref()
    }

    pub fn finished(&self) -> bool {
        self.finished
    }

    pub fn items(&self) -> &[SubagentItem] {
        &self.items
    }
}

/// 一次工具调用的入参,字段含义同 [`TurnProgress::tool_started_with_description`]。子 agent 的
/// 工具经它进账本;主 agent 的位置参数入口内部也折成它 —— 两边共用一份详情过滤。
#[derive(Debug, Clone, Copy)]
pub struct ToolCallInput<'a> {
    plain_label: &'a str,
    tool_name: &'a str,
    command: Option<&'a str>,
    language: Option<&'a str>,
    description: Option<&'a str>,
}

impl<'a> ToolCallInput<'a> {
    pub fn new(plain_label: &'a str, tool_name: &'a str) -> Self {
        Self {
            plain_label,
            tool_name,
            command: None,
            language: None,
            description: None,
        }
    }

    pub fn command(mut self, command: &'a str) -> Self {
        self.command = Some(command);
        self
    }

    pub fn language(mut self, language: &'a str) -> Self {
        self.language = Some(language);
        self
    }

    pub fn description(mut self, description: &'a str) -> Self {
        self.description = Some(description);
        self
    }
}

/// 一轮的进行中形状。
///
/// **不 derive `Default`**:`max_steps` 的 usize 默认是 0,而 0 会把每一步立刻裁掉 —— 连
/// [`running_tool_label`] 依赖的「最后一步」都留不下,状态行会整条消失。手写 Default 让任何
/// 忘记设置的构造路径落在 [`DEFAULT_MAX_STEPS`] 上,失败方向与「显示得少一点」一致。
#[derive(Debug, Clone)]
pub struct TurnProgress {
    steps: Vec<ProgressStep>,
    protocol_tools: Vec<ProtocolTool>,
    redacted_thinking: bool,
    /// 此刻是否卡在「等 API 响应,稍后重试」(抓屏认出的)。**与 `enabled()` 无关** —— 同
    /// [`running_tool_label`]:这是所有模型都该被看见的状态,不是「思考不可见」那一档的增强渲染。
    retry_wait: Option<crate::retry_wait::RetryWait>,
    /// **打断按钮的 fencing token**(本轮 turn id)。
    ///
    /// 卡片是托管在飞书服务器上、summon 无法保证能刷新的**副本**。把权限放在「按钮还在屏幕上」
    /// 这个渲染态里,等于信任客户端状态:daemon 重启后旧卡照样带按钮,点下去会打断一个**毫不相干
    /// 的新轮**(路由此前只按 chat_id,不带任何轮次身份)。
    ///
    /// 令牌粒度必须是 **turn 而不是 daemon epoch**:同一个 daemon 内第一轮结束、第二轮开始时,
    /// epoch 相同、旧卡按钮照样能误伤新轮。turn id 同时 fence 住轮次边界与进程重启。
    turn_id: String,
    /// 卡片最后一次刷新的人读时刻(如 `12:09:33`)。
    ///
    /// 陈旧必须**可读**,且不依赖写方能推送:daemon 死了就没人去定格卡片,但读的人看到
    /// 「进行中 · 更新于 12:09」而现在是 12:40,自己就能判断这是陈的。时刻由调用方(IO 层)给,
    /// 本 crate 不碰时钟。
    updated_label: String,
    /// live 卡上保留的最大步数(见 [`DEFAULT_MAX_STEPS`])。由 IO 层从
    /// `messaging.progress_max_steps` 灌进来;本 crate 不读配置。
    max_steps: usize,
    /// 配置驱动的「思考不可见」(`messaging.hide_thinking`,默认 false)。开 → 有正文的 thinking
    /// 也当 redacted 处理(置 `redacted_thinking`、留时长痕),让经网关接入的模型表现等同 claude 自家
    /// 模型(无明文思考 + 工具轨迹)。由 IO 层灌进来;本 crate 不读配置。
    hide_thinking: bool,
    /// 配置驱动的「强制显示工具轨迹」(`messaging.tool_trail`,默认 false)。开 → 即使 thinking
    /// 可见(有明文)也渲染工具轨迹;关 → 只有 thinking 不可见时才渲染(给看不到思考的模型补形状)。
    /// 由 IO 层灌进来;本 crate 不读配置。
    tool_trail: bool,
    /// 是否保留工具命令、参数与结果正文。关时账本与 live 卡仍保留工具名称、状态与耗时;
    /// CoT 按 [`Self::tool_details`] 整行不投。
    tool_details: bool,
    /// live 卡尾部的 subagent 紧凑行(已排序/截断的**纯文本**,由 IO 层从共享 SubagentState
    /// 现读后经 `subagent::subagent_tail_lines` 渲好灌进来;本 crate 不碰状态源)。
    /// 空 = 不渲区块。**只进 live 卡**——收尾卡/总结卡的渲染路径不读它;live 卡 per-turn
    /// 定格后此区块随「更新于」时刻一起冻住,陈旧可读(与 updated_label 同口径)。
    subagent_tail: Vec<String>,
    /// 子 agent 过程账本,按开始顺序。与 `protocol_tools` 同属「整轮的协议账本」,边界重置不清。
    subagent_lanes: Vec<SubagentLane>,
    /// 主 agent 此刻是否在等子 agent(由 IO 层按 provider 信号设置)。CoT 只在它为真时实时推
    /// 子 agent,否则主 agent 自己的行会和子 agent 的行交错。
    parent_waiting: bool,
    next_step_id: u64,
}

impl Default for TurnProgress {
    fn default() -> Self {
        Self {
            steps: Vec::new(),
            protocol_tools: Vec::new(),
            redacted_thinking: false,
            retry_wait: None,
            turn_id: String::new(),
            updated_label: String::new(),
            max_steps: DEFAULT_MAX_STEPS,
            hide_thinking: false,
            tool_trail: false,
            tool_details: true,
            subagent_tail: Vec::new(),
            subagent_lanes: Vec::new(),
            parent_waiting: false,
            next_step_id: 1,
        }
    }
}

impl TurnProgress {
    pub fn new() -> Self {
        Self::default()
    }

    /// 绑定本轮的步数上限。`0` 视作 1 —— 见 [`TurnProgress`] 的 Default 注释:一步不留会连
    /// 状态行一起抹掉,那不是「配置成不显示」,是把功能配坏。
    pub fn with_max_steps(mut self, max_steps: usize) -> Self {
        self.max_steps = max_steps.max(1);
        self
    }

    /// 绑定本轮的「思考隐藏」开关(`messaging.hide_thinking`)。开 → 有正文的 thinking 也当
    /// redacted(见 [`note_thinking`]):经网关接入的模型表现等同 claude 自家模型。
    pub fn with_hide_thinking(mut self, hide: bool) -> Self {
        self.hide_thinking = hide;
        self
    }

    /// 绑定本轮的「强制显示工具轨迹」开关(`messaging.tool_trail`)。开 → 即使 thinking 可见
    /// 也渲染工具轨迹(见 [`enabled`])。
    pub fn with_tool_trail(mut self, trail: bool) -> Self {
        self.tool_trail = trail;
        self
    }

    pub fn with_tool_details(mut self, show: bool) -> Self {
        self.tool_details = show;
        self
    }

    /// 是否启用增强渲染。见模块注释「只对『思考内容不可见』那一档生效」。
    /// `tool_trail` 配置开时,即使 thinking 可见也启用(强制显示工具轨迹)。
    pub fn enabled(&self) -> bool {
        self.redacted_thinking || self.tool_trail
    }

    /// 工具详情是否外显。关时 CoT 不投工具行,只剩思考正文。
    pub fn tool_details(&self) -> bool {
        self.tool_details
    }

    /// 配置是否要求隐藏 thinking 正文(IO 层据此跳过 thinking 段的渲染)。
    pub fn hide_thinking(&self) -> bool {
        self.hide_thinking
    }

    /// 配置是否强制显示工具轨迹(`messaging.tool_trail`)。
    pub fn tool_trail(&self) -> bool {
        self.tool_trail
    }

    /// 绑定本轮的 fencing token(见 `turn_id` 字段注释)。
    pub fn with_turn_id(mut self, turn_id: impl Into<String>) -> Self {
        self.turn_id = turn_id.into();
        self
    }

    /// 本轮步数上限。轮内边界(steer)重置 progress 时要按它重建 —— 重置的是内容,不是配置。
    pub fn max_steps(&self) -> usize {
        self.max_steps
    }

    /// 本轮 fencing token。空 = 无身份的老卡,点击侧一律拒(fail-closed)。
    pub fn turn_id(&self) -> &str {
        &self.turn_id
    }

    /// 轮内语义边界(steer 追加消息被消费 / 交互提问被回答)重置:清瞬态(步数、思考痕、
    /// 重试态、「更新于」时刻、subagent 尾部),**保留配置**(`turn_id` / `max_steps` /
    /// `hide_thinking` / `tool_trail`)——重置的是内容,不是配置。边界后新卡的渲染开关
    /// 必须与旧卡一致,否则隐藏的 thinking 会重新显形、工具轨迹会消失。
    pub fn reset_transient(&mut self) {
        self.steps.clear();
        self.redacted_thinking = false;
        self.retry_wait = None;
        self.updated_label = String::new();
        self.subagent_tail.clear();
    }

    /// 刷新「更新于」时刻(调用方在 emit 前给当前时刻的人读串)。
    pub fn set_updated_label(&mut self, label: impl Into<String>) {
        self.updated_label = label.into();
    }

    pub fn updated_label(&self) -> &str {
        &self.updated_label
    }

    /// 刷新 live 卡尾部的 subagent 行(调用方在 emit 前从共享状态现读渲好)。
    pub fn set_subagent_tail(&mut self, lines: Vec<String>) {
        self.subagent_tail = lines;
    }

    pub fn subagent_tail(&self) -> &[String] {
        &self.subagent_tail
    }

    /// 观测到一个 thinking 块。`secs` 是这块思考大致耗时(调用方按事件间隔估)。
    ///
    /// 有正文 → 什么都不做:那是经网关接入的模型,既有 `segments` 路径会把正文渲上卡,这里再加一条
    /// 「思考 Ns」纯属重复。正文为空 → 命中不可见档,记标志并留时长痕。
    ///
    /// **例外**:`hide_thinking` 配置开时,有正文也当不可见 —— 明文思考的模型据此模拟 claude 自家模型
    /// (无明文思考 + 工具轨迹)。判据是「配置要求隐藏」或「正文为空」,任一成立即 redacted。
    pub fn note_thinking(&mut self, text: &str, secs: u64) {
        let redacted = self.hide_thinking || text.trim().is_empty();
        if !redacted {
            return;
        }
        self.redacted_thinking = true;
        // **紧跟在某个工具后面的思考,时长并进那一步,不单独成行**(owner 2026-08-13)。
        //
        // 这种 Thinking 步没有内容,只有秒数 —— 单独渲成「💭 思考 36s」既占一行又说不清是在
        // 想什么,而它想的正是刚跑完那个工具的结果。并进去之后读起来就是「这一步花了多久」,
        // 和人对「一步」的直觉一致:`✓ 读 xxx 定义 0s` + `💭 思考 36s` → `✓ 读 xxx 定义 36s`。
        //
        // **前面没有已完成工具时仍单独成行**:开场那段思考、或工具正在跑时来的思考,并无处可
        // 并;吞掉它会让卡片看起来一动不动 —— 那正是这套进度渲染要解决的问题。
        if let Some(ProgressStep::Tool {
            secs: elapsed,
            done: true,
            ..
        }) = self.steps.last_mut()
        {
            *elapsed += secs;
            return;
        }
        self.push(ProgressStep::Thinking { secs });
    }

    /// 工具开始。新步入列并标记为在跑。
    pub fn tool_started(&mut self, label: &str) {
        self.tool_started_with_command(label, None);
    }

    pub fn tool_started_with_command(&mut self, label: &str, command: Option<&str>) -> u64 {
        self.tool_started_with_kind(label, label, command)
    }

    pub fn tool_started_with_kind(
        &mut self,
        label: &str,
        tool_name: &str,
        command: Option<&str>,
    ) -> u64 {
        let language = command.map(|_| "bash");
        self.tool_started_with_kind_and_language(label, tool_name, command, language)
    }

    pub fn tool_started_with_kind_and_language(
        &mut self,
        label: &str,
        tool_name: &str,
        command: Option<&str>,
        language: Option<&str>,
    ) -> u64 {
        self.tool_started_with_description(label, label, tool_name, command, language, None)
    }

    /// 工具开始,带模型写的这一步意图(`description`)。空白按「没有」处理 —— 它和缺字段是
    /// 同一个意思。`label` 是 live 卡步骤上的字(有 description 时就是它),`plain_label`
    /// 是不含 description 的「工具名 + 对象」,给 CoT 标题用。
    pub fn tool_started_with_description(
        &mut self,
        label: &str,
        plain_label: &str,
        tool_name: &str,
        command: Option<&str>,
        language: Option<&str>,
        description: Option<&str>,
    ) -> u64 {
        let id = self.next_tool_id();
        let label = if self.tool_details {
            label.to_string()
        } else {
            crate::segment::tool_running_label(tool_name, None)
        };
        let tool = self.protocol_tool(
            id,
            ToolCallInput {
                plain_label,
                tool_name,
                command,
                language,
                description,
            },
        );
        let command = tool.command.clone();
        self.protocol_tools.push(tool);
        self.push(ProgressStep::Tool {
            id,
            label,
            secs: 0,
            done: false,
            command,
            result: None,
        });
        id
    }

    fn next_tool_id(&mut self) -> u64 {
        let id = self.next_step_id;
        self.next_step_id = self.next_step_id.saturating_add(1);
        id
    }

    /// 主 agent 与子 agent 共用的工具账本条目:详情过滤、脱敏与截断只有这一份。
    fn protocol_tool(&self, id: u64, call: ToolCallInput<'_>) -> ProtocolTool {
        let plain_label = if self.tool_details {
            call.plain_label.to_string()
        } else {
            crate::segment::tool_title(call.tool_name, None)
        };
        let description = call
            .description
            .map(str::trim)
            .filter(|value| !value.is_empty());
        let titled_by_description =
            description.is_some() && call.command.map(str::trim).is_none_or(str::is_empty);
        let command = self
            .tool_details
            .then_some(call.command)
            .flatten()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(clip_command);
        ProtocolTool {
            id,
            label: plain_label,
            tool_name: call.tool_name.to_string(),
            secs: 0,
            done: false,
            command,
            language: self
                .tool_details
                .then_some(call.language)
                .flatten()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string),
            result: None,
            description: description.map(clip_description),
            titled_by_description,
        }
    }

    fn tool_result(&self, result: Option<&str>) -> Option<String> {
        self.tool_details
            .then_some(result)
            .flatten()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(clip_tool_result)
    }

    pub fn tool_name(&self, id: u64) -> Option<&str> {
        self.protocol_tools
            .iter()
            .find(|tool| tool.id == id)
            .map(|tool| tool.tool_name.as_str())
    }

    pub fn protocol_tools(&self) -> &[ProtocolTool] {
        &self.protocol_tools
    }

    /// 刷新当前在跑那一步的计时(渲染前调)。没有在跑的步则无操作。
    ///
    /// 这条是「长工具不像卡死」的关键:`cargo test` 能跑十几分钟,一个不动的旋转图标和进程死了在
    /// 用户眼里完全一样。
    pub fn tool_tick(&mut self, secs: u64) {
        if let Some(ProgressStep::Tool {
            secs: s,
            done: false,
            ..
        }) = self.steps.last_mut()
        {
            *s = secs;
        }
    }

    /// 工具结束:把当前在跑那一步定格成已完成 + 最终耗时。
    pub fn tool_finished(&mut self, secs: u64) {
        self.tool_finished_with_result(secs, None);
    }

    pub fn tool_finished_with_result(&mut self, secs: u64, result: Option<&str>) {
        let result = self.tool_details.then_some(result).flatten();
        let id = self
            .protocol_tools
            .iter()
            .rev()
            .find(|tool| !tool.done)
            .map(|tool| tool.id);
        if let Some(id) = id {
            self.tool_finished_by_id(id, secs, result);
        }
    }

    pub fn tool_finished_by_id(&mut self, id: u64, secs: u64, result: Option<&str>) {
        let result = self.tool_result(result);
        if let Some(tool) = self
            .protocol_tools
            .iter_mut()
            .find(|tool| tool.id == id && !tool.done)
        {
            tool.secs = secs;
            tool.done = true;
            tool.result = result.clone();
        }
        if let Some(ProgressStep::Tool {
            secs: s,
            done: done @ false,
            result: stored_result,
            ..
        }) = self.steps.iter_mut().find(|step| {
            matches!(step, ProgressStep::Tool { id: step_id, done: false, .. } if *step_id == id)
        })
        {
            *s = secs;
            *done = true;
            *stored_result = result;
        }
    }

    fn lane_mut(&mut self, key: &str) -> &mut SubagentLane {
        let index = match self.subagent_lanes.iter().position(|lane| lane.key == key) {
            Some(index) => index,
            None => {
                self.subagent_lanes.push(SubagentLane {
                    key: key.to_string(),
                    title: key.to_string(),
                    description: None,
                    finished: false,
                    items: Vec::new(),
                    provider_ids: Vec::new(),
                    tool_count: 0,
                });
                self.subagent_lanes.len() - 1
            }
        };
        &mut self.subagent_lanes[index]
    }

    /// 登记一个子 agent。幂等:重复登记只补标题/说明,不改开始顺序 —— 事件可能先于登记到达
    /// (隐式开始,标题暂用 key),正式登记时才知道它叫什么。
    pub fn subagent_started(&mut self, key: &str, title: &str, description: Option<&str>) {
        let description = description
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(clip_description);
        let lane = self.lane_mut(key);
        let title = title.trim();
        if !title.is_empty() {
            lane.title = title.to_string();
        }
        if description.is_some() {
            lane.description = description;
        }
    }

    /// 子 agent 的一段思考文本。空白不算内容;未登记的 key 隐式开始。
    pub fn subagent_thinking(&mut self, key: &str, text: &str) {
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        let text = clip_chars(text, MAX_SUBAGENT_THINKING_CHARS);
        self.lane_mut(key).items.push(SubagentItem::Thinking(text));
    }

    /// 子 agent 的工具开始,返回它在本 lane 内的序号。过滤规则与主 agent 工具相同(见
    /// [`Self::protocol_tool`]);不进 live 卡的步骤窗口。
    pub fn subagent_tool_started(
        &mut self,
        key: &str,
        provider_id: &str,
        call: ToolCallInput<'_>,
    ) -> u64 {
        let tool = self.protocol_tool(0, call);
        let lane = self.lane_mut(key);
        lane.tool_count += 1;
        let id = lane.tool_count;
        lane.items
            .push(SubagentItem::Tool(ProtocolTool { id, ..tool }));
        lane.provider_ids.push((provider_id.to_string(), id));
        id
    }

    /// 子 agent 的工具结束。只在该 lane 内按 provider id 找还没结束的那一次;找不到就丢。
    pub fn subagent_tool_finished(
        &mut self,
        key: &str,
        provider_id: &str,
        secs: u64,
        result: Option<&str>,
    ) {
        let result = self.tool_result(result);
        let Some(lane) = self.subagent_lanes.iter_mut().find(|lane| lane.key == key) else {
            return;
        };
        let Some(id) = lane
            .provider_ids
            .iter()
            .rev()
            .filter(|(provider, _)| provider == provider_id)
            .map(|(_, id)| *id)
            .find(|id| lane.items.iter().any(|item| is_running_tool(item, *id)))
        else {
            return;
        };
        if let Some(SubagentItem::Tool(tool)) =
            lane.items.iter_mut().find(|item| is_running_tool(item, id))
        {
            tool.secs = secs;
            tool.done = true;
            tool.result = result;
        }
    }

    /// 子 agent 结束。未登记的 key 不凭空造 lane。
    pub fn subagent_finished(&mut self, key: &str) {
        if let Some(lane) = self.subagent_lanes.iter_mut().find(|lane| lane.key == key) {
            lane.finished = true;
        }
    }

    pub fn subagent_lanes(&self) -> &[SubagentLane] {
        &self.subagent_lanes
    }

    pub fn set_parent_waiting(&mut self, waiting: bool) {
        self.parent_waiting = waiting;
    }

    pub fn parent_waiting(&self) -> bool {
        self.parent_waiting
    }

    /// 渲染用:保留的步(最早的已被裁掉)。
    pub fn steps(&self) -> &[ProgressStep] {
        &self.steps
    }

    /// 记下/清掉此刻的「等 API 重试」态。**每个心跳都要调一次**(`Some` 与 `None` 都调):状态
    /// 消失时若不清,卡上会一直挂着失效的倒计时和一个会误注入的「立即重试」按钮。
    pub fn set_retry_wait(&mut self, rw: Option<&crate::retry_wait::RetryWait>) {
        self.retry_wait = rw.cloned();
    }

    /// 此刻的「等 API 重试」态(渲染用)。
    pub fn retry_wait(&self) -> Option<&crate::retry_wait::RetryWait> {
        self.retry_wait.as_ref()
    }

    fn push(&mut self, step: ProgressStep) {
        self.steps.push(step);
        // `with_max_steps` 已把 0 抬到 1;这里再兜一次,免得将来有人绕过 builder 直接构造。
        let cap = self.max_steps.max(1);
        if self.steps.len() > cap {
            let overflow = self.steps.len() - cap;
            self.steps.drain(..overflow);
        }
    }
}

fn is_running_tool(item: &SubagentItem, id: u64) -> bool {
    matches!(item, SubagentItem::Tool(tool) if tool.id == id && !tool.done)
}

/// 此刻在跑的工具 label(= 最后一个未完成的 Tool 步)。**与增强开关无关**:所有模型都要渲
/// 「🔧 执行 X 中…」状态行,这条是既有行为的载体,不能被 `enabled()` gate 掉。
pub fn running_tool_label(progress: &TurnProgress) -> Option<&str> {
    match progress.steps().last() {
        Some(ProgressStep::Tool {
            label, done: false, ..
        }) => Some(label.as_str()),
        _ => None,
    }
}

/// 把耗时渲染成人读短串。秒级以下不显(噪音),分钟以上用 `m` + 秒。
pub fn humanize_secs(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else {
        format!("{}m{:02}s", secs / 60, secs % 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 经网关接入的模型仍有明文 thinking —— 增强必须整个不启用,
    /// 否则卡上会出现「正文 + 思考 Ns」重复。
    #[test]
    fn plaintext_thinking_never_enables_the_enhancement() {
        let mut p = TurnProgress::new();
        p.note_thinking("我在分析这段代码的边界条件", 12);
        assert!(!p.enabled(), "有正文 = 第三方模型档,不能启用");
        assert!(p.steps().is_empty(), "有正文时不该留时长痕(正文自己会上卡)");
    }

    /// 自家模型:thinking 块在但正文空 —— 这就是启用信号。
    #[test]
    fn redacted_thinking_enables_and_leaves_a_duration_mark() {
        let mut p = TurnProgress::new();
        p.note_thinking("", 12);
        assert!(p.enabled());
        assert_eq!(p.steps(), &[ProgressStep::Thinking { secs: 12 }]);
        // 纯空白同样算不可见(上游可能给 "\n" 之类)。
        let mut q = TurnProgress::new();
        q.note_thinking("  \n ", 3);
        assert!(q.enabled());
    }

    /// 配置驱动隐藏 thinking(messaging.hide_thinking):有正文的经网关接入的模型也当不可见 ——
    /// 置 redacted 开轨迹,表现等同 claude 自家模型。关掉则回到「有正文不启用」。
    #[test]
    fn hide_thinking_treats_plaintext_as_redacted() {
        let mut p = TurnProgress::new().with_hide_thinking(true);
        p.note_thinking("我在分析这段代码的边界条件", 12);
        assert!(p.enabled(), "hide_thinking 开:有正文也当 redacted");
        assert_eq!(
            p.steps(),
            &[ProgressStep::Thinking { secs: 12 }],
            "时长痕仍留(形状侧信道),只是正文不渲"
        );
        // 关掉后行为回到原样:有正文不启用。
        let mut q = TurnProgress::new().with_hide_thinking(false);
        q.note_thinking("我在分析", 12);
        assert!(!q.enabled(), "hide_thinking 关:有正文不启用");
        assert!(q.steps().is_empty());
    }

    /// 配置驱动强制显示工具轨迹(messaging.tool_trail):有明文 thinking 也启用增强渲染,
    /// 让 codex/cursor 等有可见思考的模型也能显示三行工具状态栏。与 hide_thinking 正交 ——
    /// tool_trail 不置 redacted_thinking,thinking 正文仍会渲染。
    #[test]
    fn tool_trail_enables_even_with_visible_thinking() {
        let mut p = TurnProgress::new().with_tool_trail(true);
        p.note_thinking("我在分析这段代码的边界条件", 12);
        assert!(p.enabled(), "tool_trail 开:有正文也启用工具轨迹");
        assert!(p.steps().is_empty(), "有正文时不留时长痕(正文自己会上卡)");
        // 关掉后行为回到原样:有正文不启用。
        let mut q = TurnProgress::new().with_tool_trail(false);
        q.note_thinking("我在分析", 12);
        assert!(!q.enabled(), "tool_trail 关:有正文不启用");
    }

    /// 轮内语义边界(steer 追加消息被消费 / 交互提问被回答)重置:瞬态全清,配置原样保留。
    /// 边界后新卡的渲染开关必须与旧卡一致 —— 否则隐藏的 thinking 会重新显形、工具轨迹会消失;
    /// fence token 丢了,切卡后旧卡的打断按钮会点到新轮上。
    #[test]
    fn reset_transient_clears_state_but_keeps_config() {
        let mut p = TurnProgress::new()
            .with_turn_id("turn-42")
            .with_max_steps(7)
            .with_hide_thinking(true)
            .with_tool_trail(true);
        p.note_thinking("内部思考", 3);
        p.tool_started("cargo test");
        p.set_updated_label("12:09:33");
        p.set_subagent_tail(vec!["sub: 跑测试中".into()]);
        p.set_retry_wait(Some(&crate::retry_wait::RetryWait {
            remaining_secs: Some(5),
            line: "retrying".into(),
        }));
        assert!(!p.steps().is_empty());
        assert!(!p.updated_label().is_empty());
        assert!(!p.subagent_tail().is_empty());
        assert!(p.retry_wait().is_some());

        p.reset_transient();

        // 瞬态清空。
        assert!(p.steps().is_empty(), "步数痕要清");
        assert!(p.updated_label().is_empty(), "「更新于」时刻要清");
        assert!(p.subagent_tail().is_empty(), "subagent 尾部要清");
        assert!(p.retry_wait().is_none(), "重试态要清");
        assert_eq!(
            p.protocol_tools().len(),
            1,
            "展示边界不能清掉同一轮的协议工具账本"
        );

        // 配置保留:fence token、步数上限、两个渲染开关。
        assert_eq!(
            p.turn_id(),
            "turn-42",
            "fence token 丢了切卡后打断按钮点不动"
        );
        assert_eq!(p.max_steps(), 7, "步数上限是配置,不是瞬态");
        assert!(
            p.hide_thinking(),
            "hide_thinking 是配置,边界后新卡仍要隐藏 thinking"
        );
        assert!(p.enabled(), "tool_trail 是配置,边界后仍要渲染工具轨迹");
    }

    #[test]
    fn reset_transient_never_reuses_a_tool_event_id_within_the_turn() {
        let mut progress = TurnProgress::new();
        progress.tool_started("first");
        progress.reset_transient();
        progress.tool_started("second");
        assert!(matches!(
            progress.steps(),
            [ProgressStep::Tool { id: 2, .. }]
        ));
    }

    /// `redacted_thinking` 是瞬态:tool_trail 关时 `enabled()` 只由它驱动,重置后必须回到
    /// false —— 否则新卡会带一个没有思考内容的「思考 Ns」痕。
    #[test]
    fn reset_transient_clears_redacted_thinking_flag() {
        let mut p = TurnProgress::new().with_hide_thinking(true); // tool_trail 默认关
        p.note_thinking("内部思考", 3);
        assert!(p.enabled(), "前置:redacted_thinking 已置位");
        p.reset_transient();
        assert!(
            !p.enabled(),
            "redacted_thinking 是瞬态,重置后 enabled 回到 false"
        );
        assert!(p.hide_thinking(), "hide_thinking 配置仍保留");
    }
    #[test]
    fn tool_step_ticks_while_running_then_freezes_on_finish() {
        let mut p = TurnProgress::new();
        p.tool_started("cargo test");
        p.tool_tick(5);
        p.tool_tick(192);
        assert_eq!(
            p.steps(),
            &[ProgressStep::Tool {
                id: 1,
                label: "cargo test".into(),
                secs: 192,
                done: false,
                command: None,
                result: None,
            }]
        );
        p.tool_finished(200);
        assert_eq!(
            p.steps(),
            &[ProgressStep::Tool {
                id: 1,
                label: "cargo test".into(),
                secs: 200,
                done: true,
                command: None,
                result: None,
            }]
        );
        // 已定格的步不该再被后续 tick 改写(下一个工具还没开始时的空 tick)。
        p.tool_tick(999);
        assert!(matches!(p.steps()[0], ProgressStep::Tool { secs: 200, .. }));
    }

    #[test]
    fn concurrent_tool_results_finish_the_matching_step() {
        let mut p = TurnProgress::new().with_max_steps(4);
        let first = p.tool_started_with_command("first", Some("echo first"));
        let second = p.tool_started_with_command("second", Some("echo second"));

        p.tool_finished_by_id(first, 3, Some("first result"));
        assert!(matches!(
            &p.steps()[0],
            ProgressStep::Tool {
                id,
                done: true,
                result: Some(result),
                ..
            } if *id == first && result == "first result"
        ));
        assert!(matches!(
            &p.steps()[1],
            ProgressStep::Tool { id, done: false, .. } if *id == second
        ));

        p.tool_finished_by_id(second, 5, Some("second result"));
        assert!(matches!(
            &p.steps()[1],
            ProgressStep::Tool {
                done: true,
                result: Some(result),
                ..
            } if result == "second result"
        ));
    }

    #[test]
    fn protocol_tool_journal_outlives_the_three_row_display_window() {
        let mut progress = TurnProgress::new().with_max_steps(3);
        let first = progress.tool_started_with_kind("first", "tool_first", Some("echo first"));
        let _second = progress.tool_started_with_kind("second", "tool_second", None);
        let _third = progress.tool_started_with_kind("third", "tool_third", None);
        let fourth = progress.tool_started_with_kind("fourth", "tool_fourth", None);

        assert_eq!(progress.steps().len(), 3, "人读窗口仍只保留三行");
        assert_eq!(progress.protocol_tools().len(), 4, "协议账本不得随 UI 裁剪");

        progress.tool_finished_by_id(first, 7, Some("first result"));
        progress.tool_finished_by_id(fourth, 9, Some("fourth result"));
        let first = &progress.protocol_tools()[0];
        let fourth = &progress.protocol_tools()[3];
        assert!(first.done);
        assert_eq!(first.result.as_deref(), Some("first result"));
        assert!(fourth.done);
        assert_eq!(fourth.result.as_deref(), Some("fourth result"));
    }

    /// 轨迹有界:长轮几十上百次工具调用,卡片只保留最近若干步。
    #[test]
    fn trajectory_is_bounded_and_keeps_the_newest() {
        let mut p = TurnProgress::new();
        for i in 0..20 {
            p.tool_started(&format!("step{i}"));
            p.tool_finished(1);
        }
        assert_eq!(p.steps().len(), DEFAULT_MAX_STEPS);
        assert!(
            matches!(&p.steps()[DEFAULT_MAX_STEPS - 1], ProgressStep::Tool { label, .. } if label == "step19"),
            "裁掉的必须是最早的,最新一步要留住"
        );
    }

    /// 上限可配。默认 3(owner 2026-08-13),`messaging.progress_max_steps` 能调大调小。
    #[test]
    fn step_cap_follows_configured_value() {
        for cap in [1usize, 2, 5, 12] {
            let mut p = TurnProgress::new().with_max_steps(cap);
            for i in 0..20 {
                p.tool_started(&format!("step{i}"));
                p.tool_finished(1);
            }
            assert_eq!(p.steps().len(), cap, "cap={cap} 应恰好保留这么多步");
            assert!(
                matches!(p.steps().last(), Some(ProgressStep::Tool { label, .. }) if label == "step19"),
                "cap={cap} 时最新一步仍要留住"
            );
        }
    }

    /// `0` 不是「不显示」而是把功能配坏:一步不留会连状态行(靠最后一步渲「🔧 执行 X 中…」)
    /// 一起抹掉。抬到 1,失败方向与「显示得少一点」一致。
    #[test]
    fn zero_cap_still_keeps_the_running_step_visible() {
        let mut p = TurnProgress::new().with_max_steps(0);
        p.tool_started("cargo test");
        assert_eq!(p.steps().len(), 1, "0 必须被抬到 1");
        assert_eq!(running_tool_label(&p), Some("cargo test"), "状态行不能消失");
    }

    /// 没走 builder 的构造路径(含 `Default`)必须落在默认值上,而不是 usize 的 0。
    #[test]
    fn default_construction_never_yields_a_zero_cap() {
        for mut p in [TurnProgress::new(), TurnProgress::default()] {
            for i in 0..10 {
                p.tool_started(&format!("s{i}"));
                p.tool_finished(1);
            }
            assert_eq!(p.steps().len(), DEFAULT_MAX_STEPS);
        }
    }

    /// thinking 与工具按到达顺序交织 —— 这正是「形状」的意义:看得出它想了一下再动手。
    /// 工具跑完后的思考并进那一步;开场那段没处并,仍单独成行。
    #[test]
    fn thinking_after_a_tool_folds_into_that_step() {
        let mut p = TurnProgress::new();
        p.note_thinking("", 8);
        p.tool_started("读 transcript.rs");
        p.tool_finished(1);
        p.note_thinking("", 15);
        p.tool_started("cargo test");
        p.tool_tick(60);
        assert_eq!(
            p.steps(),
            &[
                ProgressStep::Thinking { secs: 8 },
                ProgressStep::Tool {
                    id: 1,
                    label: "读 transcript.rs".into(),
                    // 1s 工具 + 15s 思考:用户看到的是「这一步花了 16 秒」。
                    secs: 16,
                    done: true,
                    command: None,
                    result: None,
                },
                ProgressStep::Tool {
                    id: 2,
                    label: "cargo test".into(),
                    secs: 60,
                    done: false,
                    command: None,
                    result: None,
                },
            ]
        );
    }

    /// 连着好几段思考也只并成一份耗时,不会一段一行。
    #[test]
    fn consecutive_thinking_all_folds_into_the_same_step() {
        let mut p = TurnProgress::new();
        p.tool_started("读夹具");
        p.tool_finished(2);
        p.note_thinking("", 5);
        p.note_thinking("", 7);
        assert_eq!(
            p.steps(),
            &[ProgressStep::Tool {
                id: 1,
                label: "读夹具".into(),
                secs: 14,
                done: true,
                command: None,
                result: None,
            }]
        );
    }

    /// 工具**还在跑**时来的思考不能并进去 —— 那一步的秒数由 `tool_tick` 覆写,并进去会被冲掉,
    /// 时间就凭空消失了。这种保持单独成行。
    #[test]
    fn thinking_while_a_tool_runs_stays_its_own_step() {
        let mut p = TurnProgress::new();
        p.tool_started("cargo test");
        p.note_thinking("", 9);
        assert_eq!(
            p.steps(),
            &[
                ProgressStep::Tool {
                    id: 1,
                    label: "cargo test".into(),
                    secs: 0,
                    done: false,
                    command: None,
                    result: None,
                },
                ProgressStep::Thinking { secs: 9 },
            ]
        );
    }

    /// fencing token 的粒度必须是 **turn**。若用 daemon epoch:同一 daemon 内上一轮结束、新一轮
    /// 开始时 epoch 不变,旧卡按钮照样能打断新轮 —— 这正是 2026-08-07 设计讨论里推翻的方案。
    #[test]
    fn fencing_token_is_per_turn_not_per_process() {
        let a = TurnProgress::new().with_turn_id("turn-1");
        let b = TurnProgress::new().with_turn_id("turn-2");
        assert_ne!(
            a.turn_id(),
            b.turn_id(),
            "同一进程内相邻两轮必须拿到不同令牌"
        );
        // 没绑令牌 = 无身份(老卡),点击侧据此 fail-closed。
        assert!(TurnProgress::new().turn_id().is_empty());
    }

    /// 「更新于」时刻是**读侧自愈**:不依赖写方推送就能看出卡片陈了。
    #[test]
    fn updated_label_is_settable_and_defaults_empty() {
        let mut p = TurnProgress::new();
        assert!(p.updated_label().is_empty(), "没盖时刻就不渲,别显示假时间");
        p.set_updated_label("12:09:33");
        assert_eq!(p.updated_label(), "12:09:33");
    }

    #[test]
    fn durations_read_as_human_text() {
        assert_eq!(humanize_secs(0), "0s");
        assert_eq!(humanize_secs(59), "59s");
        assert_eq!(humanize_secs(60), "1m00s");
        assert_eq!(humanize_secs(192), "3m12s");
        assert_eq!(humanize_secs(1005), "16m45s");
    }

    #[test]
    fn hidden_tool_details_keep_status_but_drop_command_and_result() {
        let mut progress = TurnProgress::new().with_tool_details(false);
        let id = progress.tool_started_with_kind_and_language(
            "运行测试",
            "Bash",
            Some("cargo test --workspace"),
            Some("bash"),
        );
        progress.tool_finished_by_id(id, 3, Some("test result: ok"));
        let tool = &progress.protocol_tools()[0];
        assert_eq!(tool.label, "Bash");
        assert!(tool.done);
        assert_eq!(tool.secs, 3);
        assert_eq!(tool.command, None);
        assert_eq!(tool.language, None);
        assert_eq!(tool.result, None);

        let read_id = progress.tool_started_with_kind_and_language(
            "读取 secret.rs",
            "Read",
            None,
            Some("rust"),
        );
        progress.tool_finished_by_id(read_id, 1, Some("private source"));
        let read = &progress.protocol_tools()[1];
        assert_eq!(read.label, "Read");
        assert!(!read.label.contains("secret.rs"));
        assert_eq!(read.language, None);
        assert_eq!(read.result, None);
    }

    /// description 属于思考正文那一档:`tool_details` 关只收走 label / 命令 / 语言,它要原样留在账本里。
    #[test]
    fn tool_description_survives_tool_details_off() {
        let mut progress = TurnProgress::new().with_tool_details(false);
        progress.tool_started_with_description(
            "查定义",
            "Bash",
            "Bash",
            Some("grep foo"),
            Some("bash"),
            Some("查定义"),
        );
        let tool = &progress.protocol_tools()[0];
        assert_eq!(tool.label, "Bash");
        assert_eq!(tool.command, None);
        assert_eq!(tool.description.as_deref(), Some("查定义"));

        let mut blank = TurnProgress::new();
        blank.tool_started_with_description("x", "x", "Bash", None, None, Some("   "));
        assert_eq!(
            blank.protocol_tools()[0].description,
            None,
            "空白 description 等于没有"
        );
        let mut bounded = TurnProgress::new();
        let long = "意".repeat(MAX_COMMAND_CHARS + 10);
        bounded.tool_started_with_description("x", "x", "Bash", None, None, Some(&long));
        let clipped = bounded.protocol_tools()[0].description.clone().unwrap();
        assert_eq!(clipped.chars().count(), MAX_COMMAND_CHARS + 1);
        assert!(clipped.ends_with('…'));
    }

    /// 重复登记同一个子 agent 不改它在账本里的位置 —— CoT 按账本里的先后给新出现的子 agent 排位,
    /// 位置一变,后开始的就会排到前面。
    #[test]
    fn subagent_started_is_idempotent_and_keeps_the_start_order() {
        let mut p = TurnProgress::new();
        p.subagent_started("a", "Agent Explore", Some("  找入口 "));
        p.subagent_started("b", "child_probe", None);
        p.subagent_started("a", "Agent Explore", Some("找入口"));
        let lanes = p.subagent_lanes();
        assert_eq!(
            lanes.iter().map(SubagentLane::key).collect::<Vec<_>>(),
            ["a", "b"]
        );
        assert_eq!(lanes[0].title(), "Agent Explore");
        assert_eq!(lanes[0].description(), Some("找入口"));
        assert_eq!(lanes[1].description(), None);
        assert!(!lanes[0].finished());

        p.subagent_finished("a");
        p.subagent_finished("nobody");
        assert!(p.subagent_lanes()[0].finished());
        assert!(!p.subagent_lanes()[1].finished());
        assert_eq!(p.subagent_lanes().len(), 2, "未知 key 的结束不凭空造 lane");
    }

    /// 思考先于登记到达时隐式开始(标题先用 key),正式登记补上标题与说明,顺序不变。
    #[test]
    fn subagent_thinking_implicitly_starts_the_lane_and_is_clipped() {
        let mut p = TurnProgress::new();
        p.subagent_thinking("x", "  \n ");
        assert!(p.subagent_lanes().is_empty(), "空白思考不算内容,也不算开始");

        p.subagent_thinking("x", &"想".repeat(MAX_SUBAGENT_THINKING_CHARS + 5));
        p.subagent_started("y", "Agent Plan", None);
        p.subagent_started("x", "Agent Explore", Some("找入口"));
        let lanes = p.subagent_lanes();
        assert_eq!(lanes[0].key(), "x", "隐式开始也占开始顺序");
        assert_eq!(lanes[0].title(), "Agent Explore");
        assert_eq!(lanes[0].description(), Some("找入口"));
        let [SubagentItem::Thinking(text)] = lanes[0].items() else {
            panic!("应只有一段思考:{:?}", lanes[0].items())
        };
        assert_eq!(text.chars().count(), MAX_SUBAGENT_THINKING_CHARS + 1);
        assert!(text.ends_with('…'));
    }

    /// 子 agent 工具按 lane 各自编号,不占主 agent 的计数器:接管重放时主/子的到达顺序会变,
    /// 共用计数器会让主工具换号。
    #[test]
    fn subagent_tools_are_numbered_per_lane_without_consuming_main_ids() {
        let mut p = TurnProgress::new();
        let main = p.tool_started_with_kind("ls", "Bash", Some("ls"));
        let a1 = p.subagent_tool_started("a", "toolu_1", ToolCallInput::new("Read lib.rs", "Read"));
        let b1 = p.subagent_tool_started("b", "toolu_1", ToolCallInput::new("Read b.rs", "Read"));
        let main2 = p.tool_started_with_kind("pwd", "Bash", Some("pwd"));
        let a2 = p.subagent_tool_started("a", "toolu_2", ToolCallInput::new("Read c.rs", "Read"));
        assert_eq!((main, main2), (1, 2));
        assert_eq!((a1, a2, b1), (1, 2, 1));
        assert_eq!(p.subagent_lanes()[0].title(), "a", "工具先到也隐式开始");
    }

    /// 子 agent 账本只给 CoT:live 卡的步骤窗口与状态行只看主 agent。
    #[test]
    fn subagent_ledger_stays_out_of_the_live_card_window() {
        let mut p = TurnProgress::new();
        p.tool_started("cargo test");
        p.subagent_started("a", "Agent Explore", None);
        p.subagent_thinking("a", "想想");
        p.subagent_tool_started("a", "t1", ToolCallInput::new("Grep foo", "Grep"));
        p.subagent_tool_finished("a", "t1", 1, Some("hit"));
        assert_eq!(p.steps().len(), 1);
        assert_eq!(running_tool_label(&p), Some("cargo test"));
        assert_eq!(p.protocol_tools().len(), 1);
    }

    /// 结果只在本 lane 内按 provider id 配对;配不上就丢,绝不落到主 agent 或别的 lane 上。
    #[test]
    fn subagent_tool_result_pairs_only_within_its_own_lane() {
        let bash = |command| ToolCallInput::new("Bash", "Bash").command(command);
        let mut p = TurnProgress::new();
        p.tool_started_with_kind("ls", "Bash", Some("ls"));
        let a = p.subagent_tool_started("a", "call_1", bash("echo a"));
        let b = p.subagent_tool_started("b", "call_1", bash("echo b"));
        p.subagent_tool_finished("b", "call_1", 2, Some("b out"));
        p.subagent_tool_finished("c", "call_1", 3, Some("unknown lane"));
        p.subagent_tool_finished("a", "call_9", 4, Some("unknown call"));

        assert!(!p.protocol_tools()[0].done, "主工具不受影响");
        let tool = |key: &str, id: u64| {
            p.subagent_lanes()
                .iter()
                .find(|lane| lane.key() == key)
                .and_then(|lane| {
                    lane.items().iter().find_map(|item| match item {
                        SubagentItem::Tool(tool) if tool.id == id => Some(tool.clone()),
                        _ => None,
                    })
                })
                .unwrap()
        };
        assert!(!tool("a", a).done);
        let done = tool("b", b);
        assert!(done.done);
        assert_eq!(done.secs, 2);
        assert_eq!(done.result.as_deref(), Some("b out"));
        assert!(p.subagent_lanes().iter().all(|lane| lane.key() != "c"));
    }

    /// 与主工具同一道详情过滤:详情关时命令、语言、结果、带对象的标题都不进账本,意图照留。
    #[test]
    fn subagent_tools_follow_the_tool_details_filter() {
        let mut p = TurnProgress::new().with_tool_details(false);
        p.subagent_tool_started(
            "a",
            "c1",
            ToolCallInput::new("Read secret.rs", "Read")
                .command("cat secret.rs")
                .language("rust")
                .description("看配置"),
        );
        p.subagent_tool_finished("a", "c1", 1, Some("private"));
        let [SubagentItem::Tool(tool)] = p.subagent_lanes()[0].items() else {
            panic!("应只有一个工具")
        };
        assert_eq!(tool.label, crate::segment::tool_title("Read", None));
        assert_eq!(tool.command, None);
        assert_eq!(tool.language, None);
        assert_eq!(tool.result, None);
        assert_eq!(tool.description.as_deref(), Some("看配置"));
        assert!(tool.done);
    }

    /// 「主 agent 在等子 agent」由 IO 层给;边界重置不动它,也不动子 agent 账本。
    #[test]
    fn parent_waiting_and_the_subagent_ledger_survive_reset_transient() {
        let mut p = TurnProgress::new();
        assert!(!p.parent_waiting());
        p.set_parent_waiting(true);
        p.subagent_thinking("a", "想想");
        p.reset_transient();
        assert!(p.parent_waiting());
        assert_eq!(p.subagent_lanes().len(), 1);
        p.set_parent_waiting(false);
        assert!(!p.parent_waiting());
    }
}
