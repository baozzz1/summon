//! **slash 执行器**:注入一条命令,等到它的收尾形态,把用户要的回显取回来。
//!
//! 设计见 `docs/spec/2026-08-14-transcript-slash-passthrough-design.md` 第四、五、十一节。
//! 两条执行路共用同一条注入闸与同一条提交确认,只在「怎么判跑完了」上分叉:
//!
//! - **池内命令**([`run_pool_command_on`]):类别与时间窗来自真机采样,事前就知道该等什么。
//! - **未知命令**([`run_generic_command_on`]):准入反转(owner 2026-08-14)之后的核心增量。
//!   事前没有类别可用,**归类改由注入之后的观测定** —— 面板特征 / spinner+转录增长 / 回到空
//!   输入框,判不出就诚实回「不确定」。判据本身全部复用既有机件(`classify_pane`、
//!   `pane_output`、`slash_pool::classify_generic`),这里只负责按拍喂事实。
//!
//! 三条不变量全部落在这一个文件里,别处不许再造一套(两条执行路一视同仁):
//!
//! - **零 marker**:slash 一律不附 transport marker(实测 `/compact <!-- summon-transport-turn:… -->`
//!   会被 claude 当成压缩指令解析进 `<command-args>`)。所以送达与完成**只**走抓屏。
//! - **at-most-once**:send-keys 一旦发出,无论后面判成什么都绝不自动重注。判不出来就回
//!   「不确定」,让人 attach 看一眼 —— 重注对 `/compact` 是浪费,对状态变更类是事故。
//! - **静止 ≠ 完成**:收尾必须是正向证明(print/slow = 回到空输入框,panel = 认出已注册面板),
//!   而且内容要连续两拍一致。只看「画面不动了」会把面板态报成执行成功,那正是 spec 第一节的事故。

use std::thread::sleep;
use std::time::{Duration, Instant};

use anyhow::bail;
use summon_core::driver::slash_pool::{
    classify_generic, is_settled, GenericTick, SlashClass, SlashEntry, GENERIC_CAP_SECS,
    GENERIC_STALL_SECS,
};
use summon_core::pane_input::{
    classify_pane, classify_pane_styled, pending_input, InjectGate, ModalKind, PaneState,
};
use summon_core::pane_output::{clip_with_notice, new_output_since, panel_body, transcript_region};

use crate::session::input_gate::prove_injectable;
use crate::session::session::Session;

/// 执行器用到的**全部** pane 能力,抽成 trait 只为一件事:让这段流程的**顺序**能在无 tmux 的
/// 单测里被钉死。顺序里有两条只靠代码审阅守不住的不变量:
/// ① 面板正文必须在按 Esc **之前**抓走(Esc 之后那一屏就没了,等于把用户要的答案丢掉);
/// ② 注入 at-most-once —— 无论后面判成什么,`submit` 都只许被调用一次。
///
/// 时间窗也放进 trait:测试拿零窗口跑,才不会为了验一条顺序去真睡几十秒。
pub trait SlashPane {
    fn capture(&self) -> anyhow::Result<String>;
    /// 带 SGR 转义的可见区抓屏。只用来分辨「输入行上那段字是 claude 自己画的还是用户草稿」
    /// (见 `pane_input::prompt_text_is_dim`)——纯文本抓屏里两者同形。
    ///
    /// 默认回落纯文本:拿不到样式时 `prompt_text_is_dim` 恒 false,收尾判据退回原行为。
    fn capture_styled(&self) -> anyhow::Result<String> {
        self.capture()
    }
    /// **过闸**注入一行。被闸拒时必须 `Err` 且一个键都没发出去 —— 调用方据此回「本条没有执行」。
    fn submit_line(&self, text: &str) -> anyhow::Result<()>;
    fn send_enter(&self) -> anyhow::Result<()>;
    /// 清掉输入行。**只在正向确认「输入行拿着的正是我们自己打进去的那条命令」之后调用** ——
    /// 它会把 attach 用户的草稿一起清掉,所以判据不成立时一个键都不许发。
    fn clear_own_draft(&self) -> anyhow::Result<()>;
    /// 重新证明输入态。panel 收尾复用它退面板:受限一次 Esc + 重判(见 `input_gate`)。
    fn prove_idle(&self) -> anyhow::Result<PaneState>;
    /// 收尾轮询间隔。
    fn poll_interval(&self) -> Duration;
    /// 提交确认之间的等待。
    fn submit_grace(&self) -> Duration;
    /// 通用执行器的观测窗(静止窗 / 总时长硬顶)。放进 trait 与 `poll_interval` 同理:
    /// 测试拿短窗口跑,才不会为了验一条超时分支去真睡三分钟。
    fn generic_windows(&self) -> (Duration, Duration) {
        (
            Duration::from_secs(GENERIC_STALL_SECS),
            Duration::from_secs(GENERIC_CAP_SECS),
        )
    }
}

impl SlashPane for Session<'_> {
    fn capture(&self) -> anyhow::Result<String> {
        Ok(self.tmux.capture()?)
    }
    fn capture_styled(&self) -> anyhow::Result<String> {
        Ok(self.tmux.capture_styled()?)
    }
    fn submit_line(&self, text: &str) -> anyhow::Result<()> {
        // 闸恒 IdleOnly:运行中注入斜杠命令不会被当命令执行,只会作为一段 prompt 文本排进队列
        // (`/compact` 变成聊天内容),那是静默的语义损坏。
        self.send_line_for_driver(text, InjectGate::IdleOnly)
    }
    fn send_enter(&self) -> anyhow::Result<()> {
        Ok(self.tmux.send_enter()?)
    }
    fn clear_own_draft(&self) -> anyhow::Result<()> {
        Ok(self.tmux.clear_input()?)
    }
    fn prove_idle(&self) -> anyhow::Result<PaneState> {
        Ok(prove_injectable(
            &self.tmux,
            InjectGate::IdleOnly,
            Default::default(),
        )?)
    }
    fn poll_interval(&self) -> Duration {
        self.poll
    }
    fn submit_grace(&self) -> Duration {
        SUBMIT_GRACE
    }
}

/// 提交后确认「输入行已经不再拿着这条命令」的重试次数与间隔。
///
/// **不用 `inject_command` 的「盲补两次 Enter」**:那是给不弹面板的 `/compact` 用的,对 panel 类
/// 有害 —— 面板一旦弹出来,补发的 Enter 就打在面板上(`/config` 面板上 Enter 是「选中并切换某项
/// 设置」)。改成正向确认:每按一次就抓屏看输入行还在不在,不在了立刻停手。
const SUBMIT_GRACE: Duration = Duration::from_millis(400);
const SUBMIT_RETRIES: usize = 3;

/// 回传给飞书的正文上限。面板正文一屏、print 输出偶尔很长;超了要**报数**截断,
/// 不能悄悄截(那等于谎报「这就是全部输出」)。
const MAX_RECEIPT_CHARS: usize = 3000;

/// 一次白名单透传的结局。**三选一,不并存** —— 回执唯一是 08-12 仲裁的既有不变量。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlashOutcome {
    /// print 类:命令跑完、回到空输入框,附本次新增的可见输出。
    ///
    /// `anchored=false` = 抓屏对不上上一屏(整屏被重画 / 滚动过多),`output` 是**整块可见转录区**
    /// 而不是「这条命令打印的东西」——里面可能有上一轮对话正文、工具输出、文件片段。回执必须
    /// 据此降级:不宣称「这就是输出」,且正文与口径说明一同改走私聊。
    Printed { output: String, anchored: bool },
    /// panel 类:抓到了面板正文。`closed=false` = 受限 Esc 没能退出面板,会话仍停在面板上。
    Panel { body: String, closed: bool },
    /// turn 类:整行已提交(正向确认命令离开输入行),**收尾与回复交给真实轮机制** ——
    /// 注入零 marker,CLI 侧当场开一轮真轮,**调用方**(chat 线程的 `run_slash_turn`)持有
    /// 这条轮直到 terminal:与普通 prompt 轮同构的消费路(流式卡、打断、收尾表情),镜像归属
    /// 由注入前绑定的 markerless fence 按偏移裁决。执行器不等收尾、不抓屏回执、不 Esc
    /// (Esc 会打断刚开的轮)。这里只证明了「已提交」,没证明「轮已开始」—— 语义据此收敛。
    HandedToTurn,
    /// 判不出终态(超时 / 停在认不出的形态)。命令**可能已经执行**,如实说不确定。
    Uncertain { reason: String },
}

/// 一次透传该发出去的**全部**消息。`private_body` 存在时,那段正文只能进私聊。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlashReceipt {
    /// 会话阻塞需要原会话可见；成功私聊时为 None。
    pub chat_body: Option<String>,
    /// 只能私聊给发起人的正文(面板全文)。
    pub private_body: Option<String>,
    /// 这次算不算成功(决定表情与 `TurnResult`)。
    pub ok: bool,
    /// `chat_body` 只确认命令照做了:群里完成表情贴上了就不再发它。
    pub only_confirms: bool,
}

/// 抓屏正文一律裹代码围栏:pane 里的 `#` `*` `|` `-` 会被飞书当 markdown 解析,
/// 面板的表格与进度条渲染出来就不成形了(仓内已有 `fix/lark-md-escape-display` 在治这一类)。
fn fenced(body: &str) -> String {
    format!("```\n{}\n```", body.trim_end())
}

