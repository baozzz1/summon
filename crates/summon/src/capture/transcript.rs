//! 转录捕获源(Transcript,claude opt-in):claude 仍交互式跑在 tmux(订阅计费不变),但本轮回复
//! **不靠抓屏**,改成 tail claude 自己的结构化转录 jsonl(append-only、CLI 协议
//! 标注,比抓 TUI 屏幕可靠)。注入仍走 tmux send-keys(经 `Session`);读输出经 `Executor` tail
//! `~/.claude/projects/<dir>/<钉死 sid>.jsonl`。
//!
//! **钉死 sid、绝不取目录最新**(评审 G8):多个 chat 共用同一 cwd → 同一 project 目录,「取最新」会
//! 读到别 chat 的转录、跨会话串流；仅定位并读取指定原生 SID 的文件。
//!
//! turn-end 判定信 claude 自己的 `stop_reason`(R1 的 `parse_stream_json_line` 已解析进
//! `AgentMessage.stop_reason`,这里只消费):**`end_turn` = 本轮终态**;`tool_use`/`stop_sequence`
//! = 中途(后者是 `No response requested.` 这类伪影),继续 tail。判定**与内容块类型解耦**——
//! claude 把同一条消息的 thinking/text 块拆成相邻多行、每行都带同一个 stop_reason,终态可能只写在
//! thinking 那行(见 `END_TURN_GRACE`)。无新字节 ~240s = stall 看门狗,按超时返回累积文本。

use std::time::{Duration, Instant};

use anyhow::Context as _;

// `is_turn_end` 收在 core:捕获层「何时算一轮结束」与历史投影 `observe::history`「一轮何时收口成
// completed」必须同判。两边曾分叉——这里早就认 end_turn 收尾,history 却只认无头路径的 `result`,
// 于是 claude 会话的每一轮在 `agents history` 里永远显示 running。
use summon_core::agent_msg::{is_turn_end, AgentKind, AgentMessage};
#[cfg(test)]
use summon_core::claude_log::claude_transcript_path;
use summon_core::claude_log::project_dirname;
use summon_core::interactive::{parse_interactive, InteractiveState};
use summon_core::pane_input::InjectGate;
use summon_core::retry_wait::{parse_retry_wait, RetryWait};
use summon_core::scrubber::pane_proves_session_gone;
use summon_core::stream_json::parse_stream_json_line;

use super::{MediationCard, TurnOutcome};
use crate::observe::log::log;
use crate::runtime::executor::{Executor, FileIdentity};
use crate::runtime::fsio::atomic_write;
use crate::session::session::Session;

/// 只把 NotFound 当作文件尚未创建；权限与其它 IO 错误不得变成冷启动预测。
fn transcript_file_exists(path: &std::path::Path) -> anyhow::Result<bool> {
    match std::fs::metadata(path) {
        Ok(meta) => {
            anyhow::ensure!(
                meta.is_file(),
                summon_i18n::format!("r92a61419a2", path.display())
            );
            Ok(true)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => {
            Err(error).with_context(|| summon_i18n::format!("r2a7ff8d689", path.display()))
        }
    }
}

/// 按原生 SID 搜索唯一转录。多个项目目录有同 SID 时，文件系统枚举顺序不构成归属证据。
fn find_transcript_in_projects(
    projects: &std::path::Path,
    sid: &str,
) -> anyhow::Result<Option<std::path::PathBuf>> {
    let entries = match std::fs::read_dir(projects) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context(summon_i18n::text!("r356d41dc91")),
    };
    let filename = format!("{sid}.jsonl");
    let mut found = None;
    for entry in entries {
        let entry = entry.context(summon_i18n::text!("rd687b564b1"))?;
        let kind = entry.file_type()?;
        if !kind.is_dir() && !kind.is_symlink() {
            continue;
        }
        let candidate = entry.path().join(&filename);
        if transcript_file_exists(&candidate)? {
            anyhow::ensure!(found.is_none(), summon_i18n::format!("r412aa6bcb7"));
            found = Some(candidate);
        }
    }
    Ok(found)
}

fn missing_transcript(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound)
    })
}

/// 文件出现前允许修正预测路径。绑定后的 tail/checkpoint 仍只读取已选中的文件。
struct MissingTranscriptSearch {
    projects: std::path::PathBuf,
    sid: String,
    interval: Duration,
    next_search: Instant,
}

impl MissingTranscriptSearch {
    fn new(sid: &str) -> anyhow::Result<Self> {
        let home = dirs::home_dir().context(summon_i18n::text!("r794aefbfad"))?;
        Ok(Self::in_projects(
            home.join(".claude/projects"),
            sid,
            Duration::from_secs(1),
        ))
    }

    fn in_projects(projects: std::path::PathBuf, sid: &str, interval: Duration) -> Self {
        Self {
            projects,
            sid: sid.to_owned(),
            interval,
            next_search: Instant::now(),
        }
    }

    fn relocate_missing<E: Executor>(
        &mut self,
        exec: &E,
        path: &mut std::path::PathBuf,
    ) -> anyhow::Result<bool> {
        match exec.file_len(path) {
            Ok(_) => return Ok(false),
            Err(error) if missing_transcript(&error) => {}
            Err(error) => return Err(error),
        }
        let now = Instant::now();
        if now < self.next_search {
            return Ok(false);
        }
        self.next_search = now + self.interval;
        let Some(found) = find_transcript_in_projects(&self.projects, &self.sid)? else {
            return Ok(false);
        };
        if found == *path {
            return Ok(false);
        }
        log(&summon_i18n::format!(
            "r6a2a179beb",
            path.display(),
            found.display()
        ));
        *path = found;
        Ok(true)
    }
}

/// 冷启 seed 等 `end_turn` 用的转录定位。**不盲信推导路径**:`project_dirname` 是对 Claude 内部
/// 编码规则的逆向,推导错了就会去 poll 一个永不存在的文件、一直等到 seed 超时(现场:blue-wing
/// 的 `My_App` 每次冷启白等 600s,第一条消息被拖十分钟)。
///
/// SID 是新铸的 UUID、全局唯一,所以「按文件名搜」是不依赖任何编码规则的权威兜底。文件由 claude
/// 在起轮时创建,故这里带一个有界轮询窗口等它出现;窗口内没出现就返回 None,由调用方退回抓屏
/// 看门狗 —— 那条路虽然糙,但至少会动,比对着不存在的路径傻等强。
///
/// `alive` 让会话中途退出时立刻放弃,不空转满整个窗口。
pub(crate) fn resolve_seed_transcript(
    work_dir: &str,
    sid: &str,
    poll: std::time::Duration,
    window: std::time::Duration,
    alive: &mut dyn FnMut() -> bool,
) -> anyhow::Result<Option<std::path::PathBuf>> {
    let home = dirs::home_dir().context(summon_i18n::text!("r140badce1a"))?;
    let projects = home.join(".claude/projects");
    let original = transcript_path_in_projects(&projects, work_dir, sid);
    if transcript_file_exists(&original)? {
        return Ok(Some(original));
    }
    let derived = predict_transcript_path(&projects, work_dir, sid)?;
    let deadline = std::time::Instant::now() + window;
    loop {
        for path in [&original, &derived] {
            if transcript_file_exists(path)? {
                return Ok(Some(path.clone()));
            }
        }
        if let Some(found) = find_transcript_in_projects(&projects, sid)? {
            log(&summon_i18n::format!(
                "rb4af9371aa",
                found.display(),
                derived.display()
            ));
            return Ok(Some(found));
        }
        if std::time::Instant::now() >= deadline || !alive() {
            return Ok(None);
        }
        std::thread::sleep(poll);
    }
}

/// 无新字节多久判 stall(会话疑似卡死)。与 scrape 路径的看门狗同量级。
const STALL_WINDOW: Duration = Duration::from_secs(240);
/// **工具在飞**时的 stall 上限。工具执行期间转录一个字节都不产出,拿 `STALL_WINDOW` 去判,会把
/// 「正在等一个慢工具」误判成「这一轮结束了」——上层据此贴 DONE、把半截累积文本当终态回复发出去,
/// 而 claude 还在跑(现场 2026-07-22:blue-wing 等一个 lark-cli 网络调用静默 272s,超阈值 32s 就被
/// 判收尾)。
///
/// 「最后见到的是 tool_use、还没见到它的结果」是 claude **活着且在干活**的正面证据,此时应放宽而
/// 不是收尾。仍然有界:工具真的永远挂住时不至于把 chat 线程钉死。
///
/// 注:`cold_seed_wait` 早就因同一原因从抓屏看门狗改成读 `end_turn`(见 session.rs 注释),常规轮
/// 这条路当时没跟着改,只是把「看屏幕静默」换成「看转录静默」,判断的错误本质没变:**拿「没信号」
/// 当「有结论」**。
const TOOL_STALL_WINDOW: Duration = Duration::from_secs(30 * 60);

/// **转录静默的硬顶**。pane 在动只是「claude 进程活着」的证据,不是「这一轮还会有产出」的证据 ——
/// footer 里的时钟、光标闪烁都会让画面永远变。所以存活证据只能延期,不能无限延期:转录连续这么久
/// 零字节,无论画面怎么动都收尾。取值同 `TOOL_STALL_WINDOW`,理由一样(工具/生成都可能真的很慢,
/// 但半小时零产出就该有人来看)。
const HARD_STALL_CEILING: Duration = Duration::from_secs(30 * 60);

/// 本轮该用多长的 stall 窗口。抽成纯函数便于单测:这条判断决定「何时宣布一轮结束」,判错就是把
/// 没结束的轮当成结束,是最贵的一类错。
fn stall_window(awaiting_tool: bool) -> Duration {
    if awaiting_tool {
        TOOL_STALL_WINDOW
    } else {
        STALL_WINDOW
    }
}

/// 是否该按 stall 收尾。**沉默不是死亡的证据**——这条判断只在拿到「没有任何存活证据」时才成立。
///
/// 转录会在两种完全无辜的情况下长时间零字节:① 工具在飞(已由 `stall_window` 放宽到 30min);
/// ② **模型在生成**——claude 只在整条 assistant 消息生成完才落盘 tool_use 记录,所以「长思考 +
/// 大文件写入」期间转录一个字节都没有,和会话卡死长得一模一样。现场 2026-08-04:SUM-14 的 run 在
/// 18:29:04 写完最后一条 tool_result 后开始生成一份 159 行的设计文档,240s 后被判 stall 收尾,而
/// agent 又正常工作了 7 分 55 秒才把 commit 打完(提交时间 18:40:59)。②式误判此前完全没被覆盖。
///
/// 区分二者的正面证据是 **pane 画面在变**:claude 生成时 TUI 的 spinner / token 计数每秒都在刷新,
/// 闲置会话的画面则一个字节都不动。抓屏本来就每 2s 在跑(菜单探测),这里零成本复用。
///
/// 判别力实测(2026-08-04,1s 采样):正在跑一轮的 `soul-arcadia-g00000000a11ce03c` 连采 10 次得到
/// **10 个互不相同**的整屏哈希;同一时刻闲置的 `soul-blue-wing` 连采 8 次是**同一个**哈希。两者不重叠。
///
/// - `since_transcript`:转录最后一次产出字节至今。
/// - `since_liveness`:最后一次拿到存活证据(转录字节 **或** pane 画面变化)至今。
/// - `awaiting_tool`:是否有未配对的工具在飞。
fn should_stall(since_transcript: Duration, since_liveness: Duration, awaiting_tool: bool) -> bool {
    // 硬顶优先:画面永远在动也不能无限延期(见 HARD_STALL_CEILING)。
    since_transcript >= HARD_STALL_CEILING || since_liveness >= stall_window(awaiting_tool)
}

/// **候选终态的确认窗**。claude 把同一条 assistant 消息**按内容块拆成相邻多条 JSONL 记录**,每条
/// 都带同一个 `stop_reason`(真机 60 份转录统计:788 条 end_turn 消息里,430 条是 `thinking`+`text` 拆
/// 成紧邻两行、两行都写 end_turn;347 条只有 `text`;9 条**只有 thinking**;2 条只有两条 thinking)。
///
/// 于是两种极端都错:「见 end_turn 就收轮」会在 55% 的轮上收在 thinking 那行、把真正的回复丢掉;
/// 「只在 assistant 文本行收轮」则在纯 thinking 收尾时永远收不了轮(2026-07-27 现场:arcadia 一轮
/// 终态只有 thinking 块,chat 线程直到 stall 兜底前一直报「跑一轮中」,后续消息全堵在队列)。
///
/// 折中:非文本记录上的 end_turn 只记成**候选**,同一条消息的文本记录若随后到达(实测恒为紧邻下一
/// 行、同一次写入)就走原路径收轮;静默满本窗口仍无后继,才确认这一轮真以 thinking 结束。
/// 窗口取 3s = 10 个 tail 轮询,远大于「同一次写入的两行」间隔;万一仍误判(文本行迟到),后果也只是
/// 该文本被 mirror 当作新 turn 转发,不会丢。
pub(crate) const END_TURN_GRACE: Duration = Duration::from_secs(3);
/// tail 轮询间隔。
const POLL: Duration = Duration::from_millis(300);
/// live 卡计时心跳节拍。取 5s:够让「⟳ cargo test 3m12s」看起来在走,又远低于飞书 PATCH 的节流阈值,
/// 不会把卡片刷成抖动。只在转录静默时才有意义(有字节时事件本身就会刷卡)。
const TICK_EVERY: Duration = Duration::from_secs(5);
/// 消费回显阶段的抓屏探测节拍。
const PROBE_EVERY: Duration = Duration::from_secs(2);
/// 发出交互中介卡片后,等用户在飞书里操作的硬上限——超时按累积收尾(不永久阻塞 chat 线程)。
const INTERACTIVE_MAX_WAIT: Duration = Duration::from_secs(600);
/// 交互中介卡未达时的重发次数上限(同一张卡)。重发挂在探测节拍(`PROBE_EVERY`)上:不新起线程、
/// 不在轮询里 sleep,菜单一消失重发就自然停,点击时效性另有抓屏复验兜底。取 3 次覆盖一次几秒级的
/// 网络抖动;每次发送本身最长阻塞一个 HTTP 超时(30s),再多就会把 tail 与打断轮询压得太久。
const INTERACTIVE_SEND_TRIES: u32 = 3;
/// **送达相**上限:等本轮随机 marker 在 pinned transcript 里首次露面(任何形态)。这一相只防
/// 「send-keys 根本没落到 pane 上 / claude 已死」,实测亚秒级(marker 以 queue-operation/enqueue
/// 形态落盘在 Enter 后 61–149ms)。超时必须失败,不能退回「baseline 后第一条 end_turn 就算本轮」。
///
/// 只有本轮 marker 的 `Enqueued` / `Consumed` 才是送达证据。旧 turn 的工具输出、hook 记录等无关
/// 字节即使持续增长，也不能替一次丢失的 send-keys 续命；否则 15s 检出会悄悄退化到 10min。
/// 唯一的放宽是注入后出现的不带正文的入队记录(claude 压缩上下文时收下输入),见
/// `note_queued_without_text`。
const PROMPT_ARRIVAL_TIMEOUT: Duration = Duration::from_secs(15);
/// 送达相里看屏补按回车的节拍与次数上限。会话刚起来(冷启 / resume)就注入时,粘贴之后紧跟的
/// 回车会被 claude 整批吞掉,正文停在输入框里、转录一个字节都不写(2026-10-27 真机 claude
/// 2.1.311,同日两次,另见 10-23 一次)—— 注入时那几次补发都在 250ms 内,救不了这种情形。
/// 节拍取 2s:正常送达 marker 在回车后 61–149ms 落盘,首拍之前早已离开送达相;3 次排在 6s 内,
/// 离 `PROMPT_ARRIVAL_TIMEOUT` 还留着给最后一次回车送达的余量。
const ARRIVAL_RESUBMIT_EVERY: Duration = Duration::from_secs(2);
const ARRIVAL_RESUBMIT_TRIES: u32 = 3;
/// **消费相**硬止损:已确认入队后,等 claude 真把这条 prompt 出队消费。排队窗口 = claude 当前在跑
/// 的工具的剩余时间,**原则上无上界**——实测 6.9s / 9.3s / 29.7s / 152.7s。所以这个值是防死锁的
/// 顶,不是对正常时长的估计:入队已经证明消息到了 claude 手里,此后再判失败纯属自伤(消息终将被
/// 回答,而 summon 已经报了错 → 用户收到两份回应)。取值与同文件 `INTERACTIVE_MAX_WAIT` 一致,
/// 两者都以「阻塞 chat 线程」换「不误判」,取舍同源。
const PROMPT_CONSUME_TIMEOUT: Duration = Duration::from_secs(600);
/// 一条 user transcript 记录的缓冲上限。入站消息本身远低于此值；上限只防坏/无换行文件撑内存。
const PROMPT_ECHO_LINE_LIMIT: usize = 8 * 1024 * 1024;
/// deadline 救援只扫描 transcript 尾部的有界窗口。当前随机 marker 正常紧邻 EOF；若一轮排队期间
/// 已在它之后追加超过此预算的数据，宁可 exact marker 救援失败，也不能把长寿命历史整份搬进内存。
const PROMPT_RESCUE_WINDOW_BYTES: usize = 8 * 1024 * 1024;
/// 主 tail 每次重读上一段短后缀作为 generation checkpoint。它同时覆盖 rename/replace、
/// 同 inode truncate+迅速长回、以及拿不到 inode 的 Executor；匹配后才把新增后缀交给 parser。
const TAIL_CHECKPOINT_BYTES: usize = 1024;
/// 文件换代时用于逐字节对齐「已经触发过回调」的当前 turn 前缀。正常路径最多额外占 8MiB；
/// 超预算只影响极端换代恢复，平时仍照常流式处理。
const TURN_REANCHOR_PREFIX_LIMIT: usize = 8 * 1024 * 1024;
/// mirror 的 Ready 主闸最多等 10s；空 transcript 可能恰好要等首个 prompt 才产生首行，因此
/// watcher 必须更早进入 Failed，让主路径放行注入，不能形成 Starting ↔ 首行的循环依赖。
const MIRROR_EMPTY_START_TIMEOUT: Duration = Duration::from_secs(3);

/// 交互中介卡的重发预算。key = 菜单签名(降级卡没有菜单,用 `None`);换一张卡就换 key、预算重新计,
/// 所以一张卡发废了不会连累下一张。
#[derive(Default)]
struct CardSendBudget {
    key: Option<u64>,
    failures: u32,
}

impl CardSendBudget {
    /// 这张卡是否已经发到不能再发。
    fn spent(&self, key: Option<u64>) -> bool {
        self.key == key && self.failures >= INTERACTIVE_SEND_TRIES
    }

    /// 有卡未达、且预算还没用尽 —— 即「正等下一个探测节拍重发」。stall 看门狗据它让位:
    /// 降级卡的发送条件本身就是 `should_stall`,不让位的话看门狗会在同一次迭代里当场收轮,
    /// 重发永远等不到第二次机会。预算用尽后它归 false,看门狗照常收口。
    fn retrying(&self) -> bool {
        (1..INTERACTIVE_SEND_TRIES).contains(&self.failures)
    }

    /// 记一次未达。
    fn note_failure(&mut self, key: Option<u64>) {
        if self.key == key {
            self.failures += 1;
        } else {
            *self = Self { key, failures: 1 };
        }
    }

    fn clear(&mut self) {
        *self = Self::default();
    }
}

/// AskUserQuestion 的 TUI 菜单在真实选项后还跟「Type something.」「Chat about this」两个 UI 元素,
/// 抓屏识别会把它们一并收进 options。按转录侧记录的真实选项数截断,只留前 N 项(真实选项恒在
/// UI 元素之前)。仅当菜单选项数恰好 = 真实选项数 + 2(两个 UI 元素)时才截断——多题场景第二题
/// 选项数与第一题不同、或 TUI 改版导致 UI 元素数变化时,计数对不上就不截,原样发卡(安全降级)。
/// `count` 为 None(非 AskUserQuestion)时同样原样返回。
fn filter_ask_user_question_menu(menu: InteractiveState, count: Option<usize>) -> InteractiveState {
    match count {
        Some(n) if n > 0 && menu.options.len() == n + 2 => InteractiveState {
            options: menu.options.into_iter().take(n).collect(),
            ..menu
        },
        _ => menu,
    }
}

/// 整屏画面签名,用来判「这两拍之间画面变没变」(见 `should_stall`)。**整屏取哈希、不做任何清洗**:
/// claude 生成时刷新的正是 spinner / 计时 / token 计数这些「状态噪音」,`TmuxDriver::snapshot` 那种
/// 剥 chrome 的做法会把唯一的存活证据一起剥掉。
fn pane_signature(pane: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    pane.hash(&mut h);
    h.finish()
}

/// 每次注入都追加随机、仅用于 transport 相关性的 marker。baseline 只排除历史字节，marker 再把
/// “baseline 后迟到的旧 turn 终态”与真正的新 user turn 分开；两层缺一都会在 daemon 重启竞态下
/// 把旧 end_turn 误认成新 run。
fn prompt_with_transport_marker(prompt: &str) -> (String, String) {
    let marker = format!("summon-transport-turn:{}", uuid::Uuid::new_v4());
    // shell 直通只能单行:claude 的 bash 模式一遇换行就提交,块注释会变成第二条命令被执行。
    // 行尾 `#` 注释让 marker 照旧完整落进转录(shell 自己忽略它),关联与送达判据原样复用。
    let wire = match summon_core::driver::bang_command(prompt) {
        Some(_) => format!("{prompt}  # {marker}"),
        None => format!("{prompt}\n\n<!-- {marker} -->"),
    };
    (wire, marker)
}

/// 一条转录记录相对本轮 marker 的身份。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MarkerRecord {
    /// claude 真正开始处理这条 prompt。
    Consumed,
    /// 消息已到 claude 手里,但还排在它的队列里(尚未消费)。
    Enqueued,
    /// 有一条输入入队,但记录不带正文,认不出是不是本轮。claude 在压缩上下文时收到输入就落这种
    /// 形态(2026-11-11 claude 2.1.322,空闲压缩期间注入),压完才写带 marker 的消费记录。
    QueuedWithoutText,
    /// 与本轮无关:不含 marker,或含 marker 但属于必须排除的形态。
    Unrelated,
    /// marker 到了,正文开头没到 —— 注入的 prompt 被 CLI 吞掉了前半段。
    Truncated,
}

/// 本轮 prompt 在转录里的身份。**两个判据缺一不可**:随机 `marker` 认哪一轮,`head` 认这一轮
/// 收全了没有。
///
/// marker 落在 prompt 末尾,所以「只验 marker」对**丢头部**的注入是瞎的:2026-08-28 有三条
/// 飞书消息掐掉开头才进 claude,marker 还在,于是一路判成投递成功,agent 读到半条消息、没有
/// 任何人报错。判据必须同时看住正文开头 —— 注入通道再换,这道闸都还在。
struct TurnEcho<'a> {
    marker: &'a str,
    /// 本轮 prompt 的开头。空串 = 不校验完整性(旁路的送达观测只需要认轮)。
    head: &'a str,
}

impl TurnEcho<'_> {
    /// `None` = 这段文本与本轮无关;`Some(true)` = 本轮且收全;`Some(false)` = 本轮但缺了开头。
    fn judge(&self, text: &str) -> Option<bool> {
        if !text.contains(self.marker) {
            return None;
        }
        Some(self.head.is_empty() || text.contains(self.head))
    }
}

/// 取 prompt 开头做完整性探针。太短会在长正文里撞巧合,太长会把 CLI 的任何规范化都变成误报;
/// 24 个字符足够跨过 `{who} @你:「` 这类前缀,落到正文头上。
fn prompt_head(prompt: &str) -> String {
    // claude 不把 `!` 写进转录 —— 它被替换成 `<bash-input>` 标签,其余逐字节保留。探针从
    // `!` 之后取,否则每条 shell 直通都会被判成「注入被掐掉了开头」。
    let probe = summon_core::driver::bang_command(prompt).unwrap_or(prompt);
    probe.chars().take(24).collect()
}

/// 判一条转录记录相对本轮 marker 的身份。**白名单语义**:只有下面显式枚举的两种形态算已消费,
/// 其余(含将来 claude 新增的记录类型)一律 `Unrelated`。
///
/// 为什么不能宽松 substring:实测本机 16 份转录、68 个 marker,**含 marker 全文**的记录共七种形态,
/// 只有两种代表已消费。另外五种(last-prompt 64 条、assistant 引用 5 条、queue-operation 的
/// enqueue/remove 各 4 条、attachment/edited_text_file 1 条)都会被宽松匹配误命中,后果是把还躺在
/// 队列里的消息当成已在跑——实测两者最长差 152.7s,期间所有时序(表情/卡片/WAL)全部提前。
///
/// fail-closed 的方向是「认不出就等着」:等着最坏是超时报错(用户看得见),放行最坏是双份回复和
/// 错位的表情(用户看不懂)。
fn classify_marker_record(line: &[u8], echo: &TurnEcho) -> MarkerRecord {
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(line) else {
        return MarkerRecord::Unrelated;
    };
    match value.get("type").and_then(serde_json::Value::as_str) {
        // 形态 A:注入那一刻没有工具边界可插(空闲态 / 纯文本轮),消息落成普通 user 记录。
        Some("user") => match user_message_echo(&value, echo) {
            Some(true) => MarkerRecord::Consumed,
            Some(false) => MarkerRecord::Truncated,
            None => MarkerRecord::Unrelated,
        },
        // 形态 B:轮内 steer,消息在工具边界出队时落成 queued_command 附件。必须精确到
        // `attachment.type` 与 `attachment.prompt` 两个字段:同为 attachment 的 edited_text_file
        // 会把含 marker 的**文件内容**整段带进 snippet,对 attachment 整行 substring 等于把
        // 「agent 编辑了一个提到 marker 的文件」误判成「claude 消费了这条消息」。
        Some("attachment") => {
            let attachment = value.get("attachment");
            let is_queued_command = attachment
                .and_then(|a| a.get("type"))
                .and_then(serde_json::Value::as_str)
                == Some("queued_command");
            let verdict = attachment
                .and_then(|a| a.get("prompt"))
                .and_then(serde_json::Value::as_str)
                .and_then(|prompt| echo.judge(prompt));
            match (is_queued_command, verdict) {
                (true, Some(true)) => MarkerRecord::Consumed,
                (true, Some(false)) => MarkerRecord::Truncated,
                _ => MarkerRecord::Unrelated,
            }
        }
        // 显式排除 1:queue-operation 只证明消息**到了 claude 手里**,不证明被消费。enqueue 在
        // Enter 后 61–149ms 就落盘;remove 甚至落在真正的消费记录**之前**,且用户按 Esc 撤掉一条
        // 排队消息同样产生 remove(那时消息从未被消费)→ 语义二义。两个方向都只当送达确认。
        Some("queue-operation") => match value.get("content") {
            Some(content) => {
                if content
                    .as_str()
                    .is_some_and(|content| content.contains(echo.marker))
                {
                    MarkerRecord::Enqueued
                } else {
                    MarkerRecord::Unrelated
                }
            }
            None if value.get("operation").and_then(serde_json::Value::as_str)
                == Some("enqueue") =>
            {
                MarkerRecord::QueuedWithoutText
            }
            None => MarkerRecord::Unrelated,
        },
        // 显式排除 2:last-prompt 是「最后一次输入」的快照,带 marker 全文且是所有形态里最多的
        // (实测 64 条),但它在一轮里可反复出现,与消费与否无关。
        Some("last-prompt") => MarkerRecord::Unrelated,
        // 其余(assistant 引用 marker 等)一律不算——白名单的缺省分支就是这条不变量本身。
        _ => MarkerRecord::Unrelated,
    }
}

/// user 记录的正文里是否有 marker(content 可能是字符串,也可能是 text block 数组)。
fn user_message_echo(value: &serde_json::Value, echo: &TurnEcho) -> Option<bool> {
    if value
        .get("message")
        .and_then(|message| message.get("role"))
        .and_then(serde_json::Value::as_str)
        != Some("user")
    {
        return None;
    }
    let content = value
        .get("message")
        .and_then(|message| message.get("content"))?;
    match content {
        serde_json::Value::String(text) => echo.judge(text),
        // 分块正文的完整性按**整条消息**判:开头在第一块、marker 在最后一块是常态,逐块判会把
        // 一条收全的消息判成截断。
        serde_json::Value::Array(blocks) => {
            let joined: String = blocks
                .iter()
                .filter(|block| {
                    block.get("type").and_then(serde_json::Value::as_str) == Some("text")
                })
                .filter_map(|block| block.get("text").and_then(serde_json::Value::as_str))
                .collect::<Vec<_>>()
                .join("\n");
            echo.judge(&joined)
        }
        _ => None,
    }
}

/// 从 Summon 自己追加的 marker 文本中取最后一个合法 UUID。用户正文可能引用旧 marker，当前轮
/// marker 总是追加在末尾；严格 UUID 形状避免把普通文本中的同前缀片段变成送达凭证。
fn last_transport_marker(text: &str) -> Option<String> {
    const PREFIX: &str = "summon-transport-turn:";
    text.match_indices(PREFIX)
        .filter_map(|(start, _)| {
            let uuid_start = start + PREFIX.len();
            let uuid_end = uuid_start.checked_add(36)?;
            let raw = text.get(uuid_start..uuid_end)?;
            uuid::Uuid::parse_str(raw)
                .ok()
                .map(|_| format!("{PREFIX}{raw}"))
        })
        .fold(None, |_, marker| Some(marker))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MirrorMarkerState {
    Enqueued,
    Consumed,
}

/// 一条 terminal 之前、且仍属于同一转录 turn 的 transport 顺序证据。`Enqueued` 与 `Consumed`
/// 必须分开：B 已入队但尚未消费时，随后出现的 markerless terminal 明确属于前序 attach A；若只
/// 传一个无类型候选集合，dispatcher 会把 A 误认成 B 并吞掉。
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct MirrorMarkerEvidence {
    enqueued: Vec<String>,
    consumed: Vec<String>,
    uncertain: Vec<String>,
    overflowed: bool,
}

impl MirrorMarkerEvidence {
    const MAX_CANDIDATES: usize = 16;

    #[cfg(test)]
    pub(crate) fn from_parts<E, C, ES, CS>(enqueued: E, consumed: C) -> Self
    where
        E: IntoIterator<Item = ES>,
        C: IntoIterator<Item = CS>,
        ES: Into<String>,
        CS: Into<String>,
    {
        let mut evidence = Self::default();
        for marker in enqueued {
            evidence.remember(MirrorMarkerState::Enqueued, marker.into());
        }
        for marker in consumed {
            evidence.remember(MirrorMarkerState::Consumed, marker.into());
        }
        evidence
    }

    pub(crate) fn enqueued(&self) -> &[String] {
        &self.enqueued
    }

    pub(crate) fn consumed(&self) -> &[String] {
        &self.consumed
    }

    pub(crate) fn uncertain(&self) -> &[String] {
        &self.uncertain
    }

    pub(crate) fn overflowed(&self) -> bool {
        self.overflowed
    }

    fn remember(&mut self, state: MirrorMarkerState, marker: String) {
        match state {
            MirrorMarkerState::Enqueued => {
                if self.consumed.iter().any(|saved| saved == &marker)
                    || self.enqueued.iter().any(|saved| saved == &marker)
                {
                    return;
                }
                if self.enqueued.len() + self.consumed.len() + self.uncertain.len()
                    == Self::MAX_CANDIDATES
                {
                    self.overflowed = true;
                    return;
                }
                self.enqueued.push(marker);
            }
            MirrorMarkerState::Consumed => {
                // 只有真正消费记录才比 unknown shape 强。Enqueued 只是排队证据，不能抹掉同
                // marker 的不确定记录，否则会把前序 terminal 错绑给尚未消费的下一轮。
                self.uncertain.retain(|saved| saved != &marker);
                self.enqueued.retain(|saved| saved != &marker);
                if self.consumed.iter().any(|saved| saved == &marker) {
                    return;
                }
                if self.enqueued.len() + self.consumed.len() + self.uncertain.len()
                    == Self::MAX_CANDIDATES
                {
                    self.overflowed = true;
                    return;
                }
                self.consumed.push(marker);
            }
        }
    }

    fn remember_uncertain(&mut self, marker: String) {
        if self.consumed.iter().any(|saved| saved == &marker)
            || self.uncertain.iter().any(|saved| saved == &marker)
        {
            return;
        }
        if self.enqueued.len() + self.consumed.len() + self.uncertain.len() == Self::MAX_CANDIDATES
        {
            self.overflowed = true;
            return;
        }
        self.uncertain.push(marker);
    }
}

/// mirror 独立从原始记录建立 typed marker→terminal 顺序证据，不能依赖主 gate 的回调：本功能
/// 要修的事故恰恰是主 gate 与 mirror 跨线程推进，只有 terminal 自己携带的转录前缀能证明先后。
fn transport_marker_evidence(line: &str) -> Option<(MirrorMarkerState, String)> {
    let value: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
    match value.get("type").and_then(serde_json::Value::as_str) {
        Some("user")
            if value
                .get("message")
                .and_then(|message| message.get("role"))
                .and_then(serde_json::Value::as_str)
                == Some("user") =>
        {
            let marker = match value.get("message")?.get("content")? {
                serde_json::Value::String(text) => last_transport_marker(text),
                serde_json::Value::Array(blocks) => blocks
                    .iter()
                    .filter(|block| {
                        block.get("type").and_then(serde_json::Value::as_str) == Some("text")
                    })
                    .filter_map(|block| {
                        block
                            .get("text")
                            .and_then(serde_json::Value::as_str)
                            .and_then(last_transport_marker)
                    })
                    .next_back(),
                _ => None,
            }?;
            Some((MirrorMarkerState::Consumed, marker))
        }
        Some("attachment")
            if value
                .get("attachment")
                .and_then(|attachment| attachment.get("type"))
                .and_then(serde_json::Value::as_str)
                == Some("queued_command") =>
        {
            let marker = value
                .get("attachment")?
                .get("prompt")?
                .as_str()
                .and_then(last_transport_marker)?;
            Some((MirrorMarkerState::Consumed, marker))
        }
        Some("queue-operation") => {
            let marker = value
                .get("content")?
                .as_str()
                .and_then(last_transport_marker)?;
            Some((MirrorMarkerState::Enqueued, marker))
        }
        // 已知明确非消费形态不能毒化：它们会稳定引用 marker，但不携带顺序语义。
        Some("last-prompt") | Some("assistant") | Some("attachment") | Some("user") => None,
        _ => None,
    }
}

/// 白名单外的 provider shape 若仍含合法 Summon marker，既不能猜成 Consumed（会错绑前序 A），
/// 也不能当 markerless attach（会与主失败回执双发）。已知明确无关的引用/快照形态继续忽略；真正
/// 未知的 shape 记为 uncertain，等待同 marker 的后续 typed 记录收敛，否则 terminal 处 fail-stop。
fn uncertain_transport_marker(line: &str) -> Option<String> {
    let marker = last_transport_marker(line)?;
    let Ok(value) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
        return Some(marker);
    };
    match value.get("type").and_then(serde_json::Value::as_str) {
        Some("assistant") | Some("last-prompt") => None,
        Some("attachment")
            if value
                .get("attachment")
                .and_then(|attachment| attachment.get("type"))
                .and_then(serde_json::Value::as_str)
                == Some("edited_text_file") =>
        {
            None
        }
        Some("user") => {
            // tool_result 也落成 type=user，但结构上不可能表示新 prompt 被消费。工具输出里
            // 恰好出现 marker 很常见（例如 grep 自己的 transcript），不能因此毒死整个 watcher。
            let content = value
                .get("message")
                .and_then(|message| message.get("content"));
            match content {
                Some(serde_json::Value::Array(blocks))
                    if blocks.iter().all(|block| {
                        block.get("type").and_then(serde_json::Value::as_str) != Some("text")
                    }) =>
                {
                    None
                }
                _ => Some(marker),
            }
        }
        _ => Some(marker),
    }
}

/// thinking-only `end_turn` 后若同一次 read 已出现下一条真实 user prompt，静默确认窗就不再有
/// 信息价值：新 user 是上一轮已经结束的确定证据。tool_result 也落成 `type:user`，但仍属于当前
/// 工具轮，不能拿它切轮；这里只认字符串正文、text block 与 queued_command 三种真实输入形态。
/// Claude Code 标了 `turnCompanion` 的 user 行同样属于当前轮(见 [`is_turn_companion`])。
fn starts_new_transcript_turn(line: &str) -> bool {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
        return false;
    };
    if turn_companion_flag(&value) {
        return false;
    }
    match value.get("type").and_then(serde_json::Value::as_str) {
        Some("user")
            if value
                .get("message")
                .and_then(|message| message.get("role"))
                .and_then(serde_json::Value::as_str)
                == Some("user") =>
        {
            match value
                .get("message")
                .and_then(|message| message.get("content"))
            {
                Some(serde_json::Value::String(_)) => true,
                Some(serde_json::Value::Array(blocks)) => blocks.iter().any(|block| {
                    block.get("type").and_then(serde_json::Value::as_str) == Some("text")
                }),
                _ => false,
            }
        }
        Some("attachment") => {
            value
                .get("attachment")
                .and_then(|attachment| attachment.get("type"))
                .and_then(serde_json::Value::as_str)
                == Some("queued_command")
        }
        _ => false,
    }
}

/// 已确认 end_turn 之后的纯旁路记录不会让主 Agent 恢复执行。部分行保留在 parser 中，
/// 完整但不认识的记录则按主 Agent 活动处理。
fn idle_metadata_prefix(bytes: &[u8]) -> (usize, bool) {
    let mut prefix = 0;
    for line in bytes.split_inclusive(|byte| *byte == b'\n') {
        if !line.ends_with(b"\n") {
            break;
        }
        let Ok(value) = serde_json::from_slice::<serde_json::Value>(line) else {
            return (prefix, true);
        };
        let passive = (value["type"] == "system"
            && matches!(
                value["subtype"].as_str(),
                Some("stop_hook_summary" | "turn_duration")
            ))
            || (value["type"] == "progress"
                && matches!(
                    value["data"]["type"].as_str(),
                    Some("agent_progress" | "bash_progress")
                ));
        if !passive {
            return (prefix, true);
        }
        prefix += line.len();
    }
    (prefix, false)
}

/// 主路径刚认出的 terminal 的字节边界:已读到的末尾减去还没消费的 pending,与 mirror 的
/// `offset - pending.len()` 同一个口径。以文本 end_turn 收尾时它就是 end_turn 那一行的行尾,两边
/// 得出同一个数;只有 thinking 的 end_turn 要静默确认,两边各取确认那一刻已读完整行的末尾,
/// 确认间隙恰好落进非新轮的记录行时两边会差这几行。
fn terminal_boundary(checkpoint: &FileTailCheckpoint, tail: &TranscriptTail) -> u64 {
    checkpoint.end().saturating_sub(tail.pending.len() as u64)
}

/// 接管轮里这个段终点是不是重放出来的:接管那一刻(`replay_end`)已经落盘的段终点,上一代可能
/// 已经把它当段终点回复发过,再交出去就是同一段话发两遍;之后写出的只有本代见过,照常交出。
fn replayed_segment_end(replay_end: Option<u64>, boundary: u64) -> bool {
    replay_end.is_some_and(|end| boundary <= end)
}

/// Claude Code 标为本轮附属的 user 行(`turnCompanion: true`,如续跑提示、Skill 正文、图片说明)
/// 一律属于当前轮。其中续跑提示跟在只有 thinking 的 end_turn 后面,随后同一轮再请求一次模型。
fn is_turn_companion(line: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(line.trim())
        .is_ok_and(|value| turn_companion_flag(&value))
}

fn turn_companion_flag(value: &serde_json::Value) -> bool {
    value.get("type").and_then(serde_json::Value::as_str) == Some("user")
        && value
            .get("turnCompanion")
            .and_then(serde_json::Value::as_bool)
            == Some(true)
}

/// 本轮等待的止损时刻。两相分别止损:未确认入队前防「send-keys 没落到 pane 上」;确认入队后
/// 排队窗口无上界,只留防死锁的硬顶。抽成纯函数便于单测——这条判断决定「何时宣布注入失败」,
/// 判早了就是在 claude 明明会回答的情况下先报错。
#[cfg(test)]
fn prompt_wait_deadline(sent_at: Instant, enqueued_at: Option<Instant>) -> Instant {
    prompt_wait_deadline_with(
        sent_at,
        enqueued_at,
        PROMPT_ARRIVAL_TIMEOUT,
        PROMPT_CONSUME_TIMEOUT,
    )
}

fn prompt_wait_deadline_with(
    sent_at: Instant,
    enqueued_at: Option<Instant>,
    arrival_timeout: Duration,
    consume_timeout: Duration,
) -> Instant {
    match enqueued_at {
        Some(at) => at + consume_timeout,
        None => sent_at + arrival_timeout,
    }
}

/// 一条已注入、还在等 claude 消费的 steer 消息。
struct PendingSteer {
    marker: String,
    message_ids: Vec<String>,
}

/// 轮内 steer 的送达观测:与 `TranscriptTail` **并行**看同一份字节流,只做一件事——认出被 steer
/// 进来的消息什么时候真被 claude 消费(据此贴 OnIt)。
///
/// 为什么不能复用 `PromptEchoGate`:那个会**丢弃** marker 之前的一切字节,而 steer 期间那些字节
/// 正是**本轮**的输出(工具调用 / 流式文本)。丢掉就等于把正在跑的这一轮抹掉。观测必须是纯旁路:
/// 不动 offset、不动 tail、不改变任何既有时序。
#[derive(Default)]
struct SteerWatch {
    pending: Vec<PendingSteer>,
    buf: Vec<u8>,
}

impl SteerWatch {
    fn register(&mut self, marker: String, message_ids: Vec<String>) {
        self.pending.push(PendingSteer {
            marker,
            message_ids,
        });
    }

    /// 喂一段与 tail 相同的字节,返回本次观测到「已被消费」的那些消息 id。
    /// 无在飞 steer 时零成本:不缓冲、不解析。
    fn feed(&mut self, chunk: &[u8]) -> Vec<String> {
        if self.pending.is_empty() {
            self.buf.clear();
            return Vec::new();
        }
        self.buf.extend_from_slice(chunk);
        let mut delivered = Vec::new();
        let mut consumed = 0usize;
        while let Some(relative_end) = self.buf[consumed..].iter().position(|byte| *byte == b'\n') {
            let end = consumed + relative_end;
            let line = &self.buf[consumed..end];
            self.pending.retain(|steer| {
                let echo = TurnEcho {
                    marker: &steer.marker,
                    head: "",
                };
                if classify_marker_record(line, &echo) == MarkerRecord::Consumed {
                    delivered.extend(steer.message_ids.iter().cloned());
                    false
                } else {
                    true
                }
            });
            consumed = end + 1;
        }
        if consumed > 0 {
            self.buf.drain(..consumed);
        }
        // 坏/无换行文件不该把内存撑爆;观测是旁路,丢掉半行只是少认一次送达,不影响本轮。
        if self.buf.len() > PROMPT_ECHO_LINE_LIMIT {
            self.buf.clear();
        }
        delivered
    }
}

/// 本轮子 agent 转录的旁路 tail。
///
/// claude 把子 agent 的转录写在 `<sid>/subagents/agent-<id>.jsonl`,旁边的 `.meta.json` 记着派它
/// 的那次 `Agent`/`Task` 调用 id。只 tail 本轮主转录里派出的那些(之前轮次留下的后台子 agent
/// 不算);解析出的事件盖上 `parent_run_id`(= 调用 id)后只交给 `on_event`,**绝不**进主转录的
/// `TranscriptTail` —— 子 agent 的 `end_turn` 会被当成主轮结束。
///
/// 主轮收尾时经 [`crate::capture::TurnOutcome::subagent_tail`] 原样交出(已对上的文件与 offset、
/// 待对上的调用 id、读过的 meta),子 agent 比主轮活得久时由 CoT 跟随线程接着读。
#[derive(Debug)]
pub struct SubagentTail {
    dir: std::path::PathBuf,
    /// 本轮派出、还没对上转录文件的调用 id。
    unmatched: std::collections::HashSet<String>,
    files: Vec<SubagentFile>,
    last_scan: Option<Instant>,
    /// 读过的 meta 文件名 → 它记的派出调用 id。meta 写好就不再变,读一次就够;目录里攒着之前
    /// 轮次的子 agent,每轮扫描都全读一遍是白花的 IO。先于派出调用被看到的 meta 也靠它之后对上。
    metas: std::collections::HashMap<String, String>,
}

#[derive(Debug)]
struct SubagentFile {
    parent: String,
    agent_id: String,
    path: std::path::PathBuf,
    offset: u64,
    pending: Vec<u8>,
}

impl SubagentTail {
    /// 找新文件的节拍。子 agent 起来到写出第一行本来就要秒级,目录不必每拍都列。
    const SCAN_EVERY: Duration = Duration::from_secs(1);

    pub(crate) fn new(main_transcript: &std::path::Path, sid: &str) -> Self {
        let dir = main_transcript
            .parent()
            .map(|dir| dir.join(sid).join("subagents"))
            .unwrap_or_default();
        Self {
            dir,
            unmatched: std::collections::HashSet::new(),
            files: Vec::new(),
            last_scan: None,
            metas: std::collections::HashMap::new(),
        }
    }

    /// 主转录的事件先过这里:记下本轮派出的子 agent。
    pub(crate) fn note(&mut self, msg: &AgentMessage) {
        if msg.kind != AgentKind::ToolUse || !matches!(msg.tool.as_deref(), Some("Agent" | "Task"))
        {
            return;
        }
        if let Some(id) = msg.tool_id.as_deref().filter(|id| !id.is_empty()) {
            if !self.files.iter().any(|file| file.parent == id) {
                self.unmatched.insert(id.to_string());
            }
        }
    }

    /// 读一拍,`now` 是这一拍的时刻(列目录的节拍按它算)。返回这一拍有没有读到子 agent 的
    /// 新字节(存活证据)。
    pub(crate) fn poll<E: Executor>(
        &mut self,
        exec: &E,
        now: Instant,
        on_event: &mut dyn FnMut(&AgentMessage),
    ) -> bool {
        if !self.unmatched.is_empty()
            && self
                .last_scan
                .is_none_or(|at| now.saturating_duration_since(at) >= Self::SCAN_EVERY)
        {
            self.last_scan = Some(now);
            self.scan(exec);
        }
        let mut progressed = false;
        for file in &mut self.files {
            // 旁路观测:读失败只是这一拍看不到子 agent,不影响主轮。
            let Ok(chunk) = exec.read_file_tail(&file.path, file.offset) else {
                continue;
            };
            if chunk.is_empty() {
                continue;
            }
            progressed = true;
            file.offset += chunk.len() as u64;
            file.pending.extend_from_slice(&chunk);
            let Some(last_newline) = file.pending.iter().rposition(|byte| *byte == b'\n') else {
                if file.pending.len() > PROMPT_ECHO_LINE_LIMIT {
                    file.pending.clear();
                }
                continue;
            };
            let complete: Vec<u8> = file.pending.drain(..=last_newline).collect();
            for line in String::from_utf8_lossy(&complete).lines() {
                for mut msg in parse_stream_json_line(line) {
                    msg.parent_run_id = Some(summon_core::agent_msg::RunId(file.parent.clone()));
                    on_event(&msg);
                }
            }
        }
        progressed
    }

    /// 已对上转录文件的子 agent:`(派出调用 id, agentId)`。状态源里结束的子 agent 只按
    /// agentId 认人。
    pub(crate) fn agent_ids(&self) -> impl Iterator<Item = (&str, &str)> {
        self.files
            .iter()
            .map(|file| (file.parent.as_str(), file.agent_id.as_str()))
    }

    fn scan<E: Executor>(&mut self, exec: &E) {
        let Ok(names) = exec.list_dir(&self.dir) else {
            return;
        };
        for name in names {
            let Some(agent_id) = name
                .strip_prefix("agent-")
                .and_then(|rest| rest.strip_suffix(".meta.json"))
            else {
                continue;
            };
            let parent = match self.metas.get(&name) {
                Some(parent) => parent.clone(),
                None => {
                    let Ok(meta) = exec.read_file_tail(&self.dir.join(&name), 0) else {
                        continue;
                    };
                    // 解析不出来(可能还没写完)不记,下一轮再读。
                    let Some(parent) = serde_json::from_slice::<serde_json::Value>(&meta)
                        .ok()
                        .and_then(|meta| meta.get("toolUseId")?.as_str().map(str::to_string))
                    else {
                        continue;
                    };
                    self.metas.insert(name.clone(), parent.clone());
                    parent
                }
            };
            if self.unmatched.remove(&parent) {
                self.files.push(SubagentFile {
                    parent,
                    agent_id: agent_id.to_string(),
                    path: self.dir.join(format!("agent-{agent_id}.jsonl")),
                    offset: 0,
                    pending: Vec::new(),
                });
            }
        }
    }
}

/// `PromptEchoGate::feed` 的一步结果。
#[derive(Debug)]
enum GateStep {
    /// 本轮 marker 还没在转录里露面。
    Waiting,
    /// 观测到入队:消息在 claude 队列里,消费还没发生。
    Enqueued,
    /// 只观测到不带正文的入队(见 [`MarkerRecord::QueuedWithoutText`]),归属未证。
    QueuedWithoutText,
    /// 观测到消费:附带该记录**之后**的字节。
    Consumed(Vec<u8>),
}

/// 在看到本轮 marker 的**消费**记录前丢弃所有完整行；命中时只把该行**之后**的字节交给
/// `TranscriptTail`。这样旧 assistant/end_turn 即使恰在新 baseline 后落盘，也没有资格收新轮。
///
/// 开闸判据是「已消费」而非「marker 露面」:被 steer 的消息从入队到被消费实测能隔 152.7s,期间
/// 转录里满是 claude 在跑的**上一件事**的输出。在入队那一刻开闸,等于把别人的输出当成本轮的。
#[derive(Default)]
struct PromptEchoGate {
    pending: Vec<u8>,
}

impl PromptEchoGate {
    fn feed(&mut self, chunk: &[u8], echo: &TurnEcho) -> anyhow::Result<GateStep> {
        self.pending.extend_from_slice(chunk);
        let mut consumed = 0usize;
        let mut saw_enqueue = false;
        let mut saw_queued_without_text = false;
        while let Some(relative_end) = self.pending[consumed..]
            .iter()
            .position(|byte| *byte == b'\n')
        {
            let end = consumed + relative_end;
            match classify_marker_record(&self.pending[consumed..end], echo) {
                MarkerRecord::Consumed => {
                    let trailing = self.pending[end + 1..].to_vec();
                    self.pending.clear();
                    return Ok(GateStep::Consumed(trailing));
                }
                MarkerRecord::Enqueued => saw_enqueue = true,
                MarkerRecord::QueuedWithoutText => saw_queued_without_text = true,
                MarkerRecord::Unrelated => {}
                // 本轮确实进了 claude,但少了开头。这一轮的正文已经不是我们发的那条了,继续等
                // 只会等到一个基于半条消息的回复 —— 必须当场失败,让上层走既有的注入失败回执。
                MarkerRecord::Truncated => anyhow::bail!(summon_i18n::format!("r718a3d7081")),
            }
            consumed = end + 1;
        }
        if consumed > 0 {
            self.pending.drain(..consumed);
        }
        anyhow::ensure!(
            self.pending.len() <= PROMPT_ECHO_LINE_LIMIT,
            summon_i18n::format!("rb9ca5f94d4", PROMPT_ECHO_LINE_LIMIT)
        );
        Ok(if saw_enqueue {
            GateStep::Enqueued
        } else if saw_queued_without_text {
            GateStep::QueuedWithoutText
        } else {
            GateStep::Waiting
        })
    }
}

/// 当前 path 是否已不再是上次读的那份 append-only 文件。长度回退覆盖原地 truncate，文件身份
/// 覆盖 rename/replace 后新文件很快长回旧 offset 之上的情形；任一命中都必须从头找随机 marker。
fn transcript_needs_rescan(
    previous_identity: Option<FileIdentity>,
    current_identity: Option<FileIdentity>,
    offset: u64,
    current_len: Option<u64>,
) -> bool {
    current_len.is_some_and(|len| len < offset)
        || previous_identity
            .zip(current_identity)
            .is_some_and(|(previous, current)| previous != current)
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct FileTailCheckpoint {
    start: u64,
    bytes: Vec<u8>,
}

/// mirror 的通知卡与 terminal reply 语义不同，但二者必须携带同一 terminal marker 证据：
/// 主路径 turn 的通知卡也不能重复发（那一轮由主路径自己实时刷卡）；markerless attach 的
/// 通知卡则仍要正常转发。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum MirrorTurnKind {
    /// 这一轮的通知卡：跨会话消息收发与后台任务完成通知合成的那一块。
    Notice,
    Reply,
    /// **停机窗口补发**:终点落在上一代 stop fence 的 `stop_eof` 右侧的 terminal。
    ///
    /// 它与 [`Reply`](MirrorTurnKind::Reply) 的唯一区别在 tracker 侧:这一轮的 transport marker
    /// 是**上一代 daemon** bind 的,本代 tracker 的 active 里当然找不到。普通 Reply 遇到这种
    /// 「本代不认识的 consumed marker」必须判 `HistoricalUnknown` 零网络(它只可能来自旧 offset
    /// 重放);但停机窗口这一类有第三种来源 —— 上一代注入、上一代死了、本代从没接手过。
    /// 那一轮**没有任何进程会投递它**,所以补发不会重复。判据的证明在 `stop_eof`:
    /// 右侧的字节全部落盘于 daemon 死后。
    CatchupReply,
    /// terminal 已落在 seed cutoff/主路径明确观察范围内：通知 tracker 收敛状态，但绝不联网。
    ReplyObserved,
    /// fresh watcher 冻结 Ready 前已存在，或从 ambiguous baseline 续起的首个 terminal。历史归属
    /// 无法证明未送达，只允许收敛 durable offset；结构上禁止联网且不得污染 live tracker。
    HistoricalObserved,
    /// 当前 terminal 前存在无法归类/超预算的 marker 证据。不能猜归属，也不能永久杀掉 watcher：
    /// 本轮只推进 durable offset、零网络且不碰 live tracker，未来确定的 terminal 仍可继续镜像。
    AmbiguousObserved,
}

type MirrorTurnSink<'a> =
    dyn FnMut(MirrorTurnKind, &MirrorMarkerEvidence, u64, &[u8], &str) -> anyhow::Result<()> + 'a;
type MirrorOffsetSink<'a> = dyn FnMut(u64, &FileTailCheckpoint) -> anyhow::Result<()> + 'a;

impl FileTailCheckpoint {
    fn end(&self) -> u64 {
        self.start.saturating_add(self.bytes.len() as u64)
    }

    fn from_read(read_from: u64, chunk: &[u8]) -> Self {
        let keep = chunk.len().min(TAIL_CHECKPOINT_BYTES);
        let start = read_from.saturating_add((chunk.len() - keep) as u64);
        Self {
            start,
            bytes: chunk[chunk.len() - keep..].to_vec(),
        }
    }

    /// 重读 checkpoint + 新后缀。前缀逐字节一致才返回 Some；不一致表示 path generation 已换，
    /// 调用方必须从 exact marker 重锚，不能沿用旧 offset。
    fn read_new<E: Executor>(
        &self,
        exec: &E,
        path: &std::path::Path,
    ) -> anyhow::Result<Option<(Vec<u8>, Self)>> {
        let observed = exec.read_file_tail(path, self.start)?;
        if !observed.starts_with(&self.bytes) {
            return Ok(None);
        }
        let suffix = observed[self.bytes.len()..].to_vec();
        let next = Self::from_read(self.start, &observed);
        Ok(Some((suffix, next)))
    }

    /// 为已知 EOF/offset 捕获它之前的短后缀。读出来可能还带 offset 之后的新追加，只取精确
    /// checkpoint 长度；若连这段都读不全，说明 snapshot 已失效，不能宣布 watcher ready。
    fn capture_at<E: Executor>(exec: &E, path: &std::path::Path, end: u64) -> anyhow::Result<Self> {
        let keep = end.min(TAIL_CHECKPOINT_BYTES as u64) as usize;
        let start = end.saturating_sub(keep as u64);
        let observed = exec.read_file_range(path, start, keep)?;
        anyhow::ensure!(observed.len() == keep, summon_i18n::format!("r53efc2e4c0"));
        Ok(Self {
            start,
            bytes: observed,
        })
    }
}

/// 一次增量读的原始字节窗口。terminal boundary 由 parser 在网络发送前给出；必须用这份已经
/// 解析过的字节推导 checkpoint，不能发送后再重读 path——网络 I/O 期间文件可能已经换代。
struct ObservedReadWindow<'a> {
    previous: &'a FileTailCheckpoint,
    start: u64,
    bytes: &'a [u8],
}

impl ObservedReadWindow<'_> {
    fn checkpoint_at(&self, boundary: u64) -> anyhow::Result<FileTailCheckpoint> {
        anyhow::ensure!(
            self.previous.end() == self.start,
            summon_i18n::format!("r57b206ff28")
        );
        let end = self.start.saturating_add(self.bytes.len() as u64);
        anyhow::ensure!(
            (self.start..=end).contains(&boundary),
            summon_i18n::format!("r9c8c5247ca")
        );
        let prefix_len = (boundary - self.start) as usize;
        let keep_from_previous = self
            .previous
            .bytes
            .len()
            .min(TAIL_CHECKPOINT_BYTES.saturating_sub(prefix_len.min(TAIL_CHECKPOINT_BYTES)));
        let keep_from_current = prefix_len.min(TAIL_CHECKPOINT_BYTES);
        let mut bytes = Vec::with_capacity(keep_from_previous + keep_from_current);
        bytes.extend_from_slice(
            &self.previous.bytes[self.previous.bytes.len().saturating_sub(keep_from_previous)..],
        );
        bytes.extend_from_slice(
            &self.bytes[prefix_len.saturating_sub(keep_from_current)..prefix_len],
        );
        let keep = bytes.len().min(TAIL_CHECKPOINT_BYTES);
        if bytes.len() > keep {
            bytes.drain(..bytes.len() - keep);
        }
        Ok(FileTailCheckpoint {
            start: boundary.saturating_sub(bytes.len() as u64),
            bytes,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum MirrorOffsetBoundaryKind {
    /// 由 parser 收口并成功处理的 terminal 精确边界；其后首个 turn 可以按 live 规则仲裁。
    TerminalBoundary,
    /// fresh watcher 在任意稳定 EOF 建的起点；可能切在旧 Summon turn 中间，恢复后首个 terminal
    /// 必须 observation-only，成功提交 terminal 后才能转成 `TerminalBoundary`。
    AmbiguousBaseline,
}

impl MirrorOffsetBoundaryKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::TerminalBoundary => "terminal",
            Self::AmbiguousBaseline => "ambiguous",
        }
    }

    fn parse(raw: &str) -> Option<Self> {
        match raw {
            "terminal" => Some(Self::TerminalBoundary),
            "ambiguous" => Some(Self::AmbiguousBaseline),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct MirrorOffsetRecord {
    epoch_fingerprint: String,
    sid: String,
    offset: u64,
    checkpoint: FileTailCheckpoint,
    boundary_kind: MirrorOffsetBoundaryKind,
}

impl MirrorOffsetRecord {
    pub(crate) fn epoch_fingerprint(&self) -> &str {
        &self.epoch_fingerprint
    }

    pub(crate) fn sid(&self) -> &str {
        &self.sid
    }

    pub(crate) fn boundary_kind(&self) -> MirrorOffsetBoundaryKind {
        self.boundary_kind
    }
}

fn mirror_start_is_ambiguous(used_saved: bool, saved: Option<&MirrorOffsetRecord>) -> bool {
    !used_saved
        || saved.is_some_and(|record| {
            record.boundary_kind == MirrorOffsetBoundaryKind::AmbiguousBaseline
        })
}

fn transcript_eof_is_terminal_boundary<E: Executor>(
    exec: &E,
    path: &std::path::Path,
    eof: u64,
) -> anyhow::Result<bool> {
    if eof == 0 {
        return Ok(false);
    }
    let read_from = eof.saturating_sub(PROMPT_RESCUE_WINDOW_BYTES as u64);
    let bytes = exec.read_file_range(path, read_from, (eof - read_from) as usize)?;
    if bytes.last().copied() != Some(b'\n') {
        return Ok(false);
    }
    let Some(line) = bytes
        .split(|byte| *byte == b'\n')
        .rev()
        .find(|line| !line.iter().all(u8::is_ascii_whitespace))
    else {
        return Ok(false);
    };
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(line) else {
        return Ok(false);
    };
    Ok(
        value.get("type").and_then(serde_json::Value::as_str) == Some("assistant")
            && is_turn_end(
                value
                    .get("message")
                    .and_then(|message| message.get("stop_reason"))
                    .and_then(serde_json::Value::as_str),
            ),
    )
}

fn encode_checkpoint(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return "-".to_string();
    }
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0x0f) as usize] as char);
    }
    encoded
}

fn decode_checkpoint(encoded: &str) -> Option<Vec<u8>> {
    if encoded == "-" {
        return Some(Vec::new());
    }
    if encoded.is_empty() || !encoded.len().is_multiple_of(2) {
        return None;
    }
    fn nibble(byte: u8) -> Option<u8> {
        match byte {
            b'0'..=b'9' => Some(byte - b'0'),
            b'a'..=b'f' => Some(byte - b'a' + 10),
            _ => None,
        }
    }
    encoded
        .as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| Some((nibble(pair[0])? << 4) | nibble(pair[1])?))
        .collect()
}

fn mirror_checkpoint_len(offset: u64) -> usize {
    usize::try_from(offset)
        .unwrap_or(usize::MAX)
        .min(TAIL_CHECKPOINT_BYTES)
}

fn format_mirror_offset(
    epoch_fingerprint: &str,
    sid: &str,
    offset: u64,
    checkpoint: &FileTailCheckpoint,
    boundary_kind: MirrorOffsetBoundaryKind,
) -> anyhow::Result<String> {
    anyhow::ensure!(
        !epoch_fingerprint.is_empty() && !epoch_fingerprint.chars().any(char::is_whitespace),
        summon_i18n::format!("rac48fdb7c4")
    );
    anyhow::ensure!(
        !sid.is_empty() && !sid.chars().any(char::is_whitespace),
        summon_i18n::format!("r96d7b6c673")
    );
    anyhow::ensure!(offset > 0, summon_i18n::format!("rc9494534d1"));
    anyhow::ensure!(
        checkpoint.end() == offset,
        summon_i18n::format!("rdc33c7b04d")
    );
    anyhow::ensure!(
        checkpoint.bytes.len() == mirror_checkpoint_len(offset),
        summon_i18n::format!("r2a9d0d35ff")
    );
    Ok(format!(
        "v3 {epoch_fingerprint} {sid} {offset} {} {}",
        encode_checkpoint(&checkpoint.bytes),
        boundary_kind.as_str()
    ))
}

/// 只认同时绑定飞书 epoch 与 boundary 指纹的 v3。旧 v2 无法证明 backlog 属于当前 app/secret/scope，
/// 身份切换后 lazy watcher 若续旧 offset 会把 A 代内容发进 B 代群，因此升级时一律从稳定 EOF 起步。
pub(crate) fn parse_mirror_offset_record(raw: &str) -> Option<MirrorOffsetRecord> {
    let mut parts = raw.split_whitespace();
    if parts.next()? != "v3" {
        return None;
    }
    let epoch_fingerprint = parts.next()?.to_string();
    let sid = parts.next()?.to_string();
    let offset = parts.next()?.parse::<u64>().ok()?;
    let bytes = decode_checkpoint(parts.next()?)?;
    let boundary_kind = MirrorOffsetBoundaryKind::parse(parts.next()?)?;
    if parts.next().is_some()
        || epoch_fingerprint.is_empty()
        || sid.is_empty()
        || offset == 0
        || bytes.len() != mirror_checkpoint_len(offset)
    {
        return None;
    }
    Some(MirrorOffsetRecord {
        epoch_fingerprint,
        sid,
        offset,
        checkpoint: FileTailCheckpoint {
            start: offset.saturating_sub(bytes.len() as u64),
            bytes,
        },
        boundary_kind,
    })
}

/// 停机那一刻的 EOF 分界线:`(eof, 该位置前 1KiB 的 base64 checkpoint)`。
///
/// 它回答的是「哪些字节一定落盘于 daemon 死后」——右侧的字节没有任何主路径活着能投递,补发它们
/// 不会重复。**只记 offset 数字不够**:同长度 replacement 能冒充,所以连同 checkpoint 一起记,
/// 下次启动 exact 复验(理由同 [`mirror_offset_is_exact_eof`])。
pub(crate) fn capture_mirror_eof_fence(path: &std::path::Path) -> anyhow::Result<(u64, String)> {
    let exec = crate::runtime::executor::LocalExecutor;
    let eof = exec.file_len(path)?;
    let checkpoint = FileTailCheckpoint::capture_at(&exec, path, eof)?;
    Ok((eof, encode_checkpoint(&checkpoint.bytes)))
}

/// 复验上一代记下的 EOF 分界线在本代盘上仍然成立(文件没被 truncate / 换代重写)。
/// 任何解不开、对不上、长度不符一律 `false` —— 这是主动出站的前置条件,认不出就当没有。
pub(crate) fn mirror_eof_fence_still_valid(
    path: &std::path::Path,
    eof: u64,
    encoded_checkpoint: &str,
) -> anyhow::Result<bool> {
    if eof == 0 || encoded_checkpoint.is_empty() {
        return Ok(false);
    }
    let Some(bytes) = decode_checkpoint(encoded_checkpoint) else {
        return Ok(false);
    };
    if bytes.len() != mirror_checkpoint_len(eof) {
        return Ok(false);
    }
    let exec = crate::runtime::executor::LocalExecutor;
    // 文件只会 append:当前 EOF 必须 ≥ 分界线,否则说明这不是同一份文件的延续。
    if exec.file_len(path)? < eof {
        return Ok(false);
    }
    let expected = FileTailCheckpoint {
        start: eof.saturating_sub(bytes.len() as u64),
        bytes,
    };
    Ok(FileTailCheckpoint::capture_at(&exec, path, eof)? == expected)
}

/// 主动群出站 watcher 的起点必须来自同一代、稳定的文件快照。无 saved offset 时要求 EOF 在
/// snapshot 两侧一致；有 backlog 时除 identity 外还必须 exact 命中持久化 boundary 指纹。
/// 若另一个 generation 刻意保留 boundary 前相同 1KiB，它与 append continuation 在现有文件协议
/// 下不可区分；Claude JSONL 的自然 truncate/rotate 不具备这种对抗性，1KiB exact 是明确证明边界。
fn stable_mirror_start<E: Executor>(
    exec: &E,
    path: &std::path::Path,
    saved: Option<&MirrorOffsetRecord>,
) -> anyhow::Result<(u64, u64, FileTailCheckpoint, bool)> {
    const ATTEMPTS: usize = 4;
    for _ in 0..ATTEMPTS {
        let identity_before = exec.file_identity(path)?;
        let eof = exec.file_len(path)?;
        let verified_saved = match saved.filter(|record| record.offset <= eof) {
            Some(record) => {
                let observed = FileTailCheckpoint::capture_at(exec, path, record.offset)?;
                (observed == record.checkpoint).then_some((record.offset, observed))
            }
            None => None,
        };
        let used_saved = verified_saved.is_some();
        let (start, checkpoint) = match verified_saved {
            Some(saved) => saved,
            None => (eof, FileTailCheckpoint::capture_at(exec, path, eof)?),
        };
        let identity_after = exec.file_identity(path)?;
        let final_len = exec.file_len(path)?;
        let observed_again = FileTailCheckpoint::capture_at(exec, path, start)?;
        let len_is_safe = if used_saved {
            final_len >= start
        } else {
            final_len == eof
        };
        if identity_before == identity_after && len_is_safe && observed_again == checkpoint {
            anyhow::ensure!(
                !checkpoint.bytes.is_empty(),
                summon_i18n::format!("rd26d1aecb0")
            );
            return Ok((start, final_len, checkpoint, used_saved));
        }
    }
    anyhow::bail!(summon_i18n::format!("r56fee16c28"))
}

/// cold start 时 transcript 文件可能已经创建但仍为 0B。空 boundary 对任意 replacement 都能
/// `starts_with`，不能据此宣布 Ready；同时 0B 是正常过渡态，不能让 watcher 永久 Failed。
/// 仅对 NotFound / exact 0B 做有硬顶的退避；权限与其它 IO 错误仍立即 fail-closed。
fn wait_for_stable_mirror_start<E: Executor>(
    exec: &E,
    path: &mut std::path::PathBuf,
    mut relocation: Option<&mut MissingTranscriptSearch>,
    saved: Option<&MirrorOffsetRecord>,
    stop: &std::sync::atomic::AtomicBool,
    empty_timeout: Duration,
    on_start_bypassed: &mut dyn FnMut(),
) -> anyhow::Result<Option<(u64, u64, FileTailCheckpoint, bool)>> {
    use std::sync::atomic::Ordering::Relaxed;

    let deadline = Instant::now() + empty_timeout;
    let mut bypassed = false;
    let mut backoff = Duration::from_millis(10);
    loop {
        if stop.load(Relaxed) {
            return Ok(None);
        }
        if let Some(search) = relocation.as_mut() {
            search.relocate_missing(exec, path)?;
        }
        match exec.file_len(path) {
            Ok(len) if len > 0 => return stable_mirror_start(exec, path, saved).map(Some),
            Ok(_) => {}
            Err(error)
                if error.chain().any(|cause| {
                    cause
                        .downcast_ref::<std::io::Error>()
                        .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound)
                }) => {}
            Err(error) => return Err(error),
        }
        let now = Instant::now();
        if !bypassed && now >= deadline {
            // 只放开主路径的 ready fence，watcher 自身继续存活。首 prompt 往往正是创建 transcript
            // 首行的动作；若这里退出，后续 attach 永久失去 mirror，若继续保持 Starting 则自锁。
            on_start_bypassed();
            bypassed = true;
        }

        // 把退避拆成小片，daemon stop 不必被最大退避整段阻塞。
        let pause_until = if bypassed {
            now + backoff
        } else {
            (now + backoff).min(deadline)
        };
        while Instant::now() < pause_until {
            if stop.load(Relaxed) {
                return Ok(None);
            }
            std::thread::sleep(
                Duration::from_millis(10)
                    .min(pause_until.saturating_duration_since(Instant::now())),
            );
        }
        backoff = (backoff * 2).min(Duration::from_millis(250));
    }
}

/// deadline 前的 exact-marker 救援读。增量 offset 因 truncate/replace 或瞬时读错漏掉字节时，
/// 从已证明安全的起点重建 gate；随机 marker + 白名单 classifier 保证不会拿历史 turn 冒充本轮。
fn reread_prompt_echo<E: Executor>(
    exec: &E,
    path: &std::path::Path,
    from: u64,
    echo: &TurnEcho,
) -> anyhow::Result<(GateStep, u64, FileTailCheckpoint)> {
    let eof = exec.file_len(path)?;
    let read_from = from
        .min(eof)
        .max(eof.saturating_sub(PROMPT_RESCUE_WINDOW_BYTES as u64));
    let chunk = exec.read_file_range(path, read_from, PROMPT_RESCUE_WINDOW_BYTES)?;
    let end = read_from.saturating_add(chunk.len() as u64);
    let checkpoint = FileTailCheckpoint::from_read(read_from, &chunk);
    let mut gate = PromptEchoGate::default();
    Ok((gate.feed(&chunk, echo)?, end, checkpoint))
}

/// `SteerWatch` 与主 parser 看同一 read，但只能看到归属当前轮的前缀。Progress 表示本轮尚未
/// 收口，整段仍归当前状态机；terminal 时 `pending` 是 terminal 右侧的下一轮/半行，必须截掉。
fn current_turn_input_prefix_len(step: &TailStep, input_len: usize, pending_len: usize) -> usize {
    match step {
        TailStep::Progress { .. } => input_len,
        TailStep::Done { .. } | TailStep::ApiError { .. } => input_len.saturating_sub(pending_len),
    }
}

/// generation 换代后要从 exact marker 重锚，但已经喂过 card/event 回调的字节不能再喂一次。
/// 保留当前 turn 的原始前缀做逐字节对齐；超过单行安全上限后不再扩内存，若此后真发生 replace
/// 就 fail-closed，而不是凭 offset 猜位置、重复或漏发。
#[derive(Default)]
struct ProcessedTurnPrefix {
    bytes: Vec<u8>,
    overflowed: bool,
}

impl ProcessedTurnPrefix {
    fn record(&mut self, chunk: &[u8]) {
        if self.overflowed {
            return;
        }
        let Some(next_len) = self.bytes.len().checked_add(chunk.len()) else {
            self.overflowed = true;
            self.bytes.clear();
            return;
        };
        if next_len > TURN_REANCHOR_PREFIX_LIMIT {
            self.overflowed = true;
            self.bytes.clear();
            return;
        }
        self.bytes.extend_from_slice(chunk);
    }

    fn unseen_suffix(&self, rebuilt_trailing: Vec<u8>) -> anyhow::Result<Vec<u8>> {
        anyhow::ensure!(
            !self.overflowed,
            summon_i18n::format!("r3a377af497", TURN_REANCHOR_PREFIX_LIMIT)
        );
        anyhow::ensure!(
            rebuilt_trailing.len() >= self.bytes.len(),
            summon_i18n::format!("r50fcf38c24", rebuilt_trailing.len(), self.bytes.len())
        );
        anyhow::ensure!(
            rebuilt_trailing.starts_with(&self.bytes),
            summon_i18n::format!("r6bdb4d02b8")
        );
        Ok(rebuilt_trailing[self.bytes.len()..].to_vec())
    }
}

/// 送达相超时的现场快照。**没有它就没法事后定位**:2026-08-11 那次事故只留下一句「超时」,而
/// 「一个字节都没读到(路径错/会话哑了)」与「读到了却没认出 marker(识别规则漏了新形态)」是
/// 两个完全不同的故障,光凭那句话分不开,只能靠翻转录反推。这些字段把二者一眼分开。
struct ArrivalDiag<'a> {
    path: &'a std::path::Path,
    marker: &'a str,
    inject_offset: u64,
    read_bytes: u64,
    saw_enqueue: bool,
    rescans: u64,
    resubmits: u32,
    last_read_error: Option<&'a str>,
}

impl std::fmt::Display for ArrivalDiag<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}",
            summon_i18n::format!(
                "re930552166",
                self.inject_offset,
                self.read_bytes,
                self.saw_enqueue,
                self.rescans,
                self.resubmits,
                self.last_read_error,
                self.marker,
                self.path.display()
            )
        )
    }
}

fn ensure_prompt_echo_not_timed_out(
    now: Instant,
    deadline: Instant,
    diag: &ArrivalDiag<'_>,
) -> anyhow::Result<()> {
    anyhow::ensure!(now < deadline, summon_i18n::format!("r8d0101f576"));
    Ok(())
}

/// Claude 的 `/quit` / `/exit` 会直接结束交互进程，来不及把 transport marker 写进 transcript。
/// 只有**精确的退出命令**能把会话消失解释为预期结果；任何参数、自然语言或普通 prompt 都继续
/// fail-closed，避免把崩溃 / 外部 kill 伪装成正常退出。
fn ensure_expected_session_exit(prompt: &str, unexpected_message: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        matches!(prompt.trim(), "/quit" | "/exit"),
        "{unexpected_message}"
    );
    Ok(())
}

enum CorrelatedPrompt {
    Echoed {
        path: std::path::PathBuf,
        trailing: Vec<u8>,
        marker: String,
        /// exact marker 消费记录右侧的绝对字节边界（含换行）。markerless terminal
        /// 只能与这个边界之后的本轮关联，不得吞掉更早 attach。
        consumed_boundary: u64,
        checkpoint: FileTailCheckpoint,
    },
    /// 精确 `/quit` / `/exit` 已被 Claude 消费，交互会话按请求结束且不会产生 prompt echo。
    SessionExited,
}

/// 接管一轮时的失败类别。调用方 `downcast_ref::<ReattachError>()` 分流:三种结局的补救动作
/// 完全不同(重投 / 恢复卡 / 继续等),靠错误文案区分等于把产品行为压在本地化字符串上。
#[derive(Debug, thiserror::Error)]
pub(crate) enum ReattachError {
    /// 转录里没有 marker 的消费记录,且 pane 已回到空输入框:prompt 从没送进 claude。
    #[error("{}", summon_i18n::text!("r457144d65e"))]
    NotDelivered,
    /// 持久会话不在。
    #[error("{}", summon_i18n::text!("rc88add2db9"))]
    SessionGone,
    /// 等 marker 消费超时(pane 忙但始终未消费)。
    #[error("{}", summon_i18n::text!("r373423f79c"))]
    ConsumeTimeout,
}

/// 一轮 prompt 的注入形态([`Transcript::run_turn`] 的入参)。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum TurnInjection {
    /// 正常轮:run_turn 自己注入(附随机 transport marker,两相送达 + exact 仲裁)。
    Correlated,
    /// turn 类 slash(`/goal <目标>`):整行已由 slash 执行器**过闸**注入并正向确认提交
    /// (命令离开输入行),零 marker。run_turn 不再注入,从 `baseline`(注入前冻结的转录
    /// 文件长度,与 mirror 的 markerless fence 同源同值)起消费。归属证明:注入时 pane 已被
    /// 正向证明是空输入框(无轮在跑),故基线右侧首个 turn 属于本轮 —— 强度低于 exact
    /// marker(排除不了「注入窗口内 attach 用户抢先起轮」的亚秒竞态),已在 spec 第五节
    /// 如实标注;文件换代时无 marker 可重锚,fail-closed 收轮。
    PreInjected { baseline: u64 },
    /// daemon 换代后接管上一代注入的轮:零注入,只定位 marker 的消费记录并从那里消费。
    ///
    /// 四个值全部来自上一代落盘的 [`summon_core::TurnAnchor`]:`inject_offset` 是注入前冻结的
    /// 转录长度(消费记录只可能在它右侧),`consumed_boundary` 是停机前已经观测到的消费边界
    /// (`None` = 停机时还没轮到它被消费,得现找),`prompt_head` 是**注入那一刻**冻结的正文
    /// 开头(空串 = 只认 marker,旧锚点没有这一行)。
    ///
    /// 构造点在 daemon 侧读盘上锚点那一段(`stream_turn_transcript`);捕获层只认形态、不产生它。
    Reattached {
        marker: String,
        prompt_head: String,
        inject_offset: u64,
        consumed_boundary: Option<u64>,
    },
}

/// 跨多次读累积的尾部解析状态机:把字节流切成完整行(半行缓冲过读)、逐行 `parse` 成
/// `AgentMessage`、按 `stop_reason` 判 turn-end。纯逻辑(无 IO/无 tmux),便于单测。
#[derive(Default)]
struct TranscriptTail {
    /// 上次读到的尾部不完整行(无 `\n` 结尾),拼到下次读的开头。
    /// 必须保留原始字节：UTF-8 codepoint 可以跨 read；若每个 chunk 先 lossy decode，替换字符会让
    /// `offset - pending.len()` 偏离文件绝对字节，进而破坏 boundary cutoff 与 checkpoint。
    pending: Vec<u8>,
    /// 本 terminal 之前的 typed transport 顺序证据；每轮收口后清空，不能跨到下一轮。
    marker_evidence: MirrorMarkerEvidence,
    /// 本轮累积的 assistant 文本块,`\n\n` 分段(同 subprocess 源的拼法)。
    assistant: Vec<String>,
    /// 见过终态信号、但那条记录不是 assistant 文本块(thinking 等)→ **候选**终态,
    /// 等 `END_TURN_GRACE` 静默确认(见该常量注释)。
    pending_end: bool,
    /// 候选终态若同时带 API 错误,确认时按 `ApiError` 收(与文本记录路径同语义)。
    pending_error: Option<String>,
    /// 本轮额度终态携带的绝对重置时间。`ApiError` 保持统一终态形态，结构化事实从这里取走。
    session_limit: Option<summon_core::claude_session_limit::ClaudeSessionLimit>,
    /// 已发出但还没见到结果的 tool_use 条数。stall 窗口据此放宽 —— 旧实现用「最后见到的是
    /// tool_use」这个单调标志,工具早就回来了也不复位(现场那轮 5 个 subagent 全部返回后,窗口仍
    /// 卡在 30min,240s 兜底根本轮不上)。
    outstanding_tools: usize,
    /// 本轮见到的事件(跨会话消息收发 + 后台任务完成通知),攒成一个滚动窗口。
    /// 轮收口时由 `take_notice_block` 渲成**一块**、先于回复文本转发(与 reply 共用 typed marker
    /// 仲裁);`reset_turn` 清理防跨轮积累。窗口大小用内置默认 —— 转录尾随拿不到会话配置。
    notices: summon_core::turn_notice::TurnNoticeWindow,
    /// 本 tail 见到启动、还没见到完成通知的后台任务。**跨轮存活**:`reset_turn` 不清 —— 任务在
    /// 轮 N 起、轮 N 收尾后才跑完,主路径据此决定「轮 N 的 end_turn 不收尾,续跑到通知到达」。
    /// 只主路径(run_turn)消费;mirror 路径不读它。
    background: BackgroundLedger,
    /// 事件的另一份,等主路径实时取走。`notices` 走 mirror 的轮收尾转发(太晚),主路径靠
    /// 它在事件到达**那一刻**刷卡。`reset_turn` 不清(事件可能与 end_turn 同 chunk 到达,清了
    /// 会丢);取走用 `drain_notices`。一个 chunk 可能带多条事件,按到达顺序全部入队。
    pending_notices: Vec<summon_core::turn_notice::TurnNotice>,
    /// AskUserQuestion 工具在转录里带出的**真实选项数**(来自 `input.questions[0].options`)。
    /// TUI 菜单在真实选项后还跟「Type something.」「Chat about this」两个 UI 元素,抓屏识别会把
    /// 它们一并收进选项;发卡前按这个数截断,只留真实选项(否则用户点到「Type something.」会在
    /// TUI 开自由输入框、飞书侧无法打字,成了陷阱按钮)。仅本轮有效,`reset_turn` 清。
    ask_user_question_count: Option<usize>,
}

/// 喂一批字节后的状态推进结果。
#[derive(Debug)]
enum TailStep {
    /// 无终态:可能产生了新的流式累积文本(`stream` 为 Some)和/或若干工具名。
    Progress {
        stream: Option<String>,
        tools: Vec<String>,
    },
    /// 本轮终态(assistant `end_turn`):带最终回复文本。
    Done {
        stream: Option<String>,
        tools: Vec<String>,
        final_text: String,
    },
    /// API 错误终态:assistant 消息带 `error` 字段(model_not_found 等),立即收尾并标记失败。
    ApiError {
        stream: Option<String>,
        tools: Vec<String>,
        final_text: String,
        error: String,
    },
}

impl TranscriptTail {
    /// 喂入新读到的字节:只解析到**最后一个 `\n`** 为止的完整行,剩余半行存 `pending` 留到下次
    /// (绝不切半截 UTF-8 行)。返回这批里产生的流式更新 / 工具名 / 是否终态。
    /// `on_event` 把每条 typed 事件 tee 出去(全量观测落盘),在 UI 投影前调用、与 turn-end 判定无关。
    ///
    /// **遇终态时把尚未消费的后续行 prepend 回 `pending`**(C2):同一 chunk 里第 2 个及以后的 turn
    /// 属于「下一轮」,过去被直接丢弃(重启补 backlog 时 ≥2 turn 只补第 1 条);现在留在 pending 里,
    /// 调用方 `reset_turn` 后再喂空串即可继续消费,不丢字节。据此 `offset - pending.len()` 恒为精确
    /// 行边界(见 `tail_forward_turns`)。
    fn feed<B: AsRef<[u8]> + ?Sized>(
        &mut self,
        chunk: &B,
        on_event: &mut dyn FnMut(&AgentMessage),
    ) -> TailStep {
        self.pending.extend_from_slice(chunk.as_ref());
        // 切出完整行:最后一个 `\n` 之后是半行,留回 pending。
        let complete = match self.pending.iter().rposition(|byte| *byte == b'\n') {
            Some(idx) => {
                let mut complete = std::mem::take(&mut self.pending);
                self.pending = complete.split_off(idx + 1);
                complete
            }
            None => Vec::new(),
        };

        let mut tools: Vec<String> = Vec::new();
        let mut streamed = false;
        // 记录已消费到 complete 的字节位置(split_inclusive 保留 `\n`,complete 恒以 `\n` 结尾)。
        let mut consumed = 0usize;
        for line_nl in complete.split_inclusive(|byte| *byte == b'\n') {
            let line_start = consumed;
            consumed += line_nl.len();
            let line_bytes = line_nl.strip_suffix(b"\n").unwrap_or(line_nl);
            // JSONL 协议要求完整行为 UTF-8。坏行沿用旧行为：lossy 后解析失败并忽略；关键是只在
            // 已拿到 `\n` 的整行上 decode，跨 read 的半个 codepoint 始终以原始字节留在 pending。
            let line = String::from_utf8_lossy(line_bytes);
            if let Some(limit) =
                summon_core::claude_session_limit::parse_claude_session_limit_event(&line)
            {
                self.session_limit = Some(limit);
            }
            if self.pending_end && starts_new_transcript_turn(&line) {
                // 当前行已属于下一轮，连同 complete 里的后续行原样塞回 pending；不能先解析 marker/
                // event，否则 A 的送达凭证与 B 的 marker 会在返回前混到一起。pending 里原有的半行
                // 已在尾部，这里 prepend 后顺序仍与文件一致。
                let mut unconsumed = complete[line_start..].to_vec();
                unconsumed.append(&mut self.pending);
                self.pending = unconsumed;
                let stream = streamed.then(|| self.accumulated());
                return self
                    .confirm_pending_end_with(stream, std::mem::take(&mut tools))
                    .expect(summon_i18n::text!("rae25c9c416"));
            }
            // 续跑提示证明 thinking 那条 end_turn 不是终点:模型还要再答一次,可能隔几十秒才落盘,
            // 候选终态留着就会被静默确认成空回复。错误候选不撤,它有自己的收尾。
            if self.pending_end && self.pending_error.is_none() && is_turn_companion(&line) {
                self.pending_end = false;
            }
            if let Some((state, marker)) = transport_marker_evidence(&line) {
                self.marker_evidence.remember(state, marker);
            } else if let Some(marker) = uncertain_transport_marker(&line) {
                self.marker_evidence.remember_uncertain(marker);
            }
            // 入向跨会话消息(Claude Code SendMessage @peer):transcript 里是独立 user 行,
            // `parse_stream_json_line` 只从 user 行提数组型 content 的 tool_result,字符串型
            // 整行丢弃——这里补旁路检测,否则 peer 派单在飞书镜像里完全不可见
            // (2026-08-10 owner 实测「Message from @peer 不回显」)。
            if let Some(notice) = peer_inbound_notice(&line) {
                self.record_notice(notice);
            }
            // 后台任务完成通知:同上,也是被「字符串型 content 整行丢弃」吞掉的一类。
            // 销账与通知同处:通知到达 = 这个后台任务不再 pending。
            self.background.observe(&line);
            if let Some(summary) = task_notice_summary(&line) {
                if let Some(notice) = summon_core::turn_notice::TurnNotice::task(&summary) {
                    self.record_notice(notice);
                }
            }
            for msg in parse_stream_json_line(&line) {
                // tee 在 UI 投影前:无论是否终态,先把 typed 事件喂出去(全量落盘)。
                on_event(&msg);
                if let Some(step) = self.apply(&msg, &mut tools, &mut streamed) {
                    // 已经收到真终态:同一条消息前半截留下的候选标记就地作废(它属于这一轮)。
                    self.pending_end = false;
                    self.pending_error = None;
                    // 终态:complete[consumed..] 是下一轮的开头,塞回 pending 头部留给下次 feed。
                    let leftover = &complete[consumed..];
                    if !leftover.is_empty() {
                        let mut unconsumed = leftover.to_vec();
                        unconsumed.append(&mut self.pending);
                        self.pending = unconsumed;
                    }
                    return step;
                }
            }
            if self.session_limit.is_some() {
                self.pending_end = false;
                self.pending_error = None;
                let leftover = &complete[consumed..];
                if !leftover.is_empty() {
                    let mut unconsumed = leftover.to_vec();
                    unconsumed.append(&mut self.pending);
                    self.pending = unconsumed;
                }
                return TailStep::ApiError {
                    stream: streamed.then(|| self.accumulated()),
                    tools: std::mem::take(&mut tools),
                    final_text: self.accumulated(),
                    error: "claude_five_hour_session_limit".to_string(),
                };
            }
        }
        TailStep::Progress {
            stream: streamed.then(|| self.accumulated()),
            tools,
        }
    }

    /// 把一条 `AgentMessage` 并进状态;返回 Some 表示触达 turn-end。
    fn apply(
        &mut self,
        msg: &AgentMessage,
        tools: &mut Vec<String>,
        streamed: &mut bool,
    ) -> Option<TailStep> {
        match msg.kind {
            AgentKind::Assistant => {
                if let Some(t) = &msg.text {
                    self.assistant.push(t.clone());
                    *streamed = true;
                }
                // turn-end 只认 stop_reason:end_turn;tool_use/stop_sequence 都续 tail。
                // claude 会写 `No response requested.`(stop_sequence)这类中途产物,
                // 若把 stop_sequence 当终态就会误取它当回复(实测 bug:回复 23 却显示该串)。
                if is_turn_end(msg.stop_reason.as_deref()) {
                    // 终态回复 = 触发 end_turn 的这条 assistant 文本(claude 的最终答复),
                    // **不取 accumulated**:否则中途的 stop_sequence 产物会混进最终卡片。
                    let final_text = msg
                        .text
                        .clone()
                        .filter(|t| !t.trim().is_empty())
                        .unwrap_or_else(|| self.accumulated());
                    return Some(TailStep::Done {
                        stream: streamed.then(|| self.accumulated()),
                        tools: std::mem::take(tools),
                        final_text,
                    });
                }
                // 流中断(`API Error: Response stalled mid-stream…`):claude 把它写成**普通 text 块**,
                // `error` 字段是 None、`stop_reason` 是 `stop_sequence` —— 于是既躲过下面那条只看
                // `error` 字段的判据,又因为不是 `end_turn` 而不收轮,结果一直挂到 stall 兜底,
                // 错误文本还混进回复正文。这里当终态收掉,上层据此发「继续」重试卡。
                if let Some(t) = msg.text.as_deref() {
                    if summon_core::retry_wait::is_stream_stall(t) {
                        return Some(TailStep::ApiError {
                            stream: streamed.then(|| self.accumulated()),
                            tools: std::mem::take(tools),
                            // 已累积的正文是**真内容**(错误文本是最后追加的那块),留给用户看。
                            final_text: self.accumulated(),
                            error: t.trim().to_string(),
                        });
                    }
                }
                // API 错误(model_not_found 等):claude 以 `error` 字段 + `stop_sequence`
                // 收尾(不是 end_turn),如果不专门检测就会傻等满 240s stall 超时。
                if let Some(err) = &msg.error {
                    if !err.is_empty() {
                        let final_text = msg
                            .text
                            .clone()
                            .filter(|t| !t.trim().is_empty())
                            .unwrap_or_else(|| self.accumulated());
                        return Some(TailStep::ApiError {
                            stream: streamed.then(|| self.accumulated()),
                            tools: std::mem::take(tools),
                            final_text,
                            error: err.clone(),
                        });
                    }
                }
            }
            AgentKind::ToolUse => {
                if let Some(tool) = &msg.tool {
                    tools.push(tool.clone());
                    // 出向跨会话消息:SendMessage 工具调用即「发往 peer」。镜像轮收尾转发,
                    // 飞书触发轮由 terminal typed marker 仲裁为 observation-only、只由主路径刷卡,
                    // 两条路不双发。
                    if tool == "SendMessage" {
                        if let Some(notice) = peer_outbound_notice(msg.tool_input.as_ref()) {
                            self.record_notice(notice);
                        }
                    }
                    // AskUserQuestion:记下真实选项数,供抓屏识别到 TUI 菜单后截断 UI 元素
                    // (「Type something.」「Chat about this」)用。TUI 渲染比转录落盘晚,这里先记
                    // 数,抓屏路径读到时菜单多半已画出。
                    if tool == "AskUserQuestion" {
                        self.ask_user_question_count = msg
                            .tool_input
                            .as_ref()
                            .and_then(|v| v.get("questions"))
                            .and_then(|v| v.as_array())
                            .and_then(|qs| qs.first())
                            .and_then(|q| q.get("options"))
                            .and_then(|v| v.as_array())
                            .map(|opts| opts.len())
                            .filter(|n| *n > 0);
                    }
                }
                // 工具在飞,等它的 tool_result 才算回来(stall 窗口据此放宽)。
                self.outstanding_tools += 1;
                // 工具块**不置候选终态**:它自证「后面必有结果」,据此收轮就是把慢工具当成结束。
                return None;
            }
            AgentKind::ToolResult => {
                self.outstanding_tools = self.outstanding_tools.saturating_sub(1);
            }
            _ => {} // user/system 等非本源关注项,只走下面与 kind 无关的终态判定
        }
        // **终态判定与 kind 解耦**:thinking(以及将来任何新块类型)带 end_turn 同样要收轮。但它可能
        // 只是同一条消息的前半截(见 END_TURN_GRACE),故只记候选,由调用方静默确认后 `confirm_pending_end`。
        if is_turn_end(msg.stop_reason.as_deref()) {
            self.pending_end = true;
        }
        // API 错误同理:错误字段会挂在该消息的每个块上,只挂 thinking 块时旧实现同样收不了轮。
        if let Some(err) = &msg.error {
            if !err.is_empty() {
                self.pending_end = true;
                self.pending_error = Some(err.clone());
            }
        }
        None
    }

    /// 静默满 `END_TURN_GRACE` 后确认候选终态:同一条消息不会再有文本记录了,这一轮就此收口。
    ///
    /// 回复取本轮**累积**文本(触发候选的那条没有正文);纯 thinking 收尾且累积为空时就是**空回复**
    /// —— 空也必须收轮:轮次是否结束由 claude 的 end_turn 决定,与「有没有话说」无关。上层对空回复
    /// 已有既定路径(不发终态气泡、贴 DONE 收尾),思考过程也早已由 on_event 投影到过程卡,不丢信息。
    fn confirm_pending_end_with(
        &mut self,
        stream: Option<String>,
        tools: Vec<String>,
    ) -> Option<TailStep> {
        if !self.pending_end {
            return None;
        }
        self.pending_end = false;
        let final_text = self.accumulated();
        match self.pending_error.take() {
            Some(error) => Some(TailStep::ApiError {
                stream,
                tools,
                final_text,
                error,
            }),
            None => Some(TailStep::Done {
                stream,
                tools,
                final_text,
            }),
        }
    }

    fn confirm_pending_end(&mut self) -> Option<TailStep> {
        self.confirm_pending_end_with(None, Vec::new())
    }

    /// 是否有未配对的工具在飞(决定 stall 窗口宽窄)。
    fn awaiting_tool(&self) -> bool {
        self.outstanding_tools > 0
    }

    /// AskUserQuestion 转录侧记录的真实选项数(用于截断抓屏菜单里的 TUI UI 元素)。
    fn ask_user_question_count(&self) -> Option<usize> {
        self.ask_user_question_count
    }

    fn accumulated(&self) -> String {
        self.assistant.join("\n\n")
    }

    /// 重置**单轮**累积状态(assistant 文本),但**保留 `pending`**——pending 可能已缓冲下一轮的
    /// 开头字节(同一 chunk 内多 turn)。`tail_forward_turns` 逐轮消费时用它,不能整体
    /// `TranscriptTail::default()` 掉 pending(那正是 C2 丢字节的根源)。
    ///
    /// **同样保留 `background` / `pending_notices`**:它们是跨轮
    /// 状态——后台任务在轮 N 起、通知在轮 N 收尾后才到,主路径靠它们跨轮续跑。清掉会把「该续跑」
    /// 误判成「该收尾」。
    fn reset_turn(&mut self) {
        self.assistant.clear();
        self.pending_end = false;
        self.pending_error = None;
        self.session_limit = None;
        self.outstanding_tools = 0;
        self.notices = summon_core::turn_notice::TurnNoticeWindow::default();
        self.ask_user_question_count = None;
        self.marker_evidence = MirrorMarkerEvidence::default();
    }

    /// 还有后台任务没收到完成通知:主路径在 end_turn 不收尾、续跑。
    fn has_pending_background(&self) -> bool {
        !self.background.is_empty()
    }

    fn pending_background_count(&self) -> usize {
        self.background.len()
    }

    fn held_background(&self) -> Vec<HeldBackgroundTask> {
        self.background.held()
    }

    /// 轮收口时取走本轮事件渲成的那一块(先于回复转发)。取走即清空。
    ///
    /// **整轮只有一块**:镜像每个 end_turn 转发一次,一条事件一条消息就是一轮一串消息
    /// (2026-09-03 owner 实况)。
    ///
    /// 同时清掉 `pending_notices`:镜像路只走这条出口、从不 `drain_notices`,不清的话那个队列
    /// 会随会话一路涨到 watcher 退出。
    fn take_notice_block(&mut self) -> Option<String> {
        self.pending_notices.clear();
        if self.notices.is_empty() {
            return None;
        }
        let block = self.notices.render();
        self.notices = summon_core::turn_notice::TurnNoticeWindow::default();
        Some(block)
    }

    /// 记一条事件:镜像那份攒进窗口等轮收尾,主路径那份进队列等下一拍取走。**两份同源**,
    /// 少写一边就是「一条路看得见、另一条路看不见」。
    fn record_notice(&mut self, notice: summon_core::turn_notice::TurnNotice) {
        self.notices.push(notice.clone());
        self.pending_notices.push(notice);
    }

    /// 取走等着实时刷卡的事件。主路径 run_turn 每拍 feed 后调,刷进本轮的通知卡;
    /// mirror 路径不调,事件留在 `notices` 里走它的轮收尾转发。
    fn drain_notices(&mut self) -> Vec<summon_core::turn_notice::TurnNotice> {
        std::mem::take(&mut self.pending_notices)
    }
}

/// 解析入向跨会话消息:只认 `type:"user"` 且 `message.content` 为**字符串**并含
/// `<cross-session-message ` 包装(SendMessage @peer 投递的落盘形态,2026-08-10 真机样本)。
/// tool_result 行的 content 是数组、`queue-operation` 记账行的 content 在顶层而非 message 下,
/// 都天然不命中——前者互斥,后者避免 enqueue 与真投递双渲染。
/// 后台任务完成通知(`Bash(run_in_background)` / Task 跑完后注入的那条 user 行)里的 summary。
///
/// 与 [`peer_inbound_notice`] 同一类问题、同一条出路:`parse_stream_json_line` 只从 user 行提
/// **数组型** content 里的 tool_result,而这条通知的 content 是**字符串**,整行被丢弃 ——
/// 于是"后台任务跑完了"这件事在飞书侧完全不可见,用户只看到一个再也不动的转圈。
///
/// 只取 `<summary>`:整段还含 task-id / tool-use-id / output-file,那些是给 agent 认领用的,
/// 对读卡片的人是噪音。summary 本身已经是人话(`Background command "…" completed (exit code 0)`)。
///
/// **只有 claude 有这个结构**,codex/cursor 的转录里不存在 —— 取不到就什么都不做,不需要按
/// CLI 分叉。
fn task_notice_summary(line: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
    let content = task_notice_content(&v)?;
    if !content.contains("<task-notification>") {
        return None;
    }
    let start = content.find("<summary>")? + "<summary>".len();
    let end = start + content[start..].find("</summary>")?;
    let summary = content[start..end].trim();
    if summary.is_empty() {
        return None;
    }
    Some(summary.to_string())
}

/// 后台任务的两类。claude 成批停掉后台子 agent 时只给子 agent 落一条汇总,后台命令不受它影响。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BackgroundKind {
    /// 后台命令:显式 `run_in_background`,或前台超时被挪到后台。
    Shell,
    /// 异步子 agent、workflow,或被 SendMessage 续用到后台的 agent。
    Agent,
}

/// 主 Agent 让出本轮时仍在跑的一个后台任务。
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct HeldBackgroundTask {
    /// 完成通知里的 task-id。后台子 agent 的 task-id 就是它的工具调用在 PreToolUse hook 里
    /// 带的 `agent_id`。
    pub task_id: String,
    pub kind: HeldTaskKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HeldTaskKind {
    Shell,
    Agent,
    /// workflow 的工具调用来自它内部起的子 agent,`agent_id` 与 task-id 对不上。
    Workflow,
}

/// 账上的一个后台任务。
#[derive(Clone, Debug)]
struct BackgroundTask {
    kind: BackgroundKind,
    workflow: bool,
    /// 起这一程的工具调用(回执里的 `tool_use_id`)。
    tool_use_id: Option<String>,
    /// 登记顺序,列清单时按它排。
    seq: u64,
}

impl BackgroundTask {
    fn started(&self, notice: &TaskRun) -> bool {
        notice.same_call(self.tool_use_id.as_deref())
    }
}

/// 后台任务的一程:task-id 加上起这一程的工具调用。agent 续用后 task-id 不变,完成通知的
/// `<tool-use-id>` 换成续用的那次 SendMessage —— 靠它分清「上一程的重复通知」和「这一程的通知」。
#[derive(Clone, Debug, PartialEq, Eq)]
struct TaskRun {
    task_id: String,
    tool_use_id: Option<String>,
}

impl TaskRun {
    /// 两边都带调用 id 时才拿它区分;缺一边就只看 task-id。
    fn same_call(&self, tool_use_id: Option<&str>) -> bool {
        match (self.tool_use_id.as_deref(), tool_use_id) {
            (Some(a), Some(b)) => a == b,
            _ => true,
        }
    }

    fn same_run(&self, other: &TaskRun) -> bool {
        self.task_id == other.task_id && self.same_call(other.tool_use_id.as_deref())
    }
}

/// `recently_settled` / `unmatched_notices` 的容量。它们只需盖住同一条通知的几种落盘形态、
/// 同一批并行工具里先后落盘的回执与通知;Monitor 每个事件都发通知,不设上限会一路涨。
const RECENT_NOTICES_CAP: usize = 32;

fn push_recent(recent: &mut std::collections::VecDeque<TaskRun>, run: TaskRun) {
    if recent.len() == RECENT_NOTICES_CAP {
        recent.pop_front();
    }
    recent.push_back(run);
}

/// 后台任务账本:见到启动、还没见到完成通知的后台任务,按 task-id 记 —— 后台命令(显式
/// `run_in_background`,或前台超时被挪到后台)、异步子 agent / workflow、被 SendMessage 续用到
/// 后台的 agent。完成通知只销它点名的那几个:同一个 agent 续用后会再通知一次,Monitor 每个事件都
/// 发一条通知,子 agent 自己的后台命令与别的轮遗留的任务也会通知到主转录。
#[derive(Debug, Default)]
struct BackgroundLedger {
    tasks: std::collections::HashMap<String, BackgroundTask>,
    /// 最近销过账的那几程(有界)。同一条通知会以几种形态先后落盘,后到的那几份只是重复。
    recently_settled: std::collections::VecDeque<TaskRun>,
    /// 最近到达、却对不上任何任务的通知(有界)。并行工具调用时启动回执可能晚于完成通知落盘,
    /// 回执到时先查这里,免得登记一程通知已经来过的任务、一直等到续跑上限。
    unmatched_notices: std::collections::VecDeque<TaskRun>,
    next_seq: u64,
}

impl BackgroundLedger {
    /// 按一行转录记账:先销完成通知点名的那几程,再登记新起的一程,最后处理成批停掉子 agent。
    fn observe(&mut self, line: &str) {
        for run in task_notice_runs(line) {
            self.settle(run);
        }
        if let Some((run, kind, workflow)) = background_task_started(line) {
            self.start(run, kind, workflow);
        }
        if background_agents_killed(line) {
            self.tasks
                .retain(|_, task| task.kind != BackgroundKind::Agent);
        }
    }

    fn is_empty(&self) -> bool {
        self.tasks.is_empty()
    }

    fn len(&self) -> usize {
        self.tasks.len()
    }

    /// 登记开始跑的一程。这一程的完成通知已经先到过(见 `unmatched_notices`)就不登记 ——
    /// 登记了就再也等不到通知。
    fn start(&mut self, run: TaskRun, kind: BackgroundKind, workflow: bool) {
        if let Some(at) = self.unmatched_notices.iter().position(|n| n.same_run(&run)) {
            self.unmatched_notices.remove(at);
            // 这一程已经结束;它的通知后到的其余形态只是重复。
            push_recent(&mut self.recently_settled, run);
            return;
        }
        let task = BackgroundTask {
            kind,
            workflow,
            tool_use_id: run.tool_use_id,
            seq: self.next_seq,
        };
        self.next_seq += 1;
        self.tasks.insert(run.task_id, task);
    }

    /// 一条完成通知销掉它那一程。已销过账那一程的重复形态不算数;对不上任何任务的记下,留给
    /// 可能晚到的启动回执。点名的任务在账上、调用 id 却对不上又不是重复时照样销账 —— 宁可提前
    /// 交付,也不等一个可能永远不来的通知。
    fn settle(&mut self, notice: TaskRun) {
        let duplicate = self
            .recently_settled
            .iter()
            .any(|run| run.same_run(&notice));
        match self.tasks.get(&notice.task_id) {
            Some(task) if !duplicate || task.started(&notice) => {
                let tool_use_id = task.tool_use_id.clone();
                self.tasks.remove(&notice.task_id);
                push_recent(
                    &mut self.recently_settled,
                    TaskRun {
                        task_id: notice.task_id,
                        tool_use_id: notice.tool_use_id.or(tool_use_id),
                    },
                );
            }
            Some(_) => {}
            None if duplicate => {}
            None => push_recent(&mut self.unmatched_notices, notice),
        }
    }

    /// 还在跑的任务,按登记顺序。
    fn pending(&self) -> Vec<&BackgroundTask> {
        let mut tasks: Vec<&BackgroundTask> = self.tasks.values().collect();
        tasks.sort_by_key(|task| task.seq);
        tasks
    }

    /// 还在跑的任务连同 task-id,按登记顺序。
    fn held(&self) -> Vec<HeldBackgroundTask> {
        let mut tasks: Vec<(&String, &BackgroundTask)> = self.tasks.iter().collect();
        tasks.sort_by_key(|(_, task)| task.seq);
        tasks
            .into_iter()
            .map(|(task_id, task)| HeldBackgroundTask {
                task_id: task_id.clone(),
                kind: match (task.kind, task.workflow) {
                    (BackgroundKind::Shell, _) => HeldTaskKind::Shell,
                    (BackgroundKind::Agent, false) => HeldTaskKind::Agent,
                    (BackgroundKind::Agent, true) => HeldTaskKind::Workflow,
                },
            })
            .collect()
    }
}

/// 一个还在跑的后台任务:种类,加上起它的那次工具调用写的 `description`(没有就是 `None`)。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PendingBackgroundTask {
    pub(crate) kind: BackgroundKind,
    pub(crate) description: Option<String>,
}

/// 读整份转录,列出此刻账上还在跑的后台任务(与托管轮续跑判定同一本账)。会话进程结束时,
/// 它们会随进程一起停。
pub(crate) fn pending_background_tasks(
    transcript: &std::path::Path,
) -> std::io::Result<Vec<PendingBackgroundTask>> {
    let mut descriptions: std::collections::HashMap<String, String> = Default::default();
    let ledger = scan_background_ledger(transcript, |line| {
        record_tool_use_descriptions(line, &mut descriptions)
    })?;
    Ok(ledger
        .pending()
        .into_iter()
        .map(|task| PendingBackgroundTask {
            kind: task.kind,
            description: task
                .tool_use_id
                .as_ref()
                .and_then(|id| descriptions.get(id))
                .cloned(),
        })
        .collect())
}

/// 读整份转录,判断这些后台任务的事还没完:有的还在跑(与托管轮续跑判定同一本账)、完成通知
/// 入了 claude 的输入队列还没被消费,或主 Agent 还没对它们最后被消费的那条完成通知收尾(消费
/// 之后还没有主对话的 end_turn)。主 Agent 处理完成通知期间动的工具仍归起任务的那一轮。
///
/// 入队与消费按 task-id 逐条记:队列里可能同时排着几条通知和别人的消息。消费只认带通知正文
/// 的 user 行与 `queued_command` 附件(与 marker 消费判定同一组形态);被 remove 撤出而没有消费
/// 记录的通知仍按排队算,由持有者的续跑上限收口。
pub(crate) fn background_work_outstanding(
    transcript: &std::path::Path,
    task_ids: &[&str],
) -> std::io::Result<bool> {
    let mut index = 0usize;
    let mut queued: std::collections::HashSet<String> = Default::default();
    let mut last_consumed = None;
    let mut last_end_turn = None;
    let ledger = scan_background_ledger(transcript, |line| {
        let held: Vec<String> = task_notice_runs(line)
            .into_iter()
            .map(|run| run.task_id)
            .filter(|id| task_ids.contains(&id.as_str()))
            .collect();
        if !held.is_empty() {
            if is_queue_enqueue(line) {
                queued.extend(held);
            } else {
                for id in &held {
                    queued.remove(id);
                }
                last_consumed = Some(index);
            }
        }
        if is_main_end_turn(line) {
            last_end_turn = Some(index);
        }
        index += 1;
    })?;
    let running = task_ids.iter().any(|id| ledger.tasks.contains_key(*id));
    let unanswered =
        last_consumed.is_some_and(|consumed| last_end_turn.is_none_or(|end| end < consumed));
    Ok(running || !queued.is_empty() || unanswered)
}

fn is_queue_enqueue(line: &str) -> bool {
    line.contains("\"queue-operation\"")
        && serde_json::from_str::<serde_json::Value>(line.trim())
            .is_ok_and(|v| v["type"] == "queue-operation" && v["operation"] == "enqueue")
}

/// 主对话(不是子 agent)里以 end_turn 收尾的 assistant 行。
fn is_main_end_turn(line: &str) -> bool {
    line.contains("\"end_turn\"")
        && !line.contains("\"isSidechain\":true")
        && serde_json::from_str::<serde_json::Value>(line.trim())
            .is_ok_and(|v| v["type"] == "assistant" && v["message"]["stop_reason"] == "end_turn")
}

fn scan_background_ledger(
    transcript: &std::path::Path,
    mut each_line: impl FnMut(&str),
) -> std::io::Result<BackgroundLedger> {
    use std::io::BufRead as _;
    let reader = std::io::BufReader::new(std::fs::File::open(transcript)?);
    let mut ledger = BackgroundLedger::default();
    for line in reader.lines() {
        let line = line?;
        ledger.observe(&line);
        each_line(&line);
    }
    Ok(ledger)
}

/// 主对话里每次工具调用的 `description`,按 tool_use id 记。后台任务的启动回执只带 id,
/// 人能读的说明在发起调用的那条 assistant 行里。
fn record_tool_use_descriptions(
    line: &str,
    descriptions: &mut std::collections::HashMap<String, String>,
) {
    if !line.contains("\"tool_use\"") || line.contains("\"isSidechain\":true") {
        return;
    }
    let Ok(v) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
        return;
    };
    let Some(blocks) = v
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(serde_json::Value::as_array)
    else {
        return;
    };
    for block in blocks {
        if block.get("type").and_then(serde_json::Value::as_str) != Some("tool_use") {
            continue;
        }
        let id = block.get("id").and_then(serde_json::Value::as_str);
        let description = block
            .get("input")
            .and_then(|input| input.get("description"))
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty());
        if let (Some(id), Some(description)) = (id, description) {
            descriptions.insert(id.to_string(), description.to_string());
        }
    }
}

/// 完成通知点名的每一程。进程重启后的孤儿汇总一条列好几个 `<task-id>`、不带 `<tool-use-id>`,
/// 末尾那个 `__orphan_summary__:*` 是扫描记号,不是任务。
fn task_notice_runs(line: &str) -> Vec<TaskRun> {
    if !line.contains("<task-notification>") {
        return Vec::new();
    }
    let Ok(v) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
        return Vec::new();
    };
    // 通知在主 agent 忙时到达,claude 有时只在它被消费时落一条 `queued_command` 附件。
    let queued = || {
        let attachment = v.get("attachment")?;
        (attachment.get("type")?.as_str()? == "queued_command")
            .then(|| attachment.get("prompt")?.as_str())
            .flatten()
    };
    let Some(content) = task_notice_content(&v).or_else(queued) else {
        return Vec::new();
    };
    let tool_use_id = xml_tag(content, "tool-use-id");
    let mut rest = content;
    let mut runs = Vec::new();
    while let Some(start) = rest.find("<task-id>") {
        rest = &rest[start + "<task-id>".len()..];
        let Some(end) = rest.find("</task-id>") else {
            break;
        };
        let id = rest[..end].trim();
        if !id.is_empty() && !id.starts_with("__orphan_summary__") {
            runs.push(TaskRun {
                task_id: id.to_string(),
                tool_use_id: tool_use_id.clone(),
            });
        }
        rest = &rest[end..];
    }
    runs
}

fn xml_tag(text: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let start = text.find(&open)? + open.len();
    let end = start + text[start..].find(&format!("</{tag}>"))?;
    let value = text[start..end].trim();
    (!value.is_empty()).then(|| value.to_string())
}

/// 这一行让一个任务开始在后台跑:返回这一程(日后完成通知里的 task-id 与起它的工具调用)。
///
/// SendMessage 的续用只认带 `resumedAgentId` 的回执。「仅入队」那种也可能是发给 teammate 的,
/// 当成后台任务就得等一个不会来的通知,一直等到续跑上限;真正把已停下的 agent 续用到后台时,
/// claude 回的是带 `resumedAgentId` 的那种。最后一项标明这一程是不是 workflow。
fn background_task_started(line: &str) -> Option<(TaskRun, BackgroundKind, bool)> {
    // isSidechain 的行属于子 agent 自己的对话:它起的是它的后台任务,完成通知回它那里。
    if line.contains("\"isSidechain\":true") {
        return None;
    }
    let (task_id, kind, workflow) = if line.contains("\"backgroundTaskId\"") {
        let v: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
        let id = v.get("toolUseResult")?.get("backgroundTaskId")?.as_str()?;
        (id.to_string(), BackgroundKind::Shell, false)
    } else if line.contains("async_launched") {
        match summon_core::subagent::detect_launch(line)? {
            summon_core::subagent::LaunchEvent::Agent { agent_id, .. } => {
                (agent_id, BackgroundKind::Agent, false)
            }
            summon_core::subagent::LaunchEvent::Workflow { task_id, .. } => {
                (task_id, BackgroundKind::Agent, true)
            }
        }
    } else if line.contains("resumedAgentId") {
        let id = summon_core::subagent::detect_resume(line)?.agent_id;
        (id, BackgroundKind::Agent, false)
    } else {
        return None;
    };
    if task_id.is_empty() {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
    let tool_use_id = v
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(serde_json::Value::as_array)
        .and_then(|blocks| {
            blocks.iter().find_map(|b| {
                (b.get("type")?.as_str()? == "tool_result")
                    .then(|| b.get("tool_use_id")?.as_str())
                    .flatten()
            })
        })
        .map(str::to_string);
    Some((
        TaskRun {
            task_id,
            tool_use_id,
        },
        kind,
        workflow,
    ))
}

/// claude 成批停掉后台子 agent(用户按停)时只落一条 `system/agents_killed`,不再给其中每个
/// agent 发完成通知。
fn background_agents_killed(line: &str) -> bool {
    line.contains("\"agents_killed\"")
        && serde_json::from_str::<serde_json::Value>(line.trim()).is_ok_and(|v| {
            v.get("type").and_then(serde_json::Value::as_str) == Some("system")
                && v.get("subtype").and_then(serde_json::Value::as_str) == Some("agents_killed")
        })
}

/// 从转录行里取 task-notification 正文。两种格式:
/// - 旧版:`type:"user"` 的字符串 content
/// - 新版(Claude Code 2.1.235 起):task-notification 不再落成 user 消息,只发
///   `type:"queue-operation"` + `operation:"enqueue"` 的 content(另有一条同内容的
///   `attachment` 和一条 `operation:"remove"` 销账行)。不解析 enqueue,后台任务账本
///   永不到 0,轮次挂到 continuation_cap(真机:samplellm 群 13:21 轮卡了 50+ 分钟)。
///   `remove` 是同内容的销账行,跳过——否则一条通知减两次。
fn task_notice_content(v: &serde_json::Value) -> Option<&str> {
    let typ = v.get("type").and_then(serde_json::Value::as_str)?;
    match typ {
        "user" => v.get("message")?.get("content")?.as_str(),
        "queue-operation" => {
            if v.get("operation").and_then(serde_json::Value::as_str) != Some("enqueue") {
                return None;
            }
            v.get("content")?.as_str()
        }
        _ => None,
    }
}

fn peer_inbound_notice(line: &str) -> Option<summon_core::turn_notice::TurnNotice> {
    let v: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
    if v.get("type").and_then(serde_json::Value::as_str) != Some("user") {
        return None;
    }
    let content = v.get("message")?.get("content")?.as_str()?;
    let start = content.find("<cross-session-message ")?;
    let tag_end = start + content[start..].find('>')?;
    let attrs = &content[start..tag_end];
    // 优先 from-name(人类可读的会话名);缺失退 from(uds socket 地址)。
    let from = attr_value(attrs, "from-name")
        .or_else(|| attr_value(attrs, "from"))
        .unwrap_or_else(|| "unknown".to_string());
    let body = &content[tag_end + 1..];
    let body = match body.find("</cross-session-message>") {
        Some(i) => &body[..i],
        None => body,
    };
    summon_core::turn_notice::TurnNotice::peer(
        summon_core::turn_notice::PeerDirection::In,
        &from,
        body.trim(),
    )
}

/// `key="value"` 属性取值。包装标签的属性面就三个且格式固定,不值得引 XML 解析器;
/// 带前导空格匹配(` from="`),避免 `from` 误命中 `from-name`/`from-mode` 的前缀。
fn attr_value(attrs: &str, key: &str) -> Option<String> {
    let pat = format!(" {key}=\"");
    let s = attrs.find(&pat)? + pat.len();
    let e = s + attrs[s..].find('"')?;
    Some(attrs[s..e].to_string())
}

/// 解析出向跨会话消息:SendMessage 工具入参 → 对端 + 一句摘要(优先 summary,退 message)。
/// 摘要的长度上界由通知卡统一施加,这里不各截一次。
fn peer_outbound_notice(
    input: Option<&serde_json::Value>,
) -> Option<summon_core::turn_notice::TurnNotice> {
    let to = input
        .and_then(|i| i.get("to"))
        .and_then(serde_json::Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .unwrap_or("?");
    let gist = input
        .and_then(|i| {
            i.get("summary")
                .and_then(serde_json::Value::as_str)
                .or_else(|| i.get("message").and_then(serde_json::Value::as_str))
        })
        .unwrap_or_default();
    summon_core::turn_notice::TurnNotice::peer(
        summon_core::turn_notice::PeerDirection::Out,
        to,
        gist,
    )
}

/// 转录捕获源:注入走 `Session`(tmux),读输出走 `Executor` tail 钉死 sid 的 jsonl。
pub struct Transcript<'a, E: Executor> {
    pub sess: &'a Session<'a>,
    pub exec: E,
}

struct PromptEchoWait<'a> {
    path: &'a std::path::Path,
    relocation: Option<MissingTranscriptSearch>,
    prompt: &'a str,
    marker: String,
    inject_offset: u64,
    file_identity: Option<FileIdentity>,
    arrival_timeout: Duration,
    consume_timeout: Duration,
    poll: Duration,
    resubmit_every: Duration,
    resubmit_tries: u32,
}

/// `resubmit`:看一眼屏,本轮正文还停在输入框里就补按一次回车,返回按没按。只在送达相
/// (还没见过本轮 marker)按节拍调用 —— 见过入队之后输入框里的字已经不是本轮的了。
fn wait_for_prompt_echo<E: Executor>(
    exec: &E,
    mut wait: PromptEchoWait<'_>,
    mut session_alive: impl FnMut() -> bool,
    mut resubmit: impl FnMut() -> bool,
) -> anyhow::Result<CorrelatedPrompt> {
    let mut path = wait.path.to_path_buf();
    let head = prompt_head(wait.prompt);
    let echo = TurnEcho {
        marker: &wait.marker,
        head: &head,
    };
    let mut offset = wait.inject_offset;
    let sent_at = Instant::now();
    let mut enqueued_at: Option<Instant> = None;
    // 不带正文的入队只放宽等待上限,不算本轮入队:补按回车照旧(它先看屏上是不是本轮正文),
    // 开闸仍只认带 marker 的消费记录。
    let mut queued_without_text_at: Option<Instant> = None;
    let mut read_bytes = 0u64;
    let mut rescans = 0u64;
    let mut resubmits = 0u32;
    let mut next_resubmit = sent_at + wait.resubmit_every;
    let mut last_read_error: Option<String> = None;
    let mut gate = PromptEchoGate::default();
    loop {
        if let Some(search) = wait.relocation.as_mut() {
            if search.relocate_missing(exec, &mut path)? {
                // 只有 exact marker 可以重新确立消费边界，旧文件的 offset/identity 不可迁移。
                offset = 0;
                gate = PromptEchoGate::default();
                wait.file_identity = None;
                rescans += 1;
            }
        }
        let current_len = match exec.file_len(&path) {
            Ok(len) => Some(len),
            Err(error) => {
                last_read_error = Some(error.to_string());
                None
            }
        };
        let current_identity = match exec.file_identity(&path) {
            Ok(identity) => identity,
            Err(error) => {
                last_read_error = Some(error.to_string());
                None
            }
        };
        if transcript_needs_rescan(wait.file_identity, current_identity, offset, current_len) {
            log(&summon_i18n::format!("r9cc19ba068", path.display()));
            offset = 0;
            gate = PromptEchoGate::default();
            rescans += 1;
        }
        if current_identity.is_some() {
            wait.file_identity = current_identity;
        }

        let read_from = offset;
        let chunk = match exec.read_file_tail(&path, offset) {
            Ok(chunk) => chunk,
            Err(error) => {
                last_read_error = Some(error.to_string());
                Vec::new()
            }
        };
        if !chunk.is_empty() {
            offset += chunk.len() as u64;
            read_bytes += chunk.len() as u64;
            match gate.feed(&chunk, &echo)? {
                GateStep::Consumed(trailing) => {
                    let consumed_boundary = offset.saturating_sub(trailing.len() as u64);
                    return Ok(CorrelatedPrompt::Echoed {
                        path,
                        trailing,
                        marker: wait.marker,
                        consumed_boundary,
                        checkpoint: FileTailCheckpoint::from_read(read_from, &chunk),
                    });
                }
                GateStep::Enqueued => {
                    enqueued_at.get_or_insert_with(Instant::now);
                }
                // 重扫过就是从文件头读的,读到的不带正文的入队可能是更早几轮留下的,不放宽。
                GateStep::QueuedWithoutText if rescans == 0 => {
                    note_queued_without_text(&mut queued_without_text_at, &wait.marker);
                }
                GateStep::QueuedWithoutText => {}
                GateStep::Waiting => {}
            }
        } else if !session_alive() {
            ensure_expected_session_exit(wait.prompt, summon_i18n::text!("r512f8df08a"))?;
            return Ok(CorrelatedPrompt::SessionExited);
        }
        let now = Instant::now();
        let deadline = prompt_wait_deadline_with(
            sent_at,
            enqueued_at.or(queued_without_text_at),
            wait.arrival_timeout,
            wait.consume_timeout,
        );
        if enqueued_at.is_none()
            && resubmits < wait.resubmit_tries
            && now >= next_resubmit
            && now < deadline
        {
            next_resubmit = now + wait.resubmit_every;
            if resubmit() {
                resubmits += 1;
                log(&summon_i18n::format!("rc0f4be89c1", wait.marker));
            }
        }
        if now >= deadline {
            // marker 是本轮随机 UUID。无条件从 0 救援一次，覆盖初始 len/identity 非原子快照。
            match reread_prompt_echo(exec, &path, 0, &echo) {
                Ok((GateStep::Consumed(trailing), end, checkpoint)) => {
                    return Ok(CorrelatedPrompt::Echoed {
                        path,
                        consumed_boundary: end.saturating_sub(trailing.len() as u64),
                        trailing,
                        marker: wait.marker,
                        checkpoint,
                    });
                }
                Ok((GateStep::Enqueued, _, _)) if enqueued_at.is_none() => {
                    enqueued_at = Some(now);
                    continue;
                }
                // 救援从文件头读,读到的不带正文的入队可能是更早几轮留下的,不放宽。
                Ok(_) => {}
                Err(error) => last_read_error = Some(error.to_string()),
            }
        }
        ensure_prompt_echo_not_timed_out(
            now,
            prompt_wait_deadline_with(
                sent_at,
                enqueued_at.or(queued_without_text_at),
                wait.arrival_timeout,
                wait.consume_timeout,
            ),
            &ArrivalDiag {
                path: &path,
                marker: &wait.marker,
                inject_offset: wait.inject_offset,
                read_bytes,
                saw_enqueue: enqueued_at.is_some(),
                rescans,
                resubmits,
                last_read_error: last_read_error.as_deref(),
            },
        )?;
        std::thread::sleep(wait.poll);
    }
}

/// 注入之后转录里出现不带正文的入队记录:claude 正在压缩上下文,压完才写本轮的消费记录,压缩
/// 常常超过送达窗。这条记录认不出归属,但它说明 claude 活着、刚收下了一条输入,把上限放宽到
/// 消费窗;真不是本轮的,最坏是送达失败晚些报出来。
fn note_queued_without_text(at: &mut Option<Instant>, marker: &str) {
    if at.is_none() {
        *at = Some(Instant::now());
        log(&summon_i18n::format!("r3c8310c412", marker));
    }
}

/// 接管一轮时的定位参数。超时与节拍做成字段(与 [`PromptEchoWait`] 同款):生产传
/// `PROMPT_CONSUME_TIMEOUT` / `POLL`,单测传小值,免得为一条判据真等十分钟。
struct ReattachWait<'a> {
    path: &'a std::path::Path,
    /// 注入那一刻冻结在锚点里的正文开头。**不能拿接管时重建的 prompt 现算** —— 重建的正文
    /// 可能带时间戳之类的差异,算出来的 head 对不上就会把一条完好的消息判成「被 CLI 截断」。
    /// 空串 = 只认 marker(随机 UUID 本身就唯一),旧锚点走这条。
    prompt_head: &'a str,
    marker: &'a str,
    inject_offset: u64,
    consumed_boundary: Option<u64>,
    consume_timeout: Duration,
    poll: Duration,
}

/// 定位上一代注入的 marker 的消费记录,产出「消费记录右侧的字节 + 绝对边界 + checkpoint」,
/// 交给 `run_turn` 下方与正常轮共用的 poll 循环。
///
/// 抽成自由函数是为了让四条判据(有边界 / 无边界 / 未送达 / 消费超时)脱离 tmux 被钉死;
/// `run_turn` 的接管臂只负责把 `exec` 与两个探针接进来,自己不做判断。
///
/// `pane_idle` 是「这条 prompt 从没进 claude」的**唯一正向证据**:转录里找不到 marker 时,
/// pane 回到空输入框说明上一代的注入落了空,可以安全重投;pane 还忙着就只能继续等 ——
/// 此刻 marker 可能正躺在 claude 队列里(实测最长 152.7s),猜「没送到」就是同一句话跑两遍。
fn locate_reattached_turn<E: Executor>(
    exec: &E,
    wait: ReattachWait<'_>,
    mut session_alive: impl FnMut() -> bool,
    mut pane_idle: impl FnMut() -> bool,
) -> anyhow::Result<(Vec<u8>, u64, FileTailCheckpoint)> {
    if !session_alive() {
        return Err(ReattachError::SessionGone.into());
    }
    // 空 marker 会让 `TurnEcho::judge` 的 `contains("")` 恒真,任何 user 记录都能冒充本轮。
    // 锚点缺它就没有可定位的东西,普通错误交恢复卡。
    anyhow::ensure!(!wait.marker.is_empty(), summon_i18n::format!("r7435746a17"));

    // 锚点带着消费边界本身就是「上一代亲眼见过这条 prompt 被消费」的证据。即便边界复验不过、
    // 要退回重找,这条证据也不会因此失效 —— 所以此后再判「未送达」就是让同一句话跑两遍。
    // 借 `saw_enqueue` 表达(它的语义正是「已证明在 claude 手里,永久否掉 NotDelivered」)。
    let mut saw_enqueue = wait.consumed_boundary.is_some();

    if let Some(boundary) = wait.consumed_boundary {
        if let Some(located) = reattach_at_consumed_boundary(exec, &wait, boundary)? {
            return Ok(located);
        }
        log(&summon_i18n::format!("r8a79fa8b93", wait.path.display()));
    }

    let echo = TurnEcho {
        marker: wait.marker,
        head: wait.prompt_head,
    };
    let deadline = Instant::now() + wait.consume_timeout;
    let mut last_probe: Option<Instant> = None;
    // 上一拍看到的文件长度。转录只增不减,长度没变就没有新记录可看 —— 不重读那最多 8MiB、
    // 也不重解析一遍。
    let mut scanned_len: Option<u64> = None;
    loop {
        let eof = exec.file_len(wait.path)?;
        // 文件比注入前还短 = 转录换代,本轮的消费记录不可能在这份文件里。白等满消费窗只会
        // 把恢复卡推迟十分钟。
        anyhow::ensure!(
            eof >= wait.inject_offset,
            summon_i18n::format!("raa157fcb10", wait.inject_offset)
        );
        if scanned_len != Some(eof) {
            scanned_len = Some(eof);
            let (step, end, checkpoint) =
                reread_prompt_echo(exec, wait.path, wait.inject_offset, &echo)?;
            match step {
                GateStep::Consumed(trailing) => {
                    let boundary = end.saturating_sub(trailing.len() as u64);
                    return Ok((trailing, boundary, checkpoint));
                }
                // 入队记录是「这条 prompt 已经在 claude 手里」的铁证,它一出现就永久否掉了
                // `NotDelivered`:此后哪怕抓屏拍到一瞬间的空输入框(队列刚被消费、消费记录还没
                // 落盘),也不能判未送达 —— 判错的代价是同一句话跑两遍。
                GateStep::Enqueued => saw_enqueue = true,
                // 认不出归属,不能拿它否掉 `NotDelivered`。
                GateStep::QueuedWithoutText | GateStep::Waiting => {}
            }
        }
        if !session_alive() {
            return Err(ReattachError::SessionGone.into());
        }
        // 抓屏按 `PROBE_EVERY` 节流:消费窗最长十分钟,每个 POLL 都 fork 一次 capture-pane 是
        // 白烧,而「输入框空了」这件事不需要亚秒级发现。
        if !saw_enqueue && last_probe.is_none_or(|probed| probed.elapsed() >= PROBE_EVERY) {
            last_probe = Some(Instant::now());
            if pane_idle() {
                return Err(ReattachError::NotDelivered.into());
            }
        }
        if Instant::now() >= deadline {
            return Err(ReattachError::ConsumeTimeout.into());
        }
        std::thread::sleep(wait.poll);
    }
}

/// 用停机前记下的消费边界直接接管。`None` = 这个边界复验不过,调用方退回按 marker 重找。
fn reattach_at_consumed_boundary<E: Executor>(
    exec: &E,
    wait: &ReattachWait<'_>,
    boundary: u64,
) -> anyhow::Result<Option<(Vec<u8>, u64, FileTailCheckpoint)>> {
    // 长度证明:转录已经比停机时记下的边界还短 = 文件换代,这份文件里根本没有那条消费记录,
    // 凭旧边界续读就是把别的字节当本轮。普通错误,交上层的恢复卡。
    let anchor = FileTailCheckpoint::capture_at(exec, wait.path, boundary)
        .context(summon_i18n::text!("r651b9b35e8"))?;
    // 长度对不等于内容对:等长或更长的**另一代**转录同样过得了上面那一关,而边界右侧的字节
    // 会被当成本轮回复喂进 parser、边界本身还会被登记给 mirror 仲裁器。所以边界左侧这段必须
    // 含本轮那个随机 marker —— 消费记录就落在它里面,别的代不可能含它。
    if !bytes_contain(&anchor.bytes, wait.marker) {
        return Ok(None);
    }
    let trailing = exec.read_file_tail(wait.path, boundary)?;
    // checkpoint 必须覆盖已经交给主循环的那段字节,否则下一拍 `read_new` 会把它们再读一遍,
    // 同一段回复被解析两次。
    let mut window = anchor.bytes;
    window.extend_from_slice(&trailing);
    Ok(Some((
        trailing,
        boundary,
        FileTailCheckpoint::from_read(anchor.start, &window),
    )))
}

/// 字节里有没有这段 ASCII 文本。空串恒 false —— 「什么都算命中」不是复验。
fn bytes_contain(haystack: &[u8], needle: &str) -> bool {
    let needle = needle.as_bytes();
    !needle.is_empty()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
}

/// 读 resume 记录里与当前 CLI 一致的 cwd 字段(无记录 / cli 不符 / 无 cwd → None)。
fn session_record_cwd(agent: &summon_core::registry::Agent, tmux_session: &str) -> Option<String> {
    let rec = crate::session::resume_store::read_record(agent, tmux_session)?;
    (rec.cli == agent.cli_kind()).then_some(rec.cwd).flatten()
}

/// 工作目录编码不可逆；捕获链路的定位结果以完整文件路径传递。
fn verify_cwd_or_sid_search(
    projects: &std::path::Path,
    cwd: &str,
    sid: &str,
) -> anyhow::Result<Option<std::path::PathBuf>> {
    let original = transcript_path_in_projects(projects, cwd, sid);
    if transcript_file_exists(&original)? {
        return Ok(Some(original));
    }
    let canonical = predict_transcript_path(projects, cwd, sid)?;
    if transcript_file_exists(&canonical)? {
        return Ok(Some(canonical));
    }
    Ok(
        find_transcript_in_projects(projects, sid)?.inspect(|found| {
            log(&summon_i18n::format!("rda1c6bd958", found.display()));
        }),
    )
}

/// 冷启动预测使用真实 cwd；历史转录目录名仍由原始字符串精确编码，不重写用户配置。
fn predict_transcript_path(
    projects: &std::path::Path,
    cwd: &str,
    sid: &str,
) -> anyhow::Result<std::path::PathBuf> {
    let real = std::fs::canonicalize(cwd).with_context(|| summon_i18n::format!("r88e61a373b"))?;
    anyhow::ensure!(real.is_dir(), summon_i18n::format!("ref1eaa44e7"));
    Ok(transcript_path_in_projects(
        projects,
        &real.to_string_lossy(),
        sid,
    ))
}

fn transcript_path_in_projects(
    projects: &std::path::Path,
    cwd: &str,
    sid: &str,
) -> std::path::PathBuf {
    projects
        .join(project_dirname(cwd))
        .join(format!("{sid}.jsonl"))
}

/// 在记录的 cwd、有效启动目录与 SID 搜索间定位指定会话的转录。
/// 冷启按有效启动目录的真实路径预测；配置均失效时只允许恢复既有同 SID 转录。
pub(crate) fn resolve_transcript_path(
    agent: &summon_core::registry::Agent,
    tmux_session: &str,
    chat_id: Option<&str>,
    explicit: Option<&str>,
    sid: &str,
) -> anyhow::Result<std::path::PathBuf> {
    let home = dirs::home_dir().context(summon_i18n::text!("r794aefbfad"))?;
    resolve_transcript_path_in(
        agent,
        tmux_session,
        chat_id,
        explicit,
        sid,
        &home.join(".claude").join("projects"),
    )
}

fn resolve_transcript_path_in(
    agent: &summon_core::registry::Agent,
    tmux_session: &str,
    chat_id: Option<&str>,
    explicit: Option<&str>,
    sid: &str,
    projects: &std::path::Path,
) -> anyhow::Result<std::path::PathBuf> {
    use summon_core::work_dir::WorkDirPlan;

    if let Some(cwd) = session_record_cwd(agent, tmux_session) {
        if std::path::Path::new(&cwd).is_dir() {
            if let Some(path) = verify_cwd_or_sid_search(projects, &cwd, sid)? {
                return Ok(path);
            }
        }
    }
    match crate::runtime::daemon::work_dir_plan(agent, tmux_session, chat_id, explicit) {
        WorkDirPlan::Use { path: cwd, .. } => {
            if let Some(path) = verify_cwd_or_sid_search(projects, &cwd, sid)? {
                return Ok(path);
            }
            return predict_transcript_path(projects, &cwd, sid);
        }
        WorkDirPlan::AllMissing { .. } => {
            return find_transcript_in_projects(projects, sid)?
                .context(summon_i18n::text!("r03861159ef"));
        }
        WorkDirPlan::Unconfigured => {}
    }
    if let Some(found) = find_transcript_in_projects(projects, sid)? {
        return Ok(found);
    }
    let cwd = std::env::current_dir()?;
    predict_transcript_path(projects, &cwd.to_string_lossy(), sid)
}

impl<'a, E: Executor> Transcript<'a, E> {
    pub fn new(sess: &'a Session<'a>, exec: E) -> Self {
        Self { sess, exec }
    }

    fn transcript_path(&self, sid: &str) -> anyhow::Result<std::path::PathBuf> {
        resolve_transcript_path(
            self.sess.agent,
            &self.sess.tmux.session,
            self.sess.chat_id.as_deref(),
            self.sess.work_dir_override(),
            sid,
        )
    }

    /// turn 类 slash 的注入基线:**注入前**冻结的转录文件长度(与 mirror 的 markerless fence
    /// 同源同值,两处必须取同一个数)。文件尚不存在(冷会话零轮)→ 0:注入后 claude 创建的
    /// 整个文件都在基线右侧,归属不受影响。`Err` 只来自定位失败(无 sid / 无 home)——那也是
    /// run_turn 会失败的同一前置,提早在注入前暴露,好让调用方走「零按键,本条没有执行」的
    /// 诚实回执。
    pub(crate) fn preinject_baseline(&self) -> anyhow::Result<u64> {
        let sid = self
            .sess
            .native_session_id()
            .ok_or_else(|| anyhow::anyhow!(summon_i18n::format!("rd6bd9dd47c")))?;
        let path = self.transcript_path(&sid)?;
        // 文件缺失(冷会话零轮)→ 0 是正确基线;文件**在**却读不出长度是另一回事 ——
        // 把它也当 0 会让消费从 0 起把整份历史当本轮重播,必须 fail-closed 上抛
        // (此刻零按键,调用方还能走「本条没有执行」的诚实回执)。
        if !path.exists() {
            return Ok(0);
        }
        self.exec
            .file_len(&path)
            .context(summon_i18n::text!("rd03eb4ab7f"))
    }

    /// 发送一轮并等待 pinned transcript 回显本轮随机 marker。返回值中的 offset 已包含同次读取
    /// 的 trailing 字节；调用方须先消费 `prefetched`，再从 offset 继续 tail，不能重复读。
    /// 只注入、不等待:发一条带本轮随机 marker 的 prompt,返回该 marker。
    /// 轮内 steer 用它——steer **不能**等消费(那要等一个工具边界,期间本轮的输出还得继续解析),
    /// 送达观测交给旁路的 `SteerWatch`。
    fn inject_with_marker(&mut self, prompt: &str) -> anyhow::Result<String> {
        let (wire_prompt, marker) = prompt_with_transport_marker(prompt);
        // steer 是**有意的轮内注入**,故用 PromptQueue(容忍 Running);面板/菜单/用户草稿仍拒。
        self.sess
            .send_line_for_driver(&wire_prompt, InjectGate::PromptQueue)?;
        Ok(marker)
    }

    fn send_correlated_prompt(
        &mut self,
        path: &std::path::Path,
        prompt: &str,
        on_marker_injected: &mut dyn FnMut(&str, u64, &str),
    ) -> anyhow::Result<CorrelatedPrompt> {
        let inject_offset = self.exec.file_len(path).unwrap_or(0);
        let file_identity = self.exec.file_identity(path).ok().flatten();
        let (wire_prompt, marker) = prompt_with_transport_marker(prompt);
        // 必须在第一次可能产生副作用的 send_line 之前登记 marker。多行投喂是「正文 + 两次
        // graced Enter」；后一个 Enter 报错不代表前一拍没提交。若等整个函数 Ok 才登记，
        // partial-send 后的 terminal 会被 mirror 当成 untracked，与失败卡同时发出。
        on_marker_injected(&marker, inject_offset, &prompt_head(prompt));
        // 普通 prompt 走 PromptQueue:claude 支持运行中排队投喂,且这条路径带 transport marker、
        // 有自己的两相送达判据。被闸拒 = 零 send-keys,marker 登记形同一次「发送即失败」——
        // 与既有 partial-send 失败路径同形,mirror 侧 tombstone 逻辑不变。
        self.sess
            .send_line_for_driver(&wire_prompt, InjectGate::PromptQueue)?;
        wait_for_prompt_echo(
            &self.exec,
            PromptEchoWait {
                path,
                relocation: Some(MissingTranscriptSearch::new(
                    &self
                        .sess
                        .native_session_id()
                        .context(summon_i18n::text!("r56fe734386"))?,
                )?),
                prompt,
                marker,
                inject_offset,
                file_identity,
                arrival_timeout: PROMPT_ARRIVAL_TIMEOUT,
                consume_timeout: PROMPT_CONSUME_TIMEOUT,
                poll: POLL,
                resubmit_every: ARRIVAL_RESUBMIT_EVERY,
                resubmit_tries: ARRIVAL_RESUBMIT_TRIES,
            },
            || self.sess.tmux.has_session(),
            || {
                let tmux = &self.sess.tmux;
                let first_line = wire_prompt.lines().next().unwrap_or_default();
                let stuck = tmux.capture_visible().is_ok_and(|pane| {
                    summon_core::pane_input::holds_unsent_prompt(&pane, first_line, || {
                        tmux.capture_styled().ok()
                    })
                });
                // 与注入同理:copy-mode 会把回车当导航吃掉。
                stuck && {
                    tmux.exit_copy_mode();
                    tmux.send_enter().is_ok()
                }
            },
        )
    }

    /// 跑一轮:定位转录 → 取注入前文件长度作基线 → 注入 prompt(`Correlated`;`PreInjected`
    /// 已由调用方过闸注入,直接从基线起;`Reattached` 是上一代注入的轮,零注入、从 marker 的
    /// 消费记录起)→ poll-tail 增量、解析、按 stop_reason 收尾。
    /// `on_stream` 收累积流式文本、`on_tool` 收工具名、`on_message` 收终态回复。
    /// 调用方须已 `ensure_started`(tmux 起好);此处只 send-keys 注入,不冷启。
    /// `on_interactive`(Q1):本轮迟迟无 end_turn 且 pane 识别出交互菜单 / 长时间认不出时回调,
    /// 由上层据此发飞书中介卡片(选项/降级),并回报卡片是否送达 —— 未达则菜单仍在的话由后续
    /// 探测节拍重发(见 `INTERACTIVE_SEND_TRIES`)。非 chat 路径(task/蒸馏/worker)传
    /// `&mut |_| MediationCard::Settled`(不发卡,也就没有可重发的东西)。
    #[allow(clippy::too_many_arguments)] // 各回调职责独立(消息/流式/工具/事件/交互),合并反而晦涩
    pub fn run_turn(
        &mut self,
        prompt: &str,
        injection: TurnInjection,
        on_message: &mut dyn FnMut(&str),
        on_stream: &mut dyn FnMut(&str),
        on_tool: &mut dyn FnMut(&str),
        on_event: &mut dyn FnMut(&AgentMessage),
        on_interactive: &mut dyn FnMut(&InteractiveState) -> MediationCard,
        // mid-turn 偷看钩子(claude/Transcript):每轮 poll 一次,返回 Some 即中断本轮改跑重跑 prompt。
        // 只有 steer 会产出 Some。
        // 非打断路径(flag 关 / task / 蒸馏)传 `&mut || None` —— 恒返回 None,run_turn 行为与原先逐字节等价。
        poll_interrupt: &mut dyn FnMut() -> Option<crate::capture::TurnInterrupt>,
        // 送达回调:**观测到本轮消费记录**那一刻恰好回调一次(见 `send_correlated_prompt`)。
        // chat 路径据它贴 OnIt —— 消息在 claude 队列里干等的那段(实测 6.9s–152.7s)不该贴「在处理」。
        // 非 chat 路径(task/蒸馏/worker)传 `&mut || {}`。
        on_delivered: &mut dyn FnMut(),
        // 注入回调:生成 transport marker 后、任何 send 副作用前立即触发。它只登记关联证据，
        // 不代表发送成功；非 chat 路传 no-op。不能等主 gate 认出消费后才触发。
        // 第二个入参是注入前冻结的转录基线偏移(字节):marker 的消费记录只可能落在它右侧,
        // 上层据此在重启后从这里往后扫,而不必重扫整份历史。第三个入参是**本次真正注入的那条
        // 正文**的 head:`!` rerun 会带着新 prompt 再注入一次,marker 与 head 必须同源落盘,
        // 否则接管时 marker 命中而 head 对不上,一条完好的消息会被判成「被 CLI 截断」。
        on_marker_injected: &mut dyn FnMut(&str, u64, &str),
        // exact marker 消费记录的绝对字节边界。首轮与 `!` rerun 每次 Echoed 都必须
        // 更新；它是 markerless tombstone 不吞更早 attach 的顺序证据，与输出权仲裁解耦。
        on_marker_consumed: &mut dyn FnMut(u64),
        // exact marker 被主 gate 消费后、任何正文回调之前裁决本轮输出归属。返回 false 表示同一
        // marker 的 terminal 已由 mirror 成功送达；此时只以空成功 outcome 收尾，禁止主路径再发。
        // 无 mirror 的调用方恒传 true。
        on_main_claim: &mut dyn FnMut() -> bool,
        // 主 parser 亲眼解析到 Done/ApiError（含 thinking-only 静默确认）时回调。它比 mirror
        // observation 更强：cold 0B bypass 后 watcher 可能从首 terminal 之后的 EOF 起步。
        on_terminal_observed: &mut dyn FnMut(),
        // 主路径对读到的一段(一个 terminal)下了结论:这个 terminal 的字节边界(与 mirror 的
        // terminal_offset 同一套偏移)、这一段的 marker 证据、这一段的回复是否由主路径投递
        // (段终点回复发出去了,或它就是终态)。续跑区间里 mirror 等的就是这个结论。非 chat 路
        // 传 `&mut |_, _, _| {}`。
        on_segment_settled: &mut dyn FnMut(u64, &MirrorMarkerEvidence, bool),
        // 轮内 steer 的送达回调:被 steer 进来的那些飞书消息**真被 claude 消费**时回调一次
        // (可能一次带多条 —— 同一个工具边界会 FIFO 一次性出队)。非 steer 路径传 `&mut |_| {}`。
        on_steer_delivered: &mut dyn FnMut(&[String]),
        // steer 注入成功后立刻回调其 transport marker(修 3)。**不是** `on_marker_injected`:
        // bind 会清当前轮的 consumed/delivery/terminal 边界(那是给 `!` rerun 新区间用的),
        // steer 不是新区间;tracker 只把它挂进本轮旁路名单,供 N:1 认领与孤儿轮放行判定。
        // 非 chat 路径传 `&mut |_| {}`。
        on_steer_marker_injected: &mut dyn FnMut(&str),
        // 心跳:转录静默期间按 `TICK_EVERY` 节流回调,让上层刷新 live 卡上「当前工具已跑多久」。
        // **必要性**:长工具(cargo test 可达十几分钟)期间转录一个字节都不产出、没有任何事件,
        // 上层若只在事件里刷卡,计时会冻在 0s —— 那和卡死在用户眼里没区别,正是本次要解决的体感。
        // 非 chat 路径(task/蒸馏/worker)传 `&mut |_| {}`,行为与本参数引入前逐字等价。
        //
        // 入参是**此刻这段静默的形态**:`Some` = claude 正卡在「等 API 响应,稍后重试」(抓屏认出的,
        // 见 `parse_retry_wait`)。不为它单开一个回调是因为二者是同一件事的两面 —— 心跳问的就是
        // 「现在静默着,该给用户看什么」,而重试等待恰恰是最需要被看见的那种静默。
        on_tick: &mut dyn FnMut(Option<&RetryWait>),
        // 事件实时回调:跨会话消息 / task-notification 行被解析那一刻触发,一条一次。
        // mirror 的 peer 转发要等轮收尾,太晚;主路径靠它在通知到达**那一刻**刷本轮的后台任务卡,
        // 让用户看见「shell 跑完了、模型在继续」。非 chat 路传 `&mut |_| {}`。
        on_notice: &mut dyn FnMut(summon_core::turn_notice::TurnNotice),
        // 续跑决定回调:本轮 end_turn 但仍有后台任务在跑(`has_pending_background`)、主路径决定
        // 不收尾而续跑时触发一次。chat 路据它在 mirror 仲裁器上落续跑区间的左端,区间内主路径
        // 读到的 terminal 都归主路径投递,mirror 让路。非 chat 路传 `&mut || {}`。只在「决定
        // 续跑」时触发,强制收尾(超 cap)不触发。
        on_continuation: &mut dyn FnMut(),
        // 续跑段终点的回复:claude 在这一段 end_turn,但还有后台任务在跑、本轮不收尾。续跑区间里
        // 主路径读到的每一段都由主路径交出去(mirror 等它的结论,见 `on_segment_settled`),
        // 在这里交出去,由上层发成独立回复;不交就会随 `reset_turn` 清掉,用户永远看不到。返回
        // 真正发出去没有。非 chat 路传 `&mut |_| false`。
        on_interim_reply: &mut dyn FnMut(&str) -> bool,
        // 主 Agent 已在 end_turn 交出回复，且转录已读到完整 EOF 时，允许调度器为排队消息结束
        // 本轮持有；后台任务仍由原生会话执行。入参是这一刻还在跑的后台任务，调度器让出前据此
        // 保留它们的归属。返回 true 即让出。非 chat 路传 `&mut |_| false`。
        on_background_idle: &mut dyn FnMut(&super::BackgroundIdle) -> bool,
        // 跨后台任务续跑的时间上限(默认一天,config 可配):还有后台任务没通知时主路径在
        // end_turn 不收尾、续跑到通知到达;但超过它就强制 break,防止后台任务永不完成时主路径
        // 永久挂起。等通知期间转录静默是正常的,stall 看门狗在此期间让位给它。
        continuation_cap: Duration,
    ) -> anyhow::Result<TurnOutcome> {
        let sid = self
            .sess
            .native_session_id()
            .ok_or_else(|| anyhow::anyhow!(summon_i18n::format!("rd6bd9dd47c")))?;
        let mut path = self.transcript_path(&sid)?;

        let mut program_status = super::program_status::Watch::new(self.sess.program_status_path());

        // baseline 只能排除注入前已有历史，不能排除 daemon 重启后旧 turn 在 baseline 之后迟到的
        // end_turn。Correlated 路先发送带随机 marker 的 prompt，并丢弃 marker user 记录之前的
        // 一切字节;PreInjected 路(turn 类 slash)注入已由执行器完成,基线见 `TurnInjection` 文档。
        // 接管轮的重放终点:接管那一刻转录的长度。它左侧的段终点上一代可能已经发过(见
        // `replayed_segment_end`);非接管轮为 `None`。
        let mut replay_end: Option<u64> = None;
        let (first_chunk, mut active_marker, mut checkpoint) = match injection {
            TurnInjection::Correlated => {
                match self.send_correlated_prompt(&path, prompt, on_marker_injected)? {
                    CorrelatedPrompt::Echoed {
                        path: located_path,
                        trailing,
                        marker,
                        consumed_boundary,
                        checkpoint,
                        ..
                    } => {
                        path = located_path;
                        // deadline rescue 可能晚于 mirror 读到并成功发送同一 terminal。必须先做
                        // exact-marker 仲裁，再触发 OnIt / 流式回调；否则同一答案会由 mirror 和
                        // 主路径各发一次。
                        on_marker_consumed(consumed_boundary);
                        if !on_main_claim() {
                            return Ok(TurnOutcome {
                                session_id: Some(sid),
                                ..TurnOutcome::default()
                            });
                        }
                        // 到这里才算「claude 真的开始处理这条消息」——send_correlated_prompt 只在
                        // 观测到消费记录时返回。放在这一行之前的任何位置都会在排队窗口里谎报
                        // 「在处理」。
                        on_delivered();
                        (trailing, Some(marker), checkpoint)
                    }
                    // 显式 `/quit`:会话按请求结束,不是异常。直接以空终态收尾,**不调
                    // on_delivered** —— 它的语义是「claude 开始处理这条消息」,而这条消息恰恰
                    // 没有被处理。
                    CorrelatedPrompt::SessionExited => {
                        log(summon_i18n::text!("rccd110938a"));
                        return Ok(TurnOutcome {
                            session_id: Some(sid),
                            ..TurnOutcome::default()
                        });
                    }
                }
            }
            TurnInjection::PreInjected { baseline } => {
                // 与 Correlated 同序:先仲裁输出权(markerless fence 已由调用方在注入前绑定,
                // claim 在这里把 tracker phase 落成 MainClaimed),再触发用户可见回调。
                if !on_main_claim() {
                    return Ok(TurnOutcome {
                        session_id: Some(sid),
                        ..TurnOutcome::default()
                    });
                }
                // 执行器已正向确认「命令离开输入行」——这就是零 marker 线的送达证据(强度低于
                // 消费记录,回执语义据此收敛为「已提交」,不宣称已开始处理)。
                on_delivered();
                // 转录文件可能尚不存在(冷会话的首轮恰好由这条 slash 触发):有界等它出现,
                // 超时/会话消失都如实报错,不静默空转。
                let arrival_deadline = Instant::now() + PROMPT_ARRIVAL_TIMEOUT;
                while self.exec.file_len(&path).is_err() {
                    anyhow::ensure!(
                        self.sess.tmux.has_session(),
                        summon_i18n::format!("r3f7083f2f2")
                    );
                    anyhow::ensure!(
                        Instant::now() < arrival_deadline,
                        summon_i18n::format!("recda8fd0cb", path.display())
                    );
                    std::thread::sleep(POLL);
                }
                // 基线 checkpoint:文件已比 baseline 短说明注入窗口内被 truncate/替换 ——
                // 没有 marker 可重锚,从 0 起读会把历史当本轮,fail-closed 报错。
                let checkpoint = FileTailCheckpoint::capture_at(&self.exec, &path, baseline)
                    .context(summon_i18n::text!("re67c982c12"))?;
                (Vec::new(), None, checkpoint)
            }
            // 接管上一代 daemon 注入的那一轮:prompt 早就在 claude 手里了,这里一个按键都不能
            // 发 —— 再注入一次就是同一句话跑两遍。
            TurnInjection::Reattached {
                marker,
                prompt_head,
                inject_offset,
                consumed_boundary,
            } => {
                let (trailing, boundary, checkpoint) = locate_reattached_turn(
                    &self.exec,
                    ReattachWait {
                        path: &path,
                        prompt_head: &prompt_head,
                        marker: &marker,
                        inject_offset,
                        consumed_boundary,
                        consume_timeout: PROMPT_CONSUME_TIMEOUT,
                        poll: POLL,
                    },
                    || self.sess.tmux.has_session(),
                    // **只读**判输入态:接管的前提是那一轮还在跑,而注入闸对静止画面会按 Esc
                    // 自愈 —— 长工具 / 等 API 的画面同样静止,那一下就是打断被接管的轮。
                    || {
                        crate::session::input_gate::observe_idle_prompt(
                            &self.sess.tmux,
                            Default::default(),
                        )
                    },
                )?;
                // 与 Correlated::Echoed 同序:先登记消费边界(mirror 据它判自己发没发过这一轮),
                // 再仲裁输出权,最后才是用户可见的送达回调。
                on_marker_consumed(boundary);
                if !on_main_claim() {
                    return Ok(TurnOutcome {
                        session_id: Some(sid),
                        ..TurnOutcome::default()
                    });
                }
                on_delivered();
                replay_end = Some(checkpoint.end());
                (trailing, Some(marker), checkpoint)
            }
        };

        let mut tail = TranscriptTail::default();
        let mut processed_prefix = ProcessedTurnPrefix::default();
        // 轮内 steer 的旁路观测:只读同一份字节,不动 offset/tail。无 steer 时恒空、零成本。
        let mut steer_watch = SteerWatch::default();
        let mut subagents = SubagentTail::new(&path, &sid);
        let mut prefetched = Some(first_chunk);
        let mut last_progress = Instant::now();
        // 存活证据时刻:转录产出字节 **或** pane 画面变化都刷新它。stall 判据看它,不看 last_progress
        // ——后者只驱动抓屏节拍（program_status::probe_due）,若被画面变化一并重置就再也探不到菜单了。
        let mut last_liveness = Instant::now();
        // 上次心跳时刻(计时刷卡节流,见 TICK_EVERY)。
        let mut last_tick = Instant::now();
        // 上一次抓屏的画面签名。变了 = claude 在画东西 = 活着(见 `should_stall`)。
        let mut last_pane_sig: Option<u64> = None;
        // 本轮是否是被 stall 看门狗收的。上层据此把 run 判 Timeout 而不是 Done ——
        // 「我等不到了」和「它干完了」必须可区分,否则 task 会带着空 completion 静默停在 InProgress。

        // 交互态(Q1):interactive_sig=当前菜单已停止发卡的签名(送达,或重发预算用尽);
        // degrade_fired=降级卡已送达;card_budget=未达时的重发预算;
        // wait_deadline=发卡后等用户的硬上限(到点放弃,避免永久阻塞 chat 线程)。
        let mut interactive_sig: Option<u64> = None;
        let mut card_budget = CardSendBudget::default();
        // PF5:上次抓屏探测时刻(探测节拍与 POLL 解耦)。
        let mut last_probe: Option<Instant> = None;
        // 抓屏认出的「等 API 响应,稍后重试」态,随每次探测更新、转录一有新字节即清空。
        // 心跳把它带给上层渲卡 —— 这一态下转录零字节而画面每秒在变,菜单识别和 stall 兜底都够不着。
        let mut retry_wait: Option<RetryWait> = None;
        let mut status_hint_attempts = 0;
        let mut status_hint_signature = None;
        let mut degrade_fired = false;
        let mut wait_deadline: Option<Instant> = None;
        let mut last_acc = String::new(); // 最近一次累积流式全文(打断时给「已打断」提示用)
        let mut interrupted = false; // 任务4:被「⏸ 打断」按钮纯停 → 带 partial 收尾、卡片由 daemon 定格为已暂停
        let mut session_ended = false;
        let mut timed_out = false;
        let mut api_error: Option<String> = None; // Claude API 错误(model_not_found 等),秒级感知
        let mut claude_session_limit = None;
        let mut followup_message_ids = Vec::new();
        // `!` 重跑会替换当前 prompt；会话若在 marker 回显后才退出，判定必须针对正在跑的那轮，
        // 不能继续看最初入参。
        let mut active_prompt = prompt.to_string();
        // 跨后台任务续跑:首个 end_turn 且还有后台任务没通知时记下起点,后续每个 end_turn 检查
        // 是否超过 continuation_cap。None = 本轮还没决定过续跑。
        let mut continuation_start: Option<Instant> = None;
        let mut interim_replies = 0usize;
        let mut main_idle = false;
        let mut yielded_to_queue = false;
        let final_text = loop {
            let chunk = match prefetched.take() {
                Some(chunk) => chunk,
                None => match checkpoint.read_new(&self.exec, &path)? {
                    Some((chunk, next_checkpoint)) => {
                        checkpoint = next_checkpoint;
                        chunk
                    }
                    None => {
                        // markerless 轮(turn 类 slash)没有 exact marker 可重锚:凭旧 offset 或
                        // 从 0 续读都会把历史当本轮,唯一诚实的选择是 fail-closed 收轮报错。
                        let Some(marker) = active_marker.as_deref() else {
                            anyhow::bail!(summon_i18n::format!("rb7cc4044ac"));
                        };
                        log(&summon_i18n::format!("r62cc1fc7d3", path.display()));
                        // 换代重锚只需要认出「哪一轮」:正文完整性在注入阶段已判过,这里再判
                        // 一次也没有 prompt 可比。
                        let echo = TurnEcho { marker, head: "" };
                        let (step, _, next_checkpoint) =
                            reread_prompt_echo(&self.exec, &path, 0, &echo)?;
                        let GateStep::Consumed(rebuilt_trailing) = step else {
                            anyhow::bail!(summon_i18n::format!("r4f9b851fb2"));
                        };
                        let unseen = processed_prefix.unseen_suffix(rebuilt_trailing)?;
                        checkpoint = next_checkpoint;
                        unseen
                    }
                },
            };
            if main_idle {
                let mut unparsed = tail.pending.clone();
                unparsed.extend_from_slice(&chunk);
                let (passive_prefix, resumed) = idle_metadata_prefix(&unparsed);
                main_idle = !resumed;
                // terminal 同一拍读到的 hook 汇总可能已在 pending 中；纯旁路记录可在 EOF
                // 收敛，半行仍须等待完整 JSONL，未知记录须等主轮正常结束。
                if chunk.is_empty() && !resumed {
                    tail.pending.drain(..passive_prefix);
                }
            }
            if main_idle
                && chunk.is_empty()
                && tail.pending.is_empty()
                && steer_watch.pending.is_empty()
                && on_background_idle(&super::BackgroundIdle {
                    session_id: &sid,
                    transcript: &path,
                    tasks: tail.held_background(),
                })
            {
                yielded_to_queue = true;
                log(summon_i18n::text!("r8254b7479f"));
                break String::new();
            }

            // 打断(claude/Transcript):控制卡「⏸ 打断」按钮 → Stop(纯停);steer → Rerun。
            // 真正的打断动作都是发 tmux Esc。flag 全关且无按钮时 poll_interrupt 恒 None,本分支不进、
            // run_turn 与原先逐字节等价(真机待验:claude 被 Esc 后转录续跑/收尾语义)。
            match poll_interrupt() {
                Some(
                    interrupt @ (crate::capture::TurnInterrupt::Stop
                    | crate::capture::TurnInterrupt::SessionEnded),
                ) => {
                    session_ended =
                        matches!(interrupt, crate::capture::TurnInterrupt::SessionEnded);
                    let _ = self.sess.interrupt_turn(); // driver 决定交互式打断键(best-effort)
                    interrupted = true;
                    break last_acc.clone(); // 带已生成 partial 收尾;daemon 据 interrupted 定格「已暂停」
                }
                Some(crate::capture::TurnInterrupt::Rerun(rerun)) => {
                    let crate::capture::RerunRequest {
                        prompt: rerun_prompt,
                        message_ids,
                        cause: _,
                    } = rerun;
                    on_stream(&summon_i18n::format!("rb5219daae6"));
                    let _ = self.sess.interrupt_turn(); // driver 决定交互式打断键(best-effort)
                    std::thread::sleep(POLL); // 给 claude 一拍回到输入态

                    // send 成功才把 ids 带进终态 outcome；send 失败时保持 started WAL，交恢复卡。
                    let (trailing, next_marker, next_consumed_boundary, next_checkpoint) =
                        match self.send_correlated_prompt(
                            &path,
                            &rerun_prompt,
                            on_marker_injected,
                        )? {
                            CorrelatedPrompt::Echoed {
                                path: located_path,
                                trailing,
                                marker,
                                consumed_boundary,
                                checkpoint,
                                ..
                            } => {
                                path = located_path;
                                (trailing, marker, consumed_boundary, checkpoint)
                            }
                            CorrelatedPrompt::SessionExited => {
                                log(summon_i18n::text!("rccd110938a"));
                                followup_message_ids.extend(message_ids);
                                // `! /quit` 发生在旧轮中途：退出本身正常，但旧轮确实被打断；按暂停
                                // 定格已有过程卡，随后 chat 线程仍会发送正常会话退出回执。
                                interrupted = true;
                                break last_acc.clone();
                            }
                        };
                    followup_message_ids.extend(message_ids);
                    active_prompt = rerun_prompt;
                    on_marker_consumed(next_consumed_boundary);
                    // 重置:marker user 记录之前的 Esc/旧 turn 尾巴已丢弃，只把精确重跑轮交给 parser。
                    // markerless 轮被 `!` rerun 后也随之升级成 marker 轮(tracker 侧 bind 会清 fence)。
                    active_marker = Some(next_marker);
                    checkpoint = next_checkpoint;
                    prefetched = Some(trailing);
                    tail = TranscriptTail::default();
                    processed_prefix = ProcessedTurnPrefix::default();
                    last_acc.clear();
                    interactive_sig = None;
                    degrade_fired = false;
                    card_budget.clear();
                    wait_deadline = None;
                    // 续跑相位也复位:tail 已 default(没有后台任务),若 continuation_start
                    // 留着旧时刻,新轮首轮内起后台任务会让 waiting_for_background 误真(Arcadia P2-2)。
                    continuation_start = None;
                    main_idle = false;
                    last_progress = Instant::now();
                    last_liveness = Instant::now();
                    last_pane_sig = None;
                    continue;
                }
                Some(crate::capture::TurnInterrupt::Steer(steer)) => {
                    main_idle = false;
                    // **不发 Esc、不重置 tail、不清 last_acc**:本轮继续跑,消息只是被挂进 claude
                    // 自己的队列,由它在下一个工具边界消费、并入同一个回复(N:1)。本轮已产出的
                    // 内容仍属本轮,任何重置都会把它抹掉。
                    match self.inject_with_marker(&steer.prompt) {
                        Ok(marker) => {
                            // 先登记进 tracker 再进旁路观测:若 claude 在 end_turn 后才消费这条
                            // 消息(自成一轮),tracker 只有认识这个 marker 才不会把那轮答案当
                            // 「本代不认识的历史轮」静默丢弃(修 3)。
                            on_steer_marker_injected(&marker);
                            followup_message_ids.extend(steer.message_ids.iter().cloned());
                            steer_watch.register(marker, steer.message_ids);
                        }
                        // 注入失败不该中断已经在跑的这一轮:正文没进 CLI,WAL 保留 started 交恢复卡。
                        Err(e) => on_stream(&summon_i18n::format!("ra5e2365952")),
                    }
                }
                None => {}
            }
            if !chunk.is_empty() {
                processed_prefix.record(&chunk);
                last_progress = Instant::now();
                // 转录有新字节是最强的存活证据(比画面在动强)。
                last_liveness = Instant::now();
                // 有产出 = 重试已经过去。不清会让卡片继续渲一个早就失效的倒计时,
                // 「立即重试」按钮也会一直挂着 —— 点下去就落到已经恢复的会话上。
                retry_wait = None;
                // 有新字节 = claude 在推进(用户的选择已生效 / 正常输出)→ 退出交互等待态、复位。
                interactive_sig = None;
                degrade_fired = false;
                card_budget.clear();
                wait_deadline = None;
                let step = tail.feed(&chunk, &mut |msg: &AgentMessage| {
                    subagents.note(msg);
                    on_event(msg);
                });
                if matches!(&step, TailStep::Done { .. } | TailStep::ApiError { .. }) {
                    // parser terminal 是最强本轮事实，先通知 tracker；否则主回调 panic 后 guard 会
                    // 留下多余 tombstone，误伤下一条 markerless attach。
                    on_terminal_observed();
                }
                // 事件:到达那一刻交出去(mirror 的转发要等轮收尾,太晚)。
                // 放在 match 之前:无论本拍是 Progress 还是 Done,事件都属于「现在」。
                for notice in tail.drain_notices() {
                    on_notice(notice);
                }
                // 旁路观测只能消费 parser 归入当前轮的前缀。若 chunk 在 A terminal 后还带下一轮
                // queued_command S，整块先喂会把 S 提前 ack，而主函数随即返回、pending 被丢弃。
                let current_prefix =
                    current_turn_input_prefix_len(&step, chunk.len(), tail.pending.len());
                let delivered = steer_watch.feed(&chunk[..current_prefix]);
                if !delivered.is_empty() {
                    on_steer_delivered(&delivered);
                }
                match step {
                    TailStep::Progress { stream, tools } => {
                        // 工具在飞与否由 tail 的 tool_use/tool_result 配对计数说了算(见 awaiting_tool)。
                        for t in tools {
                            on_tool(&t);
                        }
                        if let Some(acc) = stream {
                            on_stream(&acc);
                            last_acc = acc;
                        }
                    }
                    TailStep::Done {
                        stream,
                        tools,
                        final_text,
                    } => {
                        for t in tools {
                            on_tool(&t);
                        }
                        if let Some(acc) = stream {
                            on_stream(&acc);
                        }
                        // 跨后台任务续跑:本轮 end_turn 时仍有后台任务(shell 或子任务)在跑,
                        // 不收尾——reset_turn 后继续 tail,等 task-notification 到达、模型续跑的 end_turn。
                        // 同一张流式卡、同一个角标,续跑输出持续流式显示。上限 continuation_cap(默认一天)。
                        if tail.has_pending_background() {
                            let is_first = continuation_start.is_none();
                            let start = continuation_start.get_or_insert_with(Instant::now);
                            if start.elapsed() >= continuation_cap {
                                log(&summon_i18n::format!(
                                    "r684a50c173",
                                    continuation_cap.as_secs(),
                                    tail.pending_background_count()
                                ));
                                on_segment_settled(
                                    terminal_boundary(&checkpoint, &tail),
                                    &tail.marker_evidence,
                                    true,
                                );
                                break final_text;
                            }
                            // 首次决定续跑才落 claim:claim 值取 active turn 的 consumed_boundary,
                            // 重入时若中途有 `!` rerun 会把它改大,反而放出本该压的早期续跑 terminal。
                            if is_first {
                                on_continuation();
                            }
                            let delivered = !final_text.trim().is_empty()
                                && !replayed_segment_end(
                                    replay_end,
                                    terminal_boundary(&checkpoint, &tail),
                                )
                                && on_interim_reply(&final_text);
                            if delivered {
                                interim_replies += 1;
                            }
                            on_segment_settled(
                                terminal_boundary(&checkpoint, &tail),
                                &tail.marker_evidence,
                                delivered,
                            );
                            log(&summon_i18n::format!(
                                "r6632808a4e",
                                tail.pending_background_count()
                            ));
                            tail.reset_turn();
                            main_idle = true;
                            continue;
                        }
                        on_segment_settled(
                            terminal_boundary(&checkpoint, &tail),
                            &tail.marker_evidence,
                            true,
                        );
                        break final_text;
                    }
                    TailStep::ApiError {
                        stream,
                        tools,
                        final_text,
                        error,
                    } => {
                        for t in tools {
                            on_tool(&t);
                        }
                        if let Some(acc) = stream {
                            on_stream(&acc);
                        }
                        claude_session_limit = tail.session_limit.take();
                        api_error = Some(error);
                        on_segment_settled(
                            terminal_boundary(&checkpoint, &tail),
                            &tail.marker_evidence,
                            true,
                        );
                        break final_text;
                    }
                }
            } else {
                let idle = last_progress.elapsed();
                // 候选终态确认:claude 已经写了 end_turn(只是写在 thinking 那条记录上),静默满 grace
                // 说明同一条消息不会再有文本记录 → 收轮。放在交互探测之前:这一轮已经结束了,不该再
                // 因为「转录静默」去发菜单/降级卡。
                if idle >= END_TURN_GRACE {
                    match tail.confirm_pending_end() {
                        Some(TailStep::ApiError {
                            final_text, error, ..
                        }) => {
                            api_error = Some(error);
                            on_terminal_observed();
                            on_segment_settled(
                                terminal_boundary(&checkpoint, &tail),
                                &tail.marker_evidence,
                                true,
                            );
                            break final_text;
                        }
                        Some(TailStep::Done { final_text, .. }) => {
                            on_terminal_observed();
                            // 同直接 Done:有后台任务在跑就续跑,不收尾(thinking-only 收尾同样适用)。
                            if tail.has_pending_background() {
                                let is_first = continuation_start.is_none();
                                let start = continuation_start.get_or_insert_with(Instant::now);
                                if start.elapsed() >= continuation_cap {
                                    log(&summon_i18n::format!(
                                        "r684a50c173",
                                        continuation_cap.as_secs(),
                                        tail.pending_background_count()
                                    ));
                                    on_segment_settled(
                                        terminal_boundary(&checkpoint, &tail),
                                        &tail.marker_evidence,
                                        true,
                                    );
                                    break final_text;
                                }
                                if is_first {
                                    on_continuation();
                                }
                                let delivered = !final_text.trim().is_empty()
                                    && !replayed_segment_end(
                                        replay_end,
                                        terminal_boundary(&checkpoint, &tail),
                                    )
                                    && on_interim_reply(&final_text);
                                if delivered {
                                    interim_replies += 1;
                                }
                                on_segment_settled(
                                    terminal_boundary(&checkpoint, &tail),
                                    &tail.marker_evidence,
                                    delivered,
                                );
                                log(&summon_i18n::format!(
                                    "rdb4d4cf43e",
                                    tail.pending_background_count()
                                ));
                                tail.reset_turn();
                                main_idle = true;
                                continue;
                            }
                            on_segment_settled(
                                terminal_boundary(&checkpoint, &tail),
                                &tail.marker_evidence,
                                true,
                            );
                            break final_text;
                        }
                        _ => {}
                    }
                }
                // 交互态硬上限:发卡后等用户超时 → 放弃,按累积收尾(上层 Empty→DONE,表情不卡)。
                if wait_deadline.is_some_and(|dl| Instant::now() >= dl) {
                    break tail.accumulated();
                }
                // 「在等后台任务」= 已决定续跑(首个 end_turn 置位 continuation_start)且仍有
                // pending 的后台任务。**首轮内**(还没 end_turn)起的后台任务不阻塞本轮,那期间静默是
                // 真 stall,不能让位给 continuation_cap —— 否则模型在首轮起完后台任务后卡死,
                // 会傻等满一天(240s stall 被 pending 计数误压)。
                let waiting_for_background =
                    continuation_start.is_some() && tail.has_pending_background();
                // PF5:探测有自己的节拍(2s),不跟 300ms POLL 同频 fork-exec 抓屏。
                let blocked = program_status.poll();
                if super::program_status::probe_due(
                    idle,
                    last_probe.map(|t: Instant| t.elapsed()),
                    blocked.is_some(),
                ) {
                    last_probe = Some(Instant::now());
                    // 抓屏失败 + 复验会话确实不在 = 会话已死(用户在会话里敲 `/quit`、CLI 崩溃等)。
                    // **必须两条都成立才收敛**:单看抓屏失败可能只是 tmux 瞬时忙,拿它当「已死」会
                    // 把还在跑的一轮判死。此前这里是 `if let Ok(pane)`,错误被整个吞掉 → 一直空转到
                    // 240s stall 才收尾,期间该 chat 的新消息全部排队(实测 `/quit` 场景)。
                    // 抓屏探测:识别出菜单 → 发选项卡(新菜单才发);长时间认不出 → 发降级卡(原文+文本)。
                    match self.sess.tmux.capture() {
                        // 会话已死(用户敲 `/quit`、CLI 崩溃等)→ 立刻收尾,别空转到 stall。
                        // **判据是「空 pane + 复验不在」而非 Err**:`Tmux::capture` 不看 tmux 退出码,
                        // 对不存在的会话返回的是 `Ok("")`(实测 `capture-pane` exit=1、stdout 空,而
                        // `Command::output()` 本身是成功的)。真正可信的信号是 `has_session`;空 pane
                        // 只是省掉一次 fork-exec 的前置筛(活着的 CLI pane 永远不空)。
                        Ok(pane)
                            if pane_proves_session_gone(&pane, || self.sess.tmux.has_session()) =>
                        {
                            ensure_expected_session_exit(
                                &active_prompt,
                                summon_i18n::text!("r2b4cbe3bc5"),
                            )?;
                            log(summon_i18n::text!("r2a51d821aa"));
                            break tail.accumulated();
                        }
                        Ok(pane) => {
                            // 画面变了 = claude 在画东西 = 活着。菜单是**静止**画面,所以这条不会
                            // 把「卡在菜单」误判成存活:签名一旦稳定下来,存活证据就不再刷新。
                            let sig = pane_signature(&pane);
                            if last_pane_sig != Some(sig) {
                                last_pane_sig = Some(sig);
                                last_liveness = Instant::now();
                            }
                            // 每拍无条件重算(而不是只在没菜单时才看):这一态与菜单互斥,但让它跟着
                            // 每次抓屏走,状态消失时也能立刻归 None,不需要额外的失效逻辑。
                            retry_wait = parse_retry_wait(&pane);
                            if let Some(menu) = parse_interactive(&pane) {
                                let sig = menu.signature();
                                if interactive_sig != Some(sig) {
                                    // AskUserQuestion:转录侧记录了真实选项数,截断抓屏菜单尾部的
                                    // TUI UI 元素(「Type something.」「Chat about this」),只留真实
                                    // 选项——否则用户点到「Type something.」会在 TUI 开自由输入框、
                                    // 飞书侧无法打字,成了陷阱按钮。非 AskUserQuestion(如 /effort
                                    // 确认菜单)无此计数,原样发卡。
                                    let menu = filter_ask_user_question_menu(
                                        menu,
                                        tail.ask_user_question_count(),
                                    );
                                    let delivery = on_interactive(&menu);
                                    let sent = delivery == MediationCard::Settled;
                                    if delivery == MediationCard::Undelivered {
                                        card_budget.note_failure(Some(sig));
                                    }
                                    // 未达且预算未尽 → 不置签名,下个探测节拍(菜单还在的话)重发同一张卡。
                                    // 预算用尽仍未达才置:菜单是个真实的待答问题,attach 的人仍可能当面
                                    // 点掉它,那个等待窗要留着。
                                    if sent || card_budget.spent(Some(sig)) {
                                        interactive_sig = Some(sig);
                                        degrade_fired = false;
                                        wait_deadline = Some(Instant::now() + INTERACTIVE_MAX_WAIT);
                                    }
                                }
                            } else if let Some(title) = blocked {
                                // 自报状态只发只读提示，等待期限与控制性重试仍由真实菜单决定。
                                let hint = InteractiveState {
                                    title: title.to_owned(),
                                    options: Vec::new(),
                                    raw: pane.clone(),
                                };
                                if status_hint_attempts < 3
                                    && status_hint_signature != Some(hint.signature())
                                {
                                    match on_interactive(&hint) {
                                        MediationCard::Settled => {
                                            status_hint_attempts += 1;
                                            status_hint_signature = Some(hint.signature());
                                        }
                                        MediationCard::Undelivered => status_hint_attempts += 1,
                                        MediationCard::Yielded => {}
                                    }
                                }
                            } else if !waiting_for_background
                                && should_stall(idle, last_liveness.elapsed(), tail.awaiting_tool())
                                && !degrade_fired
                                && !card_budget.spent(None)
                            {
                                let degrade = InteractiveState {
                                    title: String::new(),
                                    options: Vec::new(),
                                    raw: pane,
                                };
                                let delivery = on_interactive(&degrade);
                                if delivery == MediationCard::Settled {
                                    degrade_fired = true;
                                    wait_deadline = Some(Instant::now() + INTERACTIVE_MAX_WAIT);
                                } else if delivery == MediationCard::Undelivered {
                                    // 降级卡没送出去就不置等待窗:它本就是「认不出、疑似卡死」的兜底,
                                    // 没有待答问题可等。重发期间 stall 看门狗按 `retrying()` 让位,
                                    // 预算用尽的那一拍它立刻收口,按超时收尾。
                                    card_budget.note_failure(None);
                                }
                            }
                        }
                        // 起 tmux 进程本身失败(罕见,如 fork 失败):瞬时故障,下个节拍再看。
                        Err(_) => {}
                    }
                }
                // 既没识别出菜单、也没进交互等待态,且确无存活证据 → 看门狗:按累积收尾。
                // **续跑例外**:等后台任务期间转录静默是正常的,stall 让位给 continuation_cap
                // (默认一天),到点强制收尾。首轮内的静默不在此列(见 waiting_for_background 注释)。
                // **重发例外**:有卡正未达重发时也让位。降级卡的发送门就是 `should_stall`,两处
                // 入参完全相同 —— 不让位,看门狗会在同一次迭代里当场收轮,重发次数形同虚设。
                if waiting_for_background {
                    if continuation_start.is_some_and(|t| t.elapsed() >= continuation_cap) {
                        log(&summon_i18n::format!(
                            "r684a50c173",
                            continuation_cap.as_secs(),
                            tail.pending_background_count()
                        ));
                        break tail.accumulated();
                    }
                } else if wait_deadline.is_none()
                    && !card_budget.retrying()
                    && should_stall(idle, last_liveness.elapsed(), tail.awaiting_tool())
                {
                    timed_out = true;
                    log(&summon_i18n::format!(
                        "r66c4376ede",
                        idle.as_secs(),
                        last_liveness.elapsed().as_secs(),
                        if tail.awaiting_tool() {
                            summon_i18n::text!("r0a98c0d424")
                        } else {
                            ""
                        }
                    ));
                    break tail.accumulated();
                }
            }
            // 子 agent 转录每拍都读,与主转录有无新字节无关:同步子 agent 跑着时主转录恰恰是静默的。
            // 它写出字节只在主 agent 有任一工具在飞时算本轮活着:主 agent 在等工具返回(同步
            // `Agent`、阻塞的 `TaskOutput` 都是这种),这段时间子 agent 的进展算这一轮的进展;工具
            // 在飞本来就走放宽的静默窗口。主 agent 没有工具在飞时(异步派出后自己在想、或已写完却
            // 没落 `end_turn`),后台子 agent 写多少都与本轮无关,拿它续命会把一个卡住的主轮一路
            // 拖到硬顶。
            let subagent_bytes = subagents.poll(&self.exec, Instant::now(), on_event);
            if subagent_bytes && tail.awaiting_tool() {
                last_liveness = Instant::now();
            }
            // 计时心跳:静默期按 TICK_EVERY 回调一次,让 live 卡上的「已跑 Ns」继续走。
            if last_tick.elapsed() >= TICK_EVERY {
                last_tick = Instant::now();
                on_tick(retry_wait.as_ref());
            }
            std::thread::sleep(POLL);
        };

        let trimmed = final_text.trim();
        // 被按钮纯停:partial 不是「终态回复」,不回调 on_message(卡片由 daemon 定格为「已暂停」);
        // 正常收尾才落终态回复。
        if !interrupted && !timed_out && !trimmed.is_empty() {
            // result 控制协议按原文完全匹配，空白只用于判空。
            on_message(&final_text);
        }
        Ok(TurnOutcome {
            session_id: Some(sid),
            reply: (!interrupted && !trimmed.is_empty()).then(|| final_text.clone()),
            followup_message_ids,
            interrupted,
            session_ended,
            timed_out,
            api_error,
            claude_session_limit,
            api_error_kind: None,
            subagent_tail: Some(Box::new(subagents)),
            interim_replies,
            yielded_to_queue,
        })
    }

    /// 会话镜像最小原型(claude/Transcript,`messaging.session_mirror` **默认开**):**持续** tail
    /// 转录,每个完成的 assistant turn 回调 `on_turn(evidence, boundary, text)` —— 无论这轮由飞书
    /// 注入触发,还是用户 attach 进 tmux 直敲触发(RPC→pub-sub)。evidence 是本 terminal 前的
    /// 有界 Enqueued/Consumed transport 候选与未知 shape 歧义证据；终端直敲通常为空。daemon 按
    /// active exact marker 选关联，避免
    /// 上轮残留 marker 覆盖本轮。boundary 是 terminal 行后的绝对字节 offset。
    ///
    /// **重启续读**(`offset_path`):传 `Some(path)` 时只接受 v3 中与当前飞书 epoch、
    /// sid、boundary 前 1KiB exact 指纹同时匹配的 offset。fresh EOF 若最后一条完整 JSONL 已被
    /// parser 语义证明为 `end_turn`，直接落 TerminalBoundary；只有 EOF 落在未完成轮中、无法证明
    /// 边界时，首个 terminal 才做历史收敛：推进 offset 但绝不联网。
    /// 停机期间的回答因无 durable delivery fact 无法证明未送达，不自动补发；用户需发“继续”。
    /// 旧格式/错代际从稳定 EOF 起。每个成功处理的 terminal 原子写新 boundary + 指纹；
    /// 传 `None` 时从当前 EOF 起、不持久化。
    ///
    /// **offset 必须与 sid、boundary 指纹同存同验**:文件按 chat tag 键控,而 offset 只对某一个
    /// 具体 transcript generation 有意义。`/bridge` 把一个 2.5MB 的外部历史会话召回绑到旧 chat
    /// 时,遗留 offset(103857)恰好小于新 EOF,整段 2.4MB 历史就被当成停机 backlog 补投进群。
    /// sid/指纹不符即丢弃、从稳定 EOF 起。
    ///
    /// slice 2 由 `daemon::run_mirror`(detached watcher)调用。
    #[allow(clippy::too_many_arguments)] // 参数各自独立(epoch/stop/三个回调/offset 路径/补发下界),收成 struct 只是换个地方列
    pub(crate) fn mirror_tail(
        &self,
        expected_epoch_fingerprint: &str,
        stop: &std::sync::atomic::AtomicBool,
        on_turn: &mut MirrorTurnSink<'_>,
        on_ready: &mut dyn FnMut(),
        on_start_bypassed: &mut dyn FnMut(),
        offset_path: Option<&std::path::Path>,
        // 停机窗口补发下界(见 `tail_forward_turns` 的同名参数)。None = 不补,只观察。
        catchup_floor: Option<u64>,
    ) -> anyhow::Result<()> {
        let sid = self
            .sess
            .native_session_id()
            .ok_or_else(|| anyhow::anyhow!(summon_i18n::format!("rdc672a399f")))?;
        let mut path = self.transcript_path(&sid)?;
        let mut search = MissingTranscriptSearch::new(&sid)?;

        // 确定起始 offset:有同一 epoch + sid + generation 的持久化 offset 才续读历史字节，
        // 否则从 EOF 起。续读只为了收敛 boundary，不构成“停机回答未送达”的证明。
        // mirror 是主动群出站路径，任何 metadata/权限瞬错都必须 fail-closed；把错误猜成 EOF=0
        // 会在恢复后从头把整份历史灌进群。
        let saved = offset_path
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| parse_mirror_offset_record(&s))
            .filter(|record| {
                record.sid == sid && record.epoch_fingerprint == expected_epoch_fingerprint
            });
        let Some((start, eof, checkpoint, used_saved)) = wait_for_stable_mirror_start(
            &self.exec,
            &mut path,
            Some(&mut search),
            saved.as_ref(),
            stop,
            MIRROR_EMPTY_START_TIMEOUT,
            on_start_bypassed,
        )?
        else {
            return Ok(());
        };
        let proven_terminal_eof = !used_saved
            && start == eof
            && transcript_eof_is_terminal_boundary(&self.exec, &path, eof)?;
        let ambiguous_baseline =
            mirror_start_is_ambiguous(used_saved, saved.as_ref()) && !proven_terminal_eof;
        if start < eof {
            log(&summon_i18n::format!("rca8adee723"));
        }
        // offset 持久化回调:只有可见块全部送达后才写 exact boundary + 从解析流派生的指纹。
        // 不能发送后重读 path 生成指纹：网络 I/O 期间文件可能已换代，那会把旧 turn 的投递事实
        // 错记到新 generation。写失败也 fail-stop，避免后续成功跨过未提交的 turn。
        let persist_record = |off: u64,
                              boundary_checkpoint: &FileTailCheckpoint,
                              boundary_kind: MirrorOffsetBoundaryKind|
         -> anyhow::Result<()> {
            if let Some(p) = offset_path {
                let encoded = format_mirror_offset(
                    expected_epoch_fingerprint,
                    &sid,
                    off,
                    boundary_checkpoint,
                    boundary_kind,
                )?;
                atomic_write(p, encoded.as_bytes())?;
            }
            Ok(())
        };

        // fresh/cold watcher 若恰好从稳定 EOF 起步，本轮可能很久都没有 terminal；Ready 前先把这枚
        // exact 非空 checkpoint 落成 v3。EOF 本身已是 end_turn 时可标 Terminal；否则只能标
        // Ambiguous，保守观察跨边界的首个 terminal。这样升级不会机械吞每个存量 chat 的首条新回复。
        // 有 saved backlog(start < eof)时绝不能提前跨过未送达字节。
        if !used_saved && start == eof && start > 0 {
            persist_record(
                start,
                &checkpoint,
                if proven_terminal_eof {
                    MirrorOffsetBoundaryKind::TerminalBoundary
                } else {
                    MirrorOffsetBoundaryKind::AmbiguousBaseline
                },
            )?;
        }

        let mut persist_terminal = |off: u64, boundary_checkpoint: &FileTailCheckpoint| {
            persist_record(
                off,
                boundary_checkpoint,
                MirrorOffsetBoundaryKind::TerminalBoundary,
            )
        };

        tail_forward_turns(
            &self.exec,
            &path,
            start,
            checkpoint,
            POLL,
            END_TURN_GRACE,
            stop,
            self.sess.mirror_watermark(),
            ambiguous_baseline,
            catchup_floor,
            on_turn,
            &mut persist_terminal,
            on_ready,
        )?;
        Ok(())
    }
}

/// 持续 tail 一个转录文件,**每个 assistant `end_turn`** 回调 `on_turn`(完成的回复文本)。抽出
/// path/exec 便于单测(喂临时文件 + LocalExecutor)。
///
/// **C2:一次 `read_file_tail` 读到的字节里若含 ≥2 个 end_turn,逐个消费全部转发**(重启补停机
/// backlog 时整段一次性到达,是本功能主场景,而非罕见边界)。靠 `feed` 遇终态把余量留在 `pending`
/// + `reset_turn` 保留 pending,内层循环 drain 到无更多终态为止。
///
/// `on_turn` 的 Err 表示可见块未成功送达：tailer 立即 fail-stop，且不得提交本轮或后续 offset。
/// 继续消费会让后续成功轮的 boundary 跨过失败轮，重启时永久跳过它。
///
/// `on_turn_offset`:可选回调,每次成功转发 turn 后传入**精确到 turn 行边界**的绝对 offset
/// (`已读 offset - pending.len()`,恒落在某行 `\n` 后)。用于持久化消费位置,供 daemon 重启后从
/// 上次位置续读——精确到行边界既不丢同 chunk 内后续 turn,也不会落在半行中间。
#[allow(clippy::too_many_arguments)] // tail 循环的参数:exec/path/offset/poll/grace/stop/watermark/双回调
/// 停机窗口内最多补发多少个 turn。停机通常只有几秒,窗口里撑死一两轮;设上限是为了让「判据万一
/// 松了」的后果有界。被丢弃的数量必须 log 出来 —— 静默截断会读成「已经补全了」。
const MIRROR_CATCHUP_MAX_TURNS: usize = 5;

#[allow(clippy::too_many_arguments)] // 同 mirror_tail:tail 的入参是一串正交的边界条件,聚合反而更难读
fn tail_forward_turns<E: Executor>(
    exec: &E,
    path: &std::path::Path,
    start_offset: u64,
    mut checkpoint: FileTailCheckpoint,
    poll: Duration,
    // 候选终态确认窗(生产传 `END_TURN_GRACE`;测试传小值免真等)。
    grace: Duration,
    stop: &std::sync::atomic::AtomicBool,
    // 冷启 seed 的真实连续前缀 cutoff。normal run_turn 绝不能写它：prompt gate 会跨过仍未投递的
    // 前序 attach，标量高水位一旦跨洞就会永久吞回复。None = 没有 seed 前缀。
    skip_until: Option<&std::sync::atomic::AtomicU64>,
    // saved 起点若是任意 EOF baseline，它可能切在旧 turn 中间；即使 Ready 时尚无 backlog，
    // 随后才落下的首个 terminal 也必须按历史观察，直到提交 terminal boundary 才解除。
    restored_ambiguous_baseline: bool,
    // 停机窗口补发的下界(上一代 stop fence 记的 `stop_eof`)。**终点**落在它右侧的轮,收尾
    // 字节落盘于上一代死后 —— 上一代不可能发过它;完整资格判据(含代际)见 `assess_catchup`。
    // `None` = 本 chat 未获批补发,历史一律只观察(逐字节等同 2026-08-13 之前的行为)。
    catchup_floor: Option<u64>,
    on_turn: &mut MirrorTurnSink<'_>,
    on_turn_offset: &mut MirrorOffsetSink<'_>,
    on_ready: &mut dyn FnMut(),
) -> anyhow::Result<()> {
    use std::sync::atomic::Ordering;
    // 补发配额。停机窗口通常只装得下一两轮;设上限是让「判据万一松了」的后果有界,
    // 用尽后剩下的历史退回只观察,并在退回那一刻 log 一次(静默截断会读成「补全了」)。
    let mut catchup_budget = catchup_floor.map(|_| MIRROR_CATCHUP_MAX_TURNS).unwrap_or(0);
    let mut catchup_dropped = 0usize;
    let mut offset = start_offset; // 下一个要读的绝对字节位置(FILE 字节)
                                   // 当前尚未收口 turn 的真实起点。不能在 thinking-only grace 时用 terminal boundary 反推：
                                   // 那会令 `turn_start == watermark`，把主路径已消费的轮误判成未消费并重复转发。
    let mut turn_start = start_offset;
    // ready 前先完成一次 exact checkpoint 复核并冻结当时 backlog。否则唤醒主线程之后、首次
    // read_new 之前仍有 replace 窗口，watcher 宣称 ready 却没有真正持有可消费快照。
    let first_before = checkpoint.clone();
    let Some((first_chunk, first_after)) = checkpoint.read_new(exec, path)? else {
        anyhow::bail!(summon_i18n::format!("rd85274f2a1"))
    };
    let initial_eof = first_after.end();
    let mut prefetched = Some((first_before, first_chunk, first_after));
    on_ready();
    let mut tail = TranscriptTail::default();
    let mut pending_end_checkpoint: Option<FileTailCheckpoint> = None;
    let mut ambiguous_first_terminal = restored_ambiguous_baseline;
    let mut noop = |_: &AgentMessage| {};
    // PF4:空闲指数退避——会话空闲数月也以 300ms 开文件太奢侈;无新字节逐步放宽到 5s,
    // 一有新字节立刻收紧回 poll(镜像时效仍是秒级)。
    let max_sleep = Duration::from_secs(5);
    let mut sleep_for = poll;
    let mut last_bytes = Instant::now();
    // 一轮收口后的转发:seed cutoff 之下只推进状态机,不重复推飞书。
    let forward = |final_text: &str,
                   notice_block: Option<String>,
                   turn_start: u64,
                   boundary: u64,
                   evidence: &MirrorMarkerEvidence,
                   boundary_checkpoint: &FileTailCheckpoint,
                   observation_only: Option<MirrorTurnKind>,
                   catchup: bool,
                   on_turn: &mut MirrorTurnSink<'_>,
                   on_turn_offset: &mut MirrorOffsetSink<'_>|
     -> anyhow::Result<()> {
        let visible = summon_core::feed_tag::strip_feed_tag(final_text);
        let t = visible.trim();
        if let Some(kind) = observation_only {
            on_turn(kind, evidence, boundary, &boundary_checkpoint.bytes, t)?;
            on_turn_offset(boundary, boundary_checkpoint)?;
            return Ok(());
        }
        // 只有 cold seed 是可由标量表达的连续前缀；normal turn 的归属全部交给 typed marker 仲裁。
        let consumed = skip_until.is_some_and(|wm| turn_start < wm.load(Ordering::Relaxed));
        if !consumed {
            // 通知卡先于回复文本:先看到「这一轮里发生了什么」,再看到这一轮的回应。
            if let Some(block) = &notice_block {
                on_turn(
                    MirrorTurnKind::Notice,
                    evidence,
                    boundary,
                    &boundary_checkpoint.bytes,
                    block,
                )?;
            }
        }
        if consumed || t.is_empty() {
            // 即使正文为空也必须通知 exact terminal 已观察；否则 main finish 留下的 markerless
            // tombstone 会误伤下一条真正的 markerless attach。
            on_turn(
                MirrorTurnKind::ReplyObserved,
                evidence,
                boundary,
                &boundary_checkpoint.bytes,
                t,
            )?;
        } else {
            // 停机窗口产物走 CatchupReply:同样联网发,但允许 tracker 放行「上一代 bind 的 marker」。
            on_turn(
                if catchup {
                    MirrorTurnKind::CatchupReply
                } else {
                    MirrorTurnKind::Reply
                },
                evidence,
                boundary,
                &boundary_checkpoint.bytes,
                t,
            )?;
        }
        // 每个 terminal 都提交连续 boundary：正文/peer 为空是“无需联网”的成功，不应在重启后
        // 反复解析。watermark 覆盖轮也必须落盘，否则 watermark 归零后会把历史重发回飞书。
        on_turn_offset(boundary, boundary_checkpoint)?;
        Ok(())
    };
    // 停机窗口补发的资格判定(修 2,两处终态收口共用,单一事实源)。两条判据缺一不可:
    // - `boundary > floor`:终点落盘于上一代 stop fence 之后 —— 上一代死前不可能发过它;
    // - `turn_start < initial_eof`:**代际按轮的起点判**。起点在 boot 冻结 EOF 左侧,证明这一轮
    //   由上一代或停机窗口开启,本代主路径从未拥有过它 —— 它收尾后没有任何进程会投递
    //   (CLAUDE.md「没有任何进程会投递的那一轮」)。跨停机仍在跑、boot 之后才收尾的轮
    //   (boundary > initial_eof)正是这一类,所以**不能**用 `boundary <= initial_eof` 之类的
    //   终点上界截断停机窗口 —— 那会把 spec 指定必补的轮永久漏掉。
    // 防「live 轮蚕食配额」(实测 boot+2h53m 才耗尽 5 轮)靠的是代际判据本身:本代新生轮的
    // 起点必然 >= initial_eof(watcher 先冻结起点,主路径才允许注入,见 spec 第三节),
    // 永不占配额;spec 第五节「boot 只预热 watcher,不声称补发」由此成立。
    let mut assess_catchup = |turn_start: u64, boundary: u64| -> bool {
        let eligible =
            catchup_floor.is_some_and(|floor| boundary > floor && turn_start < initial_eof);
        if !eligible {
            return false;
        }
        if catchup_budget == 0 {
            catchup_dropped += 1;
            if catchup_dropped == 1 {
                log(&summon_i18n::format!("r04215eb5ad", path.display()));
            }
            return false;
        }
        catchup_budget -= 1;
        true
    };
    while !stop.load(Ordering::Relaxed) {
        let (previous_checkpoint, chunk, next_checkpoint) = match prefetched.take() {
            Some(prefetched) => prefetched,
            None => {
                let previous = checkpoint.clone();
                let Some((chunk, next)) = checkpoint.read_new(exec, path)? else {
                    // 没有上游 generation-complete 信号：truncate 后任意一次 EOF 都可能只是分段 rewrite
                    // 的中间态。此时若重建空/部分 checkpoint，剩余历史会被当成新 append 主动灌群。
                    // mirror 是主动出站，唯一安全裁决是退出并标 Failed；当前及后续 turn 只走主路径。
                    anyhow::bail!(summon_i18n::format!("rdf27cd0e66"));
                };
                (previous, chunk, next)
            }
        };
        checkpoint = next_checkpoint;
        if chunk.is_empty() {
            // 候选终态确认(纯 thinking 收尾的轮):静默满 grace → 这一轮真的结束了,按累积转发。
            // 不确认的话,这轮的正文会被下一轮吞掉(下一轮的终态文本才是它的回复)。
            if last_bytes.elapsed() >= grace {
                if let Some(step) = tail.confirm_pending_end() {
                    let boundary = offset.saturating_sub(tail.pending.len() as u64);
                    let final_text = match &step {
                        TailStep::Done { final_text, .. }
                        | TailStep::ApiError { final_text, .. } => final_text.clone(),
                        TailStep::Progress { .. } => String::new(),
                    };
                    let notice_block = tail.take_notice_block();
                    // 停机窗口产物例外:资格判据集中在 `assess_catchup`(代际按轮的起点判,修 2)。
                    let catchup_ok = assess_catchup(turn_start, boundary);
                    let historical =
                        (turn_start < initial_eof && !catchup_ok) || ambiguous_first_terminal;
                    let ambiguous = tail.marker_evidence.overflowed()
                        || !tail.marker_evidence.uncertain().is_empty();
                    let observation_only = historical
                        .then_some(MirrorTurnKind::HistoricalObserved)
                        .or_else(|| ambiguous.then_some(MirrorTurnKind::AmbiguousObserved));
                    let evidence = tail.marker_evidence.clone();
                    let boundary_checkpoint = pending_end_checkpoint
                        .take()
                        .ok_or_else(|| anyhow::anyhow!(summon_i18n::format!("rd64448dea6")))?;
                    anyhow::ensure!(
                        boundary_checkpoint.end() == boundary,
                        summon_i18n::format!("r42626f2d8a")
                    );
                    ambiguous_first_terminal = false;
                    forward(
                        &final_text,
                        notice_block,
                        turn_start,
                        boundary,
                        &evidence,
                        &boundary_checkpoint,
                        observation_only,
                        catchup_ok,
                        on_turn,
                        on_turn_offset,
                    )?;
                    turn_start = boundary;
                    tail.reset_turn();
                }
            }
            std::thread::sleep(sleep_for);
            sleep_for = (sleep_for * 2).min(max_sleep);
            continue;
        }
        sleep_for = poll;
        last_bytes = Instant::now();
        let read_window = ObservedReadWindow {
            previous: &previous_checkpoint,
            start: offset,
            bytes: &chunk,
        };
        offset += chunk.len() as u64;
        // 一个 chunk 内可能含 ≥2 个 end_turn:循环逐个消费转发,直到本 chunk 无更多终态(剩余半行 /
        // 下一轮开头留在 pending 等下次读)。首次喂整块原始字节,之后喂空 slice 只 drain pending。
        let mut input: &[u8] = &chunk;
        loop {
            let step = tail.feed(input, &mut noop);
            input = b""; // 后续迭代只 drain pending,不重复喂已消费字节
            match step {
                // 镜像转发也认 API 错误为终态:Claude 报错后以 stop_sequence 结束(无 end_turn),
                // 不等 stall,直接把错误文本当 turn 转发(和 run_turn 路径行为一致)。
                TailStep::Done { final_text, .. } | TailStep::ApiError { final_text, .. } => {
                    // 精确行边界 = 已读 offset 减去未消费的 pending(end_turn 行右侧 `\n` 后)。
                    let boundary = offset.saturating_sub(tail.pending.len() as u64);
                    let boundary_checkpoint = if boundary >= read_window.start {
                        read_window.checkpoint_at(boundary)?
                    } else {
                        // thinking terminal 已在上一 read 完整出现，而下一 user 行横跨 read 时，
                        // 新 user 只负责确认旧候选；真实 terminal boundary 在当前窗口左侧。
                        let checkpoint = pending_end_checkpoint
                            .take()
                            .ok_or_else(|| anyhow::anyhow!(summon_i18n::format!("r0d6b444f2d")))?;
                        anyhow::ensure!(
                            checkpoint.end() == boundary,
                            summon_i18n::format!("r115cc7e083")
                        );
                        checkpoint
                    };
                    let notice_block = tail.take_notice_block();
                    // 停机窗口产物例外:资格判据集中在 `assess_catchup`(代际按轮的起点判,修 2)。
                    let catchup_ok = assess_catchup(turn_start, boundary);
                    let historical =
                        (turn_start < initial_eof && !catchup_ok) || ambiguous_first_terminal;
                    let ambiguous = tail.marker_evidence.overflowed()
                        || !tail.marker_evidence.uncertain().is_empty();
                    let observation_only = historical
                        .then_some(MirrorTurnKind::HistoricalObserved)
                        .or_else(|| ambiguous.then_some(MirrorTurnKind::AmbiguousObserved));
                    let evidence = tail.marker_evidence.clone();
                    ambiguous_first_terminal = false;
                    forward(
                        &final_text,
                        notice_block,
                        turn_start,
                        boundary,
                        &evidence,
                        &boundary_checkpoint,
                        observation_only,
                        catchup_ok,
                        on_turn,
                        on_turn_offset,
                    )?;
                    pending_end_checkpoint = None;
                    turn_start = boundary;
                    tail.reset_turn(); // 只清本轮累积,保留 pending(可能含下一轮开头)
                }
                TailStep::Progress { .. } => {
                    if tail.pending_end {
                        let boundary = offset.saturating_sub(tail.pending.len() as u64);
                        if boundary >= read_window.start {
                            pending_end_checkpoint = Some(read_window.checkpoint_at(boundary)?);
                        } else {
                            anyhow::ensure!(
                                pending_end_checkpoint
                                    .as_ref()
                                    .is_some_and(|checkpoint| checkpoint.end() == boundary),
                                summon_i18n::format!("r295a5c2905")
                            );
                        }
                    } else {
                        pending_end_checkpoint = None;
                    }
                    break; // 本 chunk 无更多终态,等下次读
                }
            }
        }
    }
    Ok(())
}

/// 跟踪一份外部会话的转录,每轮收尾(assistant `end_turn`,或 API 报错收尾)回调一次。
///
/// - 正文非空的轮先 `on_reply(boundary, 正文)`;API 报错收尾的轮同样按回复转发,正文是这一轮
///   累积的文字(通常就是报错)。
/// - 不转发的轮(正文为空、超出补发上限、marker 证据不确定)只记日志。
/// - 每轮最后都 `on_boundary(boundary)`,转发与否都一样。boundary 是该轮收尾记录之后的字节
///   位置,从它续读不会再报这一轮。
///
/// 从 `start_offset` 开始读,之前的轮一条不回调;之后结束的轮都回调,包括开读时已经落盘的。
/// 任一回调返回 Err 即停止跟踪并返回该错误,这一轮的 `on_boundary` 不再回调。
///
/// `poll` 是无新字节时的起始轮询间隔(空闲时照常指数放宽)。`on_ready` 在首读之后回调一次:
/// 此后落盘的字节一定不在首读里。两者只影响何时读、不影响报哪些轮,测试据此不必按生产节拍真等。
pub(crate) fn tail_external_replies(
    path: &std::path::Path,
    start_offset: u64,
    poll: Duration,
    stop: &std::sync::atomic::AtomicBool,
    on_ready: &mut dyn FnMut(),
    on_reply: &mut dyn FnMut(u64, &str) -> anyhow::Result<()>,
    on_boundary: &mut dyn FnMut(u64) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    let exec = crate::runtime::executor::LocalExecutor;
    let checkpoint = FileTailCheckpoint::capture_at(&exec, path, start_offset)?;
    let mut on_turn =
        |kind: MirrorTurnKind, _: &MirrorMarkerEvidence, boundary: u64, _: &[u8], text: &str| {
            match kind {
                MirrorTurnKind::Reply | MirrorTurnKind::CatchupReply if !text.trim().is_empty() => {
                    on_reply(boundary, text)
                }
                // 通知块是同一轮里先报的一块,这一轮自己的回调随后就到;遥控不转发通知。
                MirrorTurnKind::Notice => Ok(()),
                _ => {
                    log(&summon_i18n::format!("r844aa3f64d", path.display()));
                    Ok(())
                }
            }
        };
    // 补发下界取起点。`tail_forward_turns` 把起于首读 EOF 之前的轮当历史只观察,而起点与首读
    // 之间常有新字节:写下 offset 之后、开读之前落下的,daemon 停机期间结束的,重启时正在跑的
    // 那一轮。它们都结束在起点之后、是该发的回复,经这个下界按 CatchupReply 报出;
    // 轮数上限沿用 `MIRROR_CATCHUP_MAX_TURNS`,超出的按 HistoricalObserved 跳过。
    //
    // `on_boundary` 接在 `on_turn_offset` 上:`tail_forward_turns` 对每一轮(含只观察的)都先调
    // `on_turn`、成功后才调 `on_turn_offset`,所以回复入队一定先于 offset 越过这一轮;跳过的轮
    // 也推进 offset,再重启时不会重新取得补发资格。
    tail_forward_turns(
        &exec,
        path,
        start_offset,
        checkpoint,
        poll,
        END_TURN_GRACE,
        stop,
        None,
        false,
        Some(start_offset),
        &mut on_turn,
        &mut |boundary, _| on_boundary(boundary),
        on_ready,
    )
}

/// 冷启 seed 等待(claude/Transcript,根因修):从 `start_offset` tail 转录文件,直到**首个**
/// assistant `end_turn` 收尾即返回 Ok ——取代 `session.rs::cold_start` 里脆弱的抓屏看门狗。后者用
/// `StallWatch` 看 raw pane 变化,慢工具(日历 API 等)阻塞期 raw pane 静默 >300s 就被误判卡死、
/// 整轮冷启 Err →「起会话失败」。转录里 end_turn 是确定性收尾、tool_use 是真进度,不靠抓屏猜稳定。
/// **idle 本身不算失败**(bootstrap 的慢工具可跑很久):只在 ① `alive()` 转 false(tmux 会话已退,
/// 真失败)或 ② 到 `deadline`(总时长封顶兜底)才 Err。比旧看门狗严格更宽容,不可能让误判更糟。
/// 纯 tail + 注入式判活,便于单测(喂临时文件 + 假 alive)。
/// seed 等待结局。`Ended`=转录出现 end_turn(bootstrap 正常收尾);`TimedOut`=到总时长封顶仍未见
/// end_turn。**TimedOut 不是致命错误**——会话仍活着(bootstrap 慢或转录定位未命中),调用方据此降级
/// 为「会话已起」继续注入用户消息,而非丢弃。会话**真退出**仍走 `Err`。
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum SeedWait {
    Ended,
    TimedOut,
}

pub(crate) fn wait_first_turn_end<E: Executor>(
    exec: &E,
    path: &std::path::Path,
    start_offset: u64,
    poll: Duration,
    // 候选终态确认窗(生产传 `END_TURN_GRACE`;测试传小值免真等)。
    grace: Duration,
    deadline: Instant,
    alive: &mut dyn FnMut() -> bool,
) -> anyhow::Result<SeedWait> {
    let mut offset = start_offset;
    let mut tail = TranscriptTail::default();
    let mut noop = |_: &AgentMessage| {};
    let mut last_bytes = Instant::now();
    loop {
        let chunk = exec.read_file_tail(path, offset).unwrap_or_default();
        if !chunk.is_empty() {
            offset += chunk.len() as u64;
            last_bytes = Instant::now();
            match tail.feed(&chunk, &mut noop) {
                TailStep::Done { .. } => return Ok(SeedWait::Ended),
                // seed 轮就遇到 API 错误(model_not_found 等):不等满 deadline,立即报错让上层处理。
                TailStep::ApiError { error, .. } => {
                    anyhow::bail!(summon_i18n::format!("rffc41cc9f2"));
                }
                TailStep::Progress { .. } => {}
            }
            // 有新字节 = 在推进:继续立即读把 backlog 抽干,读空后才进 idle 检查。
            continue;
        }
        // 候选终态确认(seed 轮以纯 thinking 收尾):静默满 grace 即认这一轮结束,别傻等满封顶。
        if last_bytes.elapsed() >= grace {
            match tail.confirm_pending_end() {
                Some(TailStep::ApiError { error, .. }) => {
                    anyhow::bail!(summon_i18n::format!("rffc41cc9f2"));
                }
                Some(_) => return Ok(SeedWait::Ended),
                None => {}
            }
        }
        // 无新字节:idle 不算失败,只有进程已退 / 总时长封顶才放弃。
        if !alive() {
            anyhow::bail!(summon_i18n::format!("rc9b1d44cab"));
        }
        // 超时**不算失败**:会话还活着,只是没在封顶时限内检测到 end_turn(bootstrap 慢,或转录定位
        // 未命中——真机:blue-wing /summon 1min 就收尾却傻等 600s)。返回 TimedOut 让冷启降级、不丢消息。
        if Instant::now() >= deadline {
            return Ok(SeedWait::TimedOut);
        }
        std::thread::sleep(poll);
    }
}

#[cfg(test)]
mod tests {
    /// 只认轮、不校验正文完整性的判据 —— 完整性有专门用例覆盖,别的用例不该被它牵动。
    fn echo(marker: &str) -> TurnEcho<'_> {
        TurnEcho { marker, head: "" }
    }

    fn agent_call(id: &str) -> AgentMessage {
        let mut msg = AgentMessage::of(AgentKind::ToolUse);
        msg.tool = Some("Agent".to_string());
        msg.tool_id = Some(id.to_string());
        msg
    }

    fn subagent_fixture() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("sid-1.jsonl");
        std::fs::write(&main, "").unwrap();
        std::fs::create_dir_all(dir.path().join("sid-1").join("subagents")).unwrap();
        (dir, main)
    }

    fn write_subagent(main: &std::path::Path, agent: &str, tool_use: &str, lines: &str) {
        let dir = main.parent().unwrap().join("sid-1").join("subagents");
        std::fs::write(
            dir.join(format!("agent-{agent}.meta.json")),
            format!(r#"{{"agentType":"Explore","description":"找入口","toolUseId":"{tool_use}"}}"#),
        )
        .unwrap();
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join(format!("agent-{agent}.jsonl")))
            .unwrap();
        std::io::Write::write_all(&mut file, lines.as_bytes()).unwrap();
    }

    const SUB_TOOL_USE: &str = r#"{"type":"assistant","isSidechain":true,"agentId":"a1","message":{"content":[{"type":"tool_use","id":"toolu_sub","name":"Bash","input":{"command":"echo hi"}}]}}"#;
    const SUB_END: &str = r#"{"type":"assistant","isSidechain":true,"agentId":"a1","message":{"stop_reason":"end_turn","content":[{"type":"text","text":"入口在 main.rs"}]}}"#;

    #[test]
    #[ignore = "process-wide tmux fixture; run with --ignored --test-threads=1"]
    fn program_status_capture_path_prompts_before_silence_threshold() {
        use std::cell::Cell;
        struct Empty;
        impl Executor for Empty {
            fn spawn(
                &self,
                _: &crate::runtime::executor::SpawnSpec,
            ) -> anyhow::Result<std::process::Child> {
                unreachable!()
            }
            fn read_file_tail(&self, _: &std::path::Path, _: u64) -> anyhow::Result<Vec<u8>> {
                Ok(vec![])
            }
            fn tmux(&self, _: &[&str]) -> anyhow::Result<String> {
                unreachable!()
            }
        }
        let (agent, dir) = crate::test_support::agent_dir("claude", "");
        let fake = dir.path().join("tmux");
        crate::test_support::write_test_executable(
            &fake,
            "#!/bin/sh\nprintf 'Unrecognized waiting screen\\n'\n",
        );
        let mut config = summon_core::config::Config::default();
        config.tools.tmux = Some(fake);
        summon_core::tools::configure(&config);
        struct Reset;
        impl Drop for Reset {
            fn drop(&mut self) {
                summon_core::tools::configure(&summon_core::config::Config::default());
            }
        }
        let _reset = Reset;
        let session =
            crate::session::session::Session::with_name(&agent, "status-test".into(), 100, 100);
        crate::session::resume_store::write(
            &agent,
            "status-test",
            "claude",
            &uuid::Uuid::new_v4().to_string(),
            Some("/tmp"),
            None,
        );
        let status = session.program_status_path();
        std::fs::create_dir_all(status.parent().unwrap()).unwrap();
        let mut observed = vec![];
        for enabled in [false, true] {
            let write = |seq, state| {
                atomic_write(&status,serde_json::json!({"generation":"test","sequence":seq,"report":{"state":state,"app":"claude-code","id":null,"kind":"question"}}).to_string().as_bytes()).unwrap()
            };
            write(0, "idle");
            let started = Instant::now();
            let first = Cell::new(None);
            let mut capture = Transcript::new(&session, Empty);
            let result = capture
                .run_turn(
                    "probe",
                    TurnInjection::PreInjected { baseline: 0 },
                    &mut |_| {},
                    &mut |_| {},
                    &mut |_| {},
                    &mut |_| {},
                    &mut |hint| {
                        assert!(hint.options.is_empty());
                        assert_eq!(hint.title, "Claude 正在等待回答");
                        first.set(Some(started.elapsed()));
                        MediationCard::Settled
                    },
                    &mut || {
                        if first.get().is_some() || started.elapsed() > Duration::from_secs(5) {
                            Some(crate::capture::TurnInterrupt::Stop)
                        } else {
                            None
                        }
                    },
                    &mut || {
                        if enabled {
                            write(1, "blocked");
                        }
                    },
                    &mut |_, _, _| {},
                    &mut |_| {},
                    &mut || true,
                    &mut || {},
                    &mut |_, _, _| {},
                    &mut |_| {},
                    &mut |_| {},
                    &mut |_| {},
                    &mut |_| {},
                    &mut || {},
                    &mut |_| false,
                    &mut |_| false,
                    Duration::from_secs(60),
                )
                .unwrap();
            assert!(result.interrupted);
            assert!(result.reply.is_none());
            observed.push(first.get());
        }
        assert_eq!(observed[0], None, "静默五秒内，未知菜单缺少状态时没有提示");
        assert!(
            observed[1].is_some_and(|t| t < Duration::from_secs(2)),
            "新鲜 blocked 应提前驱动实际 run_turn 抓屏/提示: {observed:?}"
        );
        eprintln!("program_status production capture comparison: {observed:?}");
    }

    /// 让位(`Yielded`)的一拍不记签名、不扣预算:那条通道(Claude 授权 hook)没答成就结束时,
    /// 屏上还在的同一张菜单下一拍照常认定、发卡。
    #[test]
    #[ignore = "process-wide tmux fixture; run with --ignored --test-threads=1"]
    fn a_yielded_menu_is_offered_again_on_the_next_probe() {
        use std::cell::Cell;
        struct Empty;
        impl Executor for Empty {
            fn spawn(
                &self,
                _: &crate::runtime::executor::SpawnSpec,
            ) -> anyhow::Result<std::process::Child> {
                unreachable!()
            }
            fn read_file_tail(&self, _: &std::path::Path, _: u64) -> anyhow::Result<Vec<u8>> {
                Ok(vec![])
            }
            fn tmux(&self, _: &[&str]) -> anyhow::Result<String> {
                unreachable!()
            }
        }
        let (agent, dir) = crate::test_support::agent_dir("claude", "");
        let fake = dir.path().join("tmux");
        crate::test_support::write_test_executable(&fake, "#!/bin/sh\nprintf '──────────────────────────────\\n Bash command\\n\\n   rm -f a\\n\\n Do you want to proceed?\\n ❯ 1. Yes\\n   2. No\\n\\n Esc to cancel\\n'\n");
        let mut config = summon_core::config::Config::default();
        config.tools.tmux = Some(fake);
        summon_core::tools::configure(&config);
        struct Reset;
        impl Drop for Reset {
            fn drop(&mut self) {
                summon_core::tools::configure(&summon_core::config::Config::default());
            }
        }
        let _reset = Reset;
        let session =
            crate::session::session::Session::with_name(&agent, "yield-test".into(), 100, 100);
        crate::session::resume_store::write(
            &agent,
            "yield-test",
            "claude",
            &uuid::Uuid::new_v4().to_string(),
            Some("/tmp"),
            None,
        );
        let status = session.program_status_path();
        std::fs::create_dir_all(status.parent().unwrap()).unwrap();
        let seq = Cell::new(0u64);
        let blocked = || {
            seq.set(seq.get() + 1);
            atomic_write(&status, serde_json::json!({"generation":"test","sequence":seq.get(),"report":{"state":"blocked","app":"claude-code","id":null,"kind":"question"}}).to_string().as_bytes()).unwrap()
        };
        blocked();
        let offers = Cell::new(0u32);
        let signatures = std::cell::RefCell::new(Vec::new());
        let started = Instant::now();
        let mut capture = Transcript::new(&session, Empty);
        capture
            .run_turn(
                "probe",
                TurnInjection::PreInjected { baseline: 0 },
                &mut |_| {},
                &mut |_| {},
                &mut |_| {},
                &mut |_| {},
                &mut |menu| {
                    assert_eq!(menu.options.len(), 2, "认出的是那张授权菜单");
                    offers.set(offers.get() + 1);
                    signatures.borrow_mut().push(menu.signature());
                    if offers.get() <= 2 {
                        MediationCard::Yielded
                    } else {
                        MediationCard::Settled
                    }
                },
                &mut || {
                    if offers.get() >= 3 || started.elapsed() > Duration::from_secs(20) {
                        Some(crate::capture::TurnInterrupt::Stop)
                    } else {
                        None
                    }
                },
                &mut || blocked(),
                &mut |_, _, _| {},
                &mut |_| {},
                &mut || true,
                &mut || {},
                &mut |_, _, _| {},
                &mut |_| {},
                &mut |_| {},
                &mut |_| {},
                &mut |_| {},
                &mut || {},
                &mut |_| false,
                &mut |_| false,
                Duration::from_secs(60),
            )
            .unwrap();
        assert_eq!(offers.get(), 3, "让位两拍后同一张菜单第三拍照常发");
        let signatures = signatures.into_inner();
        assert!(
            signatures.windows(2).all(|w| w[0] == w[1]),
            "始终是同一张菜单"
        );
    }

    #[test]
    fn subagent_tail_tags_events_of_subagents_spawned_this_turn() {
        let (_dir, main) = subagent_fixture();
        let mut tail = SubagentTail::new(&main, "sid-1");
        tail.note(&agent_call("toolu_parent"));
        write_subagent(&main, "a1", "toolu_parent", &format!("{SUB_TOOL_USE}\n"));
        write_subagent(
            &main,
            "old",
            "toolu_earlier_turn",
            &format!("{SUB_TOOL_USE}\n"),
        );

        let mut seen = Vec::new();
        assert!(tail.poll(
            &crate::runtime::executor::LocalExecutor,
            Instant::now(),
            &mut |msg| { seen.push(msg.clone()) }
        ));
        assert_eq!(seen.len(), 1, "只 tail 本轮派出的子 agent:{seen:?}");
        assert_eq!(seen[0].tool.as_deref(), Some("Bash"));
        assert_eq!(
            seen[0].parent_run_id.as_ref().map(|id| id.0.as_str()),
            Some("toolu_parent")
        );
    }

    #[test]
    fn subagent_tail_waits_for_complete_lines_and_keeps_its_offset() {
        let (_dir, main) = subagent_fixture();
        let mut tail = SubagentTail::new(&main, "sid-1");
        tail.note(&agent_call("toolu_parent"));
        let (head, rest) = SUB_TOOL_USE.split_at(40);
        write_subagent(&main, "a1", "toolu_parent", head);

        let exec = crate::runtime::executor::LocalExecutor;
        let mut seen = Vec::new();
        tail.poll(&exec, Instant::now(), &mut |msg| seen.push(msg.clone()));
        assert!(seen.is_empty(), "半行不解析");

        write_subagent(&main, "a1", "toolu_parent", &format!("{rest}\n{SUB_END}\n"));
        tail.poll(&exec, Instant::now(), &mut |msg| seen.push(msg.clone()));
        assert_eq!(seen.len(), 2, "{seen:?}");
        assert_eq!(seen[1].stop_reason.as_deref(), Some("end_turn"));

        assert!(!tail.poll(&exec, Instant::now(), &mut |msg| seen.push(msg.clone())));
        assert_eq!(seen.len(), 2, "读过的行不重放");
    }

    /// 数 `.meta.json` 读了几次的执行器,其余直通本机。
    struct MetaReadCounter {
        inner: crate::runtime::executor::LocalExecutor,
        meta_reads: std::cell::Cell<usize>,
    }

    impl Executor for MetaReadCounter {
        fn spawn(
            &self,
            spec: &crate::runtime::executor::SpawnSpec,
        ) -> anyhow::Result<std::process::Child> {
            self.inner.spawn(spec)
        }
        fn read_file_tail(&self, path: &std::path::Path, from: u64) -> anyhow::Result<Vec<u8>> {
            if path.to_string_lossy().ends_with(".meta.json") {
                self.meta_reads.set(self.meta_reads.get() + 1);
            }
            self.inner.read_file_tail(path, from)
        }
        fn list_dir(&self, path: &std::path::Path) -> anyhow::Result<Vec<String>> {
            self.inner.list_dir(path)
        }
        fn tmux(&self, args: &[&str]) -> anyhow::Result<String> {
            self.inner.tmux(args)
        }
    }

    /// 读过的 meta 不重读:目录里攒着之前轮次的子 agent,每秒一轮全读一遍是白花的 IO。meta 先于
    /// 主转录里的派出调用被看到时,记住的归属照样能在之后对上。
    #[test]
    fn subagent_tail_reads_each_meta_file_once() {
        let (_dir, main) = subagent_fixture();
        write_subagent(
            &main,
            "old",
            "toolu_earlier_turn",
            &format!("{SUB_TOOL_USE}\n"),
        );
        write_subagent(&main, "b1", "toolu_second", &format!("{SUB_TOOL_USE}\n"));
        let exec = MetaReadCounter {
            inner: crate::runtime::executor::LocalExecutor,
            meta_reads: std::cell::Cell::new(0),
        };
        let mut tail = SubagentTail::new(&main, "sid-1");
        tail.note(&agent_call("toolu_first"));
        let mut seen = Vec::new();
        tail.poll(&exec, Instant::now(), &mut |msg| seen.push(msg.clone()));
        assert_eq!(exec.meta_reads.get(), 2);
        assert!(seen.is_empty());

        tail.last_scan = None;
        tail.poll(&exec, Instant::now(), &mut |msg| seen.push(msg.clone()));
        assert_eq!(exec.meta_reads.get(), 2, "没有新 meta 就不读");

        tail.note(&agent_call("toolu_second"));
        write_subagent(&main, "a1", "toolu_first", &format!("{SUB_TOOL_USE}\n"));
        tail.last_scan = None;
        tail.poll(&exec, Instant::now(), &mut |msg| seen.push(msg.clone()));
        assert_eq!(exec.meta_reads.get(), 3, "只读新出现的那一份");
        let parents: Vec<_> = seen
            .iter()
            .map(|msg| msg.parent_run_id.as_ref().unwrap().0.as_str())
            .collect();
        assert_eq!(parents.len(), 2, "{parents:?}");
        assert!(parents.contains(&"toolu_first") && parents.contains(&"toolu_second"));
    }

    /// 列目录的节拍按调用方给的时刻算:主轮收尾后接着读的跟随线程用自己的时钟。对上的文件
    /// 连同 agentId 交得出去 —— 状态源里结束的子 agent 只剩 agentId。
    #[test]
    fn subagent_tail_scans_on_the_given_clock_and_reports_agent_ids() {
        let (_dir, main) = subagent_fixture();
        let exec = crate::runtime::executor::LocalExecutor;
        let mut tail = SubagentTail::new(&main, "sid-1");
        tail.note(&agent_call("toolu_parent"));
        let start = Instant::now();
        let mut seen = Vec::new();
        assert!(!tail.poll(&exec, start, &mut |msg| seen.push(msg.clone())));

        write_subagent(&main, "a1", "toolu_parent", &format!("{SUB_TOOL_USE}\n"));
        tail.poll(&exec, start + Duration::from_millis(500), &mut |msg| {
            seen.push(msg.clone())
        });
        assert!(seen.is_empty(), "不到一个节拍不重列目录");
        assert_eq!(tail.agent_ids().count(), 0);

        assert!(
            tail.poll(&exec, start + SubagentTail::SCAN_EVERY, &mut |msg| {
                seen.push(msg.clone())
            })
        );
        assert_eq!(seen.len(), 1);
        assert_eq!(
            tail.agent_ids().collect::<Vec<_>>(),
            [("toolu_parent", "a1")]
        );
    }

    #[test]
    fn subagent_tail_is_idle_without_spawns() {
        let (_dir, main) = subagent_fixture();
        write_subagent(&main, "a1", "toolu_parent", &format!("{SUB_TOOL_USE}\n"));
        let mut tail = SubagentTail::new(&main, "sid-1");
        let mut seen = Vec::new();
        assert!(!tail.poll(
            &crate::runtime::executor::LocalExecutor,
            Instant::now(),
            &mut |msg| { seen.push(msg.clone()) }
        ));
        assert!(seen.is_empty());
    }

    /// 子 agent 的行只经旁路交给 `on_event`:主循环里 `TranscriptTail` 只喂主转录的字节。
    #[test]
    fn subagent_lines_never_reach_the_main_turn_tail() {
        let production = include_str!("transcript.rs");
        let body = &production[production.find("pub fn run_turn(").expect("run_turn")..];
        let body = &body[..body
            .find("let trimmed = final_text.trim();")
            .expect("主循环结尾")];
        assert_eq!(
            body.matches("tail.feed(").count(),
            1,
            "主循环只有一处喂主 tail"
        );
        assert!(body.contains("subagents.poll(&self.exec, Instant::now(), on_event)"));
        assert!(
            body.contains("subagents.note(msg)"),
            "主转录的事件要先登记派出的子 agent"
        );
        assert!(!body.contains("subagents.poll(&self.exec, Instant::now(), &mut |"));
    }

    /// 子转录的字节只在主 agent 有工具在飞(同步 `Agent`、阻塞的 `TaskOutput`)时算本轮的存活
    /// 证据;主 agent 没在等时,后台子 agent 写多少都不该给主轮续命。读子转录本身每拍照做 ——
    /// 条件放在读之前,子 agent 的行就推不出去了。
    #[test]
    fn subagent_bytes_are_liveness_only_while_the_parent_awaits_a_tool() {
        let production = include_str!("transcript.rs");
        let body = &production[production.find("pub fn run_turn(").expect("run_turn")..];
        let body: String = body[..body
            .find("let trimmed = final_text.trim();")
            .expect("主循环结尾")]
            .split_whitespace()
            .collect();
        let poll = "letsubagent_bytes=subagents.poll(&self.exec,Instant::now(),on_event);";
        let refresh = "ifsubagent_bytes&&tail.awaiting_tool(){last_liveness=Instant::now();}";
        assert_eq!(body.matches(poll).count(), 1, "每拍无条件读一次子转录");
        let at = body.find(poll).unwrap();
        assert!(
            body[at..].starts_with(&format!("{poll}{refresh}")),
            "读完紧跟着按「主 agent 有工具在飞」决定要不要刷新存活时刻"
        );
        assert_eq!(
            body.matches("subagents.poll(").count(),
            1,
            "只有这一处读子转录"
        );
    }

    use super::*;

    /// shell 直通是单行的:marker 换行会被 claude 的 bash 模式当成第二条命令提交。
    #[test]
    fn a_shell_passthrough_carries_its_marker_as_a_trailing_comment_on_one_line() {
        let (wire, marker) = prompt_with_transport_marker("! gh auth login");

        assert!(!wire.contains('\n'), "shell 直通必须单行,实得:{wire:?}");
        assert!(wire.starts_with("! gh"), "`!` 必须仍是首字符:{wire:?}");
        assert!(
            wire.ends_with(&format!("# {marker}")),
            "marker 应挂在行尾注释里:{wire:?}"
        );
    }

    /// 普通消息照旧走换行 + HTML 注释,shell 直通的单行形态不能外溢到它。
    #[test]
    fn an_ordinary_prompt_keeps_the_block_comment_marker() {
        let (wire, marker) = prompt_with_transport_marker("在群里有人问了一句话");

        assert!(
            wire.contains(&format!("\n\n<!-- {marker} -->")),
            "实得:{wire:?}"
        );
    }

    /// claude 把 `!` 吃掉、换成 `<bash-input>` 标签才落进转录,所以完整性探针必须从 `!`
    /// 之后取 —— 否则每一条 shell 直通都会被判成「注入被掐掉了开头」。
    #[test]
    fn the_integrity_probe_matches_the_bash_input_echo_claude_actually_writes() {
        let (wire, marker) = prompt_with_transport_marker("! gh auth login");
        let head = prompt_head(&wire);
        assert!(!head.contains('!'), "探针不该带上 `!`:{head:?}");

        // claude 真机回显形态(2026-09-02 实测):`!` 被替换成标签,其余逐字节保留。
        let echoed = format!("<bash-input>{}</bash-input>", &wire[1..]);
        let judged = TurnEcho {
            marker: &marker,
            head: &head,
        }
        .judge(&echoed);

        assert_eq!(judged, Some(true), "真机回显应判为本轮且收全:{echoed:?}");
    }

    const FIXTURE: &str =
        include_str!("../../../summon-core/tests/fixtures/claude-transcript-turn.jsonl");
    const NORESP: &str =
        include_str!("../../../summon-core/tests/fixtures/claude-transcript-noresp.jsonl");

    fn production_source() -> &'static str {
        include_str!("transcript.rs")
            .split_once("\n#[cfg(test)]\nmod tests {")
            .map(|(production, _)| production)
            .expect("transcript 顶层 tests module")
    }

    /// 回归(2026-08-28):marker 落在 prompt 末尾,所以「只验 marker」对丢头部的注入是瞎的 ——
    /// 真实故障里 claude 只收到消息的后半段,marker 完好,判据一路放行,agent 读到半条消息而
    /// 没有任何人报错。带上正文开头之后,同一条记录必须判成截断并让等待循环失败。
    #[test]
    fn a_prompt_that_lost_its_head_is_a_failure_not_a_delivery() {
        const M: &str = "summon-transport-turn:head-loss";
        let prompt = "张三 @你:「先看这份文档的背景说明,再决定要不要改」";
        let full = format!(
            r#"{{"type":"user","message":{{"role":"user","content":"{prompt}\n\n<!-- {M} -->"}}}}"#
        );
        // 掐掉开头(真实故障的形态:切点落在字符中间,尾部连同 marker 一并保留)。
        let beheaded = format!(
            r#"{{"type":"user","message":{{"role":"user","content":"要不要改」\n\n<!-- {M} -->"}}}}"#
        );
        let intact = TurnEcho {
            marker: M,
            head: &prompt_head(prompt),
        };
        assert_eq!(
            classify_marker_record(full.as_bytes(), &intact),
            MarkerRecord::Consumed
        );
        assert_eq!(
            classify_marker_record(beheaded.as_bytes(), &intact),
            MarkerRecord::Truncated,
            "只剩尾部的记录必须判截断"
        );
        let err = PromptEchoGate::default()
            .feed(format!("{beheaded}\n").as_bytes(), &intact)
            .expect_err("截断必须让等待循环失败,而不是当成一次成功投递");
        assert!(
            err.to_string().contains("截断"),
            "错误里要说清是截断: {err}"
        );
    }

    /// 分块正文(content 是 text block 数组)按整条消息判完整性:开头在第一块、marker 在最后
    /// 一块是常态,逐块判会把收全的消息误判成截断。
    #[test]
    fn a_block_split_prompt_is_judged_as_one_message() {
        const M: &str = "summon-transport-turn:blocks";
        let prompt = "张三 @你:「分块投递的正文也要判成完整」";
        let line = format!(
            r#"{{"type":"user","message":{{"role":"user","content":[{{"type":"text","text":"{prompt}"}},{{"type":"text","text":"<!-- {M} -->"}}]}}}}"#
        );
        assert_eq!(
            classify_marker_record(
                line.as_bytes(),
                &TurnEcho {
                    marker: M,
                    head: &prompt_head(prompt)
                }
            ),
            MarkerRecord::Consumed
        );
    }

    #[test]
    fn prompt_echo_gate_discards_old_end_turn_before_exact_marker() {
        let marker = "summon-transport-turn:new-random-id";
        let chunk = format!(
            "{}\n{{\"type\":\"user\",\"sessionId\":\"s\",\"message\":{{\"role\":\"user\",\"content\":\"新任务\\n<!-- {marker} -->\"}}}}\n{}\n",
            r#"{"type":"assistant","sessionId":"s","message":{"role":"assistant","stop_reason":"end_turn","content":[{"type":"text","text":"旧轮完成"}]}}"#,
            r#"{"type":"assistant","sessionId":"s","message":{"role":"assistant","stop_reason":"end_turn","content":[{"type":"text","text":"新轮完成"}]}}"#,
        );
        let mut gate = PromptEchoGate::default();
        let GateStep::Consumed(trailing) = gate.feed(chunk.as_bytes(), &echo(marker)).unwrap()
        else {
            panic!("exact user marker 应打开 gate");
        };
        let trailing = String::from_utf8(trailing).unwrap();
        assert!(!trailing.contains("旧轮完成"), "旧 end_turn 必须被丢弃");
        assert!(trailing.contains("新轮完成"));

        let mut tail = TranscriptTail::default();
        match tail.feed(&trailing, &mut |_| {}) {
            TailStep::Done { final_text, .. } => assert_eq!(final_text, "新轮完成"),
            other => panic!("marker 后的新 end_turn 才能收轮，实际 {other:?}"),
        }
    }

    #[test]
    fn prompt_echo_gate_handles_split_user_record_and_rejects_historical_marker() {
        let marker = "summon-transport-turn:fresh";
        let mut gate = PromptEchoGate::default();
        let historical =
            r#"{"type":"user","message":{"role":"user","content":"summon-transport-turn:old"}}"#;
        assert!(matches!(gate
            .feed(format!("{historical}\n{{\"type\":\"user\",\"message\":{{\"role\":\"user\",\"content\":\"<!-- summon-trans").as_bytes(), &echo(marker))
            .unwrap(), GateStep::Waiting));
        let GateStep::Consumed(trailing) = gate
            .feed(
                format!(
                    "port-turn:fresh -->\"}}}}\n{}",
                    r#"{"type":"assistant","message":{"role":"assistant","stop_reason":"end_turn","content":[{"type":"text","text":"ok"}]}}"#
                )
                .as_bytes(),
                &echo(marker),
            )
            .unwrap()
        else {
            panic!("跨 chunk 的 exact marker 也必须命中");
        };
        assert!(String::from_utf8(trailing).unwrap().contains("\"ok\""));
    }

    #[test]
    fn missing_prompt_marker_times_out_fail_closed() {
        let mut gate = PromptEchoGate::default();
        let old = br#"{"type":"assistant","message":{"role":"assistant","stop_reason":"end_turn","content":[{"type":"text","text":"old"}]}}
"#;
        assert!(matches!(
            gate.feed(old, &echo("never-seen")).unwrap(),
            GateStep::Waiting
        ));
        let deadline = Instant::now();
        let error = ensure_prompt_echo_not_timed_out(deadline, deadline, &probe_diag())
            .expect_err("marker 永未出现时不得把旧 end_turn 当作成功");
        assert!(error.to_string().contains("marker"));
    }

    // ——— steer 识别(步骤1):marker 会以七种形态落盘,只有两种代表「已消费」———
    // 下面的记录形状全部照抄本机真实转录(16 份带 summon marker 的 jsonl,68 个 marker),
    // 不是臆造的样例。

    const M: &str = "summon-transport-turn:cc33";

    /// 形态 A(实测 62 条):无工具边界可插时,消息落成普通 user 记录 —— **必须命中**。
    #[test]
    fn plain_user_record_counts_as_consumed() {
        let line = format!(
            r#"{{"parentUuid":"p","isSidechain":false,"promptId":"x","type":"user","message":{{"role":"user","content":"插播:确认收到\n\n<!-- {M} -->"}}}}"#
        );
        assert_eq!(
            classify_marker_record(line.as_bytes(), &echo(M)),
            MarkerRecord::Consumed
        );
        // content 也可能是 text block 数组,同样必须命中。
        let blocks = format!(
            r#"{{"type":"user","message":{{"role":"user","content":[{{"type":"text","text":"<!-- {M} -->"}}]}}}}"#
        );
        assert_eq!(
            classify_marker_record(blocks.as_bytes(), &echo(M)),
            MarkerRecord::Consumed
        );
    }

    /// 形态 B(实测 4 条):轮内 steer,消息在工具边界出队时落成 queued_command 附件 —— **必须命中**。
    /// 只认这一种会在纯文本轮永远超时,只认 user 会在 steer 轮永远超时,两种事前不可预知。
    #[test]
    fn queued_command_attachment_counts_as_consumed() {
        let line = format!(
            r#"{{"parentUuid":"p","isSidechain":false,"attachment":{{"type":"queued_command","prompt":"插播:确认收到\n\n<!-- {M} -->","commandMode":"prompt","origin":{{"kind":"human"}},"timestamp":"2026-08-04T01:32:00.900Z"}},"type":"attachment","uuid":"u"}}"#
        );
        assert_eq!(
            classify_marker_record(line.as_bytes(), &echo(M)),
            MarkerRecord::Consumed
        );
    }

    /// 排除 1(实测 enqueue/remove 各 4 条,**都带 marker 全文**):queue-operation 只证明消息到了
    /// claude 手里,不证明被消费。remove 甚至落在真正的消费记录之前,且用户按 Esc 撤掉排队消息同样
    /// 产生 remove。两个方向都不得算消费。
    #[test]
    fn queue_operation_is_enqueue_never_consumption() {
        for op in ["enqueue", "remove"] {
            let line = format!(
                r#"{{"type":"queue-operation","operation":"{op}","timestamp":"2026-08-04T01:32:00.900Z","sessionId":"s","content":"插播\n\n<!-- {M} -->"}}"#
            );
            assert_eq!(
                classify_marker_record(line.as_bytes(), &echo(M)),
                MarkerRecord::Enqueued,
                "queue-operation/{op} 只能算入队"
            );
        }
    }

    #[test]
    fn explicit_quit_accepts_session_exit_as_normal() {
        for prompt in ["/quit", "  /quit\n", "/exit"] {
            ensure_expected_session_exit(prompt, "等待本轮 prompt echo 时持久会话意外退出")
                .unwrap_or_else(|error| panic!("{prompt:?} 应视为主动退出，实际 {error:#}"));
        }
    }

    #[test]
    fn non_quit_session_exit_remains_an_error() {
        for prompt in ["继续处理", "/quit now", "请执行 /quit"] {
            let error =
                ensure_expected_session_exit(prompt, "等待本轮 prompt echo 时持久会话意外退出")
                    .expect_err("非精确退出命令不得吞掉会话消失");
            assert!(
                error.to_string().contains("意外退出"),
                "异常退出须保留明确错误，实际 {error:#}"
            );
        }
    }

    /// 排除 2(实测 64 条,是所有形态里最多的):last-prompt 是「最后一次输入」的快照,带 marker
    /// 全文却与消费与否无关。
    #[test]
    fn last_prompt_snapshot_never_counts_as_consumption() {
        let line = format!(
            r#"{{"type":"last-prompt","lastPrompt":"用户 @你:「x」 <!-- {M} -->…","leafUuid":"l","sessionId":"s"}}"#
        );
        assert_eq!(
            classify_marker_record(line.as_bytes(), &echo(M)),
            MarkerRecord::Unrelated
        );
    }

    /// 另两种真实的误命中源:agent 自己在回复里引用了 marker(实测 5 条);被编辑文件的 snippet
    /// 里恰好有 marker(实测 1 条,同为 attachment 但子类型不同)。宽松 substring 会把它们当消费。
    #[test]
    fn assistant_quote_and_other_attachments_never_count_as_consumption() {
        let quoted = format!(
            r#"{{"type":"assistant","message":{{"role":"assistant","stop_reason":"end_turn","content":[{{"type":"text","text":"判据是 <!-- {M} --> 这个 marker"}}]}}}}"#
        );
        assert_eq!(
            classify_marker_record(quoted.as_bytes(), &echo(M)),
            MarkerRecord::Unrelated
        );
        let edited = format!(
            r#"{{"attachment":{{"type":"edited_text_file","filename":"/tmp/FINDINGS.md","snippet":"1\tmarker <!-- {M} --> 出现在文件里"}},"type":"attachment","uuid":"u"}}"#
        );
        assert_eq!(
            classify_marker_record(edited.as_bytes(), &echo(M)),
            MarkerRecord::Unrelated,
            "attachment 必须精确到 queued_command + prompt 字段"
        );
    }

    /// 端到端序列:实测每条被 steer 的消息恒为 enqueue → remove → queued_command。gate 在前两条
    /// 上**必须不开**(此时消息还躺在队列里,实测最长 152.7s 才被消费),只在第三条上开。
    #[test]
    fn gate_stays_shut_through_the_whole_queue_window() {
        let mut gate = PromptEchoGate::default();
        let enqueue = format!(
            r#"{{"type":"queue-operation","operation":"enqueue","content":"<!-- {M} -->"}}"#
        );
        let remove = format!(
            r#"{{"type":"queue-operation","operation":"remove","content":"<!-- {M} -->"}}"#
        );
        let last = format!(r#"{{"type":"last-prompt","lastPrompt":"<!-- {M} -->"}}"#);
        // 排队窗口内 claude 还在跑上一件事,转录里持续有它的输出 —— 一条都不该开闸。
        let busy = r#"{"type":"assistant","message":{"role":"assistant","stop_reason":"tool_use","content":[{"type":"text","text":"还在跑别的工具"}]}}"#;
        for line in [enqueue.as_str(), busy, last.as_str(), remove.as_str()] {
            match gate.feed(format!("{line}\n").as_bytes(), &echo(M)).unwrap() {
                GateStep::Consumed(_) => panic!("队列窗口内不得开闸: {line}"),
                GateStep::Enqueued | GateStep::QueuedWithoutText | GateStep::Waiting => {}
            }
        }
        let consumed = format!(
            r#"{{"attachment":{{"type":"queued_command","prompt":"<!-- {M} -->"}},"type":"attachment"}}"#
        );
        let after = r#"{"type":"assistant","message":{"role":"assistant","stop_reason":"end_turn","content":[{"type":"text","text":"本轮回复"}]}}"#;
        let GateStep::Consumed(trailing) = gate
            .feed(format!("{consumed}\n{after}\n").as_bytes(), &echo(M))
            .unwrap()
        else {
            panic!("消费记录必须开闸");
        };
        let trailing = String::from_utf8(trailing).unwrap();
        assert!(trailing.contains("本轮回复"));
        assert!(!trailing.contains("queued_command"), "消费记录本身不该回流");
    }

    /// 测试用的诊断快照:字段值本身与判定无关,只为凑出 `ensure_prompt_echo_not_timed_out` 的入参。
    fn probe_diag() -> ArrivalDiag<'static> {
        ArrivalDiag {
            path: std::path::Path::new("/probe.jsonl"),
            marker: "summon-transport-turn:probe",
            inject_offset: 0,
            read_bytes: 0,
            saw_enqueue: false,
            rescans: 0,
            resubmits: 0,
            last_read_error: None,
        }
    }

    /// 两相止损:确认入队**之前**用短的送达窗(防 send-keys 没落到 pane 上);确认入队**之后**
    /// 排队窗口无上界,只留防死锁的硬顶。固定 15s 在实测样本(6.9/9.3/29.7/152.7s)上过半会误判。
    #[test]
    fn wait_deadline_extends_only_after_enqueue_confirmed() {
        let sent = Instant::now();
        assert_eq!(
            prompt_wait_deadline(sent, None),
            sent + PROMPT_ARRIVAL_TIMEOUT
        );
        let enqueued = sent + Duration::from_millis(120); // 实测 marker 61–149ms 就以 enqueue 落盘
        assert_eq!(
            prompt_wait_deadline(sent, Some(enqueued)),
            enqueued + PROMPT_CONSUME_TIMEOUT
        );
        assert!(
            prompt_wait_deadline(sent, Some(enqueued)) > sent + Duration::from_secs(153),
            "实测最长排队 152.7s,止损必须在它之外"
        );
    }

    #[test]
    fn real_prompt_wait_times_out_while_unrelated_bytes_keep_growing() {
        use std::io::Write as _;
        use std::sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        };

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("arrival-growth.jsonl");
        std::fs::write(&path, "").unwrap();
        let marker = "summon-transport-turn:123e4567-e89b-12d3-a456-426614174001".to_string();
        let stop = Arc::new(AtomicBool::new(false));
        let writer_path = path.clone();
        let writer_marker = marker.clone();
        let writer_stop = Arc::clone(&stop);
        let writer = std::thread::spawn(move || {
            // 持续时间刻意超过下面的验收上界：若实现错误地用“任意新字节”重置 arrival
            // deadline，本测试会等完整段增长结束后才返回，无法再从时间断言里漏过去。
            for _ in 0..80 {
                if writer_stop.load(Ordering::Acquire) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(3));
                let mut file = std::fs::OpenOptions::new()
                    .append(true)
                    .open(&writer_path)
                    .unwrap();
                // last-prompt 会带 exact marker 全文，却只是快照、不是 enqueue/consume 证据。
                // 这同时钉住“不能退化成 substring 命中”的白名单边界。
                writeln!(
                    file,
                    "{}",
                    serde_json::json!({
                        "type": "last-prompt",
                        "lastPrompt": writer_marker
                    })
                )
                .unwrap();
            }
        });
        let exec = crate::runtime::executor::LocalExecutor;
        let started = Instant::now();
        let result = wait_for_prompt_echo(
            &exec,
            PromptEchoWait {
                path: &path,
                relocation: None,
                prompt: "new prompt",
                marker,
                inject_offset: 0,
                file_identity: exec.file_identity(&path).unwrap(),
                arrival_timeout: Duration::from_millis(25),
                consume_timeout: Duration::from_millis(200),
                poll: Duration::from_millis(2),
                resubmit_every: Duration::from_millis(5),
                resubmit_tries: ARRIVAL_RESUBMIT_TRIES,
            },
            || true,
            || false,
        );
        let waited = started.elapsed();
        let error = match result {
            Err(error) => error,
            Ok(_) => panic!("旧 turn 持续长字节也不能替本 marker 续命"),
        };
        stop.store(true, Ordering::Release);
        writer.join().unwrap();
        assert!(error.to_string().contains("marker"), "{error:#}");
        assert!(
            waited < Duration::from_millis(100),
            "arrival 窗应独立于无关转录增长，实际 {:?}",
            waited
        );
    }

    /// 时序预算给足:并行跑全量时线程调度可能延迟数十毫秒,紧时序(8ms/45ms/20ms)会 flake。
    /// 断言的核心是「enqueue 把等待切到 consume 窗」,不是具体毫秒数。
    #[test]
    fn real_prompt_wait_extends_only_after_exact_enqueue_then_consumes() {
        use std::io::Write as _;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("enqueue-consume.jsonl");
        std::fs::write(&path, "").unwrap();
        let marker = "summon-transport-turn:123e4567-e89b-12d3-a456-426614174002";
        let writer_path = path.clone();
        let writer_marker = marker.to_string();
        let writer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&writer_path)
                .unwrap();
            writeln!(
                file,
                "{}",
                serde_json::json!({
                    "type": "queue-operation",
                    "operation": "enqueue",
                    "content": writer_marker.clone()
                })
            )
            .unwrap();
            file.flush().unwrap();
            std::thread::sleep(Duration::from_millis(200));
            writeln!(
                file,
                "{}",
                serde_json::json!({
                    "type": "user",
                    // 正文 + marker:转录里的 user 记录本来就是这个形态,判据两头都要看。
                    "message": {"role": "user", "content": format!("queued prompt\n\n{writer_marker}")}
                })
            )
            .unwrap();
        });
        let exec = crate::runtime::executor::LocalExecutor;
        let outcome = wait_for_prompt_echo(
            &exec,
            PromptEchoWait {
                path: &path,
                relocation: None,
                prompt: "queued prompt",
                marker: marker.into(),
                inject_offset: 0,
                file_identity: exec.file_identity(&path).unwrap(),
                arrival_timeout: Duration::from_millis(200),
                consume_timeout: Duration::from_millis(1000),
                poll: Duration::from_millis(5),
                resubmit_every: Duration::from_millis(5),
                resubmit_tries: ARRIVAL_RESUBMIT_TRIES,
            },
            || true,
            || false,
        )
        .expect("exact enqueue 应把等待切到 consume 窗");
        writer.join().unwrap();
        assert!(matches!(outcome, CorrelatedPrompt::Echoed { .. }));
    }

    /// claude 压缩上下文时收下的输入:入队记录不带正文,压完才落带 marker 的消费记录。
    const QUEUED_WITHOUT_TEXT: &str = r#"{"type":"queue-operation","operation":"enqueue","timestamp":"2026-11-11T09:42:10.615Z","sessionId":"s"}"#;

    #[test]
    fn a_textless_enqueue_is_told_apart_from_other_queue_records() {
        let marker = "summon-transport-turn:123e4567-e89b-12d3-a456-426614174010";
        assert_eq!(
            classify_marker_record(QUEUED_WITHOUT_TEXT.as_bytes(), &echo(marker)),
            MarkerRecord::QueuedWithoutText
        );
        let dequeue = r#"{"type":"queue-operation","operation":"dequeue","sessionId":"s"}"#;
        assert_eq!(
            classify_marker_record(dequeue.as_bytes(), &echo(marker)),
            MarkerRecord::Unrelated
        );
        let other =
            r#"{"type":"queue-operation","operation":"enqueue","content":"<task-notification>"}"#;
        assert_eq!(
            classify_marker_record(other.as_bytes(), &echo(marker)),
            MarkerRecord::Unrelated
        );
    }

    #[test]
    fn an_exact_enqueue_outranks_a_textless_one_in_the_same_chunk() {
        let marker = "summon-transport-turn:123e4567-e89b-12d3-a456-426614174011";
        let exact = serde_json::json!({
            "type": "queue-operation",
            "operation": "enqueue",
            "content": marker
        });
        let mut gate = PromptEchoGate::default();
        let chunk = format!("{QUEUED_WITHOUT_TEXT}\n{exact}\n");
        assert!(matches!(
            gate.feed(chunk.as_bytes(), &echo(marker)).unwrap(),
            GateStep::Enqueued
        ));
        let mut gate = PromptEchoGate::default();
        assert!(matches!(
            gate.feed(format!("{QUEUED_WITHOUT_TEXT}\n").as_bytes(), &echo(marker))
                .unwrap(),
            GateStep::QueuedWithoutText
        ));
    }

    /// 2026-11-11 真机:claude 空闲压缩期间收到注入,入队记录不带正文,压了 18s 才落消费记录。
    /// 送达窗只有 15s,不放宽就把一条已经送到、随后被回答的消息报成失败。
    #[test]
    fn a_textless_enqueue_after_injection_waits_out_a_compaction() {
        use std::io::Write as _;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("compacting.jsonl");
        std::fs::write(&path, "").unwrap();
        let marker = "summon-transport-turn:123e4567-e89b-12d3-a456-426614174012";
        let writer_path = path.clone();
        let writer = std::thread::spawn(move || {
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&writer_path)
                .unwrap();
            std::thread::sleep(Duration::from_millis(30));
            writeln!(file, "{QUEUED_WITHOUT_TEXT}").unwrap();
            std::thread::sleep(Duration::from_millis(400));
            file.write_all(consumed_user_line("stuck prompt", marker).as_bytes())
                .unwrap();
        });
        let exec = crate::runtime::executor::LocalExecutor;
        let outcome = wait_for_prompt_echo(
            &exec,
            resubmit_wait(&path, marker, Duration::from_millis(200)),
            || true,
            || false,
        )
        .expect("压缩期间入队的消息应等到消费");
        writer.join().unwrap();
        assert!(matches!(outcome, CorrelatedPrompt::Echoed { .. }));
    }

    /// 注入之前就在转录里的不带正文的入队(更早一次压缩留下的)不放宽送达窗:到点的救援会从
    /// 文件头重读,读到它也不能替一次丢失的注入续命。
    #[test]
    fn a_textless_enqueue_from_before_injection_does_not_extend_arrival() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("old-compaction.jsonl");
        std::fs::write(&path, format!("{QUEUED_WITHOUT_TEXT}\n")).unwrap();
        let exec = crate::runtime::executor::LocalExecutor;
        let mut wait = resubmit_wait(
            &path,
            "summon-transport-turn:123e4567-e89b-12d3-a456-426614174013",
            Duration::from_millis(100),
        );
        wait.inject_offset = exec.file_len(&path).unwrap();
        let started = Instant::now();
        let result = wait_for_prompt_echo(&exec, wait, || true, || false);
        assert!(result.is_err(), "marker 从没出现,不能算送达");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "不该放宽到消费窗: {:?}",
            started.elapsed()
        );
    }

    fn consumed_user_line(prompt: &str, marker: &str) -> String {
        serde_json::json!({
            "type": "user",
            "message": {"role": "user", "content": format!("{prompt}\n\n{marker}")}
        })
        .to_string()
            + "\n"
    }

    fn resubmit_wait<'a>(
        path: &'a std::path::Path,
        marker: &str,
        arrival_timeout: Duration,
    ) -> PromptEchoWait<'a> {
        PromptEchoWait {
            path,
            relocation: None,
            prompt: "stuck prompt",
            marker: marker.into(),
            inject_offset: 0,
            file_identity: None,
            arrival_timeout,
            consume_timeout: Duration::from_secs(5),
            poll: Duration::from_millis(2),
            resubmit_every: Duration::from_millis(5),
            resubmit_tries: ARRIVAL_RESUBMIT_TRIES,
        }
    }

    /// 会话刚起来就注入时,粘贴后紧跟的回车被整批吞掉:转录文件都还没建,正文停在输入框里。
    /// 送达相看到它就补按回车,补按之后的那条消费记录照常认作本轮。
    #[test]
    fn a_prompt_left_in_the_input_box_is_resubmitted_before_arrival_times_out() {
        use std::io::Write as _;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cold-start.jsonl");
        let marker = "summon-transport-turn:123e4567-e89b-12d3-a456-426614174003";
        let exec = crate::runtime::executor::LocalExecutor;
        let mut presses = 0;
        let outcome = wait_for_prompt_echo(
            &exec,
            resubmit_wait(&path, marker, Duration::from_secs(5)),
            || true,
            || {
                presses += 1;
                let mut file = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&path)
                    .unwrap();
                file.write_all(consumed_user_line("stuck prompt", marker).as_bytes())
                    .unwrap();
                true
            },
        )
        .expect("补按回车后的消费记录应认作本轮");
        assert!(matches!(outcome, CorrelatedPrompt::Echoed { .. }));
        assert_eq!(presses, 1);
    }

    /// 补按有上限:回车怎么按都不生效时照旧在送达窗收尾,诊断里写明按过几次。
    #[test]
    fn resubmits_stop_at_the_cap_and_the_timeout_reports_them() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("never-arrives.jsonl");
        std::fs::write(&path, "").unwrap();
        let exec = crate::runtime::executor::LocalExecutor;
        let mut presses = 0;
        let error = match wait_for_prompt_echo(
            &exec,
            resubmit_wait(
                &path,
                "summon-transport-turn:cap",
                Duration::from_millis(200),
            ),
            || true,
            || {
                presses += 1;
                true
            },
        ) {
            Err(error) => error,
            Ok(_) => panic!("marker 从没出现,不能算送达"),
        };
        assert_eq!(presses, ARRIVAL_RESUBMIT_TRIES);
        assert!(
            error
                .to_string()
                .contains(&format!("补按回车={ARRIVAL_RESUBMIT_TRIES}")),
            "{error:#}"
        );
    }

    /// 屏上没有停着的正文(没按)不占补按次数:claude 晚几拍才把粘贴画出来时,后面的节拍还能按。
    #[test]
    fn probes_that_find_nothing_stuck_do_not_use_up_resubmits() {
        use std::io::Write as _;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("late-paste.jsonl");
        std::fs::write(&path, "").unwrap();
        let marker = "summon-transport-turn:123e4567-e89b-12d3-a456-426614174004";
        let exec = crate::runtime::executor::LocalExecutor;
        let probes_before_stuck = ARRIVAL_RESUBMIT_TRIES + 2;
        let mut probes = 0;
        let outcome = wait_for_prompt_echo(
            &exec,
            resubmit_wait(&path, marker, Duration::from_secs(5)),
            || true,
            || {
                probes += 1;
                if probes <= probes_before_stuck {
                    return false;
                }
                let mut file = std::fs::OpenOptions::new()
                    .append(true)
                    .open(&path)
                    .unwrap();
                file.write_all(consumed_user_line("stuck prompt", marker).as_bytes())
                    .unwrap();
                true
            },
        )
        .expect("没按的节拍不该把补按次数用掉");
        assert!(matches!(outcome, CorrelatedPrompt::Echoed { .. }));
        assert_eq!(probes, probes_before_stuck + 1);
    }

    /// 见过本轮入队之后不再补按:那时输入框里的字已经不是本轮正文(多半是有人 attach 进去
    /// 打的草稿),按下去就是替人发出去。
    #[test]
    fn no_resubmit_once_the_marker_is_enqueued() {
        use std::io::Write as _;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("enqueued.jsonl");
        let marker = "summon-transport-turn:123e4567-e89b-12d3-a456-426614174005";
        let enqueue = serde_json::json!({
            "type": "queue-operation",
            "operation": "enqueue",
            "content": marker
        });
        std::fs::write(&path, format!("{enqueue}\n")).unwrap();
        let writer_path = path.clone();
        let writer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&writer_path)
                .unwrap();
            file.write_all(consumed_user_line("stuck prompt", marker).as_bytes())
                .unwrap();
        });
        let exec = crate::runtime::executor::LocalExecutor;
        let mut presses = 0;
        let outcome = wait_for_prompt_echo(
            &exec,
            resubmit_wait(&path, marker, Duration::from_millis(10)),
            || true,
            || {
                presses += 1;
                true
            },
        )
        .expect("入队之后等消费");
        writer.join().unwrap();
        assert!(matches!(outcome, CorrelatedPrompt::Echoed { .. }));
        assert_eq!(presses, 0);
    }

    /// 文件在注入后被 truncate/replace 时，旧 offset 可能落到 exact marker 之后。必须识别文件换代
    /// 并从头重建 gate；这是「marker 已落盘、增量 reader 却永远看不到」的确定性复现。
    #[test]
    fn transcript_replacement_before_marker_is_recovered() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("turn.jsonl");
        std::fs::write(&path, vec![b'x'; 4096]).unwrap();
        let exec = crate::runtime::executor::LocalExecutor;
        let old_offset = exec.file_len(&path).unwrap();
        let old_identity = exec.file_identity(&path).unwrap();

        let marker = "summon-transport-turn:replacement";
        let user = format!(
            "{{\"type\":\"user\",\"message\":{{\"role\":\"user\",\"content\":\"<!-- {marker} -->\"}}}}\n"
        );
        let answer = r#"{"type":"assistant","message":{"role":"assistant","stop_reason":"end_turn","content":[{"type":"text","text":"恢复成功"}]}}
"#;
        let replacement = dir.path().join("replacement.jsonl");
        std::fs::write(&replacement, format!("{user}{answer}")).unwrap();
        std::fs::rename(&replacement, &path).unwrap();

        let new_len = exec.file_len(&path).unwrap();
        let new_identity = exec.file_identity(&path).unwrap();
        assert!(transcript_needs_rescan(
            old_identity,
            new_identity,
            old_offset,
            Some(new_len)
        ));
        let (step, end, _) = reread_prompt_echo(&exec, &path, 0, &echo(marker)).unwrap();
        let GateStep::Consumed(trailing) = step else {
            panic!("替换文件里的 exact marker 必须被重扫命中");
        };
        assert!(String::from_utf8(trailing).unwrap().contains("恢复成功"));
        assert_eq!(end, new_len);
    }

    #[test]
    fn processed_turn_prefix_returns_only_unseen_bytes_and_rejects_unsafe_replay() {
        let mut prefix = ProcessedTurnPrefix::default();
        prefix.record(b"already-seen");
        assert_eq!(
            prefix.unseen_suffix(b"already-seen-new".to_vec()).unwrap(),
            b"-new"
        );
        assert!(prefix.unseen_suffix(b"already-different".to_vec()).is_err());
        assert!(prefix.unseen_suffix(b"short".to_vec()).is_err());

        let mut overflowed = ProcessedTurnPrefix::default();
        overflowed.record(&vec![b'x'; TURN_REANCHOR_PREFIX_LIMIT + 1]);
        assert!(overflowed.unseen_suffix(Vec::new()).is_err());
    }

    /// marker 已消费、部分输出已触发回调后再 atomic replace：重建时必须精确跳过已处理前缀，
    /// 只把新 terminal 喂给既有 tail，不能重复「前半」或静默漏掉「最终答案」。
    #[test]
    fn replacement_after_consumed_marker_reanchors_without_replaying_callbacks() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("turn.jsonl");
        let marker = "summon-transport-turn:123e4567-e89b-12d3-a456-426614174000";
        let user = serde_json::json!({
            "type": "user",
            "message": {"role": "user", "content": format!("任务\n<!-- {marker} -->")}
        });
        let partial = serde_json::json!({
            "type": "assistant",
            "message": {"role": "assistant", "stop_reason": "tool_use",
                "content": [{"type": "text", "text": "前半"}]}
        });
        let terminal = serde_json::json!({
            "type": "assistant",
            "message": {"role": "assistant", "stop_reason": "end_turn",
                "content": [{"type": "text", "text": "最终答案"}]}
        });
        let old = format!("旧历史会改变新文件 offset\n{user}\n{partial}\n");
        std::fs::write(&path, &old).unwrap();

        let exec = crate::runtime::executor::LocalExecutor;
        let (step, _, checkpoint) = reread_prompt_echo(&exec, &path, 0, &echo(marker)).unwrap();
        let GateStep::Consumed(first_trailing) = step else {
            panic!("初始 marker 应已消费");
        };
        let mut processed = ProcessedTurnPrefix::default();
        processed.record(&first_trailing);
        let mut tail = TranscriptTail::default();
        let mut noop = |_: &AgentMessage| {};
        assert!(matches!(
            tail.feed(&first_trailing, &mut noop),
            TailStep::Progress { .. }
        ));

        let replacement = dir.path().join("replacement.jsonl");
        std::fs::write(
            &replacement,
            format!("短历史\n{user}\n{partial}\n{terminal}\n"),
        )
        .unwrap();
        std::fs::rename(&replacement, &path).unwrap();
        assert!(
            checkpoint.read_new(&exec, &path).unwrap().is_none(),
            "offset 改变后的新 generation 不能沿旧 checkpoint 续读"
        );

        let (step, _, _) = reread_prompt_echo(&exec, &path, 0, &echo(marker)).unwrap();
        let GateStep::Consumed(rebuilt) = step else {
            panic!("新 generation 应能按 exact marker 重锚");
        };
        let unseen = processed.unseen_suffix(rebuilt).unwrap();
        let step = tail.feed(&unseen, &mut noop);
        let TailStep::Done {
            stream, final_text, ..
        } = step
        else {
            panic!("重锚后只喂 unseen terminal 应正常收尾");
        };
        assert_eq!(final_text, "最终答案");
        assert_eq!(stream.as_deref(), Some("前半\n\n最终答案"));
    }

    /// 检出能力不降:真的「send-keys 没落到 pane 上」表现为转录纹丝不动,静默窗依旧 15s 到点失败。
    #[test]
    fn silent_transcript_still_fails_within_arrival_window() {
        let sent = Instant::now();
        let deadline = prompt_wait_deadline(sent, None);
        assert_eq!(deadline, sent + PROMPT_ARRIVAL_TIMEOUT);
        ensure_prompt_echo_not_timed_out(deadline, deadline, &probe_diag())
            .expect_err("转录一个字节都没动时必须照旧在送达窗内失败");
    }

    /// 超时错误必须带现场:事故当天只留下一句「超时」,分不清「一个字节没读到」还是
    /// 「读到了但没认出 marker」,只能靠事后翻转录反推。诊断字段就是为了把这两类一眼分开。
    #[test]
    fn arrival_timeout_error_carries_diagnostics() {
        let now = Instant::now();
        let diag = ArrivalDiag {
            path: std::path::Path::new("/tmp/sid.jsonl"),
            marker: "summon-transport-turn:cc33",
            inject_offset: 4096,
            read_bytes: 0,
            saw_enqueue: false,
            rescans: 2,
            resubmits: 3,
            last_read_error: Some("temporary read failure"),
        };
        let error = ensure_prompt_echo_not_timed_out(now, now, &diag).expect_err("应超时");
        let text = error.to_string();
        for field in [
            "offset=4096",
            "读到 0 字节",
            "见过入队=false",
            "重扫=2",
            "补按回车=3",
            "temporary read failure",
            "summon-transport-turn:cc33",
            "/tmp/sid.jsonl",
        ] {
            assert!(text.contains(field), "诊断缺字段 {field}: {text}");
        }
    }

    /// 补按回车的接线:生产闭包先经 `holds_unsent_prompt` 证明输入框里停着的是本轮正文,证得了
    /// 才按。等待循环的测试都传自己的闭包,接线只能在源码上钉住。
    #[test]
    fn the_resubmit_presses_enter_only_after_proving_this_prompt_is_stuck() {
        let production = production_source();
        let start = production
            .find("fn send_correlated_prompt(")
            .expect("send_correlated_prompt 定义");
        let body = &production[start..];
        let body = &body[..body.find("\n    }\n").expect("函数结尾")];
        let first_line = body
            .find("wire_prompt.lines().next()")
            .expect("比对的是本轮正文");
        let proof = body
            .find("holds_unsent_prompt(&pane, first_line,")
            .expect("补按前的证明");
        let gated = body.find("stuck && {").expect("证不得就不按");
        assert!(first_line < proof && proof < gated);
        let open = gated + "stuck && ".len();
        let mut depth = 0usize;
        let close = body[open..]
            .char_indices()
            .find_map(|(i, c)| {
                match c {
                    '{' => depth += 1,
                    '}' => depth -= 1,
                    _ => {}
                }
                (depth == 0).then_some(open + i)
            })
            .expect("补按块结尾");
        let block = &body[open..=close];
        assert!(
            block.contains("tmux.send_enter()"),
            "回车只在证得之后的块里按"
        );
        for outside in [&body[..open], &body[close..]] {
            for key in ["send_enter(", "send_keys", "\"Enter\""] {
                assert!(!outside.contains(key), "补按块外不许另有回车:{key}");
            }
        }
    }

    /// 步骤2 的时序不变量:OnIt 只能在**观测到消费记录之后**贴。`send_correlated_prompt` 是唯一
    /// 会一直等到消费记录出现的地方(开闸判据见 `PromptEchoGate`),所以 `on_delivered()` 必须排在
    /// 它之后、进主 poll 循环之前。判据存在不等于被用上:把回调挪到函数开头,上面那些纯函数测试
    /// 照样全绿,而用户会在消息还躺在 claude 队列里(实测最长 152.7s)时就看到「在处理」。
    #[test]
    fn run_turn_acks_delivery_only_after_the_consumption_gate_opens() {
        let production = production_source();
        let start = production.find("pub fn run_turn(").expect("run_turn 定义");
        let body = &production[start..];
        let send = body
            .find("match self.send_correlated_prompt(&path, prompt, on_marker_injected)?")
            .expect("本轮首次注入");
        let consumed = body
            .find("on_marker_consumed(consumed_boundary)")
            .expect("exact marker 消费边界");
        let claim = body.find("if !on_main_claim()").expect("主路径输出仲裁");
        assert!(consumed < claim, "消费边界必须在主路径仲裁前登记");
        let ack = body.find("on_delivered();").expect("送达回调调用点");
        let main_loop = body.find("let final_text = loop {").expect("主 poll 循环");
        assert!(
            ack > send,
            "on_delivered 必须排在 send_correlated_prompt 之后——早贴就是在排队窗口里谎报在处理"
        );
        assert!(
            claim > send && claim < ack,
            "exact marker 消费后必须先仲裁 main/mirror，再触发任何用户可见回调"
        );
        assert!(
            ack < main_loop,
            "on_delivered 必须在进主循环前贴完——晚贴等于要等第一个工具跑起来才认送达"
        );
    }

    /// `Session` / `Tmux` 上**所有**会往 pane 写东西的原语。零注入的臂靠它做黑名单,所以名字
    /// 取 Rust 侧真实存在的那些拼法(tmux 子命令的 `send-keys` 在仓里并不存在)。
    /// `exit_copy_mode` 也在内:它无条件 `tmux copy-mode -q`,会把正 attach 的用户踢出滚屏。
    const PANE_WRITE_PRIMITIVES: &[&str] = &[
        "send_line(",
        "send_text(",
        "send_enter(",
        "send_key(",
        "send_correlated_prompt",
        "send_line_for_driver",
        "send_escape",
        "exit_copy_mode",
        // Esc 只在「静止的已知面板」上被授权,而长工具 / 等 API 响应的画面同样静止 ——
        // 注入闸那条路会把 Esc 打在正被接管的那一轮上。
        "prove_injectable",
    ];

    /// turn 类 slash 的 markerless 轮(`TurnInjection::PreInjected`):注入已由执行器过闸完成,
    /// run_turn 的 PreInjected 臂**零注入**(不碰 [`PANE_WRITE_PRIMITIVES`] 里的任何一个),
    /// 且与 Correlated 同序 —— 先 on_main_claim 仲裁输出权,再 on_delivered。接错顺序 = 仲裁
    /// 旁路/排队窗口谎报,都是 08-12 的既有红线;文件换代时无 marker 可重锚,必须 fail-closed。
    #[test]
    fn a_preinjected_markerless_turn_claims_before_ack_and_never_sends() {
        let production = production_source();
        let start = production
            .find("TurnInjection::PreInjected { baseline } =>")
            .expect("PreInjected 臂");
        let body = &production[start..];
        let arm_end = body
            .find("TurnInjection::Reattached {")
            .expect("下一个注入形态");
        let arm = &body[..arm_end];
        for forbidden in PANE_WRITE_PRIMITIVES {
            assert!(
                !arm.contains(forbidden),
                "PreInjected 臂不得再往 pane 写任何东西(注入已由 slash 执行器 at-most-once 完成): {forbidden}"
            );
        }
        let claim = arm.find("if !on_main_claim()").expect("输出权仲裁");
        let ack = arm.find("on_delivered();").expect("送达回调");
        assert!(claim < ack, "必须先仲裁输出权,再触发用户可见回调");
        assert!(
            production.contains("summon_i18n::format!(\"rb7cc4044ac\""),
            "markerless 轮的 truncate/replace 必须 fail-closed(无 marker 可重锚)"
        );
    }

    /// 接管夹具:按真机形态拼一段转录 —— 停机前的历史、上一代那条 prompt 的**消费**记录、
    /// 以及这一轮的终态回复。返回 (整份内容, 消费记录右侧的绝对字节边界)。
    fn reattach_transcript(
        history: &str,
        prompt: &str,
        marker: &str,
        reply: &str,
    ) -> (String, u64) {
        let consumed = serde_json::json!({
            "type": "user",
            "message": {"role": "user", "content": format!("{prompt}\n\n<!-- {marker} -->")}
        })
        .to_string();
        let terminal = serde_json::json!({
            "type": "assistant",
            "message": {
                "role": "assistant",
                "stop_reason": "end_turn",
                "content": [{"type": "text", "text": reply}]
            }
        })
        .to_string();
        let head = format!("{history}{consumed}\n");
        let boundary = head.len() as u64;
        (format!("{head}{terminal}\n"), boundary)
    }

    /// 停机前已经落盘的上一轮输出。它必须在 `inject_offset` 左侧,用来钉住「接管不会把历史
    /// 当成本轮」。
    fn reattach_history() -> String {
        format!(
            "{}\n",
            serde_json::json!({
                "type": "assistant",
                "message": {
                    "role": "assistant",
                    "stop_reason": "end_turn",
                    "content": [{"type": "text", "text": "上一轮的回复"}]
                }
            })
        )
    }

    #[test]
    fn silent_reset_result_keeps_whitespace_through_transcript_capture() {
        let keyword = summon_core::messaging::result_control::SILENT_RESET_KEYWORD;
        for text in [
            keyword.to_string(),
            format!("{keyword}\n"),
            format!(" {keyword}"),
        ] {
            let line = serde_json::json!({
                "type": "assistant",
                "message": { "stop_reason": "end_turn", "content": [{"type": "text", "text": text}] }
            }).to_string() + "\n";
            assert_eq!(
                terminal_reply(line.as_bytes()).as_deref(),
                Some(text.as_str())
            );
        }
        // run_turn 的终态适配必须把 parser 原文交给结果判据，两端都保留空白。
        let source = include_str!("transcript.rs")
            .split("\n#[cfg(test)]\nmod tests {")
            .next()
            .unwrap();
        assert!(source.contains("on_message(&final_text);"));
        assert!(source.contains("then(|| final_text.clone())"));
    }

    /// 把接管产出的字节喂进本轮 parser,取终态回复 —— `run_turn` 下方共用的 poll 循环拿到
    /// `first_chunk` 后做的就是这件事。
    fn terminal_reply(chunk: &[u8]) -> Option<String> {
        let mut tail = TranscriptTail::default();
        match tail.feed(chunk, &mut |_: &AgentMessage| {}) {
            TailStep::Done { final_text, .. } => Some(final_text),
            _ => None,
        }
    }

    /// 只回一屏固定画面的 pane 探针:任何按键都 panic。接管路径的「零 tmux 写」在这里被钉死,
    /// 而判定本身仍走生产的 `observe_idle_prompt`(合成判据不算数,得是真机那一屏)。
    struct SilentPane(&'static str);

    impl crate::session::input_gate::PaneProbe for SilentPane {
        fn exit_copy_mode(&self) {
            // 它无条件 `tmux copy-mode -q`:正 attach 着滚屏看输出的用户会被踢回底部。
            panic!("接管路径不得改动 pane 状态")
        }
        fn capture(&self) -> std::io::Result<String> {
            Ok(self.0.to_string())
        }
        fn send_escape(&self) -> std::io::Result<()> {
            panic!("接管路径不得往 pane 发任何键")
        }
        fn cursor(&self) -> std::io::Result<crate::session::tmux::PaneCursor> {
            panic!("接管路径不得探测 pane 光标")
        }
        fn send_end(&self) -> std::io::Result<()> {
            panic!("接管路径不得往 pane 发任何键")
        }
        fn send_home(&self) -> std::io::Result<()> {
            panic!("接管路径不得往 pane 发任何键")
        }
    }

    /// 真机形态:空 `❯` 输入行 = claude 手上什么都没有。
    const REATTACH_IDLE_PANE: &str =
        "✻ Worked for 7m 25s\n──── summon:soul-x ──\n❯ \n────\n\n  ⏵⏵ bypass permissions on\n";
    /// 真机形态:spinner 行 + 空输入框 = 正在跑一轮(消息可能就排在队列里)。
    const REATTACH_BUSY_PANE: &str = "✽ Composing… (11m 6s · ↓ 34.5k tokens)\n────\n❯ \n";

    fn pane_is_idle(pane: &'static str) -> bool {
        crate::session::input_gate::observe_idle_prompt(&SilentPane(pane), Default::default())
    }

    /// 数 `read_file_range` 次数的执行器,其余一律直通本机。等消费记录那个循环最长跑十分钟,
    /// 「这一拍到底重读没重读」只能靠数出来。
    struct CountingExec {
        inner: crate::runtime::executor::LocalExecutor,
        range_reads: std::cell::Cell<usize>,
    }

    impl CountingExec {
        fn new() -> Self {
            Self {
                inner: crate::runtime::executor::LocalExecutor,
                range_reads: std::cell::Cell::new(0),
            }
        }
    }

    impl Executor for CountingExec {
        fn spawn(
            &self,
            spec: &crate::runtime::executor::SpawnSpec,
        ) -> anyhow::Result<std::process::Child> {
            self.inner.spawn(spec)
        }
        fn read_file_tail(&self, path: &std::path::Path, from: u64) -> anyhow::Result<Vec<u8>> {
            self.inner.read_file_tail(path, from)
        }
        fn read_file_range(
            &self,
            path: &std::path::Path,
            from: u64,
            max_bytes: usize,
        ) -> anyhow::Result<Vec<u8>> {
            self.range_reads.set(self.range_reads.get() + 1);
            self.inner.read_file_range(path, from, max_bytes)
        }
        fn file_len(&self, path: &std::path::Path) -> anyhow::Result<u64> {
            self.inner.file_len(path)
        }
        fn file_identity(&self, path: &std::path::Path) -> anyhow::Result<Option<FileIdentity>> {
            self.inner.file_identity(path)
        }
        fn tmux(&self, args: &[&str]) -> anyhow::Result<String> {
            self.inner.tmux(args)
        }
    }

    /// 转录比注入前还短 = 换代,本轮的消费记录不可能在这份文件里。白等满十分钟只会把恢复卡
    /// 推迟十分钟,所以当场报普通错误(不是 `ReattachError`,那三类都不是这回事)。
    #[test]
    fn reattached_scan_bails_when_the_transcript_shrank_below_inject_offset() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("reattach-shrunk.jsonl");
        std::fs::write(&path, reattach_history()).unwrap();

        let started = Instant::now();
        let error = locate_reattached_turn(
            &crate::runtime::executor::LocalExecutor,
            ReattachWait {
                path: &path,
                prompt_head: "",
                marker: "summon-transport-turn:123e4567-e89b-12d3-a456-426614174015",
                inject_offset: 1 << 20,
                consumed_boundary: None,
                consume_timeout: Duration::from_secs(600),
                poll: Duration::from_millis(5),
            },
            || true,
            || panic!("换代已成立时不必再问 pane"),
        )
        .expect_err("转录比注入前短就不该继续等");

        assert!(error.to_string().contains("换代"), "{error:#}");
        assert!(
            error.downcast_ref::<ReattachError>().is_none(),
            "换代不是接管的三类结局,别让调用方按它们分流"
        );
        assert!(started.elapsed() < Duration::from_secs(1), "必须当场返回");
    }

    /// 上一代停机前已经观测到消费边界:接管不必再找,直接从边界右侧接着消费,一个字节都不注入。
    #[test]
    fn reattached_with_consumed_boundary_and_terminal_on_disk_finishes_without_injecting() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("reattach-boundary.jsonl");
        let marker = "summon-transport-turn:123e4567-e89b-12d3-a456-426614174010";
        let prompt = "接着把剩下的活干完";
        let (content, boundary) =
            reattach_transcript(&reattach_history(), prompt, marker, "接管后收到的最终回复");
        std::fs::write(&path, &content).unwrap();

        let (trailing, observed_boundary, checkpoint) = locate_reattached_turn(
            &crate::runtime::executor::LocalExecutor,
            ReattachWait {
                path: &path,
                prompt_head: &prompt_head(prompt),
                marker,
                inject_offset: reattach_history().len() as u64,
                consumed_boundary: Some(boundary),
                consume_timeout: Duration::from_secs(1),
                poll: Duration::from_millis(5),
            },
            || true,
            || panic!("边界已知时不该抓屏,更不该碰 pane"),
        )
        .expect("有消费边界就必须能接管");

        assert_eq!(observed_boundary, boundary, "消费边界原样交还给 tracker");
        assert_eq!(
            terminal_reply(&trailing).as_deref(),
            Some("接管后收到的最终回复"),
            "边界右侧的字节里已有本轮终态,第一拍就该收尾"
        );
        assert_eq!(
            checkpoint.end(),
            content.len() as u64,
            "checkpoint 必须覆盖已交出去的字节,否则主循环会把它们再读一遍"
        );
    }

    /// 停机时还没观测到消费:从 `inject_offset` 往后现找 marker 的消费记录,同样零注入。
    #[test]
    fn reattached_without_boundary_finds_marker_from_inject_offset() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("reattach-scan.jsonl");
        let marker = "summon-transport-turn:123e4567-e89b-12d3-a456-426614174011";
        let prompt = "重启期间这条还排在队列里";
        let history = reattach_history();
        let (content, boundary) = reattach_transcript(&history, prompt, marker, "排完队才给的回复");
        std::fs::write(&path, &content).unwrap();

        // 两种 head 都要走一遍:锚点带 head(注入那刻冻的正文开头,顺带复核消息没被 CLI 截断)
        // 与旧锚点的空 head(只认随机 marker)必须找到**同一条**记录 —— 否则升级前落的锚点
        // 接管不回来。
        for head in [prompt_head(prompt), String::new()] {
            let (trailing, observed_boundary, checkpoint) = locate_reattached_turn(
                &crate::runtime::executor::LocalExecutor,
                ReattachWait {
                    path: &path,
                    prompt_head: &head,
                    marker,
                    inject_offset: history.len() as u64,
                    consumed_boundary: None,
                    consume_timeout: Duration::from_secs(1),
                    poll: Duration::from_millis(5),
                },
                || true,
                || panic!("消费记录已在盘上时不该抓屏"),
            )
            .unwrap_or_else(|e| panic!("head={head:?} 时应找得到消费记录: {e:#}"));

            assert_eq!(
                observed_boundary, boundary,
                "head={head:?} 现找出来的边界要与真实字节位置一致"
            );
            assert_eq!(
                terminal_reply(&trailing).as_deref(),
                Some("排完队才给的回复")
            );
            assert_eq!(checkpoint.end(), content.len() as u64);
        }
    }

    /// 长度对不等于内容对:等长(或更长)的**另一代**转录同样过得了长度证明,而边界右侧的
    /// 字节会被当成本轮回复喂进 parser、边界本身还会被登记给 mirror 仲裁器。所以边界左侧那段
    /// 必须含本轮那个随机 marker;不含就退回按 marker 重找,找到的是真边界而不是锚点那个。
    #[test]
    fn reattached_boundary_without_marker_falls_back_to_scanning() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("reattach-stale-boundary.jsonl");
        let marker = "summon-transport-turn:123e4567-e89b-12d3-a456-426614174014";
        let prompt = "边界指到了另一代的字节上";
        // 历史要比 checkpoint 窗口(1KiB)长,好让边界左侧那段**全是**与本轮无关的字节。
        let history = reattach_history().repeat(16);
        assert!(
            history.len() > 1024,
            "边界左侧必须够长,否则命中的是更早的记录"
        );
        let (content, boundary) = reattach_transcript(&history, prompt, marker, "重找之后的回复");
        std::fs::write(&path, &content).unwrap();

        let stale = history.len() as u64;
        assert_ne!(stale, boundary, "夹具本身要造出一个错的边界");

        let (trailing, observed_boundary, checkpoint) = locate_reattached_turn(
            &crate::runtime::executor::LocalExecutor,
            ReattachWait {
                path: &path,
                prompt_head: &prompt_head(prompt),
                marker,
                inject_offset: 0,
                consumed_boundary: Some(stale),
                consume_timeout: Duration::from_secs(1),
                poll: Duration::from_millis(5),
            },
            || true,
            || panic!("重找时消费记录就在盘上,不该抓屏"),
        )
        .expect("边界复验不过要退回重找,而不是拿着错边界接管");

        assert_eq!(
            observed_boundary, boundary,
            "复验不过时必须用重找出来的真边界,不能沿用锚点那个"
        );
        assert_eq!(terminal_reply(&trailing).as_deref(), Some("重找之后的回复"));
        assert_eq!(checkpoint.end(), content.len() as u64);
    }

    /// 锚点里的 head 与转录里那条消费记录的正文对不上 = 这一轮真的只进去了半条(marker 落在
    /// 正文末尾,所以「marker 在、开头不在」正是截断的形状)。必须当场失败,不能拿半条消息的
    /// 回复当本轮结果。这条路径的前提是 head **同源**:见
    /// `correlated_prompt_registers_marker_before_the_first_send_side_effect` 钉住的注入点。
    #[test]
    fn reattached_head_that_does_not_match_the_record_reports_truncation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("reattach-truncated.jsonl");
        let marker = "summon-transport-turn:123e4567-e89b-12d3-a456-426614174016";
        let history = reattach_history();
        let (content, _) =
            reattach_transcript(&history, "只进去了后半段", marker, "半条消息的回复");
        std::fs::write(&path, &content).unwrap();

        let error = locate_reattached_turn(
            &crate::runtime::executor::LocalExecutor,
            ReattachWait {
                path: &path,
                // 锚点冻的是完整正文的开头,转录里那条却没有它。
                prompt_head: &prompt_head("这是完整正文的开头,只进去了后半段"),
                marker,
                inject_offset: history.len() as u64,
                consumed_boundary: None,
                consume_timeout: Duration::from_secs(1),
                poll: Duration::from_millis(5),
            },
            || true,
            || panic!("已经认出截断,不必再问 pane"),
        )
        .expect_err("marker 在、正文开头不在 = 截断,不能当成接管成功");

        assert!(error.to_string().contains("截断"), "{error:#}");
        assert!(
            error.downcast_ref::<ReattachError>().is_none(),
            "截断不是接管的三类结局"
        );
    }

    /// 锚点带着消费边界 = 上一代亲眼见过这条 prompt 被消费。边界复验不过、退回重找也找不着时,
    /// 结局只能是超时或恢复卡 —— **绝不能**因为此刻 pane 空闲就判「未送达」:那条路接上重投点
    /// 就是同一句话跑两遍。
    #[test]
    fn reattached_stale_boundary_never_degrades_into_not_delivered() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("reattach-stale-no-marker.jsonl");
        // 转录里压根没有这个 marker(另一代的文件),但长度足够长,过得了边界的长度证明。
        let history = reattach_history().repeat(16);
        std::fs::write(&path, &history).unwrap();

        let error = locate_reattached_turn(
            &crate::runtime::executor::LocalExecutor,
            ReattachWait {
                path: &path,
                prompt_head: "",
                marker: "summon-transport-turn:123e4567-e89b-12d3-a456-426614174017",
                inject_offset: 0,
                consumed_boundary: Some(history.len() as u64),
                consume_timeout: Duration::from_millis(50),
                poll: Duration::from_millis(20),
            },
            || true,
            || pane_is_idle(REATTACH_IDLE_PANE),
        )
        .expect_err("marker 不在这份转录里,接不上");

        assert!(
            matches!(
                error.downcast_ref::<ReattachError>(),
                Some(ReattachError::ConsumeTimeout)
            ),
            "边界证明过「已消费」,此后永不许退化成可重投的 NotDelivered: {error:#}"
        );
    }

    /// 转录里没有 marker、pane 又回到空输入框:上一代的注入落了空,如实报「未送达」让上层重投。
    #[test]
    fn reattached_marker_missing_and_pane_idle_reports_not_delivered() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("reattach-lost.jsonl");
        let history = reattach_history();
        std::fs::write(&path, &history).unwrap();

        let error = locate_reattached_turn(
            &crate::runtime::executor::LocalExecutor,
            ReattachWait {
                path: &path,
                prompt_head: "",
                marker: "summon-transport-turn:123e4567-e89b-12d3-a456-426614174012",
                inject_offset: history.len() as u64,
                consumed_boundary: None,
                consume_timeout: Duration::from_secs(1),
                poll: Duration::from_millis(5),
            },
            || true,
            || pane_is_idle(REATTACH_IDLE_PANE),
        )
        .expect_err("找不到消费记录且 pane 空闲,不能当成接管成功");

        assert!(
            matches!(
                error.downcast_ref::<ReattachError>(),
                Some(ReattachError::NotDelivered)
            ),
            "调用方靠 downcast 分流,不许把类别藏进文案: {error:#}"
        );
    }

    /// 同样找不到 marker,但 pane 还在跑:消息可能正躺在队列里(实测最长 152.7s),
    /// 只能等到消费窗封顶,绝不能猜「没送到」——猜错就是同一句话跑两遍。
    #[test]
    fn reattached_marker_missing_and_pane_busy_waits_then_times_out() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("reattach-busy.jsonl");
        let history = reattach_history();
        std::fs::write(&path, &history).unwrap();

        let exec = CountingExec::new();
        let started = Instant::now();
        let error = locate_reattached_turn(
            &exec,
            ReattachWait {
                path: &path,
                prompt_head: "",
                marker: "summon-transport-turn:123e4567-e89b-12d3-a456-426614174013",
                inject_offset: history.len() as u64,
                consumed_boundary: None,
                consume_timeout: Duration::from_millis(50),
                poll: Duration::from_millis(20),
            },
            || true,
            || pane_is_idle(REATTACH_BUSY_PANE),
        )
        .expect_err("pane 忙且始终没有消费记录,只能超时");

        assert!(
            matches!(
                error.downcast_ref::<ReattachError>(),
                Some(ReattachError::ConsumeTimeout)
            ),
            "{error:#}"
        );
        assert!(
            started.elapsed() >= Duration::from_millis(50),
            "pane 忙时必须真等满消费窗,实际只等了 {:?}",
            started.elapsed()
        );
        assert_eq!(
            exec.range_reads.get(),
            1,
            "文件长度没变就没有新记录:整个消费窗只该重读并重解析一次,不是每 5ms 一次"
        );
    }

    /// 接管臂是**零注入**的:上一代已经把这条 prompt 送进 claude 了,再发一次就是同一句话跑
    /// 两遍。回调顺序与 `Correlated::Echoed` 同序 —— 先登记消费边界(mirror 据它判自己发没发过
    /// 这一轮),再仲裁输出权,最后才是用户可见的送达回调;接错顺序就是仲裁旁路 / 排队窗谎报。
    #[test]
    fn reattached_arm_claims_before_ack_and_never_sends() {
        let production = production_source();
        let start = production
            .find("TurnInjection::Reattached {")
            .expect("Reattached 臂");
        let body = &production[start..];
        let arm_end = body.find("let final_text = loop {").expect("主 poll 循环");
        let arm = &body[..arm_end];
        for forbidden in PANE_WRITE_PRIMITIVES {
            assert!(
                !arm.contains(forbidden),
                "接管臂不得往 pane 写任何东西: {forbidden}"
            );
        }
        // 判据存在不等于被用上:定位逻辑必须真在这条臂里被调用,否则四条判据全绿也管不着生产。
        assert!(
            arm.contains("locate_reattached_turn("),
            "接管臂必须走 locate_reattached_turn 定位消费记录"
        );
        let consumed = arm.find("on_marker_consumed(").expect("消费边界登记");
        let claim = arm.find("if !on_main_claim()").expect("输出权仲裁");
        let ack = arm.find("on_delivered();").expect("送达回调");
        assert!(consumed < claim, "消费边界必须在主路径仲裁前登记");
        assert!(claim < ack, "必须先仲裁输出权,再触发用户可见回调");
    }

    /// 多行 tmux 投喂可能出现「正文已提交、后续 graced Enter 报错」的 partial success。marker
    /// 必须先登记，失败路径才能 tombstone 迟到 terminal；等整个 send Ok 才登记会重新双发。
    #[test]
    fn correlated_prompt_registers_marker_before_the_first_send_side_effect() {
        let production = production_source();
        let start = production
            .find("fn send_correlated_prompt(")
            .expect("send_correlated_prompt 定义");
        let body = &production[start..];
        let register = body
            .find("on_marker_injected(&marker, inject_offset, &prompt_head(prompt));")
            .expect("marker 登记");
        let send = body
            .find("send_line_for_driver(&wire_prompt, InjectGate::PromptQueue)?;")
            .expect("首次可能产生副作用的 send");
        assert!(register < send, "marker 必须在任何部分成功可能之前登记");
    }

    #[test]
    fn mirror_ready_fence_is_after_checkpoint_and_eof_errors_fail_closed() {
        let production = production_source();
        let mirror = production
            .find("pub(crate) fn mirror_tail(")
            .expect("mirror_tail 定义");
        let body = &production[mirror..];
        assert!(
            body.contains("wait_for_stable_mirror_start("),
            "主动群出站必须用 fail-closed 稳定快照，不能把 file_len 错误猜成 EOF=0"
        );
        let stable = body
            .find("wait_for_stable_mirror_start(")
            .expect("稳定 start snapshot");
        let launch = body.find("tail_forward_turns(").expect("启动 tail");
        assert!(stable < launch, "冻结 snapshot 必须先于 tail 启动");

        let tail = production
            .find("fn tail_forward_turns<")
            .expect("tail_forward_turns 定义");
        let tail_body = &production[tail..];
        let prefetched = tail_body
            .find("checkpoint.read_new(exec, path)?")
            .expect("ready 前 exact checkpoint 复核");
        let ready = tail_body.find("on_ready();").expect("ready fence");
        let loop_start = tail_body.find("while !stop.load").expect("tail loop");
        assert!(
            prefetched < ready && ready < loop_start,
            "只有 checkpoint 复核并冻结 backlog 后才能放行首个 prompt"
        );
    }

    /// 步骤3:steer 的送达观测是**旁路**——排队窗口内不认送达,消费记录到了才认,且一次可以带
    /// 多条(同一个工具边界 FIFO 一次性出队 → N:1)。
    #[test]
    fn steer_watch_reports_delivery_only_at_the_consumption_record() {
        let mut watch = SteerWatch::default();
        watch.register("mk-a".into(), vec!["om_1".into()]);
        watch.register("mk-b".into(), vec!["om_2".into()]);
        // 入队 + 本轮仍在跑的输出:一条都不算送达。
        let queueing = concat!(
            r#"{"type":"queue-operation","operation":"enqueue","content":"<!-- mk-a -->"}"#,
            "\n",
            r#"{"type":"queue-operation","operation":"enqueue","content":"<!-- mk-b -->"}"#,
            "\n",
            r#"{"type":"assistant","message":{"role":"assistant","stop_reason":"tool_use","content":[{"type":"text","text":"本轮还在跑"}]}}"#,
            "\n",
        );
        assert!(
            watch.feed(queueing.as_bytes()).is_empty(),
            "排队窗口内不得认作送达"
        );
        // 同一个工具边界一次性出队两条 → 一次回调带两条 id。
        let dequeued = concat!(
            r#"{"attachment":{"type":"queued_command","prompt":"<!-- mk-a -->"},"type":"attachment"}"#,
            "\n",
            r#"{"attachment":{"type":"queued_command","prompt":"<!-- mk-b -->"},"type":"attachment"}"#,
            "\n",
        );
        assert_eq!(watch.feed(dequeued.as_bytes()), vec!["om_1", "om_2"]);
        // 已认过的不重复回调。
        assert!(watch.feed(dequeued.as_bytes()).is_empty());
    }

    #[test]
    fn steer_after_current_terminal_in_same_read_is_not_acknowledged_early() {
        let marker = "summon-transport-turn:123e4567-e89b-12d3-a456-426614174099";
        let current = rec("text", "A 完成", Some("end_turn"));
        let next_steer = serde_json::json!({
            "type": "attachment",
            "attachment": {"type": "queued_command", "prompt": marker}
        })
        .to_string()
            + "\n";
        let chunk = format!("{current}{next_steer}");

        let mut tail = TranscriptTail::default();
        let step = tail.feed(chunk.as_bytes(), &mut |_| {});
        assert!(matches!(step, TailStep::Done { .. }));
        let current_prefix = current_turn_input_prefix_len(&step, chunk.len(), tail.pending.len());
        assert_eq!(current_prefix, current.len());

        let mut watch = SteerWatch::default();
        watch.register(marker.into(), vec!["om_next".into()]);
        assert!(
            watch.feed(&chunk.as_bytes()[..current_prefix]).is_empty(),
            "A terminal 右侧的下一轮 queued_command 不能提前消费 WAL/贴 OnIt"
        );
        assert_eq!(
            watch.pending.len(),
            1,
            "未送达的 steer 必须继续留在 pending，交下一轮恢复"
        );
    }

    /// 无在飞 steer 时观测必须零成本:不缓冲任何字节(它与 tail 看同一份流,长轮里会白吃内存)。
    #[test]
    fn steer_watch_costs_nothing_without_pending_steers() {
        let mut watch = SteerWatch::default();
        assert!(watch.feed(b"{\"type\":\"assistant\"}\n").is_empty());
        assert!(watch.buf.is_empty(), "无在飞 steer 时不得缓冲");
    }

    /// steer 与 `!` 重跑的**本质区别**必须钉死在代码里:steer 不发 Esc、不重置 tail。
    /// 二者混淆的后果是把正在跑的这一轮抹掉(tail 一重置,已产出的流式内容就不再属于本轮)。
    #[test]
    fn steer_branch_neither_interrupts_nor_resets_the_running_turn() {
        let production = production_source();
        let start = production
            .find("Some(crate::capture::TurnInterrupt::Steer(steer)) => {")
            .expect("Steer 分支");
        let end = start
            + production[start..]
                .find("None => {}")
                .expect("Steer 分支的结束锚");
        let branch = &production[start..end];
        assert!(
            !branch.contains("interrupt_turn()"),
            "steer 不是打断:不得发 Esc"
        );
        assert!(
            !branch.contains("TranscriptTail::default()"),
            "steer 不得重置 tail —— 本轮已产出的内容仍属本轮"
        );
        assert!(
            !branch.contains("last_acc.clear()"),
            "steer 不得清掉已累积的流式全文"
        );
        assert!(
            branch.contains("inject_with_marker"),
            "steer 必须只注入、不等消费(等消费会把本轮输出丢进 gate)"
        );
    }

    /// 判据存在不等于被用上:删掉 `run_turn` 里的调用点,上面那条纯函数测试照样绿(实测),
    /// 而 `/quit` 会退回空转 240s。这里钉住调用点确实在 `run_turn` 内、且在抓屏探测那段里。
    #[test]
    fn run_turn_actually_consults_the_session_gone_check() {
        let production = production_source();
        let wait_start = production
            .find("fn wait_for_prompt_echo<")
            .expect("prompt echo 等待函数");
        let start = production.find("pub fn run_turn(").expect("run_turn 定义");
        assert!(
            production[wait_start..start].contains("ensure_expected_session_exit("),
            "等待 prompt echo 的会话消失必须区分显式退出与意外退出"
        );
        assert!(
            production[wait_start..start].contains("|| self.sess.tmux.has_session()"),
            "生产投递必须把真实 tmux 判活交给等待循环"
        );
        let body = &production[start..];
        let probe = body
            .find("if super::program_status::probe_due(")
            .expect("抓屏探测段");
        let watchdog = body[probe..]
            .find("// 既没识别出菜单")
            .expect("stall 看门狗收口");
        assert!(
            body[probe..probe + watchdog].contains("pane_proves_session_gone("),
            "抓屏探测段必须查会话是否已死,否则会话被 /quit 掉后要空转到 stall 才收尾"
        );
        assert!(
            body[probe..probe + watchdog].contains("ensure_expected_session_exit("),
            "marker 回显后的会话消失也必须保留意外退出错误"
        );
    }

    /// 实测事故:`/bridge` 把外部历史会话召回绑到旧 chat，裸 offset 恰好小于新 EOF，整段历史
    /// 被当停机 backlog。v3 必须同时绑 epoch + sid + boundary 前缀；旧格式和畸形指纹一律丢弃。
    #[test]
    fn mirror_offset_v3_binds_epoch_session_boundary_and_exact_checkpoint() {
        let offset = 103_857;
        let checkpoint = FileTailCheckpoint {
            start: offset - TAIL_CHECKPOINT_BYTES as u64,
            bytes: vec![0x5a; TAIL_CHECKPOINT_BYTES],
        };
        let encoded = format_mirror_offset(
            "epoch-a",
            "sid-a",
            offset,
            &checkpoint,
            MirrorOffsetBoundaryKind::TerminalBoundary,
        )
        .unwrap();
        let mine = parse_mirror_offset_record(&encoded).expect("合法 v3 应解析");
        assert_eq!(mine.epoch_fingerprint(), "epoch-a");
        assert_eq!(mine.sid(), "sid-a");
        assert_eq!(mine.offset, offset);
        assert_eq!(
            mine.boundary_kind(),
            MirrorOffsetBoundaryKind::TerminalBoundary
        );
        assert_eq!(mine.checkpoint, checkpoint);

        let ambiguous = format_mirror_offset(
            "epoch-a",
            "sid-a",
            offset,
            &checkpoint,
            MirrorOffsetBoundaryKind::AmbiguousBaseline,
        )
        .unwrap();
        let ambiguous = parse_mirror_offset_record(&ambiguous).expect("合法 ambiguous v3 应解析");
        assert_eq!(
            ambiguous.boundary_kind(),
            MirrorOffsetBoundaryKind::AmbiguousBaseline
        );
        assert!(
            !mirror_start_is_ambiguous(true, Some(&mine)),
            "验证过的 TerminalBoundary 不得被重写为 arbitrary baseline"
        );
        assert!(mirror_start_is_ambiguous(true, Some(&ambiguous)));
        assert!(mirror_start_is_ambiguous(false, Some(&mine)));
        assert!(mirror_start_is_ambiguous(false, None));

        for invalid in [
            "103857",
            "sid-a 103857",
            "v2 sid-a 1 00",
            "v2 sid-a 1 -",
            "v2 sid-a 1 0",
            "v2 sid-a 1 gg",
            "v2 sid-a 1 00 extra",
            "v2 sid-a 0 -",
            "v3 epoch-a sid-a 1 -",
            "v3 epoch-a sid-a 1 gg",
            "v3 epoch-a sid-a 1 00 extra",
            "v3 epoch-a sid-a 1 00",
            "v3 epoch-a sid-a 1 00 unknown",
            "v3 epoch-a sid-a 0 -",
            "",
        ] {
            assert_eq!(
                parse_mirror_offset_record(invalid),
                None,
                "坏/旧 offset 必须 fail-closed:{invalid}"
            );
        }
    }

    #[test]
    fn fresh_eof_only_becomes_terminal_baseline_after_a_real_end_turn() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("baseline.jsonl");
        let in_flight = rec("text", "还在跑", Some("tool_use"));
        std::fs::write(&path, &in_flight).unwrap();
        assert!(!transcript_eof_is_terminal_boundary(
            &crate::runtime::executor::LocalExecutor,
            &path,
            in_flight.len() as u64
        )
        .unwrap());

        let completed = format!("{in_flight}{}", rec("text", "完成", Some("end_turn")));
        std::fs::write(&path, &completed).unwrap();
        assert!(transcript_eof_is_terminal_boundary(
            &crate::runtime::executor::LocalExecutor,
            &path,
            completed.len() as u64
        )
        .unwrap());

        std::fs::write(&path, completed.trim_end()).unwrap();
        assert!(
            !transcript_eof_is_terminal_boundary(
                &crate::runtime::executor::LocalExecutor,
                &path,
                completed.trim_end().len() as u64
            )
            .unwrap(),
            "半写 JSONL 没有换行，不能证明稳定 terminal boundary"
        );
    }

    #[test]
    fn stable_mirror_start_rejects_stale_saved_boundary_without_file_identity() {
        use std::sync::atomic::{AtomicBool, Ordering};

        struct ReplaceAfterFirstLen {
            replacement: Vec<u8>,
            replaced: AtomicBool,
        }

        impl Executor for ReplaceAfterFirstLen {
            fn spawn(
                &self,
                _spec: &crate::runtime::executor::SpawnSpec,
            ) -> anyhow::Result<std::process::Child> {
                unreachable!("本测试不 spawn")
            }

            fn read_file_tail(
                &self,
                path: &std::path::Path,
                from_byte: u64,
            ) -> anyhow::Result<Vec<u8>> {
                use std::io::{Read as _, Seek as _};
                let mut file = std::fs::File::open(path)?;
                file.seek(std::io::SeekFrom::Start(from_byte))?;
                let mut out = Vec::new();
                file.read_to_end(&mut out)?;
                Ok(out)
            }

            fn file_len(&self, path: &std::path::Path) -> anyhow::Result<u64> {
                let len = std::fs::metadata(path)?.len();
                if !self.replaced.swap(true, Ordering::SeqCst) {
                    std::fs::write(path, &self.replacement)?;
                }
                Ok(len)
            }

            fn tmux(&self, _args: &[&str]) -> anyhow::Result<String> {
                unreachable!("本测试不碰 tmux")
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("replace-between-len-and-capture.jsonl");
        let original = b"original-boundary\n";
        std::fs::write(&path, original).unwrap();
        let original_checkpoint = FileTailCheckpoint::from_read(0, original);
        let record = MirrorOffsetRecord {
            epoch_fingerprint: "epoch".into(),
            sid: "sid".into(),
            offset: original.len() as u64,
            checkpoint: original_checkpoint,
            boundary_kind: MirrorOffsetBoundaryKind::TerminalBoundary,
        };
        let replacement = b"different-generation-and-longer-backlog\n".to_vec();
        let exec = ReplaceAfterFirstLen {
            replacement: replacement.clone(),
            replaced: AtomicBool::new(false),
        };

        let (start, eof, _, used_saved) = stable_mirror_start(&exec, &path, Some(&record)).unwrap();
        assert_eq!(start, replacement.len() as u64);
        assert_eq!(eof, replacement.len() as u64);
        assert_ne!(start, record.offset, "stale saved offset 绝不能被复用");
        assert!(!used_saved);
    }

    #[test]
    fn mirror_offset_exact_eof_requires_both_length_and_checkpoint_identity() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("exact-eof.jsonl");
        let original = b"original generation\n";
        std::fs::write(&path, original).unwrap();
        let record = MirrorOffsetRecord {
            epoch_fingerprint: "epoch".into(),
            sid: "sid".into(),
            offset: original.len() as u64,
            checkpoint: FileTailCheckpoint::from_read(0, original),
            boundary_kind: MirrorOffsetBoundaryKind::TerminalBoundary,
        };
        let _ = &record; // 记录本身仍是 fence 的另一半(offset 原文 exact 比对),这里只验分界线。
        let (stop_eof, stop_cp) = capture_mirror_eof_fence(&path).unwrap();
        assert_eq!(stop_eof, original.len() as u64);
        assert!(mirror_eof_fence_still_valid(&path, stop_eof, &stop_cp).unwrap());

        // 分界线右侧新增字节 —— 这正是「停机窗口产物」的形态,验真必须仍然成立,否则该补的补不了。
        std::fs::write(&path, [original.as_slice(), b"downtime turn\n"].concat()).unwrap();
        assert!(
            mirror_eof_fence_still_valid(&path, stop_eof, &stop_cp).unwrap(),
            "append 之后分界线依然成立,补发才有依据"
        );

        // 同长度 replacement:字节数对得上、内容不是同一份 → 必须拒。
        std::fs::write(&path, vec![b'x'; original.len()]).unwrap();
        assert!(
            !mirror_eof_fence_still_valid(&path, stop_eof, &stop_cp).unwrap(),
            "同长度 replacement 不能只凭 offset 冒充停机 EOF fence"
        );

        // 文件变短 = 换代/truncate,不是同一份文件的延续。
        std::fs::write(&path, b"tiny").unwrap();
        assert!(
            !mirror_eof_fence_still_valid(&path, stop_eof, &stop_cp).unwrap(),
            "EOF 回退说明换了 generation,必须拒绝"
        );

        // 空/坏输入一律拒(旧格式 fence 的 stop_eof=0 走的就是这条)。
        std::fs::write(&path, original).unwrap();
        assert!(!mirror_eof_fence_still_valid(&path, 0, &stop_cp).unwrap());
        assert!(!mirror_eof_fence_still_valid(&path, stop_eof, "").unwrap());
        assert!(!mirror_eof_fence_still_valid(&path, stop_eof, "!!!not-base64").unwrap());
    }

    #[test]
    fn stable_mirror_start_rejects_an_empty_unprovable_boundary() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.jsonl");
        std::fs::write(&path, "").unwrap();

        let error = stable_mirror_start(&crate::runtime::executor::LocalExecutor, &path, None)
            .expect_err("空 checkpoint 对任意 replacement 都 starts_with，不能宣布 ready");
        assert!(error.to_string().contains("空 transcript"), "{error:#}");
    }

    #[test]
    fn mirror_waits_for_cold_empty_transcript_then_observes_ambiguous_first_terminal() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::{Arc, Mutex};

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cold-empty-then-live.jsonl");
        std::fs::write(&path, "").unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let observed = Arc::new(Mutex::new(Vec::<(MirrorTurnKind, String)>::new()));
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (worker_path, worker_stop, worker_observed) =
            (path.clone(), Arc::clone(&stop), Arc::clone(&observed));
        let worker = std::thread::spawn(move || {
            let Some((start, _, checkpoint, _)) = wait_for_stable_mirror_start(
                &crate::runtime::executor::LocalExecutor,
                &mut worker_path.clone(),
                None,
                None,
                &worker_stop,
                Duration::from_secs(1),
                &mut || {},
            )
            .unwrap() else {
                return;
            };
            tail_forward_turns(
                &crate::runtime::executor::LocalExecutor,
                &worker_path,
                start,
                checkpoint,
                Duration::from_millis(5),
                Duration::from_millis(10),
                &worker_stop,
                None,
                true,
                None, // catchup_floor:本测不验停机补发
                &mut |kind, _, _, _, text| {
                    worker_observed
                        .lock()
                        .unwrap()
                        .push((kind, text.to_string()));
                    Ok(())
                },
                &mut |_, _| Ok(()),
                &mut || ready_tx.send(()).unwrap(),
            )
            .unwrap();
        });

        assert!(
            ready_rx.recv_timeout(Duration::from_millis(50)).is_err(),
            "0B transcript 还没有可验证 checkpoint，不得提前宣布 Ready"
        );
        let baseline = serde_json::json!({
            "type": "system",
            "message": {"role": "system", "content": "cold start"}
        })
        .to_string()
            + "\n";
        std::fs::write(&path, baseline).unwrap();
        ready_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("非空稳定 checkpoint 建立后应宣布 Ready");

        let answer = serde_json::json!({
            "type": "assistant",
            "message": {"role": "assistant", "stop_reason": "end_turn",
                "content": [{"type": "text", "text": "冷启后的答案"}]}
        })
        .to_string()
            + "\n";
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        use std::io::Write as _;
        file.write_all(answer.as_bytes()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while observed.lock().unwrap().is_empty() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        let live_answer = serde_json::json!({
            "type": "assistant",
            "message": {"role": "assistant", "stop_reason": "end_turn",
                "content": [{"type": "text", "text": "下一轮 live attach"}]}
        })
        .to_string()
            + "\n";
        file.write_all(live_answer.as_bytes()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while observed.lock().unwrap().len() < 2 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        stop.store(true, Ordering::Relaxed);
        worker.join().unwrap();
        assert_eq!(
            observed.lock().unwrap().as_slice(),
            [
                (
                    MirrorTurnKind::HistoricalObserved,
                    "冷启后的答案".to_string()
                ),
                (MirrorTurnKind::Reply, "下一轮 live attach".to_string())
            ],
            "fresh arbitrary EOF 可能切中旧轮：首 terminal 只收敛，提交后的 live turn 才可联网"
        );
    }

    #[test]
    fn mirror_waits_for_cold_missing_transcript_within_the_same_bound() {
        use std::sync::atomic::AtomicBool;
        use std::sync::Arc;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("not-created-yet.jsonl");
        let stop = Arc::new(AtomicBool::new(false));
        let (result_tx, result_rx) = std::sync::mpsc::channel();
        let (worker_path, worker_stop) = (path.clone(), Arc::clone(&stop));
        let worker = std::thread::spawn(move || {
            result_tx
                .send(wait_for_stable_mirror_start(
                    &crate::runtime::executor::LocalExecutor,
                    &mut worker_path.clone(),
                    None,
                    None,
                    &worker_stop,
                    Duration::from_secs(1),
                    &mut || {},
                ))
                .unwrap();
        });

        assert!(
            result_rx.recv_timeout(Duration::from_millis(50)).is_err(),
            "cold-start NotFound 与 0B 一样只是有界过渡态，不能立即标 Failed"
        );
        let baseline = b"{\"type\":\"system\"}\n";
        std::fs::write(&path, baseline).unwrap();
        let Some((start, eof, checkpoint, _)) = result_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("文件出现后应建立稳定快照")
            .unwrap()
        else {
            panic!("未请求 stop，不应提前退出");
        };
        worker.join().unwrap();
        assert_eq!(start, baseline.len() as u64);
        assert_eq!(eof, baseline.len() as u64);
        assert_eq!(checkpoint.end(), eof);
    }

    #[test]
    fn mirror_empty_start_bypasses_main_fence_but_keeps_watcher_alive() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("permanently-empty.jsonl");
        std::fs::write(&path, "").unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let worker_path = path.clone();
        let (bypass_tx, bypass_rx) = std::sync::mpsc::channel();
        let started = Instant::now();
        let worker = std::thread::spawn(move || {
            wait_for_stable_mirror_start(
                &crate::runtime::executor::LocalExecutor,
                &mut worker_path.clone(),
                None,
                None,
                &worker_stop,
                Duration::from_millis(40),
                &mut || bypass_tx.send(()).unwrap(),
            )
            .unwrap()
        });

        bypass_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("首行依赖首 prompt 时必须先放开主路径 fence");
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "空 transcript 必须在主路径 10s ready wait 前 bypass"
        );
        assert!(
            !worker.is_finished(),
            "bypass 后 watcher 必须继续等待首行，不能永久关闭"
        );
        stop.store(true, Ordering::Relaxed);
        assert!(worker.join().unwrap().is_none());
    }

    #[test]
    fn mirror_freezes_boundary_checkpoint_before_delivery_side_effect() {
        use std::sync::atomic::AtomicBool;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("boundary-before-send.jsonl");
        let original = serde_json::json!({
            "type": "assistant",
            "message": {"role": "assistant", "stop_reason": "end_turn",
                "content": [{"type": "text", "text": "old generation answer"}]}
        })
        .to_string()
            + "\n";
        std::fs::write(&path, &original).unwrap();
        let expected = FileTailCheckpoint::from_read(0, original.as_bytes());
        let replacement = "x".repeat(original.len());
        let path_for_send = path.clone();
        let mut on_turn =
            move |_: MirrorTurnKind, _: &MirrorMarkerEvidence, _: u64, _: &[u8], _: &str| {
                std::fs::write(&path_for_send, &replacement).unwrap();
                Ok(())
            };
        let mut committed = None;
        let mut on_offset = |_: u64, checkpoint: &FileTailCheckpoint| {
            committed = Some(checkpoint.clone());
            Ok(())
        };
        let error = tail_forward_turns(
            &crate::runtime::executor::LocalExecutor,
            &path,
            0,
            FileTailCheckpoint::capture_at(&crate::runtime::executor::LocalExecutor, &path, 0)
                .unwrap(),
            Duration::from_millis(1),
            Duration::from_millis(1),
            &AtomicBool::new(false),
            None,
            false,
            None, // catchup_floor:本测不验停机补发
            &mut on_turn,
            &mut on_offset,
            &mut || {},
        )
        .expect_err("发送后换代必须让 watcher fail-closed");
        assert!(error.to_string().contains("truncate/replace"), "{error:#}");
        assert_eq!(
            committed,
            Some(expected),
            "持久化 anchor 必须来自发送前已解析的旧 generation，不能事后重读新文件"
        );
    }

    /// 续跑区间里 mirror 拿主路径报的 terminal 边界和自己的 terminal_offset 比大小,两边对同一个
    /// terminal 必须得出同一个数。主路径从 marker 记录之后读起、mirror 从文件头读起,读到的
    /// chunk 里还带着下一轮的半行。
    #[test]
    fn main_and_mirror_report_the_same_terminal_boundary() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let prompt = serde_json::json!({
            "type": "user",
            "message": {"role": "user", "content": "跑一下全量"}
        })
        .to_string()
            + "\n";
        let answer = serde_json::json!({
            "type": "assistant",
            "message": {"role": "assistant", "stop_reason": "end_turn",
                "content": [{"type": "text", "text": "全量在后台跑,好了告诉你"}]}
        })
        .to_string()
            + "\n";
        let next_turn_head = r#"{"type":"user","message":{"role":"user","#;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("boundary.jsonl");
        std::fs::write(&path, format!("{prompt}{answer}{next_turn_head}")).unwrap();

        let stop = AtomicBool::new(false);
        let mut mirror_boundary = None;
        let mut on_turn =
            |_: MirrorTurnKind, _: &MirrorMarkerEvidence, boundary: u64, _: &[u8], _: &str| {
                mirror_boundary.get_or_insert(boundary);
                stop.store(true, Ordering::Relaxed);
                Ok(())
            };
        tail_forward_turns(
            &crate::runtime::executor::LocalExecutor,
            &path,
            0,
            FileTailCheckpoint::capture_at(&crate::runtime::executor::LocalExecutor, &path, 0)
                .unwrap(),
            Duration::from_millis(1),
            Duration::from_millis(1),
            &stop,
            None,
            false,
            None,
            &mut on_turn,
            &mut |_, _| Ok(()),
            &mut || {},
        )
        .unwrap();

        let bytes = std::fs::read(&path).unwrap();
        let main_start = prompt.len();
        let chunk = &bytes[main_start..];
        let checkpoint = FileTailCheckpoint::from_read(main_start as u64, chunk);
        let mut tail = TranscriptTail::default();
        let step = tail.feed(chunk, &mut |_| {});
        assert!(matches!(step, TailStep::Done { .. }));
        let expected = (prompt.len() + answer.len()) as u64;
        assert_eq!(terminal_boundary(&checkpoint, &tail), expected);
        assert_eq!(mirror_boundary, Some(expected));
    }

    #[test]
    fn mirror_prefetches_before_ready_and_fails_on_ready_time_replacement() {
        use std::sync::atomic::AtomicBool;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ready-replacement.jsonl");
        std::fs::write(&path, b"non-empty-stable-prefix\n").unwrap();
        let start = std::fs::metadata(&path).unwrap().len();
        let checkpoint =
            FileTailCheckpoint::capture_at(&crate::runtime::executor::LocalExecutor, &path, start)
                .unwrap();
        let path_for_ready = path.clone();
        let mut turns = Vec::new();
        let error = tail_forward_turns(
            &crate::runtime::executor::LocalExecutor,
            &path,
            start,
            checkpoint,
            Duration::from_millis(1),
            Duration::from_millis(1),
            &AtomicBool::new(false),
            None,
            false,
            None, // catchup_floor:本测不验停机补发
            &mut |_, _, _, _, text| {
                turns.push(text.to_string());
                Ok(())
            },
            &mut |_, _| Ok(()),
            &mut || {
                std::fs::write(&path_for_ready, b"replacement-after-ready\n").unwrap();
            },
        )
        .expect_err("ready 回调期间换代必须在任何转发前 fail-closed");
        assert!(error.to_string().contains("truncate/replace"), "{error:#}");
        assert!(turns.is_empty());
    }

    /// 主路径的通知卡靠 `drain_notices` 实时刷,所以跨会话消息必须和后台任务通知**一样**
    /// 进得了这条队列 —— 只进镜像那份窗口的话,飞书里发起的轮看不见任何 SendMessage 收发。
    #[test]
    fn main_path_drains_peer_notices_too() {
        use summon_core::turn_notice::{PeerDirection, TurnNotice};
        let inbound = r#"{"type":"user","message":{"role":"user","content":"Another Claude session sent a message: <cross-session-message from=\"uds:/x\" from-name=\"peer-a\">帮我看看</cross-session-message>"}}"#;
        let send = r#"{"type":"assistant","sessionId":"s","message":{"role":"assistant","stop_reason":"tool_use","content":[{"type":"tool_use","id":"t1","name":"SendMessage","input":{"to":"peer-b","summary":"回执","message":"内容"}}]}}"#;
        let mut tail = TranscriptTail::default();
        tail.feed(&format!("{inbound}\n{send}\n"), &mut |_| {});
        assert_eq!(
            tail.drain_notices(),
            vec![
                TurnNotice::peer(PeerDirection::In, "peer-a", "帮我看看").expect("非空"),
                TurnNotice::peer(PeerDirection::Out, "peer-b", "回执").expect("非空"),
            ],
            "入向与出向都要交给主路径"
        );
        assert!(tail.drain_notices().is_empty(), "取走即清空");
    }

    /// 镜像那条路从不 `drain_notices`,所以它收口时必须把主路径那份队列一并清掉 —— 否则队列
    /// 随会话一路涨到 watcher 退出(一轮二十条通知、跑五十轮就是上千条挂在内存里)。
    #[test]
    fn mirror_collection_does_not_pile_up_the_main_path_queue() {
        let notice = |summary: &str| {
            serde_json::json!({
                "type": "queue-operation",
                "operation": "enqueue",
                "content": format!("<task-notification><summary>{summary}</summary></task-notification>")
            })
            .to_string()
                + "\n"
        };
        let mut tail = TranscriptTail::default();
        for turn in 1..=3 {
            for n in 1..=4 {
                tail.feed(
                    notice(&format!("轮 {turn} 任务 {n}")).as_bytes(),
                    &mut |_| {},
                );
            }
            assert!(tail.take_notice_block().is_some(), "第 {turn} 轮应有事件块");
            tail.reset_turn();
            assert!(
                tail.pending_notices.is_empty(),
                "镜像收口后不该留下没人取的事件:{:?}",
                tail.pending_notices
            );
        }
    }

    /// 事件与 end_turn 落在同一个 chunk 里时,主路径仍要拿到它 —— `reset_turn` 紧跟着终态跑,
    /// 它要是把待取队列一起清了,这条事件就在「轮刚好收尾」那一刻静默丢掉。
    #[test]
    fn a_notice_arriving_with_the_terminal_is_not_lost_by_reset_turn() {
        let notice = serde_json::json!({
            "type": "queue-operation",
            "operation": "enqueue",
            "content": "<task-notification><summary>压哨完成</summary></task-notification>"
        })
        .to_string()
            + "\n";
        let done = r#"{"type":"assistant","sessionId":"s","message":{"role":"assistant","stop_reason":"end_turn","content":[{"type":"text","text":"收到"}]}}"#;
        let mut tail = TranscriptTail::default();
        match tail.feed(format!("{notice}{done}\n").as_bytes(), &mut |_| {}) {
            TailStep::Done { .. } => {}
            other => panic!("end_turn 应收尾,实得 {other:?}"),
        }
        tail.reset_turn();
        assert_eq!(
            tail.drain_notices(),
            vec![summon_core::turn_notice::TurnNotice::task("压哨完成").expect("非空")],
            "reset_turn 不得清掉还没交给主路径的事件"
        );
    }

    /// 一个 chunk 里来两条事件时,两条都要到主路径手里:队列换成单槽的话先到的那条会被顶掉,
    /// 卡上就少一行 —— 而 workflow 轮里几条通知挤在同一次读里是常态。
    #[test]
    fn several_notices_in_one_chunk_all_reach_the_main_path() {
        let notice = |summary: &str| {
            serde_json::json!({
                "type": "queue-operation",
                "operation": "enqueue",
                "content": format!("<task-notification><summary>{summary}</summary></task-notification>")
            })
            .to_string()
                + "\n"
        };
        let mut tail = TranscriptTail::default();
        tail.feed(
            format!("{}{}", notice("任务 1 完成"), notice("任务 2 完成")).as_bytes(),
            &mut |_| {},
        );
        assert_eq!(tail.drain_notices().len(), 2, "同一拍的两条都要取到");
    }

    /// 镜像路径整轮只转发**一块**:跨会话消息与后台任务通知按到达顺序攒进同一个滚动窗口,
    /// 轮收口时一起取走;取走即清空,`reset_turn` 也清。
    #[test]
    fn mirror_forwards_a_turns_notices_as_one_block() {
        let mut tail = TranscriptTail::default();
        let notice = |summary: &str| {
            serde_json::json!({
                "type": "queue-operation",
                "operation": "enqueue",
                "content": format!("<task-notification><summary>{summary}</summary></task-notification>")
            })
            .to_string()
                + "\n"
        };
        let peer = serde_json::json!({
            "type": "user",
            "message": {
                "role": "user",
                "content": "<cross-session-message from=\"peer-a\">帮我看看</cross-session-message>"
            }
        })
        .to_string()
            + "\n";
        for n in 1..=4 {
            tail.feed(notice(&format!("任务 {n} 完成")).as_bytes(), &mut |_| {});
        }
        tail.feed(peer.as_bytes(), &mut |_| {});
        let card = tail.take_notice_block().expect("本轮有事件");
        assert!(
            card.starts_with("**通知** · 跨会话 1 条 · 后台任务 4 个(显示最近 3 条)"),
            "只有被裁的那一类带括注:{card}"
        );
        assert!(!card.contains("任务 1 完成"), "窗口外的要滚掉:{card}");
        assert!(card.contains("✅ 任务 4 完成"), "{card}");
        assert!(
            card.contains("⇄ 来自 `peer-a`:帮我看看"),
            "跨会话消息与通知同卡混排:{card}"
        );
        assert!(tail.take_notice_block().is_none(), "取走即清空");

        tail.feed(notice("下一轮的任务").as_bytes(), &mut |_| {});
        tail.reset_turn();
        assert!(
            tail.take_notice_block().is_none(),
            "reset_turn 要连事件窗口一起清"
        );
    }

    // ── 后台任务:claude 2.1.301 真机转录形状(正文截短、id 脱敏) ──
    const SHELL_BG_CALL: &str = r#"{"type":"assistant","isSidechain":false,"message":{"role":"assistant","content":[{"type":"tool_use","id":"tu_shell","name":"Bash","input":{"command":"cargo nextest run","run_in_background":true}}],"stop_reason":"tool_use"}}"#;
    const SHELL_BG_STARTED: &str = r#"{"type":"user","isSidechain":false,"message":{"role":"user","content":[{"tool_use_id":"tu_shell","type":"tool_result","content":"Command running in background with ID: bfyy2gdqz. Output is being written to: /tmp/bfyy2gdqz.output.","is_error":false}]},"toolUseResult":{"stdout":"","stderr":"","interrupted":false,"isImage":false,"noOutputExpected":false,"backgroundTaskId":"bfyy2gdqz"}}"#;
    const SHELL_MOVED_TO_BACKGROUND: &str = r#"{"type":"user","isSidechain":false,"message":{"role":"user","content":[{"tool_use_id":"tu_slow","type":"tool_result","content":"Command did not complete within its 600s timeout and was moved to the background (ID: b2be82guw).","is_error":false}]},"toolUseResult":{"stdout":"","stderr":"","interrupted":false,"isImage":false,"noOutputExpected":false,"backgroundTaskId":"b2be82guw","timedOutAfterMs":600000}}"#;
    const SHELL_BG_FAILED: &str = r#"{"type":"user","isSidechain":false,"message":{"role":"user","content":[{"tool_use_id":"tu_shell","type":"tool_result","content":"Permission denied","is_error":true}]},"toolUseResult":"Error: Permission denied"}"#;
    const AGENT_BG_CALL: &str = r#"{"type":"assistant","isSidechain":false,"message":{"role":"assistant","content":[{"type":"tool_use","id":"tu_agent","name":"Agent","input":{"description":"审一下","subagent_type":"adversarial-reviewer","prompt":"p","run_in_background":"true"}}],"stop_reason":"tool_use"}}"#;
    const AGENT_BG_LAUNCHED: &str = r#"{"type":"user","isSidechain":false,"message":{"role":"user","content":[{"tool_use_id":"tu_agent","type":"tool_result","content":[{"type":"text","text":"Async agent launched successfully.\nagentId: a1c7b6848c7434fad (internal ID - do not mention to user.)"}]}]},"toolUseResult":{"isAsync":true,"status":"async_launched","agentId":"a1c7b6848c7434fad","description":"审一下","resolvedModel":"claude-opus-5-5","prompt":"p","outputFile":"/tmp/a.output","canReadOutputFile":false}}"#;
    const WAIT_END: &str = r#"{"type":"assistant","isSidechain":false,"message":{"role":"assistant","content":[{"type":"text","text":"派出去了,等它回来"}],"stop_reason":"end_turn"}}"#;
    const AGENT_RESUME_CALL: &str = r#"{"type":"assistant","isSidechain":false,"message":{"role":"assistant","content":[{"type":"tool_use","id":"tu_send","name":"SendMessage","input":{"to":"a1c7b6848c7434fad","summary":"补一个问题","message":"m"}}],"stop_reason":"tool_use"}}"#;
    const AGENT_RESUMED: &str = r#"{"type":"user","isSidechain":false,"message":{"role":"user","content":[{"tool_use_id":"tu_send","type":"tool_result","content":[{"type":"text","text":"{\"success\":true,\"message\":\"Resuming agent a1c7b68\",\"resumedAgentId\":\"a1c7b6848c7434fad\"}"}]}]},"toolUseResult":{"success":true,"message":"Resuming agent a1c7b68","resumedAgentId":"a1c7b6848c7434fad","pin":{"id":"a1c7b6848c7434fad","name":"a1c7b6848c7434fad","ref":"3d8ba1"}}}"#;
    const MESSAGE_QUEUED: &str = r#"{"type":"user","isSidechain":false,"message":{"role":"user","content":[{"tool_use_id":"tu_send","type":"tool_result","content":[{"type":"text","text":"Message queued for delivery to reviewer at its next tool round."}]}]},"toolUseResult":{"success":true,"message":"Message queued for delivery to reviewer at its next tool round.","pin":{"id":"teammate-reviewer"}}}"#;
    const WORKFLOW_LAUNCHED: &str = r#"{"type":"user","isSidechain":false,"message":{"role":"user","content":[{"tool_use_id":"tu_wf","type":"tool_result","content":[{"type":"text","text":"Workflow launched in background."}]}]},"toolUseResult":{"status":"async_launched","taskType":"local_workflow","taskId":"wco3l6qra","runId":"wf_r1","workflowName":"review-changes"}}"#;
    const AGENTS_KILLED: &str = r#"{"parentUuid":"p","isSidechain":false,"type":"system","subtype":"agents_killed","timestamp":"2026-10-16T12:04:27.227Z","uuid":"u","isMeta":false,"userType":"external","entrypoint":"cli","sessionId":"s","version":"2.1.301"}"#;
    const MONITOR_EVENT: &str = r#"{"type":"queue-operation","operation":"enqueue","content":"<task-notification>\n<task-id>b0000mon1</task-id>\n<summary>Monitor event: \"build key steps\"</summary>\n<event>EXECUTE_BUILD_STEP start</event>\n</task-notification>"}"#;

    fn task_notification(task_id: &str, tool_use_id: &str, summary: &str) -> String {
        let content = format!(
            "<task-notification>\n<task-id>{task_id}</task-id>\n<tool-use-id>{tool_use_id}</tool-use-id>\n<output-file>/tmp/{task_id}.output</output-file>\n<status>completed</status>\n<summary>{summary}</summary>\n</task-notification>"
        );
        serde_json::json!({"type": "queue-operation", "operation": "enqueue", "content": content})
            .to_string()
            + "\n"
    }

    fn lines(rows: &[&str]) -> String {
        rows.iter().map(|row| format!("{row}\n")).collect()
    }

    fn shell_done() -> String {
        task_notification(
            "bfyy2gdqz",
            "tu_shell",
            "Background command \"cargo nextest run\" completed (exit code 0)",
        )
    }

    fn agent_done(tool_use_id: &str) -> String {
        task_notification(
            "a1c7b6848c7434fad",
            tool_use_id,
            "Agent \"审一下\" finished",
        )
    }

    /// 后台命令的完成通知**跨轮**到达:起后台命令的那个 end_turn 时它还 pending,`reset_turn`
    /// 不清;通知按 task-id 销账、事件落队,`drain_notices` 取走。这是主路径跨轮续跑(同一张
    /// 流式卡等后台任务)的基础。
    #[test]
    fn tail_keeps_a_background_shell_pending_across_turn_reset() {
        let mut tail = TranscriptTail::default();
        let turn = lines(&[SHELL_BG_CALL, SHELL_BG_STARTED, WAIT_END]);
        match tail.feed(turn.as_bytes(), &mut |_| {}) {
            TailStep::Done { .. } => {}
            other => panic!("end_turn 应收尾,实得 {other:?}"),
        }
        assert_eq!(tail.pending_background_count(), 1);
        tail.reset_turn();
        assert!(tail.has_pending_background(), "reset_turn 必须保留");
        assert!(tail.drain_notices().is_empty(), "通知未到,drain 应为空");

        match tail.feed(shell_done().as_bytes(), &mut |_| {}) {
            TailStep::Progress { .. } => {}
            other => panic!("task-notification 不应触发收尾,实得 {other:?}"),
        }
        assert!(!tail.has_pending_background(), "通知到达应销账");
        assert_eq!(
            tail.drain_notices(),
            vec![summon_core::turn_notice::TurnNotice::task(
                "Background command \"cargo nextest run\" completed (exit code 0)"
            )
            .expect("非空")],
        );
        assert_eq!(tail.notices.total(), 1, "mirror 转发的那份进滚动窗口");

        // 同内容的 remove 销账行不再落事件。
        let remove = serde_json::json!({
            "type": "queue-operation",
            "operation": "remove",
            "content": "<task-notification><task-id>bfyy2gdqz</task-id><summary>Background command \"cargo nextest run\" completed (exit code 0)</summary></task-notification>"
        })
        .to_string()
            + "\n";
        let _ = tail.feed(remove.as_bytes(), &mut |_| {});
        assert!(
            tail.drain_notices().is_empty(),
            "remove 是销账行,不该再落一条事件"
        );
        assert!(!tail.has_pending_background());
    }

    fn user_row_notice(task_id: &str) -> String {
        let content = format!(
            "<task-notification>\n<task-id>{task_id}</task-id>\n<status>completed</status>\n<summary>Background command \"x\" completed (exit code 0)</summary>\n</task-notification>"
        );
        serde_json::json!({"type": "user", "message": {"role": "user", "content": content}})
            .to_string()
            + "\n"
    }

    fn queued_attachment_notice(task_id: &str, tool_use_id: &str) -> String {
        let prompt = format!(
            "<task-notification>\n<task-id>{task_id}</task-id>\n<tool-use-id>{tool_use_id}</tool-use-id>\n<status>completed</status>\n<summary>Agent \"审一下\" finished</summary>\n</task-notification>"
        );
        serde_json::json!({
            "isSidechain": false,
            "type": "attachment",
            "attachment": {
                "type": "queued_command",
                "prompt": prompt,
                "commandMode": "task-notification",
                "origin": {"kind": "task-notification"}
            }
        })
        .to_string()
            + "\n"
    }

    /// 通知的三种落盘形态都能销账:`queue-operation` enqueue(见上)、旧版 user 行、以及主 agent 忙时
    /// 只在被消费那一刻落下的 `queued_command` 附件。
    #[test]
    fn every_notice_form_settles_its_task() {
        let mut tail = TranscriptTail::default();
        let _ = tail.feed(lines(&[SHELL_BG_STARTED]).as_bytes(), &mut |_| {});
        let _ = tail.feed(user_row_notice("bfyy2gdqz").as_bytes(), &mut |_| {});
        assert!(!tail.has_pending_background(), "旧版 user 行");

        let _ = tail.feed(lines(&[AGENT_BG_LAUNCHED]).as_bytes(), &mut |_| {});
        let attachment = queued_attachment_notice("a1c7b6848c7434fad", "tu_agent");
        let _ = tail.feed(attachment.as_bytes(), &mut |_| {});
        assert!(!tail.has_pending_background(), "queued_command 附件");
    }

    /// 同一条通知先后以几种形态落盘,之后再续用这个 agent:后到的重复形态不能把续用的登记吞掉。
    #[test]
    fn duplicate_notice_forms_do_not_swallow_a_later_resume() {
        let mut tail = TranscriptTail::default();
        let _ = tail.feed(
            lines(&[AGENT_BG_CALL, AGENT_BG_LAUNCHED]).as_bytes(),
            &mut |_| {},
        );
        let duplicates = format!(
            "{}{}",
            agent_done("tu_agent"),
            queued_attachment_notice("a1c7b6848c7434fad", "tu_agent")
        );
        let _ = tail.feed(duplicates.as_bytes(), &mut |_| {});
        assert!(!tail.has_pending_background());

        let _ = tail.feed(
            lines(&[AGENT_RESUME_CALL, AGENT_RESUMED]).as_bytes(),
            &mut |_| {},
        );
        assert!(tail.has_pending_background(), "续用应登记");
        let _ = tail.feed(agent_done("tu_send").as_bytes(), &mut |_| {});
        assert!(!tail.has_pending_background());
    }

    /// 续用的 agent 是新的一程:上一程那条通知后到的重复形态销不掉这一程;这一程的通知若先于
    /// 续用回执落盘,回执到时也不再登记。
    #[test]
    fn a_resumed_run_is_told_apart_from_the_previous_run_by_its_call() {
        let mut tail = TranscriptTail::default();
        let _ = tail.feed(
            lines(&[AGENT_BG_CALL, AGENT_BG_LAUNCHED]).as_bytes(),
            &mut |_| {},
        );
        let _ = tail.feed(agent_done("tu_agent").as_bytes(), &mut |_| {});
        let _ = tail.feed(
            lines(&[AGENT_RESUME_CALL, AGENT_RESUMED]).as_bytes(),
            &mut |_| {},
        );
        let late_duplicate = queued_attachment_notice("a1c7b6848c7434fad", "tu_agent");
        let _ = tail.feed(late_duplicate.as_bytes(), &mut |_| {});
        assert!(tail.has_pending_background(), "上一程的重复通知不销这一程");
        let _ = tail.feed(agent_done("tu_send").as_bytes(), &mut |_| {});
        assert!(!tail.has_pending_background());

        let mut early = TranscriptTail::default();
        let _ = early.feed(
            lines(&[AGENT_BG_CALL, AGENT_BG_LAUNCHED]).as_bytes(),
            &mut |_| {},
        );
        let _ = early.feed(agent_done("tu_agent").as_bytes(), &mut |_| {});
        let _ = early.feed(agent_done("tu_send").as_bytes(), &mut |_| {});
        let _ = early.feed(
            lines(&[AGENT_RESUME_CALL, AGENT_RESUMED]).as_bytes(),
            &mut |_| {},
        );
        assert!(!early.has_pending_background(), "这一程的通知已经来过");
    }

    /// 不带调用 id 的重复形态同样只是重复:吞不掉之后续用的登记。
    #[test]
    fn a_duplicate_without_a_call_id_does_not_swallow_a_later_resume() {
        let mut tail = TranscriptTail::default();
        let _ = tail.feed(
            lines(&[AGENT_BG_CALL, AGENT_BG_LAUNCHED]).as_bytes(),
            &mut |_| {},
        );
        let _ = tail.feed(agent_done("tu_agent").as_bytes(), &mut |_| {});
        let _ = tail.feed(user_row_notice("a1c7b6848c7434fad").as_bytes(), &mut |_| {});
        let _ = tail.feed(
            lines(&[AGENT_RESUME_CALL, AGENT_RESUMED]).as_bytes(),
            &mut |_| {},
        );
        assert!(tail.has_pending_background(), "续用应登记");
    }

    /// 通知点名的任务在账上、调用 id 对不上又不是重复:照样销账,不等一个可能永远不来的通知。
    #[test]
    fn a_notice_with_an_unexpected_call_id_still_settles_its_task() {
        let mut tail = TranscriptTail::default();
        let _ = tail.feed(lines(&[AGENT_BG_LAUNCHED]).as_bytes(), &mut |_| {});
        let _ = tail.feed(agent_done("tu_other").as_bytes(), &mut |_| {});
        assert!(!tail.has_pending_background());
    }

    /// 通知先于回执落盘、回执到时不再登记的那一程,它的通知后到的其余形态也只是重复:销不掉
    /// 之后续用的那一程。
    #[test]
    fn late_forms_of_an_early_notice_do_not_settle_a_later_resume() {
        let mut tail = TranscriptTail::default();
        let _ = tail.feed(agent_done("tu_agent").as_bytes(), &mut |_| {});
        let _ = tail.feed(
            lines(&[AGENT_BG_CALL, AGENT_BG_LAUNCHED]).as_bytes(),
            &mut |_| {},
        );
        let _ = tail.feed(
            lines(&[AGENT_RESUME_CALL, AGENT_RESUMED]).as_bytes(),
            &mut |_| {},
        );
        let late = queued_attachment_notice("a1c7b6848c7434fad", "tu_agent");
        let _ = tail.feed(late.as_bytes(), &mut |_| {});
        assert!(tail.has_pending_background(), "续用的那一程还在跑");
    }

    /// 前台命令超时被挪到后台,同样是要等通知的后台任务。
    #[test]
    fn a_command_moved_to_the_background_is_pending_until_its_notification() {
        let mut tail = TranscriptTail::default();
        let _ = tail.feed(lines(&[SHELL_MOVED_TO_BACKGROUND]).as_bytes(), &mut |_| {});
        assert!(tail.has_pending_background());
        let done = task_notification("b2be82guw", "tu_slow", "Background command \"x\" completed");
        let _ = tail.feed(done.as_bytes(), &mut |_| {});
        assert!(!tail.has_pending_background());
    }

    /// 启动失败的后台命令没有任务号、也不会有通知:不登记,否则会话冻到 continuation_cap。
    #[test]
    fn a_failed_background_launch_is_never_pending() {
        let mut tail = TranscriptTail::default();
        let _ = tail.feed(
            lines(&[SHELL_BG_CALL, SHELL_BG_FAILED]).as_bytes(),
            &mut |_| {},
        );
        assert!(!tail.has_pending_background());
    }

    /// 派到后台的子 agent 与后台命令一样让主路径续跑,完成通知按 task-id 销账。
    #[test]
    fn tail_keeps_a_background_agent_pending_until_its_notification() {
        let mut tail = TranscriptTail::default();
        let turn = lines(&[AGENT_BG_CALL, AGENT_BG_LAUNCHED, WAIT_END]);
        match tail.feed(turn.as_bytes(), &mut |_| {}) {
            TailStep::Done { .. } => {}
            other => panic!("end_turn 应收尾,实得 {other:?}"),
        }
        assert_eq!(tail.pending_background_count(), 1);
        tail.reset_turn();
        assert!(tail.has_pending_background(), "reset_turn 必须保留");

        let _ = tail.feed(agent_done("tu_agent").as_bytes(), &mut |_| {});
        assert!(!tail.has_pending_background(), "通知到达应销账");
        assert_eq!(
            tail.drain_notices(),
            vec![
                summon_core::turn_notice::TurnNotice::task("Agent \"审一下\" finished")
                    .expect("非空")
            ],
        );
    }

    /// 对不上本 tail 任何任务的通知什么都不销:Monitor 的事件、别的轮遗留的子 agent、已经销过账
    /// 的 agent 再来一条。同时在跑的后台命令照旧等它自己那条通知。
    #[test]
    fn notifications_for_other_tasks_leave_the_pending_ones_alone() {
        let mut tail = TranscriptTail::default();
        let turn = lines(&[
            SHELL_BG_CALL,
            SHELL_BG_STARTED,
            AGENT_BG_CALL,
            AGENT_BG_LAUNCHED,
        ]);
        let _ = tail.feed(turn.as_bytes(), &mut |_| {});
        let _ = tail.feed(agent_done("tu_agent").as_bytes(), &mut |_| {});
        assert_eq!(tail.pending_background_count(), 1, "只剩后台命令");

        let stray = task_notification("a9e6926e0291f1870", "tu_old", "Agent \"上一轮的\" finished");
        let others = format!("{MONITOR_EVENT}\n{stray}{}", agent_done("tu_agent"));
        let _ = tail.feed(others.as_bytes(), &mut |_| {});
        assert_eq!(tail.pending_background_count(), 1, "后台命令还没通知");

        let _ = tail.feed(shell_done().as_bytes(), &mut |_| {});
        assert!(!tail.has_pending_background());
    }

    /// SendMessage 把已结束的 agent 续用到后台:它重新 pending,直到再通知一次。
    #[test]
    fn a_resumed_agent_is_pending_again_until_it_notifies_again() {
        let mut tail = TranscriptTail::default();
        let _ = tail.feed(
            lines(&[AGENT_BG_CALL, AGENT_BG_LAUNCHED]).as_bytes(),
            &mut |_| {},
        );
        let _ = tail.feed(agent_done("tu_agent").as_bytes(), &mut |_| {});
        assert!(!tail.has_pending_background());

        let _ = tail.feed(
            lines(&[AGENT_RESUME_CALL, AGENT_RESUMED]).as_bytes(),
            &mut |_| {},
        );
        assert!(tail.has_pending_background(), "续用后重新 pending");
        let _ = tail.feed(agent_done("tu_send").as_bytes(), &mut |_| {});
        assert!(!tail.has_pending_background());
    }

    /// 「仅入队」的 SendMessage 回执不算起了后台任务:它可能是发给 teammate 的,当成后台任务
    /// 会等一个不会来的通知,一直等到续跑上限。
    #[test]
    fn a_queued_message_is_not_a_background_task() {
        let mut tail = TranscriptTail::default();
        let _ = tail.feed(
            lines(&[AGENT_RESUME_CALL, MESSAGE_QUEUED]).as_bytes(),
            &mut |_| {},
        );
        assert!(!tail.has_pending_background());
    }

    /// 后台 workflow 同样续跑,按它的 taskId 销账。
    #[test]
    fn a_background_workflow_is_pending_until_its_notification() {
        let mut tail = TranscriptTail::default();
        let _ = tail.feed(lines(&[WORKFLOW_LAUNCHED]).as_bytes(), &mut |_| {});
        assert!(tail.has_pending_background());
        let done = task_notification(
            "wco3l6qra",
            "tu_wf",
            "Workflow \"review-changes\" completed",
        );
        let _ = tail.feed(done.as_bytes(), &mut |_| {});
        assert!(!tail.has_pending_background());
    }

    /// 用户成批停掉后台子 agent 时 claude 只落一条 `agents_killed`,不给每个 agent 发通知:
    /// 子 agent 一并销账,后台命令不受影响。
    #[test]
    fn killed_agents_stop_being_pending_but_shells_still_are() {
        let mut tail = TranscriptTail::default();
        let turn = lines(&[
            SHELL_BG_CALL,
            SHELL_BG_STARTED,
            AGENT_BG_CALL,
            AGENT_BG_LAUNCHED,
            WORKFLOW_LAUNCHED,
            AGENTS_KILLED,
        ]);
        let _ = tail.feed(turn.as_bytes(), &mut |_| {});
        assert_eq!(tail.pending_background_count(), 1, "只剩后台命令");
        let _ = tail.feed(shell_done().as_bytes(), &mut |_| {});
        assert!(!tail.has_pending_background());
    }

    /// 一条通知列了好几个任务(进程重启后的孤儿汇总):逐个销账,扫描记号不算任务。
    #[test]
    fn a_notification_listing_several_tasks_settles_each_of_them() {
        let mut tail = TranscriptTail::default();
        let second = SHELL_MOVED_TO_BACKGROUND;
        let _ = tail.feed(lines(&[SHELL_BG_STARTED, second]).as_bytes(), &mut |_| {});
        assert_eq!(tail.pending_background_count(), 2);
        let summary = serde_json::json!({
            "type": "queue-operation",
            "operation": "enqueue",
            "content": "<task-notification>\n<task-id>bfyy2gdqz</task-id>\n<task-id>b2be82guw</task-id>\n<task-id>__orphan_summary__:shell</task-id>\n<status>stopped</status>\n<summary>2 background shell command tasks didn't finish before the previous session ended.</summary>\n</task-notification>"
        })
        .to_string()
            + "\n";
        let _ = tail.feed(summary.as_bytes(), &mut |_| {});
        assert!(!tail.has_pending_background());
        assert!(
            !tail
                .background
                .unmatched_notices
                .iter()
                .any(|run| run.task_id.starts_with("__orphan")),
            "扫描记号不是任务"
        );
    }

    /// 同一批并行工具里,完成通知可能先于启动回执落盘:回执晚到时不再登记,否则要等一个已经
    /// 来过的通知,一直等到续跑上限。
    #[test]
    fn a_launch_receipt_after_its_notification_is_not_waited_for() {
        let mut tail = TranscriptTail::default();
        let _ = tail.feed(agent_done("tu_agent").as_bytes(), &mut |_| {});
        let _ = tail.feed(
            lines(&[AGENT_BG_CALL, AGENT_BG_LAUNCHED]).as_bytes(),
            &mut |_| {},
        );
        assert!(!tail.has_pending_background());
    }

    /// 子 agent 自己对话里的启动(isSidechain)是它的后台任务,通知回不到主转录,不算主轮的。
    #[test]
    fn a_sidechain_launch_is_not_a_main_turn_background_task() {
        let mut tail = TranscriptTail::default();
        let sidechain = AGENT_BG_LAUNCHED.replace("\"isSidechain\":false", "\"isSidechain\":true");
        let _ = tail.feed(lines(&[&sidechain]).as_bytes(), &mut |_| {});
        assert!(!tail.has_pending_background());
    }

    /// run_turn 里每个「收尾还是续跑」的判定都问整本账(`has_pending_background`):直接
    /// end_turn、thinking-only 静默确认、以及等通知期间让 stall 看门狗让位。漏一处,那种收尾
    /// 形态就会在子 agent 还在跑时提前交付,或在静默 240s 后被看门狗收掉。
    #[test]
    fn every_continuation_decision_asks_the_whole_ledger() {
        let squashed: String = production_source().split_whitespace().collect();
        assert_eq!(
            squashed.matches("iftail.has_pending_background(){").count(),
            2,
            "两处 end_turn 续跑判定"
        );
        assert_eq!(
            squashed
                .matches("continuation_start.is_some()&&tail.has_pending_background();")
                .count(),
            1,
            "stall 让位判定"
        );
        assert!(
            !squashed.contains("tail.background."),
            "run_turn 不直接读账本"
        );
    }

    fn pending_in(rows: &[&str]) -> Vec<PendingBackgroundTask> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        std::fs::write(&path, lines(rows)).unwrap();
        pending_background_tasks(&path).unwrap()
    }

    const QUEUE_DEQUEUE: &str = r#"{"type":"queue-operation","operation":"dequeue","timestamp":"2026-10-23T08:18:35.273Z"}"#;

    fn notice_text(task_id: &str, tool_use_id: &str) -> String {
        format!(
            "<task-notification>\n<task-id>{task_id}</task-id>\n<tool-use-id>{tool_use_id}</tool-use-id>\n<status>completed</status>\n<summary>done</summary>\n</task-notification>"
        )
    }

    /// claude 2.1.309 实测:主 Agent 忙时到达的通知依次落带正文的 enqueue、前一轮的 end_turn、
    /// 不带正文的 dequeue,再落这一行,处理完再有 end_turn。
    fn notice_user(task_id: &str, tool_use_id: &str) -> String {
        serde_json::json!({
            "type": "user",
            "isSidechain": false,
            "message": {"role": "user", "content": notice_text(task_id, tool_use_id)},
        })
        .to_string()
    }

    /// 工具边界出队时的消费行。
    fn notice_attachment(task_id: &str, tool_use_id: &str) -> String {
        serde_json::json!({
            "type": "attachment",
            "isSidechain": false,
            "attachment": {
                "type": "queued_command",
                "prompt": notice_text(task_id, tool_use_id),
                "commandMode": "task-notification",
            },
        })
        .to_string()
    }

    /// 队列里同时排着几条通知与别人的消息:每条通知各自要被消费、被收尾;remove 与引用通知
    /// 原文的工具结果都不算消费。
    #[test]
    fn background_work_waits_for_every_queued_notice_to_be_consumed_and_answered() {
        let second_launch = AGENT_BG_LAUNCHED
            .replace("a1c7b6848c7434fad", "a2b2c3d4e5f6a7b8c")
            .replace("tu_agent", "tu_agent2");
        let held = ["a1c7b6848c7434fad", "a2b2c3d4e5f6a7b8c"];
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        std::fs::write(&path, lines(&[AGENT_BG_LAUNCHED, &second_launch])).unwrap();
        let append = |rows: &[&str]| {
            std::fs::write(
                &path,
                std::fs::read_to_string(&path).unwrap() + &lines(rows),
            )
            .unwrap()
        };
        let outstanding = || background_work_outstanding(&path, &held).unwrap();
        let other_enqueue = serde_json::json!({
            "type": "queue-operation", "operation": "enqueue", "content": "别人的消息",
        })
        .to_string();
        let other_user = serde_json::json!({
            "type": "user", "isSidechain": false,
            "message": {"role": "user", "content": "别人的消息"},
        })
        .to_string();
        let remove = serde_json::json!({
            "type": "queue-operation", "operation": "remove",
            "content": notice_text("a2b2c3d4e5f6a7b8c", "tu_agent2"),
        })
        .to_string();
        let quoted = serde_json::json!({
            "type": "user", "isSidechain": false,
            "message": {"role": "user", "content": [{
                "type": "tool_result", "tool_use_id": "tu_cat",
                "content": notice_text("a2b2c3d4e5f6a7b8c", "tu_agent2"),
            }]},
        })
        .to_string();

        // 别的轮进行中,别人的消息与两条通知先后入队,那一轮随后结束。
        append(&[
            &other_enqueue,
            task_notification("a1c7b6848c7434fad", "tu_agent", "first").trim_end(),
            task_notification("a2b2c3d4e5f6a7b8c", "tu_agent2", "second").trim_end(),
            WAIT_END,
        ]);
        assert!(outstanding());
        // 先取出别人的消息并答完:两条通知都还排着。
        append(&[QUEUE_DEQUEUE, &other_user, WAIT_END]);
        assert!(outstanding());
        // 第二条被 remove 撤出:没有消费记录,仍按排队算。
        append(&[&remove]);
        assert!(outstanding());
        // 第一条被消费并答完,撤出的第二条仍排着。
        append(&[
            QUEUE_DEQUEUE,
            &notice_user("a1c7b6848c7434fad", "tu_agent"),
            WAIT_END,
        ]);
        assert!(outstanding());
        // 引用通知原文的工具结果不是消费,其后又有几轮结束,第二条仍排着。
        append(&[&quoted, WAIT_END, WAIT_END]);
        assert!(outstanding());
        // 第二条在工具边界出队,主 Agent 处理中。
        append(&[&notice_attachment("a2b2c3d4e5f6a7b8c", "tu_agent2")]);
        assert!(outstanding());
        append(&[WAIT_END]);
        assert!(!outstanding());
    }

    /// 让轮交出去的清单带 task-id 与种类:子 agent 按 agent_id 认领调用,workflow 认不了。
    #[test]
    fn held_background_lists_ids_and_kinds_in_launch_order() {
        let mut ledger = BackgroundLedger::default();
        for line in [SHELL_BG_STARTED, AGENT_BG_LAUNCHED, WORKFLOW_LAUNCHED] {
            ledger.observe(line);
        }
        let held = |id: &str, kind| HeldBackgroundTask {
            task_id: id.into(),
            kind,
        };
        assert_eq!(
            ledger.held(),
            [
                held("bfyy2gdqz", HeldTaskKind::Shell),
                held("a1c7b6848c7434fad", HeldTaskKind::Agent),
                held("wco3l6qra", HeldTaskKind::Workflow),
            ]
        );

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        std::fs::write(
            &path,
            lines(&[SHELL_BG_STARTED, AGENT_BG_LAUNCHED, WORKFLOW_LAUNCHED])
                + &task_notification("a1c7b6848c7434fad", "tu_agent", "Agent finished"),
        )
        .unwrap();
        assert!(background_work_outstanding(&path, &["a1c7b6848c7434fad", "bfyy2gdqz"]).unwrap());
        let append = |rows: &[&str]| {
            std::fs::write(
                &path,
                std::fs::read_to_string(&path).unwrap() + &lines(rows),
            )
            .unwrap()
        };
        append(&[
            WAIT_END,
            QUEUE_DEQUEUE,
            &notice_user("a1c7b6848c7434fad", "tu_agent"),
        ]);
        assert!(background_work_outstanding(&path, &["a1c7b6848c7434fad"]).unwrap());
        append(&[WAIT_END]);
        assert!(!background_work_outstanding(&path, &["a1c7b6848c7434fad"]).unwrap());
        assert!(background_work_outstanding(&path, &["wco3l6qra"]).unwrap());
        assert!(!background_work_outstanding(&path, &["never-launched"]).unwrap());
    }

    #[test]
    fn pending_background_tasks_lists_what_is_still_running_in_launch_order() {
        let shell = |description: Option<&str>| PendingBackgroundTask {
            kind: BackgroundKind::Shell,
            description: description.map(str::to_string),
        };
        let agent = PendingBackgroundTask {
            kind: BackgroundKind::Agent,
            description: Some("审一下".to_string()),
        };
        let described_shell_call = SHELL_BG_CALL.replace(
            r#""run_in_background":true"#,
            r#""run_in_background":true,"description":"跑全量测试""#,
        );
        assert_eq!(
            pending_in(&[
                &described_shell_call,
                SHELL_BG_STARTED,
                AGENT_BG_CALL,
                AGENT_BG_LAUNCHED,
                WAIT_END,
            ]),
            vec![shell(Some("跑全量测试")), agent.clone()]
        );
        assert_eq!(
            pending_in(&[SHELL_BG_CALL, SHELL_BG_STARTED]),
            vec![shell(None)],
            "调用没写 description 时只报种类"
        );
        assert_eq!(
            pending_in(&[
                SHELL_BG_CALL,
                SHELL_BG_STARTED,
                AGENT_BG_CALL,
                AGENT_BG_LAUNCHED,
                shell_done().trim_end(),
            ]),
            vec![agent],
            "完成通知销掉点名的那一个"
        );
        assert_eq!(
            pending_in(&[
                SHELL_BG_CALL,
                SHELL_BG_STARTED,
                AGENT_BG_CALL,
                AGENT_BG_LAUNCHED,
                AGENTS_KILLED,
            ]),
            vec![shell(None)],
            "成批停掉子 agent 不影响后台命令"
        );
    }

    #[test]
    fn pending_background_tasks_ignore_sidechain_launches() {
        let sidechain_call =
            AGENT_BG_CALL.replace(r#""isSidechain":false"#, r#""isSidechain":true"#);
        let sidechain_launch =
            AGENT_BG_LAUNCHED.replace(r#""isSidechain":false"#, r#""isSidechain":true"#);
        assert!(pending_in(&[&sidechain_call, &sidechain_launch]).is_empty());
    }

    /// 整条主路径:派出后台子 agent 的那个 end_turn 不收尾,那一段的话作为中途回复交出去;子 agent
    /// 的完成通知到了、claude 续跑的那个 end_turn 才是这一轮的终态。
    #[test]
    #[ignore = "process-wide tmux fixture; run with --ignored --test-threads=1"]
    fn a_turn_waits_for_its_background_agent_before_settling() {
        use std::cell::{Cell, RefCell};
        /// 转录先只有派出那一段;读了一阵之后(主路径早已在那个 end_turn 决定了去留)才长出
        /// 完成通知与续跑的终态。
        struct Replay {
            bytes: RefCell<Vec<u8>>,
            later: RefCell<Option<Vec<u8>>>,
            first_read: Cell<Option<Instant>>,
        }
        impl Executor for Replay {
            fn spawn(
                &self,
                _: &crate::runtime::executor::SpawnSpec,
            ) -> anyhow::Result<std::process::Child> {
                unreachable!()
            }
            fn read_file_tail(&self, _: &std::path::Path, from: u64) -> anyhow::Result<Vec<u8>> {
                let first = *self.first_read.get().get_or_insert_with(Instant::now);
                self.first_read.set(Some(first));
                if first.elapsed() > Duration::from_millis(1500) {
                    if let Some(more) = self.later.borrow_mut().take() {
                        self.bytes.borrow_mut().extend(more);
                    }
                }
                let bytes = self.bytes.borrow();
                Ok(bytes.get(from as usize..).unwrap_or_default().to_vec())
            }
            fn tmux(&self, _: &[&str]) -> anyhow::Result<String> {
                unreachable!()
            }
        }
        let (agent, dir) = crate::test_support::agent_dir("claude", "");
        let fake = dir.path().join("tmux");
        crate::test_support::write_test_executable(&fake, "#!/bin/sh\nprintf '❯ \\n'\n");
        let mut config = summon_core::config::Config::default();
        config.tools.tmux = Some(fake);
        summon_core::tools::configure(&config);
        struct Reset;
        impl Drop for Reset {
            fn drop(&mut self) {
                summon_core::tools::configure(&summon_core::config::Config::default());
            }
        }
        let _reset = Reset;
        let session =
            crate::session::session::Session::with_name(&agent, "bg-agent-test".into(), 100, 100);
        crate::session::resume_store::write(
            &agent,
            "bg-agent-test",
            "claude",
            &uuid::Uuid::new_v4().to_string(),
            Some("/tmp"),
            None,
        );
        let finished = r#"{"type":"assistant","isSidechain":false,"message":{"role":"assistant","content":[{"type":"text","text":"审完了"}],"stop_reason":"end_turn"}}"#;
        let replay = Replay {
            bytes: RefCell::new(lines(&[AGENT_BG_CALL, AGENT_BG_LAUNCHED, WAIT_END]).into()),
            later: RefCell::new(Some(
                (task_notification("a1c7b6848c7434fad", "tu_agent", "Agent \"审一下\" finished")
                    + &lines(&[finished]))
                    .into(),
            )),
            first_read: Cell::new(None),
        };
        let started = Instant::now();
        let continuations = Cell::new(0u32);
        let interim = RefCell::new(Vec::new());
        let mut capture = Transcript::new(&session, replay);
        let result = capture
            .run_turn(
                "probe",
                TurnInjection::PreInjected { baseline: 0 },
                &mut |_| {},
                &mut |_| {},
                &mut |_| {},
                &mut |_| {},
                &mut |_| MediationCard::Settled,
                &mut || {
                    (started.elapsed() > Duration::from_secs(20))
                        .then_some(crate::capture::TurnInterrupt::Stop)
                },
                &mut || {},
                &mut |_, _, _| {},
                &mut |_| {},
                &mut || true,
                &mut || {},
                &mut |_, _, _| {},
                &mut |_| {},
                &mut |_| {},
                &mut |_| {},
                &mut |_| {},
                &mut || continuations.set(continuations.get() + 1),
                &mut |text| {
                    interim.borrow_mut().push(text.to_string());
                    true
                },
                &mut |_| false,
                Duration::from_secs(60),
            )
            .unwrap();
        assert!(!result.interrupted, "不该靠超时打断收尾");
        assert_eq!(result.reply.as_deref(), Some("审完了"));
        assert_eq!(interim.into_inner(), ["派出去了,等它回来"]);
        assert_eq!(continuations.get(), 1);
    }

    #[test]
    #[ignore = "process-wide tmux fixture; run with --ignored --test-threads=1"]
    fn background_idle_yields_only_after_a_complete_main_terminal() {
        use std::cell::{Cell, RefCell};
        struct Replay {
            bytes: RefCell<Vec<u8>>,
            reads: Cell<usize>,
            later: RefCell<std::collections::VecDeque<Vec<u8>>>,
        }
        impl Executor for Replay {
            fn spawn(
                &self,
                _: &crate::runtime::executor::SpawnSpec,
            ) -> anyhow::Result<std::process::Child> {
                unreachable!()
            }
            fn read_file_tail(&self, _: &std::path::Path, from: u64) -> anyhow::Result<Vec<u8>> {
                if self.reads.replace(self.reads.get() + 1) > 0 {
                    if let Some(next) = self.later.borrow_mut().pop_front() {
                        self.bytes.borrow_mut().extend(next);
                    }
                }
                Ok(self
                    .bytes
                    .borrow()
                    .get(from as usize..)
                    .unwrap_or_default()
                    .to_vec())
            }
            fn file_len(&self, _: &std::path::Path) -> anyhow::Result<u64> {
                Ok(self.bytes.borrow().len() as u64)
            }
            fn read_file_range(
                &self,
                _: &std::path::Path,
                from: u64,
                max: usize,
            ) -> anyhow::Result<Vec<u8>> {
                Ok(self
                    .bytes
                    .borrow()
                    .get(from as usize..)
                    .unwrap_or_default()
                    .iter()
                    .take(max)
                    .copied()
                    .collect())
            }
            fn tmux(&self, _: &[&str]) -> anyhow::Result<String> {
                unreachable!()
            }
        }
        let (agent, dir) = crate::test_support::agent_dir("claude", "");
        let fake = dir.path().join("tmux");
        let calls = dir.path().join("tmux-calls");
        crate::test_support::write_test_executable(
            &fake,
            "#!/bin/sh\necho \"$*\" >> \"$(dirname \"$0\")/tmux-calls\"\nprintf '❯ \\n'\n",
        );
        let mut config = summon_core::config::Config::default();
        config.tools.tmux = Some(fake);
        summon_core::tools::configure(&config);
        struct Reset;
        impl Drop for Reset {
            fn drop(&mut self) {
                summon_core::tools::configure(&summon_core::config::Config::default());
            }
        }
        let _reset = Reset;
        let session =
            crate::session::session::Session::with_name(&agent, "bg-idle-test".into(), 100, 100);
        let sid = uuid::Uuid::new_v4().to_string();
        crate::session::resume_store::write(
            &agent,
            "bg-idle-test",
            "claude",
            &sid,
            Some("/tmp"),
            None,
        );
        let busy = r#"{"type":"assistant","isSidechain":false,"message":{"role":"assistant","content":[{"type":"thinking","thinking":"working"}],"stop_reason":null}}"#;
        let finished = r#"{"type":"assistant","isSidechain":false,"message":{"role":"assistant","content":[{"type":"text","text":"finished"}],"stop_reason":"end_turn"}}"#;
        let sync_call = r#"{"type":"assistant","isSidechain":false,"message":{"role":"assistant","content":[{"type":"tool_use","id":"tu_sync","name":"Bash","input":{"command":"wait"}}],"stop_reason":"tool_use"}}"#;
        let sync_done = r#"{"type":"user","isSidechain":false,"message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"tu_sync","content":"done"}]}}"#;
        let metadata = lines(&[
            r#"{"type":"system","subtype":"stop_hook_summary"}"#,
            r#"{"type":"system","subtype":"turn_duration"}"#,
            r#"{"type":"progress","data":{"type":"agent_progress"}}"#,
        ]);
        let notification = task_notification("a1c7b6848c7434fad", "tu_agent", "Agent finished");
        for (name, suffix, later, yields) in [
            ("idle", String::new(), vec![], true),
            ("idle-metadata-buffered", metadata.clone(), vec![], true),
            (
                "idle-metadata-partial",
                String::new(),
                vec![
                    metadata[..15].to_string(),
                    String::new(),
                    metadata[15..].to_string(),
                ],
                true,
            ),
            ("unconsumed-steer", String::new(), vec![], true),
            (
                "tool-wait",
                String::new(),
                vec![
                    lines(&[sync_call]),
                    String::new(),
                    lines(&[sync_done]) + &notification + &lines(&[finished]),
                ],
                false,
            ),
            (
                "resumed",
                String::new(),
                vec![lines(&[busy]), notification.clone() + &lines(&[finished])],
                false,
            ),
            (
                "partial",
                "{".to_string(),
                vec![
                    String::new(),
                    notification[1..].to_string() + &lines(&[finished]),
                ],
                false,
            ),
            (
                "buffered",
                lines(&[busy]),
                vec![notification.clone() + &lines(&[finished])],
                false,
            ),
        ] {
            std::fs::write(&calls, "").unwrap();
            let replay = Replay {
                reads: Cell::new(0),
                bytes: RefCell::new(
                    (lines(&[AGENT_BG_CALL, AGENT_BG_LAUNCHED, WAIT_END]) + &suffix).into_bytes(),
                ),
                later: RefCell::new(later.into_iter().map(String::into_bytes).collect()),
            };
            let unconsumed_steer = name == "unconsumed-steer";
            let steer_sent = Cell::new(false);
            let start = Instant::now();
            let idle_calls = Cell::new(0);
            let continuations = Cell::new(0);
            let interim = RefCell::new(Vec::new());
            let final_messages = RefCell::new(Vec::new());
            let mut capture = Transcript::new(&session, replay);
            let result = capture
                .run_turn(
                    "probe",
                    TurnInjection::PreInjected { baseline: 0 },
                    &mut |text| final_messages.borrow_mut().push(text.to_string()),
                    &mut |_| {},
                    &mut |_| {},
                    &mut |_| {},
                    &mut |_| MediationCard::Settled,
                    &mut || {
                        if unconsumed_steer && !steer_sent.replace(true) {
                            return Some(crate::capture::TurnInterrupt::Steer(
                                crate::capture::SteerRequest {
                                    prompt: "queued message".to_string(),
                                    message_ids: vec!["om_queued".to_string()],
                                },
                            ));
                        }
                        (start.elapsed() > Duration::from_secs(5))
                            .then_some(crate::capture::TurnInterrupt::Stop)
                    },
                    &mut || {},
                    &mut |_, _, _| {},
                    &mut |_| {},
                    &mut || true,
                    &mut || {},
                    &mut |_, _, _| {},
                    &mut |_| {},
                    &mut |_| {},
                    &mut |_| {},
                    &mut |_| {},
                    &mut || continuations.set(continuations.get() + 1),
                    &mut |text| {
                        interim.borrow_mut().push(text.to_string());
                        true
                    },
                    &mut |idle| {
                        idle_calls.set(idle_calls.get() + 1);
                        assert_eq!(
                            idle.tasks,
                            [HeldBackgroundTask {
                                task_id: "a1c7b6848c7434fad".into(),
                                kind: HeldTaskKind::Agent,
                            }],
                            "{name}"
                        );
                        assert_eq!(idle.session_id, sid, "{name}");
                        yields
                    },
                    Duration::from_secs(60),
                )
                .unwrap();
            if unconsumed_steer {
                assert!(
                    result.interrupted,
                    "pending native input must keep the current turn"
                );
                assert_eq!(result.followup_message_ids, ["om_queued"]);
                assert_eq!(idle_calls.get(), 0);
                continue;
            }
            assert!(!result.interrupted, "{name}: must settle without Esc");
            assert!(!result.timed_out, "{name}");
            if yields {
                assert_eq!(idle_calls.get(), 1, "{name}");
                assert_eq!(result.interim_replies, 1);
                assert_eq!(interim.into_inner(), ["派出去了,等它回来"]);
                assert!(final_messages.borrow().is_empty());
                assert!(result.reply.is_none());
                assert!(result.yielded_to_queue);
                assert_eq!(continuations.get(), 1);
            } else {
                assert_eq!(idle_calls.get(), 0, "{name}");
                assert_eq!(result.reply.as_deref(), Some("finished"), "{name}");
            }
            assert!(
                !std::fs::read_to_string(&calls).unwrap().contains("Escape"),
                "{name}"
            );
        }
    }

    /// 接管上一代注入的轮:接管那一刻已经落盘的段终点属于重放 —— 上一代可能已经把它当段终点回复
    /// 发过,不再交出;接管之后才写出的段终点只有本代见过,照常交给 `on_interim_reply`,且只交那一条
    /// end_turn 自己的正文,接管前落盘的中间文字不跟着重发。
    #[test]
    fn a_reattached_turn_hands_off_only_the_segment_ends_written_after_takeover() {
        use std::cell::{Cell, RefCell};
        /// 转录按阶段长出来:第一拍读到的是接管时盘上已有的字节,之后每过一段时间追加一批。
        struct Staged {
            bytes: RefCell<Vec<u8>>,
            later: RefCell<Vec<(Duration, Vec<u8>)>>,
            first_read: Cell<Option<Instant>>,
        }
        impl Executor for Staged {
            fn spawn(
                &self,
                _: &crate::runtime::executor::SpawnSpec,
            ) -> anyhow::Result<std::process::Child> {
                unreachable!()
            }
            fn read_file_tail(&self, _: &std::path::Path, from: u64) -> anyhow::Result<Vec<u8>> {
                let first = *self.first_read.get().get_or_insert_with(Instant::now);
                self.first_read.set(Some(first));
                let mut later = self.later.borrow_mut();
                while later.first().is_some_and(|(at, _)| first.elapsed() >= *at) {
                    let (_, more) = later.remove(0);
                    self.bytes.borrow_mut().extend(more);
                }
                let bytes = self.bytes.borrow();
                Ok(bytes.get(from as usize..).unwrap_or_default().to_vec())
            }
            fn tmux(&self, _: &[&str]) -> anyhow::Result<String> {
                unreachable!()
            }
        }
        let (agent, dir) = crate::test_support::agent_dir("claude", "");
        let fake = dir.path().join("tmux");
        crate::test_support::write_test_executable(&fake, "#!/bin/sh\nprintf '❯ \\n'\n");
        let mut config = summon_core::config::Config::default();
        config.tools.tmux = Some(fake);
        summon_core::tools::configure(&config);
        struct Reset;
        impl Drop for Reset {
            fn drop(&mut self) {
                summon_core::tools::configure(&summon_core::config::Config::default());
            }
        }
        let _reset = Reset;
        let marker = "summon-transport-turn:123e4567-e89b-12d3-a456-426614174021";
        let prompt = "把素材生成完再接着做";
        let history = reattach_history();
        let consumed = format!(
            "{}\n",
            serde_json::json!({
                "type": "user",
                "message": {"role": "user", "content": format!("{prompt}\n\n<!-- {marker} -->")}
            })
        );
        let boundary = (history.len() + consumed.len()) as u64;
        // 接管前已经落盘的中间文字(stop_reason=tool_use):它不属于之后那个段终点的正文。
        let earlier = r#"{"type":"assistant","isSidechain":false,"message":{"role":"assistant","content":[{"type":"text","text":"接管前就写下的中间进展"}],"stop_reason":"tool_use"}}"#;
        let launched = lines(&[earlier, AGENT_BG_CALL, AGENT_BG_LAUNCHED]);
        let finished = r#"{"type":"assistant","isSidechain":false,"message":{"role":"assistant","content":[{"type":"text","text":"审完了"}],"stop_reason":"end_turn"}}"#;
        let notified =
            task_notification("a1c7b6848c7434fad", "tu_agent", "Agent \"审一下\" finished")
                + &lines(&[finished]);
        struct Case {
            name: &'static str,
            /// 接管时盘上已有的字节。
            at_takeover: String,
            /// 之后陆续写出的字节(距第一次读的时长, 字节)。
            later: Vec<(Duration, String)>,
            /// 期望交给 `on_interim_reply` 的段终点回复。
            expected: Vec<&'static str>,
        }
        let cases = [
            Case {
                name: "接管之后才写出段终点",
                at_takeover: format!("{history}{consumed}{launched}"),
                later: vec![
                    (Duration::from_millis(1500), lines(&[WAIT_END])),
                    (Duration::from_millis(3000), notified.clone()),
                ],
                expected: vec!["派出去了,等它回来"],
            },
            Case {
                name: "段终点在接管时已经落盘",
                at_takeover: format!("{history}{consumed}{launched}{}", lines(&[WAIT_END])),
                later: vec![(Duration::from_millis(1500), notified.clone())],
                expected: vec![],
            },
        ];
        for Case {
            name,
            at_takeover,
            later,
            expected,
        } in cases
        {
            let session_name = format!("reattach-interim-{}", expected.len());
            let session =
                crate::session::session::Session::with_name(&agent, session_name.clone(), 100, 100);
            crate::session::resume_store::write(
                &agent,
                &session_name,
                "claude",
                &uuid::Uuid::new_v4().to_string(),
                Some("/tmp"),
                None,
            );
            let staged = Staged {
                bytes: RefCell::new(at_takeover.into_bytes()),
                later: RefCell::new(
                    later
                        .into_iter()
                        .map(|(at, bytes)| (at, bytes.into_bytes()))
                        .collect(),
                ),
                first_read: Cell::new(None),
            };
            let started = Instant::now();
            let interim = RefCell::new(Vec::new());
            let mut capture = Transcript::new(&session, staged);
            let result = capture
                .run_turn(
                    "probe",
                    TurnInjection::Reattached {
                        marker: marker.to_string(),
                        prompt_head: prompt_head(prompt),
                        inject_offset: history.len() as u64,
                        consumed_boundary: Some(boundary),
                    },
                    &mut |_| {},
                    &mut |_| {},
                    &mut |_| {},
                    &mut |_| {},
                    &mut |_| MediationCard::Settled,
                    &mut || {
                        (started.elapsed() > Duration::from_secs(20))
                            .then_some(crate::capture::TurnInterrupt::Stop)
                    },
                    &mut || {},
                    &mut |_, _, _| {},
                    &mut |_| {},
                    &mut || true,
                    &mut || {},
                    &mut |_, _, _| {},
                    &mut |_| {},
                    &mut |_| {},
                    &mut |_| {},
                    &mut |_| {},
                    &mut || {},
                    &mut |text| {
                        interim.borrow_mut().push(text.to_string());
                        true
                    },
                    &mut |_| false,
                    Duration::from_secs(60),
                )
                .unwrap();
            assert!(!result.interrupted, "{name}:不该靠超时打断收尾");
            assert_eq!(result.reply.as_deref(), Some("审完了"), "{name}");
            assert_eq!(interim.into_inner(), expected, "{name}");
            assert_eq!(result.interim_replies, expected.len(), "{name}");
        }
    }

    #[test]
    fn tail_preserves_utf8_and_byte_boundary_when_codepoint_is_split_across_reads() {
        let line = serde_json::json!({
            "type": "assistant",
            "message": {"role": "assistant", "stop_reason": "end_turn",
                "content": [{"type": "text", "text": "中文回复"}]}
        })
        .to_string()
            + "\n";
        let bytes = line.as_bytes();
        let chinese = "中".as_bytes();
        let character_start = bytes
            .windows(chinese.len())
            .position(|window| window == chinese)
            .unwrap();
        let split = character_start + 1;

        let mut tail = TranscriptTail::default();
        assert!(matches!(
            tail.feed(&bytes[..split], &mut |_| {}),
            TailStep::Progress { .. }
        ));
        assert_eq!(
            split as u64 - tail.pending.len() as u64,
            0,
            "半个 UTF-8 codepoint 必须按原始字节留在 pending，boundary 不能漂移"
        );

        let TailStep::Done { final_text, .. } = tail.feed(&bytes[split..], &mut |_| {}) else {
            panic!("跨 read 拼回完整 UTF-8 行后应正常收尾");
        };
        assert_eq!(final_text, "中文回复");
        assert_eq!(
            bytes.len() as u64 - tail.pending.len() as u64,
            bytes.len() as u64,
            "terminal boundary 必须等于原始文件字节 EOF"
        );
    }

    /// 回归 bug:stop_sequence 的 "No response requested." 是 claude 伪影,**不能**当 turn-end;
    /// 应继续 tail 到真正的 end_turn,final 文本 = "23",且收尾发生在 end_turn 行而非 stop_sequence 行。
    #[test]
    fn stop_sequence_artifact_does_not_finalize_turn() {
        let mut tail = TranscriptTail::default();
        // 截到 stop_sequence assistant 行(前 3 行)为止:不应收尾
        let head: String = NORESP.lines().take(3).collect::<Vec<_>>().join("\n") + "\n";
        match tail.feed(&head, &mut |_| {}) {
            TailStep::Progress { stream, .. } => {
                // stop_sequence 文本仍可作为中途流式进度被看到
                assert_eq!(stream.as_deref(), Some("No response requested."));
            }
            TailStep::Done { .. } => panic!("stop_sequence 伪影不应收尾,应继续 tail"),
            TailStep::ApiError { .. } => panic!("fixture 无 API 错误,不应走此分支"),
        }
        // 喂入剩余行:真 end_turn 到来 → 收尾,final = "23"(非 "No response requested.")
        let rest: String = NORESP.lines().skip(3).collect::<Vec<_>>().join("\n") + "\n";
        match tail.feed(&rest, &mut |_| {}) {
            TailStep::Done {
                tools, final_text, ..
            } => {
                assert_eq!(tools, vec!["Bash".to_string()]);
                assert_eq!(final_text, "23");
            }
            TailStep::Progress { .. } => panic!("end_turn 到来应收尾"),
            TailStep::ApiError { .. } => panic!("fixture 无 API 错误,不应走此分支"),
        }
    }

    /// 冷启 seed 等待(根因修):转录出现 end_turn → `wait_first_turn_end` 返回 Ok(取代抓屏看门狗)。
    #[test]
    fn wait_first_turn_end_ok_on_end_turn() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("sid.jsonl");
        std::fs::write(&p, FIXTURE).unwrap(); // 含一个 end_turn
        let r = wait_first_turn_end(
            &crate::runtime::executor::LocalExecutor,
            &p,
            0,
            Duration::from_millis(1),
            Duration::from_millis(1),
            Instant::now() + Duration::from_secs(5),
            &mut || true,
        );
        assert!(
            matches!(r, Ok(SeedWait::Ended)),
            "见到 end_turn 应 Ok(Ended),实得 {r:?}"
        );
    }

    /// 到总时长封顶仍无 end_turn、但会话仍活 → Ok(TimedOut)(降级,非致命)。修:seed 超时不再 bail
    /// 丢用户消息(真机 bug:blue-wing /summon 已收尾但转录定位未命中,傻等 600s 超时把第二条 prompt 丢了)。
    #[test]
    fn wait_first_turn_end_timeout_is_not_fatal() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("sid.jsonl");
        std::fs::write(&p, "").unwrap(); // 空转录:永远读不到 end_turn
        let r = wait_first_turn_end(
            &crate::runtime::executor::LocalExecutor,
            &p,
            0,
            Duration::from_millis(1),
            Duration::from_millis(1),
            Instant::now(), // deadline 已到 → 下一次 idle 检查即 TimedOut
            &mut || true,   // 会话仍活
        );
        assert!(
            matches!(r, Ok(SeedWait::TimedOut)),
            "会话仍活但超时应 Ok(TimedOut),实得 {r:?}"
        );
    }

    /// 进程在 seed 跑完前已退出(转录有 tool_use 进度但无 end_turn,且 `alive()`=false)→ Err。
    /// 关键:**idle 本身不报错**(慢工具可跑很久),只有会话已退/总时长封顶才放弃——这正是根因修
    /// 与旧抓屏看门狗(idle 300s 即误判 Err)的区别。
    #[test]
    fn wait_first_turn_end_errs_when_session_dies_before_end_turn() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("sid.jsonl");
        // FIXTURE 前 4 行:到 tool_use assistant 为止,无 end_turn(真进度但未收尾)。
        let head: String = FIXTURE.lines().take(4).collect::<Vec<_>>().join("\n") + "\n";
        std::fs::write(&p, head).unwrap();
        let r = wait_first_turn_end(
            &crate::runtime::executor::LocalExecutor,
            &p,
            0,
            Duration::from_millis(1),
            Duration::from_millis(1),
            Instant::now() + Duration::from_secs(5),
            &mut || false, // 会话已退 → 抽干 backlog 后第一次 idle 检查即 Err(不卡)
        );
        assert!(r.is_err(), "会话已退且无 end_turn 应 Err");
    }

    /// 单条消息一轮(assistant "hi" 直接 end_turn,无工具):final 回复 = "hi"。
    #[test]
    fn single_message_end_turn_returns_its_text() {
        let line = r#"{"type":"assistant","sessionId":"s","message":{"id":"m","role":"assistant","stop_reason":"end_turn","content":[{"type":"text","text":"hi"}]}}"#;
        let mut tail = TranscriptTail::default();
        match tail.feed(&format!("{line}\n"), &mut |_| {}) {
            TailStep::Done { final_text, .. } => assert_eq!(final_text, "hi"),
            TailStep::Progress { .. } => panic!("end_turn 应收尾"),
            TailStep::ApiError { .. } => panic!("fixture 无 API 错误,不应走此分支"),
        }
    }

    /// tool_use 轮(stop_reason: tool_use)不收尾:截到 tool_result 之前喂入,应仍是 Progress。
    #[test]
    fn tool_use_stop_reason_does_not_end_turn() {
        // 取 fixture 前 4 行(到 tool_use assistant 为止,无 end_turn)
        let head: String = FIXTURE.lines().take(4).collect::<Vec<_>>().join("\n") + "\n";
        let mut tail = TranscriptTail::default();
        match tail.feed(&head, &mut |_| {}) {
            TailStep::Progress { stream, tools } => {
                assert_eq!(tools, vec!["Bash".to_string()]);
                // 累积里只有第一个 assistant 文本块(tool_use 块无 text)
                assert_eq!(stream.as_deref(), Some("Let me check the file."));
            }
            TailStep::Done { .. } => panic!("tool_use 轮不应收尾"),
            TailStep::ApiError { .. } => panic!("fixture 无 API 错误,不应走此分支"),
        }
    }

    /// 半行缓冲:把 fixture 在某字节处切两半,分两次喂,不切坏 UTF-8、最终仍正确收尾。
    #[test]
    fn buffers_partial_trailing_line_across_reads() {
        // 在第一个 end_turn 行的中间切(任取一个落在末行内的字节偏移)
        let split = FIXTURE.len() - 20; // 末行 "...2 files."}} 内部
        let (a, b) = FIXTURE.split_at(split);
        let mut tail = TranscriptTail::default();
        // 前半:末行不完整 → 不应收尾(pending 缓冲住半行)
        match tail.feed(a, &mut |_| {}) {
            TailStep::Progress { .. } => {}
            TailStep::Done { .. } => panic!("半行未闭合不应收尾"),
            TailStep::ApiError { .. } => panic!("fixture 无 API 错误,不应走此分支"),
        }
        // 后半:补全末行 → 收尾,final 文本 = end_turn 消息文本
        match tail.feed(b, &mut |_| {}) {
            TailStep::Done { final_text, .. } => {
                assert_eq!(final_text, "There are 2 files.");
            }
            TailStep::Progress { .. } => panic!("补全末行后应收尾"),
            TailStep::ApiError { .. } => panic!("fixture 无 API 错误,不应走此分支"),
        }
    }

    /// 逐字节喂入(模拟极碎的 tail 读)也不丢不重、最终一次性收尾。
    #[test]
    fn byte_by_byte_feed_still_ends_turn() {
        let mut tail = TranscriptTail::default();
        let mut done: Option<String> = None;
        for ch in FIXTURE.chars() {
            let mut buf = [0u8; 4];
            let s = ch.encode_utf8(&mut buf);
            if let TailStep::Done { final_text, .. } = tail.feed(s, &mut |_| {}) {
                done = Some(final_text);
            }
        }
        assert_eq!(done.as_deref(), Some("There are 2 files."));
    }

    /// 造一条 claude 转录里的 assistant 记录:`blocks` 是内容块(type, 正文),共享同一 stop_reason。
    /// claude 会把同一条消息的多个块**拆成相邻多行**,每行都带同一个 stop_reason —— 这正是本组回归
    /// 的核心形状,故这里一行只放一个块。
    fn rec(block_type: &str, body: &str, stop_reason: Option<&str>) -> String {
        let block = match block_type {
            "thinking" => serde_json::json!({"type": "thinking", "thinking": body}),
            _ => serde_json::json!({"type": block_type, "text": body}),
        };
        let mut message = serde_json::json!({"id": "m1", "role": "assistant", "content": [block]});
        if let Some(sr) = stop_reason {
            message["stop_reason"] = serde_json::Value::String(sr.to_string());
        }
        serde_json::json!({"type": "assistant", "sessionId": "s", "message": message}).to_string()
            + "\n"
    }

    /// ①(2026-07-27 现场根因):终态只有 thinking 块、无 text 块。旧实现只在 assistant 文本臂判
    /// `end_turn`,这条记录永远到不了判定 → tail 一直等,chat 线程报「跑一轮中」、后续消息全堵住。
    /// 修复后:先记候选(不立刻收,见 ②),静默确认后必须收轮。
    #[test]
    fn thinking_only_end_turn_finalizes_after_grace() {
        let mut tail = TranscriptTail::default();
        // 先有一段中途正文(工具前叙述),再以纯 thinking 收尾。
        assert!(matches!(
            tail.feed(&rec("text", "查一下", Some("tool_use")), &mut |_| {}),
            TailStep::Progress { .. }
        ));
        match tail.feed(&rec("thinking", "想完了", Some("end_turn")), &mut |_| {}) {
            TailStep::Progress { .. } => {}
            other => panic!("thinking 上的 end_turn 只是候选,不能立刻收轮: {other:?}"),
        }
        match tail.confirm_pending_end() {
            Some(TailStep::Done { final_text, .. }) => assert_eq!(final_text, "查一下"),
            other => panic!("静默确认后必须收轮(否则 chat 永久卡死): {other:?}"),
        }
        // 已确认过的候选不会二次收轮。
        assert!(tail.confirm_pending_end().is_none());
    }

    /// ②(必须与 ① 同时成立):真机 60 份转录里 430/788 条 end_turn 消息是 `thinking`+`text` **拆成
    /// 相邻两行、两行都带 end_turn**。若「见 end_turn 就收轮」,55% 的轮会收在 thinking 那行、把真正
    /// 的回复丢掉 —— 比原 bug 更严重。故终态回复仍取文本记录,thinking 记录只当候选。
    #[test]
    fn thinking_then_text_end_turn_keeps_text_as_final_reply() {
        let mut tail = TranscriptTail::default();
        assert!(matches!(
            tail.feed(&rec("thinking", "先想想", Some("end_turn")), &mut |_| {}),
            TailStep::Progress { .. }
        ));
        match tail.feed(&rec("text", "最终回复", Some("end_turn")), &mut |_| {}) {
            TailStep::Done { final_text, .. } => assert_eq!(final_text, "最终回复"),
            other => panic!("文本记录到达应按原路径收轮: {other:?}"),
        }
        // 候选已被真终态吸收,不会在下一轮里冒出来多收一次。
        assert!(tail.confirm_pending_end().is_none());
    }

    /// ③ stop_sequence 仍不是终态(claude 的 `No response requested.` 伪影),挂在 thinking 上也一样:
    /// 不收轮、也不留候选。
    #[test]
    fn stop_sequence_on_thinking_record_leaves_no_candidate() {
        let mut tail = TranscriptTail::default();
        assert!(matches!(
            tail.feed(
                &rec("thinking", "无需回复", Some("stop_sequence")),
                &mut |_| {}
            ),
            TailStep::Progress { .. }
        ));
        assert!(
            tail.confirm_pending_end().is_none(),
            "stop_sequence 不能当终态,否则会误取伪影当回复(实测:回复 23 却显示该串)"
        );
    }

    /// ④ 中途 thinking(无 stop_reason)不收轮、不留候选 —— claude 边想边干活的常态。
    #[test]
    fn mid_turn_thinking_without_stop_reason_leaves_no_candidate() {
        let mut tail = TranscriptTail::default();
        assert!(matches!(
            tail.feed(&rec("thinking", "边干边想", None), &mut |_| {}),
            TailStep::Progress { .. }
        ));
        assert!(tail.confirm_pending_end().is_none());
    }

    /// ⑤ 纯 thinking 收尾且本轮**一个字正文都没有**(现场那轮就是:thinking 块文本为空):
    /// 仍必须收轮,产出**空回复**且不 panic。轮次是否结束由 claude 的 end_turn 决定,与「有没有话说」
    /// 无关;空回复上层已有既定路径(不发终态气泡,贴 DONE 收尾)。
    #[test]
    fn thinking_only_end_turn_with_empty_accumulation_still_closes_turn() {
        let mut tail = TranscriptTail::default();
        assert!(matches!(
            tail.feed(&rec("thinking", "", Some("end_turn")), &mut |_| {}),
            TailStep::Progress { .. }
        ));
        match tail.confirm_pending_end() {
            Some(TailStep::Done { final_text, .. }) => {
                assert!(
                    final_text.is_empty(),
                    "没话说也要收轮,回复为空: {final_text:?}"
                );
            }
            other => panic!("空回复也必须收轮,不能因为没话说就永远不收: {other:?}"),
        }
    }

    /// 主路径以某个 terminal 收尾的每一个出口(终态、超 cap、API 错误,文本 end_turn 与静默确认
    /// 两种形态)都要先报结论:续跑区间里 mirror 可能正等着它,漏报就只能干等到超时让路。
    #[test]
    fn every_terminal_exit_of_run_turn_settles_the_segment_first() {
        let squashed: String = production_source().split_whitespace().collect();
        let body = &squashed[squashed.find("pubfnrun_turn(").expect("run_turn")..];
        let body = &body[..body.find("\n}").unwrap_or(body.len())];
        let exits = body.matches("breakfinal_text;").count();
        let settled = body
            .matches("on_segment_settled(terminal_boundary(&checkpoint,&tail),&tail.marker_evidence,true,);breakfinal_text;")
            .count();
        assert_eq!(
            exits, 6,
            "run_turn 以 terminal 收尾的出口数变了,确认新出口也先报结论"
        );
        assert_eq!(settled, exits, "每个以 terminal 收尾的出口都要先报结论");
    }

    /// 两处续跑判定(文本 end_turn、thinking 收尾的静默确认)都要先交出非空、且不是接管时重放出来
    /// 的这一段,记下发出与否,再清空;漏一处那种收尾形态的回复就随 `reset_turn` 消失。
    #[test]
    fn both_continuation_points_hand_off_the_segment_before_resetting() {
        let squashed: String = production_source().split_whitespace().collect();
        let handoff =
            "letdelivered=!final_text.trim().is_empty()&&!replayed_segment_end(replay_end,terminal_boundary(&checkpoint,&tail),)&&on_interim_reply(&final_text);ifdelivered{interim_replies+=1;}on_segment_settled(terminal_boundary(&checkpoint,&tail),&tail.marker_evidence,delivered,);";
        let guarded = squashed
            .match_indices(handoff)
            .filter(|(at, _)| {
                let after = &squashed[at + handoff.len()..];
                let reset = after.find("tail.reset_turn();").unwrap_or(usize::MAX);
                after[..reset.min(after.len())]
                    .find("breakfinal_text;")
                    .is_none()
                    && reset != usize::MAX
            })
            .count();
        assert_eq!(guarded, 2, "续跑的两处 reset_turn 前都要交出段终点回复");
    }

    /// Claude Code 2.1.300 起的真实转录行:一轮以只有 thinking 的 end_turn 收尾时,它追加这一行、
    /// 同一轮再请求一次模型。
    const CONTINUE_COMPANION: &str = r#"{"parentUuid":"p","isSidechain":false,"type":"user","message":{"role":"user","content":"[Your previous response had no visible output. Please continue and produce a user-visible response.]"},"isMeta":true,"turnCompanion":true,"uuid":"u","sessionId":"s"}"#;

    /// 续跑提示属于当前轮:它撤销 thinking 留下的候选终态,续跑隔多久落下一行都不按空回复收轮,
    /// 回复取续跑自己的 end_turn。
    #[test]
    fn turn_companion_keeps_the_turn_open_until_the_continuation_ends() {
        let mut tail = TranscriptTail::default();
        assert!(matches!(
            tail.feed(&rec("thinking", "想完了", Some("end_turn")), &mut |_| {}),
            TailStep::Progress { .. }
        ));
        match tail.feed(&format!("{CONTINUE_COMPANION}\n"), &mut |_| {}) {
            TailStep::Progress { .. } => {}
            other => panic!("续跑提示不是下一轮的开头: {other:?}"),
        }
        assert!(
            tail.confirm_pending_end().is_none(),
            "续跑还没落盘,静默确认不能把这一轮按空回复收掉"
        );
        match tail.feed(
            &rec("text", "续跑出来的回复", Some("end_turn")),
            &mut |_| {},
        ) {
            TailStep::Done { final_text, .. } => assert_eq!(final_text, "续跑出来的回复"),
            other => panic!("续跑的 end_turn 才是这一轮的终点: {other:?}"),
        }
    }

    /// API 错误若只挂在 thinking 块上(错误字段会挂该消息每个块),同样要收轮 —— 否则又是一次
    /// 「等满 stall 才恢复」。确认时按 ApiError 收,与文本记录路径同语义。
    #[test]
    fn api_error_on_thinking_record_confirms_as_api_error() {
        let line = r#"{"type":"assistant","sessionId":"s","message":{"id":"m","role":"assistant","stop_reason":"stop_sequence","error":"model_not_found","content":[{"type":"thinking","thinking":"?"}]}}"#;
        let mut tail = TranscriptTail::default();
        assert!(matches!(
            tail.feed(&format!("{line}\n"), &mut |_| {}),
            TailStep::Progress { .. }
        ));
        match tail.confirm_pending_end() {
            Some(TailStep::ApiError { error, .. }) => assert_eq!(error, "model_not_found"),
            other => panic!("thinking 上的 API 错误也要收轮: {other:?}"),
        }
    }

    /// stall 看门狗为什么 16 分钟没兜住:旧实现的「工具在飞」是个只置不复位的标志,5 个 subagent
    /// 早已全部返回,窗口仍停在 30min。改成 tool_use/tool_result 配对计数:结果回来就切回 240s 常规
    /// 窗口,即使终态判定再出新形态,兜底也在分钟级而不是半小时级。
    #[test]
    fn awaiting_tool_clears_once_tool_results_pair_up() {
        let mut tail = TranscriptTail::default();
        assert!(!tail.awaiting_tool());
        // 并行发两个工具(subagent 扇出的形状)。
        let tool = r#"{"type":"assistant","sessionId":"s","message":{"id":"m","role":"assistant","stop_reason":"tool_use","content":[{"type":"tool_use","id":"t1","name":"Task","input":{}}]}}"#;
        tail.feed(&format!("{tool}\n{tool}\n"), &mut |_| {});
        assert!(tail.awaiting_tool(), "工具在飞时必须放宽 stall 窗口");
        assert_eq!(stall_window(tail.awaiting_tool()), TOOL_STALL_WINDOW);
        let result = r#"{"type":"user","sessionId":"s","message":{"role":"user","content":[{"type":"tool_result","content":"ok"}]}}"#;
        tail.feed(&format!("{result}\n"), &mut |_| {});
        assert!(tail.awaiting_tool(), "还有一个工具没回,仍算在飞");
        tail.feed(&format!("{result}\n"), &mut |_| {});
        assert!(!tail.awaiting_tool(), "工具都回来了就该切回常规 stall 窗口");
        assert_eq!(stall_window(tail.awaiting_tool()), STALL_WINDOW);
    }

    /// AskUserQuestion 转录落盘时,真实选项数被记下,供抓屏菜单截断 TUI UI 元素用。
    #[test]
    fn tail_tracks_ask_user_question_count_from_transcript() {
        let mut tail = TranscriptTail::default();
        assert_eq!(tail.ask_user_question_count(), None);
        let tool = r#"{"type":"assistant","sessionId":"s","message":{"id":"m","role":"assistant","stop_reason":"tool_use","content":[{"type":"tool_use","id":"t1","name":"AskUserQuestion","input":{"questions":[{"question":"选哪个?","options":[{"label":"A"},{"label":"B"},{"label":"C"}]}]}}]}}"#;
        tail.feed(&format!("{tool}\n"), &mut |_| {});
        assert_eq!(
            tail.ask_user_question_count(),
            Some(3),
            "应从 input.questions[0].options 记下真实选项数"
        );
        // 非 AskUserQuestion 工具不覆盖这个计数。
        let other = r#"{"type":"assistant","sessionId":"s","message":{"id":"m2","role":"assistant","stop_reason":"tool_use","content":[{"type":"tool_use","id":"t2","name":"Bash","input":{"command":"ls"}}]}}"#;
        tail.feed(&format!("{other}\n"), &mut |_| {});
        assert_eq!(
            tail.ask_user_question_count(),
            Some(3),
            "非 AskUserQuestion 工具不应覆盖已记的计数"
        );
        // reset_turn 清掉,防跨轮泄漏。
        tail.reset_turn();
        assert_eq!(tail.ask_user_question_count(), None);
    }

    /// 抓屏菜单按转录侧真实选项数截断,去掉尾部的 TUI UI 元素。
    #[test]
    fn filter_ask_user_question_menu_truncates_ui_elements() {
        use summon_core::interactive::Choice;
        let menu = InteractiveState {
            title: "选哪个?".into(),
            options: vec![
                Choice {
                    index: 1,
                    label: "Staging".into(),
                },
                Choice {
                    index: 2,
                    label: "Production".into(),
                },
                Choice {
                    index: 3,
                    label: "Type something.".into(),
                },
                Choice {
                    index: 4,
                    label: "Chat about this".into(),
                },
            ],
            raw: String::new(),
        };
        // 转录侧说有 2 个真实选项 → 截断到前 2 个。
        let filtered = filter_ask_user_question_menu(menu.clone(), Some(2));
        assert_eq!(filtered.options.len(), 2);
        assert_eq!(filtered.options[0].label, "Staging");
        assert_eq!(filtered.options[1].label, "Production");
        assert_eq!(filtered.title, "选哪个?", "title/raw 不动");
        // 非 AskUserQuestion(None)→ 原样。
        let untouched = filter_ask_user_question_menu(menu.clone(), None);
        assert_eq!(untouched.options.len(), 4);
        // 计数与菜单选项数对不上(多题场景第二题选项数不同 / TUI 改版)→ 不截断,原样发卡。
        let untouched = filter_ask_user_question_menu(menu.clone(), Some(3));
        assert_eq!(untouched.options.len(), 4, "计数对不上时安全降级,不截");
        // 计数 0(开放式问题,无选项)→ 原样,交给降级卡。
        let untouched = filter_ask_user_question_menu(menu, Some(0));
        assert_eq!(untouched.options.len(), 4);
    }

    /// 中介卡未达 → 后续探测节拍重发,直到送达或预算用尽。**上限必须存在**:菜单是静止画面,
    /// 没有上限就是对着一个发不出去的接口每 2s 敲一次,直到 stall 才停。
    #[test]
    fn an_undelivered_card_is_retried_until_the_budget_runs_out() {
        let mut budget = CardSendBudget::default();
        let sig = Some(7);
        for attempt in 1..INTERACTIVE_SEND_TRIES {
            assert!(!budget.spent(sig), "第 {attempt} 次失败后仍该重发");
            budget.note_failure(sig);
        }
        assert!(!budget.spent(sig), "最后一次机会还没用");
        budget.note_failure(sig);
        assert!(budget.spent(sig), "预算用尽后停止重发");
    }

    /// 重发期间 stall 看门狗必须让位:降级卡的发送门就是 `should_stall`,同一次迭代里的看门狗
    /// 若照常收轮,第二次机会永远轮不到。预算用尽的那一拍让位结束,收轮不被无限推迟。
    #[test]
    fn a_card_still_being_resent_holds_off_the_stall_watchdog() {
        let mut budget = CardSendBudget::default();
        assert!(!budget.retrying(), "没有未达的卡就不该拦看门狗");
        budget.note_failure(None);
        assert!(budget.retrying(), "第一次未达之后要等下一拍重发");
        for _ in 1..INTERACTIVE_SEND_TRIES {
            budget.note_failure(None);
        }
        assert!(budget.spent(None));
        assert!(!budget.retrying(), "预算用尽就把收轮权交回看门狗");
        budget.clear();
        assert!(!budget.retrying());
    }

    /// 预算按「哪一张卡」计:菜单翻页 / 从菜单退到降级卡都是另一张卡,不该继承上一张的失败次数
    /// ——否则一次网络抖动会让后面每一张卡都只剩一次机会。
    #[test]
    fn each_card_gets_its_own_send_budget() {
        let mut budget = CardSendBudget::default();
        for _ in 0..INTERACTIVE_SEND_TRIES {
            budget.note_failure(Some(1));
        }
        assert!(budget.spent(Some(1)));
        assert!(!budget.spent(Some(2)), "下一屏菜单是新的一张卡");
        assert!(!budget.spent(None), "降级卡也是新的一张卡");
        budget.note_failure(Some(2));
        assert!(!budget.spent(Some(1)), "换 key 后旧签名的账已作废");
        // 转录一有新字节就复位:那说明 claude 已经在推进,菜单已成过去式。
        budget.clear();
        assert!(!budget.spent(Some(2)));
    }

    /// 判据存在不等于被用上:钉住探测段真的按「送达」分流 —— 未达不置签名(下拍重发)、
    /// 降级卡未达不置等待窗(让位给 stall 看门狗)。改掉任一条,发卡失败就会退回「发一次就当发过」。
    #[test]
    fn the_probe_only_stops_resending_a_card_that_was_delivered() {
        let production = production_source();
        let start = production.find("pub fn run_turn(").expect("run_turn 定义");
        let body = &production[start..];
        let probe = body
            .find("if super::program_status::probe_due(")
            .expect("抓屏探测段");
        let watchdog = body[probe..]
            .find("// 既没识别出菜单")
            .expect("stall 看门狗收口");
        let segment = &body[probe..probe + watchdog];
        assert!(
            segment.contains("if sent || card_budget.spent(Some(sig)) {"),
            "菜单卡必须送达(或预算用尽)才停止重发"
        );
        assert!(
            segment.contains("card_budget.note_failure(Some(sig));"),
            "菜单卡未达必须记账,否则重发没有止损"
        );
        assert!(
            segment.contains("let sent = delivery == MediationCard::Settled;"),
            "让位的一拍不算送达:不置签名,让位结束后同一张菜单还要能发"
        );
        assert!(
            segment.contains("if delivery == MediationCard::Settled {"),
            "降级卡未达不得置 degrade_fired / 等待窗"
        );
        assert!(
            segment.contains("if delivery == MediationCard::Undelivered {"),
            "让位的这一拍不能当成未达扣重发预算"
        );
        // 看门狗紧跟在探测段之后、同一次迭代里跑。它不让位,降级卡的重发就是死代码。
        let watchdog_block = &body[probe + watchdog..];
        let block_end = watchdog_block.find("// 计时心跳").expect("看门狗段的下界");
        assert!(
            watchdog_block[..block_end].contains("&& !card_budget.retrying()"),
            "stall 看门狗必须给正在重发的卡让位"
        );
    }

    /// 冷启 seed 轮以纯 thinking 收尾时,不该傻等满封顶(否则第一条用户消息被拖十分钟)。
    #[test]
    fn wait_first_turn_end_ends_on_thinking_only_end_turn() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("sid.jsonl");
        std::fs::write(&p, rec("thinking", "想完了", Some("end_turn"))).unwrap();
        let r = wait_first_turn_end(
            &crate::runtime::executor::LocalExecutor,
            &p,
            0,
            Duration::from_millis(1),
            Duration::from_millis(1),
            Instant::now() + Duration::from_secs(5),
            &mut || true,
        );
        assert!(
            matches!(r, Ok(SeedWait::Ended)),
            "纯 thinking 收尾也是收尾,应 Ok(Ended),实得 {r:?}"
        );
    }

    #[test]
    fn is_turn_end_only_for_end_turn() {
        assert!(is_turn_end(Some("end_turn")));
        // stop_sequence 是 claude 伪影(如 "No response requested."),不是真 turn-end
        assert!(!is_turn_end(Some("stop_sequence")));
        assert!(!is_turn_end(Some("tool_use")));
        assert!(!is_turn_end(None));
    }

    /// 钉子:CLI 拒绝未知斜杠命令时写的是 `type=system` / `subtype=informational` 行,
    /// **不是 assistant 消息** —— 解析层(`parse_stream_json_line` 只认 `subtype=init`)直接丢弃,
    /// `TranscriptTail` 也不会把它并进 `assistant` 累积。样本取自真机转录
    /// (`~/.claude/projects/…/cbcfd6db-….jsonl`,2026-07-21 用户误发 `/introduce` 那一轮)。
    ///
    /// 所以**不要**在 assistant 累积文本里 `contains("from unknown skill")` 做兜底:真机永不命中,
    /// 反而会在 agent 正常讨论这个错误时误截断整轮。防线应留在 dispatcher 侧(summon 自有命令
    /// 不透传 CLI),这里只固化「错误行进不了累积」这个事实。
    #[test]
    fn unknown_skill_error_line_never_reaches_assistant_accumulation() {
        let real = r#"{"parentUuid":"f2d4acd0","isSidechain":false,"type":"system","subtype":"informational","content":"Args from unknown skill: 互相介绍下, starlight现在是开发","isMeta":false,"level":"warning","userType":"external","sessionId":"cbcfd6db"}"#;
        let mut tail = TranscriptTail::default();
        let step = tail.feed(&format!("{real}\n"), &mut |_| {});
        // 该行不产生任何 AgentMessage → 无流式更新、非终态。
        match step {
            TailStep::Progress { stream, tools } => {
                assert!(stream.is_none(), "system 行不该产生流式累积: {stream:?}");
                assert!(tools.is_empty());
            }
            other => panic!("system 行不该触发终态: {other:?}"),
        }
        assert!(
            !tail.accumulated().contains("unknown skill"),
            "错误文本混进了 assistant 累积,基于累积文本的检测会误判"
        );
    }

    /// 回归(2026-07-22 现场):blue-wing 在跑一个慢工具期间被判「本轮结束」,上层贴 DONE 并把
    /// 半截文本当终态发出去,而 claude 还在跑。实测该轮转录静默 272s,只超阈值 32s。
    /// 工具执行期转录必然一个字节不产出,所以「静默」不能等价于「结束」——最后见到 tool_use 就是
    /// claude 活着的正面证据,此时必须放宽而不是收尾。
    #[test]
    fn stall_window_is_widened_while_a_tool_is_in_flight() {
        assert_eq!(stall_window(false), STALL_WINDOW);
        assert!(
            stall_window(true) > STALL_WINDOW,
            "工具在飞时不能沿用常规 stall 窗口,否则慢工具被误判成收尾"
        );
        // 现场那次静默 272s:常规窗口会收尾,工具在飞时必须继续等。
        let observed = Duration::from_secs(272);
        assert!(
            observed >= stall_window(false),
            "复现现场:272s 会触发常规 stall"
        );
        assert!(observed < stall_window(true), "工具在飞时 272s 不该收尾");
        // 仍然有界:工具真挂死时不会把 chat 线程永远钉住。
        assert!(stall_window(true) <= Duration::from_secs(60 * 60));
    }

    /// 复现 2026-08-04 SUM-14 现场:最后一条 tool_result 落在 18:29:04,agent 随即开始生成一份
    /// 159 行设计文档(期间转录零字节、无工具在飞),240s 后被判 stall;而它实际又干到 18:40:59
    /// 才提交完。画面在动就不该收尾。
    #[test]
    fn generating_model_with_live_pane_is_not_stalled() {
        let silent = Duration::from_secs(240);
        // 旧判据(只看转录静默)会收尾 —— 钉死这就是当年的 bug。
        assert!(
            silent >= stall_window(false),
            "复现:240s 触发旧的常规 stall"
        );
        // 新判据:pane 每秒在刷新 spinner,存活证据是新鲜的 → 不收尾。
        assert!(
            !should_stall(silent, Duration::from_secs(1), false),
            "模型在生成、画面在动,不能按 stall 收尾"
        );
        // 现场实际静默了 715s(18:29:04 → 18:40:59),同样不该收尾。
        assert!(!should_stall(
            Duration::from_secs(715),
            Duration::from_secs(1),
            false
        ));
    }

    /// 画面静止 = 没有任何存活证据,常规窗口照旧收尾(别把修复做成「永不 stall」)。
    #[test]
    fn frozen_pane_still_stalls_on_the_regular_window() {
        let silent = Duration::from_secs(240);
        assert!(
            should_stall(silent, silent, false),
            "转录静默且画面静止 → 没有存活证据,该收尾"
        );
        assert!(
            !should_stall(Duration::from_secs(239), Duration::from_secs(239), false),
            "未到窗口不收尾"
        );
    }

    /// 硬顶:pane 因时钟/光标之类永远在动时,存活证据也不能无限延期。
    #[test]
    fn live_pane_cannot_defer_past_the_hard_ceiling() {
        assert!(
            should_stall(HARD_STALL_CEILING, Duration::from_secs(0), false),
            "转录零产出满硬顶后,画面再动也要收尾"
        );
        assert!(
            !should_stall(
                HARD_STALL_CEILING - Duration::from_secs(1),
                Duration::from_secs(0),
                false
            ),
            "未到硬顶且有存活证据 → 继续等"
        );
    }

    /// 工具在飞的既有语义不能被这次改动改坏:272s 现场仍然继续等。
    #[test]
    fn tool_in_flight_semantics_survive_the_liveness_gate() {
        let observed = Duration::from_secs(272);
        assert!(
            !should_stall(observed, observed, true),
            "工具在飞时 272s 静默(连画面都静止)仍不该收尾"
        );
        assert!(
            should_stall(observed, observed, false),
            "同样的静默,没有工具在飞且画面静止时该收尾"
        );
    }

    /// API 错误(model_not_found 等)的 assistant 消息应触发 TailStep::ApiError 终态,
    /// 而不是被当作中间 Progress 等 stall 超时。stop_reason=stop_sequence 不是 end_turn,
    /// 但 error 字段非空 → 立即收尾。
    #[test]
    fn api_error_triggers_immediate_terminal_state() {
        let line = r#"{"type":"assistant","sessionId":"s","message":{"id":"m","role":"assistant","stop_reason":"stop_sequence","error":"model_not_found","isApiErrorMessage":true,"apiErrorStatus":404,"content":[{"type":"text","text":"There's an issue with the selected model."}]}}"#;
        let mut tail = TranscriptTail::default();
        match tail.feed(&format!("{line}\n"), &mut |_| {}) {
            TailStep::ApiError {
                final_text, error, ..
            } => {
                assert_eq!(error, "model_not_found");
                assert_eq!(final_text, "There's an issue with the selected model.");
            }
            TailStep::Done { .. } => panic!("API 错误不应走 Done 分支"),
            TailStep::Progress { .. } => panic!("API 错误应立即收尾,不应是 Progress"),
        }
    }

    #[test]
    fn live_claude_session_limit_is_an_immediate_terminal_state() {
        let line = r#"{"type":"assistant","timestamp":"2026-09-02T09:20:55.766Z","userType":"external","isSidechain":false,"message":{"role":"assistant","stop_reason":"stop_sequence","content":[{"type":"text","text":"You've hit your session limit · resets 6:10pm (Asia/Singapore)"}]},"quotaLimits":{"status":"rejected","resetsAt":1788343800,"unifiedRateLimitFallbackAvailable":false,"rateLimitType":"five_hour","overageStatus":"rejected","overageDisabledReason":"org_level_disabled","isUsingOverage":false}}"#;
        let mut tail = TranscriptTail::default();
        match tail.feed(&format!("{line}\n"), &mut |_| {}) {
            TailStep::ApiError { final_text, .. } => {
                assert_eq!(
                    final_text,
                    "You've hit your session limit · resets 6:10pm (Asia/Singapore)"
                );
                assert_eq!(
                    tail.session_limit
                        .as_ref()
                        .map(|limit| limit.reset_at.timestamp()),
                    Some(1788343800)
                );
            }
            TailStep::Done { .. } => panic!("额度终态不应走正常完成"),
            TailStep::Progress { .. } => panic!("额度终态不应继续等待 stall"),
        }
    }

    #[test]
    fn unstructured_session_limit_text_is_not_enough_to_end_the_turn() {
        let line = r#"{"type":"assistant","timestamp":"2026-07-16T09:47:23.129Z","userType":"external","isSidechain":false,"message":{"role":"assistant","stop_reason":"stop_sequence","content":[{"type":"text","text":"You've hit your session limit · resets 6:40pm (Asia/Singapore)"}]}}"#;
        let mut tail = TranscriptTail::default();
        match tail.feed(&format!("{line}\n"), &mut |_| {}) {
            TailStep::Progress { .. } => assert!(tail.session_limit.is_none()),
            TailStep::ApiError { .. } => panic!("展示文本不能单独证明五小时额度终态"),
            TailStep::Done { .. } => panic!("stop_sequence 不应走正常完成"),
        }
    }

    /// 正常 end_turn 消息不应触发 ApiError 终态。
    #[test]
    fn normal_end_turn_does_not_trigger_api_error() {
        let line = r#"{"type":"assistant","sessionId":"s","message":{"id":"m","role":"assistant","stop_reason":"end_turn","content":[{"type":"text","text":"hello"}]}}"#;
        let mut tail = TranscriptTail::default();
        match tail.feed(&format!("{line}\n"), &mut |_| {}) {
            TailStep::Done { final_text, .. } => assert_eq!(final_text, "hello"),
            TailStep::ApiError { .. } => panic!("正常消息不应走 ApiError"),
            TailStep::Progress { .. } => panic!("end_turn 应收尾"),
        }
    }

    /// 会话镜像原型核心:持续 tail 一个不断追加 assistant `end_turn` 的转录文件,**每个 turn** 都被
    /// 转发(含「飞书没注入、纯追加」的轮 —— 模拟用户终端直敲)。逐轮追加(等上一轮被消费再追下一轮)
    /// 避免「两 turn 挤进同一次读」的已知边界。
    #[test]
    fn terminal_baseline_forwards_each_live_appended_turn() {
        use std::io::Write as _;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::{Arc, Mutex};
        use std::time::{Duration, Instant};

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mirror.jsonl");
        std::fs::write(&path, "").unwrap();
        let append = |text: &str| {
            let line = serde_json::json!({
                "type": "assistant",
                "sessionId": "s",
                "message": {"role": "assistant", "stop_reason": "end_turn",
                    "content": [{"type": "text", "text": text}]}
            })
            .to_string()
                + "\n";
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            f.write_all(line.as_bytes()).unwrap();
        };

        let stop = Arc::new(AtomicBool::new(false));
        let got = Arc::new(Mutex::new(Vec::<String>::new()));
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (p2, s2, g2) = (path.clone(), stop.clone(), got.clone());
        let h = std::thread::spawn(move || {
            let mut on_turn =
                |kind: MirrorTurnKind, _: &MirrorMarkerEvidence, _: u64, _: &[u8], t: &str| {
                    if kind == MirrorTurnKind::Reply {
                        g2.lock().unwrap().push(t.to_string());
                    }
                    Ok(())
                };
            let mut on_off = |_: u64, _: &FileTailCheckpoint| Ok(());
            tail_forward_turns(
                &crate::runtime::executor::LocalExecutor,
                &p2,
                0,
                FileTailCheckpoint::capture_at(&crate::runtime::executor::LocalExecutor, &p2, 0)
                    .unwrap(),
                Duration::from_millis(10),
                Duration::from_millis(30),
                &s2,
                None,
                false,
                None, // catchup_floor:本测不验停机补发
                &mut on_turn,
                &mut on_off,
                &mut || ready_tx.send(()).unwrap(),
            )
            .unwrap();
        });
        ready_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("TerminalBoundary 起点应先完成 exact ready fence");
        let wait_len = |n: usize| {
            let dl = Instant::now() + Duration::from_secs(3);
            while got.lock().unwrap().len() < n && Instant::now() < dl {
                std::thread::sleep(Duration::from_millis(15));
            }
        };
        append("你好"); // 飞书注入触发的轮(模拟)
        wait_len(1);
        append("再见"); // 终端直敲触发的轮(飞书没注入,镜像仍转发)
        wait_len(2);
        stop.store(true, Ordering::Relaxed);
        h.join().unwrap();
        assert_eq!(
            *got.lock().unwrap(),
            vec!["你好".to_string(), "再见".to_string()],
            "每个 assistant turn 都应被转发(含纯终端触发的)"
        );
    }

    /// 入向跨会话消息行(SendMessage @peer 投递的真实落盘形态)应渲染成可回显块;
    /// `queue-operation` 记账行与 tool_result 行不得命中(前者防 enqueue+投递双渲染)。
    /// 后台任务完成通知:content 是**字符串**型,`parse_stream_json_line` 整行丢弃 ——
    /// 不旁路提取的话,"后台任务跑完了"在飞书侧完全不可见,用户只看到一个不再动的转圈。
    ///
    /// 夹具照真机转录写(2026-08-12 本会话):整段含 task-id / tool-use-id / output-file /
    /// summary,只有 summary 是给人读的。
    #[test]
    fn task_notice_summary_extracts_only_the_summary() {
        let line = r#"{"type":"user","message":{"role":"user","content":"<task-notification>\n<task-id>b437izj0t</task-id>\n<tool-use-id>toolu_01EpgU8</tool-use-id>\n<output-file>/tmp/x.output</output-file>\n<status>completed</status>\n<summary>Background command \"等待并核对 daemon 启动后状态与 boot 日志\" completed (exit code 0)</summary>\n</task-notification>"}}"#;
        let got = super::task_notice_summary(line).expect("应提取到 summary");
        assert!(got.contains("等待并核对 daemon 启动后状态"), "实得 {got}");
        assert!(
            got.contains("exit code 0"),
            "退出码要留着:失败与成功得看得出区别"
        );
        // task-id / tool-use-id / 文件路径是给 agent 认领用的,对读卡片的人是噪音。
        assert!(!got.contains("b437izj0t"), "不该带 task-id:{got}");
        assert!(!got.contains("toolu_01EpgU8"), "不该带 tool-use-id:{got}");
        assert!(
            !got.contains("/tmp/x.output"),
            "不该带 output-file 路径:{got}"
        );

        // 非通知的 user 行不得误命中(工具结果里出现这些字眼很常见)。
        let plain = r#"{"type":"user","message":{"role":"user","content":"我们聊聊 task-notification 这个结构"}}"#;
        assert_eq!(
            super::task_notice_summary(plain),
            None,
            "只提到名字不算通知"
        );
        // 数组型 content(正常 tool_result)走原路径,不归这里管。
        let arr = r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","content":"ok"}]}}"#;
        assert_eq!(super::task_notice_summary(arr), None);
        // assistant 行一律不看。
        let asst = r#"{"type":"assistant","message":{"content":"<task-notification><summary>x</summary></task-notification>"}}"#;
        assert_eq!(super::task_notice_summary(asst), None);
        // summary 为空 = 没信息,不该渲染成一行空壳。
        let blank = r#"{"type":"user","message":{"role":"user","content":"<task-notification><summary>   </summary></task-notification>"}}"#;
        assert_eq!(super::task_notice_summary(blank), None);
    }

    /// 新版(Claude Code 2.1.235+)task-notification 走 `queue-operation` enqueue,不再是 user 消息。
    #[test]
    fn task_notice_summary_parses_queue_operation_enqueue() {
        // 真机 2.1.235 转录原样(enqueue 行)。
        let line = r#"{"type":"queue-operation","operation":"enqueue","content":"<task-notification>\n<task-id>bgs6nk69k</task-id>\n<tool-use-id>call_fowz8t6sfp4a9a9hyp0cpl6t</tool-use-id>\n<output-file>/tmp/x.output</output-file>\n<status>completed</status>\n<summary>Background command \"Configure and build parity probe binary\" completed (exit code 0)</summary>\n</task-notification>"}"#;
        let got = super::task_notice_summary(line).expect("enqueue 行应提取到 summary");
        assert!(
            got.contains("Configure and build parity probe binary"),
            "实得 {got}"
        );
        assert!(got.contains("exit code 0"), "退出码要留着:{got}");
        assert!(!got.contains("bgs6nk69k"), "不该带 task-id:{got}");

        // remove 销账行不得命中(同内容,但 operation 是 remove)。
        let remove = r#"{"type":"queue-operation","operation":"remove","content":"<task-notification><summary>Background command \"x\" completed (exit code 0)</summary></task-notification>"}"#;
        assert_eq!(
            super::task_notice_summary(remove),
            None,
            "remove 是销账行,不渲染"
        );

        // enqueue 但 content 不是 task-notification → 不误命中。
        let other = r#"{"type":"queue-operation","operation":"enqueue","content":"<cross-session-message from=\"x\">hi</cross-session-message>"}"#;
        assert_eq!(
            super::task_notice_summary(other),
            None,
            "非通知的 enqueue 不命中"
        );

        // 缺 operation 字段 → 不命中(不当成 enqueue)。
        let no_op = r#"{"type":"queue-operation","content":"<task-notification><summary>x</summary></task-notification>"}"#;
        assert_eq!(super::task_notice_summary(no_op), None);
    }

    #[test]
    fn peer_inbound_notice_parses_delivery_line_only() {
        let delivered = r#"{"type":"user","message":{"role":"user","content":"Another Claude session sent a message: <cross-session-message from=\"uds:/tmp/cc-socks/1.sock\" from-name=\"summon:soul-flare-5-x\" from-mode=\"bypass\">\n派单正文第一行\n第二行\n</cross-session-message>"}}"#;
        let notice = peer_inbound_notice(delivered).expect("投递行应命中");
        let line = format!("{:?}", notice);
        assert!(
            line.contains("summon:soul-flare-5-x"),
            "应取 from-name:{line}"
        );
        assert!(line.contains("派单正文第一行"), "应含正文:{line}");
        assert!(
            !line.contains("uds:/tmp"),
            "有 from-name 时不应露 socket 地址:{line}"
        );

        let queue_op = r#"{"type":"queue-operation","operation":"enqueue","content":"<cross-session-message from=\"x\">hi"}"#;
        assert!(
            peer_inbound_notice(queue_op).is_none(),
            "记账行不得成条(防双发)"
        );

        let tool_result = r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"ok"}]}}"#;
        assert!(
            peer_inbound_notice(tool_result).is_none(),
            "tool_result 行不得命中"
        );
    }

    /// 入向与出向(SendMessage tool_use)都挂在本轮的事件窗口上,收口时一起可取;出向优先取 summary。
    #[test]
    fn peer_notices_flow_through_turn() {
        let inbound = r#"{"type":"user","message":{"role":"user","content":"Another Claude session sent a message: <cross-session-message from=\"uds:/x\" from-name=\"peer-a\">帮我看看</cross-session-message>"}}"#;
        let send = r#"{"type":"assistant","sessionId":"s","message":{"role":"assistant","stop_reason":"tool_use","content":[{"type":"tool_use","id":"t1","name":"SendMessage","input":{"to":"peer-b","summary":"回执","message":"内容"}}]}}"#;
        let done = r#"{"type":"assistant","sessionId":"s","message":{"role":"assistant","stop_reason":"end_turn","content":[{"type":"text","text":"收到"}]}}"#;
        let mut tail = TranscriptTail::default();
        let mut noop = |_: &AgentMessage| {};
        match tail.feed(&format!("{inbound}\n{send}\n{done}\n"), &mut noop) {
            TailStep::Done { final_text, .. } => assert_eq!(final_text, "收到"),
            other => panic!("应收轮,得到 {other:?}"),
        }
        let card = tail.take_notice_block().expect("本轮有事件");
        assert!(
            card.starts_with("**通知** · 跨会话 2 条\n"),
            "入向+出向各一条:{card}"
        );
        assert!(card.contains("⇄ 来自 `peer-a`:帮我看看"), "{card}");
        assert!(
            card.contains("⇄ 发往 `peer-b`:回执"),
            "有 summary 用 summary:{card}"
        );
    }

    /// 镜像端到端:peer 派单 + 回应轮,镜像**先**转发跨会话块、**再**转发回复文本。
    #[test]
    fn mirror_forwards_peer_block_before_reply() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::{Arc, Mutex};
        use std::time::{Duration, Instant};

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mirror.jsonl");
        let inbound = r#"{"type":"user","message":{"role":"user","content":"Another Claude session sent a message: <cross-session-message from=\"uds:/x\" from-name=\"peer-a\">求助正文</cross-session-message>"}}"#;
        let done = r#"{"type":"assistant","sessionId":"s","message":{"role":"assistant","stop_reason":"end_turn","content":[{"type":"text","text":"这就去办"}]}}"#;
        std::fs::write(&path, "").unwrap();

        let stop = Arc::new(AtomicBool::new(false));
        let got = Arc::new(Mutex::new(Vec::<(MirrorTurnKind, String)>::new()));
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (p2, s2, g2) = (path.clone(), stop.clone(), got.clone());
        let h = std::thread::spawn(move || {
            let mut on_turn =
                |kind: MirrorTurnKind, _: &MirrorMarkerEvidence, _: u64, _: &[u8], t: &str| {
                    g2.lock().unwrap().push((kind, t.to_string()));
                    Ok(())
                };
            let mut on_off = |_: u64, _: &FileTailCheckpoint| Ok(());
            tail_forward_turns(
                &crate::runtime::executor::LocalExecutor,
                &p2,
                0,
                FileTailCheckpoint::capture_at(&crate::runtime::executor::LocalExecutor, &p2, 0)
                    .unwrap(),
                Duration::from_millis(10),
                Duration::from_millis(30),
                &s2,
                None,
                false,
                None, // catchup_floor:本测不验停机补发
                &mut on_turn,
                &mut on_off,
                &mut || ready_tx.send(()).unwrap(),
            )
            .unwrap();
        });
        ready_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("live peer 轮必须在 Ready 之后追加");
        std::fs::write(&path, format!("{inbound}\n{done}\n")).unwrap();
        let dl = Instant::now() + Duration::from_secs(3);
        while got.lock().unwrap().len() < 2 && Instant::now() < dl {
            std::thread::sleep(Duration::from_millis(15));
        }
        stop.store(true, Ordering::Relaxed);
        h.join().unwrap();
        let got = got.lock().unwrap();
        assert_eq!(got.len(), 2, "{got:?}");
        assert!(
            got[0].0 == MirrorTurnKind::Notice
                && got[0].1.contains("⇄ 来自 `peer-a`")
                && got[0].1.contains("求助正文"),
            "{got:?}"
        );
        assert_eq!(got[1], (MirrorTurnKind::Reply, "这就去办".into()));
    }

    /// 终端里直敲的一轮也按常驻约定带着总结标签收尾;镜像回推到群里的正文不带它。
    #[test]
    fn mirror_forwards_the_reply_without_the_feed_tag() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::{Arc, Mutex};
        use std::time::{Duration, Instant};

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mirror-feed.jsonl");
        let ask = r#"{"type":"user","message":{"role":"user","content":"帮我看看"}}"#;
        let done = r#"{"type":"assistant","sessionId":"s","message":{"role":"assistant","stop_reason":"end_turn","content":[{"type":"text","text":"这就去办\n<summon-feed>已接手</summon-feed>"}]}}"#;
        std::fs::write(&path, "").unwrap();

        let stop = Arc::new(AtomicBool::new(false));
        let got = Arc::new(Mutex::new(Vec::<(MirrorTurnKind, String)>::new()));
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (p2, s2, g2) = (path.clone(), stop.clone(), got.clone());
        let h = std::thread::spawn(move || {
            let mut on_turn =
                |kind: MirrorTurnKind, _: &MirrorMarkerEvidence, _: u64, _: &[u8], t: &str| {
                    g2.lock().unwrap().push((kind, t.to_string()));
                    Ok(())
                };
            let mut on_off = |_: u64, _: &FileTailCheckpoint| Ok(());
            tail_forward_turns(
                &crate::runtime::executor::LocalExecutor,
                &p2,
                0,
                FileTailCheckpoint::capture_at(&crate::runtime::executor::LocalExecutor, &p2, 0)
                    .unwrap(),
                Duration::from_millis(10),
                Duration::from_millis(30),
                &s2,
                None,
                false,
                None,
                &mut on_turn,
                &mut on_off,
                &mut || ready_tx.send(()).unwrap(),
            )
            .unwrap();
        });
        ready_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("镜像轮必须在 Ready 之后追加");
        std::fs::write(&path, format!("{ask}\n{done}\n")).unwrap();
        let dl = Instant::now() + Duration::from_secs(3);
        while got.lock().unwrap().is_empty() && Instant::now() < dl {
            std::thread::sleep(Duration::from_millis(15));
        }
        stop.store(true, Ordering::Relaxed);
        h.join().unwrap();
        let got = got.lock().unwrap();
        assert_eq!(&*got, &[(MirrorTurnKind::Reply, "这就去办".to_string())]);
    }

    /// mirror 必须把 terminal 前的有界 marker 证据同时交给 peer/reply 仲裁；peer 不取得 reply
    /// permit，但仍需据此识别主路径轮，避免把同一过程块重复推到群里。
    #[test]
    fn mirror_binds_exact_marker_to_terminal_reply_only() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::{Arc, Mutex};
        use std::time::{Duration, Instant};

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mirror-marker.jsonl");
        let marker = "summon-transport-turn:123e4567-e89b-12d3-a456-426614174000";
        let user = serde_json::json!({
            "type": "user",
            "message": {"role": "user", "content": format!("任务\n<!-- {marker} -->")}
        });
        let peer = r#"{"type":"assistant","message":{"role":"assistant","stop_reason":"tool_use","content":[{"type":"tool_use","name":"SendMessage","input":{"to":"peer","summary":"过程"}}]}}"#;
        let steer = r#"{"type":"attachment","attachment":{"type":"queued_command","prompt":"追加\n<!-- summon-transport-turn:123e4567-e89b-12d3-a456-426614174001 -->"}}"#;
        let done = r#"{"type":"assistant","message":{"role":"assistant","stop_reason":"end_turn","content":[{"type":"text","text":"最终答案"}]}}"#;
        let transcript = format!("{user}\n{peer}\n{steer}\n{done}\n");
        std::fs::write(&path, "").unwrap();

        let stop = Arc::new(AtomicBool::new(false));
        let got = Arc::new(Mutex::new(
            Vec::<(MirrorTurnKind, Vec<String>, u64, String)>::new(),
        ));
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (p2, s2, g2) = (path.clone(), stop.clone(), got.clone());
        let h = std::thread::spawn(move || {
            let mut on_turn = |kind: MirrorTurnKind,
                               evidence: &MirrorMarkerEvidence,
                               boundary: u64,
                               _: &[u8],
                               text: &str| {
                g2.lock().unwrap().push((
                    kind,
                    evidence.consumed().to_vec(),
                    boundary,
                    text.to_string(),
                ));
                Ok(())
            };
            let mut on_off = |_: u64, _: &FileTailCheckpoint| Ok(());
            tail_forward_turns(
                &crate::runtime::executor::LocalExecutor,
                &p2,
                0,
                FileTailCheckpoint::capture_at(&crate::runtime::executor::LocalExecutor, &p2, 0)
                    .unwrap(),
                Duration::from_millis(10),
                Duration::from_millis(30),
                &s2,
                None,
                false,
                None, // catchup_floor:本测不验停机补发
                &mut on_turn,
                &mut on_off,
                &mut || ready_tx.send(()).unwrap(),
            )
            .unwrap();
        });
        ready_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("typed marker 轮必须在 Ready 之后追加");
        std::fs::write(&path, transcript).unwrap();
        let file_len = std::fs::metadata(&path).unwrap().len();
        let deadline = Instant::now() + Duration::from_secs(3);
        while got.lock().unwrap().len() < 2 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(15));
        }
        stop.store(true, Ordering::Relaxed);
        h.join().unwrap();

        let got = got.lock().unwrap();
        assert_eq!(got.len(), 2, "通知卡 + 最终答案各一次:{got:?}");
        assert_eq!(got[0].0, MirrorTurnKind::Notice);
        assert_eq!(
            got[0].1,
            vec![
                marker.to_string(),
                "summon-transport-turn:123e4567-e89b-12d3-a456-426614174001".into()
            ],
            "peer 必须携带同一 terminal 的 typed marker 证据供零网络仲裁"
        );
        assert!(got[0].3.contains("⇄ 发往 `peer`"), "{:?}", got[0]);
        assert_eq!(
            got[1],
            (
                MirrorTurnKind::Reply,
                vec![
                    marker.to_string(),
                    "summon-transport-turn:123e4567-e89b-12d3-a456-426614174001".into()
                ],
                file_len,
                "最终答案".into()
            ),
            "同一 terminal 必须保留主 prompt 与后续 steer 候选"
        );
    }

    /// **修 3 的前提复现**(2026-08-13 实测样本:18:00:07 steer 注入 → 18:04:24 孤儿 terminal):
    /// steer 消息若在 claude 写完 end_turn **之后**才被消费,它不并进原轮,而是自成一轮。
    /// 此时孤儿轮 terminal 的 evidence.consumed() 里**只有** steer marker、没有原轮 marker
    /// (evidence 按轮重置)——tracker 若不认识 steer marker,这一轮的答案就永久丢失。
    #[test]
    fn steer_consumed_after_end_turn_forms_an_orphan_turn_with_only_the_steer_marker() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("steer-after-end-turn.jsonl");
        let main_marker = "summon-transport-turn:123e4567-e89b-12d3-a456-426614174000";
        let steer_marker = "summon-transport-turn:123e4567-e89b-12d3-a456-426614174001";
        let main_user = serde_json::json!({
            "type": "user",
            "message": {"role": "user", "content": format!("主任务\n<!-- {main_marker} -->")}
        });
        // steer 注入落在轮中:先只有 enqueue 记录(排进 claude 队列,尚未消费)。
        let steer_enqueue = serde_json::json!({
            "type": "queue-operation",
            "content": format!("追加消息\n<!-- {steer_marker} -->")
        });
        let main_done = r#"{"type":"assistant","message":{"role":"assistant","stop_reason":"end_turn","content":[{"type":"text","text":"主轮答案"}]}}"#;
        // end_turn 之后 claude 才出队消费 steer:自成一轮,消费记录落成新的 user 行。
        let steer_user = serde_json::json!({
            "type": "user",
            "message": {"role": "user", "content": format!("追加消息\n<!-- {steer_marker} -->")}
        });
        let orphan_done = r#"{"type":"assistant","message":{"role":"assistant","stop_reason":"end_turn","content":[{"type":"text","text":"② 补完了"}]}}"#;
        let transcript =
            format!("{main_user}\n{steer_enqueue}\n{main_done}\n{steer_user}\n{orphan_done}\n");
        std::fs::write(&path, "").unwrap();

        let stop = AtomicBool::new(false);
        let mut got = Vec::<(MirrorTurnKind, Vec<String>, Vec<String>, String)>::new();
        tail_forward_turns(
            &crate::runtime::executor::LocalExecutor,
            &path,
            0,
            FileTailCheckpoint::capture_at(&crate::runtime::executor::LocalExecutor, &path, 0)
                .unwrap(),
            Duration::from_millis(1),
            Duration::from_millis(1),
            &stop,
            None,
            false,
            None, // catchup_floor:孤儿 steer 轮是本代 live 产物,与停机补发无关
            &mut |kind, evidence, _, _, text| {
                got.push((
                    kind,
                    evidence.consumed().to_vec(),
                    evidence.enqueued().to_vec(),
                    text.to_string(),
                ));
                if got.len() == 2 {
                    stop.store(true, Ordering::Relaxed);
                }
                Ok(())
            },
            &mut |_, _| Ok(()),
            &mut || std::fs::write(&path, &transcript).unwrap(),
        )
        .unwrap();

        assert_eq!(
            got.len(),
            2,
            "主轮 + 孤儿 steer 轮各收一枚 terminal:{got:?}"
        );
        assert_eq!(
            (got[0].0, got[0].1.as_slice(), got[0].2.as_slice()),
            (
                MirrorTurnKind::Reply,
                &[main_marker.to_string()][..],
                &[steer_marker.to_string()][..],
            ),
            "主轮 terminal:steer 只有 enqueue 证据,消费凭证仍归主 marker"
        );
        assert_eq!(got[0].3, "主轮答案");
        assert_eq!(
            (got[1].0, got[1].1.as_slice(), got[1].2.as_slice()),
            (
                MirrorTurnKind::Reply,
                &[steer_marker.to_string()][..],
                &[][..],
            ),
            "孤儿轮 terminal 的 consumed 里只有 steer marker、没有原轮 marker——tracker 不认识它就会丢答案"
        );
        assert_eq!(got[1].3, "② 补完了");
    }

    /// 修 2:catchup 配额只属于「上一代开启的轮」。此前 `after_fence` 只有下界
    /// (`boundary > floor`),本代 boot 后新生的 live 轮也被算进停机窗口 —— 5 轮配额被
    /// live 轮蚕食殆尽(实测 boot+2h53m 耗尽),之后真正需要补的轮全部退回只观察。
    /// 代际按轮的**起点**判:live 轮起点在 boot 冻结 EOF 右侧,永不占配额。不能改用终点
    /// 上界截断窗口 —— 那会漏掉跨停机轮,见下一条测试。
    #[test]
    fn downtime_catchup_quota_never_charges_turns_born_after_boot() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("catchup-upper-bound.jsonl");
        let prefix = b"already-delivered-generation-prefix\n";
        let old_marker = "summon-transport-turn:123e4567-e89b-12d3-a456-426614174010";
        let downtime_user = serde_json::json!({
            "type": "user",
            "message": {"role": "user", "content": format!("停机前的任务\n<!-- {old_marker} -->")}
        });
        let downtime_done = r#"{"type":"assistant","message":{"role":"assistant","stop_reason":"end_turn","content":[{"type":"text","text":"停机窗口答案"}]}}"#;
        // 停机窗口产物在 boot 前就已整轮落盘(位于 initial_eof 之内)。
        let on_disk_at_boot = format!(
            "{}{downtime_user}\n{downtime_done}\n",
            String::from_utf8_lossy(prefix)
        );
        std::fs::write(&path, &on_disk_at_boot).unwrap();
        let start = prefix.len() as u64;
        let floor = prefix.len() as u64; // 上一代 stop fence 的 stop_eof

        let live_user = serde_json::json!({
            "type": "user",
            "message": {"role": "user", "content": "boot 后的新任务\n<!-- summon-transport-turn:123e4567-e89b-12d3-a456-426614174011 -->"}
        });
        let live_done = r#"{"type":"assistant","message":{"role":"assistant","stop_reason":"end_turn","content":[{"type":"text","text":"boot 后的 live 轮"}]}}"#;
        let live_turn = format!("{live_user}\n{live_done}\n");

        let stop = AtomicBool::new(false);
        let mut got = Vec::<(MirrorTurnKind, String)>::new();
        tail_forward_turns(
            &crate::runtime::executor::LocalExecutor,
            &path,
            start,
            FileTailCheckpoint::capture_at(&crate::runtime::executor::LocalExecutor, &path, start)
                .unwrap(),
            Duration::from_millis(1),
            Duration::from_millis(1),
            &stop,
            None,
            false,
            Some(floor),
            &mut |kind, _, _, _, text| {
                got.push((kind, text.to_string()));
                if got.len() == 2 {
                    stop.store(true, Ordering::Relaxed);
                }
                Ok(())
            },
            &mut |_, _| Ok(()),
            // Ready 时 initial_eof 已冻结在停机产物之后;此刻再落盘的都是本代 live 产物。
            &mut || {
                use std::io::Write as _;
                let mut file = std::fs::OpenOptions::new()
                    .append(true)
                    .open(&path)
                    .unwrap();
                file.write_all(live_turn.as_bytes()).unwrap();
            },
        )
        .unwrap();

        assert_eq!(
            got,
            [
                (MirrorTurnKind::CatchupReply, "停机窗口答案".to_string()),
                (MirrorTurnKind::Reply, "boot 后的 live 轮".to_string()),
            ],
            "上一代开启的轮走补发;boot 后新生的 live 轮绝不占补发配额"
        );
    }

    /// 后台任务的完成通知在停机窗口里入队、主 Agent 的回复也在停机窗口里收尾:回复没有 summon
    /// marker,终点在 stop_eof 右侧 —— 走补发,不当历史只观察。让轮之后的后台结果就是这种形态。
    #[test]
    fn background_notice_reply_finished_during_downtime_is_caught_up() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("yielded-background-catchup.jsonl");
        let prefix = b"already-delivered-generation-prefix\n";
        let notice = serde_json::json!({
            "type": "queue-operation",
            "operation": "enqueue",
            "content": "<task-notification>\n<task-id>bg1</task-id>\n<status>completed</status>\n<summary>Background command \"build\" completed (exit code 0)</summary>\n</task-notification>"
        });
        let answer = r#"{"type":"assistant","message":{"role":"assistant","stop_reason":"end_turn","content":[{"type":"text","text":"后台构建完成"}]}}"#;
        std::fs::write(
            &path,
            format!("{}{notice}\n{answer}\n", String::from_utf8_lossy(prefix)),
        )
        .unwrap();
        let start = prefix.len() as u64;
        let floor = prefix.len() as u64;

        let stop = AtomicBool::new(false);
        let mut got = Vec::<(MirrorTurnKind, String)>::new();
        tail_forward_turns(
            &crate::runtime::executor::LocalExecutor,
            &path,
            start,
            FileTailCheckpoint::capture_at(&crate::runtime::executor::LocalExecutor, &path, start)
                .unwrap(),
            Duration::from_millis(1),
            Duration::from_millis(1),
            &stop,
            None,
            false,
            Some(floor),
            &mut |kind, _, _, _, text| {
                got.push((kind, text.to_string()));
                stop.store(true, Ordering::Relaxed);
                Ok(())
            },
            &mut |_, _| Ok(()),
            &mut || {},
        )
        .unwrap();

        // 通知卡与回复一起补,与在线时一致
        assert_eq!(
            got,
            [
                (
                    MirrorTurnKind::Notice,
                    "**通知** · 后台任务 1 个\n✅ Background command \"build\" completed (exit code 0)"
                        .to_string()
                ),
                (MirrorTurnKind::CatchupReply, "后台构建完成".to_string()),
            ]
        );
    }

    /// 终审点名的回归(修 2 的硬不变量):上一代跨停机轮 —— 起点在停机前(用户消息已落盘,
    /// 上一代 stop fence 签在它右侧),claude 跑到新 daemon boot **之后**才写下 end_turn。
    /// 这一轮没有任何进程会投递:上一代死了,本代主路径从没拥有过它 —— 正是 CLAUDE.md
    /// 「没有任何进程会投递的那一轮」。曾用 `boundary <= initial_eof` 截断停机窗口,把它
    /// 永久漏发;代际必须按轮的**起点**判,不能截断终点。
    #[test]
    fn cross_shutdown_turn_finishing_after_boot_eof_is_caught_up_exactly_once() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cross-shutdown-catchup.jsonl");
        let prefix = b"already-delivered-generation-prefix\n";
        let old_marker = "summon-transport-turn:123e4567-e89b-12d3-a456-426614174020";
        let straddle_user = serde_json::json!({
            "type": "user",
            "message": {"role": "user", "content": format!("停机前注入的任务\n<!-- {old_marker} -->")}
        });
        // boot 时盘上只有上一代前缀 + 跨停机轮的起点(用户消息):这一轮还在跑,终点未落盘。
        let on_disk_at_boot = format!("{}{straddle_user}\n", String::from_utf8_lossy(prefix));
        std::fs::write(&path, &on_disk_at_boot).unwrap();
        let start = prefix.len() as u64; // mirror 上次提交的 terminal boundary
        let floor = on_disk_at_boot.len() as u64; // 优雅停机在 exact EOF(轮中)签的 stop_eof

        let straddle_done = r#"{"type":"assistant","message":{"role":"assistant","stop_reason":"end_turn","content":[{"type":"text","text":"跨停机轮答案"}]}}"#;
        let live_user = serde_json::json!({
            "type": "user",
            "message": {"role": "user", "content": "boot 后的新任务\n<!-- summon-transport-turn:123e4567-e89b-12d3-a456-426614174021 -->"}
        });
        let live_done = r#"{"type":"assistant","message":{"role":"assistant","stop_reason":"end_turn","content":[{"type":"text","text":"boot 后的 live 轮"}]}}"#;
        // ready(initial_eof 冻结)之后才落盘:跨停机轮的终点(boundary > initial_eof)+ 本代 live 轮。
        let after_boot = format!("{straddle_done}\n{live_user}\n{live_done}\n");

        let stop = AtomicBool::new(false);
        let mut got = Vec::<(MirrorTurnKind, Vec<String>, String)>::new();
        tail_forward_turns(
            &crate::runtime::executor::LocalExecutor,
            &path,
            start,
            FileTailCheckpoint::capture_at(&crate::runtime::executor::LocalExecutor, &path, start)
                .unwrap(),
            Duration::from_millis(1),
            Duration::from_millis(1),
            &stop,
            None,
            false,
            Some(floor),
            &mut |kind, evidence, _, _, text| {
                got.push((kind, evidence.consumed().to_vec(), text.to_string()));
                if got.len() == 2 {
                    stop.store(true, Ordering::Relaxed);
                }
                Ok(())
            },
            &mut |_, _| Ok(()),
            &mut || {
                use std::io::Write as _;
                let mut file = std::fs::OpenOptions::new()
                    .append(true)
                    .open(&path)
                    .unwrap();
                file.write_all(after_boot.as_bytes()).unwrap();
            },
        )
        .unwrap();

        assert_eq!(
            got,
            [
                (
                    MirrorTurnKind::CatchupReply,
                    vec![old_marker.to_string()],
                    "跨停机轮答案".to_string(),
                ),
                (
                    MirrorTurnKind::Reply,
                    vec!["summon-transport-turn:123e4567-e89b-12d3-a456-426614174021".to_string()],
                    "boot 后的 live 轮".to_string(),
                ),
            ],
            "起点在停机前的轮即使 boot 后才收尾也恰好补发一次;本代 live 轮照常走 Reply"
        );
    }

    #[test]
    fn mirror_observes_one_ambiguous_terminal_when_marker_candidates_exceed_the_bound() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mirror-marker-overflow.jsonl");
        let mut transcript = String::new();
        for index in 1..=17u128 {
            let marker = format!("summon-transport-turn:{}", uuid::Uuid::from_u128(index));
            transcript.push_str(
                &serde_json::json!({
                    "type": "user",
                    "message": {"role": "user", "content": marker}
                })
                .to_string(),
            );
            transcript.push('\n');
        }
        transcript.push_str(
            r#"{"type":"assistant","message":{"role":"assistant","stop_reason":"end_turn","content":[{"type":"text","text":"answer"}]}}"#,
        );
        transcript.push('\n');
        std::fs::write(&path, "").unwrap();

        let stop = AtomicBool::new(false);
        let mut observed = Vec::new();
        let mut commits = 0;
        tail_forward_turns(
            &crate::runtime::executor::LocalExecutor,
            &path,
            0,
            FileTailCheckpoint::capture_at(&crate::runtime::executor::LocalExecutor, &path, 0)
                .unwrap(),
            Duration::from_millis(1),
            Duration::from_millis(1),
            &stop,
            None,
            false,
            None, // catchup_floor:本测不验停机补发
            &mut |kind, _, _, _, _| {
                observed.push(kind);
                Ok(())
            },
            &mut |_, _| {
                commits += 1;
                stop.store(true, Ordering::Relaxed);
                Ok(())
            },
            &mut || std::fs::write(&path, &transcript).unwrap(),
        )
        .unwrap();

        assert_eq!(observed, [MirrorTurnKind::AmbiguousObserved]);
        assert_eq!(commits, 1, "歧义轮只收敛自身 offset，未来 watcher 仍可继续");
    }

    /// mirror 没有上游的 generation-complete 信号：一旦看到 truncate/replace，就必须退出。
    /// 不能在 truncate 到 0 的空窗里建 checkpoint，否则随后分段重写的整份历史会被当成 append 灌群。
    #[test]
    fn mirror_fails_closed_before_history_is_rewritten_after_truncate() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::{Arc, Mutex};

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mirror-replaced.jsonl");
        let line = |text: &str| {
            serde_json::json!({
                "type": "assistant",
                "message": {"role": "assistant", "stop_reason": "end_turn",
                    "content": [{"type": "text", "text": text}]}
            })
            .to_string()
                + "\n"
        };
        std::fs::write(&path, line("baseline")).unwrap();

        let stop = Arc::new(AtomicBool::new(false));
        let got = Arc::new(Mutex::new(Vec::<String>::new()));
        let offsets = Arc::new(Mutex::new(Vec::<u64>::new()));
        let (p2, s2, g2, o2) = (
            path.clone(),
            Arc::clone(&stop),
            Arc::clone(&got),
            Arc::clone(&offsets),
        );
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let mut on_turn =
                |_: MirrorTurnKind, _: &MirrorMarkerEvidence, _: u64, _: &[u8], text: &str| {
                    g2.lock().unwrap().push(text.to_string());
                    Ok(())
                };
            let mut on_offset = |offset: u64, _: &FileTailCheckpoint| {
                o2.lock().unwrap().push(offset);
                Ok(())
            };
            let result = tail_forward_turns(
                &crate::runtime::executor::LocalExecutor,
                &p2,
                0,
                FileTailCheckpoint::capture_at(&crate::runtime::executor::LocalExecutor, &p2, 0)
                    .unwrap(),
                Duration::from_millis(10),
                Duration::from_millis(30),
                &s2,
                None,
                false,
                None, // catchup_floor:本测不验停机补发
                &mut on_turn,
                &mut on_offset,
                &mut || {},
            );
            done_tx
                .send(result.map_err(|error| error.to_string()))
                .unwrap();
        });
        let deadline = Instant::now() + Duration::from_secs(3);
        while got.lock().unwrap().is_empty() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(got.lock().unwrap().as_slice(), ["baseline"]);

        std::fs::write(&path, "").unwrap();
        let error = match done_rx.recv_timeout(Duration::from_secs(3)) {
            Ok(Err(error)) => error,
            Ok(Ok(())) => panic!("truncate 后 mirror 不得成功退出"),
            Err(error) => {
                stop.store(true, Ordering::Relaxed);
                panic!("mirror 未在 truncate 后 fail-closed:{error}");
            }
        };
        worker.join().unwrap();
        assert!(error.contains("truncate/replace"), "{error}");

        // 模拟上游在 truncate 空窗之后分段重写旧历史。mirror 已进入 Failed，任何历史和后续
        // terminal 都不能再被这个 watcher 主动发送；当前轮由主捕获链路兜底。
        std::fs::write(
            &path,
            format!(
                "{}{}{}",
                line("重写历史一"),
                line("重写历史二"),
                line("未来终端")
            ),
        )
        .unwrap();
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(
            got.lock().unwrap().as_slice(),
            ["baseline"],
            "换代失败后不得把分段重写历史或后续 terminal 当成新 append 灌群"
        );
        assert_eq!(offsets.lock().unwrap().len(), 1, "失败换代不得推进 offset");
    }

    /// R6 双发消除:run_turn 已消费到的转录(watermark 覆盖)那一 turn,mirror **不再重复转发**——
    /// 只有 watermark 之外(终端 attach 直敲、飞书没注入)的 turn 才推回飞书。turn1 模拟飞书触发轮
    /// (watermark = 文件尾),turn2 模拟终端轮(watermark 不前移)。
    #[test]
    fn mirror_skips_turns_below_watermark() {
        use std::io::Write as _;
        use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
        use std::sync::{Arc, Mutex};
        use std::time::{Duration, Instant};

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mirror.jsonl");
        std::fs::write(&path, "").unwrap();
        let append = |text: &str| -> u64 {
            let line = serde_json::json!({
                "type": "assistant", "sessionId": "s",
                "message": {"role": "assistant", "stop_reason": "end_turn",
                    "content": [{"type": "text", "text": text}]}
            })
            .to_string()
                + "\n";
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            f.write_all(line.as_bytes()).unwrap();
            std::fs::metadata(&path).unwrap().len()
        };

        // turn1 已落盘,watermark = 文件尾(模拟 run_turn 已消费这一轮、自己发过了)。
        let len1 = append("你好");
        let watermark = Arc::new(AtomicU64::new(len1));
        let stop = Arc::new(AtomicBool::new(false));
        let got = Arc::new(Mutex::new(Vec::<String>::new()));
        let committed = Arc::new(Mutex::new(Vec::<u64>::new()));
        let (p2, s2, g2, wm2, c2) = (
            path.clone(),
            stop.clone(),
            got.clone(),
            watermark.clone(),
            committed.clone(),
        );
        let h = std::thread::spawn(move || {
            let mut on_turn =
                |kind: MirrorTurnKind, _: &MirrorMarkerEvidence, _: u64, _: &[u8], t: &str| {
                    if kind == MirrorTurnKind::Reply {
                        g2.lock().unwrap().push(t.to_string());
                    }
                    Ok(())
                };
            let mut on_off = |offset: u64, _: &FileTailCheckpoint| {
                c2.lock().unwrap().push(offset);
                Ok(())
            };
            tail_forward_turns(
                &crate::runtime::executor::LocalExecutor,
                &p2,
                0,
                FileTailCheckpoint::capture_at(&crate::runtime::executor::LocalExecutor, &p2, 0)
                    .unwrap(),
                Duration::from_millis(10),
                Duration::from_millis(30),
                &s2,
                Some(&wm2),
                false,
                None, // catchup_floor:本测不验停机补发
                &mut on_turn,
                &mut on_off,
                &mut || {},
            )
            .unwrap();
        });
        // 等 mirror 消费 turn1：它不发送，但必须提交 boundary；否则重启后 watermark 归零会重发。
        let consumed_deadline = Instant::now() + Duration::from_secs(3);
        while committed.lock().unwrap().is_empty() && Instant::now() < consumed_deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            committed.lock().unwrap().as_slice(),
            [len1],
            "watermark 已消费轮也必须推进连续 offset"
        );
        // turn2:飞书没注入(watermark 不前移)→ 终端直敲轮,应转发。
        let len2 = append("再见");
        let dl = Instant::now() + Duration::from_secs(3);
        while got.lock().unwrap().is_empty() && Instant::now() < dl {
            std::thread::sleep(Duration::from_millis(15));
        }
        stop.store(true, Ordering::Relaxed);
        h.join().unwrap();
        assert_eq!(
            *got.lock().unwrap(),
            vec!["再见".to_string()],
            "watermark 覆盖的 turn1 应被跳过,只转发 watermark 之外的 turn2"
        );
        assert_eq!(
            committed.lock().unwrap().last().copied(),
            Some(len2),
            "成功发送的终端轮必须提交 boundary"
        );
    }

    #[test]
    fn seed_prefix_cutoff_stops_before_later_attach_turn_in_the_same_read() {
        use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
        use std::sync::{Arc, Mutex};

        let current = rec("text", "飞书当前轮", Some("end_turn"));
        let attach_user = serde_json::json!({
            "type": "user",
            "message": {"role": "user", "content": "终端追加问题"}
        })
        .to_string()
            + "\n";
        let attach_answer = rec("text", "终端追加答案", Some("end_turn"));
        let transcript = format!("{current}{attach_user}{attach_answer}");

        // cold seed cutoff 是真实连续前缀，可以安全停在 seed terminal；normal turn 不再写它。
        let watermark = AtomicU64::new(current.len() as u64);

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("main-and-attach-one-read.jsonl");
        std::fs::write(&path, "").unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let watermark = Arc::new(watermark);
        let delivered = Arc::new(Mutex::new(Vec::<String>::new()));
        let offsets = Arc::new(Mutex::new(Vec::<u64>::new()));
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (worker_path, worker_stop, worker_watermark, worker_delivered, worker_offsets) = (
            path.clone(),
            Arc::clone(&stop),
            Arc::clone(&watermark),
            Arc::clone(&delivered),
            Arc::clone(&offsets),
        );
        let worker = std::thread::spawn(move || {
            tail_forward_turns(
                &crate::runtime::executor::LocalExecutor,
                &worker_path,
                0,
                FileTailCheckpoint::capture_at(
                    &crate::runtime::executor::LocalExecutor,
                    &worker_path,
                    0,
                )
                .unwrap(),
                Duration::from_millis(1),
                Duration::from_millis(1),
                &worker_stop,
                Some(&worker_watermark),
                false,
                None, // catchup_floor:本测不验停机补发
                &mut |kind, _, _, _, text| {
                    if kind == MirrorTurnKind::Reply {
                        worker_delivered.lock().unwrap().push(text.to_string());
                    }
                    Ok(())
                },
                &mut |offset, _| {
                    let mut offsets = worker_offsets.lock().unwrap();
                    offsets.push(offset);
                    if offsets.len() == 2 {
                        worker_stop.store(true, Ordering::Relaxed);
                    }
                    Ok(())
                },
                &mut || ready_tx.send(()).unwrap(),
            )
            .unwrap();
        });
        ready_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("seed cutoff 必须在 Ready 后才测 live chunk");
        std::fs::write(&path, transcript).unwrap();
        worker.join().unwrap();

        assert_eq!(delivered.lock().unwrap().as_slice(), ["终端追加答案"]);
        assert_eq!(
            offsets.lock().unwrap().as_slice(),
            [
                current.len() as u64,
                std::fs::metadata(&path).unwrap().len()
            ],
            "seed 前缀可跳过，但 attach terminal 仍必须送达并精确提交"
        );
    }

    /// C2 回归:重启时 initial backlog 的 ≥2 个 turn 可在一次 read 到达。必须全部解析并
    /// 逐轮提交 exact boundary，但历史归属无法证明未送达，每轮都只能 `HistoricalObserved`。
    #[test]
    fn initial_backlog_observes_all_turns_arriving_in_one_read() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::{Arc, Mutex};
        use std::time::{Duration, Instant};

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("backlog.jsonl");
        // 一次性写入 3 个 end_turn(模拟停机期间攒下的 backlog,重启后一次读全进来)。
        let mut buf = String::new();
        for t in ["第一轮", "第二轮", "第三轮"] {
            buf.push_str(
                &serde_json::json!({
                    "type": "assistant", "sessionId": "s",
                    "message": {"role": "assistant", "stop_reason": "end_turn",
                        "content": [{"type": "text", "text": t}]}
                })
                .to_string(),
            );
            buf.push('\n');
        }
        std::fs::write(&path, &buf).unwrap();
        let file_len = std::fs::metadata(&path).unwrap().len();

        let stop = Arc::new(AtomicBool::new(false));
        let got = Arc::new(Mutex::new(Vec::<(MirrorTurnKind, String)>::new()));
        let last_off = Arc::new(Mutex::new(0u64));
        let (p2, s2, g2, o2) = (path.clone(), stop.clone(), got.clone(), last_off.clone());
        let h = std::thread::spawn(move || {
            let mut on_turn =
                |kind: MirrorTurnKind, _: &MirrorMarkerEvidence, _: u64, _: &[u8], t: &str| {
                    g2.lock().unwrap().push((kind, t.to_string()));
                    Ok(())
                };
            let mut on_off = |off: u64, _: &FileTailCheckpoint| {
                *o2.lock().unwrap() = off;
                Ok(())
            };
            tail_forward_turns(
                &crate::runtime::executor::LocalExecutor,
                &p2,
                0,
                FileTailCheckpoint::capture_at(&crate::runtime::executor::LocalExecutor, &p2, 0)
                    .unwrap(),
                Duration::from_millis(10),
                Duration::from_millis(30),
                &s2,
                None,
                false,
                None, // catchup_floor:本测不验停机补发
                &mut on_turn,
                &mut on_off,
                &mut || {},
            )
            .unwrap();
        });
        let dl = Instant::now() + Duration::from_secs(3);
        while got.lock().unwrap().len() < 3 && Instant::now() < dl {
            std::thread::sleep(Duration::from_millis(15));
        }
        stop.store(true, Ordering::Relaxed);
        h.join().unwrap();
        assert_eq!(
            *got.lock().unwrap(),
            vec![
                (MirrorTurnKind::HistoricalObserved, "第一轮".to_string()),
                (MirrorTurnKind::HistoricalObserved, "第二轮".to_string()),
                (MirrorTurnKind::HistoricalObserved, "第三轮".to_string())
            ],
            "同一次 read 里的历史 turn 必须全部本地收敛，但不得联网"
        );
        // 持久化 offset 精确落在最后一个 turn 的行边界(= 文件末尾),重启从此续读不重放。
        assert_eq!(
            *last_off.lock().unwrap(),
            file_len,
            "最后收敛的 turn 其持久化 offset 应精确到行边界(文件末尾)"
        );
    }

    /// 镜像侧同一判据:续跑隔了远超确认窗的时间才落盘,其间不转发任何一轮;续跑落盘后整轮
    /// (带本轮 marker)只转发一次,正文是续跑的回复。
    #[test]
    fn mirror_keeps_turn_companion_inside_the_same_turn() {
        use std::io::Write;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::{Arc, Mutex};

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("thinking-then-companion.jsonl");
        std::fs::write(&path, "").unwrap();
        let append = |text: &str| {
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            file.write_all(text.as_bytes()).unwrap();
        };
        let marker = "summon-transport-turn:123e4567-e89b-12d3-a456-426614174020";
        let user = serde_json::json!({
            "type": "user",
            "message": {"role": "user", "content": format!("任务\n<!-- {marker} -->")}
        });
        let before_continuation = format!(
            "{user}\n{}{CONTINUE_COMPANION}\n",
            rec("thinking", "想完了", Some("end_turn"))
        );

        let stop = Arc::new(AtomicBool::new(false));
        let delivered = Arc::new(Mutex::new(Vec::<(Vec<String>, String)>::new()));
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (worker_path, worker_stop, worker_delivered) =
            (path.clone(), Arc::clone(&stop), Arc::clone(&delivered));
        let worker = std::thread::spawn(move || {
            tail_forward_turns(
                &crate::runtime::executor::LocalExecutor,
                &worker_path,
                0,
                FileTailCheckpoint::capture_at(
                    &crate::runtime::executor::LocalExecutor,
                    &worker_path,
                    0,
                )
                .unwrap(),
                Duration::from_millis(1),
                Duration::from_millis(1),
                &worker_stop,
                None,
                false,
                None, // catchup_floor:本测不验停机补发
                &mut |_, evidence, _, _, text| {
                    worker_delivered
                        .lock()
                        .unwrap()
                        .push((evidence.consumed().to_vec(), text.to_string()));
                    Ok(())
                },
                &mut |_, _| Ok(()),
                &mut || ready_tx.send(()).unwrap(),
            )
            .unwrap();
        });
        ready_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("Ready 之后再写入");

        append(&before_continuation);
        std::thread::sleep(Duration::from_millis(100));
        assert!(
            delivered.lock().unwrap().is_empty(),
            "续跑还没落盘,不能先按空回复转发: {:?}",
            delivered.lock().unwrap()
        );

        append(&rec("text", "续跑出来的回复", Some("end_turn")));
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while delivered.lock().unwrap().is_empty() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        stop.store(true, Ordering::Relaxed);
        worker.join().unwrap();

        assert_eq!(
            *delivered.lock().unwrap(),
            vec![(vec![marker.to_string()], "续跑出来的回复".to_string())],
            "续跑提示不能把一轮切成「空回复的本轮 + 无 marker 的终端侧一轮」"
        );
    }

    /// thinking-only 的 end_turn 不能等静默确认到把下一轮一起吞掉。一次 read 已经包含明确的新 user
    /// prompt 时，该 user 行就是确定边界：先提交 A，并把 B 的原始行完整留给下一次 feed。
    #[test]
    fn thinking_only_turn_stops_before_next_user_in_the_same_read() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("thinking-then-next-turn.jsonl");
        let marker_a = "summon-transport-turn:123e4567-e89b-12d3-a456-426614174010";
        let marker_b = "summon-transport-turn:123e4567-e89b-12d3-a456-426614174011";
        let user = |marker: &str, text: &str| {
            serde_json::json!({
                "type": "user",
                "message": {"role": "user", "content": format!("{text}\n<!-- {marker} -->")}
            })
            .to_string()
                + "\n"
        };
        let a = format!(
            "{}{}{}",
            user(marker_a, "任务 A"),
            rec("text", "A 正文", Some("tool_use")),
            rec("thinking", "A 收尾", Some("end_turn"))
        );
        let transcript = format!(
            "{a}{}{}",
            user(marker_b, "任务 B"),
            rec("text", "B 回复", Some("end_turn"))
        );
        std::fs::write(&path, &transcript).unwrap();

        let stop = AtomicBool::new(false);
        let mut delivered = Vec::<(Vec<String>, String)>::new();
        let mut offsets = Vec::<u64>::new();
        tail_forward_turns(
            &crate::runtime::executor::LocalExecutor,
            &path,
            0,
            FileTailCheckpoint::capture_at(&crate::runtime::executor::LocalExecutor, &path, 0)
                .unwrap(),
            Duration::from_millis(1),
            Duration::from_millis(1),
            &stop,
            None,
            false,
            None, // catchup_floor:本测不验停机补发
            &mut |_, evidence, _, _, text| {
                delivered.push((evidence.consumed().to_vec(), text.to_string()));
                Ok(())
            },
            &mut |offset, _| {
                offsets.push(offset);
                // 正确实现会继续抽干本次 read 里的 pending B，再在外层循环看到 stop。
                // 错误实现只发被 B 覆盖后的一个 turn，也能有界退出，不让失败测试挂住。
                stop.store(true, Ordering::Relaxed);
                Ok(())
            },
            &mut || {},
        )
        .unwrap();

        assert_eq!(
            delivered,
            vec![
                (vec![marker_a.to_string()], "A 正文".into()),
                (vec![marker_b.to_string()], "B 回复".into()),
            ],
            "同一次 read 中 A/B 必须各转发一次，marker 不能跨轮混合"
        );
        assert_eq!(
            offsets,
            vec![a.len() as u64, transcript.len() as u64],
            "A 的连续提交点必须停在 B user 行之前，不能越过尚未送达的下一轮"
        );
    }

    #[test]
    fn thinking_only_turn_keeps_its_real_start_for_watermark_deduplication() {
        use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("thinking-watermark.jsonl");
        let transcript = format!(
            "{}{}",
            rec("text", "已由主路径发送", Some("tool_use")),
            rec("thinking", "收尾", Some("end_turn"))
        );
        std::fs::write(&path, &transcript).unwrap();
        let watermark = AtomicU64::new(transcript.len() as u64);
        let stop = AtomicBool::new(false);
        let mut delivered = Vec::<String>::new();
        let mut committed = Vec::<u64>::new();

        tail_forward_turns(
            &crate::runtime::executor::LocalExecutor,
            &path,
            0,
            FileTailCheckpoint::capture_at(&crate::runtime::executor::LocalExecutor, &path, 0)
                .unwrap(),
            Duration::from_millis(1),
            Duration::from_millis(1),
            &stop,
            Some(&watermark),
            false,
            None, // catchup_floor:本测不验停机补发
            &mut |kind, _, _, _, text| {
                if kind == MirrorTurnKind::Reply {
                    delivered.push(text.to_string());
                }
                Ok(())
            },
            &mut |offset, _| {
                committed.push(offset);
                stop.store(true, Ordering::Relaxed);
                Ok(())
            },
            &mut || {},
        )
        .unwrap();

        assert!(
            delivered.is_empty(),
            "thinking-only grace 仍必须用 turn 的真实起点命中 watermark，不能重复发送"
        );
        assert_eq!(committed, [transcript.len() as u64]);
    }

    #[test]
    fn thinking_boundary_before_current_read_uses_the_saved_checkpoint() {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

        struct TwoReadExecutor {
            data: Vec<u8>,
            first_end: usize,
            reads: AtomicUsize,
        }

        impl Executor for TwoReadExecutor {
            fn spawn(
                &self,
                _spec: &crate::runtime::executor::SpawnSpec,
            ) -> anyhow::Result<std::process::Child> {
                unreachable!("本测试不 spawn")
            }

            fn read_file_tail(
                &self,
                _path: &std::path::Path,
                from_byte: u64,
            ) -> anyhow::Result<Vec<u8>> {
                let visible = if self.reads.fetch_add(1, Ordering::SeqCst) == 0 {
                    self.first_end
                } else {
                    self.data.len()
                };
                let from = usize::try_from(from_byte).unwrap();
                Ok(self.data[from..visible].to_vec())
            }

            fn tmux(&self, _args: &[&str]) -> anyhow::Result<String> {
                unreachable!("本测试不碰 tmux")
            }
        }

        let a = format!(
            "{}{}",
            rec("text", "A 正文", Some("tool_use")),
            rec("thinking", "A 收尾", Some("end_turn"))
        );
        let user_b = serde_json::json!({
            "type": "user",
            "message": {"role": "user", "content": "B 问题"}
        })
        .to_string()
            + "\n";
        let b = rec("text", "B 回复", Some("end_turn"));
        let all = format!("{a}{user_b}{b}").into_bytes();
        // 第一读在 B user 行中间停下；第二读确认 A 时，A boundary 已在当前 read window 左侧。
        let first_end = a.len() + user_b.len() / 2;
        let exec = TwoReadExecutor {
            data: all.clone(),
            first_end,
            reads: AtomicUsize::new(0),
        };
        let stop = AtomicBool::new(false);
        let mut delivered = Vec::<(MirrorTurnKind, String)>::new();
        let mut offsets = Vec::<u64>::new();
        tail_forward_turns(
            &exec,
            std::path::Path::new("virtual.jsonl"),
            0,
            FileTailCheckpoint {
                start: 0,
                bytes: Vec::new(),
            },
            Duration::from_millis(1),
            Duration::from_secs(1),
            &stop,
            None,
            false,
            None, // catchup_floor:本测不验停机补发
            &mut |kind, _, _, _, text| {
                delivered.push((kind, text.to_string()));
                Ok(())
            },
            &mut |offset, _| {
                offsets.push(offset);
                if offsets.len() == 2 {
                    stop.store(true, Ordering::Relaxed);
                }
                Ok(())
            },
            &mut || {},
        )
        .unwrap();

        assert_eq!(
            delivered,
            [
                (MirrorTurnKind::HistoricalObserved, "A 正文".into()),
                (MirrorTurnKind::HistoricalObserved, "B 回复".into())
            ],
            "Ready 前冻结的 partial turn 即使 terminal 后到，也必须与已完整的 initial turn 一样零网络"
        );
        assert_eq!(offsets, [a.len() as u64, all.len() as u64]);
    }

    /// offset 是「此前所有可见块均已成功送达」的连续提交点，不能只是 parser 读到哪里。
    /// 同一次 read 里首轮发送失败后若还消费第二轮并提交其 boundary，重启就永远跳过失败轮。
    #[test]
    fn failed_mirror_delivery_does_not_advance_offset_or_consume_later_turn() {
        use std::sync::atomic::AtomicBool;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("failed-delivery.jsonl");
        let line = |text: &str| {
            serde_json::json!({
                "type": "assistant", "sessionId": "s",
                "message": {"role": "assistant", "stop_reason": "end_turn",
                    "content": [{"type": "text", "text": text}]}
            })
            .to_string()
                + "\n"
        };
        std::fs::write(&path, format!("{}{}", line("失败轮"), line("后续轮"))).unwrap();

        let stop = AtomicBool::new(false);
        let mut deliveries = Vec::new();
        let mut offsets = Vec::new();
        let mut on_turn =
            |_: MirrorTurnKind, _: &MirrorMarkerEvidence, _: u64, _: &[u8], text: &str| {
                deliveries.push(text.to_string());
                anyhow::bail!("注入发送失败")
            };
        let result = tail_forward_turns(
            &crate::runtime::executor::LocalExecutor,
            &path,
            0,
            FileTailCheckpoint::capture_at(&crate::runtime::executor::LocalExecutor, &path, 0)
                .unwrap(),
            Duration::from_millis(1),
            Duration::from_millis(1),
            &stop,
            None,
            false,
            None, // catchup_floor:本测不验停机补发
            &mut on_turn,
            &mut |offset, _: &FileTailCheckpoint| {
                offsets.push(offset);
                Ok(())
            },
            &mut || {},
        );

        assert!(
            result.is_err(),
            "发送失败必须终止 watcher，实际为 {result:?}"
        );
        assert_eq!(deliveries, ["失败轮"], "失败后不得跨过后续 turn");
        assert!(offsets.is_empty(), "失败轮不得提交 offset:{offsets:?}");
    }

    /// 镜像侧的同一形态:一轮以纯 thinking 收尾(终态记录无正文)。不确认候选终态的话,这轮的正文
    /// 会被下一轮吞掉(下一轮的终态文本才是它的回复)—— 现场那轮的正文就是这么消失的。
    #[test]
    fn mirror_forwards_turn_that_ends_with_thinking_only_record() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::{Arc, Mutex};
        use std::time::{Duration, Instant};

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mirror-thinking.jsonl");
        // 中途正文 + 纯 thinking 终态(两行都由 claude 一次写入)。
        let mut buf = rec("text", "答案在这", Some("tool_use"));
        buf.push_str(&rec("thinking", "收工", Some("end_turn")));
        std::fs::write(&path, &buf).unwrap();

        let stop = Arc::new(AtomicBool::new(false));
        let got = Arc::new(Mutex::new(Vec::<String>::new()));
        let (p2, s2, g2) = (path.clone(), stop.clone(), got.clone());
        let h = std::thread::spawn(move || {
            let mut on_turn =
                |_: MirrorTurnKind, _: &MirrorMarkerEvidence, _: u64, _: &[u8], t: &str| {
                    g2.lock().unwrap().push(t.to_string());
                    Ok(())
                };
            let mut on_off = |_: u64, _: &FileTailCheckpoint| Ok(());
            tail_forward_turns(
                &crate::runtime::executor::LocalExecutor,
                &p2,
                0,
                FileTailCheckpoint::capture_at(&crate::runtime::executor::LocalExecutor, &p2, 0)
                    .unwrap(),
                Duration::from_millis(10),
                Duration::from_millis(30),
                &s2,
                None,
                false,
                None, // catchup_floor:本测不验停机补发
                &mut on_turn,
                &mut on_off,
                &mut || {},
            )
            .unwrap();
        });
        let dl = Instant::now() + Duration::from_secs(3);
        while got.lock().unwrap().is_empty() && Instant::now() < dl {
            std::thread::sleep(Duration::from_millis(10));
        }
        stop.store(true, Ordering::Relaxed);
        h.join().unwrap();
        assert_eq!(
            *got.lock().unwrap(),
            vec!["答案在这".to_string()],
            "纯 thinking 收尾的轮,镜像也必须收口转发,不能挂着等下一轮"
        );
    }

    #[test]
    fn initial_empty_terminal_is_historically_observed_and_commits_boundary() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mirror-empty-terminal.jsonl");
        let marker = "summon-transport-turn:123e4567-e89b-12d3-a456-426614174088";
        let user = serde_json::json!({
            "type": "user",
            "message": {"role": "user", "content": marker}
        })
        .to_string()
            + "\n";
        let transcript = format!("{user}{}", rec("thinking", "", Some("end_turn")));
        std::fs::write(&path, &transcript).unwrap();
        let stop = AtomicBool::new(false);
        let mut observed = Vec::<(MirrorTurnKind, Vec<String>, String)>::new();
        let mut offsets = Vec::<u64>::new();

        tail_forward_turns(
            &crate::runtime::executor::LocalExecutor,
            &path,
            0,
            FileTailCheckpoint::capture_at(&crate::runtime::executor::LocalExecutor, &path, 0)
                .unwrap(),
            Duration::from_millis(1),
            Duration::from_millis(1),
            &stop,
            None,
            false,
            None, // catchup_floor:本测不验停机补发
            &mut |kind, evidence, _, _, text| {
                observed.push((kind, evidence.consumed().to_vec(), text.to_string()));
                Ok(())
            },
            &mut |offset, _| {
                offsets.push(offset);
                stop.store(true, Ordering::Relaxed);
                Ok(())
            },
            &mut || {},
        )
        .unwrap();

        assert_eq!(
            observed,
            [(
                MirrorTurnKind::HistoricalObserved,
                vec![marker.to_string()],
                String::new()
            )],
            "initial backlog 的空 terminal 只收敛 durable boundary，不产生可发送 Reply"
        );
        assert_eq!(offsets, [transcript.len() as u64]);
    }

    #[cfg(unix)]
    #[test]
    fn transcript_path_predicts_symlink_target_but_preserves_existing_literal_file() {
        let (dir, mut agent, projects) = transcript_location_fixture();
        let real = dir.path().join("real-work");
        let link = dir.path().join("work-link");
        std::fs::create_dir(&real).unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let configured = link.to_string_lossy().into_owned();
        agent.runtime.as_mut().unwrap().working_dir = Some(configured.clone());
        let sid = "symlink-session";
        let target = transcript_path_in_projects(
            &projects,
            &real.canonicalize().unwrap().to_string_lossy(),
            sid,
        );
        let resolve = || {
            resolve_transcript_path_in(&agent, "test-location", None, None, sid, &projects).unwrap()
        };
        assert_eq!(resolve(), target);
        assert!(!target.exists());
        assert_eq!(
            agent.runtime.as_ref().unwrap().working_dir.as_deref(),
            Some(configured.as_str())
        );
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(&target, "real").unwrap();
        assert_eq!(resolve(), target);
        let original = transcript_path_in_projects(&projects, &configured, sid);
        std::fs::create_dir_all(original.parent().unwrap()).unwrap();
        std::fs::write(&original, "original").unwrap();
        assert_eq!(resolve(), original);
    }

    #[test]
    fn transcript_path_missing_workdir_without_existing_sid_fails() {
        let (dir, mut agent, projects) = transcript_location_fixture();
        agent.runtime.as_mut().unwrap().working_dir =
            Some(dir.path().join("absent").display().to_string());
        assert!(resolve_transcript_path_in(
            &agent,
            "test-location",
            None,
            None,
            "absent",
            &projects
        )
        .is_err());
    }

    #[test]
    fn transcript_sid_search_rejects_ambiguity_and_io_errors() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["a", "b"] {
            let path = dir.path().join(name).join("wanted.jsonl");
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "record").unwrap();
        }
        assert!(find_transcript_in_projects(dir.path(), "wanted")
            .unwrap_err()
            .to_string()
            .contains("多个"));
        assert!(find_transcript_in_projects(dir.path(), "other")
            .unwrap()
            .is_none());
        let invalid = dir.path().join("not-a-directory");
        std::fs::write(&invalid, "file").unwrap();
        assert!(find_transcript_in_projects(&invalid, "wanted").is_err());
        assert!(transcript_file_exists(dir.path()).is_err());
        assert!(!transcript_file_exists(&dir.path().join("missing")).unwrap());
    }

    #[test]
    fn transcript_relocation_is_throttled_and_keeps_a_bound_file() {
        let dir = tempfile::tempdir().unwrap();
        let exec = crate::runtime::executor::LocalExecutor;
        let mut path = dir.path().join("missing.jsonl");
        let mut search = MissingTranscriptSearch::in_projects(
            dir.path().to_path_buf(),
            "wanted",
            Duration::from_secs(60),
        );
        assert!(!search.relocate_missing(&exec, &mut path).unwrap());
        let found = dir.path().join("actual/wanted.jsonl");
        std::fs::create_dir_all(found.parent().unwrap()).unwrap();
        std::fs::write(&found, "record").unwrap();
        assert!(!search.relocate_missing(&exec, &mut path).unwrap());
        search.next_search = Instant::now();
        assert!(search.relocate_missing(&exec, &mut path).unwrap());
        assert_eq!(path, found);
        let duplicate = dir.path().join("duplicate/wanted.jsonl");
        std::fs::create_dir_all(duplicate.parent().unwrap()).unwrap();
        std::fs::write(duplicate, "duplicate").unwrap();
        search.next_search = Instant::now();
        assert!(!search.relocate_missing(&exec, &mut path).unwrap());
        assert_eq!(path, found);
        path = dir.path().join("missing-again.jsonl");
        assert!(search.relocate_missing(&exec, &mut path).is_err());
    }

    #[test]
    fn relocated_prompt_requires_exact_marker_and_returns_actual_tail_path() {
        let dir = tempfile::tempdir().unwrap();
        let predicted = dir.path().join("predicted/wanted.jsonl");
        let actual = dir.path().join("actual/wanted.jsonl");
        let other = dir.path().join("actual/other.jsonl");
        std::fs::create_dir_all(actual.parent().unwrap()).unwrap();
        let marker = "summon-transport-turn:123e4567-e89b-12d3-a456-426614174070";
        std::fs::write(other, consumed_user_line("stuck prompt", marker)).unwrap();
        let mut wait = resubmit_wait(&predicted, marker, Duration::from_secs(2));
        wait.inject_offset = 999;
        wait.relocation = Some(MissingTranscriptSearch::in_projects(
            dir.path().to_path_buf(),
            "wanted",
            Duration::from_millis(1),
        ));
        let mut presses = 0;
        let history = consumed_user_line(
            "stuck prompt",
            "summon-transport-turn:123e4567-e89b-12d3-a456-426614174071",
        ) + &rec("text", "old answer", Some("end_turn"));
        let result = wait_for_prompt_echo(
            &crate::runtime::executor::LocalExecutor,
            wait,
            || true,
            || {
                presses += 1;
                if presses == 1 {
                    std::fs::write(&actual, &history).unwrap();
                } else {
                    use std::io::Write as _;
                    let mut file = std::fs::OpenOptions::new()
                        .append(true)
                        .open(&actual)
                        .unwrap();
                    file.write_all(consumed_user_line("stuck prompt", marker).as_bytes())
                        .unwrap();
                    file.write_all(rec("text", "current answer", Some("end_turn")).as_bytes())
                        .unwrap();
                }
                true
            },
        )
        .unwrap();
        let CorrelatedPrompt::Echoed {
            path,
            trailing,
            consumed_boundary,
            ..
        } = result
        else {
            panic!("missing echo")
        };
        assert_eq!(path, actual);
        assert!(
            presses >= 2,
            "other SID and historical marker must not satisfy the wait"
        );
        let expected = rec("text", "current answer", Some("end_turn"));
        let mut bytes = trailing;
        bytes.extend(
            crate::runtime::executor::LocalExecutor
                .read_file_tail(&path, consumed_boundary + bytes.len() as u64)
                .unwrap(),
        );
        assert_eq!(bytes, expected.as_bytes());
    }

    #[test]
    fn mirror_relocation_establishes_eof_before_forwarding_live_output() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let dir = tempfile::tempdir().unwrap();
        let mut predicted = dir.path().join("predicted/wanted.jsonl");
        let actual = dir.path().join("actual/wanted.jsonl");
        let history = rec("text", "old answer", Some("end_turn"));
        let writer_path = actual.clone();
        let writer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(25));
            std::fs::create_dir_all(writer_path.parent().unwrap()).unwrap();
            std::fs::write(writer_path, history).unwrap();
        });
        let mut search = MissingTranscriptSearch::in_projects(
            dir.path().to_path_buf(),
            "wanted",
            Duration::from_millis(1),
        );
        let stop = AtomicBool::new(false);
        let exec = crate::runtime::executor::LocalExecutor;
        let (start, eof, checkpoint, used_saved) = wait_for_stable_mirror_start(
            &exec,
            &mut predicted,
            Some(&mut search),
            None,
            &stop,
            Duration::from_secs(2),
            &mut || {},
        )
        .unwrap()
        .unwrap();
        writer.join().unwrap();
        assert_eq!(predicted, actual);
        assert_eq!(start, eof);
        assert!(!used_saved);
        use std::io::Write as _;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&actual)
            .unwrap();
        file.write_all(rec("text", "live answer", Some("end_turn")).as_bytes())
            .unwrap();
        let mut observed = Vec::new();
        tail_forward_turns(
            &exec,
            &predicted,
            start,
            checkpoint,
            Duration::from_millis(1),
            Duration::from_millis(1),
            &stop,
            None,
            false,
            None,
            &mut |_, _, _, _, text| {
                observed.push(text.to_owned());
                stop.store(true, Ordering::Relaxed);
                Ok(())
            },
            &mut |_, _| Ok(()),
            &mut || {},
        )
        .unwrap();
        assert_eq!(observed, ["live answer"]);
    }

    fn transcript_location_fixture() -> (
        tempfile::TempDir,
        summon_core::registry::Agent,
        std::path::PathBuf,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let mut agent: summon_core::registry::Agent =
            serde_yaml::from_str("name: transcript-location\nruntime:\n  cli: claude").unwrap();
        agent.dir = dir.path().join("agents/transcript-location");
        let cwd = dir.path().join("current");
        std::fs::create_dir_all(&cwd).unwrap();
        agent.runtime.as_mut().unwrap().working_dir = Some(cwd.to_string_lossy().into_owned());
        std::fs::create_dir_all(agent.state_dir().join("runtime/chats")).unwrap();
        let projects = dir.path().join("projects");
        std::fs::create_dir_all(&projects).unwrap();
        (dir, agent, projects)
    }

    fn record_transcript_location(agent: &summon_core::registry::Agent, cwd: &str, sid: &str) {
        std::fs::write(
            agent
                .state_dir()
                .join("runtime/chats/test-location.session"),
            summon_core::session_id::format_record("claude", sid, Some(cwd), None),
        )
        .unwrap();
    }

    #[test]
    fn transcript_path_preserves_sid_search_result_after_cwd_change() {
        let (_dir, agent, projects) = transcript_location_fixture();
        let cwd = agent
            .runtime
            .as_ref()
            .unwrap()
            .working_dir
            .as_deref()
            .unwrap();
        for (index, encoded) in [
            "-Users-example--summon-agents-arcadia-workspace",
            "-Users-example-project-with-hyphens",
            "-Users-example-My-App",
            "-Users-example-project-with-spaces",
            "-Users-example----",
        ]
        .iter()
        .enumerate()
        {
            let sid = format!("preserve-path-{index}");
            record_transcript_location(&agent, cwd, &sid);
            let expected = projects.join(encoded).join(format!("{sid}.jsonl"));
            std::fs::create_dir_all(expected.parent().unwrap()).unwrap();
            std::fs::write(&expected, "current-session-transcript").unwrap();
            let actual =
                resolve_transcript_path_in(&agent, "test-location", None, None, &sid, &projects)
                    .unwrap();
            assert_eq!(actual, expected, "SID 命中的文件路径必须逐字保留");
            assert_eq!(
                std::fs::read_to_string(actual).unwrap(),
                "current-session-transcript"
            );
        }
    }

    #[test]
    fn transcript_path_finds_sid_when_all_work_directories_are_missing() {
        let (dir, mut agent, projects) = transcript_location_fixture();
        let absent = dir.path().join("absent");
        agent.runtime.as_mut().unwrap().working_dir = Some(absent.to_string_lossy().into_owned());
        record_transcript_location(&agent, &absent.to_string_lossy(), "missing-cwd");
        let expected = projects
            .join("-Users-example--summon")
            .join("missing-cwd.jsonl");
        std::fs::create_dir_all(expected.parent().unwrap()).unwrap();
        std::fs::write(&expected, "transcript").unwrap();
        assert_eq!(
            resolve_transcript_path_in(
                &agent,
                "test-location",
                None,
                None,
                "missing-cwd",
                &projects,
            )
            .unwrap(),
            expected
        );
    }

    #[test]
    fn transcript_path_ignores_other_session_files() {
        let (_dir, agent, projects) = transcript_location_fixture();
        let other = projects.join("-Users-example--summon/other-session.jsonl");
        std::fs::create_dir_all(other.parent().unwrap()).unwrap();
        std::fs::write(&other, "other-session-transcript").unwrap();
        let actual = resolve_transcript_path_in(
            &agent,
            "test-location",
            None,
            None,
            "wanted-session",
            &projects,
        )
        .unwrap();
        assert_eq!(
            actual,
            transcript_path_in_projects(
                &projects,
                &std::fs::canonicalize(
                    agent
                        .runtime
                        .as_ref()
                        .unwrap()
                        .working_dir
                        .as_ref()
                        .unwrap()
                )
                .unwrap()
                .to_string_lossy(),
                "wanted-session",
            )
        );
        assert!(!actual.exists());
        assert_ne!(actual, other);
    }

    #[test]
    fn transcript_path_keeps_recorded_cwd_priority_for_existing_transcript() {
        let (dir, agent, projects) = transcript_location_fixture();
        let recorded = dir.path().join("recorded");
        std::fs::create_dir_all(&recorded).unwrap();
        record_transcript_location(&agent, &recorded.to_string_lossy(), "recorded-session");
        let expected =
            transcript_path_in_projects(&projects, &recorded.to_string_lossy(), "recorded-session");
        std::fs::create_dir_all(expected.parent().unwrap()).unwrap();
        std::fs::write(&expected, "recorded").unwrap();
        let configured = transcript_path_in_projects(
            &projects,
            agent
                .runtime
                .as_ref()
                .unwrap()
                .working_dir
                .as_deref()
                .unwrap(),
            "recorded-session",
        );
        std::fs::create_dir_all(configured.parent().unwrap()).unwrap();
        std::fs::write(&configured, "configured").unwrap();
        assert_eq!(
            resolve_transcript_path_in(
                &agent,
                "test-location",
                None,
                None,
                "recorded-session",
                &projects,
            )
            .unwrap(),
            expected
        );
    }

    #[test]
    fn transcript_path_predicts_explicit_cold_start_directory() {
        let (dir, agent, projects) = transcript_location_fixture();
        let explicit = dir.path().join("explicit");
        std::fs::create_dir_all(&explicit).unwrap();
        let actual = resolve_transcript_path_in(
            &agent,
            "test-location",
            None,
            Some(&explicit.to_string_lossy()),
            "cold-session",
            &projects,
        )
        .unwrap();
        assert_eq!(
            actual,
            transcript_path_in_projects(
                &projects,
                &explicit.canonicalize().unwrap().to_string_lossy(),
                "cold-session"
            )
        );
        assert!(!actual.exists());
    }

    #[test]
    fn transcript_path_follows_per_chat_cwd_override() {
        // 冷启预测遵循与启动侧相同的 per-chat cwd 优先级。
        let tmp = std::env::temp_dir().join(format!("summon-tscwd-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let mut agent: summon_core::registry::Agent =
            serde_yaml::from_str("name: starlight\nruntime:\n  working_dir: /tmp").unwrap();
        let agent_dir = tmp.join("agents").join(&agent.name);
        std::fs::create_dir_all(&agent_dir).unwrap();
        agent.dir = agent_dir;
        let chats = agent.state_dir().join("runtime").join("chats");
        std::fs::create_dir_all(&chats).unwrap();
        // 本会话设了 per-chat cwd 覆盖(等价于跑过 /cd /var/tmp)。
        std::fs::write(chats.join("soul-starlight-gz.cwd"), "/var/tmp\n").unwrap();
        let sess = crate::session::session::Session::with_name(
            &agent,
            "soul-starlight-gz".into(),
            100,
            100,
        );
        let t = Transcript::new(&sess, crate::runtime::executor::LocalExecutor);
        // per-chat /cd 覆盖 /var/tmp 应优先于 agent 级 /tmp(与 launch cwd 一致)。
        assert_eq!(
            t.transcript_path("test-transcript-path-sid").unwrap(),
            claude_transcript_path(
                &std::fs::canonicalize("/var/tmp").unwrap().to_string_lossy(),
                "test-transcript-path-sid"
            )
            .unwrap()
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn transcript_path_priority1_verifies_recorded_cwd_before_trusting() {
        // 记录的 cwd 缺失时，预测路径采用存在的有效启动目录。
        let uniq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let tmp = std::env::temp_dir().join(format!("summon-c6-{}-{uniq}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let mut agent: summon_core::registry::Agent = serde_yaml::from_str(&format!(
            "name: starlight\nruntime:\n  working_dir: {}",
            tmp.join("fallback").to_string_lossy()
        ))
        .unwrap();
        let agent_dir = tmp.join("agents").join(&agent.name);
        std::fs::create_dir_all(&agent_dir).unwrap();
        agent.dir = agent_dir;
        let chats = agent.state_dir().join("runtime").join("chats");
        std::fs::create_dir_all(&chats).unwrap();
        // 随机 sid(磁盘上必不存在其转录);.session 的 cwd 指向「转录不在那儿」的旧目录
        // (模拟 /cd 覆写成新目录、转录仍留旧目录 → 该 cwd 推导的文件不存在)。
        let sid = format!("c6sid-{}-{uniq}", std::process::id());
        let stale = "/nonexistent-stale-cwd-c6";
        std::fs::write(
            chats.join("soul-starlight-gz.session"),
            summon_core::session_id::format_record("claude", &sid, Some(stale), None),
        )
        .unwrap();
        // 当前配置(agent 级 working_dir)= 回退目标(真实存在的目录)。
        let fallback = tmp.join("fallback");
        std::fs::create_dir_all(&fallback).unwrap();
        let sess = crate::session::session::Session::with_name(
            &agent,
            "soul-starlight-gz".into(),
            100,
            100,
        );
        let t = Transcript::new(&sess, crate::runtime::executor::LocalExecutor);
        // stale cwd 下无该 sid 转录、SID 搜也搜不到 → 落到 #2 回退 fallback,而非无条件采信 stale。
        assert_eq!(
            t.transcript_path("test-transcript-path-sid").unwrap(),
            claude_transcript_path(
                &fallback.canonicalize().unwrap().to_string_lossy(),
                "test-transcript-path-sid"
            )
            .unwrap()
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn transcript_path_skips_stale_chat_cwd_override_when_dir_gone() {
        // 陈旧的 per-chat 覆盖与启动侧使用同一套目录存在性校验。
        let tmp = std::env::temp_dir().join(format!("summon-tscwd-stale-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let fallback = tmp.join("fallback");
        std::fs::create_dir_all(&fallback).unwrap();
        let fallback_s = fallback.to_string_lossy().into_owned();
        let mut agent: summon_core::registry::Agent = serde_yaml::from_str(&format!(
            "name: starlight\nruntime:\n  working_dir: {fallback_s}"
        ))
        .unwrap();
        let agent_dir = tmp.join("agents").join(&agent.name);
        std::fs::create_dir_all(&agent_dir).unwrap();
        agent.dir = agent_dir;
        let chats = agent.state_dir().join("runtime").join("chats");
        std::fs::create_dir_all(&chats).unwrap();
        // 本会话 .cwd 覆盖指向一个不存在的目录(等价于 /cd 到过的目录后来被删)。
        std::fs::write(
            chats.join("soul-starlight-gz.cwd"),
            "/nonexistent-stale-cwd\n",
        )
        .unwrap();
        let sess = crate::session::session::Session::with_name(
            &agent,
            "soul-starlight-gz".into(),
            100,
            100,
        );
        let t = Transcript::new(&sess, crate::runtime::executor::LocalExecutor);
        assert_eq!(
            t.transcript_path("test-transcript-path-sid").unwrap(),
            claude_transcript_path(
                &fallback.canonicalize().unwrap().to_string_lossy(),
                "test-transcript-path-sid"
            )
            .unwrap(),
            "陈旧 .cwd 覆盖必须被跳过,落到存在的 agent 默认目录"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn transcript_path_skips_level1_when_session_record_cwd_dir_gone() {
        // 记录 cwd 和 per-chat 覆盖均缺失时，按 agent 默认目录预测。
        let uniq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let tmp = std::env::temp_dir().join(format!("summon-l1skip-{}-{uniq}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let fallback = tmp.join("fallback");
        std::fs::create_dir_all(&fallback).unwrap();
        let fallback_s = fallback.to_string_lossy().into_owned();
        let mut agent: summon_core::registry::Agent = serde_yaml::from_str(&format!(
            "name: starlight\nruntime:\n  working_dir: {fallback_s}"
        ))
        .unwrap();
        let agent_dir = tmp.join("agents").join(&agent.name);
        std::fs::create_dir_all(&agent_dir).unwrap();
        agent.dir = agent_dir;
        let chats = agent.state_dir().join("runtime").join("chats");
        std::fs::create_dir_all(&chats).unwrap();
        // .session 记录:cwd 指向已删目录,sid 随机(磁盘上必无其转录)。
        let sid = format!("l1skip-{}-{uniq}", std::process::id());
        std::fs::write(
            chats.join("soul-starlight-gz.session"),
            summon_core::session_id::format_record(
                "claude",
                &sid,
                Some("/nonexistent-session-cwd-l1"),
                None,
            ),
        )
        .unwrap();
        // .cwd 覆盖同样陈旧(事故链形态)。
        std::fs::write(
            chats.join("soul-starlight-gz.cwd"),
            "/nonexistent-session-cwd-l1\n",
        )
        .unwrap();
        let sess = crate::session::session::Session::with_name(
            &agent,
            "soul-starlight-gz".into(),
            100,
            100,
        );
        let t = Transcript::new(&sess, crate::runtime::executor::LocalExecutor);
        assert_eq!(
            t.transcript_path("test-transcript-path-sid").unwrap(),
            claude_transcript_path(
                &fallback.canonicalize().unwrap().to_string_lossy(),
                "test-transcript-path-sid"
            )
            .unwrap(),
            ".session cwd 目录已删必须跳过第1级,且第2级不得再采信同样陈旧的 .cwd"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn transcript_path_rejects_all_missing_configured_directories() {
        // 显式配置的目录全不可访问时，只允许恢复已经存在的同 SID 转录。
        let tmp = std::env::temp_dir().join(format!(
            "summon-allmissing-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        let mut agent: summon_core::registry::Agent = serde_yaml::from_str(
            "name: starlight\nruntime:\n  working_dir: /nonexistent-allmissing-default",
        )
        .unwrap();
        let agent_dir = tmp.join("agents").join(&agent.name);
        std::fs::create_dir_all(&agent_dir).unwrap();
        agent.dir = agent_dir;
        let chats = agent.state_dir().join("runtime").join("chats");
        std::fs::create_dir_all(&chats).unwrap();
        std::fs::write(
            chats.join("soul-starlight-gz.cwd"),
            "/nonexistent-allmissing-cwd\n",
        )
        .unwrap();
        let sess = crate::session::session::Session::with_name(
            &agent,
            "soul-starlight-gz".into(),
            100,
            100,
        );
        let t = Transcript::new(&sess, crate::runtime::executor::LocalExecutor);
        assert!(t.transcript_path("test-transcript-path-sid").is_err());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    // ===== RR3.2 + RR3.3:会话快照挂载 + claude 转录解析层回放 =====

    /// RR3.2 挂载形状验证:`RecordStore::snapshot_claude` 重建的快照子树,与「临时 HOME 指向快照根」
    /// 时 `claude_transcript_path(cwd, sid)` 的解析路径**逐段一致**——即回放挂载只需把 HOME 设成
    /// `session/claude` 即命中快照那份转录,全程不碰真实 `~/.claude`。
    ///
    /// 不真改全局 `HOME`(避免与并发测数据竞争):转而断言「快照写出的相对子树 == HOME-relative 解析
    /// 路径的相对子树」。`claude_transcript_path` 在某 home 下恒为 `<home>/.claude/projects/<proj>/
    /// <sid>.jsonl`,而 `snapshot_claude` 写的就是 `session/claude/.claude/projects/<proj>/<sid>.jsonl`
    /// (HOME 子树前缀 `.claude/…` 同构),故两者 strip 各自前缀后必须相等。
    // `RecordStore` 是 debug-only 录制设施,release 下不存在。
    #[cfg(debug_assertions)]
    #[test]
    fn replay_mount_snapshot_subtree_matches_home_relative_transcript_path() {
        use crate::runtime::record_store::{RecordStore, SessionCli};

        // 造一份真实 HOME 下的转录,snapshot 出来。
        let real_home = tempfile::tempdir().unwrap();
        let raw = tempfile::tempdir().unwrap();
        let store = RecordStore::new(raw.path());
        let cwd = "/Users/me/work";
        let sid = "07778859-d337-4a01-a41d-e4c11ae7be81";
        let proj = summon_core::claude_log::project_dirname(cwd);
        let real_dir = real_home
            .path()
            .join(".claude")
            .join("projects")
            .join(&proj);
        std::fs::create_dir_all(&real_dir).unwrap();
        std::fs::write(real_dir.join(format!("{sid}.jsonl")), FIXTURE).unwrap();
        assert_eq!(
            store
                .snapshot_session("scn", SessionCli::Claude, sid, Some(cwd), real_home.path())
                .unwrap(),
            1
        );

        // 快照里那份转录,相对「挂载根」session/claude 的子路径。
        let mount_root = store.session_dir("scn").join("claude");
        let snapped = mount_root
            .join(".claude")
            .join("projects")
            .join(&proj)
            .join(format!("{sid}.jsonl"));
        assert!(snapped.is_file(), "快照应有那份转录: {}", snapped.display());
        let snap_rel = snapped.strip_prefix(&mount_root).unwrap();

        // 若把 HOME 设成 mount_root,claude_transcript_path 解析出的 HOME-relative 子路径。
        // 这里用任意 home 算相对子路径(只比相对部分,不依赖真实 HOME)。
        let probe_home = std::path::Path::new("/probe-home");
        let resolved_rel = probe_home
            .join(".claude")
            .join("projects")
            .join(&proj)
            .join(format!("{sid}.jsonl"));
        let resolved_rel = resolved_rel.strip_prefix(probe_home).unwrap();

        assert_eq!(
            snap_rel, resolved_rel,
            "快照子树须与 HOME-relative 转录解析路径同构(挂载即命中)"
        );
    }

    /// RR3.3 解析层回放:把录制的 `<sid>.jsonl` 整份喂 `TranscriptTail`(claude 转录解析器),断言
    /// 解析出的 AgentEvent(工具名 + end_turn 终态文本 + tee 的全量事件)。**交互层 tmux 不回放**——
    /// 这里只验解析器对录制转录的行为(改了 transcript 解析能被回归抓住)。
    #[test]
    fn replay_claude_transcript_parse_layer_from_snapshot() {
        // 模拟从 session 快照读回的录制转录字节(与真实 `<sid>.jsonl` 同形)。
        let recorded = FIXTURE;
        // tee 全量 AgentEvent(回放断言解析器产出的事件序列,不只是终态)。
        let mut events: Vec<(AgentKind, Option<String>)> = Vec::new();
        let mut tail = TranscriptTail::default();
        let step = tail.feed(recorded, &mut |ev| {
            events.push((ev.kind.clone(), ev.text.clone()));
        });
        // 终态:end_turn 行收尾,工具名 Bash,最终回复 = end_turn 文本。
        match step {
            TailStep::Done {
                tools, final_text, ..
            } => {
                assert_eq!(tools, vec!["Bash".to_string()]);
                assert_eq!(final_text, "There are 2 files.");
            }
            TailStep::Progress { .. } => panic!("录制转录应在 end_turn 收尾"),
            TailStep::ApiError { .. } => panic!("录制转录无 API 错误,不应走此分支"),
        }
        // 解析器 tee 出的全量事件含中途 assistant 叙述 + 工具 + 终态 assistant。
        assert!(
            events.iter().any(|(k, t)| *k == AgentKind::Assistant
                && t.as_deref() == Some("Let me check the file.")),
            "应解析出中途 assistant 叙述事件"
        );
        assert!(
            events.iter().any(|(k, _)| *k == AgentKind::ToolUse),
            "应解析出 tool_use 事件"
        );
    }

    #[test]
    fn terminal_evidence_keeps_next_prompt_enqueue_out_of_the_previous_attach() {
        let marker = "summon-transport-turn:123e4567-e89b-12d3-a456-426614174000";
        let enqueue = serde_json::json!({
            "type": "queue-operation",
            "operation": "enqueue",
            "content": format!("<!-- {marker} -->")
        })
        .to_string()
            + "\n";
        let attach_terminal = rec("text", "A attach", Some("end_turn"));
        let consumed = serde_json::json!({
            "type": "attachment",
            "attachment": {
                "type": "queued_command",
                "prompt": format!("<!-- {marker} -->")
            }
        })
        .to_string()
            + "\n";
        let prompt_terminal = rec("text", "B reply", Some("end_turn"));

        let mut tail = TranscriptTail::default();
        let first = tail.feed(
            format!("{enqueue}{attach_terminal}{consumed}{prompt_terminal}").as_bytes(),
            &mut |_| {},
        );
        assert!(matches!(first, TailStep::Done { .. }));
        assert_eq!(tail.marker_evidence.enqueued(), [marker]);
        assert!(tail.marker_evidence.consumed().is_empty());

        tail.reset_turn();
        let second = tail.feed(b"", &mut |_| {});
        assert!(matches!(second, TailStep::Done { .. }));
        assert!(tail.marker_evidence.enqueued().is_empty());
        assert_eq!(tail.marker_evidence.consumed(), [marker]);
    }

    #[test]
    fn unrelated_marker_records_never_become_consumed_terminal_evidence() {
        let marker = "summon-transport-turn:123e4567-e89b-12d3-a456-426614174099";
        let records = [
            serde_json::json!({"type": "last-prompt", "lastPrompt": marker}),
            serde_json::json!({
                "type": "assistant",
                "message": {"role": "assistant", "content": marker}
            }),
            serde_json::json!({
                "type": "attachment",
                "attachment": {"type": "edited_text_file", "snippet": marker}
            }),
            serde_json::json!({
                "type": "user",
                "message": {
                    "role": "user",
                    "content": [{"type": "tool_result", "content": marker}]
                }
            }),
        ];
        for record in records {
            assert_eq!(
                transport_marker_evidence(&record.to_string()),
                None,
                "白名单外记录不得被提升为 consumed:{record}"
            );
            assert_eq!(
                uncertain_transport_marker(&record.to_string()),
                None,
                "已知无关 shape 也不能阻塞 terminal:{record}"
            );
        }

        let unknown = serde_json::json!({"type": "provider-schema-v-next", "opaque": marker});
        assert_eq!(transport_marker_evidence(&unknown.to_string()), None);
        assert_eq!(
            uncertain_transport_marker(&unknown.to_string()),
            Some(marker.to_string()),
            "未知 shape 只能升级为 uncertain，绝不能猜成 Consumed"
        );

        for unknown in [
            serde_json::json!({
                "type": "attachment",
                "attachment": {"type": "provider-v-next", "opaque": marker}
            }),
            serde_json::json!({
                "type": "queue-operation",
                "operation": "provider-v-next",
                "metadata": marker
            }),
        ] {
            assert_eq!(transport_marker_evidence(&unknown.to_string()), None);
            assert_eq!(
                uncertain_transport_marker(&unknown.to_string()),
                Some(marker.to_string()),
                "typed 提取失败的未来 attachment/queue shape 必须 fail-closed:{unknown}"
            );
        }

        for operation in ["enqueue", "remove"] {
            let record = serde_json::json!({
                "type": "queue-operation",
                "operation": operation,
                "content": marker
            });
            assert_eq!(
                transport_marker_evidence(&record.to_string()),
                Some((MirrorMarkerState::Enqueued, marker.to_string())),
                "queue-operation/{operation} 只能作为 Enqueued 顺序证据"
            );
        }

        let mut evidence = MirrorMarkerEvidence::default();
        evidence.remember(MirrorMarkerState::Enqueued, marker.to_string());
        evidence.remember_uncertain(marker.to_string());
        assert_eq!(
            evidence.uncertain(),
            [marker],
            "Enqueued 之后的 unknown shape 可能正是消费记录，不能被较弱证据盖掉"
        );
        evidence.remember(MirrorMarkerState::Consumed, marker.to_string());
        assert!(
            evidence.uncertain().is_empty(),
            "后续白名单 typed 记录应收敛同 marker 的 unknown shape"
        );
    }

    #[test]
    fn unknown_marker_shape_observes_only_one_terminal_and_keeps_watcher_alive() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let dir = tempfile::tempdir().unwrap();
        let marker = "summon-transport-turn:123e4567-e89b-12d3-a456-426614174099";
        let terminal = serde_json::json!({
            "type": "assistant",
            "message": {
                "role": "assistant",
                "stop_reason": "end_turn",
                "content": [{"type": "text", "text": "answer"}]
            }
        });
        let unknowns = [
            serde_json::json!({"type": "provider-schema-v-next", "opaque": marker}).to_string(),
            serde_json::json!({
                "type": "attachment",
                "attachment": {"type": "provider-v-next", "opaque": marker}
            })
            .to_string(),
            serde_json::json!({
                "type": "queue-operation",
                "operation": "provider-v-next",
                "metadata": marker
            })
            .to_string(),
            format!(r#"{{"type":"provider-broken","opaque":"{marker}""#),
        ];
        for (index, unknown) in unknowns.into_iter().enumerate() {
            let path = dir
                .path()
                .join(format!("unknown-marker-shape-{index}.jsonl"));
            let future_terminal = serde_json::json!({
                "type": "assistant",
                "message": {
                    "role": "assistant",
                    "stop_reason": "end_turn",
                    "content": [{"type": "text", "text": "future attach"}]
                }
            });
            let transcript = format!("{unknown}\n{terminal}\n{future_terminal}\n");
            std::fs::write(&path, "").unwrap();
            let stop = AtomicBool::new(false);
            let mut observed = Vec::new();
            let mut commits = 0usize;
            tail_forward_turns(
                &crate::runtime::executor::LocalExecutor,
                &path,
                0,
                FileTailCheckpoint::capture_at(&crate::runtime::executor::LocalExecutor, &path, 0)
                    .unwrap(),
                Duration::from_millis(1),
                Duration::from_millis(1),
                &stop,
                None,
                false,
                None, // catchup_floor:本测不验停机补发
                &mut |kind, _, _, _, _| {
                    observed.push(kind);
                    Ok(())
                },
                &mut |_, _| {
                    commits += 1;
                    if commits == 2 {
                        stop.store(true, Ordering::Relaxed);
                    }
                    Ok(())
                },
                &mut || std::fs::write(&path, &transcript).unwrap(),
            )
            .unwrap();
            assert_eq!(
                observed,
                [MirrorTurnKind::AmbiguousObserved, MirrorTurnKind::Reply],
                "unknown shape 只跳过当前 terminal，下一条确定的 live turn 仍可发送"
            );
            assert_eq!(commits, 2, "歧义轮与后续正常轮都应推进各自 boundary");
        }
    }

    #[test]
    fn initial_unknown_and_overflowed_backlog_is_observed_and_committed_locally() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("historical-unknown-overflow.jsonl");
        let mut transcript = String::new();
        for index in 1..=17u128 {
            transcript.push_str(
                &serde_json::json!({
                    "type": "user",
                    "message": {
                        "role": "user",
                        "content": format!(
                            "summon-transport-turn:{}",
                            uuid::Uuid::from_u128(index)
                        )
                    }
                })
                .to_string(),
            );
            transcript.push('\n');
        }
        transcript.push_str(
            r#"{"type":"provider-broken","opaque":"summon-transport-turn:123e4567-e89b-12d3-a456-426614174099""#,
        );
        transcript.push('\n');
        transcript.push_str(&rec("text", "historical answer", Some("end_turn")));
        std::fs::write(&path, &transcript).unwrap();

        let stop = AtomicBool::new(false);
        let mut observed = Vec::new();
        let mut committed = Vec::new();
        tail_forward_turns(
            &crate::runtime::executor::LocalExecutor,
            &path,
            0,
            FileTailCheckpoint::capture_at(&crate::runtime::executor::LocalExecutor, &path, 0)
                .unwrap(),
            Duration::from_millis(1),
            Duration::from_millis(1),
            &stop,
            None,
            false,
            None, // catchup_floor:本测不验停机补发
            &mut |kind, _, _, _, _| {
                observed.push(kind);
                Ok(())
            },
            &mut |offset, _| {
                committed.push(offset);
                stop.store(true, Ordering::Relaxed);
                Ok(())
            },
            &mut || {},
        )
        .unwrap();

        assert_eq!(observed, [MirrorTurnKind::HistoricalObserved]);
        assert_eq!(committed, [transcript.len() as u64]);
    }

    #[test]
    fn normal_run_turn_never_advances_the_seed_prefix_cutoff() {
        let production = production_source();
        let run_turn = production.find("pub fn run_turn(").expect("run_turn 定义");
        let after = &production[run_turn..];
        let end = after
            .find("pub(crate) fn mirror_tail(")
            .expect("mirror_tail 定义");
        assert!(
            !after[..end].contains("mirror_watermark"),
            "normal turn 不能推进 prefix cutoff；gate 会跨过尚未投递的前序 attach"
        );
    }
}
