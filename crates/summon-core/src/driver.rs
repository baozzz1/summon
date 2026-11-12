mod bang;
mod claude;
mod codex;
mod common;
mod cursor;
pub mod slash_pool;

pub use bang::{bang_admission, bang_command, shell_passthrough_supported, BangAdmission};
pub use claude::fork_headless_command_args;

use crate::config::registry::CaptureMode;

pub use codex::{
    codex_model_supports_fast, config_declares_summary, parse_context_window,
    reasoning_summary_decision, CodexServiceTier, ParseCodexServiceTierError,
};
pub use cursor::{
    cursor_input_is_clear, is_cursor_ready, scrub_cursor_chrome, CursorSessionMode,
    ParseCursorSessionModeError,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CliDriver {
    Claude,
    Cursor,
    Codex,
    /// traex(traecli):codex 的 fork。协议/flag/rollout 格式与 codex 同构
    /// (2026-08-18 真机冒烟:initialize 握手、thread/start、turn/start 事件流逐字段一致),
    /// 故全部分派复用 codex 模块;差异只有四点,收在本枚举:二进制名、家目录、配置文件名、
    /// rollout 子目录(见 `private_home` / `config_filename` / `sessions_subdir`)。
    Traex,
    Other,
}

/// model/effort 解析后要落到启动命令上的「片段」。形态因 CLI 而异,但统一成一串 argv:
/// - claude:model 走 `--model`,effort 走独立 `--effort`(两个正交 flag)。
/// - codex :model 走 `-m`,effort 走 `-c model_reasoning_effort=<v>`(config 覆盖,TOML 裸词)。
/// - cursor:无 effort 轴——tier 在 bind 时已烤进 model 名,故只剩一个 `--model <id>`;
///   传 effort 直接 `EffortUnsupported`(运行期只校验 model 串,不做后缀拼接)。
///
/// 调用方(tmux / headless 两条 launch 路径)拿 `args` 原样拼到启动命令,可能为空(=用 CLI 默认)。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LaunchModel {
    pub args: Vec<String>,
}

/// 无头一轮启动命令的输入项。把 resume/sys_prompt_file/model/effort 收进一个结构,避免
/// `headless_command` 形参爆炸(满足 clippy),也让 tmux/headless 两条路径都能复用同一组入参。
/// model/effort 经 `resolve_launch` 落成各 CLI 的 launch 片段;effort 非法时由 `headless_command`
/// 降级(忽略 effort 重试 model-only)而非 panic,见设计 §3 fail-closed 分层。
#[derive(Debug, Clone, Default)]
pub struct LaunchOpts<'a> {
    /// resume:续接已有原生会话(无则冷启)。
    pub resume: Option<&'a str>,
    /// claude `--append-system-prompt-file`:每轮注入系统提示的文件路径(仅 claude 用)。
    pub sys_prompt_file: Option<&'a str>,
    /// agent.yaml 顶层 `model`(per-CLI 私有 token,空=CLI 默认)。
    pub model: Option<&'a str>,
    /// agent.yaml 顶层 `effort`(规范逻辑档位,空=不指定,用 CLI 默认)。
    pub effort: Option<&'a str>,
    /// codex `model_reasoning_summary`:调用方已按优先级解析好的注入值(None=不注入);仅 codex 用。
    pub codex_reasoning_summary: Option<&'a str>,
    /// codex 服务档位；None=不覆盖 Codex 全局/线程设置。
    pub codex_service_tier: Option<CodexServiceTier>,
    /// codex 专用:显式绕过 trusted-directory 的 Git 仓库检查。默认 false。
    pub codex_skip_git_repo_check: bool,
}