/// 群成员都能看到原会话回执；面板含账号信息，锚失配的抓屏还可能含其它对话和文件内容。
/// 正文与执行状态发给发起人，群中只提示会话阻塞。锚失配时必须说明输出口径的不确定性。
pub fn compose_slash_receipt(
    name: &str,
    outcome: &SlashOutcome,
    is_group: bool,
    agent: &str,
) -> SlashReceipt {
    match outcome {
        SlashOutcome::Printed { output, .. } if output.trim().is_empty() => SlashReceipt {
            chat_body: Some(summon_i18n::format!("r339bb04154")),
            private_body: None,
            ok: true,
            only_confirms: true,
        },
        SlashOutcome::Printed {
            output,
            anchored: true,
        } => SlashReceipt {
            chat_body: Some(summon_i18n::format!("r62954dd819", fenced(output))),
            private_body: None,
            ok: true,
            only_confirms: false,
        },
        SlashOutcome::Printed {
            output,
            anchored: false,
        } => {
            let caveat = summon_i18n::format!("r3df7b9e43c");
            if is_group {
                SlashReceipt {
                    chat_body: None,
                    private_body: Some(format!("{caveat}\n\n{}", fenced(output))),
                    ok: true,
                    only_confirms: false,
                }
            } else {
                SlashReceipt {
                    chat_body: Some(format!("{caveat}\n\n{}", fenced(output))),
                    private_body: None,
                    ok: true,
                    only_confirms: false,
                }
            }
        }
        SlashOutcome::Panel { body, closed } => {
            // 成功回执必须以「回到空输入框」背书;没退出去就如实说会话还停在面板上。
            let tail = if *closed {
                summon_i18n::text!("re3468c4c02")
            } else {
                summon_i18n::text!("rffc6ae6259")
            };
            if is_group {
                SlashReceipt {
                    chat_body: (!*closed).then(|| format!("`{name}`：{tail}")),
                    private_body: Some(summon_i18n::format!("r609d36552a", fenced(body))),
                    ok: *closed,
                    only_confirms: false,
                }
            } else {
                SlashReceipt {
                    chat_body: Some(format!("`{name}`:\n\n{}\n\n{tail}", fenced(body))),
                    private_body: None,
                    ok: *closed,
                    only_confirms: false,
                }
            }
        }
        // 防御臂:主路径上 HandedToTurn 由 run_slash_turn 接管、不再走回执组合(成功判决就是
        // 轮自己的投递)。这段文案只在未来某条误把它送进回执路的代码上兜底,措辞保持真实。
        SlashOutcome::HandedToTurn => SlashReceipt {
            chat_body: Some(summon_i18n::format!("r54f36a4784")),
            private_body: None,
            ok: true,
            only_confirms: false,
        },
        SlashOutcome::Uncertain { reason } => SlashReceipt {
            chat_body: Some(summon_i18n::format!("r28a9663831")),
            private_body: None,
            ok: false,
            only_confirms: false,
        },
    }
}

/// 面板全文私聊失败时,发进**群里**的那条回执。
///
/// 它单独成函数而不是就地 `format!`,是因为这里有一条只靠代码审阅守不住的边界:这条消息发在
/// 群里,所以它**不接受正文参数** —— 想把 body 拼进来的人得先改签名。裁决 2 要避免的正是
/// 「私聊发不出去就回落到群里」这个动作。
pub fn private_delivery_failed_notice(name: &str, agent: &str) -> String {
    summon_i18n::format!("r8b1571d74b")
}

/// 一次注入的**来源**:池内已采样条目(调优执行器) / 未知命令(通用执行器)。
///
/// 用枚举而不是 `Option<&SlashEntry>`,是为了让「未知命令」在调用侧是一个**要显式选中**的
/// 分支而不是「忘了传 entry」的副产物 —— 两条路的收尾语义差别很大,不该靠一个 `None` 表达。
#[derive(Debug, Clone, Copy)]
pub enum SlashInjection {
    /// 采样池条目:类别与时间窗事前已知(真机采样调优)。
    Pool(&'static SlashEntry),
    /// 未知命令:类别由注入之后的观测归类。
    Generic,
}

impl Session<'_> {
    /// 跑一条采样池里的命令。`Slow` 类不走这里(它有自己的宽窗口与卡片回执,见 `handle_compact`)。
    /// `line` 是 `plan_slash` 给出的**整行原文**(带参形态含参数,零 marker)。
    pub fn run_pool_command(&self, entry: &SlashEntry, line: &str) -> anyhow::Result<SlashOutcome> {
        run_pool_command_on(self, entry, line)
    }

    /// 按来源分派到两条执行路。`run_slash_turn` 走这个入口 —— 未知命令也可能是一条真轮,
    /// 所以 fence/消费的接线对两条路必须是同一套。
    pub fn run_slash(&self, injection: SlashInjection, line: &str) -> anyhow::Result<SlashOutcome> {
        run_slash_on(self, injection, line)
    }
}

/// 按来源分派。两条路的**契约完全一致**:`Err` 只代表「一个键都没发出去」,其余一律收敛成
/// `SlashOutcome`。
pub fn run_slash_on(
    pane: &impl SlashPane,
    injection: SlashInjection,
    line: &str,
) -> anyhow::Result<SlashOutcome> {
    match injection {
        SlashInjection::Pool(entry) => run_pool_command_on(pane, entry, line),
        SlashInjection::Generic => run_generic_command_on(pane, line),
    }
}

/// **未知命令的通用执行器**(owner 2026-08-14 准入反转的核心增量)。
///
/// 与池内执行器共用的部分一字不差:同一道注入闸(`submit_line` 恒 `IdleOnly`,被拒 = 零按键
/// `Err`)、同一条 at-most-once 提交确认(`submit_slash`)、同一份回执组合
/// ([`compose_slash_receipt`])。**唯一的不同是收尾类别不是事前查表得来的,而是注入之后
/// 观测出来的**(见 `slash_pool::classify_generic`):
///
/// - settle 后命中已注册面板特征 → panel 语义:抓正文 → 受限 Esc → 验证回到空输入框;
/// - 转录区增长且回到空输入框、无轮迹象 → print 语义:锚定输出(锚失配由回执降级走私聊);
/// - spinner + 转录持续增长(或 spinner 独自挂满耐心窗)→ turn 语义:返回 [`HandedToTurn`]
///   交给调用方的真实轮机制,与 `/goal <目标>` 同一条路;
/// - 判不出(窗口用尽 / 停在认不出的形态)→ `Uncertain`,如实说不确定。
///
/// [`HandedToTurn`]: SlashOutcome::HandedToTurn
pub fn run_generic_command_on(pane: &impl SlashPane, line: &str) -> anyhow::Result<SlashOutcome> {
    let line = line.trim();
    // 执行器自己的零按键守卫(`plan_slash` 已保证,这里是防未来调用方的第二道)。
    // 通用路比池内路多一条:首 token 必须是 `/` 开头 —— 池内路由 entry 名字兜住了这件事,
    // 通用路没有 entry 可对,守卫只能自己写。
    if !line.starts_with('/') {
        bail!(summon_i18n::format!("r95621ee5a2"));
    }
    if line.chars().any(char::is_control) {
        bail!(summon_i18n::format!("r4495fe8822"));
    }
    let before = pane.capture()?;
    // ↓ 这一行是分界:之前的 Err = 零按键;之后一律不再 Err(那时「没执行」已经不再为真)。
    if let Submitted::Stuck { cleaned } = submit_slash(pane, line)? {
        return Ok(SlashOutcome::Uncertain {
            reason: if cleaned {
                summon_i18n::text!("r3becd960e2").into()
            } else {
                summon_i18n::text!("r83fb4527ac").to_string()
            },
        });
    }
    let observed = match observe_generic(pane) {
        Ok(observed) => observed,
        // 键已经发过了,这时报「没执行」是谎报;抓屏失败只能说不确定。
        Err(e) => {
            return Ok(SlashOutcome::Uncertain {
                reason: summon_i18n::format!("r92eec9124b"),
            })
        }
    };
    let (class, screen) = match observed {
        Observed::Classified { class, screen } => (class, screen),
        // 秒数如实:报实际流逝与命中的那道上限,不拿另一道窗口的数字顶替。
        Observed::TimedOut { waited_secs, limit } => {
            return Ok(SlashOutcome::Uncertain {
                reason: summon_i18n::format!("r31a3f9eea2"),
            })
        }
    };
    match class {
        // turn:CLI 自己开了一轮真轮。**不抓屏回执、不 prove_idle**(Esc 会打断刚开的轮),
        // 收尾与回复交给调用方的真实轮机制 —— 与 `/goal <目标>` 完全同一条路。
        SlashClass::Turn => Ok(SlashOutcome::HandedToTurn),
        SlashClass::Panel => {
            // 先抓正文再退面板:Esc 之后那一屏就没了,顺序反了等于把用户要的答案丢掉。
            let body = clip_with_notice(&panel_body(&screen), MAX_RECEIPT_CHARS);
            let closed = pane.prove_idle().is_ok_and(|s| s == PaneState::InputReady);
            Ok(SlashOutcome::Panel { body, closed })
        }
        SlashClass::Print => {
            let out = new_output_since(&before, &screen);
            Ok(SlashOutcome::Printed {
                output: clip_with_notice(&out.text, MAX_RECEIPT_CHARS),
                anchored: out.anchored,
            })
        }
        // `classify_generic` 结构上不产出 Slow(慢命令的宽窗口只对采样过的命令有意义)。
        SlashClass::Slow => Ok(SlashOutcome::Uncertain {
            reason: summon_i18n::text!("r47b24dfcae").to_string(),
        }),
    }
}

/// 通用执行器的观测结局。
enum Observed {
    /// 归类成功,连同**归类那一拍的抓屏**一起交出去 —— 面板正文/新增输出都要从那一屏取,
    /// 再抓一次可能已经变了(面板被人 Esc 掉、输出被顶走)。
    Classified { class: SlashClass, screen: String },
    /// 窗口用尽仍判不出。秒数与命中的那道上限都要如实带回去(拿另一道窗口的数字顶替 = 谎报)。
    TimedOut { waited_secs: u64, limit: String },
}

/// 按拍观测,把事实喂给纯判据 `classify_generic`,直到归类成功或窗口用尽。
///
/// 这里只做三件事,判据本身一条都不在这里:
/// ① 用既有 `classify_pane` / `transcript_region` / `panel_body` 取每一拍的事实;
/// ② 维护两条跨拍状态 —— 「spinner 期间转录区连续增长了几拍」与「spinner 连续挂了多久」;
/// ③ 记「类别相关正文与上一拍是否逐字节一致」。
///
/// **第一拍恒判不出**(没有上一拍可比,`stable` 必为 false)。这是有意的最小观测地板:
/// 提交确认本身已经睡过一次 `submit_grace`,再加两拍轮询,足以让 CLI 把 spinner 画出来 ——
/// 少了它,一条刚提交、还没来得及起 spinner 的命令会被当场判成「print,无可见输出」。
///
/// 静止窗只在**类别相关正文逐字节不变**时累计:轮在思考时 spinner 那行的走秒计时就落在转录区
/// 里,每拍都在变,所以这道窗不会误伤「想得久但确实在跑」的轮。
fn observe_generic(pane: &impl SlashPane) -> anyhow::Result<Observed> {
    let (stall, cap) = pane.generic_windows();
    let started = Instant::now();
    let deadline = started + cap;
    let mut prev_transcript: Option<Vec<String>> = None;
    let mut prev_panel: Option<String> = None;
    let mut last_change = Instant::now();
    let mut running_since: Option<Instant> = None;
    let mut growth_streak: u32 = 0;
    loop {
        let screen = pane.capture()?;
        let state = classify_pane(&screen);
        let transcript = transcript_region(&screen);
        let panel = panel_body(&screen);
        // 「增长」看**行数**而不是「文本变了」:spinner 那行每秒都在改(走秒 / token 数),
        // 拿「变了」当增长会让每一条命令的 spinner 阶段都冒充成开轮。
        let grew = prev_transcript
            .as_ref()
            .is_some_and(|prev| transcript.len() > prev.len());
        let running = state == PaneState::Running;
        growth_streak = if running && grew {
            growth_streak + 1
        } else {
            0
        };
        running_since = if running {
            running_since.or_else(|| Some(Instant::now()))
        } else {
            None
        };
        let stable = match state {
            PaneState::Modal(ModalKind::Panel) => prev_panel.as_deref() == Some(panel.as_str()),
            _ => prev_transcript.as_ref() == Some(&transcript),
        };
        if !stable {
            last_change = Instant::now();
        }
        let tick = GenericTick {
            state,
            stable,
            growth_streak,
            running_secs: running_since.map_or(0, |since| since.elapsed().as_secs()),
        };
        prev_transcript = Some(transcript);
        prev_panel = Some(panel);
        if let Some(class) = classify_generic(tick) {
            return Ok(Observed::Classified { class, screen });
        }
        let now = Instant::now();
        let limit = if now >= deadline {
            Some(summon_i18n::format!("r200905cb02", cap.as_secs()))
        } else if now.duration_since(last_change) >= stall {
            Some(summon_i18n::format!("rcf01e8ffaa", stall.as_secs()))
        } else {
            None
        };
        if let Some(limit) = limit {
            return Ok(Observed::TimedOut {
                waited_secs: started.elapsed().as_secs(),
                limit,
            });
        }
        sleep(pane.poll_interval());
    }
}

/// 跑一条池内命令(print / panel / turn)。`line` 是注入的整行原文:无参形态就是 `entry.name`,
/// 带参形态(`ArgsPolicy::Passthrough`,owner 2026-08-14 授权)含参数原文,依旧零 marker。
/// **收尾语义按注入原文的有效类别取**(`class_for_line`):单形态条目沿用基线类别
/// (print 仍 print、panel 仍 panel),参数变体若渲染出意外形态(弹了认不出的界面、迟迟不到
/// 收尾态),由下面既有的 settle 超时 / 受限 Esc / fail-closed 机制兜底,不为参数形态另设判据;
/// 双形态条目(`/goal`)的带参形态是 turn 类 —— 提交即交棒,收尾与回复由调用方的真实轮机制持有。
///
/// 返回 `Err` 只有一种情况:**一个键都没发出去**(被注入闸拒绝 / 注入前抓屏失败)。这条契约是
/// 硬的 —— 调用方据它对用户断言「本条没有执行」,说错就是谎报的镜像版。所以 `submit_line` 成功
/// **之后**的一切不如意(收尾抓屏失败、Enter 被吞、等不到收尾形态)一律收敛成 `Uncertain`:
/// 那时「没执行」已经不再为真,只有「不确定」是诚实的。
pub fn run_pool_command_on(
    pane: &impl SlashPane,
    entry: &SlashEntry,
    line: &str,
) -> anyhow::Result<SlashOutcome> {
    if entry.class == SlashClass::Slow {
        bail!(summon_i18n::format!("r3b5fb50104", entry.name));
    }
    // 执行器自己的两道零按键守卫(`plan_slash` 已保证,这里是防未来调用方的第二道):
    // ① 整行的首 token 必须就是这条条目 —— entry 与 line 配错 = 收尾判据整个错位;
    // ② 换行/控制字符是按键不是参数(换行触发多行提交补 Enter,打在面板上)。
    let line = line.trim();
    if line.split_whitespace().next() != Some(entry.name) {
        bail!(summon_i18n::format!("r1d576429bb", entry.name));
    }
    if line.chars().any(char::is_control) {
        bail!(summon_i18n::format!("r4495fe8822"));
    }
    // 有效收尾类别按**注入原文的形态**取(`/goal` 裸形态 panel、带参形态 turn)——
    // 判据收口在池的 `class_for_line`,这里不再自判 has_args。
    let class = entry.class_for_line(line);
    // turn 类也过这一道:注入前抓屏失败 = 零按键 Err,契约对所有类别一致。
    let before = pane.capture()?;
    // ↓ 这一行是分界:之前的 Err = 零按键;之后一律不再 Err。
    if let Submitted::Stuck { cleaned } = submit_slash(pane, line)? {
        return Ok(SlashOutcome::Uncertain {
            reason: if cleaned {
                summon_i18n::text!("r3becd960e2").into()
            } else {
                summon_i18n::text!("r83fb4527ac").to_string()
            },
        });
    }
    // turn 类:提交即交棒。它本质是 prompt,CLI 当场开一轮真轮,收尾与回复由调用方的真实轮
    // 机制持有(run_slash_turn → 同构消费路)—— 这里**不等收尾、不抓屏回执、不 prove_idle**
    // (那会按 Esc,打断刚开的轮);拿 print/panel 的抓屏判据等一条真轮,等到的只会是超时误报。
    if class == SlashClass::Turn {
        return Ok(SlashOutcome::HandedToTurn);
    }
    let settled = match wait_settled(pane, entry, class) {
        Ok(settled) => settled,
        // 键已经发过了,这时报「没执行」是谎报;抓屏失败只能说不确定。
        Err(e) => {
            return Ok(SlashOutcome::Uncertain {
                reason: summon_i18n::format!("r2cf25862d5"),
            })
        }
    };
    let screen = match settled {
        Settle::Done(screen) => screen,
        // 秒数如实:报实际流逝与命中的那道上限。stall 命中却写 cap_secs 是谎报
        // (真机 `/status` 20s 静止即停,旧回执却说「等了 60s」)。
        Settle::TimedOut { waited_secs, limit } => {
            return Ok(SlashOutcome::Uncertain {
                reason: summon_i18n::format!("r43d0fbb89e", entry.name),
            });
        }
    };
    match class {
        SlashClass::Print => {
            let out = new_output_since(&before, &screen);
            Ok(SlashOutcome::Printed {
                output: clip_with_notice(&out.text, MAX_RECEIPT_CHARS),
                anchored: out.anchored,
            })
        }
        SlashClass::Panel => {
            // 先抓正文再退面板:Esc 之后那一屏就没了,顺序反了等于把用户要的答案丢掉。
            let body = clip_with_notice(&panel_body(&screen), MAX_RECEIPT_CHARS);
            // 复用注入闸退面板:它只对**已注册**面板授权一次 Esc,且退完要重新证明输入态。
            // 「回到空输入框」是成功回执的背书,拿不到就如实说面板还在。
            let closed = pane.prove_idle().is_ok_and(|s| s == PaneState::InputReady);
            Ok(SlashOutcome::Panel { body, closed })
        }
        SlashClass::Slow | SlashClass::Turn => {
            unreachable!("{}", summon_i18n::format!("r385881098f"))
        }
    }
}

/// 提交的两种结局。`Err` 只留给「一个键都没发出去」。
enum Submitted {
    /// 命令已经离开输入行 = 已提交。
    Sent,
    /// 键发过了,命令仍停在输入行里(Enter 被吞)。`cleaned` = 我们打进去的那行已经被清掉。
    Stuck { cleaned: bool },
}

/// 打进去并**正向确认已提交**。斜杠命令打完会弹自动补全菜单,首个 Enter 可能只是「选中」;
/// 真机(claude 2.1.231)实测 exact 命令名一个 Enter 就执行,但这条竞态曾经真实发生过,
/// 所以留重试 —— 只是把「盲补」换成「看着补」。
///
/// 补 Enter 不算「第二次注入」:它只在**输入行仍拿着这条命令**(= 还没提交)时才发,
/// 命令一旦离开输入行就立刻停手,绝不会把同一条命令跑第二遍。
///
/// **失败时必须把自己的残留收干净。** claude 的 `clear_input_before_send()` 为 false,没人替我们
/// 清:一条没提交成功的 `/status` 会一直躺在输入框里,此后 pane 恒判 `Draft`,**每一条**后续飞书
/// 消息都被闸拒,而回执还写着「你 attach 时打了一半?」—— 把 summon 自己留下的残渣算到用户头上,
/// 且只有人工 attach 才能恢复。清之前先 `input_holds` 正向确认那串字确实是我们打的,
/// 否则一个键都不发(那可能是用户的草稿)。
fn submit_slash(pane: &impl SlashPane, cmd: &str) -> anyhow::Result<Submitted> {
    // 被闸拒时这行会 Err 且一个键都没发出去,调用方据此回「本条没有执行」。
    pane.submit_line(cmd)?;
    for _ in 0..SUBMIT_RETRIES {
        sleep(pane.submit_grace());
        let screen = pane.capture().unwrap_or_default();
        if !input_holds(&screen, cmd) {
            return Ok(Submitted::Sent);
        }
        if pane.send_enter().is_err() {
            break;
        }
    }
    let ours = input_holds(&pane.capture().unwrap_or_default(), cmd);
    let cleaned = ours && pane.clear_own_draft().is_ok();
    Ok(Submitted::Stuck { cleaned })
}

/// 收尾等待的结局。超时必须带着**实情**回去:实际等了多久、命中的是哪道上限 —— 两道窗口
/// (静止窗 / 总时长硬顶)秒数差好几倍,拿错一个写进回执就是对用户谎报等待过程。
enum Settle {
    Done(String),
    TimedOut { waited_secs: u64, limit: String },
}