/// (model, effort) 解析/校验的可恢复错误。用 `thiserror` 因为调用方(bind/onboard/卡片)要
/// match 出友好回执。**load/reread 路径绝不靠它 panic**:非法值留在内存,直到 launch 调
/// `resolve_launch` 才 Err,此时降级(忽略 effort 起 CLI + 日志),见设计 §3 fail-closed 分层。
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ModelEffortError {
    /// 该 CLI 没有独立 effort 轴(cursor tier 烤进 model 名;Other 连 model 都不下发)。
    #[error("{}", summon_i18n::format!("rcef46ba54f", cli = .cli))]
    EffortUnsupported { cli: &'static str },
    /// effort 值不在该 CLI 的合法集内。
    #[error("{}", summon_i18n::format!("rcfd3306144", cli = .cli, value = .value, allowed = .allowed))]
    InvalidEffort {
        cli: &'static str,
        value: String,
        allowed: String,
    },
}

/// 本 driver 在某捕获模式下如何压缩上下文。压缩是**交互式 slash 命令**(claude `/compact`、
/// cursor `/compress`),不是无头 flag——往 `-p` 里塞 `/compress` 会被当字面文本喂给模型、
/// 不触发压缩。故 headless 子进程无可投递通道,返回 `Unsupported`,由 CLI 自管自动压缩。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactSpec {
    /// 交互式投递:往 tmux 会话发该 slash 命令并等 turn-end。
    Tmux { command: &'static str },
    /// 无交互 REPL 可投递(headless 子进程)→ 不假装压缩。
    Unsupported,
}

/// 面板「切模型 / effort」把新值写进 agent.yaml 之后,如何让**现存会话**用上它
/// (按 CLI × 捕获模式分流,与 `CompactSpec` 同型的单一发散点)。写盘恒发生(未来冷启天然一致),
/// 这里只决定热路径——目标是**尽量不丢上下文**:
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelSwitchSpec {
    /// 交互式 CLI(claude tmux):往会话注入 `<prefix> <model>`(如 `/model claude-opus-4-8`)
    /// 就地切换,进程/上下文全保留。
    TmuxSlash { prefix: &'static str },
    /// 无头子进程(cursor/codex):model/effort 本就是**每轮** spawn 的 flag,chat 线程就地
    /// 重读配置即可,下一轮 resume 同一会话自带新值——不杀任何东西、上下文保留。
    NextTurn,
    /// 没有热切换通道(未知 CLI / claude 的 effort 是仅启动期 flag、无 slash 可投)→ 退回
    /// 快重置(清 `.session`、杀会话,下次冷启生效)。唯一会丢上下文的路,fail-safe 兜底。
    Restart,
}

/// 冷启动时如何为新会话取得一个可日后 resume 的原生 session id。各 CLI 形态不同:
/// - `PreMintUuid`:我们自己生成 UUID,启动命令直接带上(claude `--session-id`)。
/// - `CreateChat`:先跑一条子命令建空会话拿 id,再 resume 进它(cursor `create-chat`)。
/// - `CaptureRollout`:启动命令不带 id,id 由 CLI 自己生成,启动后回捞其转录文件名(codex)。
/// - `Unsupported`:该 CLI 不支持按 id resume,不记录、不续接(Other)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdAcquire {
    PreMintUuid,
    CreateChat,
    CaptureRollout,
    Unsupported,
}

impl CliDriver {
    pub fn from_kind(kind: &str) -> Self {
        match kind {
            "claude" => CliDriver::Claude,
            "cursor" | "agent" => CliDriver::Cursor,
            "codex" => CliDriver::Codex,
            // traecli 是安装脚本的别名入口,同认。
            "traex" | "traecli" => CliDriver::Traex,
            _ => CliDriver::Other,
        }
    }

    /// CLI 私有家目录:配置、rollout、catalog 的根。
    /// - codex:`$CODEX_HOME` 或 `~/.codex`(尊重环境变量,codex 官方支持)。
    /// - traex:恒为 `~/.trae` —— **不认 `CODEX_HOME`**(2026-08-18 实测:设了该变量,
    ///   app-server initialize 响应的 codexHome 仍指向 `~/.trae`),也没有自己的 HOME 环境变量。
    /// - claude/cursor/Other:无此概念(各有独立存储,不走这里)→ None。
    pub fn private_home(self) -> Option<std::path::PathBuf> {
        match self {
            CliDriver::Codex => Some(
                std::env::var_os("CODEX_HOME")
                    .map(std::path::PathBuf::from)
                    .or_else(|| dirs::home_dir().map(|home| home.join(".codex")))?,
            ),
            CliDriver::Traex => dirs::home_dir().map(|home| home.join(".trae")),
            _ => None,
        }
    }

    /// 家目录下的主配置文件名。codex = `config.toml`;traex = `traecli.toml`(fork 改了名)。
    pub fn config_filename(self) -> Option<&'static str> {
        match self {
            CliDriver::Codex => Some("config.toml"),
            CliDriver::Traex => Some("traecli.toml"),
            _ => None,
        }
    }

    /// rollout 会话存储相对家目录的子目录。codex = `sessions`;traex = `cli/sessions`
    /// (2026-08-18 真机 thread/start 响应 path = `~/.trae/cli/sessions/YYYY/MM/DD/rollout-*.jsonl`)。
    pub fn sessions_subdir(self) -> Option<&'static str> {
        match self {
            CliDriver::Codex => Some("sessions"),
            CliDriver::Traex => Some("cli/sessions"),
            _ => None,
        }
    }

    pub fn default_cmd(self) -> &'static str {
        match self {
            CliDriver::Claude => claude::default_cmd(),
            CliDriver::Cursor => cursor::default_cmd(),
            CliDriver::Codex => codex::default_cmd(),
            CliDriver::Traex => "traex",
            CliDriver::Other => common::default_cmd(),
        }
    }

    /// 冷启首轮的 PRIMER 注入决策(A3,纯函数)——此前这条 per-CLI 知识以四份手写分支散落
    /// (chat 无头 / task run / worker / seed),每新增一个 CLI 都要各改一遍。语义:
    /// - resume 轮一律不重注:PRIMER 已在会话上下文,且 argv/prompt 字节稳定 → provider 缓存命中;
    /// - Claude 且 `prefer_file`(调用方有 Session 持温重用的 primer 文件,即 chat 无头路)→
    ///   走 `--append-system-prompt-file`(返回 use_file=true,prompt 不动);
    /// - 其余(cursor/codex 无对应 flag;task/worker 一次性路径不开文件)→ PRIMER 前置进首条 prompt;
    /// - 无 primer / Other → 原样。
    ///
    /// 返回 `(use_sys_prompt_file, effective_prompt)`;文件路径本身由调用方解析(IO 不进纯件)。
    pub fn compose_first_prompt(
        self,
        primer: Option<&str>,
        prompt: &str,
        has_resume: bool,
        prefer_file: bool,
    ) -> (bool, String) {
        let primer = primer.map(str::trim).filter(|p| !p.is_empty());
        if has_resume || primer.is_none() {
            return (false, prompt.to_string());
        }
        match self {
            CliDriver::Claude if prefer_file => (true, prompt.to_string()),
            CliDriver::Claude | CliDriver::Cursor | CliDriver::Codex | CliDriver::Traex => {
                (false, format!("{}\n\n{prompt}", primer.unwrap()))
            }
            CliDriver::Other => (false, prompt.to_string()),
        }
    }

    /// 冷启首条种子指令(agent.yaml 无 `runtime.seed` 覆盖时的默认):claude 有 `/summon`
    /// 斜杠命令;其余 CLI 读 SOUL/memory 文件以该身份行事。从 registry 的裸字符串 match 收口至此。
    /// 冷启 seed。`None` = 不发种子、首条用户消息直接进会话。
    ///
    /// **Claude 曾经发 `/summon <alias>`,2026-08-08 去掉了。** 那一轮让模型现场去读
    /// SOUL/memory/history 再自我介绍一遍,而 daemon 拼的 PRIMER(`compose_chat_primer`)
    /// 已带上 SOUL 与记忆索引,history 不注入 —— 等于花一整轮模型时间重算一份已知答案。实测代价:arcadia
    /// 冷启 08:44:01 起会话 → 08:44:42 才开始处理用户消息,41 秒全在 seed 上(且它是最轻的
    /// agent);重 agent 更久,久到代码里得把 stall 窗口放宽到 300s 才不误判卡死。
    ///
    /// 其余 CLI 的 seed 保留:走 tmux 的非 capture 路(codex/Other)靠这一行触发会话初始化,
    /// codex 的 rollout 要有一轮才落盘、`capture_codex_id` 才捞得到。今天没有 agent 用这条路,
    /// 无收益的改动不做。`runtime.seed` 显式覆盖对所有 CLI 依然有效。
    pub fn default_seed(self, _agent_name: &str, agent_dir_display: &str) -> Option<String> {
        match self {
            CliDriver::Claude => None,
            _ => Some(summon_i18n::format!("r6cc28be6fb")),
        }
    }

    /// 交互式(tmux)启动时附加的权限 flag。claude 走 `--permission-mode bypassPermissions`,与无头
    /// (`headless_args`)一致——否则交互路的 claude 跑默认权限策略,工具(Bash/Edit)会弹「Do you
    /// want to proceed?」确认菜单卡住整轮(review #4:此前只 headless 加、交互路漏了)。其余 CLI 无。
    pub fn interactive_permission_args(self) -> &'static [&'static str] {
        match self {
            CliDriver::Claude => &["--permission-mode", "bypassPermissions"],
            _ => &[],
        }
    }

    pub fn captures(self) -> bool {
        match self {
            CliDriver::Claude => claude::captures(),
            CliDriver::Cursor => cursor::captures(),
            CliDriver::Codex | CliDriver::Traex | CliDriver::Other => common::captures(),
        }
    }

    /// 该 CLI 是否有已实现的捕获路径(可自动应答)。claude/cursor 走 Transcript、codex 走
    /// Subprocess 都能自动捕获;只有 Other(未支持的 CLI)没有路径。区别于 `captures()`
    /// (历史含义=「有 TUI 抓屏器」,codex=false 已失真,不能再当自动应答门控用)。
    pub fn auto_captures(self) -> bool {
        !matches!(self, CliDriver::Other)
    }

    /// 冷启动时如何取得可 resume 的原生 session id(见 `IdAcquire`)。
    pub fn id_acquisition(self) -> IdAcquire {
        match self {
            CliDriver::Claude => IdAcquire::PreMintUuid,
            CliDriver::Cursor => IdAcquire::CreateChat,
            CliDriver::Codex | CliDriver::Traex => IdAcquire::CaptureRollout,
            CliDriver::Other => IdAcquire::Unsupported,
        }
    }

    /// 卡片与面板是否把账号额度显示成 5 小时与周两个窗口(claude 的 statusLine、codex 的
    /// rate-limit)。其余 CLI 只显示一个长窗,标签用 `quota` 而不是 `7d`:Cursor 的长窗是计费周期。
    pub fn has_five_hour_and_weekly_quota(self) -> bool {
        matches!(self, CliDriver::Claude | CliDriver::Codex)
    }

    /// 是否支持按 id resume(= 会记录/续接会话)。
    pub fn supports_resume(self) -> bool {
        !matches!(self.id_acquisition(), IdAcquire::Unsupported)
    }

    /// 续接已有会话的完整启动命令。`base` 是 agent 解析出的启动二进制(可能带 cli_cmd 覆盖的参数)。
    pub fn resume_cmd(self, base: &str, id: &str) -> String {
        match self {
            CliDriver::Claude => claude::resume_cmd(base, id),
            CliDriver::Cursor => cursor::resume_cmd(base, id),
            CliDriver::Codex | CliDriver::Traex => codex::resume_cmd(base, id),
            CliDriver::Other => base.to_string(),
        }
    }

    /// 冷启动但用一个我们预先确定的 id 起新会话(`PreMintUuid` / `CreateChat` 用)。
    pub fn fresh_cmd_with_id(self, base: &str, id: &str) -> Option<String> {
        match self {
            CliDriver::Claude => Some(claude::fresh_cmd_with_id(base, id)),
            CliDriver::Cursor => Some(cursor::fresh_cmd_with_id(base, id)),
            CliDriver::Codex | CliDriver::Traex | CliDriver::Other => None,
        }
    }

    /// 跨群 fork:从 `parent` 复制一份历史,落到预铸的 `new_id` 上,启动命令走 TUI 形态。
    ///
    /// 只有 claude 走这条 —— 它是唯一「历史复制」与「tmux 交互」能在一条 argv 里同时成立的
    /// provider。codex/traex 的 fork 是 RPC 方法(`thread/fork`),不经启动命令;cursor 没有
    /// fork 原语(判据在 `fork::provider_can_fork`)。
    pub fn fork_cmd_with_id(self, base: &str, parent: &str, new_id: &str) -> Option<String> {
        match self {
            CliDriver::Claude => Some(claude::fork_cmd_with_id(base, parent, new_id)),
            CliDriver::Cursor | CliDriver::Codex | CliDriver::Traex | CliDriver::Other => None,
        }
    }

    /// 启动期把 system prompt 以**文件**注入的 flag(随每个 turn 自动生效、不污染对话历史)。
    pub fn system_prompt_file_flag(self) -> Option<&'static str> {
        match self {
            CliDriver::Claude => claude::system_prompt_file_flag(),
            CliDriver::Cursor | CliDriver::Codex | CliDriver::Traex | CliDriver::Other => {
                common::system_prompt_file_flag()
            }
        }
    }

    /// 指定模型的 flag(agent.yaml 顶层 `model` 经此传给 CLI)。
    pub fn model_flag(self) -> Option<&'static str> {
        match self {
            CliDriver::Cursor => cursor::model_flag(),
            CliDriver::Claude | CliDriver::Codex | CliDriver::Traex | CliDriver::Other => {
                common::supports_model()
            }
        }
    }

    /// 把 agent 的逻辑 (model, effort) 解析成该 CLI 的启动 argv 片段(`LaunchModel`)。
    /// 这是**唯一**收口所有 CLI 差异的解析点:tmux 路径与 headless 路径都调它。
    /// 入参:model 仍是 per-CLI 私有 token(可空=CLI 默认);effort 是规范逻辑档位(可空=不指定)。
    /// 非法组合返回 `Err`,由调用方在 bind/卡片写入前拦截、运行期 fail-closed(降级不 panic)。
    pub fn resolve_launch(
        self,
        model: Option<&str>,
        effort: Option<&str>,
    ) -> Result<LaunchModel, ModelEffortError> {
        match self {
            CliDriver::Claude => claude::resolve_launch(model, effort),
            CliDriver::Cursor => cursor::resolve_launch(model, effort),
            CliDriver::Codex => codex::resolve_launch(model, effort),
            // traex 复用 codex 的 flag 体系;错误文案里的 CLI 名改回 traex,
            // 否则非法 effort 的回执会告诉用户「CLI codex 非法」(张冠李戴)。
            CliDriver::Traex => codex::resolve_launch(model, effort).map_err(|e| match e {
                ModelEffortError::InvalidEffort { value, allowed, .. } => {
                    ModelEffortError::InvalidEffort {
                        cli: "traex",
                        value,
                        allowed,
                    }
                }
                other => other,
            }),
            CliDriver::Other => common::resolve_launch(model, effort),
        }
    }

    /// `resolve_launch` 的**降级**版:非法 (model, effort) 不报错而是丢掉 effort 重试 model-only,
    /// 仍失败(如 cursor/Other 连 model 都不接受 effort)则退到空片段。launch 路径用它实现
    /// 「load/reread 永不因坏值 panic、坏 effort 起 CLI 时静默忽略」(设计 §3 fail-closed)。
    /// 返回 `(args, dropped_effort)`:`dropped_effort` 为 true 时调用方可日志告警(core 不直接打日志)。
    pub fn resolve_launch_lenient(
        self,
        model: Option<&str>,
        effort: Option<&str>,
    ) -> (Vec<String>, bool) {
        match self.resolve_launch(model, effort) {
            Ok(lm) => (lm.args, false),
            // effort 非法/不支持:丢 effort 重试 model-only(model 自身解析不该再失败)。
            Err(_) => {
                let args = self
                    .resolve_launch(model, None)
                    .map(|lm| lm.args)
                    .unwrap_or_default();
                (args, effort.is_some())
            }
        }
    }

    /// 该 CLI 自己接受的模型别名(不在候选目录里也认)。只有 claude 有。
    pub fn model_aliases(self) -> &'static [&'static str] {
        match self {
            CliDriver::Claude => claude::MODEL_ALIASES,
            CliDriver::Codex | CliDriver::Traex | CliDriver::Cursor | CliDriver::Other => &[],
        }
    }

    /// 该 CLI 的独立 effort 合法集(给 picker/校验复用)。cursor/Other 无独立 effort 轴 → 空。
    pub fn legal_efforts(self) -> &'static [&'static str] {
        match self {
            CliDriver::Claude => claude::EFFORTS,
            CliDriver::Codex | CliDriver::Traex => codex::EFFORTS,
            CliDriver::Cursor | CliDriver::Other => &[],
        }
    }

    /// 给 bind/onboard 的 cursor model picker 用:把 base model + 逻辑 tier(+thinking/+fast)
    /// 拼成 cursor 的 model id。**这是 bind 时的扁平化**——产物写进 agent.yaml,运行期只验串、不再拼。
    /// family 后缀规则(纯数据,见设计 §2.2):
    /// - opus 家族:`{base}[-thinking]-{tier}[-fast]`,thinking 在 tier 前、fast 在最后。
    /// - GPT-5.5:`xhigh` 映射成历史命名 `extra-high`;GPT-5.6 与其余档位原样后缀。
    /// - 无 tier 轴的 family(composer/grok/gemini/auto/kimi):忽略 tier,返回 base(thinking/fast
    ///   也不挂——这些 family 没有这些修饰符)。
    pub fn cursor_compose_model(
        base: &str,
        tier: Option<&str>,
        thinking: bool,
        fast: bool,
    ) -> String {
        cursor::compose_model(base, tier, thinking, fast)
    }

    pub fn clear_input_before_send(self) -> bool {
        match self {
            CliDriver::Cursor => cursor::clear_input_before_send(),
            CliDriver::Claude | CliDriver::Codex | CliDriver::Traex | CliDriver::Other => {
                common::clear_input_before_send()
            }
        }
    }

    /// 注入这个 CLI 的 TUI 时是否用 bracketed paste 代替逐键 send-keys。差异留在 driver 里:
    /// 只有被真机证过「粘贴语义收得全、且不改变命令识别」的 CLI 才开(见 `claude::injects_by_paste`)。
    pub fn injects_by_paste(self) -> bool {
        match self {
            CliDriver::Claude => claude::injects_by_paste(),
            CliDriver::Cursor | CliDriver::Codex | CliDriver::Traex | CliDriver::Other => {
                common::injects_by_paste()
            }
        }
    }

    /// 交互式 tmux 会话打断当前生成所需的按键。该差异必须留在 driver 内，调用方不硬编码。
    ///
    /// claude 曾经单独用 `C-c`，代码里没留下选择它的理由，而两处注释自陈这条路「真机待验」
    /// (`transcript.rs` 的 poll_interrupt 分支、`session/followup.rs`)。claude TUI 打断当前生成的
    /// 键实际是 Esc，单次 `C-c` 更接近「清空输入行」。2026-08-07 统一成 `Escape`，四种 CLI 同键。
    pub fn tmux_interrupt_key(self) -> &'static str {
        match self {
            CliDriver::Claude
            | CliDriver::Cursor
            | CliDriver::Codex
            | CliDriver::Traex
            | CliDriver::Other => common::interrupt_key(),
        }
    }

    /// 是否支持无头进程组的中途打断/select 驱动。显式 `claude + headless:true` 也必须命中，
    /// 不能由调用方只列 Cursor/Codex 而静默失去暂停与 stall 看门狗。
    pub fn supports_headless_interrupt(self) -> bool {
        matches!(
            self,
            CliDriver::Claude | CliDriver::Cursor | CliDriver::Codex | CliDriver::Traex
        )
    }

    /// mid-turn steer 时是否真的发送中断键(claude 走 tmux Esc;`!` 打断已于 2026-08-20 删)。
    pub fn interrupts_followup(self) -> bool {
        match self {
            CliDriver::Claude => claude::interrupts_followup(),
            CliDriver::Cursor => cursor::interrupts_followup(),
            CliDriver::Codex | CliDriver::Traex => codex::interrupts_followup(),
            CliDriver::Other => false,
        }
    }

    /// 发完中断键后,等 agent 真正收手再注入跟进的宽限。
    pub fn interrupt_grace(self) -> std::time::Duration {
        common::interrupt_grace()
    }

    pub fn startup_grace(self) -> std::time::Duration {
        match self {
            CliDriver::Cursor => cursor::startup_grace(),
            CliDriver::Claude | CliDriver::Codex | CliDriver::Traex | CliDriver::Other => {
                common::startup_grace()
            }
        }
    }

    /// 无头一轮启动 flag(不含 `-p <prompt>`)。Claude + Cursor 支持,Codex/Other 返回 None。
    pub fn headless_args(
        self,
        resume: Option<&str>,
        sys_prompt_file: Option<&str>,
    ) -> Option<Vec<String>> {
        match self {
            CliDriver::Claude => Some(claude::headless_args(resume, sys_prompt_file)),
            CliDriver::Cursor => Some(cursor::headless_args(resume, sys_prompt_file)),
            CliDriver::Codex | CliDriver::Traex | CliDriver::Other => None,
        }
    }

    /// 无头一轮的**完整**启动命令 `(program, full_args)`,prompt 已就位(各 CLI 摆法不同):
    /// - Claude/Cursor:prompt 走 `-p <prompt>`,其余 flag 复用 `headless_args`,再接
    ///   `resolve_launch`(claude `--model`/`--effort`、cursor `--model`)的片段;
    /// - Codex:`codex exec --json [-m <m>] [-c <config>] [resume <id>] <prompt>`,prompt 是末位位置
    ///   参数,`--json` 与所有 `-m`/`-c` 都是 exec 级 flag(故须在 `resume` 子命令**之前**);
    ///   reasoning summary 与 service tier 由调用方解析后经对应的 `LaunchOpts` 字段条件注入。
    ///
    /// model/effort 经 `resolve_launch_lenient` 落地:坏 effort 不 panic、丢 effort 降级(见设计 §3)。
    /// Other 不支持无头 → None。
    pub fn headless_command(
        self,
        prompt: &str,
        opts: &LaunchOpts,
    ) -> Option<(String, Vec<String>)> {
        let (model_args, _dropped) = self.resolve_launch_lenient(opts.model, opts.effort);
        match self {
            CliDriver::Claude | CliDriver::Cursor => {
                let mut args = vec!["-p".to_string(), prompt.to_string()];
                args.extend(self.headless_args(opts.resume, opts.sys_prompt_file)?);
                args.extend(model_args);
                Some((self.default_cmd().to_string(), args))
            }
            CliDriver::Codex | CliDriver::Traex => {
                let mut model_args = model_args;
                if opts.codex_skip_git_repo_check {
                    model_args.insert(0, "--skip-git-repo-check".to_string());
                }
                // summary 注入值已由调用方按优先级解析好(None=不注入);仍是 exec 级 flag,
                // headless_command_args 会把它整体排在 resume 子命令之前。
                if let Some(s) = opts.codex_reasoning_summary {
                    model_args.push("-c".to_string());
                    model_args.push(format!("model_reasoning_summary={s}"));
                }
                if let Some(tier) = opts.codex_service_tier {
                    model_args.extend(tier.exec_args().iter().map(|arg| (*arg).to_string()));
                }
                Some((
                    self.default_cmd().to_string(),
                    codex::headless_command_args(prompt, opts.resume, &model_args),
                ))
            }
            CliDriver::Other => None,
        }
    }

    /// 按 driver 选无头 stdout 的逐行解析器:claude/cursor 共用 stream-json,codex 用自己的 JSONL。
    pub fn parse_headless_line(self, line: &str) -> Vec<crate::parse::agent_msg::AgentMessage> {
        match self {
            CliDriver::Claude | CliDriver::Cursor => {
                crate::parse::stream_json::parse_stream_json_line(line)
            }
            CliDriver::Codex | CliDriver::Traex => {
                crate::parse::stream_json::parse_codex_line(line)
            }
            CliDriver::Other => vec![],
        }
    }

    /// 本 driver 在给定捕获模式下如何压缩上下文。命令字符串 + 可否投递的差异全收在此。
    pub fn compact_spec(self, mode: CaptureMode) -> CompactSpec {
        match (self, mode) {
            (CliDriver::Claude, CaptureMode::Transcript) => CompactSpec::Tmux {
                command: claude::compact_command(),
            },
            // cursor 若将来跑 tmux 交互:发 /compress(当前默认走 CliSession,落不到这条)
            (CliDriver::Cursor, CaptureMode::Transcript) => CompactSpec::Tmux {
                command: cursor::compact_command(),
            },
            // headless 子进程(cursor/codex):上下文由**引擎/模型自身在接近窗口上限时自动压缩**
            // (codex `Feature::AutoCompaction` 默认开、跑在共享 turn loop;cursor Composer 模型有
            // 训练出的 self-summarization),且**没有可靠的非交互手动触发通道**——codex 的 `/compact`
            // 仅交互 `Op::Compact`、非 exec flag;cursor 的 `/summarize` 在 `-p` 下能否生效未证实。
            // 故 Unsupported:诚实回执「CLI 自管、无手动通道」,不假装能压(调研 2026-06)。
            //
            // Rpc **显式列在这里而不是落 catch-all**:app-server 确实有 `thread/compact/start`,
            // 但接它是独立一步(新请求方法 + 终态语义 + 回执文案),没接之前回 Unsupported 才诚实。
            (_, CaptureMode::Rpc | CaptureMode::CliSession) => CompactSpec::Unsupported,
            _ => CompactSpec::Unsupported,
        }
    }

    /// 切**模型**的热路径(见 [`ModelSwitchSpec`])。claude 跑 tmux 有 `/model <m>` slash 可注入,
    /// 就地切、上下文保留;无头每轮 spawn 自带 `--model`/`-m`,重读配置即可;其余保守重置。
    pub fn model_switch_spec(self, mode: CaptureMode) -> ModelSwitchSpec {
        match (self, mode) {
            (CliDriver::Claude, CaptureMode::Transcript) => {
                ModelSwitchSpec::TmuxSlash { prefix: "/model" }
            }
            // Rpc 与 Subprocess 同臂,**必须显式写出**:落到下面的 catch-all 就是 `Restart`
            // (杀会话冷启 = 丢上下文),而 RPC 恰恰是最不该重启的那条路(设计 §8 点名的风险)。
            // 语义也确实相同:`TurnStartParams` 每轮可带 `model`,下一轮 `turn/start` 自带新值,
            // 进程与线程上下文全保留 —— 与「每轮 spawn 自带 `-m`」对用户是同一件事。
            (_, CaptureMode::Rpc | CaptureMode::CliSession) => ModelSwitchSpec::NextTurn,
            _ => ModelSwitchSpec::Restart,
        }
    }

    /// 切 **effort** 的热路径,与 model 分开:claude REPL 有 `/effort <档位>` slash(与 `/model` 同理)
    /// → 注入就地切、上下文保留(2026-06-15 修正:此前误判 claude effort 只是启动 flag、要重置冷启;
    /// 实际可 `/effort` 就地切。注:某些模型 `/effort` 后 claude 会弹「重算缓存」确认框,见交互菜单中介)。
    /// codex 的 effort 是每轮 `-c model_reasoning_effort` → 下一轮自带;cursor 无独立 effort 轴(validate 已挡)。
    pub fn effort_switch_spec(self, mode: CaptureMode) -> ModelSwitchSpec {
        match (self, mode) {
            (CliDriver::Claude, CaptureMode::Transcript) => {
                ModelSwitchSpec::TmuxSlash { prefix: "/effort" }
            }
            // 同 `model_switch_spec`:Rpc 的 effort 走 `TurnStartParams.effort`,每轮生效、不重启。
            (_, CaptureMode::Rpc | CaptureMode::CliSession) => ModelSwitchSpec::NextTurn,
            _ => ModelSwitchSpec::Restart,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compose_first_prompt_per_cli_and_resume() {
        // resume 轮一律不重注(缓存友好)。
        for d in [CliDriver::Claude, CliDriver::Cursor, CliDriver::Codex] {
            assert_eq!(
                d.compose_first_prompt(Some("P"), "hi", true, true),
                (false, "hi".to_string()),
                "resume 轮不注 PRIMER"
            );
        }
        // claude + 文件能力 → 走 sys-prompt 文件,prompt 不动。
        assert_eq!(
            CliDriver::Claude.compose_first_prompt(Some("P"), "hi", false, true),
            (true, "hi".to_string())
        );
        // claude 无文件能力(task/worker 一次性路径)→ 前置。
        assert_eq!(
            CliDriver::Claude.compose_first_prompt(Some("P"), "hi", false, false),
            (false, "P\n\nhi".to_string())
        );
        // cursor/codex 无对应 flag → 前置(即便 prefer_file)。
        assert_eq!(
            CliDriver::Cursor.compose_first_prompt(Some("P"), "hi", false, true),
            (false, "P\n\nhi".to_string())
        );
        // 空/缺 primer 与 Other → 原样。
        assert_eq!(
            CliDriver::Codex.compose_first_prompt(Some("  "), "hi", false, true),
            (false, "hi".to_string())
        );
        assert_eq!(
            CliDriver::Other.compose_first_prompt(Some("P"), "hi", false, false),
            (false, "hi".to_string())
        );
    }

    #[test]
    fn default_seed_per_cli() {
        // claude:不发 seed。身份由 PRIMER 注入,不再派一轮模型去读 SOUL/memory 自我介绍。
        assert_eq!(CliDriver::Claude.default_seed("nimbus", "/d"), None);
        // 其余(走 tmux 的非 capture 路)保留:codex 靠这一行让会话跑起来才落 rollout。
        assert!(CliDriver::Codex
            .default_seed("nimbus", "/d")
            .unwrap()
            .contains("/d/SOUL.md"));
    }

    #[test]
    fn model_switch_spec_routes_by_cli_and_capture_mode() {
        // claude(Transcript/tmux)→ 注入 /model 就地切,上下文保留。
        assert_eq!(
            CliDriver::Claude.model_switch_spec(CaptureMode::Transcript),
            ModelSwitchSpec::TmuxSlash { prefix: "/model" }
        );
        // 无头(cursor/codex)→ 每轮 transport 自带新参数,下一轮生效,不重置。
        assert_eq!(
            CliDriver::Cursor.model_switch_spec(CaptureMode::Rpc),
            ModelSwitchSpec::NextTurn
        );
        assert_eq!(
            CliDriver::Codex.model_switch_spec(CaptureMode::Rpc),
            ModelSwitchSpec::NextTurn
        );
        // 不认识的组合 → 保守快重置(fail-safe)。
        assert_eq!(
            CliDriver::Other.model_switch_spec(CaptureMode::Transcript),
            ModelSwitchSpec::Restart
        );
    }

    #[test]
    fn effort_switch_spec_claude_slash_codex_next_turn() {
        // claude REPL 有 /effort slash → 就地切(2026-06-15 修正:此前误判要重置冷启)。
        assert_eq!(
            CliDriver::Claude.effort_switch_spec(CaptureMode::Transcript),
            ModelSwitchSpec::TmuxSlash { prefix: "/effort" }
        );
        // codex 的 effort 是每轮 -c flag → 下一轮自带。
        assert_eq!(
            CliDriver::Codex.effort_switch_spec(CaptureMode::Rpc),
            ModelSwitchSpec::NextTurn
        );
    }

    #[test]
    fn resolve_launch_claude_model_only_and_model_plus_effort() {
        // 只 model:仅 --model
        let lm = CliDriver::Claude
            .resolve_launch(Some("claude-opus-4-8"), None)
            .unwrap();
        assert_eq!(lm.args, vec!["--model", "claude-opus-4-8"]);
        // model + effort:两个正交 flag
        let lm = CliDriver::Claude
            .resolve_launch(Some("claude-opus-4-8"), Some("high"))
            .unwrap();
        assert_eq!(
            lm.args,
            vec!["--model", "claude-opus-4-8", "--effort", "high"]
        );
        // 只 effort:仅 --effort
        let lm = CliDriver::Claude.resolve_launch(None, Some("max")).unwrap();
        assert_eq!(lm.args, vec!["--effort", "max"]);
        // 都没有:空片段
        assert!(CliDriver::Claude
            .resolve_launch(None, None)
            .unwrap()
            .args
            .is_empty());
    }

    #[test]
    fn resolve_launch_claude_rejects_illegal_effort() {
        // none/minimal 不在 claude 合法集
        let err = CliDriver::Claude
            .resolve_launch(None, Some("minimal"))
            .unwrap_err();
        match err {
            ModelEffortError::InvalidEffort {
                cli,
                value,
                allowed,
            } => {
                assert_eq!(cli, "claude");
                assert_eq!(value, "minimal");
                assert_eq!(allowed, "low/medium/high/xhigh/max");
            }
            other => panic!("期望 InvalidEffort,实际 {other:?}"),
        }
    }

    #[test]
    fn resolve_launch_codex_uses_config_key_for_effort() {
        // 只 model:走 -m
        let lm = CliDriver::Codex
            .resolve_launch(Some("gpt-5.5-codex"), None)
            .unwrap();
        assert_eq!(lm.args, vec!["-m", "gpt-5.5-codex"]);
        // model + effort:effort 是 -c config 覆盖(TOML 裸词,无引号)
        let lm = CliDriver::Codex
            .resolve_launch(Some("gpt-5.5-codex"), Some("high"))
            .unwrap();
        assert_eq!(
            lm.args,
            vec!["-m", "gpt-5.5-codex", "-c", "model_reasoning_effort=high"]
        );
        // codex 接受 none/minimal(比 claude 多)
        let lm = CliDriver::Codex.resolve_launch(None, Some("none")).unwrap();
        assert_eq!(lm.args, vec!["-c", "model_reasoning_effort=none"]);

        // GPT-5.6 新增 max；仍走同一条 Codex config override。
        let lm = CliDriver::Codex
            .resolve_launch(Some("gpt-5.6-sol"), Some("max"))
            .unwrap();
        assert_eq!(
            lm.args,
            vec!["-m", "gpt-5.6-sol", "-c", "model_reasoning_effort=max"]
        );
    }

    #[test]
    fn resolve_launch_codex_rejects_illegal_effort() {
        // 未知档位仍须 fail-closed。
        let err = CliDriver::Codex
            .resolve_launch(None, Some("warp"))
            .unwrap_err();
        match err {
            ModelEffortError::InvalidEffort {
                cli,
                value,
                allowed,
            } => {
                assert_eq!(cli, "codex");
                assert_eq!(value, "warp");
                assert_eq!(allowed, "none/minimal/low/medium/high/xhigh/max/ultra");
            }
            other => panic!("期望 InvalidEffort,实际 {other:?}"),
        }
    }

    #[test]
    fn resolve_launch_cursor_model_only_ok_but_effort_unsupported() {
        // cursor:只 model → 仅 --model(tier 已烤进 model 名)
        let lm = CliDriver::Cursor
            .resolve_launch(Some("gpt-5.5-high"), None)
            .unwrap();
        assert_eq!(lm.args, vec!["--model", "gpt-5.5-high"]);
        // 无 model 无 effort:空片段(用 CLI 默认)
        assert!(CliDriver::Cursor
            .resolve_launch(None, None)
            .unwrap()
            .args
            .is_empty());
        // cursor + effort → EffortUnsupported(tier 烤进 model 名,无独立 effort 轴)
        let err = CliDriver::Cursor
            .resolve_launch(Some("gpt-5.5"), Some("high"))
            .unwrap_err();
        assert!(matches!(
            err,
            ModelEffortError::EffortUnsupported { cli: "cursor" }
        ));
    }

    #[test]
    fn resolve_launch_other_no_model_and_effort_unsupported() {
        // Other 连 model 都不下发
        assert!(CliDriver::Other
            .resolve_launch(Some("whatever"), None)
            .unwrap()
            .args
            .is_empty());
        // Other + effort → EffortUnsupported
        let err = CliDriver::Other
            .resolve_launch(None, Some("low"))
            .unwrap_err();
        assert!(matches!(
            err,
            ModelEffortError::EffortUnsupported { cli: "other" }
        ));
    }

    /// codex 家族(codex / traex)的配置位置**必须各不相同** —— traex 是 fork,家目录改成
    /// `~/.trae`、主配置改名 `traecli.toml`。
    ///
    /// **why 要专门钉一条**:任何按 codex 路径去读 traex 配置的代码都会**静默读错文件或读不到**,
    /// 不报错、不 panic,只是那个功能对 traex 悄悄失效。2026-08-18 就踩了一次:上下文预算的窗口
    /// 读取写死了 `~/.codex/config.toml`,traex agent 建出来后整个 ctx 显示是空的,直到人工核对
    /// 才发现。要读这两个 CLI 的配置,一律问 `private_home()` / `config_filename()` 要路径。
    #[test]
    fn codex_family_config_locations_are_distinct() {
        let codex = CliDriver::Codex.config_filename().expect("codex 有主配置");
        let traex = CliDriver::Traex.config_filename().expect("traex 有主配置");
        assert_ne!(codex, traex, "fork 改了配置文件名,不能共用同一个路径");
        // 家目录同理:值依赖运行环境,只断言两者不相等且都存在。
        if let (Some(ch), Some(th)) = (
            CliDriver::Codex.private_home(),
            CliDriver::Traex.private_home(),
        ) {
            assert_ne!(ch, th, "fork 有自己的家目录");
        }
    }

    #[test]
    fn legal_efforts_per_cli() {
        assert_eq!(
            CliDriver::Claude.legal_efforts(),
            &["low", "medium", "high", "xhigh", "max"]
        );
        // codex 比 claude 多 none/minimal(旧模型)与 ultra(gpt-5.6-sol/terra 公布的最高档)。
        assert_eq!(
            CliDriver::Codex.legal_efforts(),
            &["none", "minimal", "low", "medium", "high", "xhigh", "max", "ultra"]
        );
        // ultra 只有 codex 收:claude 给了要 fail-closed,否则面板会渲染出一个起不来的档位。
        assert!(!CliDriver::Claude.legal_efforts().contains(&"ultra"));
        // cursor/Other 无独立 effort 轴
        assert!(CliDriver::Cursor.legal_efforts().is_empty());
        assert!(CliDriver::Other.legal_efforts().is_empty());
    }

    #[test]
    fn cursor_compose_model_opus_family() {
        // opus + tier
        assert_eq!(
            CliDriver::cursor_compose_model("claude-opus-4-8", Some("high"), false, false),
            "claude-opus-4-8-high"
        );
        // opus + thinking + tier
        assert_eq!(
            CliDriver::cursor_compose_model("claude-opus-4-8", Some("max"), true, false),
            "claude-opus-4-8-thinking-max"
        );
        // opus + tier + fast
        assert_eq!(
            CliDriver::cursor_compose_model("claude-opus-4-8", Some("low"), false, true),
            "claude-opus-4-8-low-fast"
        );
        // opus + thinking + tier + fast(全开)
        assert_eq!(
            CliDriver::cursor_compose_model("claude-opus-4-8", Some("xhigh"), true, true),
            "claude-opus-4-8-thinking-xhigh-fast"
        );
        // 无 tier:base 原样(thinking/fast 仍可挂)
        assert_eq!(
            CliDriver::cursor_compose_model("claude-opus-4-8", None, true, false),
            "claude-opus-4-8-thinking"
        );
    }

    #[test]
    fn cursor_compose_model_gpt_family_uses_versioned_xhigh_suffix() {
        // gpt-5.5:xhigh → extra-high(GPT 家族独有命名)
        assert_eq!(
            CliDriver::cursor_compose_model("gpt-5.5", Some("xhigh"), false, false),
            "gpt-5.5-extra-high"
        );
        // gpt-5.5:其余档位原样
        assert_eq!(
            CliDriver::cursor_compose_model("gpt-5.5", Some("high"), false, false),
            "gpt-5.5-high"
        );

        // GPT-5.6 的 Cursor catalog 恢复使用原生 xhigh，不能沿用 5.5 的特殊拼法。
        assert_eq!(
            CliDriver::cursor_compose_model("gpt-5.6-sol", Some("xhigh"), false, false),
            "gpt-5.6-sol-xhigh"
        );
    }

    #[test]
    fn cursor_compose_model_no_tier_families_return_base() {
        // composer/grok/gemini 无 tier 轴:任何 tier 都忽略,返回 base
        assert_eq!(
            CliDriver::cursor_compose_model("composer-1", Some("high"), false, false),
            "composer-1"
        );
        assert_eq!(
            CliDriver::cursor_compose_model("grok-4", Some("max"), true, true),
            "grok-4"
        );
        assert_eq!(
            CliDriver::cursor_compose_model("gemini-3-pro", None, false, false),
            "gemini-3-pro"
        );
    }

    #[test]
    fn cursor_driver_uses_agent_command_and_captures() {
        let driver = CliDriver::from_kind("cursor");
        assert_eq!(driver.default_cmd(), "agent");
        assert!(driver.captures());

        let alias = CliDriver::from_kind("agent");
        assert_eq!(alias.default_cmd(), "agent");
        assert!(alias.captures());
    }

    #[test]
    fn auto_captures_blocks_only_unsupported_cli() {
        // claude/cursor/codex 都有自动捕获路径(Transcript/CliSession/Rpc),只有 Other 没有。
        assert!(CliDriver::Claude.auto_captures());
        assert!(CliDriver::Cursor.auto_captures());
        assert!(CliDriver::Codex.auto_captures());
        assert!(!CliDriver::Other.auto_captures());
    }

    #[test]
    fn compact_spec_tmux_for_interactive_unsupported_for_headless() {
        // claude tmux 交互 → /compact
        assert_eq!(
            CliDriver::Claude.compact_spec(CaptureMode::Transcript),
            CompactSpec::Tmux {
                command: "/compact"
            }
        );
        // cursor 前瞻:若跑 tmux → /compress
        assert_eq!(
            CliDriver::Cursor.compact_spec(CaptureMode::Transcript),
            CompactSpec::Tmux {
                command: "/compress"
            }
        );
        // headless transport:cursor/codex 都无可投递通道 → Unsupported(不假装压缩)
        assert_eq!(
            CliDriver::Cursor.compact_spec(CaptureMode::Rpc),
            CompactSpec::Unsupported
        );
        assert_eq!(
            CliDriver::Codex.compact_spec(CaptureMode::Rpc),
            CompactSpec::Unsupported
        );
        // headless claude 也是 Subprocess → Unsupported
        assert_eq!(
            CliDriver::Claude.compact_spec(CaptureMode::Rpc),
            CompactSpec::Unsupported
        );
    }

    #[test]
    fn interrupt_policy_is_centralized_per_driver() {
        // 2026-08-07:claude 从 `C-c` 改到 `Escape`(见 `tmux_interrupt_key` 的理由),四种 CLI 同键。
        assert_eq!(CliDriver::Claude.tmux_interrupt_key(), "Escape");
        assert_eq!(CliDriver::Cursor.tmux_interrupt_key(), "Escape");
        assert!(CliDriver::Claude.supports_headless_interrupt());
        assert!(CliDriver::Cursor.supports_headless_interrupt());
        assert!(CliDriver::Codex.supports_headless_interrupt());
        assert!(!CliDriver::Other.supports_headless_interrupt());
    }

    #[test]
    fn cursor_followups_do_not_send_interrupt_key() {
        assert!(CliDriver::Claude.interrupts_followup());
        assert!(!CliDriver::Cursor.interrupts_followup());
    }

    #[test]
    fn id_acquisition_matches_each_cli_strategy() {
        assert_eq!(CliDriver::Claude.id_acquisition(), IdAcquire::PreMintUuid);
        assert_eq!(CliDriver::Cursor.id_acquisition(), IdAcquire::CreateChat);
        assert_eq!(CliDriver::Codex.id_acquisition(), IdAcquire::CaptureRollout);
        assert_eq!(CliDriver::Other.id_acquisition(), IdAcquire::Unsupported);
        assert!(CliDriver::Claude.supports_resume());
        assert!(CliDriver::Cursor.supports_resume());
        assert!(CliDriver::Codex.supports_resume());
        assert!(!CliDriver::Other.supports_resume());
    }

    #[test]
    fn resume_cmd_uses_per_cli_syntax() {
        assert_eq!(
            CliDriver::Claude.resume_cmd("claude", "uuid-1"),
            "claude --resume uuid-1"
        );
        assert_eq!(
            CliDriver::Cursor.resume_cmd("agent", "chat-1"),
            "agent --resume=chat-1"
        );
        assert_eq!(
            CliDriver::Codex.resume_cmd("codex", "uuid-2"),
            "codex resume uuid-2"
        );
        assert_eq!(
            CliDriver::Claude.resume_cmd("claude --model opus", "uuid-3"),
            "claude --model opus --resume uuid-3"
        );
    }

    #[test]
    fn fresh_cmd_with_id_only_for_preassignable_clis() {
        assert_eq!(
            CliDriver::Claude
                .fresh_cmd_with_id("claude", "uuid-1")
                .as_deref(),
            Some("claude --session-id uuid-1")
        );
        assert_eq!(
            CliDriver::Cursor
                .fresh_cmd_with_id("agent", "chat-1")
                .as_deref(),
            Some("agent --resume=chat-1")
        );
        assert_eq!(CliDriver::Codex.fresh_cmd_with_id("codex", "x"), None);
        assert_eq!(CliDriver::Other.fresh_cmd_with_id("claude", "x"), None);
    }

    #[test]
    fn system_prompt_file_flag_only_for_claude() {
        assert_eq!(
            CliDriver::Claude.system_prompt_file_flag(),
            Some("--append-system-prompt-file")
        );
        assert_eq!(CliDriver::Cursor.system_prompt_file_flag(), None);
        assert_eq!(CliDriver::Codex.system_prompt_file_flag(), None);
        assert_eq!(CliDriver::Other.system_prompt_file_flag(), None);
    }

    #[test]
    fn model_flag_enabled_for_cursor_only_for_now() {
        assert_eq!(CliDriver::Cursor.model_flag(), Some("--model"));
        assert_eq!(CliDriver::Claude.model_flag(), None);
        assert_eq!(CliDriver::Codex.model_flag(), None);
        assert_eq!(CliDriver::Other.model_flag(), None);
    }

    #[test]
    fn cursor_input_clear_detects_placeholder_vs_pending() {
        let pending =
            " ▄▄\n  → 在群「值班群」里 张三 @你:「立刻任务」       enter send now\n ▀▀\n  GPT-5.5 · Auto-run\n";
        assert!(!cursor_input_is_clear(pending));
        let idle = " ▄▄\n  → Add a follow-up\n ▀▀\n  GPT-5.5 · Auto-run\n";
        assert!(cursor_input_is_clear(idle));
        let echoed = "  → 用户旧消息已发\n  pong\n ▄▄\n  → Add a follow-up\n ▀▀\n";
        assert!(cursor_input_is_clear(echoed));
        let again =
            "  → 用户旧消息已发\n  pong\n ▄▄\n  → 新输入还在框里       enter send now\n ▀▀\n";
        assert!(!cursor_input_is_clear(again));
        let completed_echo = "  pong\n ▄▄\n  → 用户旧消息已发\n ▀▀\n  GPT-5.5 · Auto-run\n";
        assert!(cursor_input_is_clear(completed_echo));
    }

    #[test]
    fn cursor_ready_tracks_working_line() {
        assert!(!super::cursor::is_cursor_ready(" ⠰⠳ Working\n"));
        assert!(super::cursor::is_cursor_ready(
            "  pong\n  GPT-5.5 1M High · 12% Auto-run\n"
        ));
    }

    #[test]
    fn cursor_ready_tracks_running_line() {
        assert!(!super::cursor::is_cursor_ready(
            " ⠀⠞ Running  20.78k tokens\n"
        ));
    }

    #[test]
    fn cursor_ready_tracks_tool_status_lines() {
        assert!(!super::cursor::is_cursor_ready(
            " ⠘⠆ Grepping  2.15k tokens\n"
        ));
    }

    /// 缓存友好合约:claude resume 轮不传 sys_prompt_file → argv 不含 --append-system-prompt-file;
    /// 冷启传了文件 → argv 含。调用方(daemon stream_turn_headless)负责在 resume.is_none() 时
    /// 才传 sys_prompt_file,driver 层本身已透传,此测试锁住 driver 的行为边界。
    #[test]
    fn claude_headless_command_no_sys_file_on_resume() {
        // resume 轮:sys_prompt_file=None → 不应出现 --append-system-prompt-file
        let (_prog, args) = CliDriver::Claude
            .headless_command(
                "follow up",
                &LaunchOpts {
                    resume: Some("sid-42"),
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(
            !args.iter().any(|a| a == "--append-system-prompt-file"),
            "resume 轮 argv 不应含 --append-system-prompt-file,实际: {args:?}"
        );
        assert!(args.windows(2).any(|w| w == ["--resume", "sid-42"]));
        // 冷启轮:sys_prompt_file=Some → 应含
        let (_prog, args) = CliDriver::Claude
            .headless_command(
                "hello",
                &LaunchOpts {
                    sys_prompt_file: Some("/tmp/primer.txt"),
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(
            args.windows(2)
                .any(|w| w == ["--append-system-prompt-file", "/tmp/primer.txt"]),
            "冷启轮 argv 应含 --append-system-prompt-file,实际: {args:?}"
        );
        assert!(!args.iter().any(|a| a == "--resume"));
    }

    #[test]
    fn claude_headless_args_fresh_and_resume() {
        // 冷启(无 resume):基础 flag
        let fresh = CliDriver::Claude.headless_args(None, None).unwrap();
        assert_eq!(
            fresh,
            vec![
                "--output-format",
                "stream-json",
                "--verbose",
                "--permission-mode",
                "bypassPermissions"
            ]
        );
        // resume + 系统提示文件
        let resumed = CliDriver::Claude
            .headless_args(Some("sid-9"), Some("/tmp/sys.txt"))
            .unwrap();
        assert!(resumed.windows(2).any(|w| w == ["--resume", "sid-9"]));
        assert!(resumed
            .windows(2)
            .any(|w| w == ["--append-system-prompt-file", "/tmp/sys.txt"]));
        // Codex 不支持无头
        assert!(CliDriver::Codex.headless_args(None, None).is_none());
    }

    #[test]
    fn cursor_headless_args_fresh_and_resume() {
        // 冷启(无 resume):基础 flag(含 --stream-partial-output 让打字机按 token 逐块刷新)
        let fresh = CliDriver::Cursor.headless_args(None, None).unwrap();
        assert_eq!(
            fresh,
            vec![
                "--output-format",
                "stream-json",
                "--yolo",
                "--stream-partial-output"
            ]
        );
        // sys_prompt_file 参数不产生任何额外 flag
        let with_sys = CliDriver::Cursor
            .headless_args(None, Some("/tmp/sys.txt"))
            .unwrap();
        assert!(!with_sys.iter().any(|a| a.contains("sys")));
        // resume:单个连体 --resume=<id> arg(cursor 的 = 语法)
        let resumed = CliDriver::Cursor
            .headless_args(Some("chat-42"), None)
            .unwrap();
        assert!(resumed.iter().any(|a| a == "--resume=chat-42"));
        // resume arg 不能拆成两个(cursor 要求 = 连体)
        assert!(!resumed.windows(2).any(|w| w == ["--resume", "chat-42"]));
        // --stream-partial-output 在 resume 时也存在(每轮都需要流式 delta)
        assert!(resumed.iter().any(|a| a == "--stream-partial-output"));
    }

    #[test]
    fn claude_headless_command_fresh_and_resume() {
        // 冷启:program=claude,prompt 走 -p,后接基础 flag
        let (prog, args) = CliDriver::Claude
            .headless_command("say pong", &LaunchOpts::default())
            .unwrap();
        assert_eq!(prog, "claude");
        assert_eq!(args[0], "-p");
        assert_eq!(args[1], "say pong");
        assert!(args
            .windows(2)
            .any(|w| w == ["--output-format", "stream-json"]));
        // resume + sys prompt file 透传为 claude 的 flag
        let (_p, args) = CliDriver::Claude
            .headless_command(
                "hi",
                &LaunchOpts {
                    resume: Some("sid-9"),
                    sys_prompt_file: Some("/tmp/sys.txt"),
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(args.windows(2).any(|w| w == ["--resume", "sid-9"]));
        assert!(args
            .windows(2)
            .any(|w| w == ["--append-system-prompt-file", "/tmp/sys.txt"]));
    }

    #[test]
    fn cursor_headless_command_fresh_and_resume() {
        let (prog, args) = CliDriver::Cursor
            .headless_command("say pong", &LaunchOpts::default())
            .unwrap();
        assert_eq!(prog, "agent");
        assert_eq!(&args[..2], &["-p", "say pong"]);
        assert!(args.iter().any(|a| a == "--yolo"));
        // 每轮(含冷启)都带 --stream-partial-output 让打字机按 token 刷新
        assert!(args.iter().any(|a| a == "--stream-partial-output"));
        // resume 用连体 --resume=<id>
        let (_p, args) = CliDriver::Cursor
            .headless_command(
                "hi",
                &LaunchOpts {
                    resume: Some("chat-42"),
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(args.iter().any(|a| a == "--resume=chat-42"));
        // resume 轮也带 --stream-partial-output
        assert!(args.iter().any(|a| a == "--stream-partial-output"));
    }

    #[test]
    fn codex_headless_command_fresh_and_resume() {
        // 冷启:codex exec --json -- <prompt>(`--` 防用户文本被解析成 flag)
        let (prog, args) = CliDriver::Codex
            .headless_command("say pong", &LaunchOpts::default())
            .unwrap();
        assert_eq!(prog, "codex");
        assert_eq!(args, vec!["exec", "--json", "--", "say pong"]);
        // resume:--json 在 resume 子命令之前,prompt 仍在参数终止符之后
        let (_p, args) = CliDriver::Codex
            .headless_command(
                "follow up",
                &LaunchOpts {
                    resume: Some("tid-9"),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(
            args,
            vec!["exec", "--json", "resume", "tid-9", "--", "follow up"]
        );
    }

    #[test]
    fn codex_service_tier_persisted_values_are_strict() {
        assert_eq!(
            "default".parse::<CodexServiceTier>().unwrap(),
            CodexServiceTier::Standard
        );
        assert_eq!(
            "fast".parse::<CodexServiceTier>().unwrap(),
            CodexServiceTier::Fast
        );
        assert_eq!(CodexServiceTier::Standard.as_persisted_str(), "default");
        assert_eq!(CodexServiceTier::Fast.as_persisted_str(), "fast");

        for invalid in ["", "standard", "priority", "Fast", " fast", "fast\n"] {
            assert!(
                invalid.parse::<CodexServiceTier>().is_err(),
                "不应接受非规范持久值 {invalid:?}"
            );
        }
    }

    #[test]
    fn codex_headless_service_tier_args_precede_resume() {
        let (_program, standard) = CliDriver::Codex
            .headless_command(
                "continue",
                &LaunchOpts {
                    resume: Some("tid-9"),
                    codex_service_tier: Some(CodexServiceTier::Standard),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(
            standard,
            [
                "exec",
                "--json",
                "-c",
                "service_tier=default",
                "resume",
                "tid-9",
                "--",
                "continue"
            ]
        );

        let (_program, fast) = CliDriver::Codex
            .headless_command(
                "continue",
                &LaunchOpts {
                    resume: Some("tid-9"),
                    codex_service_tier: Some(CodexServiceTier::Fast),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(
            fast,
            [
                "exec",
                "--json",
                "-c",
                "features.fast_mode=true",
                "-c",
                "service_tier=fast",
                "resume",
                "tid-9",
                "--",
                "continue"
            ]
        );
    }

    #[test]
    fn codex_agent_can_opt_in_to_skip_git_repo_check() {
        let enabled: crate::registry::Agent = serde_yaml::from_str(
            "name: x\nruntime:\n  cli: codex\n  codex_skip_git_repo_check: true\n",
        )
        .unwrap();
        let (_program, args) = CliDriver::Codex
            .headless_command(
                "inspect workspace",
                &LaunchOpts {
                    codex_skip_git_repo_check: enabled.codex_skip_git_repo_check(),
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(args.iter().any(|arg| arg == "--skip-git-repo-check"));

        let defaulted: crate::registry::Agent =
            serde_yaml::from_str("name: x\nruntime:\n  cli: codex\n").unwrap();
        let (_program, args) = CliDriver::Codex
            .headless_command(
                "inspect workspace",
                &LaunchOpts {
                    codex_skip_git_repo_check: defaulted.codex_skip_git_repo_check(),
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(!args.iter().any(|arg| arg == "--skip-git-repo-check"));
    }

    #[test]
    fn codex_headless_command_injects_reasoning_summary_when_set() {
        // 给了 summary → 注入的 `-c model_reasoning_summary=<v>` 紧跟在 effort 的 -c 之后(均 exec 级)
        let (_p, args) = CliDriver::Codex
            .headless_command(
                "hi",
                &LaunchOpts {
                    model: Some("gpt-5.5"),
                    effort: Some("high"),
                    codex_reasoning_summary: Some("detailed"),
                    ..Default::default()
                },
            )
            .unwrap();
        // effort 的 -c 在前,summary 的 -c 紧随其后(连续两元素)
        let effort_at = args
            .windows(2)
            .position(|w| w == ["-c", "model_reasoning_effort=high"])
            .expect("应含 effort 的 -c");
        assert_eq!(
            &args[effort_at + 2..effort_at + 4],
            ["-c", "model_reasoning_summary=detailed"]
        );
        // 未给 summary → argv 里完全不出现该 key
        let (_p, args) = CliDriver::Codex
            .headless_command(
                "hi",
                &LaunchOpts {
                    model: Some("gpt-5.5"),
                    effort: Some("high"),
                    codex_reasoning_summary: None,
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(!args.iter().any(|a| a.contains("model_reasoning_summary")));
    }

    #[test]
    fn other_driver_has_no_headless_command() {
        assert!(CliDriver::Other
            .headless_command("x", &LaunchOpts::default())
            .is_none());
    }

    /// traex 是 codex fork:全部分派与 codex 同构,只有四点差异(二进制名/家目录/配置文件名/
    /// rollout 子目录)。本测把「同构」钉死,防止以后给 codex 加特性时漏掉 traex。
    #[test]
    fn traex_shares_codex_dispatch_but_differs_in_binary_and_paths() {
        assert_eq!(CliDriver::from_kind("traex"), CliDriver::Traex);
        assert_eq!(CliDriver::from_kind("traecli"), CliDriver::Traex);
        assert_eq!(CliDriver::Traex.default_cmd(), "traex");
        assert_eq!(
            CliDriver::Traex.id_acquisition(),
            IdAcquire::CaptureRollout,
            "与 codex 同一套 rollout 回捞"
        );
        assert_eq!(
            CliDriver::Traex.legal_efforts(),
            CliDriver::Codex.legal_efforts(),
            "effort 合法集与 codex 相同"
        );
        assert!(CliDriver::Traex.supports_headless_interrupt());
        assert!(CliDriver::Traex.interrupts_followup());
        assert_eq!(
            CliDriver::Traex.resume_cmd("traex", "tid-1"),
            "traex resume tid-1"
        );
        // 无头命令形状与 codex 相同,仅程序名不同。
        let (prog, args) = CliDriver::Traex
            .headless_command(
                "follow up",
                &LaunchOpts {
                    resume: Some("tid-9"),
                    model: Some("gpt-5.5"),
                    effort: Some("high"),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(prog, "traex");
        assert_eq!(
            args,
            vec![
                "exec",
                "--json",
                "-m",
                "gpt-5.5",
                "-c",
                "model_reasoning_effort=high",
                "resume",
                "tid-9",
                "--",
                "follow up"
            ]
        );
        // 路径四点差异。
        assert_eq!(
            CliDriver::Traex.config_filename(),
            Some("traecli.toml"),
            "fork 改了配置文件名"
        );
        assert_eq!(
            CliDriver::Traex.sessions_subdir(),
            Some("cli/sessions"),
            "rollout 多一层 cli/(真机 thread/start 响应实测)"
        );
        let home = CliDriver::Traex.private_home().expect("traex 有家目录");
        assert!(
            home.ends_with(".trae"),
            "traex 家目录恒为 ~/.trae,不认 CODEX_HOME(实测): {home:?}"
        );
        // 非法 effort 的错误文案必须说 traex,不许张冠李戴成 codex。
        let err = CliDriver::Traex
            .resolve_launch(None, Some("warp"))
            .unwrap_err();
        match err {
            ModelEffortError::InvalidEffort { cli, .. } => assert_eq!(cli, "traex"),
            other => panic!("期望 InvalidEffort,实际 {other:?}"),
        }
    }

    /// P3:headless_command 把 model/effort 经 resolve_launch 拼进 argv(各 CLI 形态)。
    #[test]
    fn headless_command_threads_model_and_effort_per_cli() {
        // claude:--model / --effort 两个正交 flag,接在基础 flag 之后
        let (_p, args) = CliDriver::Claude
            .headless_command(
                "hi",
                &LaunchOpts {
                    model: Some("claude-opus-4-8"),
                    effort: Some("high"),
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(args.windows(2).any(|w| w == ["--model", "claude-opus-4-8"]));
        assert!(args.windows(2).any(|w| w == ["--effort", "high"]));

        // cursor:仅 --model(无独立 effort 轴),effort 给了也被降级丢掉
        let (_p, args) = CliDriver::Cursor
            .headless_command(
                "hi",
                &LaunchOpts {
                    model: Some("gpt-5.5-high"),
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(args.windows(2).any(|w| w == ["--model", "gpt-5.5-high"]));
        assert!(!args.iter().any(|a| a == "--effort"));

        // codex:-m 与 -c model_reasoning_effort= 都在 exec 级,须排在 resume 子命令之前
        let (_p, args) = CliDriver::Codex
            .headless_command(
                "follow up",
                &LaunchOpts {
                    resume: Some("tid-9"),
                    model: Some("gpt-5.5-codex"),
                    effort: Some("high"),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(
            args,
            vec![
                "exec",
                "--json",
                "-m",
                "gpt-5.5-codex",
                "-c",
                "model_reasoning_effort=high",
                "resume",
                "tid-9",
                "--",
                "follow up"
            ]
        );
    }

    /// P3 fail-closed:坏 effort 不 panic、降级成 model-only(claude/codex 丢 effort、cursor 丢整 effort)。
    #[test]
    fn headless_command_degrades_illegal_effort_without_panic() {
        // claude + 非法 effort(minimal 不在 claude 合法集)→ 仅 --model,无 --effort
        let (_p, args) = CliDriver::Claude
            .headless_command(
                "hi",
                &LaunchOpts {
                    model: Some("claude-opus-4-8"),
                    effort: Some("minimal"),
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(args.windows(2).any(|w| w == ["--model", "claude-opus-4-8"]));
        assert!(!args.iter().any(|a| a == "--effort"));

        // cursor + effort(不支持独立 effort 轴)→ 仅 --model
        let (_p, args) = CliDriver::Cursor
            .headless_command(
                "hi",
                &LaunchOpts {
                    model: Some("composer-1"),
                    effort: Some("high"),
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(args.windows(2).any(|w| w == ["--model", "composer-1"]));
    }

    /// resolve_launch_lenient 的契约:合法直通(dropped=false);非法降级 model-only(dropped=true)。
    #[test]
    fn resolve_launch_lenient_drops_bad_effort() {
        let (args, dropped) =
            CliDriver::Claude.resolve_launch_lenient(Some("claude-opus-4-8"), Some("high"));
        assert_eq!(args, vec!["--model", "claude-opus-4-8", "--effort", "high"]);
        assert!(!dropped);

        let (args, dropped) =
            CliDriver::Claude.resolve_launch_lenient(Some("claude-opus-4-8"), Some("nope"));
        assert_eq!(args, vec!["--model", "claude-opus-4-8"]);
        assert!(dropped, "坏 effort 应被丢掉并标记 dropped");

        // cursor effort 不支持:丢 effort 只剩 model
        let (args, dropped) =
            CliDriver::Cursor.resolve_launch_lenient(Some("gpt-5.5-high"), Some("high"));
        assert_eq!(args, vec!["--model", "gpt-5.5-high"]);
        assert!(dropped);
    }

    #[test]
    fn parse_headless_line_routes_per_driver() {
        use crate::parse::agent_msg::AgentKind;
        // codex driver 用 codex 解析器:thread.started → System 带 thread_id
        let codex = CliDriver::Codex
            .parse_headless_line(r#"{"type":"thread.started","thread_id":"tid-x"}"#);
        assert_eq!(codex.len(), 1);
        assert_eq!(codex[0].kind, AgentKind::System);
        assert_eq!(codex[0].session_id.as_deref(), Some("tid-x"));
        // claude driver 用 stream-json 解析器:codex 行不被它识别(无 type=system/assistant/...)
        let claude = CliDriver::Claude
            .parse_headless_line(r#"{"type":"thread.started","thread_id":"tid-x"}"#);
        assert!(claude.is_empty());
        // Other 永远空
        assert!(CliDriver::Other.parse_headless_line("anything").is_empty());
    }

    #[test]
    fn cursor_scrub_removes_status_chrome_but_keeps_answer() {
        let pane = "\
Tip: You can start the Cursor CLI with `agent` (same as `cursor-agent`).\n\
\n\
  Cursor Agent\n\
  v2026.05.28-418efe5\n\
  Use /skills to give Cursor specialized knowledge for tasks.\n\
\n\
  用一句话回复：ping\n\
\n\
\n\
  pong\n\
\n\
\n\
 ▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄\n\
  → 用一句话回复：ping\n\
 ▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀\n\
  GPT-5.5 1M High · 12%                                               Auto-run\n\
  ~/Desktop/personal/summon · develop\n";
        let scrubbed = scrub_cursor_chrome(pane);
        assert!(scrubbed.contains("pong"));
        assert!(!scrubbed.contains("Auto-run"));
        assert!(!scrubbed.contains("Cursor Agent"));
        assert!(!scrubbed.contains("▄▄"));
    }
}