/// 等到这条命令的收尾形态。返回 `TimedOut` = 到点了还没等到(超时,归入「不确定」)。
///
/// 收尾要求两条**同时**成立:① 状态是这个类别的正向收尾态;② 类别相关的正文连续两拍一致。
/// ② 不能拿整屏做 —— 底部状态行带着走秒的计时,整屏永远不会「一致」。
fn wait_settled(
    pane: &impl SlashPane,
    entry: &SlashEntry,
    class: SlashClass,
) -> anyhow::Result<Settle> {
    let started = Instant::now();
    let deadline = started + Duration::from_secs(entry.cap_secs);
    let stall = Duration::from_secs(entry.stall_secs);
    let mut prev_content: Option<String> = None;
    let mut last_change = Instant::now();
    loop {
        let screen = pane.capture()?;
        let content = settle_content(class, &screen);
        let stable = prev_content.as_deref() == Some(content.as_str());
        if !stable {
            last_change = Instant::now();
            prev_content = Some(content.clone());
        }
        if stable && !content.trim().is_empty() && is_settled(class, settled_state(pane, &screen)) {
            return Ok(Settle::Done(screen));
        }
        // 两道上限任一命中即停,并记下**命中的是哪道**:stall 单独触发也停 —— 画面彻底不动
        // 却没进收尾态,再等也不会变(典型:停在认不出的界面上)。
        let now = Instant::now();
        let limit = if now >= deadline {
            Some(summon_i18n::format!("r200905cb02", entry.cap_secs))
        } else if now.duration_since(last_change) >= stall {
            Some(summon_i18n::format!("r83f446e6cb", entry.stall_secs))
        } else {
            None
        };
        if let Some(limit) = limit {
            return Ok(Settle::TimedOut {
                waited_secs: started.elapsed().as_secs(),
                limit,
            });
        }
        sleep(pane.poll_interval());
    }
}

/// 这一拍的 pane 状态,**带 ghost 复核**。
///
/// claude 答完一轮会在空输入框里画一句建议下一句(ghost text),纯文本抓屏里它与用户草稿逐字节
/// 同形 —— print 类命令的收尾要求 `InputReady`,被判成 `Draft` 就永远等不到收尾,只能撞静止窗
/// 报「不确定」。样式分得出,但**只在纯文本已经判成 `Draft` 时才多抓一次**:那是唯一分不出来
/// 的状态,其余每拍都抓样式等于把 fork-exec 翻倍。抓不到 → 回落纯文本判据。
fn settled_state(pane: &impl SlashPane, screen: &str) -> PaneState {
    let state = classify_pane(screen);
    if state != PaneState::Draft {
        return state;
    }
    pane.capture_styled()
        .map(|styled| classify_pane_styled(&styled))
        .unwrap_or(state)
}

/// 输入行是否还拿着我们刚打进去的那条命令(= 还没提交)。
fn input_holds(pane: &str, cmd: &str) -> bool {
    pending_input(pane).is_some_and(|rest| rest == cmd.trim())
}

/// 判「静止」时看的那部分正文:panel 看面板正文,print/slow 看转录区。两者都天然排除了
/// 底部每秒都在变的状态行。
fn settle_content(class: SlashClass, pane: &str) -> String {
    match class {
        SlashClass::Panel => panel_body(pane),
        // Turn 不会走到这里(提交即返回),给它转录区语义只为穷尽匹配不放 unreachable ——
        // 万一将来有人把 turn 接进 settle 循环,拿到的也是最保守的判据。
        SlashClass::Print | SlashClass::Slow | SlashClass::Turn => {
            summon_core::pane_output::transcript_region(pane).join("\n")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};
    use summon_core::driver::slash_pool::{pool_for, ArgsPolicy};
    use summon_core::pane_input::{InjectionRefused, ModalKind};

    const IDLE: &str = "✻ Worked for 7m 25s\n──── summon:soul-x ──\n❯ \n────\n  ⏵⏵ bypass\n";

    /// 真机抓屏(2026-08-14,claude 2.1.231,`/cost`):整屏面板,底部无页脚,靠 tab 栏认出。
    const COST_PANEL: &str = "▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔\n   \
Settings  Status   Config   Usage   Stats\n\n   Session\n\n   \
Total cost:            $0.0000\n   Total duration (wall): 1m 8s\n\n   \
Current week (all models)\n   ███████████▌       23% used\n\
                                                    ↓\n";

    /// 真机抓屏(2026-08-14,claude 2.1.231,probe 会话,裸 `/goal` 无目标):底部锚定面板,
    /// 满宽 ▔ 顶栏、**无 tab 栏**、页脚 `Esc to dismiss` —— 页脚是它唯一的正向特征。
    const GOAL_PANEL: &str = "╰──────────────────────────────────────╯\n\
▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔\n   \
Goal\n   No goal set\n   /goal <condition> to set one\n   Esc to dismiss\n";

    /// 真机抓屏(同次):`/context` 之前 / 之后。之后那屏用量图落进转录区、输入框回到空。
    const BEFORE_CONTEXT: &str = "│         Welcome back Alice!        │\n\n\
────────────────────────────\n❯ \n────────────────────────────\n   \
󱙺 Opus 5 (xhigh)      󰝰 scratchpad/probe\n";
    const AFTER_CONTEXT: &str = "     ⛶ ⛶ ⛶ ⛶ ⛶   claude-opus-5\n\
     ⛶ ⛶ ⛶ ⛶ ⛶   29.1k/1m tokens (3%)\n\n     /context all to expand\n\
────────────────────────────\n❯ \n────────────────────────────\n   \
󱙺 Opus 5 (xhigh)      󰝰 scratchpad/probe\n";

    /// 真机形态(2026-08-14,claude 2.1.231):**CLI 当场开了一轮真轮**的连续三拍 ——
    /// spinner 一直挂着(`✢ …… (Ns · ↓ Nk tokens)`,真机原样含省略号与走秒),
    /// 转录区**行数在涨**(工具行一条条落屏)。这两条同时成立才是「开轮」的正向证据:
    /// 只看 spinner 会把 `/context` 那 8 秒统计也算成开轮,只看「文本变了」会把走秒当增长。
    /// 第 0 拍:轮刚起来,还在思考 —— spinner 已经挂上,转录区一个字都还没长出来。
    const TURN_T0: &str = "✢ Prestidigitating… (1s · ↓ 0.3k tokens)\n\
────────────────────────────\n❯ \n────────────────────────────\n   󱙺 Opus 5 (xhigh)\n";
    const TURN_T1: &str = "⏺ 开始处理这条目标\n\
✢ Prestidigitating… (3s · ↓ 1.2k tokens)\n\
────────────────────────────\n❯ \n────────────────────────────\n   󱙺 Opus 5 (xhigh)\n";
    const TURN_T2: &str = "⏺ 开始处理这条目标\n  ⎿ Goal set: 完成一个测试目标\n\
✢ Prestidigitating… (5s · ↓ 2.4k tokens)\n\
────────────────────────────\n❯ \n────────────────────────────\n   󱙺 Opus 5 (xhigh)\n";
    const TURN_T3: &str = "⏺ 开始处理这条目标\n  ⎿ Goal set: 完成一个测试目标\n\
⏺ Read(scratchpad/probe/notes.md)\n\
✢ Prestidigitating… (7s · ↓ 3.9k tokens)\n\
────────────────────────────\n❯ \n────────────────────────────\n   󱙺 Opus 5 (xhigh)\n";

    /// 认不出的静止画面:既没有输入行、也不命中任何已注册面板特征 —— `classify_pane` 判
    /// `Unknown`。这正是「判不出」那一格,必须 fail-closed 成诚实的「不确定」。
    const UNRECOGNIZED: &str = "某个认不出的界面,既没有输入框也不是已注册面板\n";

    fn entry(name: &str) -> &'static SlashEntry {
        pool_for(
            summon_core::driver::CliDriver::Claude,
            summon_core::config::registry::CaptureMode::Transcript,
        )
        .iter()
        .find(|e| e.name == name)
        .expect("池里应有这条")
    }

    /// 可编排的假 pane:按脚本吐屏,并按**顺序**记下每一次动作(`capture` 不记 —— 它无副作用,
    /// 记了只会淹没真正要看的那几步)。
    struct FakePane {
        /// 依次弹出,最后一屏保持不变(模拟静止画面)。
        screens: RefCell<Vec<String>>,
        /// Esc 之后统一返回这一屏。用它来证明「正文是在按 Esc **之前**抓的」。
        after_escape: String,
        escaped: Cell<bool>,
        /// 闸拒:`submit_line` 直接 Err,且一个键都发不出去。
        refuse_with: Option<PaneState>,
        /// `prove_idle` 能不能退回空输入框。
        idle_after_escape: bool,
        /// 带 SGR 的那一屏。`None` = 拿不到样式(默认实现回落纯文本)。
        styled: Option<String>,
        log: RefCell<Vec<String>>,
    }

    impl FakePane {
        fn new(screens: &[&str]) -> Self {
            Self {
                screens: RefCell::new(screens.iter().rev().map(|s| s.to_string()).collect()),
                after_escape: IDLE.to_string(),
                escaped: Cell::new(false),
                refuse_with: None,
                idle_after_escape: true,
                styled: None,
                log: RefCell::new(Vec::new()),
            }
        }
        /// 给这个 pane 配一屏带 SGR 的抓屏(真机 `capture-pane -pe` 的形态)。
        fn with_styled(mut self, styled: &str) -> Self {
            self.styled = Some(styled.to_string());
            self
        }
        fn refusing(state: PaneState) -> Self {
            let mut p = Self::new(&[IDLE]);
            p.refuse_with = Some(state);
            p
        }
        fn stuck_panel(mut self) -> Self {
            self.idle_after_escape = false;
            self
        }
        /// 输入行永远拿着这条命令 —— 模拟 Enter 被 TUI 吞掉(只给一屏,`capture` 会一直返回它)。
        fn swallowing_enter(cmd: &str) -> Self {
            Self::new(&[&format!("──── summon:soul-x ──\n❯ {cmd}\n────\n")])
        }
        fn log(&self) -> Vec<String> {
            self.log.borrow().clone()
        }
        fn count(&self, what: &str) -> usize {
            self.log.borrow().iter().filter(|l| *l == what).count()
        }
    }

    impl SlashPane for FakePane {
        fn capture_styled(&self) -> anyhow::Result<String> {
            match &self.styled {
                Some(styled) => Ok(styled.clone()),
                None => self.capture(),
            }
        }
        fn capture(&self) -> anyhow::Result<String> {
            if self.escaped.get() {
                return Ok(self.after_escape.clone());
            }
            let mut s = self.screens.borrow_mut();
            Ok(if s.len() > 1 {
                s.pop().unwrap()
            } else {
                s.last().cloned().unwrap_or_default()
            })
        }
        fn submit_line(&self, text: &str) -> anyhow::Result<()> {
            if let Some(state) = self.refuse_with {
                // 与真闸同构:被拒时**没有**记下任何按键动作。
                return Err(InjectionRefused::new(state).into());
            }
            self.log.borrow_mut().push(format!("submit:{text}"));
            Ok(())
        }
        fn send_enter(&self) -> anyhow::Result<()> {
            self.log.borrow_mut().push("enter".into());
            Ok(())
        }
        fn clear_own_draft(&self) -> anyhow::Result<()> {
            self.log.borrow_mut().push("clear_own_draft".into());
            Ok(())
        }
        fn prove_idle(&self) -> anyhow::Result<PaneState> {
            self.log.borrow_mut().push("prove_idle".into());
            self.escaped.set(true);
            if self.idle_after_escape {
                Ok(PaneState::InputReady)
            } else {
                Err(InjectionRefused::after_esc(PaneState::Modal(ModalKind::Panel)).into())
            }
        }
        fn poll_interval(&self) -> Duration {
            Duration::ZERO
        }
        fn submit_grace(&self) -> Duration {
            Duration::ZERO
        }
        /// 短窗口:静止窗要**大于零**(否则第一拍就超时,连归类都轮不上),又要短到让超时
        /// 分支立刻走完。真实值(20s / 180s)由 trait 默认实现给生产路径。
        fn generic_windows(&self) -> (Duration, Duration) {
            (Duration::from_millis(30), Duration::from_millis(300))
        }
    }

    /// panel 类收尾的顺序不变量:**正文必须在按 Esc 之前抓走**。假 pane 在 `prove_idle` 之后
    /// 只返回空闲屏 —— 顺序反了的实现会把空闲屏当成面板正文回传,用户拿到的答案就没了。
    #[test]
    fn panel_body_is_captured_before_the_escape_that_closes_it() {
        let pane = FakePane::new(&[IDLE, COST_PANEL]);
        let outcome = run_pool_command_on(&pane, entry("/cost"), "/cost").unwrap();
        let SlashOutcome::Panel { body, closed } = outcome else {
            panic!("应判成面板结局: {outcome:?}");
        };
        assert!(closed, "Esc 后回到空输入框才算成功");
        assert!(body.contains("Total cost:"), "面板正文丢了: {body}");
        assert!(!body.contains("Settings  Status"), "tab 栏是 chrome");
        assert_eq!(
            pane.log().last().map(String::as_str),
            Some("prove_idle"),
            "退面板必须是最后一步: {:?}",
            pane.log()
        );
        assert_eq!(pane.count("enter"), 0, "面板已弹出就不该再补 Enter");
    }

    /// 面板退不出去时:正文照样回传(用户要的答案已经拿到了),但结局标成「没关上」——
    /// 会话此刻仍被面板堵着,报成功等于让用户以为可以接着发下一条。
    #[test]
    fn a_panel_that_survives_the_escape_still_returns_its_body_but_not_success() {
        let pane = FakePane::new(&[IDLE, COST_PANEL]).stuck_panel();
        let SlashOutcome::Panel { body, closed } =
            run_pool_command_on(&pane, entry("/cost"), "/cost").unwrap()
        else {
            panic!("应判成面板结局");
        };
        assert!(!closed);
        assert!(body.contains("Total cost:"));
        assert_eq!(pane.count("prove_idle"), 1, "退面板只试一次");
    }

    /// print 类:回传的是**这次**新增的输出,不含上一屏、不含输入框与状态行。
    #[test]
    fn print_command_returns_only_what_this_command_printed() {
        let pane = FakePane::new(&[BEFORE_CONTEXT, AFTER_CONTEXT]);
        let SlashOutcome::Printed { output, .. } =
            run_pool_command_on(&pane, entry("/context"), "/context").unwrap()
        else {
            panic!("应判成 print 结局");
        };
        assert!(output.contains("29.1k/1m tokens"), "{output}");
        assert!(!output.contains("Welcome back Alice"), "上一屏不该混进来");
        assert!(!output.contains('❯'));
        assert_eq!(pane.count("prove_idle"), 0, "print 类不需要退面板");
    }

    /// claude 答完一轮会在**空**输入框里画一句建议下一句(ghost text)。纯文本抓屏里它与用户
    /// 草稿逐字节同形,于是 print 类要求的 `InputReady` 永远等不到 —— 命令其实早跑完了,却要
    /// 撞满静止窗才回一句「不确定」。样式分得出:claude 画的那段被 SGR 2(dim)包着。
    ///
    /// 变异自验:把 `settled_state` 换回裸 `classify_pane`,本例必转成 `Uncertain`。
    #[test]
    fn a_ghost_suggestion_in_the_prompt_does_not_stall_a_print_command() {
        let after_with_ghost = "     ⛶ ⛶ ⛶ ⛶ ⛶   claude-opus-5\n\
     ⛶ ⛶ ⛶ ⛶ ⛶   29.1k/1m tokens (3%)\n\n     /context all to expand\n\
────────────────────────────\n❯ 要不要我把这段整理成表格\n────────────────────────────\n";
        let styled = "     ⛶ ⛶ ⛶ ⛶ ⛶   claude-opus-5\n\
     ⛶ ⛶ ⛶ ⛶ ⛶   29.1k/1m tokens (3%)\n\n     /context all to expand\n\
────────────────────────────\n\u{1b}[39m❯\u{a0}\u{1b}[2m要不要我把这段整理成表格\u{1b}[0m\n\
────────────────────────────\n";
        let pane = FakePane::new(&[BEFORE_CONTEXT, after_with_ghost]).with_styled(styled);
        let SlashOutcome::Printed { output, .. } =
            run_pool_command_on(&pane, entry("/context"), "/context").unwrap()
        else {
            panic!("输入框里画着建议 ≠ 用户有草稿,print 类照样该收尾");
        };
        assert!(output.contains("29.1k/1m tokens"), "{output}");
    }

    /// 反面:输入行里是**用户真草稿**(样式里没有 dim)→ 仍判 `Draft`,不许当成空输入框。
    /// 直接钉判据而不走端到端 —— 端到端要真撞满静止窗才回不确定,为一条断言睡 20s 不值。
    #[test]
    fn a_real_draft_in_the_prompt_is_never_mistaken_for_an_empty_one() {
        let screen =
            "────────────────────────────\n❯ 我自己打了一半的话\n────────────────────────────\n";
        let draft_styled =
            "────────────────────────────\n\u{1b}[39m❯\u{a0}我自己打了一半的话\n────────────────────────────\n";
        let ghost_styled = "────────────────────────────\n\
\u{1b}[39m❯\u{a0}\u{1b}[2m要不要我把这段整理成表格\u{1b}[0m\n────────────────────────────\n";

        let with_draft = FakePane::new(&[screen]).with_styled(draft_styled);
        assert_eq!(settled_state(&with_draft, screen), PaneState::Draft);

        let with_ghost = FakePane::new(&[screen]).with_styled(ghost_styled);
        assert_eq!(settled_state(&with_ghost, screen), PaneState::InputReady);

        // 拿不到样式(默认实现回落纯文本)→ 退回原判据,fail-closed。
        let blind = FakePane::new(&[screen]);
        assert_eq!(settled_state(&blind, screen), PaneState::Draft);
    }

    /// Enter 被吞的收场:① 不许 Err(键已经发过,报「没执行」是谎报);② 结局是「不确定」;
    /// ③ **必须把我们自己留在输入框里的那行清掉** —— 不清的话 pane 恒判 `Draft`,此后每一条飞书
    /// 消息都被闸拒,回执还把责任推给用户。
    #[test]
    fn a_swallowed_enter_leaves_no_residue_and_is_reported_as_uncertain() {
        let pane = FakePane::swallowing_enter("/status");
        let outcome = run_pool_command_on(&pane, entry("/status"), "/status")
            .expect("键已经发过就不许再 Err —— 那等于对用户说「本条没有执行」");
        let SlashOutcome::Uncertain { reason } = outcome else {
            panic!("应判成不确定: {outcome:?}");
        };
        assert!(reason.contains("Enter"), "{reason}");
        assert_eq!(
            pane.count("clear_own_draft"),
            1,
            "自己的残留必须清掉: {:?}",
            pane.log()
        );
        assert_eq!(pane.count("submit:/status"), 1, "注入仍是 at-most-once");
    }

    /// 收尾抓屏失败同理:提交之后的任何失败都只能是「不确定」,不能变成「本条没有执行」。
    #[test]
    fn a_capture_failure_after_submitting_is_uncertain_not_never_ran() {
        struct BlindAfterSubmit(Cell<bool>, RefCell<Vec<String>>);
        impl SlashPane for BlindAfterSubmit {
            fn capture(&self) -> anyhow::Result<String> {
                if self.0.get() {
                    anyhow::bail!("capture 失败");
                }
                Ok(IDLE.to_string())
            }
            fn submit_line(&self, text: &str) -> anyhow::Result<()> {
                self.1.borrow_mut().push(format!("submit:{text}"));
                self.0.set(true);
                Ok(())
            }
            fn send_enter(&self) -> anyhow::Result<()> {
                Ok(())
            }
            fn clear_own_draft(&self) -> anyhow::Result<()> {
                Ok(())
            }
            fn prove_idle(&self) -> anyhow::Result<PaneState> {
                Ok(PaneState::InputReady)
            }
            fn poll_interval(&self) -> Duration {
                Duration::ZERO
            }
            fn submit_grace(&self) -> Duration {
                Duration::ZERO
            }
        }
        let pane = BlindAfterSubmit(Cell::new(false), RefCell::new(Vec::new()));
        let outcome = run_pool_command_on(&pane, entry("/status"), "/status")
            .expect("提交之后的抓屏失败不许 Err 出去");
        assert!(
            matches!(outcome, SlashOutcome::Uncertain { .. }),
            "{outcome:?}"
        );
        assert_eq!(pane.1.borrow().len(), 1);
    }

    /// 被闸拒 = **一个键都没发出去**。这条同时覆盖 attach 用户草稿(Draft)与面板吞注入(Panel):
    /// 两种都必须在按键之前停手,且错误能穿过 anyhow 链被出站侧认出来(否则只会用通用文案谎报)。
    #[test]
    fn a_refused_gate_means_zero_keys_and_a_receipt_that_says_nothing_ran() {
        for state in [
            PaneState::Draft,
            PaneState::Modal(ModalKind::Panel),
            PaneState::Running,
        ] {
            let pane = FakePane::refusing(state);
            let err = run_pool_command_on(&pane, entry("/status"), "/status")
                .expect_err("闸拒时必须 Err,不能返回任何「已执行」结局");
            assert!(pane.log().is_empty(), "{state:?} 下不该有任何动作");
            let notice = crate::session::input_gate::refusal_notice_of(&err)
                .expect("拒绝要能被出站侧认出来");
            assert!(notice.contains("本条没有执行"), "{notice}");
        }
    }

    /// 等不到收尾形态时:如实回「不确定」,且**不再注入第二次**(at-most-once 是给用户的承诺)。
    /// 这里用一条零窗口的临时条目,只为立刻走到超时分支;池内条目的真实窗口由采样池自己的测试钉。
    #[test]
    fn a_command_that_never_settles_is_uncertain_and_is_never_injected_twice() {
        let unrecognized = "某个认不出的界面,既没有输入框也不是已注册面板\n";
        let quick = SlashEntry {
            name: "/context",
            class: SlashClass::Print,
            args: ArgsPolicy::Passthrough,
            args_class: None,
            stall_secs: 0,
            cap_secs: 0,
            default_enabled: true,
            evidence: "测试用零窗口条目",
        };
        let pane = FakePane::new(&[IDLE, unrecognized]);
        let outcome = run_pool_command_on(&pane, &quick, "/context").unwrap();
        assert!(
            matches!(outcome, SlashOutcome::Uncertain { .. }),
            "{outcome:?}"
        );
        assert_eq!(
            pane.count("submit:/context"),
            1,
            "注入至多一次: {:?}",
            pane.log()
        );
    }

    /// 修法 3:超时回执的秒数必须如实 —— stall(静止窗)命中时,旧回执写的却是 cap_secs
    /// (真机 `/status` stall=20s 静止即停,回执谎称「等了 60s」)。回执要报实际流逝的秒数
    /// 与命中的那道上限,两者都不许拿另一道窗口的数字顶替。
    #[test]
    fn a_timeout_receipt_reports_the_limit_that_actually_fired_not_the_cap() {
        let unsettled = "某个认不出的界面,既没有输入框也不是已注册面板\n";
        // stall 立即命中,cap(600s)远未到:回执必须报实际秒数与静止窗,不许出现 600。
        let stall_hit = SlashEntry {
            name: "/context",
            class: SlashClass::Print,
            args: ArgsPolicy::Passthrough,
            args_class: None,
            stall_secs: 0,
            cap_secs: 600,
            default_enabled: true,
            evidence: "测试:stall 秒杀而 cap 远未到",
        };
        let pane = FakePane::new(&[IDLE, unsettled]);
        let SlashOutcome::Uncertain { reason } =
            run_pool_command_on(&pane, &stall_hit, "/context").unwrap()
        else {
            panic!("应判成不确定");
        };
        assert!(
            !reason.contains("600"),
            "stall 命中不许谎报 cap 的秒数: {reason}"
        );
        assert!(reason.contains("静止"), "要说清命中的是静止窗: {reason}");
        assert!(reason.contains("等了 0s"), "实际流逝要如实: {reason}");
        // 反向:cap 命中时报硬顶,不扯静止窗的秒数。
        let cap_hit = SlashEntry {
            stall_secs: 600,
            cap_secs: 0,
            evidence: "测试:cap 秒杀而 stall 远未到",
            ..stall_hit
        };
        let pane = FakePane::new(&[IDLE, unsettled]);
        let SlashOutcome::Uncertain { reason } =
            run_pool_command_on(&pane, &cap_hit, "/context").unwrap()
        else {
            panic!("应判成不确定");
        };
        assert!(
            reason.contains("硬顶"),
            "要说清命中的是总时长硬顶: {reason}"
        );
        assert!(
            !reason.contains("600"),
            "cap 命中不许扯静止窗的秒数: {reason}"
        );
    }

    /// spec 第五节:slash **一律不附 transport marker** —— 实测
    /// `/compact <!-- summon-transport-turn:… -->` 会被 claude 把注释当成压缩指令解析进
    /// `<command-args>`。所以注入的必须是命令原文,一个字节都不多。
    #[test]
    fn the_injected_text_is_the_bare_command_with_no_transport_marker() {
        for (name, screens) in [
            ("/status", vec![IDLE, COST_PANEL]),
            ("/context", vec![BEFORE_CONTEXT, AFTER_CONTEXT]),
        ] {
            let pane = FakePane::new(&screens);
            run_pool_command_on(&pane, entry(name), name).unwrap();
            assert_eq!(
                pane.log().first().map(String::as_str),
                Some(format!("submit:{name}").as_str()),
                "注入的必须是命令原文: {:?}",
                pane.log()
            );
            assert!(
                !pane.log().iter().any(|l| l.contains("summon-transport")),
                "{:?}",
                pane.log()
            );
        }
    }

    /// 带参形态(owner 2026-08-14 授权,只开放透传类):注入的是**整行原文**(含参数、零 marker),
    /// 收尾语义沿用条目的基线类别 —— `/context all` 仍按 print 收尾,`/status foo` 仍按 panel 收尾。
    #[test]
    fn an_args_form_injects_the_full_original_line_and_settles_by_its_base_class() {
        let pane = FakePane::new(&[BEFORE_CONTEXT, AFTER_CONTEXT]);
        let outcome = run_pool_command_on(&pane, entry("/context"), "/context all").unwrap();
        assert!(
            matches!(outcome, SlashOutcome::Printed { .. }),
            "带参不改收尾类别: {outcome:?}"
        );
        assert_eq!(
            pane.log().first().map(String::as_str),
            Some("submit:/context all"),
            "注入的必须是整行原文: {:?}",
            pane.log()
        );
        assert!(!pane.log().iter().any(|l| l.contains("summon-transport")));

        let pane = FakePane::new(&[IDLE, COST_PANEL]);
        let outcome = run_pool_command_on(&pane, entry("/status"), "/status foo").unwrap();
        assert!(matches!(outcome, SlashOutcome::Panel { .. }), "{outcome:?}");
        assert_eq!(
            pane.log().first().map(String::as_str),
            Some("submit:/status foo")
        );
        assert_eq!(pane.count("submit:/status foo"), 1, "注入仍是 at-most-once");
    }

    /// turn 类(`/goal <目标>`,owner 2026-08-14 授权):它本质是 prompt,**提交即收尾** ——
    /// 整行原文注入、零 marker、at-most-once 照旧;提交确认后立刻交给轮机制,不等收尾、
    /// 不抓屏回执、更不 prove_idle(那会按 Esc,打断刚开的轮)。
    /// 把 turn 接回 settle 循环的实现在这条上必红(FakePane 永远回 IDLE,panel 判据等不到)。
    #[test]
    fn a_turn_command_is_submitted_and_handed_to_the_turn_machinery() {
        let pane = FakePane::new(&[IDLE]);
        let outcome = run_pool_command_on(&pane, entry("/goal"), "/goal 完成一个测试目标").unwrap();
        assert_eq!(outcome, SlashOutcome::HandedToTurn);
        assert_eq!(
            pane.log(),
            vec!["submit:/goal 完成一个测试目标".to_string()],
            "提交之外一个键都不该发:不补 Enter、不 prove_idle(Esc 会打断刚开的轮)"
        );
    }

    /// 同一条 `/goal` 的裸形态是 panel:目标面板正文照抓、受限 Esc 照退 —— 双形态条目的
    /// 两条收尾路互不串门。
    #[test]
    fn bare_goal_settles_as_a_panel_and_returns_the_goal_body() {
        let pane = FakePane::new(&[IDLE, GOAL_PANEL]);
        let SlashOutcome::Panel { body, closed } =
            run_pool_command_on(&pane, entry("/goal"), "/goal").unwrap()
        else {
            panic!("裸 /goal 应判成面板结局");
        };
        assert!(closed, "Esc 后回到空输入框才算成功");
        assert!(body.contains("No goal set"), "面板正文丢了: {body}");
        assert!(
            !body.to_lowercase().contains("esc to dismiss"),
            "页脚是 chrome: {body}"
        );
        assert_eq!(pane.count("prove_idle"), 1, "panel 收尾要退面板");
    }

    /// turn 形态的 Enter 被吞:与其它类别同一收场 —— 不许谎报「已交给轮」,结局是「不确定」,
    /// 且自己的残留要清掉(否则 pane 恒判 Draft,后续每条消息都被闸拒)。
    #[test]
    fn a_turn_command_with_a_swallowed_enter_is_uncertain_not_handed_off() {
        let pane = FakePane::swallowing_enter("/goal 完成一个测试目标");
        let outcome = run_pool_command_on(&pane, entry("/goal"), "/goal 完成一个测试目标")
            .expect("键已经发过就不许再 Err");
        assert!(
            matches!(outcome, SlashOutcome::Uncertain { .. }),
            "Enter 被吞不能报成已交给轮: {outcome:?}"
        );
        assert_eq!(pane.count("clear_own_draft"), 1, "自己的残留必须清掉");
        assert_eq!(
            pane.count("submit:/goal 完成一个测试目标"),
            1,
            "注入仍是 at-most-once"
        );
    }

    /// 执行器的第二道零按键守卫:entry 与整行原文配错(首 token 不是这条命令),或参数里带
    /// 换行/控制字符 —— 都必须在**发出任何按键之前** Err(Err = 可以对用户断言「本条没有执行」)。
    #[test]
    fn a_mismatched_or_control_laden_line_is_refused_before_any_key_is_sent() {
        for line in ["/cost", "/status\nfoo", "/status a\tb"] {
            let pane = FakePane::new(&[IDLE]);
            assert!(
                run_pool_command_on(&pane, entry("/status"), line).is_err(),
                "{line:?} 不该被执行"
            );
            assert!(pane.log().is_empty(), "{line:?} 下一个键都不该发出去");
        }
    }

    /// 慢命令有自己的宽窗口与卡片回执,误走通用执行器 = 用秒级窗等一条 2.5 分钟的命令,
    /// 必然误报。这条边界钉在执行器入口,而不是只靠调用方记得分流。
    #[test]
    fn slow_commands_are_rejected_by_the_generic_executor() {
        let pane = FakePane::new(&[IDLE]);
        assert!(run_pool_command_on(&pane, entry("/compact"), "/compact").is_err());
        assert!(pane.log().is_empty(), "连键都不该发");
    }

    /// 私聊回传失败时发进群的那条兜底回执:**不带正文**。裁决 2 的边界在这里再守一次 ——
    /// 「私聊发不出去就回落到群里」正是它要避免的动作。
    #[test]
    fn the_private_delivery_fallback_never_leaks_the_panel_body_into_the_group() {
        let notice = private_delivery_failed_notice("/status", "flare-5");
        assert!(notice.contains("不发进群"), "{notice}");
        assert!(notice.contains("attach flare-5"));
        // 函数签名里根本没有 body 参数 —— 想泄漏得先改签名。这里再断言一次典型正文不在其中。
        assert!(!notice.contains("someone1"));
        assert!(!notice.contains("Session ID"));
    }

    /// 池内每条 print/panel 命令都必须能被这个执行器跑到底 —— 新命令入池却没接执行路径时,
    /// 这条会红。
    #[test]
    fn every_enabled_pool_command_has_a_working_executor_path() {
        for e in pool_for(
            summon_core::driver::CliDriver::Claude,
            summon_core::config::registry::CaptureMode::Transcript,
        ) {
            let screens: Vec<&str> = match e.class {
                SlashClass::Panel => vec![
                    IDLE,
                    if e.name == "/goal" {
                        GOAL_PANEL
                    } else {
                        COST_PANEL
                    },
                ],
                SlashClass::Print => vec![BEFORE_CONTEXT, AFTER_CONTEXT],
                SlashClass::Slow => continue,
                SlashClass::Turn => unreachable!("池里没有基线 Turn 条目(授权钉在带参形态)"),
            };
            let pane = FakePane::new(&screens);
            let outcome = run_pool_command_on(&pane, e, e.name)
                .unwrap_or_else(|err| panic!("{} 没能跑完: {err:#}", e.name));
            assert!(
                !matches!(outcome, SlashOutcome::Uncertain { .. }),
                "{} 在正常收尾形态下不该判成不确定",
                e.name
            );
            // 双形态条目的带参形态(turn)同样必须能跑到底 —— 新增双形态条目却没接 turn 路时,
            // 这条会红。
            if e.args_class == Some(SlashClass::Turn) {
                let pane = FakePane::new(&[IDLE]);
                let line = format!("{} 测试目标", e.name);
                assert_eq!(
                    run_pool_command_on(&pane, e, &line).unwrap(),
                    SlashOutcome::HandedToTurn,
                    "{line} 应交给轮机制"
                );
            }
        }
    }

    // ───────── 通用执行器(准入反转的核心增量):四种归类各一条,用真机抓屏 fixture ─────────

    /// **panel 归类**:未知命令注入后弹出的界面命中已注册面板特征 → 走 panel 语义。
    /// 与池内 panel 桶逐字节同款收尾:先抓正文再按受限 Esc,回到空输入框才算成功。
    #[test]
    fn an_unknown_command_that_opens_a_registered_panel_settles_as_a_panel() {
        let pane = FakePane::new(&[IDLE, COST_PANEL]);
        let SlashOutcome::Panel { body, closed } =
            run_generic_command_on(&pane, "/some-unknown-panel").unwrap()
        else {
            panic!("命中面板特征应判成面板结局");
        };
        assert!(closed, "Esc 后回到空输入框才算成功");
        assert!(body.contains("Total cost:"), "面板正文丢了: {body}");
        assert_eq!(
            pane.log().last().map(String::as_str),
            Some("prove_idle"),
            "退面板必须是最后一步(正文在 Esc 之前抓走): {:?}",
            pane.log()
        );
        assert_eq!(pane.count("submit:/some-unknown-panel"), 1, "注入至多一次");
        assert_eq!(pane.count("enter"), 0, "面板已弹出就不该再补 Enter");
    }

    /// **print 归类**:转录区长出新内容、回到空输出框、全程没有轮迹象 → 走 print 语义。
    /// 回传的是**这次**新增的输出(锚定),不含上一屏。
    #[test]
    fn an_unknown_command_that_prints_and_returns_to_the_prompt_settles_as_print() {
        let pane = FakePane::new(&[BEFORE_CONTEXT, AFTER_CONTEXT]);
        let SlashOutcome::Printed { output, .. } =
            run_generic_command_on(&pane, "/some-unknown-print").unwrap()
        else {
            panic!("回到空输入框 + 转录区增长应判成 print 结局");
        };
        assert!(output.contains("29.1k/1m tokens"), "{output}");
        assert!(!output.contains("Welcome back Alice"), "上一屏不该混进来");
        assert_eq!(pane.count("prove_idle"), 0, "print 归类不该按 Esc");
        assert_eq!(pane.count("submit:/some-unknown-print"), 1);
    }

    /// **turn 归类**:spinner 挂着且转录区**持续**增长 = CLI 自己开了一轮真轮 →
    /// 交给调用方的真实轮机制(`HandedToTurn`),与 `/goal <目标>` 同一条路。
    /// 这里**一个多余的键都不许发**:`prove_idle` 会按 Esc,那会打断刚开的轮。
    #[test]
    fn an_unknown_command_that_opens_a_real_turn_is_handed_to_the_turn_machinery() {
        // 依次:提交确认那一拍(T0,还在思考)→ 观测三拍,转录区一行行地涨。
        let pane = FakePane::new(&[IDLE, TURN_T0, TURN_T1, TURN_T2, TURN_T3, TURN_T3]);
        let outcome = run_generic_command_on(&pane, "/some-unknown-prompt").unwrap();
        assert_eq!(outcome, SlashOutcome::HandedToTurn);
        assert_eq!(
            pane.log(),
            vec!["submit:/some-unknown-prompt".to_string()],
            "提交之外一个键都不该发:不补 Enter、不 prove_idle(Esc 会打断刚开的轮)"
        );
    }

    /// **判不出**:停在认不出的静止画面 → 诚实回「不确定」,而且**绝不重注**。
    /// 「画面静止」在通用路上同样不单独构成任何结论 —— 那正是 spec 第一节的事故。
    #[test]
    fn an_unknown_command_that_cannot_be_classified_is_reported_as_uncertain() {
        let pane = FakePane::new(&[IDLE, UNRECOGNIZED]);
        let SlashOutcome::Uncertain { reason } =
            run_generic_command_on(&pane, "/some-unknown-weird").unwrap()
        else {
            panic!("认不出的静止画面必须判成不确定");
        };
        assert!(reason.contains("归类"), "要说清是归不了类: {reason}");
        assert!(reason.contains("静止"), "要说清命中的是静止窗: {reason}");
        assert_eq!(
            pane.count("submit:/some-unknown-weird"),
            1,
            "注入至多一次: {:?}",
            pane.log()
        );
        assert_eq!(pane.count("prove_idle"), 0, "认不出的画面上一个键都不许按");
    }

    /// 通用路与池内路共用**同一条注入闸**:被拒 = 一个键都没发出去,回执说「本条没有执行」。
    /// 准入反转把「拒绝」的位置从命令名挪到了 pane 状态上,这道闸因此更要钉住。
    #[test]
    fn the_generic_path_shares_the_same_injection_gate_and_zero_key_contract() {
        for state in [
            PaneState::Draft,
            PaneState::Modal(ModalKind::Panel),
            PaneState::Running,
        ] {
            let pane = FakePane::refusing(state);
            let err = run_generic_command_on(&pane, "/some-unknown")
                .expect_err("闸拒时必须 Err,不能返回任何「已执行」结局");
            assert!(pane.log().is_empty(), "{state:?} 下不该有任何动作");
            let notice = crate::session::input_gate::refusal_notice_of(&err)
                .expect("拒绝要能被出站侧认出来");
            assert!(notice.contains("本条没有执行"), "{notice}");
        }
    }

    /// 通用路的零按键守卫:不是 `/` 开头、或带换行/控制字符 —— 都必须在**发出任何按键之前**
    /// Err。池内路靠 entry 名字兜住了第一条,通用路没有 entry 可对,守卫只能自己写。
    #[test]
    fn a_non_slash_or_control_laden_line_is_refused_by_the_generic_path_before_any_key() {
        for line in ["随便说句话", "/foo\nbar", "/foo a\tb"] {
            let pane = FakePane::new(&[IDLE]);
            assert!(
                run_generic_command_on(&pane, line).is_err(),
                "{line:?} 不该被执行"
            );
            assert!(pane.log().is_empty(), "{line:?} 下一个键都不该发出去");
        }
    }

    /// Enter 被吞:通用路与池内路同一收场 —— 不许 Err(键已经发过),结局是「不确定」,
    /// 且必须把我们自己留在输入框里的那行清掉(否则 pane 恒判 `Draft`,后续每条消息都被闸拒)。
    #[test]
    fn a_generic_command_with_a_swallowed_enter_leaves_no_residue_and_is_uncertain() {
        let pane = FakePane::swallowing_enter("/some-unknown");
        let outcome = run_generic_command_on(&pane, "/some-unknown")
            .expect("键已经发过就不许再 Err —— 那等于对用户说「本条没有执行」");
        assert!(
            matches!(outcome, SlashOutcome::Uncertain { .. }),
            "{outcome:?}"
        );
        assert_eq!(pane.count("clear_own_draft"), 1, "自己的残留必须清掉");
        assert_eq!(
            pane.count("submit:/some-unknown"),
            1,
            "注入仍是 at-most-once"
        );
    }

    /// 分派入口:`SlashInjection` 选中哪条执行路。池内条目走调优判据(`/cost` 的 panel 窗口),
    /// 未知命令走通用观测 —— 两条路的注入原文都是**整行原文、零 marker**。
    #[test]
    fn the_injection_source_selects_the_executor_and_both_inject_the_bare_line() {
        let pool = FakePane::new(&[IDLE, COST_PANEL]);
        assert!(matches!(
            run_slash_on(&pool, SlashInjection::Pool(entry("/cost")), "/cost").unwrap(),
            SlashOutcome::Panel { .. }
        ));
        assert_eq!(pool.log().first().map(String::as_str), Some("submit:/cost"));

        let generic = FakePane::new(&[BEFORE_CONTEXT, AFTER_CONTEXT]);
        assert!(matches!(
            run_slash_on(&generic, SlashInjection::Generic, "/whatever").unwrap(),
            SlashOutcome::Printed { .. }
        ));
        assert_eq!(
            generic.log().first().map(String::as_str),
            Some("submit:/whatever")
        );
        for log in [pool.log(), generic.log()] {
            assert!(
                !log.iter().any(|l| l.contains("summon-transport")),
                "零 marker: {log:?}"
            );
        }
    }

    #[test]
    fn input_holds_only_while_the_command_sits_unsent_in_the_prompt() {
        let typed = "──── summon:soul-x ──\n❯ /status\n────\n  ⏵⏵ bypass\n";
        assert!(input_holds(typed, "/status"));
        assert!(!input_holds(typed, "/cost"), "别的命令的草稿不算我们的");
        assert!(!input_holds(IDLE, "/status"), "空输入框 = 已提交");
        // 面板已经弹出来 = 命令执行了,此时**绝不能**再补 Enter(会打在面板上)。
        let panel =
            "▔▔▔▔▔▔▔▔▔▔▔▔\n   Settings  Status   Config   Usage   Stats\n\n   Esc to cancel\n";
        assert!(!input_holds(panel, "/status"));
    }

    #[test]
    fn private_query_success_has_one_delivery_with_complete_status() {
        for outcome in [
            SlashOutcome::Panel {
                body: "account details".into(),
                closed: true,
            },
            SlashOutcome::Printed {
                output: "previous conversation".into(),
                anchored: false,
            },
        ] {
            let receipt = compose_slash_receipt("/status", &outcome, true, "a");
            assert!(receipt.chat_body.is_none(), "成功私聊不应再建群回执");
            assert!(receipt.private_body.is_some());
            assert!(receipt.ok);
        }
        let receipt = compose_slash_receipt(
            "/status",
            &SlashOutcome::Panel {
                body: "account details".into(),
                closed: true,
            },
            true,
            "a",
        );
        assert!(receipt.private_body.unwrap().contains("已退出面板"));
    }

    #[test]
    fn private_query_blockage_keeps_a_group_warning_without_account_details() {
        let receipt = compose_slash_receipt(
            "/status",
            &SlashOutcome::Panel {
                body: "secret account".into(),
                closed: false,
            },
            true,
            "a",
        );
        assert!(!receipt.ok);
        let warning = receipt.chat_body.unwrap();
        assert!(warning.contains("面板没能退出"));
        assert!(!warning.contains("secret account"));
        let private = receipt.private_body.unwrap();
        assert!(private.contains("secret account"));
        assert!(private.contains("面板没能退出"));
    }

    /// 没有输出的命令只是确认照做了;有输出、不确定的回执都要发。
    #[test]
    fn only_a_silent_success_is_a_pure_confirmation() {
        let silent = SlashOutcome::Printed {
            output: " \n".into(),
            anchored: true,
        };
        for is_group in [true, false] {
            let receipt = compose_slash_receipt("/color", &silent, is_group, "a");
            assert_eq!(
                receipt.chat_body.as_deref(),
                Some("`/color` 已执行,没有可见输出。"),
                "表情贴不上时还要靠这句"
            );
            assert!(receipt.ok && receipt.only_confirms);
        }
        let printed = SlashOutcome::Printed {
            output: "blue".into(),
            anchored: true,
        };
        let uncertain = SlashOutcome::Uncertain {
            reason: "超时".into(),
        };
        for outcome in [printed, uncertain] {
            assert!(!compose_slash_receipt("/color", &outcome, true, "a").only_confirms);
        }
    }

    /// 私聊成功时原会话没有消息，账号正文只出现在操作者的结果里。
    #[test]
    fn panel_body_never_appears_in_the_group_receipt() {
        let outcome = SlashOutcome::Panel {
            body: "Email: someone1@example.com\nSession ID: 9a86241d".into(),
            closed: true,
        };
        let group = compose_slash_receipt("/status", &outcome, true, "flare-5");
        assert!(group.chat_body.is_none());
        let private = group.private_body.expect("群里必须把全文改投私聊");
        assert!(private.contains("someone1@example.com"));
        // 私聊里没有第三者,全文就地给,不再多绕一条消息。
        let p2p = compose_slash_receipt("/status", &outcome, false, "flare-5");
        assert_eq!(p2p.private_body, None);
        assert!(p2p
            .chat_body
            .as_deref()
            .unwrap()
            .contains("someone1@example.com"));
    }

    /// 锚失配抓屏可能包含其它对话和文件；私聊必须同时说明输出口径。
    #[test]
    fn an_unanchored_print_output_goes_private_and_never_claims_to_be_the_command_output() {
        let leaky = SlashOutcome::Printed {
            output: "上一轮对话正文:密钥是 hunter2\n工具输出:/Users/…/secret.env".into(),
            anchored: false,
        };
        let group = compose_slash_receipt("/context", &leaky, true, "flare-5");
        assert!(group.chat_body.is_none());
        let private = group.private_body.expect("对不齐的正文必须改投私聊");
        assert!(private.contains("hunter2"));
        assert!(private.contains("对不齐"), "私聊里也要写清口径: {private}");
        // 锚对上的正常输出照旧留在群里(那确实只是这条命令打印的东西)。
        let clean = SlashOutcome::Printed {
            output: "29.1k/1m tokens (3%)".into(),
            anchored: true,
        };
        let ok = compose_slash_receipt("/context", &clean, true, "flare-5");
        assert_eq!(ok.private_body, None);
        assert!(ok.chat_body.as_deref().unwrap().contains("29.1k/1m tokens"));
    }

    /// 回传正文一律裹代码围栏:pane 里的 `#` `|` `-` 会被飞书当 markdown 解析,表格与进度条渲染
    /// 出来就不成形了。
    #[test]
    fn returned_pane_text_is_always_fenced() {
        let body = "| Settings | Status |\n# 不是标题\n███████ 23% used";
        for (outcome, is_group) in [
            (
                SlashOutcome::Panel {
                    body: body.into(),
                    closed: true,
                },
                false,
            ),
            (
                SlashOutcome::Printed {
                    output: body.into(),
                    anchored: true,
                },
                false,
            ),
        ] {
            let r = compose_slash_receipt("/cost", &outcome, is_group, "a");
            assert!(
                r.chat_body.as_deref().unwrap().contains("```"),
                "{}",
                r.chat_body.as_deref().unwrap()
            );
        }
        let grouped = compose_slash_receipt(
            "/cost",
            &SlashOutcome::Panel {
                body: body.into(),
                closed: true,
            },
            true,
            "a",
        );
        assert!(grouped.private_body.unwrap().contains("```"));
    }

    /// 回执显示的是**完整命令原文**:`/context all` 的回执不能写成 `/context`(那是另一条命令)。
    #[test]
    fn the_receipt_names_the_full_original_line_not_just_the_command() {
        let r = compose_slash_receipt(
            "/context all",
            &SlashOutcome::Printed {
                output: "29.1k/1m tokens".into(),
                anchored: true,
            },
            false,
            "a",
        );
        assert!(
            r.chat_body.as_deref().unwrap().contains("`/context all`"),
            "{}",
            r.chat_body.as_deref().unwrap()
        );
    }

    /// 面板没退出去 = 会话还堵着,回执必须说清楚并按失败收尾(而不是贴个 ✅ 让人以为没事)。
    #[test]
    fn a_panel_that_stayed_open_is_not_reported_as_success() {
        let stuck = SlashOutcome::Panel {
            body: "…".into(),
            closed: false,
        };
        let r = compose_slash_receipt("/cost", &stuck, false, "a");
        assert!(!r.ok);
        assert!(
            r.chat_body.as_deref().unwrap().contains("面板没能退出"),
            "{}",
            r.chat_body.as_deref().unwrap()
        );
        assert!(r.chat_body.as_deref().unwrap().contains("Esc"));
    }

    /// 不确定必须写成不确定,并明说**不会重发** —— at-most-once 是给用户的承诺,不只是实现细节。
    #[test]
    fn uncertain_outcome_says_so_and_promises_no_retry() {
        let r = compose_slash_receipt(
            "/context",
            &SlashOutcome::Uncertain {
                reason: "等了 60s 没等到收尾形态".into(),
            },
            true,
            "starlight",
        );
        assert!(!r.ok);
        assert!(r.chat_body.as_deref().unwrap().contains("不确定"));
        assert!(r.chat_body.as_deref().unwrap().contains("不会重发"));
        assert!(r.chat_body.as_deref().unwrap().contains("attach starlight"));
        assert_eq!(r.private_body, None, "不确定态没有面板全文可发");
    }

    /// turn 类回执:说清「已提交」「回复走正常轮」「不再有第二份判决」三件事,且显示整行原文。
    /// 它不宣称「已执行」「轮已开始」—— 执行器只证明了提交;也没有面板全文可发私聊。
    #[test]
    fn a_handed_to_turn_receipt_promises_the_reply_on_the_normal_turn() {
        let r = compose_slash_receipt(
            "/goal 完成一个测试目标",
            &SlashOutcome::HandedToTurn,
            true,
            "flare-5",
        );
        assert!(r.ok);
        assert!(
            r.chat_body
                .as_deref()
                .unwrap()
                .contains("`/goal 完成一个测试目标`"),
            "{}",
            r.chat_body.as_deref().unwrap()
        );
        assert!(
            r.chat_body.as_deref().unwrap().contains("已提交"),
            "{}",
            r.chat_body.as_deref().unwrap()
        );
        assert!(
            r.chat_body.as_deref().unwrap().contains("正常一轮"),
            "{}",
            r.chat_body.as_deref().unwrap()
        );
        assert!(
            r.chat_body.as_deref().unwrap().contains("第二份判决"),
            "{}",
            r.chat_body.as_deref().unwrap()
        );
        assert!(
            !r.chat_body.as_deref().unwrap().contains("已执行"),
            "只证明了提交,不许说已执行"
        );
        assert_eq!(r.private_body, None, "turn 类没有面板全文可发");
    }

    /// 静止判据看的是类别相关的正文,不是整屏 —— 底部状态行的计时每秒都在走,
    /// 拿整屏比对会让「连续两拍一致」永远不成立、每条命令都等到超时。
    #[test]
    fn stability_ignores_the_ticking_status_bar() {
        let t1 = "输出第一行\n────\n❯ \n────\n   󰥔 00:28:02\n";
        let t2 = "输出第一行\n────\n❯ \n────\n   󰥔 00:28:03\n";
        assert_ne!(t1, t2, "整屏确实不同");
        assert_eq!(
            settle_content(SlashClass::Print, t1),
            settle_content(SlashClass::Print, t2),
            "转录区应当一致"
        );
    }
}
