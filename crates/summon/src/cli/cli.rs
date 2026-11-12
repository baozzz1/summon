use anyhow::Context;
use clap::{error::ErrorKind, Parser, Subcommand};
use std::collections::BTreeSet;
use std::io::Write;
use std::process::Command;
use summon_core::config::Config;
use summon_core::credential_identity::{
    classify_credential_identity, fingerprint_of, CredentialIdentity, IdentityUnknown,
};
use summon_core::event_log::EventRecord;
use summon_core::registry::Registry;

use crate::cli::style;
use crate::cli::table;
use crate::session::resume_store;
use crate::session::session::{Session, Start};
use crate::session::tmux::Tmux;

use summon_core::autostart::DAEMON_PROCESS_NAME;

fn parse_history_limit(raw: &str) -> Result<usize, String> {
    let value = raw
        .parse::<usize>()
        .map_err(|_| summon_i18n::format!("r4e4642f71e"))?;
    if !(1..=200).contains(&value) {
        return Err(summon_i18n::text!("rfb03d366af").to_string());
    }
    Ok(value)
}

/// 会话活跃度标签 → 着色色相(任务1):live/recent 绿、idle 黄、gone 红暗。
fn liveness_hue(label: &str) -> style::Hue {
    match label {
        // teardown 泄漏:告警色。落进下面的 `_` 会把事故渲成和正常会话一样的绿色 —— 那正是
        // 「数据层标了事故、展示层伪装成正常」的老毛病。
        "leaked" => style::Hue::Alarm,
        // `Liveness::label()` 给无头闲置返回的是 "stale" 而不是 "idle";只写 "idle" 会漏配、
        // 让 stale 也走 `_` 变绿。两个都列上,不靠记忆去对。
        "stale" | "idle" => style::Hue::Idle,
        "gone" => style::Hue::Gone,
        _ => style::Hue::Good, // live / recent
    }
}

#[derive(Parser)]
#[command(
    name = "summon",
    about = summon_i18n::text!("re1aa1960c9"),
    // `--version` 打 semver + commit sha;比较版本的地方一律只用 VERSION,不带 sha。
    version = crate::version::LONG_VERSION,
    after_help = summon_i18n::text!("r8687dbc4c9")
)]
pub struct Cli {
    #[command(subcommand)]
    pub cmd: Cmd,
}

pub fn parse() -> Cli {
    super::completion::complete();
    match Cli::try_parse() {
        Ok(cli) => cli,
        Err(mut err) => {
            let args: Vec<_> = std::env::args_os().collect();
            let command = args.get(1).and_then(|arg| arg.to_str());
            let action = args.get(2).and_then(|arg| arg.to_str());
            let internal = matches!(
                (command, action),
                (Some("bridge"), Some("hook"))
                    | (Some("daemon"), Some("claude-quota-statusline"))
                    | (
                        Some("install-stamp" | "program-status-run" | "comment-reader"),
                        _
                    )
            );
            if err.kind() != ErrorKind::DisplayVersion && !internal {
                crate::runtime::localization::configure_system();
                let config = Config::load();
                summon_core::tools::configure(&config);
                crate::runtime::localization::configure(&config);
                err = match Cli::try_parse() {
                    Ok(cli) => return cli,
                    Err(error) => error,
                };
            }
            let hint = if matches!(
                err.kind(),
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
            ) {
                None
            } else {
                agents_parse_error_hint(&std::env::args().collect::<Vec<_>>())
            };
            let _ = err.print();
            if let Some(hint) = hint {
                eprintln!("\n{hint}");
            }
            // 首次运行 + 裸 summon(无子命令):给 onboarding 落点,而不是只打 clap 帮助。
            // 见 docs/spec/2026-08-20-oss-onboarding-experience-design.md 第 3.2 节。
            if std::env::args_os().count() == 1 && is_first_run() {
                eprintln!(
                    "\n{FIRST_RUN_HINT}",
                    FIRST_RUN_HINT = ::summon_core::i18n::label(FIRST_RUN_HINT)
                );
            }
            std::process::exit(err.exit_code());
        }
    }
}

/// 首次运行:`~/.summon` 不存在,或存在但没有任何 agent 脚手架。
/// 轻量探测:只看目录,不加载 Config/registry(parse 错误路径上要便宜、不能再 panic)。
fn is_first_run() -> bool {
    let Some(home) = std::env::var_os("HOME") else {
        return false;
    };
    is_first_run_at(&std::path::Path::new(&home).join(".summon/agents"))
}

/// `is_first_run` 的纯函数版本:agents 目录不存在,或存在但没有任何 agent.yaml。
fn is_first_run_at(agents_dir: &std::path::Path) -> bool {
    if !agents_dir.is_dir() {
        return true;
    }
    std::fs::read_dir(agents_dir)
        .is_ok_and(|rd| rd.flatten().all(|e| !e.path().join("agent.yaml").is_file()))
}

/// 首次运行提示:告诉全新用户「下一步干嘛」。
const FIRST_RUN_HINT: &str = "r56be42d43e";

#[derive(Subcommand)]
pub enum CommentsAction {
    #[command(about = summon_i18n::text!("r6b884bd9ba"))]
    /// 查看任务状态和起草进程。
    List,
    #[command(about = summon_i18n::text!("radfc639bef"))]
    /// 停止本次起草，任务结束后不会重新起草。
    Stop {
        #[arg(help = summon_i18n::text!("rbd7e0edf70"))]
        /// 完整评论任务 ID，来自 comments list。
        id: String,
        #[arg(help = summon_i18n::text!("r677f4fb519"))]
        /// 处理该评论的 agent 别名。
        #[arg(long)]
        agent: String,
    },
}

#[derive(Subcommand)]
pub enum Cmd {
    #[command(about = summon_i18n::text!("r7e09aa5d48"))]
    /// 查看或停止文档评论的起草任务。
    Comments {
        #[command(subcommand)]
        action: CommentsAction,
    },
    #[command(about = summon_i18n::text!("r9795d8a4e0"))]
    /// 为评论起草转发绑定文档的只读 MCP 数据。
    #[command(hide = true)]
    CommentReader {
        #[arg(help = summon_i18n::text!("rae6b60eeeb"))]
        /// 当前评论起草的私有 Unix socket。
        #[arg(long)]
        socket: std::path::PathBuf,
    },
    #[command(about = summon_i18n::text!("rc63188d348"))]
    /// 通过出站 SSH 使用另一台机器的文件与命令工具。
    RemoteTools {
        #[command(subcommand)]
        action: crate::remote_tools::RemoteToolsCommand,
    },
    #[command(about = summon_i18n::text!("reea94e342b"))]
    /// 在透明 PTY 内运行 CLI 并采集程序状态（内部启动入口）。
    #[command(hide = true)]
    ProgramStatusRun {
        #[arg(help = summon_i18n::text!("r1262f2515f"))]
        /// 有界状态快照路径。
        path: std::path::PathBuf,
        #[arg(help = summon_i18n::text!("r25183ffee6"))]
        /// 本次启动的随机代际标识。
        generation: String,
        #[arg(help = summon_i18n::text!("r7cd365b863"))]
        /// 已组装并转义的 CLI 启动命令。
        command: String,
    },
    #[command(about = summon_i18n::text!("refde3a03d2"))]
    /// 稳定落点 `~/.summon/bin` 的构建戳(内部入口:安装脚本调用,不属于用户命令面)。
    #[command(hide = true)]
    InstallStamp {
        #[command(subcommand)]
        action: InstallStampAction,
    },
    #[command(about = summon_i18n::text!("rcb677bc02b"))]
    /// 管理 agent 与会话:ls/up/down/attach/kill/activate/model/effort
    /// (裸 `agents` 显示子命令;列表走 `agents ls`)
    Agents {
        #[command(subcommand)]
        action: Option<AgentsAction>,
    },
    #[command(about = summon_i18n::text!("rea140b21b3"))]
    /// agent 在 chat 轮内向用户提问并**阻塞等答复**。一次可问多题,飞书发一张卡、用户一次答完。
    /// 只给无头 CLI(cursor/codex)用 —— claude 走 tmux 抓屏中介,自己弹菜单即可。
    /// 答案打到 stdout:`{"answers":[{"q":"…","a":"…"|null}]}`,未答的题 `a` 为 null。
    /// **不超时**:等到用户提交或点「全部跳过」为止。
    // agent 在会话里用,不在给人看的帮助里列出(`summon guide commands` 有完整说明)。
    #[command(arg_required_else_help = true, hide = true)]
    Ask {
        #[arg(help = summon_i18n::text!("rf6f523a9df"))]
        /// 本轮 prompt 给出的 turn 凭证。
        #[arg(long)]
        turn_id: String,
        #[arg(help = summon_i18n::text!("r9722bce606"))]
        /// 问题数组 JSON:`[{"q":"…","options":["a","b"]},{"q":"…"}]`;options 省略 = 开放式。
        #[arg(long)]
        questions: String,
    },
    #[command(about = summon_i18n::text!("re8e3f85378"))]
    /// agent 在**自己的 chat 会话内**给「当前正回应的那条飞书消息」贴一个语义表情。
    /// 可改可撤、单值:`summon react --turn-id <凭证> <emoji>` 设置/替换、加 `--clear` 撤掉。
    /// 身份与目标 chat **只认 `--turn-id`,不认环境变量**;凭证过期或不在活跃轮内即拒。
    /// 与系统自动贴的 OnIt/DONE 是两条独立轨。
    // agent 在会话里用,不在给人看的帮助里列出(`summon guide commands` 有完整说明)。
    #[command(arg_required_else_help = true, hide = true)]
    React {
        #[arg(help = summon_i18n::text!("re7a7bd73f2"))]
        /// 本轮 prompt 给出的 turn 凭证。**身份以它为准,不认环境变量** —— 见 `resolve_turn_target`。
        #[arg(long)]
        turn_id: String,
        #[arg(help = summon_i18n::text!("r1347703de1"))]
        /// 表情名(须在 messaging.reactions 安全集内);省略 + `--clear` 则撤掉当前语义表情。
        emoji: Option<String>,
        #[arg(help = summon_i18n::text!("r6af564ea6d"))]
        /// 撤掉 agent 当前在该消息上贴的语义表情。
        #[arg(long)]
        clear: bool,
    },
    #[command(about = summon_i18n::text!("r2c89222056"))]
    /// 向目标群发布摘要，由本机同一 agent 在该群执行完整任务。
    #[command(arg_required_else_help = true, hide = true)]
    Delegate {
        #[arg(help = summon_i18n::text!("r01981443bb"))]
        /// 本轮 prompt 给出的 turn 凭证，身份与源群以它为准。
        #[arg(long)]
        turn_id: String,
        #[arg(help = summon_i18n::text!("rdf4072a49a"))]
        /// 已有目标群的完整名称、chats 序号或 chat_id。
        #[arg(
            long,
            required_unless_present = "new_chat",
            conflicts_with = "new_chat"
        )]
        to: Option<String>,
        #[arg(help = summon_i18n::text!("r0d2e9ace9e"))]
        /// 新建仅含 Owner 与当前 Bot 的任务群。
        #[arg(long, required_unless_present = "to", conflicts_with = "to")]
        new_chat: Option<String>,
        #[arg(help = summon_i18n::text!("ra17c96820a"))]
        /// 群内可公开摘要；省略时显示固定提示。
        #[arg(long)]
        summary: Option<String>,
        #[arg(help = summon_i18n::text!("rebeda94f34"))]
        /// 私有委派任务正文；传 - 从标准输入读取。
        #[arg(long)]
        content: String,
        #[arg(help = summon_i18n::text!("r7442be9950"))]
        /// 只解析目标群、报告是否需要拉 bot 进群，不发卡也不投递。
        #[arg(long)]
        dry_run: bool,
    },
    #[command(about = summon_i18n::text!("r6cc6d9af5e"), long_about = summon_i18n::text!("r8400629796"))]
    /// agent 在**自己的 chat 会话内**给这个 chat 建一个定时器(到点把 `--content` 作为一轮
    /// 新任务投给自己)。取代「手改 `~/.summon/schedules.yaml`」——手改绕过了 id 分配、
    /// 节律校验与身份归属,坏一个字段整份表都读不出来。
    ///
    /// 节律与卡片完全同一套语义(落盘 spec 逐字节一致),`--when` 取以下之一:
    ///
    /// `in 30m` | `at 2026-08-22T09:00` | `every 2h` | `daily 09:00` |
    /// `weekday 09:30` | `weekly mon,fri 10:00` | `cron 0 9 * * 1-5`
    ///
    /// 星期几认 mon..sun / Monday / 周一..周日 / 0..7。
    /// 删除与查看仍走 `summon schedules ls|rm`。
    // agent 在会话里用,不在给人看的帮助里列出(`summon guide commands` 有完整说明)。
    #[command(arg_required_else_help = true, hide = true)]
    Remind {
        #[arg(help = summon_i18n::text!("r3e6848a7fa"))]
        /// 本轮 prompt 给出的 turn 凭证。**身份以它为准,不认环境变量**。
        #[arg(long)]
        turn_id: String,
        #[arg(help = summon_i18n::text!("ra07b06e90c"))]
        /// 节律表达式,如 `in 30m` / `daily 09:00` / `weekly mon 10:00`。写错会当场列出全部合法形态。
        #[arg(long, value_parser = validate_when)]
        when: String,
        #[arg(help = summon_i18n::text!("r8613b96729"))]
        /// 到点要跑的内容。`-` = 从 stdin 读(长文本 / 多行走这条,避开 argv 长度与引号转义)。
        #[arg(long)]
        content: String,
        #[arg(help = summon_i18n::text!("rcfa68d0046"))]
        /// 推送模式:`progress` 展示执行过程与结果；`result` 只推送最终结果，适合周期报告降噪。
        #[arg(long, value_parser = ["progress", "result"], default_value = "progress")]
        output: String,
        #[arg(help = summon_i18n::text!("rb3cdc85cf3"))]
        /// 上下文模式:`resume` 续用该 chat 的现有会话;`fresh` 每次起跑前重置会话、从空上下文冷启,
        /// 适合状态全在文件里的周期巡检(跨天累积的会话历史对当轮没有信息增量)。
        #[arg(long, value_parser = ["resume", "fresh"], default_value = "resume")]
        context: String,
    },
    #[command(about = summon_i18n::text!("rd5894d7f06"))]
    /// agent 在**自己的 chat 会话内**把一张本地图片发进该 chat(身份与目标只认 `--turn-id`,
    /// 与 `summon react` 同款)。图片走独立消息,不嵌在回复卡片里。
    /// claude(tmux)与 cursor/codex(无头子进程)共用这一条路径。
    // agent 在会话里用,不在给人看的帮助里列出(`summon guide commands` 有完整说明)。
    #[command(arg_required_else_help = true, hide = true)]
    SendImage {
        #[arg(help = summon_i18n::text!("r3e6848a7fa"))]
        /// 本轮 prompt 给出的 turn 凭证。**身份以它为准,不认环境变量**。
        #[arg(long)]
        turn_id: String,
        #[arg(help = summon_i18n::text!("re483ea50f8"))]
        /// 本地图片路径(jpg/jpeg/png/webp/gif/bmp/ico/tiff/heic,≤10MB)。
        path: String,
        #[arg(help = summon_i18n::text!("rc4903772f4"))]
        /// 图片之后再补一句说明文字(可选)。
        #[arg(long)]
        caption: Option<String>,
    },
    #[command(about = summon_i18n::text!("rda0a3b3b24"), long_about = summon_i18n::text!("r5881984136"))]
    /// agent 在**自己的 chat 会话内**把一份本地文件转换成浏览器预览 URL。
    ///
    /// **给 agent 的意义是省钱**:写完一份 spec 想让人看见,复述一遍是几千 token 的输出,
    /// 而内容已经在盘上 —— 这条命令让 daemon 生成能力链接,agent 只需把 URL 放进最终回复。
    // agent 在会话里用,不在给人看的帮助里列出(`summon guide commands` 有完整说明)。
    #[command(arg_required_else_help = true, hide = true)]
    Preview {
        #[arg(help = summon_i18n::text!("r3e6848a7fa"))]
        /// 本轮 prompt 给出的 turn 凭证。**身份以它为准,不认环境变量**。
        #[arg(long)]
        turn_id: String,
        #[arg(help = summon_i18n::text!("r36208b08a9"))]
        /// 本地文件路径(markdown / JSON / 纯文本；其它格式提供下载)。
        path: String,
    },
    #[command(about = summon_i18n::text!("r4949fc4f37"))]
    /// 查看会话状态，标已读、退出或归档；裸命令列出会话
    #[command(
        after_help = summon_i18n::text!("rd308f2e957")
    )]
    Session {
        #[command(subcommand)]
        action: Option<SessionAction>,
        #[arg(help = summon_i18n::text!("r5be1926017"))]
        /// 机器可读输出
        #[arg(long, global = true)]
        json: bool,
    },
    #[command(about = summon_i18n::text!("r094e784d79"), long_about = summon_i18n::text!("re8959c7b7d"))]
    /// 打印**当前这一轮**所在会话的 chat_id(供 agent 拼 lark-cli 等命令时用)。
    ///
    /// 为什么要有它:环境变量里的 `SUMMON_CHAT_ID` 是进程 spawn 那一刻钉死的,RPC 会话被
    /// rekey 迁到同 agent 的另一个 chat 之后就过时了 —— 照它去读历史会读到**上一个群**。
    /// 这条命令按本轮凭证问 daemon 要权威位置,是唯一可信的取法。
    // agent 在会话里用,不在给人看的帮助里列出(`summon guide commands` 有完整说明)。
    #[command(hide = true)]
    ChatId {
        #[arg(help = summon_i18n::text!("rf6f523a9df"))]
        /// 本轮 prompt 给出的 turn 凭证。
        #[arg(long)]
        turn_id: String,
    },
    #[command(about = summon_i18n::text!("r50027d040e"))]
    /// 查看/检索**本 agent 自己**在其它飞书会话里的活动(在 agent 会话内身份只认 `--turn-id`)。
    /// 裸 `chats` 只列索引(会话名 + 时间,无正文);正文要 `chats read`,跨会话搜要 `chats grep`。
    /// 硬闸:在 agent 会话内不能用 `--agent` 指向别的 agent —— 跨 agent 读会话是身份越界。
    Chats {
        #[arg(help = summon_i18n::text!("r977964f181"))]
        /// 本轮 prompt 给出的 turn 凭证。**在 agent 会话内必填** —— 身份以它为准,不认环境变量。
        /// owner 在终端直接用时不需要(那时不在任何轮内,改用 `--agent` 指定)。
        #[arg(long)]
        turn_id: Option<String>,
        #[arg(help = summon_i18n::text!("r67478e4d0a"))]
        /// 看哪个 agent 的会话。在 agent 会话内省略(自动认领自己);owner 在终端上手动指定。
        #[arg(long)]
        agent: Option<String>,
        #[command(subcommand)]
        action: Option<ChatsAction>,
    },
    #[command(about = summon_i18n::text!("ref338a3971"))]
    /// 给 agent 绑/补飞书凭据(脚手架 + 凭据验真):onboard 的非交互/脚本化孪生。
    /// 新建 agent 首选 `onboard`(交互向导);此命令用于脚本化创建或给已存在的目录补凭据。
    #[command(arg_required_else_help = true)]
    Bind(crate::setup::bind::BindArgs),
    #[command(about = summon_i18n::text!("r7347e6e82e"))]
    /// 一条龙交互向导:脚手架→权限→凭据→联调→起 daemon(新手首选)
    Onboard(crate::setup::onboard::OnboardArgs),
    #[command(about = summon_i18n::text!("r7828a4d965"))]
    /// 预检:tmux/claude/souls/凭据
    Doctor {
        #[arg(help = summon_i18n::text!("r7df95fe9f7"))]
        /// 把 tmux 路径写进 `~/.summon/config.yaml`(接候选序号或完整路径)。
        /// 不带该参数就只做体检、不改任何配置。
        #[arg(long, value_name = summon_i18n::text!("reb53af2892"))]
        set_tmux: Option<String>,
    },
    #[command(about = summon_i18n::text!("r26edae6a92"))]
    /// agent 视角的 summon 使用手册(随二进制分发,与版本一致)。
    /// 用户的 agent 不读源码也能靠它答出「summon 能做什么、怎么配、出问题怎么修」。
    Guide {
        #[arg(help = summon_i18n::text!("ra1dad7379f"))]
        /// 分节:commands | config | troubleshooting;省略 = 索引(心智模型 + 分节导航)
        section: Option<String>,
    },
    #[command(about = summon_i18n::text!("rc64abdf8c3"))]
    /// summon 原生 macOS Menu Bar：安装、重启、卸载或查看状态
    #[cfg_attr(not(target_os = "macos"), command(hide = true))]
    Menubar {
        #[command(subcommand)]
        action: MenubarAction,
    },
    #[command(about = summon_i18n::text!("rd90248fb36"))]
    /// 用量与费用:按天 / agent / 群 / CLI 汇总 token 与按公开价格表算的费用(与控制台用量页同一口径)
    #[command(
        after_help = summon_i18n::text!("r3164589899")
    )]
    Usage(crate::cli::usage::UsageArgs),
    #[command(about = summon_i18n::text!("ra4f85830c3"))]
    /// 洞察:回复后到下一条消息的间隔、长间隔期间的别处活动、下一条消息的类型与每日输入量(与控制台洞察页同一口径)
    Insights(crate::cli::insights::InsightsArgs),
    #[command(about = summon_i18n::text!("re4eb0d1cab"))]
    /// 本机打开 summon 控制台(网页里改配置、定时任务、说明文件)
    Dashboard {
        #[command(subcommand)]
        action: DashboardAction,
    },
    #[command(about = summon_i18n::text!("r1d9aca74ac"))]
    /// owner sentinel(任意会话召唤 agent):状态与开关
    Sentinel {
        #[command(subcommand)]
        action: SentinelAction,
    },
    #[command(about = summon_i18n::text!("r15365780ac"))]
    /// daemon 生命周期
    Daemon {
        #[command(subcommand)]
        action: DaemonAction,
    },
    #[cfg(npm_dist)]
    #[command(flatten)]
    NpmDist(crate::setup::npm_dist::Command),
    #[command(about = summon_i18n::text!("rd70d395e72"))]
    /// 【高级场景】外部 Claude 会话桥接:安装、检查或卸载 Claude Hooks。
    /// 默认路径(onboard / agents add)用不到它;仅在要把已有的外部 Claude 会话接管进飞书时使用。
    Bridge {
        #[command(subcommand)]
        action: BridgeAction,
    },
    #[command(about = summon_i18n::text!("r0963be19a5"))]
    /// 登录启动：on/off/status（兼容 enable/disable）
    #[command(
        after_help = summon_i18n::text!("r977e03fa90")
    )]
    Autostart {
        #[command(subcommand)]
        action: AutostartAction,
    },
    #[command(about = summon_i18n::text!("r0e0df453d5"))]
    /// 定时器(P2 自动化):列出 / 删除
    Schedules {
        #[command(subcommand)]
        action: Option<SchedulesAction>,
    },
    #[command(about = summon_i18n::text!("r23a56643c4"))]
    /// 记忆分层(P2):写一条事实到 global 或某 agent 的记忆层(`summon remember`)
    #[command(arg_required_else_help = true)]
    Remember(RememberArgs),
    #[command(about = summon_i18n::text!("r5bcecd69b4"))]
    /// 工作流(P4):列出 / 查看计划 / 执行 `~/.summon/workflows/<name>.yaml`(DAG 多步编排)
    Workflow {
        #[command(subcommand)]
        action: WorkflowAction,
    },
    #[command(about = summon_i18n::text!("r3b7aee1dfc"))]
    /// 开发/调试命令(仅 debug 构建;release 隐藏并屏蔽)
    #[cfg(debug_assertions)]
    Dev {
        #[command(subcommand)]
        action: crate::cli::dev::DevAction,
    },
}

#[derive(Subcommand)]
pub enum AgentsAction {
    #[command(about = summon_i18n::text!("rf9be1c9f2f"))]
    /// 禁用 agent 的后续任务，允许当前轮完成
    Disable {
        #[arg(help = summon_i18n::text!("r4c1adebac0"))]
        /// agent 别名
        agent: String,
    },
    #[command(about = summon_i18n::text!("r6290f7b978"))]
    /// 启用 agent 接收任务
    Enable {
        #[arg(help = summon_i18n::text!("r4c1adebac0"))]
        /// agent 别名
        agent: String,
    },
    #[command(about = summon_i18n::text!("r6b707a804b"))]
    /// 列出活跃 chat 会话(默认折叠 idle/gone 残骸;--all 全列);不带 agent 时列全部
    Ls {
        #[arg(help = summon_i18n::text!("r706ca1ce2a"))]
        /// 只看这个 agent 的会话;省略 = 列全部
        agent: Option<String>,
        #[arg(help = summon_i18n::text!("r4c9dc17d78"))]
        /// 连 idle/gone 的旧会话一起列出(默认只列 live/recent)
        #[arg(long)]
        all: bool,
    },
    #[command(about = summon_i18n::text!("rabf68766a2"))]
    /// 非交互创建 agent:单条 `agents add <alias>`,或 `--file <agents.yaml>` 批量(fleet-as-code)。
    /// 凭据从环境变量读(FEISHU_<ALIAS>_APP_ID/SECRET;清单里只写 env 名,不落真值)。
    /// 交互式新建首选 `onboard` 向导;给已存在的目录补字面凭据用 `bind`。
    Add {
        #[arg(help = summon_i18n::text!("r46868b41dd"))]
        /// 单条创建的 alias(与 --file 二选一)
        alias: Option<String>,
        #[arg(help = summon_i18n::text!("r8fcb79f13e"))]
        /// 批量清单 YAML:map 键为 alias,值含 display/role/cli/model/effort/id_env 等(全可选)
        #[arg(long)]
        file: Option<std::path::PathBuf>,
        #[arg(help = summon_i18n::text!("r9e66c8721a"))]
        /// 覆盖已存在的身份文件(默认跳过已存在的 alias)
        #[arg(long)]
        force: bool,
        #[arg(help = summon_i18n::text!("rf043d111b5"))]
        /// (单条)显示名,缺省用 alias
        #[arg(long)]
        display: Option<String>,
        #[arg(help = summon_i18n::text!("r07f3f13820"))]
        /// (单条)一句话职责,写进 agent.yaml 的 `# 职责:` 注释
        #[arg(long)]
        role: Option<String>,
        #[arg(help = summon_i18n::text!("r454a941735"))]
        /// (单条)启动 cli,缺省 claude
        #[arg(long)]
        cli: Option<String>,
        #[arg(help = summon_i18n::text!("r54e250b284"))]
        /// (单条)启动模型,缺省按 cli 取候选桶首项
        #[arg(long)]
        model: Option<String>,
        #[arg(help = summon_i18n::text!("r74736f6770"))]
        /// (单条)effort 档位(仅 claude/codex)
        #[arg(long)]
        effort: Option<String>,
        #[arg(help = summon_i18n::text!("r2e607e38bd"))]
        /// (单条)干活目录,缺省 ~/.summon/agents/<alias>/workspace
        #[arg(long)]
        workspace: Option<String>,
    },
    #[command(about = summon_i18n::text!("re9f74c9367"))]
    /// 起某 agent 的 tmux 会话(惰性 /summon)
    Up {
        #[arg(help = summon_i18n::text!("rdbcc1ac691"))]
        /// agent 别名(`summon agents ls` 里的 AGENT 列)
        agent: String,
    },
    #[command(about = summon_i18n::text!("r4a219e7fae"))]
    /// 关某 agent 的会话(默认关全部 per-chat 会话;--chat 只关一个)
    Down {
        #[arg(help = summon_i18n::text!("rdbcc1ac691"))]
        /// agent 别名(`summon agents ls` 里的 AGENT 列)
        agent: String,
        #[arg(help = summon_i18n::text!("r53d283f241"))]
        /// 只关这一个 chat 的会话;省略 = 关该 agent 的全部 per-chat 会话
        #[arg(long)]
        chat: Option<String>,
        #[arg(help = summon_i18n::text!("r110c6ea066"))]
        /// 同时丢弃 resume 记录(.session):下次冷启全新会话,不续接上下文
        #[arg(long)]
        reset: bool,
    },
    #[command(about = summon_i18n::text!("r35534a1448"))]
    /// 查看某 agent 会话:claude 走 tmux attach(可交互);无头(cursor/codex)自动降级 tail 事件流。
    /// 不给名字 → 像 `agents ls` 那样列出可选(默认折叠 idle/gone)。`--from-start` 仅对无头降级的 tail 生效。
    Attach {
        #[arg(help = summon_i18n::text!("rfd3e480b9e"))]
        /// 目标:chat_id(`oc_…`/`ou_…`)、chat-tag、tmux 会话名,或 agent 别名(交互多选);省略 = 列出可选
        target: Option<String>,
        #[arg(help = summon_i18n::text!("rff8c32123d"))]
        /// 指定 chat(chat_id 或 chat-tag);省略时按 target 推断
        #[arg(long)]
        chat: Option<String>,
        #[arg(help = summon_i18n::text!("rfedce622df"))]
        /// (无头降级 tail 时)从文件头回放,而非默认从尾跟
        #[arg(long)]
        from_start: bool,
    },
    #[command(about = summon_i18n::text!("rf77235c7b5"))]
    /// 按 chat_id/tag/session 关闭会话;传 agent 名时交互多选(需终端,非 TTY 下直接报错)
    Kill {
        #[arg(help = summon_i18n::text!("r93d7c12d01"))]
        /// 目标:chat_id(`oc_…`/`ou_…`)、chat-tag、tmux 会话名,或 agent 别名(交互多选)
        target: String,
        #[arg(help = summon_i18n::text!("r078fb04a91"))]
        /// 同时丢弃 resume 记录(.session):下次冷启全新会话,不续接上下文(无头经 control.sock 生效)
        #[arg(long)]
        reset: bool,
    },
    #[command(about = summon_i18n::text!("r5bd64b4fcf"))]
    /// fleet 状态:谁在跑一轮、谁空闲、daemon 起来多久(Menu Bar 的数据源)
    Status {
        #[arg(help = summon_i18n::text!("rb99b8ddec7"))]
        /// 机器可读快照(Menu Bar 经此消费;人读省略即可)
        #[arg(long)]
        json: bool,
    },
    #[command(about = summon_i18n::text!("r8f37eb248f"))]
    /// 查看某会话按轮次归并的历史；target 可用 session/tag/chat_id，必须唯一命中
    History {
        #[arg(help = summon_i18n::text!("r9b7771d8cd"))]
        /// 目标:chat_id(`oc_…`/`ou_…`)、chat-tag,或 tmux 会话名
        target: String,
        #[arg(help = summon_i18n::text!("rba9df4e883"))]
        /// 每页轮次数
        #[arg(long, default_value_t = 50, value_parser = parse_history_limit)]
        limit: usize,
        #[arg(help = summon_i18n::text!("r601b2f8d79"))]
        /// 只取 ordinal 小于此值的轮次（上一页返回 next_before）
        #[arg(long)]
        before: Option<u64>,
        #[arg(help = summon_i18n::text!("r91d73746d0"))]
        /// 机器可读历史（Electron 控制台经此消费）
        #[arg(long)]
        json: bool,
    },
    #[command(about = summon_i18n::text!("r1db97eb5a2"))]
    /// 在终端里打开某会话(claude→attach;无头→tail)。终端见 config.yaml 的 `menubar.terminal`
    Open {
        #[arg(help = summon_i18n::text!("r9b7771d8cd"))]
        /// 目标:chat_id(`oc_…`/`ou_…`)、chat-tag,或 tmux 会话名
        target: String,
    },
    #[command(about = summon_i18n::text!("rb9025de9dc"))]
    /// 复用已有 .env 凭据做环境检查,通过后把 draft agent 翻成 active
    Activate {
        #[arg(help = summon_i18n::text!("r63dbde3f2b"))]
        /// agent 别名(`summon agents ls` 里的 AGENT 列)(draft 状态的那个)
        agent: String,
    },
    #[command(about = summon_i18n::text!("r849719da05"))]
    /// 让 daemon 扫盘接新建的 agent 上线(onboard 自动调;手工建完 agent.yaml 后也能用)。
    /// 免 daemon restart —— 后者会打断所有在跑的无头轮。只新增:改名/删 agent 仍需 restart。
    Reload,
    #[command(about = summon_i18n::text!("r4ffedef784"))]
    /// 查看 / 改某 agent 的**默认模型**(写 agent.yaml,与飞书 `/model` 同一份真相)。
    /// 不给 <MODEL> 只打印当前值与该 cli 的候选;给了就写入:无头会话下一轮起用新值,在线的 claude
    /// 会话在两轮之间用 `/model` 就地切换。
    Model {
        #[arg(help = summon_i18n::text!("rdbcc1ac691"))]
        /// agent 别名(`summon agents ls` 里的 AGENT 列)
        agent: String,
        #[arg(help = summon_i18n::text!("rae7c0108b2"))]
        /// 目标模型 id;省略 = 只打印当前值与该 cli 的候选表
        model: Option<String>,
    },
    #[command(about = summon_i18n::text!("r7af92cb24c"))]
    /// 查看 / 改某 agent 的**默认 effort 档位**(用法同 `agents model`;cursor 无独立 effort 轴会被拒;
    /// claude 会话下次冷启生效,要当场切用会话面板)
    Effort {
        #[arg(help = summon_i18n::text!("rdbcc1ac691"))]
        /// agent 别名(`summon agents ls` 里的 AGENT 列)
        agent: String,
        #[arg(help = summon_i18n::text!("r7a8ca48f39"))]
        /// 目标档位(none/minimal/low/medium/high/…,按 cli 而定);省略 = 只打印当前值与候选
        effort: Option<String>,
    },
    #[command(about = summon_i18n::text!("re2a332e9bf"))]
    /// 实时人读跟随某 agent 会话的完整事件流(thinking/工具/回复);默认从尾跟(tail -f 语义)
    Tail {
        #[arg(help = summon_i18n::text!("rdbcc1ac691"))]
        /// agent 别名(`summon agents ls` 里的 AGENT 列)
        agent: String,
        #[arg(help = summon_i18n::text!("r2bdf4f6620"))]
        /// chat-tag;省略时若仅一个会话有 events.jsonl 直接跟,多个则列出让选
        #[arg(long)]
        chat: Option<String>,
        #[arg(help = summon_i18n::text!("r794974c366"))]
        /// 从文件头回放(而非默认从尾跟)
        #[arg(long)]
        from_start: bool,
    },
}

#[derive(Subcommand)]
pub enum DaemonAction {
    #[command(about = summon_i18n::text!("r38c506cac7"))]
    /// 前台运行
    Run,
    #[command(about = summon_i18n::text!("r29ad6745b2"))]
    /// 后台启动(日志写 ~/.summon/run/daemon.log)
    Start,
    #[command(about = summon_i18n::text!("rca4d973c0b"))]
    /// 停止
    Stop,
    #[command(about = summon_i18n::text!("r1a3d7c60f2"))]
    /// 重启(stop 等它退干净再 start;绝不杀 tmux agent 会话;脱离当前会话执行,发起者被打断也会跑完)
    Restart,
    #[command(about = summon_i18n::text!("r8f0a33b335"))]
    /// 查询状态
    Status,
    #[command(about = summon_i18n::text!("r7848aac393"))]
    /// Claude 额度接入：statusLine 安装与状态管理
    ClaudeQuota {
        #[command(subcommand)]
        action: ClaudeQuotaAction,
    },
    #[command(about = summon_i18n::text!("r05df90bf0b"))]
    /// Claude statusLine wrapper 内部入口；由 settings 调用，不属于用户命令面。
    #[command(hide = true)]
    ClaudeQuotaStatusline,
    #[command(about = summon_i18n::text!("rc043d760f2"))]
    /// Claude PreToolUse hook 内部入口；由会话专属 settings 调用，不属于用户命令面。
    #[command(hide = true)]
    ToolGuardHook,
    #[command(about = summon_i18n::text!("rb548badd6b"))]
    /// Claude PermissionRequest hook 内部入口：把授权请求与 AskUserQuestion 转飞书卡片。
    /// 由会话专属 settings 调用，参数是启动该会话时写死的 agent 与 tmux 会话名。
    #[command(hide = true)]
    PermissionHook {
        #[arg(help = summon_i18n::text!("r5868c65cc8"))]
        /// 发起请求的 agent 别名(启动会话时写进 settings)
        #[arg(long)]
        agent: String,
        #[arg(help = summon_i18n::text!("r06e18ca177"))]
        /// 发起请求的 tmux 会话名(启动会话时写进 settings)
        #[arg(long)]
        session: String,
    },
    #[command(about = summon_i18n::text!("rf2ecd477d7"))]
    /// Claude PostCompact hook 内部入口:压缩完成后报给 daemon,推压缩后的上下文长度卡。
    /// 由会话专属 settings 调用,参数是启动该会话时写死的 agent 与 tmux 会话名。
    #[command(hide = true)]
    CompactHook {
        #[arg(help = summon_i18n::text!("rc8188c954f"))]
        /// 压缩完的 agent 别名(启动会话时写进 settings)
        #[arg(long)]
        agent: String,
        #[arg(help = summon_i18n::text!("r5016e8e363"))]
        /// 压缩完的 tmux 会话名(启动会话时写进 settings)
        #[arg(long)]
        session: String,
    },
    #[command(about = summon_i18n::text!("r8cc4f8b93c"))]
    /// 查看入站消息(inbox):按群组聚合,显示排队中/处理中/已回复状态(控制台用)
    Inbox {
        #[arg(help = summon_i18n::text!("r0394b2665a"))]
        /// 输出机器可读 JSON
        #[arg(long)]
        json: bool,
    },
    #[command(about = summon_i18n::text!("r73aaa5b9c6"))]
    /// 查看出站消息(outbox):按群组聚合,显示待投递/已投递/失败状态(控制台用,始终 JSON)
    Outbox,
}

/// 两份 summon 比新旧的判据见 `summon_core::install_stamp`。
#[derive(Subcommand)]
pub enum InstallStampAction {
    #[command(about = summon_i18n::text!("r140ed1b3dd"))]
    /// 打印本二进制的构建信息(一行 JSON)
    Print,
    #[command(about = summon_i18n::text!("r028e337673"))]
    /// 比较本二进制与落点那份:打印 newer / same / older / unknown / absent 及说明,只读
    Check,
    #[command(about = summon_i18n::text!("r5da7edc93d"))]
    /// 登记落点的构建戳,描述落点里的 summon 自己(只有落点那份能跑)
    Record,
    #[cfg(npm_dist)]
    #[command(flatten)]
    NpmDist(crate::setup::npm_dist::StampAction),
}

#[derive(Subcommand)]
pub enum ClaudeQuotaAction {
    #[command(about = summon_i18n::text!("red8f9f816b"))]
    /// 安装 Claude statusLine 接入（检测到配置漂移会拒绝覆盖）
    Install,
    #[command(about = summon_i18n::text!("r5f2ef289d5"))]
    /// 查看 Claude 接入 ownership 状态
    Status,
    #[command(about = summon_i18n::text!("ra7c9fcfc0f"))]
    /// 移除 summon statusLine，并在仍拥有时恢复安装前命令
    Uninstall,
}

#[derive(Subcommand)]
pub enum BridgeAction {
    #[command(about = summon_i18n::text!("r30d9036a28"))]
    /// 结构化合并六类 Claude Hooks，保留已有配置
    InstallHooks,
    #[command(about = summon_i18n::text!("r2cb07df4a8"))]
    /// 检查六类 Claude Hooks 是否完整安装
    Status,
    #[command(about = summon_i18n::text!("r50a8fcd20e"))]
    /// 只移除 summon bridge 自己的 Hooks
    UninstallHooks,
    #[command(about = summon_i18n::text!("r53ab66acb3"))]
    /// Claude Hook 内部入口；不供人手工调用
    #[command(hide = true)]
    Hook {
        #[arg(help = summon_i18n::text!("re9501f5c66"))]
        /// Claude Hook 协议名(由 Claude 调用时给出,人不需要手填)
        #[arg(long)]
        protocol: String,
    },
}

#[derive(Subcommand)]
pub enum AutostartAction {
    #[command(about = summon_i18n::text!("r948889b5fb"))]
    /// 开启登录启动，与 Menu Bar 的 Launch at login 共用设置
    #[command(visible_alias = "on")]
    Enable {
        #[arg(help = summon_i18n::text!("r2f1113a7fc"))]
        /// 连正常退出也拉起(`daemon stop` 会被立刻复活,deploy 会拒绝部署)。开机自启本身已在
        /// 崩溃时拉起,一般不需要它
        #[arg(long, num_args = 0..=1, default_missing_value = "true", require_equals = true)]
        keep_alive: Option<bool>,
    },
    #[command(about = summon_i18n::text!("reaa85b8aa3"))]
    /// 关闭登录启动
    #[command(visible_alias = "off")]
    Disable,
    #[command(about = summon_i18n::text!("rbb43eb4064"))]
    /// 查看开机自启状态
    Status,
}

#[derive(Subcommand)]
pub enum DashboardAction {
    #[command(about = summon_i18n::text!("rb2e2420969"))]
    /// 向 daemon 要一个一次性本机链接(60 秒内有效、只能用一次)并在系统浏览器里打开
    Open {
        #[arg(help = summon_i18n::text!("r6f6f4718dc"))]
        /// 只打印链接,不打开浏览器
        #[arg(long)]
        print: bool,
    },
}

#[derive(Subcommand)]
pub enum MenubarAction {
    #[command(about = summon_i18n::text!("rf68b12ad17"))]
    /// 只读本地台账与价格表的费用估算快照(JSON)
    Usage,
    #[command(about = summon_i18n::text!("r5af696b0c3"))]
    /// 写入并加载 summon 自己的 LaunchAgent
    Install,
    #[command(about = summon_i18n::text!("ree39ee2994"))]
    /// 用当前已安装二进制重载 Menu Bar
    Restart,
    #[command(about = summon_i18n::text!("rd4683963c2"))]
    /// 停止 Menu Bar 并移除 LaunchAgent（保留二进制）
    Uninstall,
    #[command(about = summon_i18n::text!("r0eb8e3d5c2"))]
    /// 查看二进制、LaunchAgent 和运行状态
    Status,
    #[command(about = summon_i18n::text!("r9895c83074"))]
    /// 采集 Menu Bar 外部会话 section（Claude/Cursor/Codex CLI + Codex App）
    Sessions {
        #[arg(help = summon_i18n::text!("ra1346bb8ab"))]
        /// JSON 数组输出（Menu Bar 使用）
        #[arg(long)]
        json: bool,
    },
    #[command(about = summon_i18n::text!("r7e5fb9860e"))]
    /// Menu Bar 的「Handled」:只清菜单上看到的那条回复对应的 waiting
    Handled {
        #[arg(help = summon_i18n::text!("r3ea233f1b1"))]
        /// waiting 所属 agent 的 alias
        #[arg(long)]
        agent: String,
        #[arg(help = summon_i18n::text!("r57895b83f9"))]
        /// waiting 所在会话的 chat tag
        #[arg(long)]
        tag: String,
        #[arg(help = summon_i18n::text!("r2eced6e968"))]
        /// 菜单渲染时那条回复的 message_id;期间若有了新回复,不清
        #[arg(long)]
        reply: String,
        #[arg(help = summon_i18n::text!("rd065a1c58c"))]
        /// 结果输出成 JSON(Menu Bar 使用):{"outcome":"cleared|superseded|absent"}
        #[arg(long)]
        json: bool,
    },
    #[command(about = summon_i18n::text!("r885e358bd8"))]
    /// Menu Bar 的 Exit / Archive:经 daemon 结束一条会话,等到它从列表里消失再返回
    EndSession {
        #[arg(help = summon_i18n::text!("rb603a30387"))]
        /// 会话所属 agent 的 alias
        #[arg(long)]
        agent: String,
        #[arg(help = summon_i18n::text!("rb62af79db2"))]
        /// 会话的 chat tag
        #[arg(long)]
        tag: String,
        #[arg(help = summon_i18n::text!("r720fe29c69"))]
        /// 归档(下条消息新建会话,`/resume` 可恢复);不带 = 退出(下条消息接着聊)
        #[arg(long)]
        archive: bool,
        #[arg(help = summon_i18n::text!("re5388df4d3"))]
        /// 会话正在跑一轮时也执行(打断这一轮);不带时遇到在跑的轮不动手
        #[arg(long)]
        interrupt: bool,
        #[arg(help = summon_i18n::text!("rf5e53b8135"))]
        /// 结果输出成 JSON(Menu Bar 使用):{"outcome":"ended|running|pending|absent"}
        #[arg(long)]
        json: bool,
    },
    #[command(about = summon_i18n::text!("r781169a67e"))]
    /// Menu Bar 设置区的当前值(整机这一层,及覆盖了同一项的 agent)
    Settings {
        #[arg(help = summon_i18n::text!("rd3af5019b9"))]
        /// JSON 输出(Menu Bar 使用)
        #[arg(long)]
        json: bool,
    },
    #[command(about = summon_i18n::text!("r5a5c2aad5c"))]
    /// Menu Bar 设置区改一项。键:sentinel(off|dry-run|on)、show_counts / launch_at_login /
    /// cot_visible / hide_thinking / cot_tool_details / badge_effort(on|off)、
    /// idle_ttl_secs(正整数秒)、idle_retire_secs(秒,0 = 关)
    Set {
        #[arg(help = summon_i18n::text!("r40eb1c5d6a"))]
        /// 设置项,见上面的键表
        key: String,
        #[arg(help = summon_i18n::text!("r3a76d9043e"))]
        /// 新值:on / off、sentinel 的 off / dry-run / on,或秒数
        value: String,
    },
}

/// `summon schedules` 子命令。
#[derive(clap::Subcommand)]
pub enum SchedulesAction {
    #[command(about = summon_i18n::text!("r9769727f6a"))]
    /// 列出所有定时器
    Ls,
    #[command(about = summon_i18n::text!("r5ae934a928"))]
    /// 软禁用一个定时器(enabled=false)
    #[command(arg_required_else_help = true)]
    Rm {
        #[arg(help = summon_i18n::text!("r1aa154a8af"))]
        /// 定时器 id(形如 `SCH-12`,见 `summon schedules ls` 第一列)
        id: String,
    },
    #[command(about = summon_i18n::text!("r1001283fb0"))]
    /// 部分修改一个定时器；未提供的字段保持不变
    #[command(arg_required_else_help = true)]
    Edit {
        #[arg(help = summon_i18n::text!("r043bab35a9"))]
        /// 定时器 id（形如 `SCH-12`）
        id: String,
        #[arg(help = summon_i18n::text!("rea10840b92"))]
        /// 新节律，如 `daily 09:30`、`every 2h`、`cron 0 9 * * 1-5`
        #[arg(long)]
        when: Option<String>,
        #[arg(help = summon_i18n::text!("r7e755b8590"))]
        /// 新的任务正文
        #[arg(long)]
        prompt: Option<String>,
        #[arg(help = summon_i18n::text!("rbe43610894"))]
        /// 输出模式：progress（过程与结果）或 result（仅结果）
        #[arg(long)]
        output: Option<String>,
        #[arg(help = summon_i18n::text!("r20388e991b"))]
        /// 上下文模式：resume（续用会话）或 fresh（每次起跑前重置会话、从空上下文冷启）
        #[arg(long)]
        context: Option<String>,
        #[arg(help = summon_i18n::text!("ra5775367ca"))]
        /// 是否启用；可用它重新启用被 rm 归档的定时器
        #[arg(long, action = clap::ArgAction::Set)]
        enabled: Option<bool>,
    },
    #[command(about = summon_i18n::text!("r30404a6845"))]
    /// 把一个定时器交给另一个 agent 跑(群不变)
    #[command(arg_required_else_help = true)]
    Mv {
        #[arg(help = summon_i18n::text!("r1aa154a8af"))]
        /// 定时器 id(形如 `SCH-12`,见 `summon schedules ls` 第一列)
        id: String,
        #[arg(help = summon_i18n::text!("r8fefc50f79"))]
        /// 接手的 agent 别名;必须已在 registry 里,否则拒绝
        #[arg(long)]
        agent: String,
    },
}

/// `summon workflow` 子命令(P4 工作流)。
#[derive(clap::Subcommand)]
pub enum WorkflowAction {
    #[command(about = summon_i18n::text!("ree9fd47965"))]
    /// 列出所有工作流
    Ls,
    #[command(about = summon_i18n::text!("r7fbfe8d024"))]
    /// 校验并打印执行计划(拓扑序)
    #[command(arg_required_else_help = true)]
    Show {
        #[arg(help = summon_i18n::text!("re0753ca350"))]
        /// 工作流名(不含 .yaml;见 `summon workflow ls`)
        name: String,
    },
    #[command(about = summon_i18n::text!("r8aa9b9c6ca"))]
    /// 执行工作流(按拓扑序逐步起 worker run,`{step_id}` 注入上游产出)
    #[command(arg_required_else_help = true)]
    Run {
        #[arg(help = summon_i18n::text!("re0753ca350"))]
        /// 工作流名(不含 .yaml;见 `summon workflow ls`)
        name: String,
    },
}

/// `summon remember`:写一条事实到 global 层或某 agent 的记忆层(P2 记忆分层写回入口)。
#[derive(clap::Args, Debug)]
#[command(about = summon_i18n::text!("r5e12153c2f"))]
pub struct RememberArgs {
    #[arg(help = summon_i18n::text!("r5ef0126be1"))]
    /// 标题(生成文件名 slug + 索引行展示)。
    pub title: String,
    #[arg(help = summon_i18n::text!("rc57b02993c"))]
    /// 正文(可多词;省略=只留 frontmatter)。
    pub body: Vec<String>,
    #[arg(help = summon_i18n::text!("r08a0a38d28"))]
    /// 写全局层(`~/.summon/memory/global/`);与 `--agent` 二选一。
    #[arg(long)]
    pub global: bool,
    #[arg(help = summon_i18n::text!("r6e2ce57a1b"))]
    /// 写某 agent 的记忆层(默认取 env `SUMMON_AGENT`)。
    #[arg(long)]
    pub agent: Option<String>,
    #[arg(help = summon_i18n::text!("r1a04050268"))]
    /// 一行摘要(召回索引用;省略=用标题)。
    #[arg(long)]
    pub description: Option<String>,
    #[arg(help = summon_i18n::text!("r602b301231"))]
    /// 类型:user|feedback|project|reference(默认 reference)。
    #[arg(long = "type", default_value = "reference")]
    pub fact_type: String,
    #[arg(help = summon_i18n::text!("r0a9dce0218"))]
    /// 索引行钩子短语(省略=用 description)。
    #[arg(long)]
    pub hook: Option<String>,
    #[arg(help = summon_i18n::text!("r3f1ec305da"))]
    /// 本轮 turn 凭证。给了就把写下这条的 agent/模型/turn 记进 frontmatter 的 `metadata`;
    /// 凭证经 daemon 复验,已结束或对不上的一律拒绝。
    #[arg(long)]
    pub turn_id: Option<String>,
}

pub fn run(cli: Cli) -> anyhow::Result<()> {
    if let Cmd::ProgramStatusRun {
        path,
        generation,
        command,
    } = &cli.cmd
    {
        crate::capture::program_status::adopt_session_process_name();
        let code = crate::capture::program_status::run(path, generation, command)?;
        std::process::exit(code);
    }
    // Hook 是 Claude 热路径：必须在 Config::load 前短路，且任何 bridge 故障都不能阻塞 Claude。
    if let Cmd::Bridge {
        action: BridgeAction::Hook { protocol },
    } = &cli.cmd
    {
        crate::bridge::hook::run_safely(protocol);
        return Ok(());
    }
    // statusLine 是透明 wrapper：它不依赖 summon config，且 config 损坏告警也不应污染用户原
    // statusLine 的 stderr。故在统一 Config::load 之前走隐藏快路径。
    // **契约**：该分支不得调用 `tools::resolve` 或 spawn 外部工具；resolver 快照尚未 configure。
    // 若未来确需外部工具，必须先把 Config::load + tools::configure 提到这个 return 之前，并重新
    // 评估 statusLine 热路径的磁盘 IO 与 stderr 透明性，不能安静退回裸名 PATH。
    if matches!(
        &cli.cmd,
        Cmd::Daemon {
            action: DaemonAction::ClaudeQuotaStatusline
        }
    ) {
        return claude_quota_statusline();
    }
    // 构建戳与配置无关,而 `print` 的 stdout 要被打包脚本当 JSON 读:配置告警不能混进来。
    if let Cmd::InstallStamp { action } = &cli.cmd {
        return install_stamp_cmd(action);
    }
    if let Cmd::CommentReader { socket } = &cli.cmd {
        return crate::runtime::document_comment_reader::proxy(socket);
    }
    crate::runtime::localization::configure_system();
    let cfg = Config::load();
    // 外部工具路径按进程启动快照；所有 spawn 热路径只读内存，不各自重载 config。
    summon_core::tools::configure(&cfg);
    crate::runtime::localization::configure(&cfg);
    match cli.cmd {
        Cmd::Comments { action } => {
            let reg = Registry::load(&cfg.agents_dir)?;
            match action {
                CommentsAction::List => {
                    let runtime = crate::observe::fleet_snapshot::fetch_runtime_status();
                    let view = crate::observe::fleet_snapshot::build(
                        &cfg,
                        &reg,
                        runtime.as_ref(),
                        std::time::SystemTime::now(),
                    );
                    anyhow::ensure!(
                        view.comment_tasks_error.is_none(),
                        summon_i18n::format!("r27013bb374")
                    );
                    println!("{}", serde_json::to_string_pretty(&view.comment_tasks)?);
                    Ok(())
                }
                CommentsAction::Stop { id, agent } => {
                    let loaded = reg
                        .agents
                        .iter()
                        .find(|a| a.name == agent)
                        .context(summon_i18n::text!("r291c089be2"))?;
                    crate::runtime::document_comments::Store::at_run_dir()
                        .stop_draft(&id, loaded)?;
                    println!("{}", summon_i18n::format!("r2f57ba776f"));
                    Ok(())
                }
            }
        }
        Cmd::CommentReader { .. } => unreachable!("comment reader handled before config"),
        Cmd::RemoteTools { action } => crate::remote_tools::run(&cfg, action),
        Cmd::ProgramStatusRun { .. } => unreachable!("program status handled before config"),
        Cmd::InstallStamp { .. } => unreachable!("install stamp handled before config"),
        Cmd::Agents { action } => agents_cmd(&cfg, action),
        Cmd::Ask { turn_id, questions } => ask_cmd(&cfg, &turn_id, &questions),
        Cmd::Delegate {
            turn_id,
            to,
            new_chat,
            summary,
            content,
            dry_run,
        } => {
            let target = match (to, new_chat) {
                (Some(name), None) => {
                    summon_core::session::group_target::GroupTarget::Existing(name)
                }
                (None, Some(name)) => summon_core::session::group_target::GroupTarget::New { name },
                _ => anyhow::bail!(summon_i18n::format!("r252ebbe195")),
            };
            delegate_cmd(
                &cfg,
                &turn_id,
                target,
                &content,
                summary.as_deref(),
                dry_run,
            )
        }
        Cmd::Remind {
            turn_id,
            when,
            content,
            output,
            context,
        } => remind_cmd(&cfg, &turn_id, &when, &content, &output, &context),
        Cmd::React {
            turn_id,
            emoji,
            clear,
        } => react_agent_cmd(&cfg, &turn_id, emoji.as_deref(), clear),
        Cmd::SendImage {
            turn_id,
            path,
            caption,
        } => send_image_cmd(&cfg, &turn_id, &path, caption.as_deref()),
        Cmd::Preview { turn_id, path } => preview_cmd(&cfg, &turn_id, &path),
        Cmd::Session { action, json } => session_cmd(&cfg, action, json),
        Cmd::ChatId { turn_id } => {
            let (_, context) = resolve_turn_target(&cfg, &turn_id)?;
            println!("{}", context.chat_id);
            Ok(())
        }
        Cmd::Chats {
            turn_id,
            agent,
            action,
        } => chats_cmd(&cfg, turn_id.as_deref(), agent.as_deref(), action),
        Cmd::Bind(args) => crate::setup::bind::run(&cfg, args),
        Cmd::Onboard(args) => crate::setup::onboard::run(&cfg, args),
        Cmd::Doctor { set_tmux } => match set_tmux {
            Some(pick) => doctor_set_tmux(&pick),
            None => doctor(&cfg),
        },
        Cmd::Guide { section } => {
            if crate::cli::guide::print(section.as_deref()) {
                Ok(())
            } else {
                // 未知章节:exit 非 0 + 列可用章节,让 agent 知道拼错了而不是把报错当答案。
                eprintln!(
                    "{}",
                    summon_i18n::format!(
                        "r5f691d2f1f",
                        section.unwrap_or_default(),
                        crate::cli::guide::available_sections().join(" | ")
                    )
                );
                std::process::exit(2);
            }
        }
        Cmd::Menubar { action } => menubar_cmd(&cfg, action),
        Cmd::Usage(args) => crate::cli::usage::run(&cfg.agents_dir, args),
        Cmd::Insights(args) => crate::cli::insights::run(args),
        Cmd::Dashboard {
            action: DashboardAction::Open { print },
        } => dashboard_open(
            print,
            crate::runtime::daemon::submit_command,
            open_in_browser,
            &mut std::io::stdout(),
        ),
        Cmd::Sentinel { action } => sentinel_cmd(&cfg, action),
        Cmd::Daemon { action } => daemon_cmd(&cfg, action),
        #[cfg(npm_dist)]
        Cmd::NpmDist(command) => crate::setup::npm_dist::run(command),
        Cmd::Bridge { action } => bridge_cmd(action),
        Cmd::Autostart { action } => autostart_cmd(action),
        Cmd::Schedules { action } => schedules_cmd(&cfg, action),
        Cmd::Remember(args) => remember_cmd(&cfg, args),
        Cmd::Workflow { action } => workflow_cmd(&cfg, action),
        #[cfg(debug_assertions)]
        Cmd::Dev { action } => crate::cli::dev::run(&cfg, action),
    }
}

fn install_stamp_cmd(action: &InstallStampAction) -> anyhow::Result<()> {
    use crate::setup::self_install;
    match action {
        InstallStampAction::Print => {
            println!("{}", serde_json::to_string(&crate::version::build_info())?);
        }
        InstallStampAction::Check => println!("{}", self_install::check_current_exe()?),
        InstallStampAction::Record => {
            let target = self_install::record_current_exe()?;
            println!("{}", summon_i18n::format!("r477d22d4ad", target.display()));
        }
        #[cfg(npm_dist)]
        InstallStampAction::NpmDist(action) => crate::setup::npm_dist::stamp_cmd(action)?,
    }
    Ok(())
}

fn bridge_cmd(action: BridgeAction) -> anyhow::Result<()> {
    match action {
        BridgeAction::InstallHooks => {
            let path = crate::bridge::install::install()?;
            println!("{}", summon_i18n::format!("rae8953ac85", path.display()));
        }
        BridgeAction::Status => {
            let (path, status) = crate::bridge::install::status()?;
            let label = match status {
                crate::bridge::install::InstallStatus::Absent => {
                    summon_i18n::text!("r156219e305").to_string()
                }
                crate::bridge::install::InstallStatus::Partial { installed } => {
                    summon_i18n::format!("rfd4f4c5cef")
                }
                crate::bridge::install::InstallStatus::Installed => {
                    summon_i18n::text!("r5b597e1c22").to_string()
                }
                crate::bridge::install::InstallStatus::Stale {
                    configured,
                    current,
                } => summon_i18n::format!("r1c6dcc7019"),
                crate::bridge::install::InstallStatus::Disabled => {
                    summon_i18n::text!("rf6d278902e").to_string()
                }
                crate::bridge::install::InstallStatus::Corrupt => {
                    summon_i18n::text!("r8b4b3d8734").to_string()
                }
                crate::bridge::install::InstallStatus::Unavailable => {
                    summon_i18n::text!("r13ccee4ac8").to_string()
                }
            };
            println!("external bridge Hooks:{label} {}", path.display());
            print_bridge_runtime_health();
        }
        BridgeAction::UninstallHooks => {
            let path = crate::bridge::install::uninstall()?;
            println!("{}", summon_i18n::format!("r71fda77c30", path.display()));
        }
        BridgeAction::Hook { protocol } => crate::bridge::hook::run_safely(&protocol),
    }
    Ok(())
}

fn print_bridge_runtime_health() {
    use crate::bridge::store::BridgeStateHealth;

    match crate::bridge::store::BridgeStore::runtime_default().inspect_health() {
        Ok(health) => {
            let state = match health.state {
                BridgeStateHealth::Absent => summon_i18n::text!("r12876e31ef"),
                BridgeStateHealth::Healthy => summon_i18n::text!("raa18b6dcd8"),
                BridgeStateHealth::Corrupt => summon_i18n::text!("r8261880ab5"),
            };
            println!(
                "Bridge state    : {state} sessions={}/active={} conflicts={} mirror={} activating={} handoff_sensitive={} takeovers={} closing={} pending_turns={} receipts={}",
                health.sessions,
                health.active_sessions,
                health.conflicts,
                health.mirror_bindings,
                health.activating_bindings,
                health.handoff_sensitive_bindings,
                health.managed_takeovers,
                health.closing_bindings,
                health.pending_turns,
                health.delivery_receipts
            );
            println!(
                "Hook inbox      : pending={} dead={}",
                health.hook_pending, health.hook_dead
            );
        }
        Err(_) => println!("{}", summon_i18n::format!("r9e9a6398c2")),
    }
    match crate::runtime::card_action_wal::CardActionWal::at_run_dir().inspect_health() {
        Ok(health) => println!(
            "callback inbox  : pending={} consumed={} corrupt={}",
            health.pending, health.consumed, health.corrupt
        ),
        Err(_) => println!("{}", summon_i18n::format!("rae01a4b500")),
    }
    match crate::runtime::outbox_store::OutboxStore::at_run_dir().inspect_bridge_health() {
        Ok(health) => {
            println!(
                "Bridge outbox   : pending={} corrupt={} recovery_pending={} recovery_invalid={} other_corrupt={} quarantined={} dead={}",
                health.pending,
                health.corrupt,
                health.recovery_pending,
                health.recovery_invalid,
                health.other_corrupt,
                health.quarantined,
                health.dead
            );
            if health.corrupt > 0 || health.recovery_invalid > 0 || health.other_corrupt > 0 {
                println!("{}", summon_i18n::format!("rcbd77b0078"));
            }
        }
        Err(_) => println!("{}", summon_i18n::format!("raa8ec5bbff")),
    }
    match crate::runtime::daemon::default_sidecar_bin() {
        Some(path) => match crate::runtime::daemon::sidecar_protocol_version(&path) {
            Ok(version) if version == crate::runtime::daemon::SIDECAR_PROTOCOL_VERSION => {
                println!("sidecar IPC     : v{version} compatible")
            }
            Ok(version) => println!(
                "{}",
                summon_i18n::format!(
                    "r590ab7a3e2",
                    crate::runtime::daemon::SIDECAR_PROTOCOL_VERSION
                )
            ),
            Err(_) => println!("{}", summon_i18n::format!("rba999020b7")),
        },
        None => println!("{}", summon_i18n::format!("r9b58f57162")),
    }
    let cfg = Config::load();
    if let Ok((registry, warnings)) = Registry::load_tolerant(&cfg.agents_dir) {
        // 这是 doctor 面板:坏清单意味着下面的 orphan 统计漏了那些 agent,静默吞掉正好骗过排障的人。
        for warning in &warnings {
            println!("{}", summon_i18n::format!("r94be62fa2e"));
        }
        let orphans = registry
            .agents
            .iter()
            .flat_map(|agent| {
                crate::session::resume_store::orphan_records(agent)
                    .into_iter()
                    .map(move |session| (agent.name.clone(), session))
            })
            .collect::<Vec<_>>();
        println!("Orphan resumes  : {}", orphans.len());
        for (agent, session) in orphans {
            println!("{}", summon_i18n::format!("rd0b7a69a2a"));
        }
    }
    println!("{}", summon_i18n::format!("r41f15ac8cd"));
}

/// `summon react [<emoji>] [--clear]`:agent 在自己 chat 会话内给「当前正回应的那条飞书消息」贴/撤
/// 语义表情。身份与目标 chat 经 `resolve_turn_target` 从 `--turn-id` 取(**不认环境变量**:
/// 一组真实但陈旧的 env 本身自洽,会以别的 bot 身份贴到别的群去)。从 ReactionStore 找当前
/// working 消息,用本 agent 自己的飞书凭据贴。可改可撤、单值(再贴即替换、`--clear` 撤掉)。
/// `summon remind`:agent 在自己的 chat 里建定时器。
///
/// **为什么要有这条命令**:在此之前 agent 只能手改 `~/.summon/schedules.yaml`。那条路绕开了
/// id 分配(`next_id` 的 flock)、节律校验和身份归属 —— 写错一个字段整份表都读不出来,
/// 而 `load` 对损坏文件是向上报错的,于是**所有** agent 的定时器一起失效。
///
/// 归属**只认 turn 凭证**:agent 与 chat 都从 `resolve_turn_target` 取,不接受参数指定。
/// 允许指定就等于给了「往别的群埋一个定时器」的能力,那是这个仓明确避开的形状。
fn remind_cmd(
    cfg: &Config,
    turn_id: &str,
    when: &str,
    content: &str,
    output: &str,
    context: &str,
) -> anyhow::Result<()> {
    use anyhow::{bail, Context as _};

    let (agent, turn) = resolve_turn_target(cfg, turn_id)?;
    ensure_turn_can_remind(
        &turn,
        crate::runtime::self_followup::unanswered(&agent.state_dir(), &turn.conversation_tag),
    )?;

    // 内容先取到手再解析节律:`-` 走 stdin 是给长文本 / 多行留的口子,避开 argv 长度与引号转义。
    let content = if content == "-" {
        let mut buf = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf)
            .context(summon_i18n::text!("r19068a8b79"))?;
        buf
    } else {
        content.to_string()
    };
    let content = content.trim();
    if content.is_empty() {
        bail!(summon_i18n::format!("rd79b01564f"));
    }

    let now = chrono::Local::now();
    let tokens: Vec<&str> = when.split_whitespace().collect();
    let parsed = summon_core::schedule::parse_when(&tokens, now).map_err(anyhow::Error::msg)?;
    let output = output
        .parse::<summon_core::ScheduleOutput>()
        .map_err(anyhow::Error::msg)?;
    let context = context
        .parse::<summon_core::ScheduleContext>()
        .map_err(anyhow::Error::msg)?;
    // 整个 `--when` 必须都是节律。多出来的 token 几乎总是「把内容也写进了 --when」,
    // 而那会静默丢内容 —— 宁可在这里拒。
    if parsed.consumed != tokens.len() {
        bail!(summon_i18n::format!(
            "rb0b2086323",
            &tokens[parsed.consumed..]
        ));
    }

    let store = crate::schedule::schedule_store::ScheduleStore::at_summon_home();
    let created_by =
        if turn.origin == crate::runtime::current_turn_store::CurrentTurnOrigin::Schedule {
            schedule_creator_of_turn(&store, &turn.message_id).unwrap_or_default()
        } else {
            turn.requester_open_id.clone()
        };
    let id = store.next_id().context(summon_i18n::text!("rf2035eab12"))?;
    let schedule = summon_core::Schedule {
        id: id.clone(),
        kind: parsed.kind,
        spec: parsed.spec.clone(),
        agent: agent.name.clone(),
        chat_id: turn.chat_id.clone(),
        chat_type: turn.chat_type.clone(),
        prompt: content.to_string(),
        created_by,
        created_at: now.to_rfc3339(),
        output,
        context,
        enabled: true,
        last_fired: None,
        revision: 0,
    };
    store
        .add(&schedule)
        .context(summon_i18n::text!("r38d9212b02"))?;

    // 节律用**共享**的 `human_readable`:列表卡 / 编辑回执 / `schedules ls` 都走它。
    // 各写各的话,同一条定时器在不同地方会显示成不同的东西,而那正是用户判断「排对了没」的依据。
    let rhythm = summon_core::schedule::human_readable(&schedule);
    match next_fire_hint(&schedule, now) {
        Some(next) => println!(
            "{}",
            summon_i18n::format!(
                "r003dec4a56",
                schedule_mode_label(&schedule),
                next.format("%Y-%m-%d %H:%M")
            )
        ),
        None => println!(
            "{}",
            summon_i18n::format!("rc709d3ad68", schedule_mode_label(&schedule))
        ),
    }
    Ok(())
}

/// 列表与回执里的模式列:输出模式,起跑前重置会话的再带上「新上下文」。缺省续用不显示,
/// 免得列表多一列噪音。
fn schedule_mode_label(s: &summon_core::Schedule) -> String {
    match s.context {
        summon_core::ScheduleContext::Fresh => {
            format!("{}·{}", s.output.label(), s.context.label())
        }
        summon_core::ScheduleContext::Resume => s.output.label().to_string(),
    }
}

/// 「下一次几点」。**不能用 `due_plan().next_fire`** —— 那是 scheduler 这一 tick 的决策,
/// 对还没到期的一次性定时器返回 None(意思是「本 tick 无事可做」,不是「没有下一次」)。
/// 回执要答的是另一个问题,所以在这里独立算。
///
/// 这个数字是用户唯一能当场核对「我排对了没」的东西:节律表达式本身读不出下一次是几点,
/// 而排错时间只会在第一次没响的时候才被发现。
fn next_fire_hint(
    schedule: &summon_core::Schedule,
    now: chrono::DateTime<chrono::Local>,
) -> Option<chrono::DateTime<chrono::Local>> {
    use summon_core::schedule::{cron, Kind};
    match schedule.kind {
        Kind::Once => chrono::DateTime::parse_from_rfc3339(&schedule.spec)
            .ok()
            .map(|t| t.with_timezone(&chrono::Local)),
        Kind::Interval => schedule
            .spec
            .parse::<i64>()
            .ok()
            .filter(|secs| *secs > 0)
            .and_then(chrono::Duration::try_seconds)
            .and_then(|step| now.checked_add_signed(step)),
        Kind::Cron => cron::parse_cron(&schedule.spec)
            .ok()
            .map(|e| cron::next_after(&e, now)),
    }
}

fn react_agent_cmd(
    cfg: &Config,
    turn_id: &str,
    emoji: Option<&str>,
    clear: bool,
) -> anyhow::Result<()> {
    let (agent, turn) = resolve_turn_target(cfg, turn_id)?;
    let (id, sec) = crate::runtime::daemon::resolve_creds(&agent);
    let dry = id.is_none() || sec.is_none();
    let fc = crate::frontend::client::FeishuClient::new(
        &cfg.default_domain,
        id.unwrap_or_default(),
        sec.unwrap_or_default(),
        dry,
    );
    let target = crate::runtime::chat_tools::ChatToolTarget::new(
        &agent,
        &turn.chat_id,
        &turn.chat_type,
        &turn.message_id,
    );
    println!(
        "{}",
        crate::runtime::chat_tools::react(&target, &fc, emoji, clear)?
    );
    Ok(())
}

/// `summon session end`:登记「本轮完成后退出」。真正的归档由 chat 线程在本轮销账后执行 ——
/// CLI 是轮内的子进程,此刻轮还没结束,不能自己拆会话。
fn session_end_cmd(cfg: &Config, turn_id: &str) -> anyhow::Result<()> {
    let (agent, turn) = resolve_turn_target(cfg, turn_id)?;
    crate::runtime::session_end::request(
        &agent.state_dir(),
        &turn.conversation_tag,
        &turn.message_id,
    )?;
    println!("{}", summon_i18n::format!("red69345000"));
    Ok(())
}

/// `summon send-image <path> [--caption 文字]`:agent 在自己 chat 会话内把一张本地图片发进该 chat。
///
/// 与 `summon react` 同款的身份认领(SUMMON_AGENT/CHAT_ID env + 本 agent 自己的飞书凭据),因此
/// **claude(tmux)与 cursor/codex(无头子进程)共用这一条路径**——两种形态都注入了这两个 env。
///
/// 图片走独立消息,不嵌进回复卡片(见 spec 取舍)。`--caption` 会在图片之后另发一条文字。
fn send_image_cmd(
    cfg: &Config,
    turn_id: &str,
    path: &str,
    caption: Option<&str>,
) -> anyhow::Result<()> {
    let (agent, turn) = resolve_turn_target(cfg, turn_id)?;
    ensure_turn_allows_message_output(&turn)?;
    let (id, sec) = crate::runtime::daemon::resolve_creds(&agent);
    let dry = id.is_none() || sec.is_none();
    let fc = crate::frontend::client::FeishuClient::new(
        &cfg.default_domain,
        id.unwrap_or_default(),
        sec.unwrap_or_default(),
        dry,
    )
    .with_email_mask(crate::runtime::daemon::email_mask_switch(&agent));
    let target = crate::runtime::chat_tools::ChatToolTarget::new(
        &agent,
        &turn.chat_id,
        &turn.chat_type,
        &turn.message_id,
    );
    println!(
        "{}",
        crate::runtime::chat_tools::send_image(&target, &fc, path, caption)?
    );
    Ok(())
}

/// `summon preview <path>`：让 daemon 为本地文件生成浏览器预览能力 URL。
///
/// CLI 不直接写 token 表：daemon 内存里的预览服务是唯一 mint 所有者。请求同时携带本轮身份，
/// 轮已结束、服务未启用或路径不合法都会 fail-closed，不产生飞书消息。
fn preview_cmd(cfg: &Config, turn_id: &str, path: &str) -> anyhow::Result<()> {
    use summon_core::core_port::{Actor, ChatAgentIdentity, Command, CommandOutcome, Intent};
    let turn = load_current_turn_context(cfg, turn_id)?;
    let file = std::fs::canonicalize(path).with_context(|| summon_i18n::format!("r26306e03d9"))?;
    anyhow::ensure!(file.is_file(), summon_i18n::format!("r3418abbf80"));
    let path = file
        .to_str()
        .context(summon_i18n::text!("re11497b388"))?
        .to_string();
    let command = Command {
        actor: Actor::ChatAgent(Box::new(ChatAgentIdentity {
            turn_id: turn_id.to_string(),
            alias: turn.agent_alias.clone(),
            requester_open_id: turn.requester_open_id.clone(),
            source_ref: summon_core::MsgRef {
                delivery_scope: turn.delivery_scope.clone(),
                chat_id: turn.chat_id.clone(),
                chat_type: turn.chat_type.clone(),
                message_id: turn.message_id.clone(),
                thread_id: turn.thread_id.clone(),
                root_id: turn.root_id.clone(),
            },
            chat_type: turn.chat_type.clone(),
            conversation_tag: turn.conversation_tag.clone(),
        })),
        intent: Intent::CreateFilePreview {
            alias: turn.agent_alias,
            conversation_tag: turn.conversation_tag,
            turn_id: turn_id.to_string(),
            path,
        },
    };
    match crate::runtime::daemon::submit_command(&command) {
        Ok(CommandOutcome::Value(serde_json::Value::String(path))) => {
            println!(
                "{}",
                crate::runtime::preview_server::preview_reply_hint(&path)
            );
            Ok(())
        }
        Ok(CommandOutcome::Denied(reason)) => anyhow::bail!(summon_i18n::format!("r33a8715da8")),
        Ok(other) => anyhow::bail!(summon_i18n::format!("r4ce221d36c")),
        Err(error) => anyhow::bail!(summon_i18n::format!("rf478be70b2")),
    }
}

/// `summon dashboard open`:经 control.sock 以本机 owner 要一个一次性本机码链接。`print` 时只把
/// 链接打到 stdout(Menu Bar 拿它自己去开),否则交给系统浏览器,一次性码不出现在终端里。
fn dashboard_open(
    print: bool,
    submit: impl FnOnce(
        &summon_core::core_port::Command,
    ) -> anyhow::Result<summon_core::core_port::CommandOutcome>,
    open: impl FnOnce(&str) -> anyhow::Result<()>,
    out: &mut dyn std::io::Write,
) -> anyhow::Result<()> {
    use summon_core::core_port::{Actor, Command, CommandOutcome, Intent};
    let command = Command {
        actor: Actor::Owner,
        intent: Intent::DashboardOpen,
    };
    let url = match submit(&command) {
        Ok(CommandOutcome::Value(serde_json::Value::String(url))) => url,
        Ok(CommandOutcome::Denied(reason)) => anyhow::bail!(summon_i18n::format!("r0b0252b0e1")),
        Ok(other) => anyhow::bail!(summon_i18n::format!("rde3eaea342")),
        Err(error) => return Err(error),
    };
    // 链接来自本机 daemon,仍只把 http 链接交给系统打开:`open` 认得的协议远不止浏览器。
    anyhow::ensure!(
        url.starts_with("http://"),
        summon_i18n::format!("rbcb0d95462")
    );
    if print {
        writeln!(out, "{url}")?;
        return Ok(());
    }
    open(&url)?;
    writeln!(out, "{}", summon_i18n::format!("r22f709555b"))?;
    Ok(())
}

/// 交给系统默认浏览器:macOS `open`,Linux `xdg-open`。
fn open_in_browser(url: &str) -> anyhow::Result<()> {
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let status = std::process::Command::new(opener)
        .arg(url)
        .status()
        .with_context(|| summon_i18n::format!("rb372fe8be5"))?;
    anyhow::ensure!(status.success(), summon_i18n::format!("r3506f8db5b"));
    Ok(())
}

/// `summon workflow`(P4):列出 / 看计划 / 执行工作流。执行复用 P1 worker 临时 run(`run_workflow`)。
fn workflow_cmd(cfg: &Config, action: WorkflowAction) -> anyhow::Result<()> {
    let st = crate::workflow::WorkflowStore::at_summon_home();
    match action {
        WorkflowAction::Ls => {
            let names = st.list_names();
            if names.is_empty() {
                println!("{}", summon_i18n::format!("r00c7799e65"));
            }
            for n in names {
                println!("{n}");
            }
            Ok(())
        }
        WorkflowAction::Show { name } => {
            let wf = st.load(&name)?;
            let order = wf
                .topo_order()
                .map_err(|e| anyhow::anyhow!(summon_i18n::format!("rb86330d7d2")))?;
            println!("{}", summon_i18n::format!("r5c160b9acc", wf.name));
            for (i, id) in order.iter().enumerate() {
                let step = wf
                    .steps
                    .iter()
                    .find(|s| &s.id == id)
                    .expect(summon_i18n::text!("r2e06ff0522"));
                println!(
                    "  {}. {id} [{}] needs=[{}]",
                    i + 1,
                    step.agent,
                    step.needs.join(",")
                );
            }
            Ok(())
        }
        WorkflowAction::Run { name } => {
            let wf = st.load(&name)?;
            let reg = load_reg(cfg)?;
            println!("{}", summon_i18n::format!("ra023488523", wf.name));
            let results = crate::workflow::run_workflow(&wf, &reg, cfg.poll_ms, cfg.debounce_ms)?;
            for (id, result) in &results {
                println!("── step {id} ──\n{result}\n");
            }
            println!(
                "{}",
                summon_i18n::format!("ra69a21feda", wf.name, results.len())
            );
            Ok(())
        }
    }
}

/// `summon remember`:把一条事实写进 global 或某 agent 的记忆层(纯件渲染 + IO 在 `crate::memory`)。
/// 目标层:`--global` 或 `--agent <alias>`(回退 env `SUMMON_AGENT`);二者皆无 → 报错。
fn remember_cmd(cfg: &Config, args: RememberArgs) -> anyhow::Result<()> {
    let body = args.body.join(" ");
    let description = args.description.unwrap_or_else(|| args.title.clone());
    let hook = args.hook.unwrap_or_else(|| description.clone());
    let root = if args.global {
        crate::memory::global_root()
    } else {
        let alias = args
            .agent
            .or_else(|| std::env::var("SUMMON_AGENT").ok())
            .context(summon_i18n::text!("r07e3bb7891"))?;
        let reg = load_reg(cfg)?;
        let agent = reg
            .agents
            .iter()
            .find(|a| a.name == alias)
            .with_context(|| summon_i18n::format!("r85bd3cac78"))?;
        crate::memory::agent_root(agent)
    };
    let provenance = match args.turn_id.as_deref() {
        Some(turn_id) => {
            let (author, context) = resolve_turn_target(cfg, turn_id)?;
            summon_core::event_log::EventStamp {
                agent: Some(context.agent_alias.clone()),
                model: crate::runtime::daemon::recorded_model(&author, &context.conversation_tag),
                turn: Some(context.turn_id.clone()),
            }
        }
        None => summon_core::event_log::EventStamp::default(),
    };
    let slug = crate::memory::write_fact(
        &root,
        &args.title,
        &description,
        &args.fact_type,
        &body,
        &hook,
        &provenance,
    )?;
    println!(
        "{}",
        summon_i18n::format!("r711dbfd7d1", args.fact_type, root.display())
    );
    Ok(())
}

fn schedules_cmd(cfg: &Config, action: Option<SchedulesAction>) -> anyhow::Result<()> {
    let st = crate::schedule::schedule_store::ScheduleStore::at_summon_home();
    match action.unwrap_or(SchedulesAction::Ls) {
        SchedulesAction::Ls => {
            let all = st.load()?;
            if all.is_empty() {
                println!("{}", summon_i18n::format!("r6e624bb191"));
            }
            for s in all {
                // 节律走与卡片同一个人话渲染器:同一条定时器在终端和飞书里必须长一个样。
                // 飞书列表卡只列在跑的、把归档折成一行计数,所以**全量在这里看**。
                let state = if s.enabled {
                    summon_i18n::text!("rafaf49b3a4")
                } else {
                    summon_i18n::text!("r82395d4230")
                };
                println!(
                    "{}  {}  {} @ {}  {}  {}  {}",
                    s.id,
                    summon_core::human_readable(&s),
                    s.agent,
                    s.chat_id,
                    state,
                    schedule_mode_label(&s),
                    s.prompt
                );
            }
            Ok(())
        }
        SchedulesAction::Rm { id } => {
            st.set_enabled(&id, false)?;
            println!("{}", summon_i18n::format!("rb890f10e71"));
            Ok(())
        }
        SchedulesAction::Edit {
            id,
            when,
            prompt,
            output,
            context,
            enabled,
        } => {
            anyhow::ensure!(
                when.is_some()
                    || prompt.is_some()
                    || output.is_some()
                    || context.is_some()
                    || enabled.is_some(),
                summon_i18n::format!("r08957d4dd8")
            );
            let now = chrono::Local::now();
            let rhythm = when
                .map(|when| {
                    let tokens: Vec<&str> = when.split_whitespace().collect();
                    let parsed = summon_core::parse_when(&tokens, now)
                        .map_err(|error| anyhow::anyhow!(error))?;
                    anyhow::ensure!(
                        parsed.consumed == tokens.len(),
                        summon_i18n::format!("re1013512c9")
                    );
                    Ok((parsed.kind, parsed.spec))
                })
                .transpose()?;
            let prompt = prompt
                .map(|prompt| {
                    let prompt = prompt.trim().to_string();
                    anyhow::ensure!(!prompt.is_empty(), summon_i18n::format!("rdd974f172c"));
                    Ok(prompt)
                })
                .transpose()?;
            let output = output
                .map(|value| {
                    value
                        .parse::<summon_core::ScheduleOutput>()
                        .map_err(anyhow::Error::msg)
                })
                .transpose()?;
            let context = context
                .map(|value| {
                    value
                        .parse::<summon_core::ScheduleContext>()
                        .map_err(anyhow::Error::msg)
                })
                .transpose()?;
            let (updated, rescheduled) = st.edit(
                &id,
                crate::schedule::schedule_store::SchedulePatch {
                    rhythm,
                    prompt,
                    output,
                    context,
                    enabled,
                },
                &now.to_rfc3339(),
            )?;
            println!(
                "{}",
                summon_i18n::format!(
                    "r11eecbb9e6",
                    updated.id,
                    if rescheduled {
                        summon_i18n::text!("rf2fe9a5eef")
                    } else {
                        ""
                    },
                    summon_core::human_readable(&updated),
                    schedule_mode_label(&updated),
                    updated.prompt
                )
            );
            Ok(())
        }
        SchedulesAction::Mv { id, agent } => {
            // 目标 agent 不在 registry 时**必须拒**:scheduler 查不到它的 delivery scope 会
            // 静默跳过这个 tick(不报错、不重试),定时器就成了不响也不报的哑弹。
            // 这层校验是本命令相对手改 schedules.yaml 的核心价值。
            ensure_move_target_is_known(&load_reg(cfg)?, &agent)?;
            let before = st
                .load()?
                .into_iter()
                .find(|s| s.id == id)
                .with_context(|| summon_i18n::format!("rca76a985fd"))?;
            if before.agent == agent {
                println!("{}", summon_i18n::format!("r85775628ec"));
                return Ok(());
            }
            st.set_agent(&id, &agent)?;
            println!(
                "{}",
                summon_i18n::format!(
                    "r05a77a4a1a",
                    before.agent,
                    before.chat_id,
                    if before.enabled {
                        summon_i18n::text!("r828e7ce18c")
                    } else {
                        summon_i18n::text!("r4d1cc2544d")
                    }
                )
            );
            Ok(())
        }
    }
}

fn daemon_cmd(cfg: &Config, action: DaemonAction) -> anyhow::Result<()> {
    match action {
        DaemonAction::Run => crate::runtime::daemon::run(),
        DaemonAction::Start => daemon_start(),
        DaemonAction::Stop => daemon_stop(),
        DaemonAction::Restart => daemon_restart_cmd(),
        DaemonAction::Status => daemon_status(),
        DaemonAction::ClaudeQuota { action } => claude_quota_cmd(action),
        DaemonAction::ClaudeQuotaStatusline => claude_quota_statusline(),
        DaemonAction::ToolGuardHook => {
            crate::runtime::tool_guard::run_tool_guard_hook(&cfg.agents_dir);
            Ok(())
        }
        DaemonAction::PermissionHook { agent, session } => {
            crate::runtime::claude_hook::run_permission_hook(&agent, &session);
            Ok(())
        }
        DaemonAction::CompactHook { agent, session } => {
            crate::runtime::claude_hook::run_compact_hook(&agent, &session);
            Ok(())
        }
        DaemonAction::Inbox { json } => inbox_cmd(json),
        DaemonAction::Outbox => outbox_cmd(),
    }
}

fn claude_quota_cmd(action: ClaudeQuotaAction) -> anyhow::Result<()> {
    use crate::setup::claude_quota::{IntegrationPaths, IntegrationStatus};

    match action {
        ClaudeQuotaAction::Install => {
            crate::setup::claude_quota::install_current_exe()?;
            println!("{}", summon_i18n::format!("r238008664a"));
        }
        ClaudeQuotaAction::Status => {
            let paths = IntegrationPaths::for_current_user()?;
            match crate::setup::claude_quota::status(&paths.settings, &paths.state)? {
                IntegrationStatus::NotInstalled => {
                    println!("{}", summon_i18n::format!("rbcffc9c5c5"))
                }
                IntegrationStatus::Installed => println!("{}", summon_i18n::format!("r609d316e58")),
                IntegrationStatus::Missing => {
                    println!("{}", summon_i18n::format!("r91e93b6dc3"))
                }
                IntegrationStatus::Drifted { status_line_owned } => {
                    println!("{}", summon_i18n::format!("r639ee9a1ba"))
                }
            }
            println!("Claude settings: {}", paths.settings.display());
        }
        ClaudeQuotaAction::Uninstall => {
            let paths = IntegrationPaths::for_current_user()?;
            let outcome = crate::setup::claude_quota::uninstall(&paths.settings, &paths.state)?;
            if outcome.status_line_drifted {
                println!("{}", summon_i18n::format!("r2882b6a0aa"));
            } else if outcome.status_line_restored {
                println!("{}", summon_i18n::format!("r2de4d4135a"));
            } else {
                println!("{}", summon_i18n::format!("re5ca67afdb"));
            }
        }
    }
    Ok(())
}

/// 入站消息列表:按群组聚合,显示排队中/处理中/已回复状态。控制台用。
fn inbox_cmd(json: bool) -> anyhow::Result<()> {
    use crate::runtime::inbox_wal::{InboxRecord, InboxWal};
    use crate::runtime::paths::run_dir;

    let wal = InboxWal::new(run_dir().join("inbox"));
    let pending = wal.scan_pending_strict()?;

    // 扫描 consumed 目录:已回复的消息。
    let consumed_dir = run_dir().join("inbox").join("consumed");
    let mut consumed = Vec::<InboxRecord>::new();
    if let Ok(entries) = std::fs::read_dir(&consumed_dir) {
        for agent_dir in entries.flatten() {
            let agent_path = agent_dir.path();
            if !agent_path.is_dir() {
                continue;
            }
            if let Ok(files) = std::fs::read_dir(&agent_path) {
                for file in files.flatten() {
                    let path = file.path();
                    if path.extension().and_then(|e| e.to_str()) != Some("json") {
                        continue;
                    }
                    if let Ok(content) = std::fs::read_to_string(&path) {
                        if let Ok(record) = serde_json::from_str::<InboxRecord>(&content) {
                            consumed.push(record);
                        }
                    }
                }
            }
        }
    }

    // 按 chat_id 聚合。
    let mut groups: std::collections::BTreeMap<String, Vec<serde_json::Value>> =
        std::collections::BTreeMap::new();

    for record in &pending {
        let status = if record.started {
            "processing"
        } else {
            "queued"
        };
        let item = inbox_item_json(record, status);
        groups
            .entry(record.inbound.chat_id.clone())
            .or_default()
            .push(item);
    }
    for record in &consumed {
        let item = inbox_item_json(record, "replied");
        groups
            .entry(record.inbound.chat_id.clone())
            .or_default()
            .push(item);
    }

    // 每组内按创建时间排序。
    let output: Vec<serde_json::Value> = groups
        .into_iter()
        .map(|(chat_id, mut items)| {
            items.sort_by_key(|item| item.get("created_ms").and_then(|v| v.as_i64()).unwrap_or(0));
            serde_json::json!({
                "chat_id": chat_id,
                "items": items,
            })
        })
        .collect();

    if json {
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else if output.is_empty() {
        println!("{}", summon_i18n::format!("r3da4843e34"));
    } else {
        println!("CHAT_ID\tSTATUS\tAGENT\tTEXT\tCREATED");
        for group in &output {
            let chat_id = group.get("chat_id").and_then(|v| v.as_str()).unwrap_or("");
            if let Some(items) = group.get("items").and_then(|v| v.as_array()) {
                for item in items {
                    println!(
                        "{}\t{}\t{}\t{}\t{}",
                        chat_id,
                        item.get("status").and_then(|v| v.as_str()).unwrap_or(""),
                        item.get("agent").and_then(|v| v.as_str()).unwrap_or(""),
                        item.get("text").and_then(|v| v.as_str()).unwrap_or(""),
                        item.get("created_ms").and_then(|v| v.as_i64()).unwrap_or(0),
                    );
                }
            }
        }
    }
    Ok(())
}

fn inbox_item_json(
    record: &crate::runtime::inbox_wal::InboxRecord,
    status: &str,
) -> serde_json::Value {
    serde_json::json!({
        "agent": record.agent,
        "message_id": record.inbound.message_id,
        "text": record.inbound.text,
        "sender": record.inbound.sender,
        "status": status,
        "created_ms": record.inbound.create_time_ms,
    })
}

/// 出站消息列表:按群组聚合,显示待投递/已投递/失败状态。控制台用。
fn outbox_cmd() -> anyhow::Result<()> {
    use crate::runtime::outbox_store::OutboxStore;

    let store = OutboxStore::at_run_dir();
    let pending = store.pending_events_checked()?;

    // 扫描 archive 和 dead 目录。
    let outbox_root = crate::runtime::paths::run_dir().join("outbox");
    let archived = scan_outbox_dir(&outbox_root.join("archive"));
    let dead = scan_outbox_dir(&outbox_root.join("dead"));

    // 按 chat_id 聚合。
    let mut groups: std::collections::BTreeMap<String, Vec<serde_json::Value>> =
        std::collections::BTreeMap::new();

    for event in &pending {
        if let Some((chat_id, item)) = outbox_item_json(event, "pending") {
            groups.entry(chat_id).or_default().push(item);
        }
    }
    for event in &archived {
        if let Some((chat_id, item)) = outbox_item_json(event, "sent") {
            groups.entry(chat_id).or_default().push(item);
        }
    }
    for event in &dead {
        if let Some((chat_id, item)) = outbox_item_json(event, "failed") {
            groups.entry(chat_id).or_default().push(item);
        }
    }

    let output: Vec<serde_json::Value> = groups
        .into_iter()
        .map(|(chat_id, mut items)| {
            items.sort_by_key(|item| item.get("created_ms").and_then(|v| v.as_u64()).unwrap_or(0));
            serde_json::json!({
                "chat_id": chat_id,
                "items": items,
            })
        })
        .collect();

    println!("{}", serde_json::to_string_pretty(&output)?);
    Ok(())
}

fn scan_outbox_dir(dir: &std::path::Path) -> Vec<summon_core::session::outbox::OutboxEvent> {
    let mut events = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            if let Ok(content) = std::fs::read_to_string(&path) {
                if let Ok(event) =
                    serde_json::from_str::<summon_core::session::outbox::OutboxEvent>(&content)
                {
                    events.push(event);
                }
            }
        }
    }
    events
}

/// 从 OutboxEvent 提取 chat_id 和生成控制台用 JSON。返回 (chat_id, json)。
fn outbox_item_json(
    event: &summon_core::session::outbox::OutboxEvent,
    status: &str,
) -> Option<(String, serde_json::Value)> {
    use summon_core::session::outbox::OutboxKind;

    let (chat_id, text) = match &event.kind {
        OutboxKind::AgentDisabled { chat_id, .. } => (chat_id.clone(), None),
        OutboxKind::NotifyChat { chat_id, text, .. } => (chat_id.clone(), Some(text.clone())),
        OutboxKind::ProcessSupplement { chat_id, .. } => (chat_id.clone(), None),
        OutboxKind::TurnReply { chat_id, text, .. } => (chat_id.clone(), Some(text.clone())),
        OutboxKind::ResumeTurn { chat_id, .. } => (chat_id.clone(), None),
        OutboxKind::RecoveryCard { chat_id, .. } => (chat_id.clone(), None),
        OutboxKind::BridgeActivationAudit { fence, .. } => (fence.chat_id.clone(), None),
        OutboxKind::ExternalBridgeTurn { fence, .. } => (fence.chat_id.clone(), None),
        OutboxKind::BridgeContextCapsule { fence, .. } => (fence.chat_id.clone(), None),
        OutboxKind::BridgeHandoffStatus { fence, .. } => (fence.chat_id.clone(), None),
    };

    let kind_str = match &event.kind {
        OutboxKind::AgentDisabled { .. } => "agent_disabled",
        OutboxKind::NotifyChat { .. } => "notify_chat",
        OutboxKind::ProcessSupplement { .. } => "process_supplement",
        OutboxKind::TurnReply { .. } => "turn_reply",
        OutboxKind::ResumeTurn { .. } => "resume_turn",
        OutboxKind::RecoveryCard { .. } => "recovery_card",
        OutboxKind::BridgeActivationAudit { .. } => "bridge_activation_audit",
        OutboxKind::ExternalBridgeTurn { .. } => "external_bridge_turn",
        OutboxKind::BridgeContextCapsule { .. } => "bridge_context_capsule",
        OutboxKind::BridgeHandoffStatus { .. } => "bridge_handoff_status",
    };

    Some((
        chat_id,
        serde_json::json!({
            "id": event.id,
            "agent": event.agent,
            "kind": kind_str,
            "status": status,
            "created_ms": event.created_ms,
            "text": text,
        }),
    ))
}

/// Claude 只允许一个 statusLine.command。该隐藏入口先 best-effort 记录额度快照，再把同一份
/// stdin 转给安装前保存的命令，并逐字节转发 stdout/stderr 与退出码。
fn claude_quota_statusline() -> anyhow::Result<()> {
    use std::io::Read as _;

    let mut input = Vec::new();
    std::io::stdin()
        .read_to_end(&mut input)
        .context(summon_i18n::text!("r045664171a"))?;
    let paths = crate::setup::claude_quota::IntegrationPaths::for_current_user()?;
    // 本进程是 claude 的子进程、继承它的环境;`SUMMON_MANAGED_SESSION=1` 由 `Session::augment_launch`
    // 注入每个 summon 起的 tmux 会话(原是 bridge 用来排除 fleet 会话的,这里复用同一个事实)。
    let managed = std::env::var(crate::bridge::MANAGED_SESSION_ENV).is_ok_and(|v| v == "1");
    // 托管会话里要不要照常跑用户那条 statusLine:读**本机** config.yaml。这个进程是 claude
    // 的子进程,拿不到 agent 上下文,所以这个开关只能是全局的(也正是它该有的粒度:那条命令
    // 属于这台机器)。
    let delegate_in_managed = summon_core::config::Config::load()
        .claude
        .status_line_in_managed_sessions;
    // 内部路由(模型请求走自建或第三方网关而非 anthropic.com)时,CC 上报的 ctx 百分比在自动压缩后会
    // 钉死在 0,需要 wrapper 用转录真实 usage 兜底;官方端点保持原样。env 在此进程读,不下沉到
    // bridge 里,让接线可在不污染环境变量的前提下被测。
    let internal = std::env::var("ANTHROPIC_BASE_URL")
        .map(|url| crate::setup::claude_quota::is_internal_routing_url(&url))
        .unwrap_or(false);
    let Some(output) = crate::setup::claude_quota::bridge_status_line(
        &paths.state,
        &paths.snapshot,
        &input,
        managed,
        delegate_in_managed,
        internal,
    )?
    else {
        return Ok(());
    };
    std::io::stdout()
        .write_all(&output.stdout)
        .context(summon_i18n::text!("r92a3ebf34c"))?;
    std::io::stderr()
        .write_all(&output.stderr)
        .context(summon_i18n::text!("rf6ac0d5074"))?;
    if !output.status.success() {
        std::process::exit(output.status.code().unwrap_or(1));
    }
    Ok(())
}

fn menubar_cmd(cfg: &Config, action: MenubarAction) -> anyhow::Result<()> {
    use crate::setup::menubar;
    match action {
        MenubarAction::Usage => super::menubar_usage::run()?,
        MenubarAction::Install | MenubarAction::Restart => {
            menubar::install()?;
            println!("{}", summon_i18n::format!("rfc87b5835f"));
        }
        MenubarAction::Uninstall => {
            menubar::uninstall()?;
            println!("{}", summon_i18n::format!("r50b72b6094"));
        }
        MenubarAction::Status => {
            let status = menubar::status()?;
            println!(
                "Menu Bar binary:{} | plist:{} | launchd:{}",
                if status.binary_exists {
                    summon_i18n::text!("r4a558371f9")
                } else {
                    summon_i18n::text!("r65364e6fd8")
                },
                if status.plist_exists {
                    summon_i18n::text!("ra8b6c39dca")
                } else {
                    summon_i18n::text!("r156219e305")
                },
                if status.running {
                    summon_i18n::text!("r1f0eb99b7e")
                } else {
                    summon_i18n::text!("r62cdc8713b")
                },
            );
        }
        MenubarAction::Sessions { json } => {
            let sessions = crate::observe::external_sessions::discover_external_sessions(
                std::time::SystemTime::now(),
            );
            if json {
                println!("{}", serde_json::to_string(&sessions)?);
            } else if sessions.is_empty() {
                println!("{}", summon_i18n::format!("r94a6f6ec58"));
            } else {
                for session in sessions {
                    let provider = match session.provider {
                        crate::observe::external_sessions::ExternalSessionProvider::Claude => {
                            "Claude"
                        }
                        crate::observe::external_sessions::ExternalSessionProvider::Cursor => {
                            "Cursor CLI"
                        }
                        crate::observe::external_sessions::ExternalSessionProvider::CodexCli => {
                            "Codex CLI"
                        }
                        crate::observe::external_sessions::ExternalSessionProvider::CodexApp => {
                            "Codex App"
                        }
                    };
                    let state = if session.busy {
                        summon_i18n::text!("rff6a7e52c4")
                    } else {
                        match session.state {
                            crate::observe::external_sessions::ExternalSessionState::Running => {
                                summon_i18n::text!("r582d4df2b6")
                            }
                            crate::observe::external_sessions::ExternalSessionState::Completed => {
                                summon_i18n::text!("r684134665e")
                            }
                        }
                    };
                    println!("{provider} · {} · {state}", session.title);
                }
            }
        }
        MenubarAction::Handled {
            agent,
            tag,
            reply,
            json,
        } => {
            use crate::runtime::waiting_store::{clear_if_reply, HandledOutcome};
            let reg = load_reg(cfg)?;
            let agent = reg
                .agents
                .iter()
                .find(|a| a.name == agent)
                .ok_or_else(|| anyhow::anyhow!(summon_i18n::format!("r9d536d57a2")))?;
            let outcome = clear_if_reply(&agent.state_dir(), &tag, &reply)?;
            if json {
                println!("{}", serde_json::json!({ "outcome": outcome }));
            } else {
                match outcome {
                    HandledOutcome::Cleared => println!("{}", summon_i18n::format!("rd96a41b521")),
                    HandledOutcome::Superseded => {
                        println!("{}", summon_i18n::format!("r4b1ef2497a"))
                    }
                    HandledOutcome::Absent => println!("{}", summon_i18n::format!("r61df126708")),
                }
            }
        }
        MenubarAction::EndSession {
            agent,
            tag,
            archive,
            interrupt,
            json,
        } => {
            use crate::runtime::menubar_session::{end_session, EndSessionOutcome, LivePort};
            let reg = load_reg(cfg)?;
            let agent = reg
                .agents
                .iter()
                .find(|a| a.name == agent)
                .ok_or_else(|| anyhow::anyhow!(summon_i18n::format!("r9d536d57a2")))?;
            let port = LivePort { agent, tag: &tag };
            let outcome = end_session(&port, &agent.name, &tag, archive, interrupt)?;
            if json {
                println!("{}", serde_json::json!({ "outcome": outcome }));
            } else {
                let verb = if archive {
                    summon_i18n::text!("r5292ab1a36")
                } else {
                    summon_i18n::text!("r498e1d59b4")
                };
                match outcome {
                    EndSessionOutcome::Ended => println!("{}", summon_i18n::format!("r597ee8e766")),
                    EndSessionOutcome::Running => {
                        println!("{}", summon_i18n::format!("r0e67045c1b"))
                    }
                    EndSessionOutcome::Pending => {
                        println!("{}", summon_i18n::format!("ra598aacb00"))
                    }
                    EndSessionOutcome::Absent => {
                        println!("{}", summon_i18n::format!("r4e3a8715cd"))
                    }
                }
            }
        }
        MenubarAction::Settings { json } => {
            let reg = load_reg(cfg)?;
            let (autostart, _) = crate::setup::autostart::status();
            let settings = crate::setup::menubar_settings::read(cfg, &reg, autostart.enabled);
            if json {
                println!("{}", serde_json::to_string(&settings)?);
            } else {
                print!("{}", serde_yaml::to_string(&settings)?);
            }
        }
        MenubarAction::Set { key, value } => {
            use crate::setup::menubar_settings::{apply_to_config, SettingChange, SettingKey};
            match SettingChange::parse(SettingKey::parse(&key)?, &value)? {
                // 开机自启走 `summon autostart` 同一条路(含补存 PATH),保留现有的 keep_alive。
                SettingChange::LaunchAtLogin(on) => {
                    let (state, _) = crate::setup::autostart::status();
                    autostart_cmd(if on {
                        AutostartAction::Enable {
                            keep_alive: Some(state.keep_alive),
                        }
                    } else {
                        AutostartAction::Disable
                    })?;
                }
                change => {
                    use crate::setup::menubar_settings::SentinelMode;
                    if matches!(
                        change,
                        SettingChange::Sentinel(SentinelMode::DryRun | SentinelMode::On)
                    ) {
                        ensure_sentinel_identity(cfg, false)?;
                    }
                    apply_to_config(&summon_core::config::config_path(), &change)?;
                    println!("{}", summon_i18n::format!("rca2a003212"));
                }
            }
        }
    }
    Ok(())
}

/// 开 sentinel(含 dry-run)之前把它要的 lark-cli 身份配好。`sentinel set` 与 Menu Bar 设置区
/// 两个入口都经这里:交互终端里带人走完向导,其余只列命令并拒绝开启。
fn ensure_sentinel_identity(cfg: &Config, interactive: bool) -> anyhow::Result<()> {
    let lark_cli = summon_core::tools::resolve_from(cfg, summon_core::tools::Tool::LarkCli);
    crate::setup::lark_cli::ensure_identity(
        &lark_cli,
        cfg.sentinel.lark_profile.as_deref(),
        interactive,
    )
}

/// 开机自启(启动项):本地直接装/卸自启单元,不经 daemon(单元是机器级的事)。
fn autostart_cmd(action: AutostartAction) -> anyhow::Result<()> {
    use crate::setup::autostart;
    match action {
        AutostartAction::Enable { keep_alive } => {
            let keep_alive = keep_alive.unwrap_or_else(|| autostart::read_state().keep_alive);
            if let Some(why) = autostart::unavailable_reason() {
                println!("{}", summon_i18n::format!("rdafb231682"));
                return Ok(());
            }
            // 单元拉起的 daemon 靠它补齐 PATH,必须赶在装载之前:launchd 的 `RunAtLoad` 在装载那一刻
            // 就会起一个。先起 daemon、后开自启的机器,开机时也要有这一份。
            if let Err(e) = crate::setup::env::save_user_path(true) {
                eprintln!("{}", summon_i18n::format!("rc1d0201ecc"));
            }
            autostart::enable(keep_alive)?;
            match autostart::supervision_possible() {
                Ok(true) => {
                    let extra = if keep_alive {
                        summon_i18n::text!("rde25282517")
                    } else {
                        ""
                    };
                    println!("{}", summon_i18n::format!("r651c8109a9"));
                }
                Ok(false) => {
                    println!("{}", summon_i18n::format!("radb5462da7"))
                }
                Err(e) => println!("{}", summon_i18n::format!("rf714a89721")),
            }
        }
        AutostartAction::Disable => {
            autostart::disable()?;
            println!("{}", summon_i18n::format!("r0bd50d3402"));
        }
        AutostartAction::Status => {
            let (st, installed) = autostart::status();
            let (on, crash) = autostart_summary(st.enabled);
            println!(
                "{}",
                summon_i18n::format!(
                    "r0cf0b95283",
                    if st.keep_alive {
                        summon_i18n::text!("rb5141d3d19")
                    } else {
                        summon_i18n::text!("r0c70665b6e")
                    },
                    autostart::unit_kind_label(),
                    if installed {
                        summon_i18n::text!("ra8b6c39dca")
                    } else {
                        summon_i18n::text!("r484d556139")
                    },
                )
            );
            if let Some(line) = supervision_line(st.enabled) {
                println!("{}", summon_i18n::format!("r90f67d37e1"));
            }
        }
    }
    Ok(())
}

/// 自启的登记状态与「崩溃会不会被拉起」,按这台机器上单元实际能不能起来说。
fn autostart_summary(enabled: bool) -> (String, &'static str) {
    if !enabled {
        return (
            summon_i18n::text!("r3cffa9757b").to_string(),
            summon_i18n::text!("r0c70665b6e"),
        );
    }
    match crate::setup::autostart::supervision_possible() {
        Ok(true) => (
            summon_i18n::text!("r8a4ef3e48e").to_string(),
            summon_i18n::text!("rb5141d3d19"),
        ),
        Ok(false) => (
            summon_i18n::text!("rc78f47065d").to_string(),
            summon_i18n::text!("r0c70665b6e"),
        ),
        Err(e) => (
            summon_i18n::format!("re3c498d11a"),
            summon_i18n::text!("rd62f5dd6d1"),
        ),
    }
}

/// 开了自启时,此刻在跑的 daemon 是不是真归自启单元管 —— 配置说会拉起,不等于这一代会被拉起
/// (比如它是自启配置落地前起的,或托管启动失败后退回了直接 spawn)。
fn supervision_line(autostart_enabled: bool) -> Option<String> {
    use summon_core::autostart::LoginItemBackend;
    if !autostart_enabled {
        return None;
    }
    let supervisor = crate::setup::autostart::supervisor_label();
    if LoginItemBackend::current() == LoginItemBackend::SystemdUser {
        match crate::setup::autostart::linger_enabled() {
            Ok(true) => {}
            Ok(false) => return Some(summon_i18n::text!("r532014fc50").to_string()),
            Err(e) => return Some(summon_i18n::format!("rbd93937810")),
        }
    }
    let running = crate::runtime::daemon::running_pid()?;
    Some(match crate::setup::autostart::supervised_pid() {
        Ok(Some(pid)) if pid == running => {
            summon_i18n::format!("r4ad7a0cfb1")
        }
        Ok(_) => summon_i18n::format!("r691e03a507"),
        Err(e) => summon_i18n::format!("r759de9e297"),
    })
}

fn daemon_start() -> anyhow::Result<()> {
    use std::process::Stdio;
    std::fs::create_dir_all(crate::runtime::paths::run_dir())?;
    // 单例保护:已有存活 daemon 就别再起(避免多个 daemon/sidecar抢飞书事件)
    if let Some(pid) = crate::runtime::daemon::running_pid() {
        println!(
            "{}",
            summon_i18n::format!("r47a0c7ee3f", daemon_identity_label(pid))
        );
        return Ok(());
    }
    // pidfile 缺失/陈旧却仍有人持 daemon flock：可能是自启单元正在接棒。短等它
    // 补齐身份；若锁释放则本 CLI 接着启动，若始终不可证明则拒绝覆盖。
    if crate::runtime::daemon::daemon_lock_held()? {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if let Some(pid) = crate::runtime::daemon::running_pid() {
                println!(
                    "{}",
                    summon_i18n::format!("r47a0c7ee3f", daemon_identity_label(pid))
                );
                return Ok(());
            }
            if !crate::runtime::daemon::daemon_lock_held()? {
                break;
            }
            if std::time::Instant::now() >= deadline {
                anyhow::bail!(summon_i18n::format!("r41384247fd"));
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }
    if let Some(notice) = crate::runtime::daemon::agents_missing_notice(
        &summon_core::config::Config::load().agents_dir,
    ) {
        println!("{notice}");
        return Ok(());
    }
    // 锁已确认空闲，此时清陈旧身份安全；避免 PID 复用后旧 ready 恰好等于新 child.id 造成假就绪。
    let _ = std::fs::remove_file(crate::runtime::paths::pid_file());
    let _ = std::fs::remove_file(crate::runtime::paths::ready_file());
    let _ = std::fs::remove_file(crate::runtime::paths::daemon_version_file());
    let log = crate::runtime::paths::run_dir().join("daemon.log");
    // U6:append 打开(保留上一程日志——崩溃现场排查靠它;原 File::create 每次 start 都抹掉),
    // 超 10MB 启动期轮转成 .1 保留一代。运行期轮转见 observe::log(daemon 进程内启用)。
    if std::fs::metadata(&log)
        .map(|m| m.len() > 10 * 1024 * 1024)
        .unwrap_or(false)
    {
        let _ = std::fs::rename(&log, crate::runtime::paths::run_dir().join("daemon.log.1"));
    }
    // 先让落点留下两份里更新的那份,再用落点那份起 —— 开机自启登记的就是它,各条安装渠道由此
    // 收敛到同一个文件。当前这份不在落点就把自己发布进去;落点那份更新、或判不出新旧时不覆盖它、
    // 照样用它起(保住自启托管)。
    // 发布失败(落点认不出构建、安装源不完整)只是这一程仍跑当前这份,不该挡住启动。
    let published = crate::setup::self_install::publish_current_exe().map(|outcome| {
        if let Some(build) = &outcome.published {
            eprintln!(
                "{}",
                summon_i18n::format!("r1246a3a9e5", build.describe(), outcome.target.display())
            );
        }
        if let Some(notice) = &outcome.notice {
            eprintln!("note: {notice}");
        }
        outcome.target
    });
    // 声明这次是人工启动:daemon 据此不受「上一代没跑稳就不自动拉起」的熔断约束。写不下来只影响
    // 上一代刚崩过的那种情形(会被熔断拦下并在日志里说明),不挡启动。
    if let Err(e) = crate::runtime::daemon::write_start_request() {
        eprintln!("{}", summon_i18n::format!("r26d37dfbf5"));
    }
    // 自启单元只给最小环境,agent 会话的 PATH 又全继承自 daemon:趁现在把用户的 PATH 存下来。
    let autostart_enabled = crate::setup::autostart::read_state().enabled;
    if let Err(e) = crate::setup::env::save_user_path(autostart_enabled) {
        eprintln!("{}", summon_i18n::format!("rce410e45da"));
    }
    // 开了自启就交给自启单元起,daemon 归它管、崩溃能被拉起。自启登记的是落点,发布失败时
    // 不拿别的路径去改登记,直接自己 spawn。
    if let Ok(exe) = &published {
        match crate::setup::autostart::start_supervised(exe) {
            Ok(true) => return wait_supervised_ready(&log),
            Ok(false) => {}
            Err(e) => eprintln!(
                "{}",
                summon_i18n::format!("re348a40de9", crate::setup::autostart::supervisor_label())
            ),
        }
    }
    let exe = match published {
        Ok(path) => path,
        Err(e) => {
            eprintln!("{}", summon_i18n::format!("rbc4d6b74a4"));
            std::env::current_exe()?
        }
    };
    let out = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log)?;
    let err = out.try_clone()?;
    let mut command = daemon_run_command(&exe);
    command
        .stdout(Stdio::from(out))
        .stderr(Stdio::from(err))
        .stdin(Stdio::null());
    detach_daemon_child(&mut command);
    let mut child = command.spawn()?;
    let pid = child.id();
    // spawn 成功不等于 daemon 就绪。ready 只有在 signal/dispatcher/sockets/scheduler 全部就绪后
    // 才写；同时要求 pidfile+flock 身份仍是这个 child，旧哨兵/PID 复用都不能假成功。
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if ready_identity_matches(
            pid,
            crate::runtime::daemon::running_pid(),
            crate::runtime::daemon::ready_pid(),
        ) {
            break;
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                anyhow::bail!(summon_i18n::format!("rde15d8a878", log.display()));
            }
            Ok(None) => {}
            Err(error) => {
                terminate_failed_daemon_start(&mut child);
                return Err(error).context(summon_i18n::text!("r1c7650bb01"));
            }
        }
        if std::time::Instant::now() >= deadline {
            terminate_failed_daemon_start(&mut child);
            anyhow::bail!(summon_i18n::format!("r0c44b48b6a", log.display()));
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    println!(
        "{}",
        summon_i18n::format!("ra0f5282b3a", crate::version::VERSION, log.display())
    );
    Ok(())
}

/// 自启单元拉起的 daemon 没有子进程句柄可等,只能看身份文件:pidfile 与 ready 指向同一个活着、
/// 持锁的 pid 才算就绪(调用方已在启动前清掉陈旧的身份文件)。超时未就绪就像直接 spawn 那条路
/// 一样收掉它,经自启单元发信号,不依赖 pidfile 已经写下。
fn wait_supervised_ready(log: &std::path::Path) -> anyhow::Result<()> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let running = crate::runtime::daemon::running_pid();
        if let Some(pid) = supervised_ready_pid(running, crate::runtime::daemon::ready_pid()) {
            println!(
                "{}",
                summon_i18n::format!(
                    "rf6bfe15f2d",
                    crate::version::VERSION,
                    crate::setup::autostart::supervisor_label(),
                    log.display()
                )
            );
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            if let Err(e) = crate::setup::autostart::stop_supervised() {
                eprintln!("{}", summon_i18n::format!("r9603bb13ba"));
            }
            anyhow::bail!(summon_i18n::format!("r0c44b48b6a", log.display()));
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

fn supervised_ready_pid(running: Option<u32>, ready: Option<u32>) -> Option<u32> {
    running.filter(|pid| ready == Some(*pid))
}

/// daemon 必须脱离调用方的 session/控制终端；仅重定向 stdio 仍会随终端或宿主 job 退出。
fn detach_daemon_child(command: &mut Command) {
    use std::os::unix::process::CommandExt;

    // SAFETY:`pre_exec` 内只调用 POSIX async-signal-safe 的 setsid(2)，不分配内存、不拿锁。
    unsafe {
        command.pre_exec(|| {
            nix::unistd::setsid()
                .map(|_| ())
                .map_err(|errno| std::io::Error::from_raw_os_error(errno as i32))
        });
    }
}

/// detached child 不再随调用方退出；启动未就绪时必须由父 CLI 显式回收，避免留下持锁半成品。
fn terminate_failed_daemon_start(child: &mut std::process::Child) {
    let raw_pid = i32::try_from(child.id()).ok().filter(|pid| *pid > 0);
    if let Some(pid) = raw_pid {
        let _ = nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(pid),
            nix::sys::signal::Signal::SIGTERM,
        );
    }

    // daemon 的优雅停机宽限是 2s；多给 1s 收尾，仍不退再只回收这个亲生 child。
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Ok(None) | Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return;
            }
        }
    }
}

fn ready_identity_matches(expected: u32, running: Option<u32>, ready: Option<u32>) -> bool {
    running == Some(expected) && ready == Some(expected)
}

/// 直接 spawn 的 `daemon run` 子进程。
pub(crate) fn daemon_run_command(exe: &std::path::Path) -> Command {
    use std::os::unix::process::CommandExt;
    let mut command = Command::new(exe);
    command
        .arg0(DAEMON_PROCESS_NAME) // 按功能命名进程(ps 显示 summond,区别于 CLI 的 summon)
        .args(["daemon", "run"])
        .env_remove(RESTART_NO_DETACH_ENV);
    #[cfg(npm_dist)]
    crate::setup::npm_dist::scrub_daemon_env(&mut command);
    command
}

const DAEMON_STOP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
// 重启要等 producer 的在途请求与停机收尾完成，再交接给新一代。
const DAEMON_RESTART_STOP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// 设了它,`daemon restart` 就在本进程里重启:调用方已经在独立会话里(下面脱离出去的那段、升级的
/// 重启脚本)。它只管这一次重启,起 daemon 时剥掉([`daemon_run_command`]):留在 daemon 环境里,
/// daemon 拉起的进程都带着它,其中再执行的 `daemon restart` 就不脱离了。
pub(crate) const RESTART_NO_DETACH_ENV: &str = "SUMMON_RESTART_NO_DETACH";

/// `daemon restart` 命令。重启段默认脱离当前会话执行:daemon 停机会打断在跑的无头轮,codex / cursor
/// agent 在自己的轮里执行重启时,不脱离就会随这一轮被收掉 —— 停机做完了,启动那一半跑不到,机器上
/// 从此没有 daemon。脱离出去的那段被打断也会跑完,本进程只跟随它的输出、转交它的结果。
fn daemon_restart_cmd() -> anyhow::Result<()> {
    if std::env::var_os(RESTART_NO_DETACH_ENV).is_some() {
        return daemon_restart();
    }
    let exe = std::env::current_exe().context(summon_i18n::text!("r7e5c7f2030"))?;
    let log = crate::runtime::paths::run_dir().join("restart.log");
    restart_detached(&exe, &log, &mut std::io::stderr())
}

/// 用 `exe` 起脱离出去的那段,输出追加进 `log`,从这一刻的日志末尾跟随到它结束。追加而不截断:
/// 上一次的那段可能还在写(发起者先退出了),截断会把它的输出冲掉。
fn restart_detached(
    exe: &std::path::Path,
    log: &std::path::Path,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    use std::io::Seek;
    use std::process::Stdio;
    if let Some(dir) = log.parent() {
        std::fs::create_dir_all(dir)
            .with_context(|| summon_i18n::format!("rc18f37e5e1", dir.display()))?;
    }
    let sink = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)
        .with_context(|| summon_i18n::format!("rad09049d43", log.display()))?;
    let mut reader = std::fs::File::open(log)
        .with_context(|| summon_i18n::format!("rf53f6a49bc", log.display()))?;
    reader.seek(std::io::SeekFrom::End(0))?;
    let mut command = Command::new(exe);
    command
        .args(["daemon", "restart"])
        .env(RESTART_NO_DETACH_ENV, "1")
        .stdin(Stdio::null())
        .stdout(sink.try_clone()?)
        .stderr(sink);
    detach_daemon_child(&mut command);
    let child = command
        .spawn()
        .with_context(|| summon_i18n::format!("r1fff192e52", exe.display()))?;
    writeln!(
        out,
        "{}",
        summon_i18n::format!("r0cf83685a7", log.display())
    )?;
    let (status, _) = follow_detached(child, reader, out)?;
    anyhow::ensure!(
        status.success(),
        summon_i18n::format!("r2f32509b75", log.display())
    );
    Ok(())
}

/// 跟随脱离出去的那段:从 `reader` 当前位置起,把它写进日志的输出原样转给 `out`,直到它退出;
/// 返回退出状态与读到的全部输出。
pub(crate) fn follow_detached(
    mut child: std::process::Child,
    mut reader: std::fs::File,
    out: &mut dyn Write,
) -> anyhow::Result<(std::process::ExitStatus, String)> {
    use std::io::Read;
    let mut seen = Vec::new();
    let status = loop {
        // 先看是否结束再读:结束之前写下的输出这一次都读得到。
        let exited = child.try_wait()?;
        let mut chunk = Vec::new();
        reader.read_to_end(&mut chunk)?;
        out.write_all(&chunk)?;
        out.flush()?;
        seen.extend_from_slice(&chunk);
        if let Some(status) = exited {
            break status;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    };
    Ok((status, String::from_utf8_lossy(&seen).into_owned()))
}

/// 重启须等旧代释放单例锁，停机与启动的失败分别交代。
pub(crate) fn daemon_restart() -> anyhow::Result<()> {
    restart_after_stop(daemon_stop_with_timeout, daemon_start)
}

fn restart_after_stop(
    stop: impl FnOnce(std::time::Duration) -> anyhow::Result<()>,
    start: impl FnOnce() -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    eprintln!(
        "{}",
        summon_i18n::format!("rbb4793249d", DAEMON_RESTART_STOP_TIMEOUT.as_secs())
    );
    stop(DAEMON_RESTART_STOP_TIMEOUT).context(summon_i18n::text!("r2a9eb190d0"))?;
    eprintln!("{}", summon_i18n::format!("r9a39dddbf8"));
    start().context(summon_i18n::text!("r8c4e10b7a0"))
}

fn daemon_stop() -> anyhow::Result<()> {
    daemon_stop_with_timeout(DAEMON_STOP_TIMEOUT)
}

fn daemon_stop_with_timeout(timeout: std::time::Duration) -> anyhow::Result<()> {
    let running = crate::runtime::daemon::running_pid();
    if running.is_none() && crate::runtime::daemon::daemon_lock_held()? {
        anyhow::bail!(summon_i18n::format!("r107b13282d"));
    }
    match running {
        Some(pid) => {
            // 发信号前再次核对「活进程 + pidfile + 被占 flock 的 owner」同一身份。陈旧 pidfile
            // 即使 PID 已被复用也过不了此门，绝不向无关进程发信号。
            if !crate::runtime::daemon::daemon_identity_matches(pid) {
                anyhow::bail!(summon_i18n::format!("r6908f82a21"));
            }
            // 停机请求只说明这一代经 `daemon stop` 停下(人或重启段都会写);写不成不挡停机。
            let _ = crate::runtime::daemon::write_stop_request(pid);
            nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(pid as i32),
                nix::sys::signal::Signal::SIGTERM,
            )
            .with_context(|| summon_i18n::format!("ra6c55622e3"))?;
            let _ = crate::runtime::daemon::remove_identity_file_if_matches(
                &crate::runtime::paths::ready_file(),
                pid,
            );
            // pidfile 由 daemon 在完成无头进程收尾、释放 flock 前保持。必须等旧 pid
            // 的**锁身份**消失才返回；PID 即使立刻复用，也不会把新进程误认成旧 daemon。
            let exited = wait_for_process_exit(
                pid,
                timeout,
                std::time::Duration::from_millis(50),
                crate::runtime::daemon::daemon_lock_still_owned_by,
            );
            if !exited {
                anyhow::bail!(summon_i18n::format!("r38478c9ca6", timeout.as_secs()));
            }
            // 正常路径由 daemon 自删；CLI 只 compare-and-delete 旧代。KeepAlive 若已写入新 PID，
            // 绝不会被旧 stop 擦掉。
            let _ = crate::runtime::daemon::remove_identity_file_if_matches(
                &crate::runtime::paths::pid_file(),
                pid,
            );
            let _ = crate::runtime::daemon::remove_daemon_version_if_matches(pid);
            println!("{}", summon_i18n::format!("rec09706eaf"));
        }
        None => {
            // 已确认 flock 空闲，清陈旧身份文件安全，不会误碰活 daemon。
            let _ = std::fs::remove_file(crate::runtime::paths::pid_file());
            let _ = std::fs::remove_file(crate::runtime::paths::ready_file());
            let _ = std::fs::remove_file(crate::runtime::paths::daemon_version_file());
            println!("{}", summon_i18n::format!("re98e670fc1"));
        }
    }
    Ok(())
}

/// 轮询等指定 daemon 代际交出单例锁。探测器可注入,使「不提前返回 / 超时有界」可单测。
fn wait_for_process_exit(
    pid: u32,
    timeout: std::time::Duration,
    poll: std::time::Duration,
    mut is_alive: impl FnMut(u32) -> bool,
) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if !is_alive(pid) {
            return true;
        }
        let now = std::time::Instant::now();
        if now >= deadline {
            return false;
        }
        std::thread::sleep(poll.min(deadline.saturating_duration_since(now)));
    }
}

#[cfg(test)]
mod daemon_lifecycle_tests {
    use super::{
        daemon_run_command, daemon_status_line, detach_daemon_child, ready_identity_matches,
        restart_after_stop, restart_detached, supervised_ready_pid, terminate_failed_daemon_start,
        wait_for_process_exit, DAEMON_STOP_TIMEOUT, RESTART_NO_DETACH_ENV,
    };
    use std::cell::Cell;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    /// 假 summon:报自己的 pid 与进程组、收到的参数和免脱离标记,按同目录 `rc` 文件退出。
    const FAKE_RESTART: &str = r#"#!/bin/sh
echo "pid=$$ pgid=$(ps -o pgid= -p $$ | tr -d ' ') args=$* no_detach=${SUMMON_RESTART_NO_DETACH:-}"
exit "$(cat "$(dirname "$0")/rc")"
"#;

    #[test]
    fn restart_runs_in_its_own_process_group_and_reports_what_it_printed() {
        let tmp = tempfile::tempdir().unwrap();
        let exe = tmp.path().join("summon");
        crate::test_support::write_test_executable(&exe, FAKE_RESTART);
        std::fs::write(tmp.path().join("rc"), "0").unwrap();
        let log = tmp.path().join("run/restart.log");
        let mut out = Vec::new();
        restart_detached(&exe, &log, &mut out).unwrap();
        let out = String::from_utf8(out).unwrap();
        assert!(out.contains("脱离当前会话"), "{out}");
        let line = out.lines().find(|l| l.starts_with("pid=")).expect(&out);
        let field = |key: &str| {
            line.split(' ')
                .find_map(|f| f.strip_prefix(key))
                .unwrap_or_else(|| panic!("{key} 不在 {line}"))
                .to_string()
        };
        // 自成进程组,打断发起者的那一组信号够不着它。
        assert_eq!(field("pid="), field("pgid="), "{line}");
        assert_ne!(
            field("pgid="),
            nix::unistd::getpgrp().to_string(),
            "不得留在发起者的进程组里"
        );
        assert!(line.contains("args=daemon restart "), "{line}");
        assert_eq!(field("no_detach="), "1", "脱离出去的那段得在本进程里重启");
    }

    #[test]
    fn restart_fails_when_the_detached_part_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let exe = tmp.path().join("summon");
        crate::test_support::write_test_executable(&exe, FAKE_RESTART);
        std::fs::write(tmp.path().join("rc"), "3").unwrap();
        let log = tmp.path().join("restart.log");
        // 上一次重启的输出留在日志里,不转给这一次的发起者。
        std::fs::write(&log, "上一次的重启\n").unwrap();
        let mut out = Vec::new();
        let error = restart_detached(&exe, &log, &mut out).unwrap_err();
        assert!(std::fs::read_to_string(&log)
            .unwrap()
            .starts_with("上一次的重启\npid="));
        let message = format!("{error:#}");
        assert!(message.contains("没成功"), "{message}");
        assert!(message.contains(&log.display().to_string()), "{message}");
        let out = String::from_utf8(out).unwrap();
        assert!(out.contains("args=daemon restart"), "{out}");
        assert!(!out.contains("上一次的重启"), "{out}");
    }

    /// 带着免脱离标记的 `daemon restart` 才在本进程里重启;判断得在起脱离那段之前,否则脱离出去的
    /// 那段会再脱离一次,没完没了。
    #[test]
    fn restart_detaches_unless_the_marker_is_set() {
        let src = include_str!("cli.rs");
        let body = &src[src.find("fn daemon_restart_cmd()").unwrap()..];
        let body = &body[..body.find("\nfn ").unwrap()];
        let marker = body
            .find("std::env::var_os(RESTART_NO_DETACH_ENV).is_some()")
            .expect("入口得看免脱离标记");
        let in_place = body.find("return daemon_restart();").unwrap();
        let detached = body.find("restart_detached(").unwrap();
        assert!(marker < in_place && in_place < detached, "{body}");
        let dispatch = &src[src.find("fn daemon_cmd(").unwrap()..];
        let dispatch = &dispatch[..dispatch.find("\nfn ").unwrap()];
        assert!(dispatch.contains("DaemonAction::Restart => daemon_restart_cmd(),"));
    }

    #[test]
    fn the_spawned_daemon_drops_the_no_detach_marker() {
        let command = daemon_run_command(std::path::Path::new("/opt/summon"));
        assert!(command
            .get_envs()
            .any(|(key, value)| key == RESTART_NO_DETACH_ENV && value.is_none()));
    }

    #[test]
    fn restart_waits_for_slow_shutdown_before_starting() {
        use std::sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        };
        let held = Arc::new(AtomicBool::new(true));
        let release = Arc::clone(&held);
        // 按 1:100 缩放两个预算：旧代在普通 Stop 的预算之后才释放锁。
        let stop_budget = Duration::from_millis(DAEMON_STOP_TIMEOUT.as_secs() * 10);
        let worker = std::thread::spawn(move || {
            std::thread::sleep(stop_budget + Duration::from_millis(100));
            release.store(false, Ordering::SeqCst);
        });
        let starts = Cell::new(0);
        let result = restart_after_stop(
            |timeout| {
                assert_eq!(timeout, Duration::from_secs(60));
                anyhow::ensure!(
                    wait_for_process_exit(
                        42,
                        Duration::from_millis(timeout.as_secs() * 10),
                        Duration::from_millis(1),
                        |_| held.load(Ordering::SeqCst)
                    ),
                    "still locked"
                );
                Ok(())
            },
            || {
                assert!(!held.load(Ordering::SeqCst), "旧代持锁时不得启动");
                starts.set(starts.get() + 1);
                Ok(())
            },
        );
        worker.join().unwrap();
        result.unwrap();
        assert_eq!(starts.get(), 1);
    }

    #[test]
    fn restart_does_not_start_when_shutdown_fails() {
        for reason in ["单例锁仍被旧代占用", "daemon 身份无法确认"] {
            let starts = Cell::new(0);
            let error = restart_after_stop(
                |_| anyhow::bail!("{reason}"),
                || {
                    starts.set(starts.get() + 1);
                    Ok(())
                },
            )
            .unwrap_err();
            assert_eq!(starts.get(), 0);
            let message = format!("{error:#}");
            assert!(message.contains(reason));
            assert!(message.contains("未进入启动阶段"));
            assert!(message.contains("summon daemon start"));
        }
    }

    #[test]
    fn restart_distinguishes_start_failure_from_shutdown_failure() {
        let stopped = Cell::new(false);
        let error = restart_after_stop(
            |_| {
                stopped.set(true);
                Ok(())
            },
            || {
                assert!(stopped.get());
                anyhow::bail!("ready timeout")
            },
        )
        .unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("启动阶段失败"));
        assert!(message.contains("ready timeout"));
        assert!(!message.contains("未进入启动阶段"));
    }

    #[test]
    fn stop_waits_until_probe_observes_lock_release() {
        let probes = Cell::new(0usize);
        let exited = wait_for_process_exit(
            42,
            Duration::from_millis(100),
            Duration::from_millis(1),
            |pid| {
                assert_eq!(pid, 42);
                let n = probes.get() + 1;
                probes.set(n);
                n < 3
            },
        );
        assert!(exited);
        assert_eq!(probes.get(), 3, "不得在旧 daemon 仍持锁时提前返回");
    }

    #[test]
    fn stop_wait_has_a_hard_deadline() {
        let started = Instant::now();
        let exited = wait_for_process_exit(
            42,
            Duration::from_millis(5),
            Duration::from_millis(1),
            |_| true,
        );
        assert!(!exited, "始终存活的 pid 应按期超时");
        assert!(started.elapsed() < Duration::from_secs(1), "等待不得无界");
    }

    /// 自启单元只给最小环境:用户 PATH 必须在交给单元之前存下,daemon 才补得上;而且每次
    /// `daemon start` 都存(不看自启开没开),`autostart enable` 也存 —— 先起 daemon、后开自启、
    /// 再重启的机器,开机拉起的 daemon 同样有得补。
    #[test]
    fn the_user_path_is_saved_wherever_a_unit_may_later_start_the_daemon() {
        let src = include_str!("cli.rs");
        let body = &src[src.find("fn daemon_start()").unwrap()..];
        let body = &body[..body.find("\nfn ").unwrap()];
        let saved = body
            .find("crate::setup::env::save_user_path(autostart_enabled)")
            .unwrap();
        let line_start = body[..saved].rfind('\n').unwrap();
        assert!(
            !body[..line_start].trim_end().ends_with("enabled {"),
            "保存不能被「开了自启」包住"
        );
        let supervised = body
            .find("crate::setup::autostart::start_supervised(")
            .unwrap();
        let spawned = body.find("command.spawn()").unwrap();
        assert!(saved < supervised && saved < spawned);

        let enable = &src[src
            .find("AutostartAction::Enable { keep_alive } => {")
            .unwrap()..];
        let enable = &enable[..enable.find("AutostartAction::Disable").unwrap()];
        let saved_on_enable = enable
            .find("crate::setup::env::save_user_path(true)")
            .unwrap();
        let loaded = enable.find("autostart::enable(keep_alive)").unwrap();
        assert!(
            saved_on_enable < loaded,
            "launchd 装载那一刻就会起 daemon,PATH 得先存下"
        );
    }

    #[test]
    fn supervised_start_is_ready_only_when_pidfile_and_ready_agree() {
        assert_eq!(supervised_ready_pid(Some(42), Some(42)), Some(42));
        assert_eq!(supervised_ready_pid(Some(42), None), None);
        assert_eq!(supervised_ready_pid(Some(42), Some(41)), None);
        assert_eq!(supervised_ready_pid(None, Some(42)), None);
    }

    #[test]
    fn start_requires_ready_from_the_same_spawned_generation() {
        assert!(ready_identity_matches(42, Some(42), Some(42)));
        assert!(!ready_identity_matches(42, Some(42), None));
        assert!(!ready_identity_matches(42, Some(42), Some(41)));
        assert!(!ready_identity_matches(42, Some(41), Some(42)));
    }

    #[test]
    fn the_spawned_daemon_runs_the_daemon_subcommand() {
        let command = daemon_run_command(std::path::Path::new("/opt/summon"));
        assert_eq!(command.get_args().collect::<Vec<_>>(), ["daemon", "run"]);
    }

    #[test]
    fn daemon_status_reports_the_running_daemon_version() {
        assert_eq!(
            daemon_status_line(42, Some("0.1.202607130802")),
            "running(pid 42, version 0.1.202607130802)"
        );
        assert_eq!(
            daemon_status_line(42, None),
            "running(pid 42, version unknown)"
        );
    }

    #[test]
    fn daemon_child_starts_in_own_session() {
        let mut command = Command::new("sleep");
        command
            .arg("30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        detach_daemon_child(&mut command);

        let mut child = command.spawn().expect("spawn detached child");
        let pid = nix::unistd::Pid::from_raw(child.id() as i32);
        let session = nix::unistd::getsid(Some(pid)).expect("get child session");
        let process_group = nix::unistd::getpgid(Some(pid)).expect("get child process group");
        let parent_session = nix::unistd::getsid(None).expect("get test session");
        let _ = child.kill();
        let _ = child.wait();

        assert_eq!(session, pid, "daemon child 必须成为新 session leader");
        assert_eq!(process_group, pid, "setsid 应同时建立独立进程组");
        assert_ne!(session, parent_session, "daemon 不得继承调用方 session");
    }

    #[test]
    fn failed_detached_start_is_explicitly_reaped() {
        let mut command = Command::new("sleep");
        command
            .arg("30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        detach_daemon_child(&mut command);

        let mut child = command.spawn().expect("spawn detached child");
        let started = Instant::now();
        terminate_failed_daemon_start(&mut child);

        assert!(
            child.try_wait().expect("probe reaped child").is_some(),
            "启动失败的 detached child 不得残留"
        );
        assert!(started.elapsed() < Duration::from_secs(1));
    }
}

fn daemon_status() -> anyhow::Result<()> {
    match crate::runtime::daemon::running_pid() {
        Some(pid) => println!(
            "{}",
            daemon_status_line(pid, crate::runtime::daemon::daemon_version(pid).as_deref())
        ),
        None => println!("{}", summon_i18n::format!("re98e670fc1")),
    }
    Ok(())
}

fn daemon_identity_label(pid: u32) -> String {
    let version = crate::runtime::daemon::daemon_version(pid);
    match version.as_deref() {
        Some(version) => format!("pid {pid}, version {version}"),
        None => format!("pid {pid}, version unknown"),
    }
}

fn daemon_status_line(pid: u32, version: Option<&str>) -> String {
    format!(
        "running(pid {pid}, version {})",
        version.unwrap_or("unknown")
    )
}

/// CLI 侧唯一的 registry 入口。与 daemon(`boot.rs`)统一成 tolerant 语义:目录本身读不到才整体
/// 报错;单份 agent.yaml 坏掉只隔离该 agent,其余命令照常可用。
///
/// fail-closed 仍成立:坏 agent 压根不进 registry,所有调用方都是 `reg.get(name)` + `?`,
/// 查不到即报「未知 agent」而不是拿到一个「名单空 = 全放行」的半解析配置。
/// 诊断走 stderr —— `agents status --json` 等命令的 stdout 是机器契约,不能掺人话。
/// `schedules mv` 的目标校验:接手的 agent 必须在 registry 里。
///
/// 抽成独立函数是为了能直接测。这条校验是本命令相对**手改 `schedules.yaml`** 的核心价值:
/// scheduler 查不到目标 agent 的 delivery scope 时**跳过这个 tick 而不报错**
/// (见 `daemon::scheduler::DeliveryScopes` 的注释),定时器就成了不响也不报的哑弹 ——
/// 而「不响」通常要等到用户发现某份日报没来才暴露。宁可在交接这一刻拒掉。
fn ensure_move_target_is_known(registry: &Registry, agent: &str) -> anyhow::Result<()> {
    // 判据是 `is_routable()` 而不是「registry 里有」:scheduler 的投递身份表来自
    // `Registry::routable()`(status=active 且配了 feishu.app_id_env)。只查存在的话,
    // paused 或没配飞书凭据的 agent 会通过校验,然后在 tick 里被静默跳过 ——
    // 正是这条错误文案承诺挡住的后果。
    if registry.get(agent).filter(|a| a.is_routable()).is_none() {
        anyhow::bail!(summon_i18n::format!("r9799f0cdb3"));
    }
    Ok(())
}

pub(crate) fn load_reg(cfg: &Config) -> anyhow::Result<Registry> {
    let (registry, warnings) = Registry::load_tolerant(&cfg.agents_dir)
        .with_context(|| summon_i18n::format!("rc4cc94be3c", cfg.agents_dir.display()))?;
    for warning in &warnings {
        eprintln!("{}", summon_i18n::format!("r357d15167c"));
    }
    Ok(registry.with_global_config_path(&summon_core::config::config_path()))
}

/// registry 里所有 agent 的活 tmux 会话。
///
/// **一次 tmux 调用**,再本地按各 agent 前缀筛。此前是每个 agent 各 spawn 一次 tmux —— 拿的还是
/// 同一份全量列表,5 个 agent 就是 5 次进程创建(≈32ms),而 Menu Bar 每次弹出都要等这一趟。
fn live_sessions_for_agents(reg: &Registry) -> Vec<String> {
    let all = crate::session::tmux::list_all_sessions();
    let mut sessions: Vec<String> = reg
        .agents
        .iter()
        .flat_map(|agent| crate::session::tmux::filter_names(&all, &agent.tmux_name()))
        .collect();
    sessions.sort();
    sessions.dedup();
    sessions
}

// `.meta` 的 schema/解析在 summon-core::chat_meta(A4 收口);cli 侧只剩文件 IO。
use crate::session::discovery::{chat_tag_for_session, discover_sessions, event_log_tags};
#[cfg(test)]
use crate::session::discovery::{parse_meta_updated_at, read_chat_meta, session_last_active};
use summon_core::chat_meta::ChatMeta;

enum AttachTarget<'a> {
    Agent {
        agent: &'a summon_core::registry::Agent,
        session: String,
    },
    /// 直指 chat_id/tag 命中的会话(可能无头/gone)。带 `SessionView` 让调用方按 capture/liveness
    /// 分流:claude live→tmux attach、无头→降级 tail、Transcript+Gone→明确报已退出(C18)。
    Chat {
        view: summon_core::session_view::SessionView,
    },
}

/// 给 `--chat <tag>` 的候选 tag 配上人读名字(群名 / 私聊),让用户不必猜 tag 对应哪个群。
/// 纯格式化(便于单测):有群名 → `「名」`;群无名 → `(群,无名)`;私聊 → `(私聊)`;无 .meta → 提示。
fn annotate_tag_line(tag: &str, meta: Option<&ChatMeta>) -> String {
    let label = match meta {
        Some(m) => match m
            .chat_name
            .as_deref()
            .map(str::trim)
            .filter(|n| !n.is_empty())
        {
            Some(name) => format!("「{name}」"),
            None if m.chat_type.as_deref() == Some("p2p") => {
                summon_i18n::text!("r896a0a49ee").to_string()
            }
            None => summon_i18n::text!("r7ab8c0d92f").to_string(),
        },
        None => summon_i18n::text!("r75774b8ae2").to_string(),
    };
    format!("{tag}  {label}")
}

/// 扫 `runtime/chats/*.meta`,建 tag → ChatMeta 索引(tail 多流报错时给 tag 配名字用)。
fn chat_meta_by_tag(
    agent: &summon_core::registry::Agent,
) -> std::collections::HashMap<String, ChatMeta> {
    let base = agent.tmux_name();
    let mut out = std::collections::HashMap::new();
    let Ok(entries) = std::fs::read_dir(agent.state_dir().join("runtime").join("chats")) else {
        return out;
    };
    for e in entries.flatten() {
        let p = e.path();
        let Some(session) = p
            .file_name()
            .and_then(|s| s.to_str())
            .and_then(|s| s.strip_suffix(".meta"))
        else {
            continue;
        };
        let Ok(content) = std::fs::read_to_string(&p) else {
            continue;
        };
        let meta = ChatMeta::parse(&content);
        out.insert(chat_tag_for_session(&base, session, &meta), meta);
    }
    out
}

/// 把候选 tag 列表配上名字,组装成多行提示(tail 多流报错 / 列可 tail 会话共用)。
fn tag_choices_hint(agent: &summon_core::registry::Agent, tags: &[String]) -> String {
    let metas = chat_meta_by_tag(agent);
    tags.iter()
        .map(|t| annotate_tag_line(t, metas.get(t)))
        .collect::<Vec<_>>()
        .join("\n  ")
}

/// 会话块尾行给哪条指令 —— 按**调用视角**分流。会话块是 `agents ls` 与 kill 选择器共用的渲染,
/// 尾行若恒是 attach/tail,kill 选择器就在教用户反向操作(照抄反而接管了本想关掉的会话)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChatCmd {
    /// `agents ls` / attach 选择器:跟随会话。
    Follow,
    /// kill 选择器:直接寻址这条会话的关闭指令。`reset` 透传,照抄尾行等价本次调用。
    Kill { reset: bool },
}

/// 渲染尾行指令。Follow:claude/Transcript 可 attach,无头只能 tail;tag 是裸名会话时省 --chat。
/// Kill:直指**会话名**而非 tag —— 同一个群里两个 agent 的 tag 相同(tag 由 chat_id 哈希而来),
/// 拿 tag 当 kill 目标会命中所有同群 agent 的会话;会话名带 agent 前缀,唯一且裸名会话也可寻址。
fn chat_cmd(
    agent: &summon_core::registry::Agent,
    view: &summon_core::session_view::SessionView,
    kind: ChatCmd,
) -> String {
    match kind {
        ChatCmd::Follow => {
            let verb = match view.capture {
                summon_core::registry::CaptureMode::Transcript => "attach",
                summon_core::registry::CaptureMode::CliSession
                | summon_core::registry::CaptureMode::Rpc => "tail",
            };
            if view.tag == "(base/owner)" {
                format!("summon agents {verb} {}", agent.name)
            } else {
                format!("summon agents {verb} {} --chat {}", agent.name, view.tag)
            }
        }
        ChatCmd::Kill { reset } => kill_cmd(&view.session, reset),
    }
}

/// kill 指令文本。**目标只用会话名(绝对名)**——tag/chat_id 是相对名,跨 agent 会撞,
/// 建议出去的指令若用它们,照抄会被 `resolve_session_target` 判歧义。凡对外建议 kill 的地方
/// 都走这里,别再各自拼字符串。
fn kill_cmd(session: &str, reset: bool) -> String {
    let flag = if reset { " --reset" } else { "" };
    format!("summon agents kill {session}{flag}")
}

/// claude 残骸(tmux 已死)的 attach 报错正文:明确报已退出 + 指路清理。
fn gone_session_hint(view: &summon_core::session_view::SessionView) -> String {
    summon_i18n::format!("re0266324e4", view.session, kill_cmd(&view.session, true))
}

fn chat_name_from_view(view: &summon_core::session_view::SessionView) -> &str {
    match (view.chat_name.as_deref(), view.chat_type.as_deref()) {
        (Some(name), _) => name,
        (None, Some("p2p")) => summon_i18n::text!("r6add2714c4"),
        _ => "-",
    }
}

/// 会话表的列头。`numbered` 时首列是选择序号(只有 kill 交互多选需要)。
/// CHAT 封顶 24 显示列 —— 飞书群名无长度上界,不封顶一个长群名能把总宽拉到 200+。
fn session_table(numbered: bool) -> table::Table {
    let (headers, chat_col) = if numbered {
        (
            vec!["#", "AGENT", "STATE", "CLI", "CAP", "CHAT", "TYPE", "LAST"],
            5,
        )
    } else {
        (
            vec!["AGENT", "STATE", "CLI", "CAP", "CHAT", "TYPE", "LAST"],
            4,
        )
    };
    table::Table::new(headers).cap(chat_col, 24)
}

/// 一条会话 → 表格行:定长字段一行 + 可照抄指令跨列排在下面。`kind` 决定指令是跟随还是关闭。
///
/// STATE 取**会话级** liveness 而非 agent 级的 live/-:扁平表里一行就是一条会话,用 agent 级
/// 会把 recent/stale 这类无头状态抹平。CAP 是 agent 级 capture/manual(配置属性);会话能不能
/// attach 由续行的指令本身表达(attach vs tail),不再单列一列。
fn session_row(
    agent: &summon_core::registry::Agent,
    view: &summon_core::session_view::SessionView,
    now: std::time::SystemTime,
    kind: ChatCmd,
    index: Option<usize>,
) -> table::Row {
    use summon_core::session_view::humanize_since;
    let liveness = view.liveness.label();
    // 无 .meta 的会话(base/owner、threads-only 孤儿)群名/类型全是 `-`,表里就成了认不出主的空行。
    // 回退到 tag —— 它至少是这条会话的身份(`(base/owner)` 或 chat-tag)。
    let chat = match chat_name_from_view(view) {
        "-" => view.tag.as_str(),
        name => name,
    };
    let mut cells = Vec::new();
    if let Some(i) = index {
        cells.push(table::Cell::plain(format!("{i}")));
    }
    cells.extend([
        table::Cell::heading(&agent.name),
        table::Cell::status(liveness, liveness_hue(liveness)),
        table::Cell::plain(agent.cli_kind()),
        table::Cell::plain(capture_label(agent)),
        table::Cell::plain(chat),
        table::Cell::plain(view.chat_type.as_deref().unwrap_or("-")),
        table::Cell::plain(humanize_since(view.last_active, now)),
    ]);
    table::Row::new(cells).with_span(table::Cell::command(chat_cmd(agent, view, kind)))
}

/// 没有会话的 agent 也占一行(fleet 里有它是事实),但**不编造指令** —— 它没有可寻址的会话,
/// 给一条 `attach <name>` 是猜测(该 agent 未必 Transcript 捕获、base 未必该起)。
fn agent_only_row(agent: &summon_core::registry::Agent, numbered: bool) -> table::Row {
    let mut cells = Vec::new();
    if numbered {
        cells.push(table::Cell::plain(""));
    }
    cells.extend([
        table::Cell::heading(&agent.name),
        table::Cell::label("-"),
        table::Cell::plain(agent.cli_kind()),
        table::Cell::plain(capture_label(agent)),
        table::Cell::label("-"),
        table::Cell::label("-"),
        table::Cell::label("-"),
    ]);
    table::Row::new(cells)
}

fn capture_label(agent: &summon_core::registry::Agent) -> &'static str {
    if agent.captures() {
        "capture"
    } else {
        "manual"
    }
}

/// 某 agent 的全量会话(含无头)渲染成一张表;无会话返回 None。
fn chat_table(agent: &summon_core::registry::Agent, sessions: &[String]) -> Option<String> {
    let now = std::time::SystemTime::now();
    let ttl = crate::runtime::daemon::resolve_messaging(agent).idle_ttl();
    let mut t = session_table(false);
    for view in discover_sessions(agent, sessions, now, ttl) {
        t.push(session_row(agent, &view, now, ChatCmd::Follow, None));
    }
    (!t.is_empty()).then(|| t.render())
}

fn parse_multi_select(input: &str, max: usize) -> anyhow::Result<Vec<usize>> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }
    if trimmed.eq_ignore_ascii_case("all") {
        return Ok((0..max).collect());
    }

    let mut selected = BTreeSet::new();
    for token in trimmed.split([',', ' ', '\t']) {
        if token.is_empty() {
            continue;
        }
        let idx: usize = token
            .parse()
            .with_context(|| summon_i18n::format!("r60ec5c4b18"))?;
        if idx == 0 || idx > max {
            anyhow::bail!(summon_i18n::format!("r535248be60"));
        }
        selected.insert(idx - 1);
    }
    Ok(selected.into_iter().collect())
}

/// 列出全 registry 的 agent + 会话。`kind` 决定每条会话尾行给什么指令 —— 从 kill 的
/// 「未找到目标」路进来时给 kill,别在 kill 的报错里教 attach。
fn format_agent_choices(
    reg: &Registry,
    live_sessions: &[String],
    all: bool,
    kind: ChatCmd,
) -> String {
    if reg.agents.is_empty() {
        return summon_i18n::text!("r259b4d095f").to_string();
    }

    let now = std::time::SystemTime::now();
    let mut t = session_table(false);
    // C16:是否列出过任一会话 —— 全空时给明确「无活跃会话」提示(裸 ls 删 early-return 后靠此判空态)。
    let mut any_shown = false;
    // stale 折叠提示汇总到表格底部:per-agent 的提示行会在网格中间插非表格文本、把对齐撞散,
    // 而提示的唯一动作就是「加 --all」,不需要知道是哪个 agent。
    let mut hidden_total = 0usize;
    for agent in &reg.agents {
        let ttl = crate::runtime::daemon::resolve_messaging(agent).idle_ttl();
        let all_views = discover_sessions(agent, live_sessions, now, ttl);
        // Gone 永不展示(即便 --all);默认只列活跃,stale 折叠。
        hidden_total += all_views
            .iter()
            .filter(|v| v.liveness.should_display(true) && !v.liveness.should_display(false))
            .count();
        let views: Vec<_> = all_views
            .into_iter()
            .filter(|v| v.liveness.should_display(all))
            .collect();
        if views.is_empty() {
            t.push(agent_only_row(agent, false));
            continue;
        }
        any_shown = true;
        for view in &views {
            t.push(session_row(agent, view, now, kind, None));
        }
    }
    let mut out = format!(
        "{}\n{}",
        style::heading(summon_i18n::text!("r533d262fd6")),
        t.render()
    );
    // 一个会话都没列出(纯无头无 recent、或全是 stale/gone 被折叠)→ 明确报无活跃,别只留一排 agent 名。
    if !any_shown {
        out.push_str(&format!(
            "\n{}",
            style::label(summon_i18n::text!("rc1f158fc12"))
        ));
    }
    let hint = hidden_hint(hidden_total, all);
    if !hint.is_empty() {
        out.push_str(&format!("\n{}", style::label(&hint)));
    }
    out
}

fn format_chat_choices(agent: &summon_core::registry::Agent, sessions: &[String]) -> String {
    match chat_table(agent, sessions) {
        Some(t) => format!("{}\n{t}", style::heading(summon_i18n::text!("rc188b2173e"))),
        None => summon_i18n::format!("r6129dc4d83", agent.name),
    }
}

fn unknown_agent_message(reg: &Registry, name: &str, live_sessions: &[String]) -> String {
    summon_i18n::format!(
        "r39f3799fc4",
        format_agent_choices(reg, live_sessions, true, ChatCmd::Follow)
    )
}

fn missing_chat_message(
    agent: &summon_core::registry::Agent,
    tag: &str,
    sessions: &[String],
) -> String {
    summon_i18n::format!(
        "rf37ded756d",
        agent.tmux_name(),
        format_chat_choices(agent, sessions)
    )
}

/// 按 chat-tag 在该 agent 的已发现会话里反查真实 tmux 会话名。owner 私聊会话用裸名
/// `soul-<alias>`(向后兼容)而 `.meta.tag` 是 `p<8hex>`,故 `attach/tail --chat <tag>` 不能naively
/// 拼 `<base>-<tag>`;走与 `agents ls` 同一套 `discover_sessions` 反查,保证 ls 建议的命令真能命中。
/// 查不到(会话从未起过、无 .meta)返回 None,调用方回退 `<base>-<tag>`。
fn session_name_for_tag(
    agent: &summon_core::registry::Agent,
    tag: &str,
    live_sessions: &[String],
) -> Option<String> {
    let now = std::time::SystemTime::now();
    let ttl = crate::runtime::daemon::resolve_messaging(agent).idle_ttl();
    discover_sessions(agent, live_sessions, now, ttl)
        .into_iter()
        .find(|v| v.tag == tag)
        .map(|v| v.session)
}

fn resolve_attach_target<'a>(
    reg: &'a Registry,
    target: &str,
    chat: Option<&str>,
    live_sessions: &[String],
) -> anyhow::Result<AttachTarget<'a>> {
    if let Some(agent) = reg.get(target) {
        let session = match chat {
            // owner 私聊会话用裸名 `soul-<alias>`,但其 `.meta.tag` 仍是 `p<8hex>`(ls 据此显示+建议
            // `--chat <tag>`)。故不能naively拼 `<base>-<tag>`:先经与 `agents ls` 同一套
            // `discover_sessions` 按 tag 反查真实 tmux 名(含裸名),查不到再回退 `<base>-<tag>`
            //(会话未起 / 将新建)。这样 ls 建议的命令真能 attach 上。
            Some(tag) => session_name_for_tag(agent, tag, live_sessions)
                .unwrap_or_else(|| format!("{}-{tag}", agent.tmux_name())),
            None => agent.tmux_name(),
        };
        return Ok(AttachTarget::Agent { agent, session });
    }

    if chat.is_some() {
        anyhow::bail!(summon_i18n::format!(
            "rf0e2c1b248",
            format_agent_choices(reg, live_sessions, true, ChatCmd::Follow)
        ));
    }

    // C18:直指 chat_id/tag/session 的匹配集换成 discover_sessions 全集(与 ls/kill 同源),
    // 无头会话不再因 tmux-only 匹配而恒 miss;命中的 SessionView 交由 attach 按 capture/liveness 分流。
    match resolve_session_target(reg, live_sessions, target) {
        Resolved::Unique(view) => Ok(AttachTarget::Chat { view: *view }),
        Resolved::NotFound => anyhow::bail!(summon_i18n::format!(
            "rb85be46db8",
            format_agent_choices(reg, live_sessions, true, ChatCmd::Follow)
        )),
        // 歧义只列命中的候选:旧文案糊上全 fleet,而用户要的是「这几条里选一条」。
        Resolved::Ambiguous(c) => {
            anyhow::bail!("{}", ambiguous_message(reg, target, &c, ChatCmd::Follow))
        }
    }
}

/// 解析失败时的补充提示:形如 `summon agents <verb> …` 但需要的 <agent>/<target> 没给(或给了未知名)时,
/// 列出可选项 —— attach 已带某已知 agent 名(多半是 `--chat` 缺值报错)就列该 agent 的 chats,
/// 否则列全部活跃 agents + chat。其它命令/错误返回 None,交回 clap 原始报错。
fn agents_parse_error_hint(args: &[String]) -> Option<String> {
    if args.get(1).map(String::as_str) != Some("agents") {
        return None;
    }
    let verb = args.get(2).map(String::as_str)?;
    if !matches!(
        verb,
        "up" | "down" | "attach" | "kill" | "activate" | "tail"
    ) {
        return None;
    }

    let cfg = Config::load();
    let reg = load_reg(&cfg).ok()?;
    let agent_name = agents_positional(args);
    match agent_name.and_then(|name| reg.get(name)) {
        Some(agent) if verb == "attach" => {
            let sessions = crate::session::tmux::list_sessions_with_prefix(&agent.tmux_name());
            Some(format_chat_choices(agent, &sessions))
        }
        _ => {
            let live_sessions = live_sessions_for_agents(&reg);
            Some(format_agent_choices(
                &reg,
                &live_sessions,
                true,
                ChatCmd::Follow,
            ))
        }
    }
}

/// 取 `summon agents <verb>` 之后第一个非 flag 实参(`--chat` 带值,跳两个);用于错误提示里认出 agent 名。
fn agents_positional(args: &[String]) -> Option<&str> {
    let mut i = 3;
    while i < args.len() {
        let arg = args[i].as_str();
        if arg == "--chat" {
            i += 2;
        } else if arg.starts_with('-') {
            i += 1;
        } else {
            return Some(arg);
        }
    }
    None
}

fn agents_cmd(cfg: &Config, action: Option<AgentsAction>) -> anyhow::Result<()> {
    match action {
        Some(AgentsAction::Disable { agent }) => set_agent_enabled(cfg, &agent, false),
        Some(AgentsAction::Enable { agent }) => set_agent_enabled(cfg, &agent, true),
        // 裸 `summon agents` → 展示可后缀的子命令(ls/up/down/attach/kill/activate);列表走 `agents ls`。
        None => {
            use clap::CommandFactory;
            let mut root = Cli::command();
            if let Some(sub) = root.find_subcommand_mut("agents") {
                let _ = sub.print_help();
                println!();
            }
            Ok(())
        }
        Some(AgentsAction::Ls { agent, all }) => ls(cfg, agent.as_deref(), all),
        Some(AgentsAction::Add {
            alias,
            file,
            force,
            display,
            role,
            cli,
            model,
            effort,
            workspace,
        }) => crate::setup::agents_add::run(
            cfg,
            crate::setup::agents_add::AddArgs {
                alias,
                file,
                force,
                display,
                role,
                cli,
                model,
                effort,
                workspace,
            },
        ),
        Some(AgentsAction::Up { agent }) => up(cfg, &agent),
        Some(AgentsAction::Down { agent, chat, reset }) => {
            down(cfg, &agent, chat.as_deref(), reset)
        }
        Some(AgentsAction::Attach {
            target,
            chat,
            from_start,
        }) => attach(cfg, target.as_deref(), chat.as_deref(), from_start),
        Some(AgentsAction::Kill { target, reset }) => agents_kill(cfg, &target, reset),
        Some(AgentsAction::Status { json }) => agents_status(cfg, json),
        Some(AgentsAction::History {
            target,
            limit,
            before,
            json,
        }) => agents_history(cfg, &target, limit, before, json),
        Some(AgentsAction::Open { target }) => agents_open(cfg, &target),
        Some(AgentsAction::Activate { agent }) => agents_activate(cfg, &agent),
        Some(AgentsAction::Reload) => agents_reload().map(drop),
        Some(AgentsAction::Model { agent, model }) => {
            agents_config(cfg, AgentConfigField::Model, &agent, model.as_deref())
        }
        Some(AgentsAction::Effort { agent, effort }) => {
            agents_config(cfg, AgentConfigField::Effort, &agent, effort.as_deref())
        }
        Some(AgentsAction::Tail {
            agent,
            chat,
            from_start,
        }) => agents_tail(cfg, &agent, chat.as_deref(), from_start),
    }
}

/// `summon agents reload`:让 daemon 扫盘接新 agent 上线,免 `daemon restart`。
/// daemon 不在 → 不是错误,直接提示「起 daemon 即可」——它启动时本就会扫全盘。
/// 让 daemon 扫盘接新建的 agent,返回这次新上线的 alias。
pub(crate) fn agents_reload() -> anyhow::Result<Vec<String>> {
    use summon_core::core_port::{Actor, Command, CommandOutcome, Intent};
    if crate::runtime::daemon::running_pid().is_none() {
        println!("{}", summon_i18n::format!("rcb12443d66"));
        return Ok(Vec::new());
    }
    let cmd = Command {
        actor: Actor::Owner,
        intent: Intent::ReloadAgents,
    };
    match crate::runtime::daemon::submit_command(&cmd)? {
        CommandOutcome::Value(v) => {
            let admitted: Vec<String> = v
                .get("admitted")
                .and_then(|a| serde_json::from_value(a.clone()).ok())
                .unwrap_or_default();
            if admitted.is_empty() {
                println!("{}", summon_i18n::format!("r797c45966b"));
            } else {
                println!(
                    "{}",
                    summon_i18n::format!("r11b3ef7dca", admitted.join(", "))
                );
                // sidecar 没在跑 = 新 agent 的飞书长连接建不起来,必须让用户知道,别以为好了。
                if v.get("sidecar_restarted").and_then(|b| b.as_bool()) == Some(false) {
                    println!("{}", summon_i18n::format!("r72896108c4"));
                } else {
                    println!("{}", summon_i18n::format!("r9a0bbd0655"));
                }
            }
            Ok(admitted)
        }
        CommandOutcome::Denied(why) => anyhow::bail!("{why}"),
        other => anyhow::bail!(summon_i18n::format!("r111b0ce5b5")),
    }
}

/// 一个刚写好凭据、可路由的 agent 怎么上线:daemon 没跑就起(启动时扫全盘);在跑就热加载,
/// 不打断别的 agent 正在跑的轮。热加载只接 daemon 还不认识的 agent —— 已经加载过的 alias
/// 换了凭据,热加载换不掉内存里那份,如实告诉人要重启。
pub(crate) fn bring_agent_online(alias: &str) -> anyhow::Result<()> {
    if crate::runtime::daemon::running_pid().is_none() {
        println!("{}", summon_i18n::format!("r42f02ab10b"));
        return daemon_restart();
    }
    println!("{}", summon_i18n::format!("rd554e5610e"));
    let admitted = agents_reload()?;
    if already_loaded_before_reload(alias, &admitted) {
        println!("{}", summon_i18n::format!("rd63eaec226"));
    }
    Ok(())
}

/// 热加载后 alias 不在新上线名单里 = daemon 之前就认识它。
fn already_loaded_before_reload(alias: &str, admitted: &[String]) -> bool {
    !admitted.iter().any(|name| name == alias)
}

/// 本机命令面能改的 agent 常驻配置项。model 与 effort 的流程完全同构(查 agent → 校验 →
/// 写 agent.yaml),差异只在「校哪个轴 / 发哪个 Intent / 调哪个写函数」这三处,故收进枚举集中
/// 分派,而不是复制两份命令实现。新增一项(如 working_dir)挂这里即可。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AgentConfigField {
    Model,
    Effort,
}

impl AgentConfigField {
    fn label(self) -> &'static str {
        match self {
            Self::Model => summon_i18n::text!("rc98e118e0a"),
            Self::Effort => "effort",
        }
    }

    /// 该字段在 agent.yaml 里的当前值(bot 默认;本机命令只改这一层)。
    fn current(self, agent: &summon_core::registry::Agent) -> Option<String> {
        let default = crate::runtime::chat_model::effective(agent, None);
        match self {
            Self::Model => default.model,
            Self::Effort => default.effort,
        }
    }

    /// 该 agent 的 cli 上,这个字段的候选(model 走 per-cli 桶,effort 走 messaging ∩ legal)。
    fn candidates(self, opts: summon_core::model_effort::ModelEffortOptions) -> Vec<String> {
        match self {
            Self::Model => opts.models,
            Self::Effort => opts.efforts,
        }
    }

    /// fail-closed 校验:改一个轴时另一个轴取 agent 现值,保证落盘后的组合整体合法。
    /// 与飞书卡片/Core 同一校验源(`model_effort::validate`),IO 层不复制 CLI 知识。
    fn validate(self, agent: &summon_core::registry::Agent, value: &str) -> anyhow::Result<()> {
        let cli = agent.cli_kind();
        let default = crate::runtime::chat_model::effective(agent, None);
        let r = match self {
            Self::Model => {
                summon_core::model_effort::check_model_id(value)?;
                summon_core::model_effort::validate(cli, Some(value), default.effort())
            }
            Self::Effort => summon_core::model_effort::validate(cli, default.model(), Some(value)),
        };
        r.map_err(|e| anyhow::anyhow!("{e}"))
    }

    fn intent(self, alias: &str, value: &str) -> summon_core::core_port::Intent {
        use summon_core::core_port::Intent;
        match self {
            Self::Model => Intent::SetModel {
                alias: alias.to_string(),
                model: value.to_string(),
            },
            Self::Effort => Intent::SetEffort {
                alias: alias.to_string(),
                effort: value.to_string(),
            },
        }
    }

    fn write(self, manifest: &std::path::Path, value: &str) -> anyhow::Result<()> {
        match self {
            Self::Model => crate::setup::bind::set_model(manifest, value),
            Self::Effort => crate::setup::bind::set_effort(manifest, value),
        }
    }
}

/// `summon agents model|effort <alias> [值]`:不给值 = 只读(打印当前值 + 候选);给了值 = 写。
///
/// 写路径按 daemon 在不在分两条,**语义相同、都是改 agent.yaml**:
/// - daemon 在跑 → 发 `Intent::SetModel/SetEffort` 让 Core 写。必须走它,因为 Core 还要同步
///   自己那份 in-memory agent 快照,否则 `agents status` / 面板的 5s 轮询会把界面刷回旧值。
/// - daemon 不在 → CLI 自己校验+写盘。盘是唯一真相源,daemon 下次启动自然读到,这是降级不是错误
///   (与 `agents status` 在 daemon 不在时降级为盘上事实同一个哲学)。
fn agents_config(
    cfg: &Config,
    field: AgentConfigField,
    alias: &str,
    value: Option<&str>,
) -> anyhow::Result<()> {
    let reg = load_reg(cfg)?;
    let agent = reg
        .agents
        .iter()
        .find(|a| a.name == alias)
        .with_context(|| summon_i18n::format!("r29c6290f80"))?;
    let cli = agent.cli_kind();
    let Some(value) = value else {
        let opts = summon_core::model_effort::options_for(
            &crate::runtime::daemon::resolve_messaging(agent),
            cli,
        );
        let candidates = field.candidates(opts);
        println!(
            "{}",
            summon_i18n::format!(
                "r4b6b69fee2",
                field.label(),
                field
                    .current(agent)
                    .unwrap_or_else(|| summon_i18n::format!("r742646a848"))
            )
        );
        if candidates.is_empty() {
            println!("{}", summon_i18n::format!("r286b139997"));
        } else {
            println!(
                "{}",
                summon_i18n::format!("r4831a948e6", candidates.join(" / "))
            );
        }
        return Ok(());
    };
    // daemon 在跑就必须让它写:Core 同时更新 in-memory 快照,否则轮询会把界面刷回旧值。
    if crate::runtime::daemon::running_pid().is_some() {
        use summon_core::core_port::{Actor, Command, CommandOutcome};
        let cmd = Command {
            actor: Actor::Owner,
            intent: field.intent(alias, value),
        };
        match crate::runtime::daemon::submit_command(&cmd)? {
            CommandOutcome::Done => {
                println!(
                    "{}",
                    summon_i18n::format!(
                        "r0a96ed65dc",
                        field.label(),
                        config_field_effect(field, agent)
                    )
                );
                return Ok(());
            }
            CommandOutcome::Denied(why) => anyhow::bail!("{why}"),
            other => anyhow::bail!(summon_i18n::format!("r111b0ce5b5")),
        }
    }
    // daemon 不在:自己校验+写盘(fail-closed 同一套校验源)。
    field.validate(agent, value)?;
    field.write(&agent.manifest_path(), value)?;
    println!("{}", summon_i18n::format!("rda04ad7b3d", field.label()));
    Ok(())
}

/// `agents model / effort` 写进 agent.yaml 之后,在跑的会话什么时候用上它。按采集通道分:claude
/// 的 model / effort 是启动参数,model 由 daemon 往在线会话注入 `/model` 就地切,effort 等冷启;
/// Rpc 通道的 cursor 用会话面板另存的那份模型,不读这个值。
fn config_field_effect(
    field: AgentConfigField,
    agent: &summon_core::registry::Agent,
) -> &'static str {
    use summon_core::registry::CaptureMode;
    match (agent.capture_mode(), field) {
        (CaptureMode::Transcript, AgentConfigField::Model) => {
            summon_i18n::text!("r84a6cd6e3f")
        }
        (CaptureMode::Transcript, AgentConfigField::Effort) => {
            summon_i18n::text!("r84c1d33199")
        }
        (CaptureMode::Rpc, _) if agent.cli_kind() == "cursor" => {
            summon_i18n::text!("r2e8cb5858f")
        }
        (CaptureMode::Rpc | CaptureMode::CliSession, _) => summon_i18n::text!("r776bde9752"),
    }
}

/// `summon ask`:chat 轮内阻塞提问。身份走 `load_current_turn_context`(每轮随机凭证 +
/// env 三件套交叉校验),问题数组由 agent 给 JSON。阻塞到人提交/全部跳过,答案打 stdout。
///
/// 答案与题目的配对在**这里**做:daemon 只回一个按题序排列的答案数组(未答为 null),
/// 题面本来就是本进程传出去的,没必要让它绕一圈回来。
fn ask_cmd(cfg: &Config, turn_id: &str, questions_json: &str) -> anyhow::Result<()> {
    use summon_core::core_port::{AskAnswer, AskQuestion, Command, CommandOutcome, Intent};
    let questions: Vec<AskQuestion> =
        serde_json::from_str(questions_json).context(summon_i18n::text!("rd30f22fa42"))?;
    if questions.is_empty() {
        anyhow::bail!(summon_i18n::format!("r63ca0c5ad2"));
    }
    let ctx = load_current_turn_context(cfg, turn_id)?;
    ensure_turn_can_ask(&ctx)?;
    let cmd = Command {
        actor: summon_core::core_port::Actor::ChatAgent(Box::new(
            summon_core::core_port::ChatAgentIdentity {
                turn_id: ctx.turn_id.clone(),
                alias: ctx.agent_alias.clone(),
                requester_open_id: ctx.requester_open_id.clone(),
                source_ref: summon_core::MsgRef {
                    delivery_scope: ctx.delivery_scope.clone(),
                    chat_id: ctx.chat_id.clone(),
                    chat_type: ctx.chat_type.clone(),
                    message_id: ctx.message_id.clone(),
                    thread_id: ctx.thread_id.clone(),
                    root_id: ctx.root_id.clone(),
                },
                chat_type: ctx.chat_type.clone(),
                conversation_tag: ctx.conversation_tag.clone(),
            },
        )),
        intent: Intent::ChatAsk {
            turn_id: ctx.turn_id.clone(),
            alias: ctx.agent_alias.clone(),
            conversation_tag: ctx.conversation_tag.clone(),
            questions: questions.clone(),
        },
    };
    // 阻塞在这一句上:control.sock handler 收到 `Blocked` 后会在 daemon 侧就地 `wait_gate`
    // (boot.rs),拿到裁决才回写。所以这里等到的已经是最终答案,CLI 不自己管闸门。
    // 闸门裁决统一裹在 `{"answer": …}` 里(`Resolution::into_outcome`),这里的 answer 是
    // 按题序排列的答案数组 JSON 串。
    let raw = match crate::runtime::daemon::submit_command(&cmd)? {
        CommandOutcome::Value(v) => v
            .get("answer")
            .and_then(|a| a.as_str())
            .unwrap_or("[]")
            .to_string(),
        CommandOutcome::Denied(why) => anyhow::bail!("{why}"),
        other => anyhow::bail!(summon_i18n::format!("r111b0ce5b5")),
    };
    let picked: Vec<Option<String>> = serde_json::from_str(&raw).unwrap_or_default();
    let answers: Vec<AskAnswer> = questions
        .iter()
        .enumerate()
        .map(|(i, q)| AskAnswer {
            question: q.question.clone(),
            answer: picked.get(i).cloned().flatten(),
        })
        .collect();
    println!("{}", serde_json::json!({ "answers": answers }));
    Ok(())
}

/// `--when` 的解析期校验。
///
/// **为什么要在这一层而不是等到命令体里**:命令体第一件事是拿 turn 凭证向 daemon 复验身份,
/// 那之后才轮到节律。于是「节律写错」这个纯语法问题,要么被前面的鉴权错误盖住,要么白跑一趟
/// 网络往返。调用方是 agent,它靠错误消息学会怎么用 —— 让它当场拿到「合法形态有哪些」,
/// 比让它去猜、或者多跑一轮 `--help` 都强。
///
/// 用 `Local::now()` 只为了让 `in`/`at` 能算出时刻并做「已经过去了」这类判定;
/// 校验通过后原样返回输入,真正落盘的 spec 仍由命令体用同一个 `parse_when` 现算。
fn validate_when(raw: &str) -> Result<String, String> {
    let tokens: Vec<&str> = raw.split_whitespace().collect();
    let parsed = summon_core::schedule::parse_when(&tokens, chrono::Local::now())?;
    if parsed.consumed != tokens.len() {
        return Err(summon_i18n::format!(
            "rb0b2086323",
            &tokens[parsed.consumed..]
        ));
    }
    Ok(raw.to_string())
}

/// 定时轮建定时器 = agent 自续,有上限。
///
/// 一个内容里含「定个提醒」的定时器每次触发都会再建一个,没有上限就是自繁殖:表一直涨、
/// 群里一直响。owner 2026-10-12 授权放开,上限是连续 [`MAX_UNANSWERED_FOLLOWUPS`] 轮没有
/// 人类消息 —— 用户一回话计数清零。计数见 `runtime::self_followup`。
///
/// [`MAX_UNANSWERED_FOLLOWUPS`]: crate::runtime::self_followup::MAX_UNANSWERED_FOLLOWUPS
fn ensure_turn_can_remind(
    context: &crate::runtime::current_turn_store::CurrentTurnContext,
    unanswered: u32,
) -> anyhow::Result<()> {
    let limit = crate::runtime::self_followup::MAX_UNANSWERED_FOLLOWUPS;
    if context.origin == crate::runtime::current_turn_store::CurrentTurnOrigin::Schedule
        && unanswered >= limit
    {
        anyhow::bail!(summon_i18n::format!("rc7e80fcb36"));
    }
    Ok(())
}

/// 定时轮没有发起人;它建的定时器归到触发它的那个定时器的创建者名下,归属链不断。
fn schedule_creator_of_turn(
    store: &crate::schedule::schedule_store::ScheduleStore,
    fire_id: &str,
) -> Option<String> {
    let id = fire_id.strip_prefix("sched-")?.rsplit_once('-')?.0;
    store
        .load()
        .ok()?
        .into_iter()
        .find(|schedule| schedule.id == id)
        .map(|schedule| schedule.created_by)
}

fn ensure_turn_can_ask(
    context: &crate::runtime::current_turn_store::CurrentTurnContext,
) -> anyhow::Result<()> {
    if context.origin == crate::runtime::current_turn_store::CurrentTurnOrigin::Schedule {
        anyhow::bail!(summon_i18n::format!("r83dd49fdfb"));
    }
    Ok(())
}

fn ensure_turn_allows_message_output(
    context: &crate::runtime::current_turn_store::CurrentTurnContext,
) -> anyhow::Result<()> {
    if context.origin == crate::runtime::current_turn_store::CurrentTurnOrigin::Schedule
        && context.schedule_output == summon_core::ScheduleOutput::Result
    {
        anyhow::bail!(summon_i18n::format!("r5bfe153726"));
    }
    Ok(())
}

fn delegate_cmd(
    cfg: &Config,
    turn_id: &str,
    target: summon_core::session::group_target::GroupTarget,
    content: &str,
    summary: Option<&str>,
    dry_run: bool,
) -> anyhow::Result<()> {
    use summon_core::core_port::{Actor, ChatAgentIdentity, Command, CommandOutcome, Intent};
    let (_, context) = resolve_turn_target(cfg, turn_id)?;
    let content = if content == "-" {
        let mut text = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut text)?;
        text
    } else {
        content.into()
    };
    let command = Command {
        actor: Actor::ChatAgent(Box::new(ChatAgentIdentity {
            turn_id: turn_id.into(),
            alias: context.agent_alias.clone(),
            requester_open_id: context.requester_open_id.clone(),
            source_ref: summon_core::MsgRef {
                delivery_scope: context.delivery_scope.clone(),
                chat_id: context.chat_id.clone(),
                chat_type: context.chat_type.clone(),
                message_id: context.message_id.clone(),
                thread_id: context.thread_id.clone(),
                root_id: context.root_id.clone(),
            },
            chat_type: context.chat_type.clone(),
            conversation_tag: context.conversation_tag.clone(),
        })),
        intent: Intent::DelegateInstructions {
            target,
            summary: summary.map(str::to_owned),
            content,
            dry_run,
        },
    };
    match crate::runtime::daemon::submit_command(&command)? {
        CommandOutcome::Value(value) => {
            if let Some(message) = value.get("message").and_then(|v| v.as_str()) {
                println!("{message}");
            } else {
                println!("{}", serde_json::to_string_pretty(&value)?);
            }
            Ok(())
        }
        CommandOutcome::Denied(reason) => anyhow::bail!(reason),
        other => anyhow::bail!(summon_i18n::format!("rfeab9e4ff7")),
    }
}

/// 主动 CLI(`preview` / `send-image` / `react`)的发送目标解析:**以本轮凭证为准,不认环境变量**。
///
/// ## 为什么不能只信 env
///
/// 这三条命令原先直接读 `SUMMON_AGENT` + `SUMMON_CHAT_ID`,再校验「这个 chat 是否属于这个
/// agent」。那道校验挡不住真正危险的情形:一组**真实但陈旧**的组合(子进程从上一个上下文
/// 继承来的、或持久 RPC/ACP 进程保留的启动期环境)本身自洽,校验照样通过 —— 结果是**以别的
/// bot 的身份、把内容发进别的群**。2026-08-14 实测到这一幕:一个 agent 的轮里执行 preview,
/// 最终以另一个 agent 的身份发到了另一个群。这不是显示错乱,是跨群数据泄露。
///
/// turn 凭证不同:它由 summon 每轮现发、只写在**本轮 prompt** 里,陈旧环境无从得知,过期也
/// 立刻失效。所以身份必须以它为准,env 只用于定位 store(且随后被交叉校验)。
///
/// 复用 `summon ask` 已经在用的那套(`load_current_turn_context` + `validate_current_turn_identity`),
/// 不另起判据 —— 同一个问题两份答案迟早漂移。
fn resolve_turn_target(
    cfg: &Config,
    turn_id: &str,
) -> anyhow::Result<(
    summon_core::registry::Agent,
    crate::runtime::current_turn_store::CurrentTurnContext,
)> {
    // 第一道:盘上 lease + 身份交叉校验(与 `summon ask` 同一套)。
    let context = load_current_turn_context(cfg, turn_id)?;
    // 第二道:**向 daemon 复验这一轮还活在本代内存里**。
    //
    // 只验盘上是不够的:lease 文件是 best-effort 清理的,轮末删除失败,旧凭证就会一直"有效",
    // 一个早已结束的轮仍能往群里发东西。内存表换代即空、轮结束即撤,是唯一说得准的权威。
    //
    // **daemon 不可用时 fail-closed**:拿不到复验 = 没有证据,不放行。这条命令本来就只在
    // daemon 拉起的会话里跑,daemon 不在时它没有正当用途。
    // **以 daemon 返回的为准**,不再用盘上那份 —— 否则 rekey 之后仍按旧位置动作,跨 chat 那条
    // 路就没真正闭合。
    let context = verify_turn_is_live(&context, turn_id)?;
    let registry = load_reg(cfg)?;
    let agent = registry
        .get(&context.agent_alias)
        .with_context(|| summon_i18n::format!("rd1a66e5252", context.agent_alias))?
        .clone();
    Ok((agent, context))
}

/// 向 daemon 复验「这一轮此刻还活着」。拿不到答复一律拒绝(fail-closed)。
fn verify_turn_is_live(
    context: &crate::runtime::current_turn_store::CurrentTurnContext,
    turn_id: &str,
) -> anyhow::Result<crate::runtime::current_turn_store::CurrentTurnContext> {
    use summon_core::core_port::{Actor, ChatAgentIdentity, Command, CommandOutcome, Intent};
    let cmd = Command {
        // **不冒充 Owner**:这是 agent 在轮内发起的动作,不是本机 owner 在终端敲的。冒充会绕开
        // Core 对 alias/凭证一致性的校验,等于把刚建的闸自己拆了。
        actor: Actor::ChatAgent(Box::new(ChatAgentIdentity {
            turn_id: turn_id.to_string(),
            alias: context.agent_alias.clone(),
            requester_open_id: context.requester_open_id.clone(),
            source_ref: summon_core::MsgRef {
                delivery_scope: context.delivery_scope.clone(),
                chat_id: context.chat_id.clone(),
                chat_type: context.chat_type.clone(),
                message_id: context.message_id.clone(),
                thread_id: context.thread_id.clone(),
                root_id: context.root_id.clone(),
            },
            chat_type: context.chat_type.clone(),
            conversation_tag: context.conversation_tag.clone(),
        })),
        intent: Intent::VerifyActiveTurn {
            alias: context.agent_alias.clone(),
            conversation_tag: context.conversation_tag.clone(),
            turn_id: turn_id.to_string(),
        },
    };
    match crate::runtime::daemon::submit_command(&cmd) {
        // **取回权威 context**:盘上那份只是申请材料,位置(chat_id/tag)可能因 rekey 而过时。
        Ok(CommandOutcome::Value(value)) => {
            serde_json::from_value(value).context(summon_i18n::text!("r9f48869716"))
        }
        Ok(CommandOutcome::Denied(reason)) => anyhow::bail!(summon_i18n::format!("r1e2a833baf")),
        Ok(other) => anyhow::bail!(summon_i18n::format!("r98a88d75b4")),
        // 连不上 daemon:不猜、不放行。盘上凭证可能是上一代残留的,而能证否它的只有 daemon。
        Err(error) => {
            anyhow::bail!(summon_i18n::format!("r6418ca8f29"))
        }
    }
}

fn load_current_turn_context(
    cfg: &Config,
    turn_id: &str,
) -> anyhow::Result<crate::runtime::current_turn_store::CurrentTurnContext> {
    if turn_id.trim().is_empty() {
        anyhow::bail!(summon_i18n::format!("rec004294b3"));
    }
    let alias = std::env::var("SUMMON_AGENT")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .context(summon_i18n::text!("r9ea605da8f"))?;
    let registry = load_reg(cfg)?;
    let agent = registry
        .get(&alias)
        .with_context(|| summon_i18n::format!("rd6f559fcf1"))?;
    let store = crate::runtime::current_turn_store::CurrentTurnStore::for_agent(&agent.state_dir());
    let now_ms: i64 = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .context(summon_i18n::text!("r5a77d1a456"))?
        .as_millis()
        .try_into()
        .context(summon_i18n::text!("r06e87706c5"))?;
    let expected_tag = std::env::var("SUMMON_CONVERSATION_TAG")
        .ok()
        .filter(|value| !value.trim().is_empty());
    let expected_chat_id = std::env::var("SUMMON_CHAT_ID")
        .ok()
        .filter(|value| !value.trim().is_empty());
    // **按 turn_id 定位,不按 env 里的位置**。
    //
    // RPC 会话会被 `rekey` 从一个 chat 迁到同 agent 的另一个 chat,而进程环境是 spawn 时固定的、
    // 改不了 —— 迁移之后 `SUMMON_CONVERSATION_TAG` 仍指着 source chat。按它定位,轻则平白找不到
    // 轮,重则定位到旧会话。turn_id 全局唯一,直接拿它找,位置从查到的 context 里取。
    //
    // agent 维度不放松:store 仍由 env 的 alias 选定,跨 agent 的陈旧环境会去错误目录里找、
    // 自然找不到,照旧 fail-closed。
    let context = store.load_by_turn_id(turn_id, now_ms).map_err(|error| {
        if crate::runtime::daemon::running_pid().is_none() {
            error.context(summon_i18n::text!("rf180272ed6"))
        } else {
            error
        }
    })?;
    // 身份(alias)必须一致 —— 那是"我是谁";位置(tag/chat_id)不一致是 rekey 的常态,不拒,
    // 只记一行便于排查。把两者混为一谈,要么放过跨 agent 冒名,要么把正常迁移判成攻击。
    if context.agent_alias != alias {
        anyhow::bail!(summon_i18n::format!("re7302c26e5", context.agent_alias));
    }
    if let Some(tag) = expected_tag.as_deref() {
        if context.conversation_tag != tag {
            crate::observe::log::log(&summon_i18n::format!(
                "r22dbb785dc",
                context.conversation_tag
            ));
        }
    }
    let _ = expected_chat_id;
    Ok(context)
}

fn command_binary(cmd: &str) -> &str {
    cmd.split_whitespace().next().unwrap_or(cmd)
}

fn command_available(binary: &str, version_arg: &str) -> bool {
    Command::new(binary)
        .arg(version_arg)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn agents_activate(cfg: &Config, name: &str) -> anyhow::Result<()> {
    let reg = load_reg(cfg)?;
    let agent = reg
        .get(name)
        .with_context(|| summon_i18n::format!("r33ea22a48e"))?;

    println!("== activate {} ==", agent.name);
    let tmux = summon_core::tools::resolve(summon_core::tools::Tool::Tmux);
    let tmux_ok = command_available(&tmux.to_string_lossy(), "-V");
    if !tmux_ok {
        anyhow::bail!(summon_i18n::format!("rfcbbf641e5", tmux.display()));
    }
    println!("tmux            : OK");

    let cli_cmd = agent.cli_cmd();
    let cli_bin = command_binary(&cli_cmd);
    if !command_available(cli_bin, "--version") {
        anyhow::bail!(summon_i18n::format!(
            "r049f53157b",
            cli_bin,
            agent.cli_kind(),
            cli_bin
        ));
    }
    println!("{} on PATH  : OK", cli_bin);

    if let Some(wd) = agent.working_dir() {
        std::fs::create_dir_all(&wd).with_context(|| summon_i18n::format!("r435201e5cd"))?;
        println!("working_dir     : OK ({wd})");
    }

    let feishu = agent
        .feishu
        .as_ref()
        .with_context(|| summon_i18n::format!("rb9759ba7b5", agent.name))?;
    if feishu.app_id_env.as_deref().unwrap_or_default().is_empty()
        || feishu
            .app_secret_env
            .as_deref()
            .unwrap_or_default()
            .is_empty()
    {
        anyhow::bail!(summon_i18n::format!("r4936452f19", agent.name));
    }

    let (app_id, app_secret) = crate::runtime::daemon::resolve_creds(agent);
    let app_id = app_id.with_context(|| {
        summon_i18n::format!("rb9c837878c", agent.name, agent.dir.join(".env").display())
    })?;
    let app_secret = app_secret.with_context(|| {
        summon_i18n::format!("r0a8dce1ecb", agent.name, agent.dir.join(".env").display())
    })?;
    println!("feishu creds    : OK");

    let fc =
        crate::frontend::client::FeishuClient::new(&cfg.default_domain, app_id, app_secret, false);
    fc.check_auth().context(summon_i18n::text!("r4ec73d7eb5"))?;
    println!("feishu auth     : OK");

    crate::setup::bind::flip_status_active(&agent.manifest_path())?;
    println!("status          : active");
    bring_agent_online(&agent.name)
}

fn set_agent_enabled(cfg: &Config, name: &str, enabled: bool) -> anyhow::Result<()> {
    let registry = load_reg(cfg)?;
    let agent = registry
        .get(name)
        .with_context(|| summon_i18n::format!("r33ea22a48e"))?;
    let _manifest = crate::setup::bind::manifest_write_lock(&agent.manifest_path())?;
    let gate = crate::runtime::agent_availability::AgentAvailability::lock(
        &crate::runtime::paths::run_dir().join("inbox"),
        &agent.name,
    )?;
    gate.set_enabled(enabled)?;
    println!(
        "{}",
        summon_i18n::format!(
            "r714c2b00f5",
            agent.name,
            if enabled {
                summon_i18n::text!("rf4f0ead111")
            } else {
                summon_i18n::text!("r7df5c456c7")
            },
            if enabled {
                ""
            } else {
                summon_i18n::text!("r965201d97f")
            }
        )
    );
    Ok(())
}

fn up(cfg: &Config, name: &str) -> anyhow::Result<()> {
    let reg = load_reg(cfg)?;
    let live_sessions = live_sessions_for_agents(&reg);
    let agent = reg
        .get(name)
        .with_context(|| unknown_agent_message(&reg, name, &live_sessions))?;
    let sess = Session::for_agent(agent, cfg.poll_ms, cfg.debounce_ms);
    let start = crate::runtime::agent_availability::with_local_start(
        agent,
        &crate::runtime::paths::run_dir().join("inbox"),
        |fresh| Session::for_agent(fresh, cfg.poll_ms, cfg.debounce_ms).ensure_started(),
    )?;
    let state = match start {
        Start::AlreadyRunning => summon_i18n::text!("rca4ec0e9b6"),
        Start::ColdStarted => summon_i18n::text!("r021372b989"),
        Start::Resumed => summon_i18n::text!("re42e19285f"),
    };
    println!("{} {state}", sess.tmux.session);
    Ok(())
}

/// `down --chat/--reset` 的 (kill 目标, reset 记录名) 解析,抽成纯函数供 `down()` 与回归测试共用,
/// 让「杀谁就清谁」的 C17 行为被测试真正钉死(而非复读 `session_name_for_tag`)。
///
/// C17:`--chat <tag>` 不能 naive 拼 `<base>-<tag>` —— owner 私聊会话 tmux 名是裸 `soul-<alias>`,
/// 而 `.meta.tag` 是 `p<hex>`,拼出的名字不存在:tmux 杀不中、reset 清错记录名(no-op 假成功)。
/// 故经 `discover_sessions` 反查真实会话名(与 attach/tail 同源),查不到才回退拼名(会话从未起过);
/// kill 目标与 reset 记录名共用**同一次**解析结果,保证「杀谁就清谁」。
///
/// 返回 `(targets, records)`:`targets` 是待 kill 的 tmux 会话名;`records` 是 `--reset` 时待清的记录名
///(`--chat` 时=`targets`;否则=该 agent 全部 `.session` 记录)。
fn down_targets_and_records(
    agent: &summon_core::registry::Agent,
    chat: Option<&str>,
    live_sessions: &[String],
) -> (Vec<String>, Vec<String>) {
    let base = agent.tmux_name();
    let targets: Vec<String> = match chat {
        Some(tag) => vec![session_name_for_tag(agent, tag, live_sessions)
            .unwrap_or_else(|| format!("{base}-{tag}"))],
        // 全部:base 自身 + 所有 base-* per-chat 会话。
        None => crate::session::tmux::list_sessions_with_prefix(&base),
    };
    let records: Vec<String> = match chat {
        Some(_) => targets.clone(),
        None => list_session_records(agent),
    };
    (targets, records)
}

fn down(cfg: &Config, name: &str, chat: Option<&str>, reset: bool) -> anyhow::Result<()> {
    let reg = load_reg(cfg)?;
    let live_sessions = live_sessions_for_agents(&reg);
    let agent = reg
        .get(name)
        .with_context(|| unknown_agent_message(&reg, name, &live_sessions))?;
    let base = agent.tmux_name();
    let (targets, records) = down_targets_and_records(agent, chat, &live_sessions);
    // 退出回执与 `agents kill` 同一入口(入队 + 去重标记,daemon drain 投递)。视图在动手前取一次:
    // 它记的是「关之前」的 chat 归属,关完再查就查不到了。
    let views = kill_candidates_for_agent(agent, &live_sessions);
    let mut acted = false;
    for s in &targets {
        // tag 从会话名反查 meta,拿不到就跳过,不猜。视图以 tag 为键(历史遗留的裸名 / tag 名双会话
        // 会让会话名对不上),回执按 tag 找视图。
        let tag = crate::session::discovery::chat_tag_for_session(
            &base,
            s,
            &crate::session::discovery::read_chat_meta(agent, s),
        );
        let t = Tmux::new(s.clone());
        if t.has_session() {
            t.kill_session()?;
            println!("{}", summon_i18n::format!("r250d0810de"));
            acted = true;
            if let Some(view) = views.iter().find(|view| view.tag == tag) {
                enqueue_exit_receipt(&reg, view, KillRoute::Tmux, reset);
            }
        }
        // `agents down` 同样绕过 daemon 的 `do_teardown`,生命周期状态得在这里落。
        // 走到这里说明 kill 没报错(失败会 `?` 提前返回),或本来就没有进程 —— 两种都是
        // 「路由已终止」。
        crate::session::lifecycle::mark_closed(agent, &tag);
    }
    // --reset:丢弃 resume 记录,下次冷启全新会话。即便会话已不在也要清,否则残留记录会让下次续接
    // 旧上下文。resume id 存两套平行存储:tmux 路 runtime/chats/<session>.session、无头路
    // sessions/<tag>/session_id——**两处都要清**,否则无头 agent reset 后照样 resume(arch-review
    // C5/C10)。records 与 kill 目标同源(down_targets_and_records),只统计真删了的:no-op 不谎报
    //「已清除」(C17)。
    if reset {
        // 单一 resume 记录(runtime/chats/<session>.session):按会话名清,含已迁移的无头记录。
        // 只计真删(clear 对不存在文件是 no-op),no-op 不谎报「已清」(C17)。
        let mut cleared = records
            .iter()
            .filter(|s| resume_store::clear(agent, s))
            .count();
        // 迁移期兜底:清尚未迁移的旧无头残留 sessions/<tag>/session_id。--chat 给的就是 tag;
        // 否则枚举全部仍有旧记录的 tag。已迁移的记录已在上面按会话名清掉,这里不会重复计数。
        let headless_tags: Vec<String> = match chat {
            Some(tag) => vec![tag.to_string()],
            None => resume_store::legacy_headless_tags(&agent.state_dir()),
        };
        cleared += headless_tags
            .iter()
            .filter(|tag| resume_store::clear_legacy_headless(&agent.state_dir(), tag))
            .count();
        if cleared > 0 {
            println!("{}", summon_i18n::format!("rb11917ac79"));
            acted = true;
        } else {
            println!("{}", summon_i18n::format!("r709bc95bf9"));
        }
    }
    if !acted {
        println!("{}", summon_i18n::format!("r0c901808a0"));
    }
    Ok(())
}

/// 列出某 agent 现存的 resume 记录对应的 tmux 会话名(`runtime/chats/*.session`)。
fn list_session_records(agent: &summon_core::registry::Agent) -> Vec<String> {
    let dir = agent.state_dir().join("runtime").join("chats");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|e| {
            let path = e.path();
            (path.extension().is_some_and(|x| x == "session"))
                .then(|| path.file_stem().and_then(|s| s.to_str()).map(String::from))
                .flatten()
        })
        .collect()
}

fn attach(
    cfg: &Config,
    name: Option<&str>,
    chat: Option<&str>,
    from_start: bool,
) -> anyhow::Result<()> {
    let reg = load_reg(cfg)?;
    // 不给 agent 名 → 像 `agents ls` 那样列出可选(默认折叠 idle/gone,--all 才全列;这里无 --all,恒折叠)。
    let Some(name) = name else {
        let live = live_sessions_for_agents(&reg);
        println!(
            "{}",
            format_agent_choices(&reg, &live, false, ChatCmd::Follow)
        );
        return Ok(());
    };
    // 统一入口:无头(cursor/codex)每轮是一次性子进程、没有可 attach 的持久 tmux,故 `attach` 一个
    // 无头 agent 自动降级成 `tail` 它的事件流(read-only follow)。用户不必记「claude 用 attach、无头用
    // tail」——一律 `attach`,工具按 capture_mode 选对的查看方式(claude→真 tmux attach,无头→tail)。
    // `--from-start` 透传给降级的 tail(claude 真 tmux attach 是交互态,from_start 无意义、忽略)。
    if let Some(agent) = reg.get(name) {
        if agent.headless() {
            eprintln!("{}", summon_i18n::format!("rb2b5da629d"));
            return agents_tail(cfg, name, chat, from_start);
        }
    }
    let live_sessions = live_sessions_for_agents(&reg);
    match resolve_attach_target(&reg, name, chat, &live_sessions)? {
        AttachTarget::Agent { agent, session } => {
            // 按 agent 名(claude,非无头——无头已在上面早退降级 tail):会话未起则先起 base。
            if !Tmux::new(session.clone()).has_session() {
                if let Some(tag) = chat {
                    let sessions =
                        crate::session::tmux::list_sessions_with_prefix(&agent.tmux_name());
                    anyhow::bail!("{}", missing_chat_message(agent, tag, &sessions));
                }
                // TTY 探测必须**前置到起会话之前**:`tmux attach` 在非终端下必然失败
                // (`open terminal failed: not a terminal`),若先 ensure_started 再失败,
                // 就会在脚本/CI 里留下一个谁都没 attach 上的孤儿 base 会话(实测踩到)。
                // 有会话可 attach 时不做此检查——那条路不产生副作用,失败即失败。
                anyhow::ensure!(
                    !attach_needs_tty_before_start(
                        false,
                        std::io::IsTerminal::is_terminal(&std::io::stdout())
                    ),
                    summon_i18n::format!("r1001816722", agent.name, agent.name,)
                );
                eprintln!("{}", summon_i18n::format!("ra74ffe2e55"));
                crate::runtime::agent_availability::with_local_start(
                    agent,
                    &crate::runtime::paths::run_dir().join("inbox"),
                    |fresh| {
                        anyhow::ensure!(
                            fresh.tmux_name() == session,
                            summon_i18n::format!("r897737d01e")
                        );
                        Session::for_agent(fresh, cfg.poll_ms, cfg.debounce_ms).ensure_started()
                    },
                )?;
            }
            tmux_attach_session(&session)
        }
        // C18:直指 chat_id/tag 命中的会话按 capture/liveness 分流(与按 agent 名路径行为一致)。
        AttachTarget::Chat { view } => {
            use summon_core::registry::CaptureMode;
            use summon_core::session_view::Liveness;
            match view.capture {
                // 无头(含 Rpc):无持久 tmux 可 attach → 降级 tail 事件流(与按 agent 名的 headless 分支同款)。
                CaptureMode::CliSession | CaptureMode::Rpc => {
                    eprintln!("{}", summon_i18n::format!("re9c18ae9cb", view.agent));
                    agents_tail(cfg, &view.agent, Some(&view.tag), from_start)
                }
                CaptureMode::Transcript => match view.liveness {
                    Liveness::LiveTmux => tmux_attach_session(&view.session),
                    // tmux 已死的 claude 残骸(Gone):明确报已退出 + 指路清理,不再空 attach/报「未知」。
                    _ => anyhow::bail!("{}", gone_session_hint(&view)),
                },
            }
        }
    }
}

/// 交互式接管一个 tmux 会话(claude live 会话专用)。
fn tmux_attach_session(session: &str) -> anyhow::Result<()> {
    let status = Command::new(summon_core::tools::resolve(summon_core::tools::Tool::Tmux))
        // `=` 强制精确匹配:否则裸名 `soul-x` 会前缀误命中 `soul-x-g<tag>`(见 tmux.rs::target)
        .args(["attach", "-t", &format!("={session}")])
        .status()?;
    if !status.success() {
        anyhow::bail!(summon_i18n::format!("rc712ee6da6"));
    }
    Ok(())
}

/// `summon agents tail <alias> [--chat <tag>]`:人读跟随某 agent 会话的 events.jsonl。
/// --chat 指定 tag;省略时若仅一个有事件流的会话直接跟,多个则列出让选。默认从文件尾跟,
/// --from-start 从头回放。Ctrl-C 退出(默认 SIGINT 行为)。
fn agents_tail(
    cfg: &Config,
    name: &str,
    chat: Option<&str>,
    from_start: bool,
) -> anyhow::Result<()> {
    let reg = load_reg(cfg)?;
    let live_sessions = live_sessions_for_agents(&reg);
    let agent = reg
        .get(name)
        .with_context(|| unknown_agent_message(&reg, name, &live_sessions))?;

    let tag = match chat {
        Some(t) => {
            // --chat <tag> 给了但不是该 agent 的已知会话(无 events.jsonl、也不在 discover_sessions)
            // → 多半 typo 或拿错 agent(tag = chat_id 哈希、跨 agent 通用)。提示 + 列可 tail 的会话,
            // 别默默去等一个永远不来的文件(用户会以为卡死)。
            let has_events = agent
                .state_dir()
                .join("threads")
                .join(t)
                .join("events.jsonl")
                .is_file();
            let known = session_name_for_tag(agent, t, &live_sessions).is_some();
            if !has_events && !known {
                let tags = event_log_tags(agent);
                let avail = if tags.is_empty() {
                    summon_i18n::text!("rcf4072cdb4").to_string()
                } else {
                    summon_i18n::format!("r2723db4679", tag_choices_hint(agent, &tags))
                };
                anyhow::bail!(summon_i18n::format!("r2c4bc3d118", agent.name));
            }
            t.to_string()
        }
        None => {
            let tags = event_log_tags(agent);
            match tags.as_slice() {
                [only] => only.clone(),
                [] => anyhow::bail!(summon_i18n::format!("rd9ee864339", agent.name)),
                _ => anyhow::bail!(summon_i18n::format!(
                    "r1c0a2d6966",
                    agent.name,
                    agent.name,
                    tag_choices_hint(agent, &tags)
                )),
            }
        }
    };

    let path = agent
        .state_dir()
        .join("threads")
        .join(&tag)
        .join("events.jsonl");
    let driver = summon_core::driver::CliDriver::from_kind(agent.cli_kind());
    follow_event_log(&path, from_start, driver)
}

/// stdout 终端列数;非 tty / ioctl 失败按 80 兜底。
fn terminal_columns() -> usize {
    let mut ws: nix::libc::winsize = unsafe { std::mem::zeroed() };
    // SAFETY:ws 是 winsize 存储,fd 1 的 TIOCGWINSZ 只向该存储写窗口尺寸。
    let ok = unsafe { nix::libc::ioctl(1, nix::libc::TIOCGWINSZ, &mut ws) };
    if ok == 0 && ws.ws_col > 0 {
        ws.ws_col as usize
    } else {
        80
    }
}

enum TailPoll {
    Waiting,
    Events(Vec<summon_core::event_log::TailOut>),
    Unfinished(summon_core::event_log::TailBlock),
}

struct TailPoller {
    grouper: summon_core::event_log::TailGrouper,
    line: Vec<u8>,
    from_start: bool,
}

impl TailPoller {
    fn new(driver: summon_core::driver::CliDriver, color: bool, from_start: bool) -> Self {
        Self {
            grouper: summon_core::event_log::TailGrouper::for_driver(driver, color),
            line: Vec::new(),
            from_start,
        }
    }

    fn poll(
        &mut self,
        reader: &mut impl std::io::BufRead,
        stale: impl FnOnce() -> bool,
    ) -> std::io::Result<TailPoll> {
        if reader.read_until(b'\n', &mut self.line)? == 0 {
            if self.from_start && stale() {
                if let Some(block) = self.grouper.flush_pending() {
                    return Ok(TailPoll::Unfinished(block));
                }
            }
            return Ok(TailPoll::Waiting);
        }
        if self.line.last() != Some(&b'\n') {
            return Ok(TailPoll::Waiting);
        }
        let record = serde_json::from_slice::<EventRecord>(&self.line);
        self.line.clear();
        Ok(match record {
            Ok(record) => TailPoll::Events(self.grouper.push(&record)),
            Err(_) => TailPoll::Waiting,
        })
    }
}

fn write_tail_event(
    stdout: &mut impl std::io::Write,
    tty: bool,
    color: bool,
    columns: usize,
    pending_full: &mut Option<String>,
    event: summon_core::event_log::TailOut,
) -> std::io::Result<()> {
    use summon_core::event_log::{render_live_preview, TailOut};
    match event {
        TailOut::Live(block) => {
            *pending_full = Some(block.render(color));
            if tty {
                write!(
                    stdout,
                    "\r\x1b[2K{}",
                    render_live_preview(&block, color, columns)
                )?;
            }
        }
        TailOut::Frozen => {
            let full = pending_full.take().unwrap_or_default();
            if tty {
                write!(stdout, "\r\x1b[2K{full}\n")?;
            } else {
                writeln!(stdout, "{full}")?;
            }
        }
        TailOut::Line(line) => writeln!(stdout, "{line}")?,
        TailOut::LiveClear => {
            if tty {
                write!(stdout, "\r\x1b[2K")?;
            }
        }
    }
    stdout.flush()
}

/// 跟随一个 events.jsonl:文件未生成则等;默认 seek 到尾、只跟新追加的行(--from-start 从头读)。
/// 200ms 轮询,逐行 parse 成 `EventRecord`,经 [`TailGrouper`] 把同段流式碎片
/// (thinking/reply)归并成一个块后输出:tty 下块增长时原地单行刷新、定稿再展开全文;
/// 非 tty(管道)只在定稿时输出一条完整行。
fn follow_event_log(
    path: &std::path::Path,
    from_start: bool,
    driver: summon_core::driver::CliDriver,
) -> anyhow::Result<()> {
    use std::io::{BufReader, IsTerminal, Seek, SeekFrom, Write};

    // 文件可能还没生成(会话刚起未落第一条):自旋等它出现。提示**只打一次**,之后安静轮询——
    // 别每轮(500ms)重刷「等待…出现」刷屏。
    if !path.is_file() {
        eprintln!("{}", summon_i18n::format!("r1c05d1d252", path.display()));
    }
    while !path.is_file() {
        std::thread::sleep(std::time::Duration::from_millis(500));
    }

    let file = std::fs::File::open(path)
        .with_context(|| summon_i18n::format!("r1f3571e506", path.display()))?;
    let mut reader = BufReader::new(file);
    // 默认 tail -f 语义:从尾开始,只看新事件;--from-start 则从头回放。
    if !from_start {
        reader.seek(SeekFrom::End(0))?;
        // 已有内容、从尾跟 → 打一句提示,免得「当前无新事件」时一片空白看着像卡死。
        if let Ok(meta) = std::fs::metadata(path) {
            if meta.len() > 0 {
                let when = meta
                    .modified()
                    .ok()
                    .map(|t| {
                        let dt: chrono::DateTime<chrono::Local> = t.into();
                        dt.format("%m-%d %H:%M").to_string()
                    })
                    .unwrap_or_else(|| summon_i18n::text!("r4d8c1c5b42").to_string());
                eprintln!("{}", summon_i18n::format!("r66ecfa2ec2"));
            }
        }
    }

    let mut stdout = std::io::stdout();
    let tty = stdout.is_terminal();
    let color = tty;
    let mut poller = TailPoller::new(driver, color, from_start);
    let mut pending_full: Option<String> = None;
    loop {
        match poller.poll(&mut reader, || {
            std::fs::metadata(path)
                .and_then(|metadata| metadata.modified())
                .ok()
                .and_then(|modified| modified.elapsed().ok())
                .is_some_and(|age| age >= std::time::Duration::from_secs(600))
        }) {
            Ok(TailPoll::Waiting) | Err(_) => {
                std::thread::sleep(std::time::Duration::from_millis(200))
            }
            Ok(TailPoll::Unfinished(block)) => {
                pending_full = None;
                let full = summon_i18n::format!("r8f3dc2acbc", block.render(color));
                let write = if tty {
                    write!(stdout, "\r\x1b[2K{full}\n")
                } else {
                    writeln!(stdout, "{full}")
                };
                if write.and_then(|()| stdout.flush()).is_err() {
                    return Ok(());
                }
            }
            Ok(TailPoll::Events(events)) => {
                for event in events {
                    let columns = if tty { terminal_columns() } else { 0 };
                    if write_tail_event(&mut stdout, tty, color, columns, &mut pending_full, event)
                        .is_err()
                    {
                        return Ok(());
                    }
                }
            }
        }
    }
}

fn ls(cfg: &Config, name: Option<&str>, all: bool) -> anyhow::Result<()> {
    let reg = load_reg(cfg)?;
    if let Some(name) = name {
        let agent = reg
            .get(name)
            .with_context(|| summon_i18n::format!("r33ea22a48e"))?;
        return ls_agent(agent, all);
    }
    // C16:不用 tmux 空集 early-return —— 纯无头(cursor/codex)fleet 无任何 soul-* tmux,
    // 但盘上 .meta/events.jsonl 仍有 recent 会话。空态与否交给 format_agent_choices 内
    // discover_sessions 并集判定(它已含 stale/gone 折叠 + 全空提示),避免同命令有名/无名两套真相。
    let live_sessions = live_sessions_for_agents(&reg);
    println!(
        "{}",
        format_agent_choices(&reg, &live_sessions, all, ChatCmd::Follow)
    );
    Ok(())
}

/// `summon agents status [--json]`:fleet 快照(Menu Bar 的数据源,也可人读)。
///
/// 会话事实本地直读(与 `agents ls` 同源),busy/uptime 经 control.sock 问 daemon。
/// **daemon 不在 → 降级**(busy 全 false、无 uptime)而不是报错:盘上会话事实照样是真的,
/// Menu Bar 不该因为 daemon 挂了就白屏。
#[derive(clap::Subcommand)]
pub enum SentinelAction {
    #[command(about = summon_i18n::text!("r1ec866e2d6"))]
    /// 查看 sentinel 配置与运行状态
    Status {
        #[arg(help = summon_i18n::text!("r632058a470"))]
        /// 机器可读 JSON(stdout 是机器契约,诊断走 stderr)
        #[arg(long)]
        json: bool,
    },
    #[command(about = summon_i18n::text!("rea9f0f5c5a"))]
    /// 改开关(行级写 config.yaml 保注释;daemon 每 tick 热读,最迟一个轮询间隔生效)
    Set {
        #[arg(help = summon_i18n::text!("rf211affe53"))]
        /// 总开关
        #[arg(long)]
        enabled: Option<bool>,
        #[arg(help = summon_i18n::text!("r1a32f778c1"))]
        /// 只报告命中、不执行任何对外动作
        #[arg(long)]
        dry_run: Option<bool>,
    },
}

/// `summon session` 的子命令。
#[derive(clap::Subcommand)]
pub enum SessionAction {
    #[command(about = summon_i18n::text!("rd7847dfbe4"))]
    /// 查看与 Menu Bar 同源的会话状态
    #[command(visible_alias = "list")]
    Ls {
        #[arg(help = summon_i18n::text!("rbb8012e92c"))]
        /// 每 2 秒刷新状态，Ctrl+C 退出；JSON 模式每行一份快照
        #[arg(long)]
        watch: bool,
    },
    #[command(about = summon_i18n::text!("rcb648c030e"))]
    /// 标记当前回复为已读；执行期间到达的新回复仍保留未读
    #[command(alias = "handled")]
    Read {
        #[arg(help = summon_i18n::text!("r2fd8f701d8"))]
        /// 列表中的 TARGET；也可用唯一的 agent、tag、chat_id 或会话名
        #[arg(add = clap_complete::ArgValueCompleter::new(super::completion::targets))]
        target: String,
        #[arg(help = summon_i18n::text!("r546b802afb"))]
        /// 只标记先前看到的这条回复，期间的新回复不会被清掉
        #[arg(long)]
        reply: Option<String>,
    },
    #[command(about = summon_i18n::text!("r5adf598b96"))]
    /// 退出会话；下条消息接着当前上下文聊
    Exit {
        #[arg(help = summon_i18n::text!("r2fd8f701d8"))]
        /// 列表中的 TARGET；也可用唯一的 agent、tag、chat_id 或会话名
        #[arg(add = clap_complete::ArgValueCompleter::new(super::completion::targets))]
        target: String,
        #[arg(help = summon_i18n::text!("r368422d2a7"))]
        /// 明确允许打断正在运行的这一轮
        #[arg(long)]
        interrupt: bool,
    },
    #[command(about = summon_i18n::text!("r62ee15e904"))]
    /// 归档会话；下条消息新开，/resume 可恢复
    Archive {
        #[arg(help = summon_i18n::text!("r2fd8f701d8"))]
        /// 列表中的 TARGET；也可用唯一的 agent、tag、chat_id 或会话名
        #[arg(add = clap_complete::ArgValueCompleter::new(super::completion::targets))]
        target: String,
        #[arg(help = summon_i18n::text!("r368422d2a7"))]
        /// 明确允许打断正在运行的这一轮
        #[arg(long)]
        interrupt: bool,
    },
    #[command(about = summon_i18n::text!("r77900636ce"))]
    /// 导出会话的排查材料。默认是不含对话内容的诊断包，加 --full 另附对话内容与原生记录
    #[command(
        after_help = summon_i18n::text!("rf389bed7dc")
    )]
    Export {
        #[arg(help = summon_i18n::text!("rf79deefc63"))]
        /// 会话：<agent>/<tag>、tag 或 chat_id（只在一个 agent 下出现时可省 agent）
        #[arg(
            add = clap_complete::ArgValueCompleter::new(super::completion::targets),
            required_unless_present = "turn_id"
        )]
        target: Option<String>,
        #[arg(help = summon_i18n::text!("rd3365eed0c"))]
        /// 完整包：另附对话内容与原生记录。自己看过再决定交给谁
        #[arg(long)]
        full: bool,
        #[arg(help = summon_i18n::text!("rfc3e523e27"))]
        /// 取最近几轮（默认 3，在跑的那一轮算一轮）
        #[arg(long, conflicts_with = "since")]
        turns: Option<usize>,
        #[arg(help = summon_i18n::text!("rf8a5ae6548"))]
        /// 取最近一段时间，如 30m、2h、1d
        #[arg(long)]
        since: Option<String>,
        #[arg(help = summon_i18n::text!("r23908a2868"))]
        /// 包的体积上限（MB）；超出时从时间窗最早的一端裁
        #[arg(long, default_value_t = 25)]
        max_mb: u64,
        #[arg(help = summon_i18n::text!("r0fd6747087"))]
        /// agent 在自己的轮里用本轮凭证指当前会话
        #[arg(long, conflicts_with = "target")]
        turn_id: Option<String>,
    },
    #[command(about = summon_i18n::text!("r2f8d24b152"))]
    /// agent 登记本轮正常完成后退出，保留上下文；用户下条消息接着聊。
    /// 失败、打断、停机接管，或轮末仍有排队消息/活跃子代理时不退出。
    #[command(hide = true)]
    End {
        #[arg(help = summon_i18n::text!("r0564396b37"))]
        /// 本轮 prompt 给出的 turn 凭证
        #[arg(long)]
        turn_id: String,
    },
}

fn session_cmd(cfg: &Config, action: Option<SessionAction>, json: bool) -> anyhow::Result<()> {
    let action = action.unwrap_or(SessionAction::Ls { watch: false });
    if let SessionAction::End { turn_id } = action {
        return session_end_cmd(cfg, &turn_id);
    }
    if let SessionAction::Ls { watch } = action {
        return super::session::list(cfg, json, watch);
    }
    if let SessionAction::Export {
        target,
        full,
        turns,
        since,
        max_mb,
        turn_id,
    } = action
    {
        return session_export_cmd(
            cfg,
            ExportArgs {
                target,
                full,
                turns,
                since,
                max_mb,
                turn_id,
            },
            json,
        );
    }
    let rows = super::session::snapshot_rows(cfg)?;
    menubar_cmd(cfg, super::session::menu_action(&rows, &action, json)?)
}

struct ExportArgs {
    target: Option<String>,
    full: bool,
    turns: Option<usize>,
    since: Option<String>,
    max_mb: u64,
    turn_id: Option<String>,
}

/// `summon session export`:直接读盘,daemon 不在也能导出。
fn session_export_cmd(cfg: &Config, args: ExportArgs, json: bool) -> anyhow::Result<()> {
    use crate::runtime::daemon::export;
    use summon_core::export::{Mode, WindowSpec, DEFAULT_TURNS};
    let registry = load_reg(cfg)?;
    let target = match (&args.turn_id, &args.target) {
        (Some(turn_id), _) => {
            let (agent, context) = resolve_turn_target(cfg, turn_id)?;
            export::target_for(&agent, &context.conversation_tag)
        }
        (None, Some(spec)) => export::find_target(&registry.agents, spec)?,
        (None, None) => anyhow::bail!(summon_i18n::format!("r97a8341d9f")),
    };
    let window = match &args.since {
        Some(span) => WindowSpec::SinceMs(parse_span_ms(span)?),
        None => WindowSpec::Turns(args.turns.unwrap_or(DEFAULT_TURNS)),
    };
    anyhow::ensure!(args.max_mb > 0, summon_i18n::format!("r3ff82a4511"));
    let req = export::Request {
        mode: if args.full {
            Mode::Full
        } else {
            Mode::Diagnostic
        },
        window,
        max_bytes: args.max_mb.saturating_mul(1024 * 1024),
    };
    let names: Vec<String> = registry.agents.iter().map(|a| a.name.clone()).collect();
    let bundle = export::export(&target, &names, &req)?;
    if json {
        println!(
            "{}",
            serde_json::json!({"path": bundle.path, "manifest": bundle.manifest})
        );
    } else {
        let home = dirs::home_dir()
            .map(|h| h.to_string_lossy().into_owned())
            .unwrap_or_default();
        print!("{}", export::summary(&bundle, &home));
    }
    Ok(())
}

/// `30m` / `2h` / `1d` / `90s` → 毫秒。
fn parse_span_ms(text: &str) -> anyhow::Result<i64> {
    let text = text.trim();
    let split = text
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(text.len());
    let (number, unit) = text.split_at(split);
    let n: i64 = number
        .parse()
        .ok()
        .filter(|n| *n > 0)
        .with_context(|| summon_i18n::format!("rf8baee7f69"))?;
    let unit_ms = match unit {
        "s" => 1_000,
        "m" => 60_000,
        "h" => 3_600_000,
        "d" => 86_400_000,
        _ => anyhow::bail!(summon_i18n::format!("rf8baee7f69")),
    };
    Ok(n.saturating_mul(unit_ms))
}

/// `summon chats` 的子命令。裸 `chats`(action=None)= 列索引。
#[derive(clap::Subcommand)]
pub enum ChatsAction {
    #[command(about = summon_i18n::text!("r195ca5b3f3"))]
    /// 读某个会话最近若干轮正文(这是**唯一**会把别的会话正文拿进来的入口)
    Read {
        #[arg(help = summon_i18n::text!("rd5c609ea53"))]
        /// 序号(与列表一致)、会话名片段,或完整 tag。命中多个会报错、不猜。
        target: String,
        #[arg(help = summon_i18n::text!("r8ef8a856c5"))]
        /// 读最近几轮
        #[arg(long, default_value_t = 20)]
        last: usize,
    },
    #[command(about = summon_i18n::text!("rdadb7d011e"))]
    /// 跨全部会话搜关键词,**只回命中行**(不回整轮 —— 回整轮等于绕过「正文按需读」)
    Grep {
        #[arg(help = summon_i18n::text!("ra65326c1ca"))]
        /// 关键词(大小写不敏感的子串)
        pattern: String,
        #[arg(help = summon_i18n::text!("r7765353e4c"))]
        /// 每个会话最多回几条命中
        #[arg(long, default_value_t = crate::session::cross_chat::GREP_MAX_PER_CHAT)]
        max_per_chat: usize,
    },
}

/// 解析 `summon chats` 要看哪个 agent,并守住「跨 agent 不可读」这条硬闸。
///
/// 在 agent 会话里跑时 `SUMMON_AGENT` 一定有值。此时 `--agent` 只允许指向自己:否则 arcadia
/// 就能读 starlight 的全部会话转录 —— 那是身份越界,与 `frontend/client.rs` 的凭据隔离同一条
/// 不变量。终端上没有该 env(owner 本人),`--agent` 自由。
/// 纯判定(env 由调用方读进来,便于单测且不动进程环境)。这道闸只管 `summon chats`;
/// `summon session export` 不设它(owner 2026-10-29 定,见会话导出设计)。
fn resolve_chats_agent(env_alias: Option<&str>, flag: Option<&str>) -> anyhow::Result<String> {
    match (env_alias.filter(|s| !s.is_empty()), flag) {
        (Some(env), Some(f)) if env != f => anyhow::bail!(summon_i18n::format!("r4bf617ce79")),
        (Some(env), _) => Ok(env.to_string()),
        (None, Some(f)) => Ok(f.to_string()),
        (None, None) => anyhow::bail!(summon_i18n::format!("rf1e5911534")),
    }
}

fn chats_cmd(
    cfg: &Config,
    turn_id: Option<&str>,
    agent_flag: Option<&str>,
    action: Option<ChatsAction>,
) -> anyhow::Result<()> {
    use summon_core::cross_chat as cc;

    // 身份裁定:**在 agent 会话内一律以本轮凭证为准**,env 只用来判断"是不是在会话内"。
    //
    // 陈旧 env 不只会让发送走错群,也会让**只读**越界:2026-08-14 一个 agent 在自己的进程里跑
    // `summon chats`,实际返回的是另一个 agent 的会话索引 —— 跨 agent 读会话是身份越界,和错发同源。
    let env_alias = std::env::var("SUMMON_AGENT")
        .ok()
        .filter(|value| !value.trim().is_empty());
    // 在会话内时连**位置**也一并从凭证取:env 里的 tag 在 rekey 之后指着 source chat,
    // 拿它排除"当前会话"会把错误的那个排掉、反而把真正的当前会话列出来。
    let mut turn_tag: Option<String> = None;
    let alias = match (env_alias.as_deref(), turn_id) {
        (Some(_), Some(tid)) => {
            let (agent, context) = resolve_turn_target(cfg, tid)?;
            turn_tag = Some(context.conversation_tag.clone());
            agent.name
        }
        (Some(env), None) => anyhow::bail!(summon_i18n::format!("r21dbb010c1")),
        (None, _) => resolve_chats_agent(None, agent_flag)?,
    };
    let reg = load_reg(cfg)?;
    let agent = reg
        .get(&alias)
        .with_context(|| summon_i18n::format!("rccdface8de"))?;
    // 当前会话排除在外:它本来就在上下文里,列出来只会让序号和 PRIMER 里的对不上。
    // 用 daemon 注入的 `SUMMON_CONVERSATION_TAG`(= `chat_tag(chat_type, chat_id)`),不自己
    // 从 chat_id 重算 —— 重算要猜 chat_type,猜错就排错会话。
    // 凭证给出的位置优先;只有 owner 在终端直连(没有凭证)时才回落到 env。
    let current_tag = turn_tag.or_else(|| {
        std::env::var("SUMMON_CONVERSATION_TAG")
            .ok()
            .filter(|s| !s.is_empty())
    });
    let digests = crate::session::cross_chat::collect_digests(&agent.state_dir());
    // 与 PRIMER 同一把排序(`select_for_index`),但不设上限:命令行是显式检索,该给全。
    let (listed, _) =
        cc::select_for_index(&digests, current_tag.as_deref().unwrap_or(""), usize::MAX);

    match action {
        None => {
            if listed.is_empty() {
                println!("{}", summon_i18n::format!("r513499f693"));
                return Ok(());
            }
            println!("{}", summon_i18n::format!("r00821b4798"));
            for (i, d) in listed.iter().enumerate() {
                println!("{}", cc::render_row(i + 1, d));
            }
            println!("{}", summon_i18n::format!("r0cfdd5c375", listed.len()));
        }
        Some(ChatsAction::Read { target, last }) => {
            let d = crate::session::cross_chat::resolve_target(&listed, &target)?;
            let (body, skipped) =
                crate::session::cross_chat::read_turns(&agent.state_dir(), &d.tag, last)?;
            let kind = if d.is_group {
                summon_i18n::text!("r0df737cb62")
            } else {
                summon_i18n::text!("r6add2714c4")
            };
            // 跳过的轮数必须说出来:否则 agent 会把「最近 20 轮」当成「这个会话的全部」。
            let more = if skipped > 0 {
                summon_i18n::format!("r9f6c9433ae")
            } else {
                summon_i18n::text!("r9faa502c6b").to_string()
            };
            println!("{}", summon_i18n::format!("rab590bbb8c", d.name));
            if body.is_empty() {
                println!("{}", summon_i18n::format!("rb31cdaa1f2"));
            } else {
                println!("{body}");
            }
        }
        Some(ChatsAction::Grep {
            pattern,
            max_per_chat,
        }) => {
            let found = crate::session::cross_chat::grep_chats(
                &agent.state_dir(),
                &pattern,
                current_tag.as_deref(),
                max_per_chat,
            );
            if found.is_empty() {
                println!("{}", summon_i18n::format!("r160192cdfc"));
                return Ok(());
            }
            println!("{}", summon_i18n::format!("r8a11b7aaeb", found.len()));
            for (d, hits, dropped) in &found {
                let kind = if d.is_group {
                    summon_i18n::text!("r0df737cb62")
                } else {
                    summon_i18n::text!("r6add2714c4")
                };
                println!("{}({kind})", d.name);
                for h in hits {
                    println!("  [{}] {}", h.header, h.line);
                }
                if *dropped > 0 {
                    println!("{}", summon_i18n::format!("r2fd27bd73c"));
                }
                println!();
            }
            println!("{}", summon_i18n::format!("rcad3a54a76"));
        }
    }
    Ok(())
}

/// sentinel 的三态显示词:off / dry-run / on。Rust 渲染、两端只显示(与 fleet 快照哲学一致)。
fn sentinel_mode(s: &summon_core::config::Sentinel) -> &'static str {
    if !s.enabled {
        "off"
    } else if s.dry_run {
        "dry-run"
    } else {
        "on"
    }
}

fn sentinel_cmd(cfg: &Config, action: SentinelAction) -> anyhow::Result<()> {
    match action {
        SentinelAction::Status { json } => {
            let s = &cfg.sentinel;
            // 闸门状态文件是运行时事实的旁证:水位多少、搜索窗建没建。读不出当"无"。
            let gate: Option<summon_core::sentinel::gate::GateState> = std::fs::read_to_string(
                crate::runtime::paths::run_dir().join("sentinel-gate.json"),
            )
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok());
            let daemon_running = crate::runtime::daemon::running_pid().is_some();
            // 走 resolver 而不是裸读 `sentinel.lark_cli_path`:该字段迁到 `tools.lark_cli` 后仍是
            // 兼容别名,直接读它会在已迁移的配置上显示「<daemon PATH>」——而"lark-cli 没钉死"
            // 正是 sentinel 全盲事故的根因,这个假象会把排查引到反方向。
            let lark_cli = summon_core::tools::resolve_from(cfg, summon_core::tools::Tool::LarkCli);
            // sentinel 以 owner 本人身份读会话:用的是 lark-cli 默认配置(或 `sentinel.lark_profile`),
            // 它的应用与用户授权不由 onboard 配,缺了要在这里说清楚怎么补。
            let identity =
                crate::setup::lark_cli::identity_state(&lark_cli, s.lark_profile.as_deref());
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "mode": sentinel_mode(s),
                        "enabled": s.enabled,
                        "dry_run": s.dry_run,
                        "interval_secs": s.interval_secs,
                        "fast_interval_secs": s.fast_interval_secs,
                        "effective_fast_interval_secs": s.fast_interval_secs.max(3),
                        "fast_top_k": s.fast_top_k,
                        "slow_batch_size": s.slow_batch_size,
                        "top_k": s.top_k,
                        "effective_slow_interval_secs": s.interval_secs.max(30),
                        "lark_cli_path": lark_cli.to_string_lossy(),
                        "lark_cli_identity": identity.map(|state| serde_json::json!({
                            "configured": state.configured,
                            "bot_ready": state.bot_ready,
                            "user_ready": state.user_ready,
                        })),
                        "daemon_running": daemon_running,
                        "watermarks": gate.as_ref().map(|g| g.chat_watermarks.len()),
                        "search_since": gate.as_ref().map(|g| g.search_since),
                    })
                );
                return Ok(());
            }
            println!("sentinel: {}", sentinel_mode(s));
            println!(
                "{}",
                summon_i18n::format!(
                    "r699765693c",
                    s.fast_interval_secs.max(3),
                    s.fast_top_k,
                    s.interval_secs.max(30),
                    s.slow_batch_size,
                    s.top_k,
                    s.max_age_secs,
                    s.per_hour
                )
            );
            println!("  lark-cli {}", lark_cli.display());
            match identity {
                Some(state) if state.ready() => println!("{}", summon_i18n::format!("ree9e38c547")),
                Some(state) => println!(
                    "{}",
                    summon_i18n::format!(
                        "r84d9bf2d9a",
                        crate::setup::lark_cli::missing_steps(state, s.lark_profile.as_deref())
                            .join(" ; ")
                    )
                ),
                None => println!("{}", summon_i18n::format!("r2f947e9200")),
            }
            match gate {
                Some(g) => println!(
                    "{}",
                    summon_i18n::format!("r5d37652b0b", g.chat_watermarks.len(), g.search_since)
                ),
                None => println!("{}", summon_i18n::format!("r204ddaac1a")),
            }
            if !daemon_running {
                println!("{}", summon_i18n::format!("r3e384c1e19"));
            }
            Ok(())
        }
        SentinelAction::Set { enabled, dry_run } => {
            anyhow::ensure!(
                enabled.is_some() || dry_run.is_some(),
                summon_i18n::format!("r4666c35574")
            );
            let path = summon_core::config::config_path();
            if enabled == Some(true) {
                use std::io::IsTerminal as _;
                ensure_sentinel_identity(cfg, std::io::stdin().is_terminal())?;
            }
            if let Some(v) = enabled {
                crate::setup::sentinel_config::set_sentinel_flag(&path, "enabled", v)?;
            }
            if let Some(v) = dry_run {
                crate::setup::sentinel_config::set_sentinel_flag(&path, "dry_run", v)?;
            }
            let now = Config::load().sentinel;
            println!(
                "{}",
                summon_i18n::format!(
                    "r72c570a2f6",
                    sentinel_mode(&now),
                    now.fast_interval_secs.max(3)
                )
            );
            Ok(())
        }
    }
}

fn agents_status(cfg: &Config, json: bool) -> anyhow::Result<()> {
    let reg = load_reg(cfg)?;
    let now = std::time::SystemTime::now();
    // 组装收口在 `observe::fleet_snapshot`:飞书 steward 跑在 daemon 内、要的是同一份快照,
    // 而在那边抄第二份组装正是刚消除掉的毛病。CLI 这条路的 runtime 经 control.sock 问 daemon。
    let runtime = crate::observe::fleet_snapshot::fetch_runtime_status();
    let snapshot = crate::observe::fleet_snapshot::build(cfg, &reg, runtime.as_ref(), now);

    if json {
        let mut value = serde_json::to_value(&snapshot)?;
        value["language"] = serde_json::to_value(summon_core::i18n::language())?;
        println!("{}", serde_json::to_string(&value)?);
        return Ok(());
    }
    print!("{}", format_fleet(&snapshot));
    Ok(())
}

/// fleet 快照的人读渲染(纯函数,便于单测)。
fn format_fleet(snapshot: &summon_core::fleet::FleetSnapshot) -> String {
    use summon_core::fleet::format_uptime;
    use summon_core::session_view::humanize_elapsed_secs;

    let mut out = String::new();
    match (&snapshot.daemon.running, snapshot.daemon.uptime_secs) {
        (true, Some(secs)) => out.push_str(&summon_i18n::format!(
            "rc7c0b89330",
            format_uptime(secs),
            snapshot.daemon.pid.unwrap_or(0)
        )),
        _ => out.push_str(summon_i18n::text!("r4b11ef15eb")),
    }
    if snapshot.agents.is_empty() {
        out.push_str(summon_i18n::text!("r3e04b4e02e"));
        return out;
    }
    out.push_str(&summon_i18n::format!(
        "rb142b6d973",
        snapshot.busy_agents(),
        snapshot.agents.len()
    ));
    let working = summon_core::fleet::VisibleState::Working;
    for agent in &snapshot.agents {
        let (mark, state) = if agent.busy {
            ("●", summon_i18n::text!("rff6a7e52c4"))
        } else if agent.state == working {
            ("●", summon_i18n::text!("r165333e7fc"))
        } else {
            ("○", summon_i18n::text!("rdae661d17c"))
        };
        let last = agent
            .last_active_secs
            .map(humanize_elapsed_secs)
            .unwrap_or_else(|| "-".to_string());
        out.push_str(&summon_i18n::format!(
            "rc42e4078e1",
            agent.alias,
            agent.sessions
        ));
        for chat in &agent.chats {
            let name = chat.name.as_deref().unwrap_or(&chat.tag);
            let busy = if chat.busy {
                summon_i18n::text!("rda675e3844")
            } else if chat.state == working {
                summon_i18n::text!("rf2a0cfd3a9")
            } else {
                ""
            };
            out.push_str(&format!("    {name}({}){busy}\n", chat.target));
        }
    }
    out
}

/// `summon agents history <target>`：读取 summon 自己的稳定事件日志，并在 Rust 侧归并成轮次。
/// target 先走 discover_sessions 唯一解析，再由 registry 中的 agent 目录拼路径，拒绝把用户输入
/// 直接当文件路径使用。
fn agents_history(
    cfg: &Config,
    target: &str,
    limit: usize,
    before: Option<u64>,
    json: bool,
) -> anyhow::Result<()> {
    let reg = load_reg(cfg)?;
    let live_sessions = live_sessions_for_agents(&reg);
    let view = match resolve_session_target(&reg, &live_sessions, target) {
        Resolved::Unique(view) => *view,
        Resolved::NotFound => anyhow::bail!(summon_i18n::format!("r4a9f913957")),
        // 旧文案只说「请改用完整 session target」,不告诉你有哪些可选;这里直接把候选列出来。
        Resolved::Ambiguous(c) => {
            anyhow::bail!("{}", ambiguous_message(&reg, target, &c, ChatCmd::Follow))
        }
    };
    let agent = reg
        .get(&view.agent)
        .with_context(|| summon_i18n::format!("ra452c85a2f", view.agent))?;
    let path = agent
        .state_dir()
        .join("threads")
        .join(&view.tag)
        .join("events.jsonl");
    let turns = read_history_turns(&path)?;
    let page = summon_core::history::history_page(&view.session, &turns, before, limit);
    if json {
        println!("{}", serde_json::to_string(&page)?);
    } else {
        print!("{}", format_history_page(&page));
    }
    Ok(())
}

fn read_history_turns(
    path: &std::path::Path,
) -> anyhow::Result<Vec<summon_core::history::HistoryTurn>> {
    use std::io::{BufRead, BufReader};

    let mut builder = summon_core::history::HistoryBuilder::default();
    let rotated = std::path::PathBuf::from(format!("{}.1", path.display()));
    let paths: Vec<&std::path::Path> = [rotated.as_path(), path]
        .into_iter()
        .filter(|candidate| candidate.is_file())
        .collect();
    if paths.is_empty() {
        anyhow::bail!(summon_i18n::format!("r5a19e9a085", path.display()));
    }
    for candidate in paths {
        let file = std::fs::File::open(candidate)
            .with_context(|| summon_i18n::format!("rd8f3c52506", candidate.display()))?;
        for line in BufReader::new(file).lines() {
            let Ok(line) = line else {
                continue;
            };
            // append 正在写的最后半行或历史脏行只跳过该行，不能让整个历史页白屏。
            if let Ok(record) = serde_json::from_str::<EventRecord>(&line) {
                builder.push(record);
            }
        }
    }
    Ok(builder.finish())
}

fn format_history_page(page: &summon_core::history::HistoryPage) -> String {
    use summon_core::history::HistoryTurnStatus;

    let mut out = summon_i18n::format!("r3d9f98022a", page.target);
    if page.turns.is_empty() {
        out.push_str(summon_i18n::text!("r5755d7f2a5"));
        return out;
    }
    for turn in &page.turns {
        let state = match turn.status {
            HistoryTurnStatus::Running => summon_i18n::text!("rdc9591e56d"),
            HistoryTurnStatus::Completed => summon_i18n::text!("rc0b3fbff51"),
            HistoryTurnStatus::Failed => summon_i18n::text!("r28384d7afd"),
        };
        out.push_str(&format!("\n#{} · {state}", turn.ordinal));
        if turn.tool_uses > 0 {
            out.push_str(&summon_i18n::format!("r620d20ffeb", turn.tool_uses));
        }
        out.push('\n');
        if let Some(user) = &turn.user {
            out.push_str(&summon_i18n::format!("r63692e93b0"));
        }
        if let Some(assistant) = &turn.assistant {
            out.push_str(&summon_i18n::format!("r3cbd03c2e8"));
        }
        if let Some(error) = &turn.error {
            out.push_str(&summon_i18n::format!("r03d0907f2d"));
        }
    }
    if let Some(before) = page.next_before {
        out.push_str(&summon_i18n::format!("r141b8237c9", page.target));
    }
    out
}

/// `summon agents open <target>`:按配置的终端打开某会话。
///
/// 按 capture 分流:claude/Transcript 有 TUI 可接管 → `attach`;无头(cursor/codex)没有 TUI,
/// 只能跟事件流 → `tail`。终端 argv 见 `config.yaml` 的 `menubar.terminal`(留空 = 系统 Terminal.app)。
fn agents_open(cfg: &Config, target: &str) -> anyhow::Result<()> {
    use summon_core::registry::CaptureMode;
    let reg = load_reg(cfg)?;
    let live_sessions = live_sessions_for_agents(&reg);
    // 旧行为 `.find()` 静默取首条:registry 按名排序,一个群 tag 恒开首字母最小那个 agent 的会话,
    // 另外几条用户根本看不见。裸 tag 不唯一就报歧义,不猜。
    let view = match resolve_session_target(&reg, &live_sessions, target) {
        Resolved::Unique(view) => *view,
        Resolved::NotFound => {
            anyhow::bail!(summon_i18n::format!("r318bfa8423"))
        }
        Resolved::Ambiguous(c) => {
            anyhow::bail!("{}", ambiguous_message(&reg, target, &c, ChatCmd::Follow))
        }
    };

    // 用当前这个二进制,而不是 PATH 里碰巧的那个 —— 新终端里的 attach/tail 要与发起 open 的 CLI 同源。
    let exe = std::env::current_exe()
        .context(summon_i18n::text!("r0239106f9a"))?
        .to_string_lossy()
        .into_owned();
    let cmd = match view.capture {
        CaptureMode::Transcript => format!("{exe} agents attach {}", view.session),
        CaptureMode::CliSession | CaptureMode::Rpc => {
            format!("{exe} agents tail {} --chat {}", view.agent, view.tag)
        }
    };
    let title = match &view.chat_name {
        Some(name) => format!("{} · {name}", view.agent),
        None => format!("{} · {}", view.agent, view.tag),
    };
    let argv = summon_core::menubar::render_terminal_argv(&cfg.menubar.terminal, &cmd, &title);
    spawn_terminal(&argv, TERMINAL_GRACE)
}

/// 等终端命令自己退出的宽限期。cmux / osascript 都在几百毫秒内返回;超过这个数还没退的,
/// 八成是终端进程本身(有人会把 `menubar.terminal` 配成 `kitty --` 那种)。
const TERMINAL_GRACE: std::time::Duration = std::time::Duration::from_secs(5);

/// 起终端命令,**等到它退出**(或等够宽限期就放手)。
///
/// 为什么必须等:cmux / osascript 这类 CLI 只是「把指令投给终端 app」—— 要连 socket、发命令,
/// 得花几百毫秒。spawn 完就退出会让子进程半路夭折,**实测 cmux 的 workspace 根本建不出来**;
/// 而 `spawn()` 本身是成功的,于是失败完全静默:退出码 0,却什么都没发生。
///
/// 为什么不能死等:有人会把 `menubar.terminal` 配成终端进程本身(`kitty --`),那是长命的,
/// 等它就是永远不返回。宽限期一到就放手,让它自己活下去。
///
/// stdin/stdout 切 null:调用方(Menu Bar)会读我们的 stdout 到 EOF,若把管道继承给长命终端,
/// 那管道永远不 EOF —— Menu Bar 会挂死在等我们退出。stderr 则落到临时文件(不是 null):终端命令
/// 失败时(如 cmux 门禁的 "Access denied")把真实原因读回来拼进错误,否则只剩无信息量的退出码。
/// 用文件而非 pipe——长命终端不会因 summon 退出关掉管道读端而收 EPIPE,也不会因无人读、管道写满
/// 而卡住子进程。
fn spawn_terminal(argv: &[String], grace: std::time::Duration) -> anyhow::Result<()> {
    let (program, args) = argv
        .split_first()
        .context(summon_i18n::text!("re9c59bec12"))?;

    // 终端命令的 stderr 落到临时文件,失败时读回来拼进错误(见函数 doc)。
    let err_file = tempfile::NamedTempFile::new().context(summon_i18n::text!("r8c9644f1ce"))?;
    let err_sink = err_file
        .reopen()
        .context(summon_i18n::text!("r84237721f0"))?;

    let mut child = std::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(err_sink)
        .spawn()
        .with_context(|| summon_i18n::format!("ra515d32bda"))?;

    let deadline = std::time::Instant::now() + grace;
    loop {
        match child
            .try_wait()
            .context(summon_i18n::text!("re4708b57d9"))?
        {
            Some(status) if status.success() => return Ok(()),
            // 终端命令自己失败了(门禁拒绝、app 没开、参数不对…)。必须报出来 —— 静默正是上一版的
            // 病根。子进程已退,stderr 已 flush 到文件,读回来把真实原因带上。
            Some(status) => {
                let detail = std::fs::read_to_string(err_file.path()).unwrap_or_default();
                let detail = detail.trim();
                if detail.is_empty() {
                    anyhow::bail!(summon_i18n::format!("r66796f1d16"));
                }
                anyhow::bail!(summon_i18n::format!("r3b3e9c5680"));
            }
            None => {}
        }
        if std::time::Instant::now() >= deadline {
            // 还活着 = 它就是终端本身,不是投递指令的 CLI。放手。NamedTempFile 在此 drop 删文件;
            // 长命终端仍持 fd 写入已解链 inode,summon(短命 CLI)很快退出后 inode 释放,不留垃圾。
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

fn ls_agent(agent: &summon_core::registry::Agent, all: bool) -> anyhow::Result<()> {
    let base = agent.tmux_name();
    let sessions = crate::session::tmux::list_sessions_with_prefix(&base);
    let now = std::time::SystemTime::now();
    let ttl = crate::runtime::daemon::resolve_messaging(agent).idle_ttl();
    let views = discover_sessions(agent, &sessions, now, ttl);
    let total = views.len();
    // Gone 会话(tmux 已死的 Transcript 残骸 + 超 7d 的过期无头历史)永不展示,
    // 即便 --all 也不列——它们是已退出的死历史,展示只会混淆。
    let shown: Vec<_> = views
        .iter()
        .filter(|v| v.liveness.should_display(all))
        .collect();
    if shown.is_empty() {
        let hint = hidden_hint(total - shown.len(), all);
        println!("{}", summon_i18n::format!("rdaddf916ab"));
        return Ok(());
    }
    let mut t = session_table(false);
    for view in &shown {
        t.push(session_row(agent, view, now, ChatCmd::Follow, None));
    }
    println!("{}", t.render());
    let hidden = total - shown.len();
    if hidden > 0 {
        println!("{}", style::label(&hidden_hint(hidden, all)));
    }
    Ok(())
}

/// stale 会话被折叠时的提示行(`--all` 看全量)。`hidden=0` 或 `all` 时空串。
/// Gone 会话永不展示也不计入此提示——它们是死历史,不该暗示「有更多可看」。
fn hidden_hint(hidden: usize, all: bool) -> String {
    if all || hidden == 0 {
        String::new()
    } else {
        summon_i18n::format!("r18e7d5e79b")
    }
}

fn sessions_for_agent(
    agent: &summon_core::registry::Agent,
    live_sessions: &[String],
) -> Vec<summon_core::session_view::SessionView> {
    let now = std::time::SystemTime::now();
    let ttl = crate::runtime::daemon::resolve_messaging(agent).idle_ttl();
    discover_sessions(agent, live_sessions, now, ttl)
}

fn kill_liveness_rank(liveness: summon_core::session_view::Liveness) -> u8 {
    use summon_core::session_view::Liveness;
    match liveness {
        // 排在最前:这是「本该关掉却还占着进程」的残留,恰恰是 kill 最该先处理的对象。
        Liveness::ClosedButProcessLive => 0,
        Liveness::LiveTmux => 1,
        Liveness::RecentHeadless => 2,
        Liveness::IdleHeadless => 3,
        Liveness::Gone => 4,
    }
}

/// 交互 kill 只展示还有可操作意义的会话:live 在前,recent/stale 在后,gone 隐藏。
/// 同状态内按最近活跃时间倒序,时间相同再用 tag/session 稳定消歧,避免列表每次抖动。
fn visible_kill_candidates(
    mut views: Vec<summon_core::session_view::SessionView>,
) -> Vec<summon_core::session_view::SessionView> {
    // all=true:kill 候选要连 stale 一起给(仍有可操作意义),只挡死历史;泄漏态由 should_display 恒留。
    views.retain(|view| view.liveness.should_display(true));
    views.sort_by(|a, b| {
        kill_liveness_rank(a.liveness)
            .cmp(&kill_liveness_rank(b.liveness))
            .then_with(|| b.last_active.cmp(&a.last_active))
            .then_with(|| a.tag.cmp(&b.tag))
            .then_with(|| a.session.cmp(&b.session))
    });
    views
}

/// 某 agent 的交互 kill 候选。与 `prompt_and_kill` 共用同一 Vec,保证「所见即所选」;
/// 显式 `kill <session/tag/chat_id>` 则走 `discover_all_sessions`,gone 仍可定向清理。
fn kill_candidates_for_agent(
    agent: &summon_core::registry::Agent,
    live_sessions: &[String],
) -> Vec<summon_core::session_view::SessionView> {
    visible_kill_candidates(sessions_for_agent(agent, live_sessions))
}

/// 全 registry 会话并集(`kill`/`attach` 直指 chat_id/tag/session 时的匹配全集,含无头/gone)。
fn discover_all_sessions(
    reg: &Registry,
    live_sessions: &[String],
) -> Vec<summon_core::session_view::SessionView> {
    reg.agents
        .iter()
        .flat_map(|agent| sessions_for_agent(agent, live_sessions))
        .collect()
}

/// SessionView 直指匹配谓词:session 名 / chat-tag / chat_id 任一命中。
fn session_view_matches(view: &summon_core::session_view::SessionView, target: &str) -> bool {
    view.session == target || view.tag == target || view.chat_id.as_deref() == Some(target)
}

/// 裸 target 的解析结果。**返回类型即契约**:想拿到会话就必须显式处理 `Ambiguous`,
/// 物理上写不出「静默取首条」(open 旧病)或「静默全量展开」(kill 旧病)。
#[derive(Debug)]
enum Resolved {
    Unique(Box<summon_core::session_view::SessionView>),
    NotFound,
    /// 命中多条。tag / chat_id 是**相对名**:agent 作用域由容器(`agent.dir/`、`soul-<alias>-` 前缀)
    /// 提供,tag 只由 chat_id 哈希而来,故同一个群里每个 agent 各有一条同 tag 会话。
    Ambiguous(Vec<summon_core::session_view::SessionView>),
}

/// 把裸 target(会话名 / tag / chat_id)解析成唯一会话。
///
/// **为什么不静默挑一条**:会话名是绝对名(带 `soul-<alias>-` 前缀,全局唯一);tag 与 chat_id 是
/// 相对名,跨 agent 必撞(实测一个群 tag 命中 4 个 agent)。猜错的代价不对称——open 猜错只是打开
/// 了别人的会话,kill 猜错是把没打算关的 agent 一起关了。故一律交还调用方报歧义,让用户用绝对名消歧。
fn resolve_session_target(reg: &Registry, live_sessions: &[String], target: &str) -> Resolved {
    let mut matches: Vec<summon_core::session_view::SessionView> =
        discover_all_sessions(reg, live_sessions)
            .into_iter()
            .filter(|v| session_view_matches(v, target))
            .collect();
    match matches.len() {
        0 => Resolved::NotFound,
        1 => Resolved::Unique(Box::new(matches.remove(0))),
        _ => Resolved::Ambiguous(matches),
    }
}

/// 歧义报错正文:只列**命中的候选**(不是全 fleet —— 用户问的是「这几条里选哪条」),
/// 每条按 `kind` 给可照抄的绝对名指令,照抄任一行即消歧。
fn ambiguous_message(
    reg: &Registry,
    target: &str,
    candidates: &[summon_core::session_view::SessionView],
    kind: ChatCmd,
) -> String {
    let now = std::time::SystemTime::now();
    let agents: BTreeSet<&str> = candidates.iter().map(|v| v.agent.as_str()).collect();
    let mut out = summon_i18n::format!("rcca138aadb", candidates.len(), agents.len());
    let mut t = session_table(false);
    for view in candidates {
        // 候选所属 agent 必在 registry(候选正是从它枚举出来的);真丢了就跳过该条,不能让
        // 一条脏数据把整个报错吞掉 —— 用户此刻已经在错误路径上了。
        let Some(agent) = reg.get(&view.agent) else {
            continue;
        };
        t.push(session_row(agent, view, now, kind, None));
    }
    out.push_str(&t.render());
    out
}

/// `agents kill <target>` 该走哪个入口。**交互多选是唯一要读 stdin 的路**,所以入口分流必须
/// 把「stdin 是不是终端」算进去。非终端下 `read_line` 的两种结局都是错的:
/// - stdin 是不会关闭的管道/继承 fd(脚本、CI、任务编排):永久阻塞。实测 5 分钟零输出、
///   daemon.log 零日志,最后被外部 timeout 杀成 143 —— 无超时、无提示,查都没处查。
/// - stdin 是 `/dev/null`:立刻 EOF 读到空串,被当成「回车取消」静默什么都不做,退出码还是 0。
///
/// 两者共同的病根是「在没有人的地方问人」。TTY 判定把它挡在进 `read_line` 之前。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KillEntry {
    /// target 是 agent 名 + stdin 是终端:列候选让用户多选。
    Interactive,
    /// target 是 chat_id/tag/session:直接寻址,全程不碰 stdin。
    DirectTarget,
    /// target 是 agent 名但 stdin 不是终端:立刻失败,别进那个问不到人的 `read_line`。
    FailFast,
}

/// 入口分流的纯判定(TTY 探测本身不可单测,判定可以)。
fn kill_entry(target_is_agent_name: bool, stdin_is_tty: bool) -> KillEntry {
    match (target_is_agent_name, stdin_is_tty) {
        (true, true) => KillEntry::Interactive,
        (true, false) => KillEntry::FailFast,
        // 直指目标不需要 stdin,TTY 与否都一样跑。
        (false, _) => KillEntry::DirectTarget,
    }
}

/// `agents attach` 是否该在**起 base 会话之前**拒绝。与 `kill_entry` 同一类判定:
/// 非终端下 `tmux attach` 必然失败,若先 `ensure_started` 再失败,就留下一个谁都没 attach 上的
/// 孤儿 base 会话(实测踩到)。只有「会话未起」这条路会起副作用;已有会话时失败即失败,不拦。
fn attach_needs_tty_before_start(session_exists: bool, stdout_is_tty: bool) -> bool {
    !session_exists && !stdout_is_tty
}

/// 非终端下拿 agent 名 kill 的报错正文。**必须把候选原样列出来** —— 脚本作者手里没有交互界面,
/// 报错就是他唯一能拿到目标名的地方;每条尾行是可直接照抄的绝对名 kill 指令(与交互选择器同一渲染)。
fn non_interactive_kill_message(
    agent: &summon_core::registry::Agent,
    views: &[summon_core::session_view::SessionView],
    reset: bool,
) -> String {
    let mut out = summon_i18n::format!("r7a0c4f4995", agent.name);
    if views.is_empty() {
        out.push_str(summon_i18n::text!("r10a4e34e39"));
        out.push_str(summon_i18n::text!("r138da0799e"));
        return out;
    }
    out.push_str(summon_i18n::text!("r04364862f7"));
    let now = std::time::SystemTime::now();
    let mut t = session_table(false);
    for view in views {
        t.push(session_row(agent, view, now, ChatCmd::Kill { reset }, None));
    }
    out.push_str(&t.render());
    out
}

fn agents_kill(cfg: &Config, target: &str, reset: bool) -> anyhow::Result<()> {
    use std::io::IsTerminal as _;

    let reg = load_reg(cfg)?;
    let live_sessions = live_sessions_for_agents(&reg);
    // 按 agent 进交互选择时隐藏 gone,按当前状态排序;直指 session/tag/chat_id 则仍查全集,
    // 保留对 gone 残骸的定向清理能力。
    if let Some(agent) = reg.get(target) {
        let views = kill_candidates_for_agent(agent, &live_sessions);
        // 报错与交互列的是**同一份 views**:所见即所选,照抄报错等价于在交互里点那一条。
        return match kill_entry(true, std::io::stdin().is_terminal()) {
            KillEntry::Interactive => prompt_and_kill(&reg, agent, &views, reset),
            // DirectTarget 与本分支互斥(kill_entry 的定义),归到这里是 fail-closed 兜底:
            // 宁可报错也不能落进会永久阻塞的交互路。
            KillEntry::FailFast | KillEntry::DirectTarget => {
                anyhow::bail!("{}", non_interactive_kill_message(agent, &views, reset))
            }
        };
    }

    // 裸 target 必须唯一。旧行为把命中的**全部**杀掉:同群 tag 跨 agent 必撞,一句 `kill <tag>`
    // 会把该群里所有 agent 的会话一起关了,且毫无提示 —— 破坏性动作绝不猜。
    match resolve_session_target(&reg, &live_sessions, target) {
        Resolved::Unique(view) => kill_entries(&reg, &[*view], reset),
        Resolved::NotFound => anyhow::bail!(summon_i18n::format!(
            "r500198d91e",
            format_agent_choices(&reg, &live_sessions, true, ChatCmd::Kill { reset })
        )),
        Resolved::Ambiguous(c) => {
            anyhow::bail!(
                "{}",
                ambiguous_message(&reg, target, &c, ChatCmd::Kill { reset })
            )
        }
    }
}

/// 交互 kill:编号渲染与选择索引**共用同一个 `Vec<SessionView>`**——编号即索引,物理上无法
/// 错位(C3)。headless recent/stale 仍可选,gone 只在直指 target 时可达。
///
/// **调用前提:stdin 是终端**(由 `kill_entry` 把关)。下面的 `read_line` 在非 TTY 下要么永久
/// 阻塞、要么读到 EOF 静默取消,两种都是错的,所以这里不做二次兜底 —— 唯一入口就是那个判定。
fn prompt_and_kill(
    reg: &Registry,
    agent: &summon_core::registry::Agent,
    views: &[summon_core::session_view::SessionView],
    reset: bool,
) -> anyhow::Result<()> {
    if views.is_empty() {
        println!("{}", summon_i18n::format!("rc09b134a7a", agent.name));
        return Ok(());
    }
    println!("{}", summon_i18n::format!("rb85c15f724", agent.name));
    let now = std::time::SystemTime::now();
    let mut t = session_table(true);
    for (idx, view) in views.iter().enumerate() {
        // 编号直接作用于 views[idx],与下方 selected→views[idx] 是同一集合、同一顺序。
        // 续行给 kill 而非 attach:这里的语境是「选择要关闭的会话」,教 attach 是反向操作。
        t.push(session_row(
            agent,
            view,
            now,
            ChatCmd::Kill { reset },
            Some(idx + 1),
        ));
    }
    println!("{}", t.render());
    print!("{}", summon_i18n::format!("r77e7e546af"));
    std::io::stdout().flush()?;
    let mut input = String::new();
    std::io::stdin().read_line(&mut input)?;
    let selected = parse_multi_select(&input, views.len())?;
    if selected.is_empty() {
        println!("{}", summon_i18n::format!("ra37778f17c"));
        return Ok(());
    }
    let targets: Vec<summon_core::session_view::SessionView> =
        selected.into_iter().map(|idx| views[idx].clone()).collect();
    kill_entries(reg, &targets, reset)
}

/// kill 一个会话该走哪条路,按 (capture, liveness, daemon_up) 决策。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KillRoute {
    /// claude/Transcript + tmux 在:CLI 本地 `Tmux::kill`(与 daemon 解耦)。
    Tmux,
    /// 无头 + daemon 在:经 control.sock 投 `TeardownChat`,让 daemon 打断在跑轮 + 拆 handle。
    Control,
    /// 无头 + daemon 不在,或 claude 残骸(Gone,tmux 已死):按盘归档 `.session` 兜底。
    OfflineArchive,
}

/// 纯路由决策(便于单测):
/// - Transcript + LiveTmux → 本地 `Tmux::kill`(claude tmux 持久层,daemon 不在也能杀)。
/// - Transcript + 非 live(Gone 残骸)→ tmux 已死,按盘归档 `.session`(reset 清记录)。
/// - Subprocess(无头)→ daemon 在经 control.sock,不在按盘归档兜底(实体仅存活于 daemon 内)。
fn kill_route(
    capture: summon_core::registry::CaptureMode,
    liveness: summon_core::session_view::Liveness,
    daemon_up: bool,
) -> KillRoute {
    use summon_core::registry::CaptureMode;
    use summon_core::session_view::Liveness;
    match capture {
        CaptureMode::Transcript => match liveness {
            // 泄漏态与 live 同路:它**有活着的 tmux**,只是路由已退休。走归档路等于只改磁盘状态、
            // 把进程留在那儿——而它恰恰排在 kill 候选第一位,用户选了却杀不掉是自相矛盾。
            Liveness::LiveTmux | Liveness::ClosedButProcessLive => KillRoute::Tmux,
            // tmux 已死的 claude 残骸:无进程可杀,按盘归档 .session。
            _ => KillRoute::OfflineArchive,
        },
        // Rpc 与 Subprocess 同路:两者的进程实体都只活在 daemon 里(RPC 会话进程挂在
        // daemon 的 InFlight 上),daemon 不在就只剩按盘归档,绝没有第二个地方能杀它。
        CaptureMode::CliSession | CaptureMode::Rpc => {
            if daemon_up {
                KillRoute::Control
            } else {
                KillRoute::OfflineArchive
            }
        }
    }
}

fn kill_entries(
    reg: &Registry,
    views: &[summon_core::session_view::SessionView],
    reset: bool,
) -> anyhow::Result<()> {
    let daemon_up = crate::runtime::daemon::running_pid().is_some();
    for view in views {
        let route = kill_route(view.capture, view.liveness, daemon_up);
        // CLI 侧的三条关闭路都绕过 daemon 的 `do_teardown`,所以生命周期状态得在这里落。
        // **仅在确实终止后才写**:kill 失败还写 Closed,会凭空造出一条「已关闭但进程还在」的
        // 假泄漏事故,比不写更糟。
        let terminated = match route {
            KillRoute::Tmux => kill_entry_tmux(view),
            KillRoute::Control => kill_entry_control(view, reset),
            KillRoute::OfflineArchive => kill_entry_offline_archive(reg, view, reset),
        };
        let commit = close_commit_plan(route, terminated);
        if commit.write_closed {
            if let Some(agent) = reg.get(&view.agent) {
                crate::session::lifecycle::mark_closed(agent, &view.tag);
            }
        }
        if commit.send_receipt {
            enqueue_exit_receipt(reg, view, route, reset);
        }
    }
    Ok(())
}

/// 哪些 kill 路由需要 CLI 自己入队退出回执。`Control` 已由 daemon 内部发,再入队就是双发;
/// 另两条没有任何其它路径会发——`Tmux` 现状仅靠 daemon 事后考古(遍历内存 chats map),
/// daemon 重启后该 map 为空,久未活动的会话 kill 必然静默,这正是本次要修的 bug。
fn should_enqueue_exit_receipt(route: KillRoute) -> bool {
    match route {
        KillRoute::Tmux | KillRoute::OfflineArchive => true,
        KillRoute::Control => false,
    }
}

/// 入队前的完整判定,抽成纯函数便于单测(真入队写全局 outbox,单测不该碰盘)。
/// `mark_present` = 别的路径已负责这次退出;`chat_id` 缺失 = threads-only 孤儿,无投递目标。
fn should_enqueue_for(route: KillRoute, mark_present: bool, chat_id: Option<&str>) -> bool {
    should_enqueue_exit_receipt(route) && !mark_present && chat_id.is_some()
}

/// 去重标记的 chat_type 取值。`.meta` 缺 chat_type 时**必须按 tag 首字符推断**(与
/// `discovery` 构造 SessionView 的回退同源),不能硬编码 "group":标记键是
/// `chat_tag(chat_type, chat_id)`,而 idle-scan 那侧用的是入站事件带来的真实 chat_type。
/// 两侧推断不一致 → 键不匹配 → 去重失效 → 同一次退出发两条。
fn exit_chat_type(view: &summon_core::session_view::SessionView) -> &str {
    view.chat_type
        .as_deref()
        .or_else(|| summon_core::session_view::chat_type_from_tag(&view.tag))
        .unwrap_or("group")
}

/// kill 成功后入队退出回执。**只落盘,不联网、不连 control.sock**——daemon 在不在都一样成功,
/// `kill` 也保持瞬时返回。daemon 的 drain 线程负责投递(TICK_MS=2s);daemon 不在则下次启动补发。
/// 全程 best-effort:入队失败只提示,绝不让「通知发不出去」把已完成的 kill 变成报错。
///
/// 本函数只保留 CLI 独有的两件事:`SessionView` 特有的 chat_type 回退推断、入队前的去重预检;
/// 组装(文案/卡片/时间戳/随机 id/写标记)一律走 `exit_receipt`,与 daemon 同一入口。
fn enqueue_exit_receipt(
    reg: &Registry,
    view: &summon_core::session_view::SessionView,
    route: KillRoute,
    reset: bool,
) {
    use crate::runtime::exit_receipt::{self, EnqueueOutcome, ExitReceipt};
    let Some(agent) = reg.get(&view.agent) else {
        return;
    };
    let chat_type = exit_chat_type(view);
    let mark_present = view
        .chat_id
        .as_deref()
        .is_some_and(|id| crate::session::exit_mark::exists(agent, chat_type, id));
    if !should_enqueue_for(route, mark_present, view.chat_id.as_deref()) {
        return;
    }
    let chat_id = view
        .chat_id
        .as_deref()
        .expect(summon_i18n::text!("rb67b5ce956"));
    // creator 只在「发原 chat 失败要回退 p2p 群主」时用到;读不到降级 None 可接受。
    // 用 core 的 pub 解析器直接读盘,不为一次读取去放宽 daemon 内部函数的可见性。
    let creator = std::fs::read_to_string(
        agent
            .state_dir()
            .join(summon_core::chat_meta::meta_rel_path(&view.session)),
    )
    .ok()
    .and_then(|c| summon_core::chat_meta::ChatMeta::parse(&c).creator);
    // 本会话首次冷启时间:从 .session 读(reset 路径会在 kill 时清掉 .session,但入队回执在 kill 之前)。
    let session_created_at =
        crate::session::resume_store::read_record(agent, &view.session).and_then(|r| r.created_at);
    let receipt = ExitReceipt {
        agent,
        delivery_scope: &crate::runtime::daemon::feishu_delivery_scope(agent),
        chat_type,
        chat_id,
        chat_name: view.chat_name.as_deref(),
        creator: creator.as_deref(),
        event_operator: None,
        reason: summon_core::exit::manual_kill_reason(reset),
        session_created_at: session_created_at.as_deref(),
    };
    let messaging = crate::runtime::daemon::resolve_messaging(agent);
    let store = crate::runtime::outbox_store::OutboxStore::at_run_dir();
    match exit_receipt::enqueue(&store, &messaging, &receipt) {
        EnqueueOutcome::Enqueued => {
            println!("{}", summon_i18n::format!("r6dadbff730"))
        }
        // 文案被配空 → 不发(与 deliver_notify 同一语义),不是失败,也就不提示。
        EnqueueOutcome::SkippedEmptyText => {}
        EnqueueOutcome::Failed(e) => println!("{}", summon_i18n::format!("r97cb6d7773")),
    }
}

/// claude/Transcript + tmux 在:本地 tmux 直杀。
/// 返回「这条会话的路由是否确实终止了」。**kill 失败必须返回 false** —— 上层据此决定要不要
/// 写 Closed / 发退出回执:失败了还写,就等于把一个仍在跑的会话记成已关闭,凭空造出泄漏事故。
/// kill 收尾要落哪些副作用。抽成纯函数便于单测 —— 这条判断决定「关闭没成功时要不要记成已关闭」,
/// 判错的后果不是少记一笔,而是**凭空造出一条「已关闭但进程还在」的假泄漏事故**,比不写更糟。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CloseCommitPlan {
    write_closed: bool,
    send_receipt: bool,
}

fn close_commit_plan(route: KillRoute, terminated: bool) -> CloseCommitPlan {
    if !terminated {
        // 没关掉:什么都不落。会话仍在跑,记成已关闭就是谎报。
        return CloseCommitPlan {
            write_closed: false,
            send_receipt: false,
        };
    }
    CloseCommitPlan {
        // Control 路是 daemon 内部走 `do_teardown` 拆的,那里已经写过 Closed,这里不重复。
        write_closed: route != KillRoute::Control,
        send_receipt: true,
    }
}

fn kill_entry_tmux(view: &summon_core::session_view::SessionView) -> bool {
    let t = Tmux::new(view.session.clone());
    if t.has_session() {
        if let Err(e) = t.kill_session() {
            println!(
                "{}",
                summon_i18n::format!("r166606d0a6", view.agent, view.session)
            );
            false
        } else {
            println!(
                "{}",
                summon_i18n::format!("reefdb91ac6", view.agent, view.session)
            );
            true
        }
    } else {
        println!(
            "{}",
            summon_i18n::format!("r78d3a869d1", view.agent, view.session)
        );
        // 本来就没有进程 = 路由确实已终止,算成功(幂等)。
        true
    }
}

/// 无头 + daemon 在:经 control.sock 投 `TeardownChat`,让 daemon 打断在跑轮 + 拆 dispatcher handle。
/// 缺 chat_id(threads-only 孤儿,无 .meta)→ 无法定位 dispatcher 会话,提示用 reset 或盘上清理。
/// 返回是否确实拆掉了。经 daemon 拆的这条路,daemon 内部会走 `do_teardown` 自己写 Closed;
/// 这里的返回值只用于决定要不要发退出回执 —— CLI 侧不重复写状态。
fn kill_entry_control(view: &summon_core::session_view::SessionView, reset: bool) -> bool {
    use summon_core::core_port::{Actor, Command, CommandOutcome, Intent, TeardownMode};
    let Some(chat_id) = view.chat_id.clone() else {
        println!(
            "{}",
            summon_i18n::format!("r84a8875446", view.agent, view.session)
        );
        return false;
    };
    let mode = if reset {
        TeardownMode::Reset
    } else {
        TeardownMode::Clean
    };
    let cmd = Command {
        actor: Actor::Owner,
        intent: Intent::TeardownChat {
            alias: view.agent.clone(),
            chat_id: summon_core::ids::ChatId::new(chat_id),
            mode,
        },
    };
    match crate::runtime::daemon::submit_command(&cmd) {
        Ok(CommandOutcome::Done) => {
            let how = if reset {
                summon_i18n::text!("r8ab1be1b60")
            } else {
                summon_i18n::text!("r19cb588502")
            };
            println!("{} {} {how}", view.agent, view.session);
            return true;
        }
        Ok(CommandOutcome::Denied(msg)) => {
            println!(
                "{}",
                summon_i18n::format!("r9d3c3e931f", view.agent, view.session)
            );
        }
        Ok(_) => println!(
            "{}",
            summon_i18n::format!("r4c801e522f", view.agent, view.session)
        ),
        Err(e) => println!(
            "{}",
            summon_i18n::format!("r2322872f27", view.agent, view.session)
        ),
    }
    false
}

/// 按盘归档兜底,覆盖两类:① 无头 + daemon 不在(实体仅存活于 daemon 内,无运行实体可拆);
/// ② claude 残骸(Gone,tmux 已死)。reset 时清盘上 resume 记录(下次冷启=真重置),据实回执
/// 是否真清了记录(no-op 不谎报「已清」,C17 同源)。reset 必须清**两套**存储:tmux 路 `.session`
/// 与无头路 `sessions/<tag>/session_id`——无头 chat 本无 `.session`,只清 tmux 路是纯 no-op 却谎报
/// 已清(arch-review C5/C10),故走统一入口 `clear_chat_resume`(按 `view.tag` 一并清无头路)。
fn kill_entry_offline_archive(
    reg: &Registry,
    view: &summon_core::session_view::SessionView,
    reset: bool,
) -> bool {
    use summon_core::registry::CaptureMode;
    let reason = match view.capture {
        CaptureMode::Transcript => summon_i18n::text!("r9a4ef1ef7b"),
        CaptureMode::CliSession | CaptureMode::Rpc => summon_i18n::text!("rd0058a5b8b"),
    };
    let suffix = if reset {
        // 副作用静默兜底 + 据实回执:真删了才说「已清」,no-op 别谎报。清两套存储(C5/C10)。
        let cleared = reg
            .get(&view.agent)
            .map(|agent| resume_store::clear_chat(agent, &view.session, &view.tag))
            .unwrap_or(false);
        if cleared {
            summon_i18n::text!("r95123d7bf0")
        } else {
            summon_i18n::text!("reefce034b0")
        }
    } else {
        ""
    };
    println!("{} {} {reason}{suffix}", view.agent, view.session);
    // 归档路只改磁盘状态,不涉及进程;走到这里即视为路由终止。
    true
}

/// U3:doctor 的 per-agent CLI 检查清单(纯映射)——routable agents 按 cli_kind 去重,给出待验
/// 可执行名(per-agent `cli_cmd` 覆盖优先、取首 token)。无头 agent 的 CLI 不在 PATH 时每轮 spawn
/// 都失败,旧 doctor 硬编码只查 claude 会静默放过。
/// 扫出这台机器上**真实存在**的 tmux 可执行文件(按候选目录顺序,去重)。
///
/// 为什么 doctor 要主动扫而不是只说「找不到」:2026-08-07 devbox 现场,tmux 明明装着
/// (`~/homebrew/bin/tmux`),只是不在 daemon 继承到的 PATH 里。只报「缺失」会把人引向「去装一个」,
/// 而正确动作是「告诉 summon 它在哪」。列出真实候选 = 把诊断直接变成可执行的下一步。
/// 把选定的 tmux 路径写进 `~/.summon/config.yaml` 的 `tools.tmux`。
///
/// `pick` 可以是 `doctor` 列出的候选**序号**,也可以是完整路径 —— 序号是给人省事的,
/// 路径是给脚本/非交互场景用的。**写之前先验它真的能跑**(`tmux -V`):把一个错路径写进配置,
/// 比不配更糟(不配至少还能靠 PATH 自愈兜底,配错则是显式地把自己钉死在坏值上)。
fn doctor_set_tmux(pick: &str) -> anyhow::Result<()> {
    let candidates = doctor_tmux_candidates();
    let path = match pick.trim().parse::<usize>() {
        Ok(i) if i >= 1 && i <= candidates.len() => candidates[i - 1].clone(),
        Ok(i) => anyhow::bail!(summon_i18n::format!("r9ee1c403f2", candidates.len())),
        Err(_) => pick.trim().to_string(),
    };
    let ok = Command::new(&path)
        .arg("-V")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    anyhow::ensure!(ok, summon_i18n::format!("ra03f7b6314"));

    let cfg_path = summon_core::config::config_path();
    let raw = std::fs::read_to_string(&cfg_path).unwrap_or_default();
    let updated = summon_core::tools::patch_tools_yaml(
        &raw,
        &[(summon_core::tools::Tool::Tmux, path.clone().into())],
        true,
    )
    .map_err(anyhow::Error::msg)?;
    crate::runtime::fsio::atomic_write(&cfg_path, updated.as_bytes())
        .with_context(|| summon_i18n::format!("r889b854e15", cfg_path.display()))?;
    println!(
        "{}",
        summon_i18n::format!("rf74f950293", cfg_path.display())
    );
    println!("{}", summon_i18n::format!("rf64ec817c9"));
    Ok(())
}

fn doctor_tmux_candidates() -> Vec<String> {
    let home = std::env::var("HOME").unwrap_or_default();
    let mut out: Vec<String> = Vec::new();
    for dir in summon_core::config::TMUX_CANDIDATE_DIRS {
        let dir = if let Some(rest) = dir.strip_prefix("~/") {
            if home.is_empty() {
                continue;
            }
            format!("{home}/{rest}")
        } else {
            dir.to_string()
        };
        let p = std::path::Path::new(&dir).join("tmux");
        if p.is_file() && !out.iter().any(|e| e == &p.to_string_lossy()) {
            out.push(p.to_string_lossy().to_string());
        }
    }
    // PATH 上的那个(可能不在候选目录里)也算一个候选；不再为 which 再起 shell。
    if let Some(found) = summon_core::tools::find_executable(
        std::path::Path::new("tmux"),
        &std::env::var_os("PATH").unwrap_or_default(),
    ) {
        let p = found.to_string_lossy().to_string();
        if !out.iter().any(|e| e == &p) {
            out.push(p);
        }
    }
    out
}

fn doctor_cli_cmds(reg: &Registry) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    for a in reg.routable() {
        let kind = a.cli_kind().to_string();
        if out.iter().any(|(k, _)| *k == kind) {
            continue;
        }
        let cmd = a
            .cli_cmd()
            .split_whitespace()
            .next()
            .unwrap_or("claude")
            .to_string();
        out.push((kind, cmd));
    }
    out
}

/// 把「盘上身份指纹」与 daemon 上报的 epoch 指纹逐 agent 配对判定(纯函数)。
///
/// `runtime = None` 表示 daemon 不在或 control.sock 问不到 —— 一律降级为「无法确认」,
/// 绝不因为「两边都没值」而落进相等分支报绿。
fn classify_credential_rows(
    disk_fingerprints: &[(String, String)],
    runtime: Option<&summon_core::fleet::RuntimeStatus>,
) -> Vec<(String, CredentialIdentity)> {
    let reported = runtime.map(|rt| rt.credentials.as_slice()).unwrap_or(&[]);
    disk_fingerprints
        .iter()
        .map(|(agent, disk)| {
            let state = classify_credential_identity(
                runtime.is_some(),
                fingerprint_of(reported, agent),
                Some(disk.as_str()),
            );
            (agent.clone(), state)
        })
        .collect()
}

/// doctor 的「飞书凭据身份」小节渲染(纯函数)。
///
/// 为什么单独成节而不是塞进 app_id/secret 那两行:那两行问的是「盘上凭据在不在」,恒为 OK,
/// 正是它让 owner 在 agent 已经聋掉时看到全绿。漂移是另一个维度的事实,必须自己占一行、
/// 带 ⚠️、带修复指令。
///
/// **入参只有别名与判定结果**:指纹连进函数的机会都没有,结构上杜绝凭据(或其摘要)进输出。
fn format_credential_identity_section(rows: &[(String, CredentialIdentity)]) -> String {
    if rows.is_empty() {
        return String::new();
    }
    let drifted = rows.iter().filter(|(_, s)| s.is_drifted()).count();
    let matched = rows
        .iter()
        .filter(|(_, s)| matches!(s, CredentialIdentity::Match))
        .count();
    let total = rows.len();
    let head = if drifted > 0 {
        summon_i18n::format!("rd0e78aebbf")
    } else if matched == total {
        summon_i18n::format!("r64a9f6820b")
    } else {
        summon_i18n::format!("r3c3ea5add3")
    };
    let mut out = summon_i18n::format!("r0fa397016e");
    for (agent, state) in rows {
        let detail = match state {
            CredentialIdentity::Match => summon_i18n::text!("r0680a4a36c").to_string(),
            CredentialIdentity::Drifted => summon_i18n::text!("ra1224b32bb").to_string(),
            CredentialIdentity::Unknown(reason) => {
                let why = match reason {
                    IdentityUnknown::DaemonDown => summon_i18n::text!("r4fcc05508e"),
                    IdentityUnknown::NotReported => {
                        summon_i18n::text!("r2be4010642")
                    }
                    IdentityUnknown::DiskUnreadable => summon_i18n::text!("r5efb0daaff"),
                };
                summon_i18n::format!("ra2ff1dd484")
            }
        };
        out.push_str(&format!("  {agent:<12} : {detail}\n"));
    }
    if drifted > 0 {
        out.push_str(summon_i18n::text!("r45ee5a466b"));
    }
    out
}

/// doctor 的 codex 策略面板(纯映射,便于单测)。
///
/// 只报**需要人处理**的三件事:非法值(已降级,配了等于没配)、非 never 的 approval
/// (会暂停 turn 等飞书 operator)、read-only + never(写不了又无法升级)。
/// 一切正常时只出一行,不刷屏。
fn format_codex_policy_section(
    agents: &[&summon_core::registry::Agent],
    global: &summon_core::codex_policy::CodexPolicyOverride,
) -> String {
    let mut out = String::new();
    // 判据与控制台「需要处理」共用。
    let findings = crate::setup::config_findings::codex_findings(agents, global);
    for finding in &findings {
        out.push_str(&finding.doctor_line());
        out.push('\n');
    }
    if findings.is_empty() {
        out.push_str(summon_i18n::text!("r5d462b9b7f"));
    } else {
        out.push_str(&summon_i18n::format!(
            "r12e5a3deb4",
            summon_core::codex_policy::SANDBOX_VALUES,
            summon_core::codex_policy::APPROVAL_VALUES
        ));
        out.push_str(summon_i18n::text!("r28f4d08318"));
    }
    out
}

/// sidecar 缺失时的修法:源码构建的用 deploy.sh 装齐;安装包装的由分发渠道给修法。
fn sidecar_missing_fix() -> String {
    #[cfg(npm_dist)]
    if let Some(fix) = crate::setup::npm_dist::sidecar_missing_fix() {
        return fix;
    }
    summon_i18n::format!(
        "rd1fdf63da1",
        summon_core::companion::SIDECAR,
        ::summon_core::i18n::label(summon_core::companion::WHERE)
    )
}

/// 自动升级的体检由带它的分发渠道给出。
#[cfg(npm_dist)]
fn auto_upgrade_doctor_lines(cfg: &Config) -> Vec<String> {
    crate::setup::npm_dist::auto_upgrade::doctor_lines(cfg)
}

/// 不带自动升级的构建:config.yaml 里写了 `upgrade:` 段才说一声它不起作用。
#[cfg(not(npm_dist))]
fn auto_upgrade_doctor_lines(_cfg: &Config) -> Vec<String> {
    let written = std::fs::read_to_string(summon_core::config::config_path())
        .ok()
        .and_then(|text| serde_yaml::from_str::<serde_yaml::Value>(&text).ok())
        .is_some_and(|value| summon_core::config::path_exists_in_value(&value, "upgrade"));
    if written {
        vec![summon_i18n::text!("rd1ff56324b").to_string()]
    } else {
        Vec::new()
    }
}

fn doctor(cfg: &Config) -> anyhow::Result<()> {
    fn ok(b: bool) -> &'static str {
        if b {
            "OK"
        } else {
            summon_i18n::text!("r65364e6fd8")
        }
    }
    fn on_path(cmd: &str, arg: &str) -> bool {
        Command::new(cmd)
            .arg(arg)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }
    // 排错第一句先说清「你手上这个二进制是什么」:版本号能对着 registry 查,commit sha 能对着
    // 仓库查 —— 用户贴 doctor 输出过来时,这两个数省掉一轮「你装的是哪版」的往返。
    println!(
        "summon          : {} (commit {})",
        crate::version::VERSION,
        crate::version::COMMIT
    );
    // U3 三大根因之一:daemon 在不在跑。「飞书发消息没反应」最常见原因,旧 doctor 不查。
    match crate::runtime::daemon::running_pid() {
        Some(pid) => {
            println!("daemon          : OK ({})", daemon_identity_label(pid));
            let sock_ok =
                std::os::unix::net::UnixStream::connect(crate::runtime::paths::control_sock())
                    .is_ok();
            println!("control.sock    : {}", ok(sock_ok));
            if !sock_ok {
                println!("{}", summon_i18n::format!("r2f98780268"));
            }
        }
        None => {
            println!("{}", summon_i18n::format!("re3f996d927"));
            println!("{}", summon_i18n::format!("r53e31c3b69"));
        }
    }
    // U3 之二:sidecar 二进制。缺失时 daemon 照常启动但收不到任何飞书事件(仅 inject 模式)。
    match crate::runtime::daemon::default_sidecar_bin() {
        Some(p) => println!("sidecar bin     : OK ({})", p.display()),
        None => {
            println!("{}", summon_i18n::format!("r4f23f1f9cd"));
            println!(
                "{}",
                summon_i18n::format!("rb0540d1d75", sidecar_missing_fix())
            );
        }
    }
    // 文件预览服务器:默认开;查端口在不在听,以及 bind/base_url 组合是否自洽。
    let pv = &cfg.preview_server;
    if !pv.enabled {
        println!("{}", summon_i18n::format!("r943b82eb44"));
    } else {
        let listening = std::net::TcpStream::connect((pv.bind.as_str(), pv.port)).is_ok();
        // 对外地址每次 mint 现查(VPN 地址会变),这里打印的是**此刻**会用的那个。
        let base = pv.base_url_with_host(crate::runtime::preview_server::detect_public_host());
        println!(
            "{}",
            summon_i18n::format!(
                "r71f9b5a569",
                ok(listening),
                pv.bind,
                pv.port,
                base.as_deref().unwrap_or(summon_i18n::text!("ra6b3fd4d86"))
            )
        );
        if !listening {
            println!("{}", summon_i18n::format!("rc7c9676593"));
        }
        // 查不到可路由地址 = 一条链接也 mint 不出来。localhost 不做兜底:链接的收件人是
        // 飞书客户端,可能在手机上,那个地址会打到收件设备自己。判据与控制台「需要处理」共用。
        for finding in crate::setup::config_findings::preview_findings(cfg, base.as_deref()) {
            println!("{}", finding.doctor_line());
        }
    }
    // 网页终端:默认开;查端口在不在听,bind 为 127.0.0.1 时手机打不开。
    let wt = &cfg.web_terminal;
    if !wt.enabled {
        println!("{}", summon_i18n::format!("r021add024a"));
    } else {
        let bind = cfg.preview_server.bind.as_str();
        let listening = std::net::TcpStream::connect((bind, wt.port)).is_ok();
        let base = wt.base_url_with_host(crate::runtime::preview_server::detect_public_host());
        println!(
            "{}",
            summon_i18n::format!(
                "rbc0febf56d",
                ok(listening),
                wt.port,
                base.as_deref().unwrap_or(summon_i18n::text!("ra6b3fd4d86"))
            )
        );
        if !listening {
            println!("{}", summon_i18n::format!("rdddfc77b71"));
        }
        for finding in crate::setup::config_findings::terminal_findings(cfg) {
            println!("{}", finding.doctor_line());
        }
    }
    // 配置控制台:默认开;查端口在不在听,以及远程链接出不出得来(与预览、终端同口径)。
    for line in crate::runtime::dashboard::doctor_lines(cfg) {
        println!("{line}");
    }
    for line in auto_upgrade_doctor_lines(cfg) {
        println!("{line}");
    }
    // outbox 死信:重试耗尽的消息永久躺在 dead/,无主动通知,只能靠体检暴露。
    let dead = crate::runtime::outbox_store::OutboxStore::at_run_dir().dead_count();
    if dead > 0 {
        println!("{}", summon_i18n::format!("r1b64f5c4a2", dead));
    } else {
        println!("outbox dead     : 0");
    }
    println!("tools:");
    let path = std::env::var_os("PATH").unwrap_or_default();
    let mut tmux_ok = false;
    for tool in summon_core::tools::Tool::doctor_tools() {
        let inspected = summon_core::tools::inspect(cfg, tool, &path);
        let actual = inspected
            .actual
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| inspected.requested.display().to_string());
        let usable = inspected.actual.is_some();
        if tool == summon_core::tools::Tool::Tmux {
            tmux_ok = usable;
        }
        println!(
            "  {:<14}: {} ({}, {})",
            tool.key(),
            ok(usable),
            inspected.source.label(),
            actual
        );
        for warning in inspected.warnings {
            println!("    ⚠ {warning}");
        }
        if !usable {
            println!(
                "{}",
                summon_i18n::format!("rd5fa8fec54", tool.command(), tool.key())
            );
        }
    }
    if !tmux_ok {
        let candidates = doctor_tmux_candidates();
        if candidates.is_empty() {
            println!(
                "{}",
                summon_i18n::format!(
                    "ra559d3a5f8",
                    crate::setup::onboard::install_hint(
                        summon_core::tools::Tool::Tmux,
                        crate::setup::onboard::PackageManager::detect(),
                    )
                )
            );
        } else {
            // 装着但当前用不上 = 路径问题,不是安装问题。把可选项和落地命令一起给出来。
            println!("{}", summon_i18n::format!("re6c6de76cd"));
            for (i, c) in candidates.iter().enumerate() {
                println!("     [{}] {c}", i + 1);
            }
            println!("{}", summon_i18n::format!("r96a9e82a7a"));
            println!("{}", summon_i18n::format!("r9d694e3799"));
        }
    }
    println!(
        "agents dir      : {} ({})",
        ok(cfg.agents_dir.is_dir()),
        cfg.agents_dir.display()
    );
    match load_reg(cfg) {
        Ok(reg) => {
            println!(
                "registry        : OK ({} agents, {} routable)",
                reg.agents.len(),
                reg.routable().len()
            );
            // U3 之三:按 registry 各 agent 的 cli_kind 查对应可执行(不再硬编码只查 claude)。
            for (kind, cmd) in doctor_cli_cmds(&reg) {
                let cli_ok = on_path(&cmd, "--version");
                println!("cli {kind:<12}: {} ({cmd})", ok(cli_ok));
                if !cli_ok {
                    println!("{}", summon_i18n::format!("r2439228bda"));
                }
            }
            // 盘上身份指纹与 daemon epoch 指纹同源(都走 FeishuEpochIdentity::capture)。
            let mut disk_fingerprints: Vec<(String, String)> = Vec::new();
            for a in reg.routable() {
                // 经 resolve_creds(per-agent .env + 进程 env),只验在不在、不打印值
                let (id, sec) = crate::runtime::daemon::resolve_creds(a);
                println!("  {:<12} app_id : {}", a.name, ok(id.is_some()));
                println!("  {:<12} secret : {}", a.name, ok(sec.is_some()));
                if id.is_none() || sec.is_none() {
                    println!("{}", summon_i18n::format!("rb791ac58d0", a.dir.display()));
                }
                disk_fingerprints.push((
                    a.name.clone(),
                    crate::runtime::daemon::feishu_identity_fingerprint(a),
                ));
            }
            // codex 权限策略:非法值会被静默降级,危险组合会让 agent「干什么都失败」。
            // 两者在运行时都只落一行日志,doctor 是唯一能主动看见它们的地方。
            print!(
                "{}",
                format_codex_policy_section(&reg.routable(), &cfg.codex)
            );
            // 上面两行只证明「盘上凭据在不在」,daemon 内存里用的是不是这一份是另一回事:
            // daemon epoch 冻结 resolved 身份,盘上一变就对该 agent 的外部入站 fail-closed,
            // 而此前唯一信号在 daemon.log 里。这节把它摆到 doctor 面上。
            let runtime = crate::observe::fleet_snapshot::fetch_runtime_status();
            let rows = classify_credential_rows(&disk_fingerprints, runtime.as_ref());
            print!("{}", format_credential_identity_section(&rows));
        }
        // 新机器装完、还没 onboard:没有 agents 目录是正常起点,不是错误。
        Err(_) if !cfg.agents_dir.is_dir() => {
            println!("{}", summon_i18n::format!("r23fbfcb293"))
        }
        Err(e) => println!("{}", summon_i18n::format!("r72d786939a")),
    }
    let (ast, installed) = crate::setup::autostart::status();
    let (on, crash) = autostart_summary(ast.enabled);
    let ka = if ast.keep_alive {
        summon_i18n::text!("rb5141d3d19")
    } else {
        summon_i18n::text!("r0c70665b6e")
    };
    let pl = if installed {
        summon_i18n::text!("rac7f2437c0")
    } else {
        summon_i18n::text!("r484d556139")
    };
    let kind = crate::setup::autostart::unit_kind_label();
    println!("{}", summon_i18n::format!("r2ed3ba076c"));
    if let Some(line) = supervision_line(ast.enabled) {
        println!("{}", summon_i18n::format!("r3e55e7ca24"));
    }
    if let Some(note) = crate::runtime::daemon::last_refusal() {
        println!("{}", summon_i18n::format!("rcb08635bc4"));
    }
    if cfg!(target_os = "macos") {
        match crate::setup::menubar::status() {
            Ok(status) => println!(
                "native menubar  : binary {} | plist {} | launchd {}",
                ok(status.binary_exists),
                if status.plist_exists {
                    summon_i18n::text!("rac7f2437c0")
                } else {
                    summon_i18n::text!("r484d556139")
                },
                if status.running {
                    summon_i18n::text!("r1f0eb99b7e")
                } else {
                    summon_i18n::text!("r62cdc8713b")
                },
            ),
            Err(error) => println!("{}", summon_i18n::format!("r29ee005a62")),
        }
    }

    println!();
    println!("External Bridge");
    match crate::bridge::install::status() {
        Ok((path, status)) => println!("Hooks           : {status:?} ({})", path.display()),
        Err(_) => println!("Hooks           : unavailable"),
    }
    print_bridge_runtime_health();

    // 配置演进检查:对比用户 config.yaml 实际写了哪些 key 与已知字段元信息表,
    // 告知可能缺少的新字段(使用内置默认)和已废弃字段。
    doctor_config_evolution();
    Ok(())
}

/// doctor 的"配置演进检查"节:读用户 config.yaml 原始 Value,与已知字段元信息表对比,
/// 输出可能缺少的新字段(用内置默认)和已废弃字段。纯展示、不影响功能。
fn doctor_config_evolution() {
    use summon_core::config::{config_path, path_exists_in_value, CONFIG_FIELD_META};

    let cp = config_path();
    let raw_value: Option<serde_yaml::Value> = std::fs::read_to_string(&cp)
        .ok()
        .and_then(|s| serde_yaml::from_str(&s).ok());

    let user_value = match &raw_value {
        Some(v) => v,
        None => {
            // 无 config.yaml(纯默认)→ 所有字段都算"使用内置默认"。
            // 但这种情况太常见,只给一行友好提示、不列全表(否则刷屏)。
            println!();
            println!("{}", summon_i18n::format!("r42b4b040ce"));
            println!("─────────────────────");
            println!("{}", summon_i18n::format!("r8cef68d85a", cp.display()));
            println!("{}", summon_i18n::format!("r619facd6ba"));
            return;
        }
    };

    let mut missing: Vec<&summon_core::config::ConfigFieldMeta> = Vec::new();
    let mut deprecated: Vec<&summon_core::config::ConfigFieldMeta> = Vec::new();
    for meta in CONFIG_FIELD_META {
        let present = path_exists_in_value(user_value, meta.path);
        if !present && meta.deprecated_since.is_none() {
            missing.push(meta);
        }
        if present && meta.deprecated_since.is_some() {
            deprecated.push(meta);
        }
    }

    println!();
    println!("{}", summon_i18n::format!("r42b4b040ce"));
    println!("─────────────────────");

    if missing.is_empty() && deprecated.is_empty() {
        println!("{}", summon_i18n::format!("re1ad7e366e"));
        return;
    }

    if !missing.is_empty() {
        // 按 since 倒序(最新的排前面),让用户先看到最近新增的。
        missing.sort_by(|a, b| b.since.cmp(a.since));
        println!("{}", summon_i18n::format!("r1bd4cae5e3"));
        // 算最长 path 长度做对齐。
        let max_path = missing.iter().map(|m| m.path.len()).max().unwrap_or(0);
        for m in &missing {
            let since_label = if m.since == "initial" {
                "initial".to_string()
            } else {
                summon_i18n::format!("r90c184d775", m.since)
            };
            println!(
                "   - {:<max_path$}  [{}]",
                m.path,
                since_label,
                max_path = max_path
            );
            println!("     {}", summon_core::i18n::label(m.description));
        }
    }

    if !deprecated.is_empty() {
        println!("{}", summon_i18n::format!("r3eb54aaf75"));
        for m in &deprecated {
            let dep = summon_core::i18n::label(m.deprecated_since.unwrap_or(""));
            println!("{}", summon_i18n::format!("rf00d9e3f5e", m.path, dep));
            println!("     {}", summon_core::i18n::label(m.description));
        }
    }

    println!();
    println!("{}", summon_i18n::format!("rc46e76116d"));
    println!("{}", summon_i18n::format!("r619facd6ba"));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 开 sentinel 之前先补全它要的 lark-cli 身份(这份身份只有 sentinel 用):补不全就不开。
    #[test]
    fn enabling_sentinel_first_ensures_the_lark_cli_identity() {
        let cli = include_str!("cli.rs");
        let set = &cli[cli
            .find("SentinelAction::Set { enabled, dry_run } => {")
            .unwrap()..];
        let gate = set
            .find("if enabled == Some(true) {")
            .expect("只在开启 sentinel 时补身份");
        let ensure = set
            .find("ensure_sentinel_identity(cfg,")
            .expect("开 sentinel 前要补全 lark-cli 身份");
        assert!(gate < ensure);
        let write = set
            .find("set_sentinel_flag(&path, \"enabled\", v)")
            .expect("写 enabled 开关");
        assert!(ensure < write);
        // Menu Bar 设置区是另一个能开 sentinel 的入口,同一道检查。
        let menubar = &cli[cli.find("MenubarAction::Set { key, value } => {").unwrap()..];
        let check = menubar
            .find("ensure_sentinel_identity(cfg, false)?;")
            .expect("Menu Bar 开 sentinel 前也要检查 lark-cli 身份");
        let apply = menubar
            .find("apply_to_config(&summon_core::config::config_path(), &change)?;")
            .unwrap();
        assert!(check < apply);
    }

    /// 结构守护:`daemon stop` 发 SIGTERM 之前先留下停机请求,读它的一方据此分辨人停与 daemon
    /// 自己退了。标记本身的读写由 `crash_guard` 的测试证明。
    #[test]
    fn daemon_stop_leaves_a_stop_request_before_signalling() {
        let src = include_str!("cli.rs");
        let stop = &src[src
            .find("fn daemon_stop_with_timeout(")
            .expect("daemon_stop_with_timeout")..];
        let request = stop
            .find("crate::runtime::daemon::write_stop_request(pid)")
            .expect("停机请求");
        let signal = stop.find("nix::sys::signal::kill(").expect("发信号");
        assert!(request < signal, "停机请求必须先于 SIGTERM");
    }

    #[test]
    fn doctor_fixes_follow_the_install_channel_and_platform() {
        // 测试二进制不在任何安装包里,走的是源码安装的修法。
        assert!(sidecar_missing_fix().contains("deploy.sh"));
        let doctor = include_str!("cli.rs");
        let doctor = &doctor[doctor.find("fn doctor(cfg: &Config)").unwrap()..];
        let doctor = &doctor[..doctor.find("\n}\n").unwrap()];
        assert!(!doctor.contains("brew install tmux"));
        assert!(!doctor.contains("docs/spec"));
        assert!(!doctor.contains("(或 sh scripts/deploy.sh)"));
    }

    /// 热加载只接新 agent:alias 在这次新上线名单里就算上线了;不在 = daemon 早就加载过它
    /// (换凭据的场景),要提示重启。
    #[test]
    fn a_reload_that_did_not_admit_the_alias_means_it_was_already_loaded() {
        assert!(!already_loaded_before_reload(
            "coder",
            &["coder".to_string()]
        ));
        assert!(already_loaded_before_reload("coder", &[]));
        assert!(already_loaded_before_reload("coder", &["ops".to_string()]));
    }

    /// bind 写好凭据、agents activate 翻成 active 之后都走同一条上线路:daemon 在跑就热加载,
    /// 没跑才启动 —— 重启会打断所有 agent 正在跑的无头轮。
    #[test]
    fn bind_and_activate_bring_the_agent_online_without_a_blanket_restart() {
        let cli = include_str!("cli.rs");
        let activate = &cli[cli
            .find("    crate::setup::bind::flip_status_active(&agent.manifest_path())?;")
            .unwrap()..];
        let activate = &activate[..activate.find("\n}").unwrap()];
        assert!(
            activate.contains("bring_agent_online(&agent.name)"),
            "{activate}"
        );
        let bind = include_str!("../setup/bind.rs");
        let run = &bind[bind
            .find("pub fn run(cfg: &Config, args: BindArgs)")
            .unwrap()..];
        let run = &run[..run.find("\n}\n").unwrap()];
        assert!(
            run.contains("crate::cli::cli::bring_agent_online(&args.alias)?;"),
            "{run}"
        );
        let summary = &bind[bind.find("pub(crate) fn print_summary(").unwrap()..];
        let summary = &summary[..summary.find("\n}\n").unwrap()];
        assert!(!summary.contains("daemon stop"), "{summary}");
        // 分流本身:daemon 没跑才起,在跑只热加载。
        let online = &cli[cli
            .find("pub(crate) fn bring_agent_online(alias: &str)")
            .unwrap()..];
        let online: String = online[..online.find("\n}\n").unwrap()]
            .split_whitespace()
            .collect();
        let start = online
            .find("ifcrate::runtime::daemon::running_pid().is_none(){")
            .expect("只有 daemon 没在跑时才起");
        let restart = online.find("returndaemon_restart();").expect("没跑就起");
        let reload = online
            .find("letadmitted=agents_reload()?;")
            .expect("在跑就热加载");
        assert!(start < restart && restart < reload, "{online}");
    }
    use clap::CommandFactory as _;
    use summon_core::registry::Agent;

    /// 造一个**投递得出去**的 agent:`status: active` + 配了飞书凭据环境变量名。
    /// 少任何一项 `is_routable()` 就是 false,而 scheduler 的投递身份表正是按它筛的。
    fn agent(name: &str) -> Agent {
        serde_yaml::from_str(&format!(
            "name: {name}\ndisplay: {name} display\nstatus: active\nfeishu:\n  app_id_env: {name}_APP_ID\n"
        ))
        .unwrap()
    }

    /// registry 里有、但投递不出去(paused / 没配飞书凭据)的 agent。
    fn unroutable_agent(name: &str) -> Agent {
        serde_yaml::from_str(&format!("name: {name}\ndisplay: {name} display\n")).unwrap()
    }

    /// `schedules mv` 必须拒掉 registry 里没有的目标 —— 这是它相对**手改 `schedules.yaml`**
    /// 的核心价值。手改不会有任何校验,而交接给一个不存在的 agent 后果是**静默的**:
    /// scheduler 查不到它的 delivery scope 就跳过 tick、不报错,定时器变成不响也不报的哑弹,
    /// 通常要等某份日报没来才被发现。
    fn tail_record_line(text: &str, terminal: bool) -> Vec<u8> {
        let record: EventRecord = serde_json::from_value(serde_json::json!({
            "ts":1000,"seq":1,"kind":if terminal {"result"} else {"assistant"},"text":text
        }))
        .unwrap();
        let mut line = serde_json::to_vec(&record).unwrap();
        line.push(b'\n');
        line
    }

    #[test]
    fn tail_eof_can_age_then_flush_and_accept_another_unfinished_run() {
        let mut poller = TailPoller::new(summon_core::driver::CliDriver::Codex, false, true);
        let mut reader = std::io::Cursor::new(tail_record_line("first partial", false));
        assert!(matches!(
            poller.poll(&mut reader, || false).unwrap(),
            TailPoll::Events(_)
        ));
        assert!(matches!(
            poller.poll(&mut reader, || false).unwrap(),
            TailPoll::Waiting
        ));
        assert!(
            matches!(poller.poll(&mut reader, || true).unwrap(), TailPoll::Unfinished(block) if block.text == "first partial")
        );
        assert!(matches!(
            poller.poll(&mut reader, || true).unwrap(),
            TailPoll::Waiting
        ));
        reader
            .get_mut()
            .extend(tail_record_line("second partial", false));
        assert!(matches!(
            poller.poll(&mut reader, || false).unwrap(),
            TailPoll::Events(_)
        ));
        assert!(matches!(
            poller.poll(&mut reader, || false).unwrap(),
            TailPoll::Waiting
        ));
        assert!(
            matches!(poller.poll(&mut reader, || true).unwrap(), TailPoll::Unfinished(block) if block.text == "second partial")
        );
        let mut live = TailPoller::new(summon_core::driver::CliDriver::Codex, false, false);
        let mut live_reader = std::io::Cursor::new(tail_record_line("live partial", false));
        live.poll(&mut live_reader, || false).unwrap();
        assert!(matches!(
            live.poll(&mut live_reader, || true).unwrap(),
            TailPoll::Waiting
        ));
    }

    #[test]
    fn tail_partial_utf8_record_is_completed_before_rendering() {
        let mut poller = TailPoller::new(summon_core::driver::CliDriver::Codex, false, true);
        let line = tail_record_line("中文 fragment", false);
        let split = line
            .windows("中".len())
            .position(|part| part == "中".as_bytes())
            .unwrap()
            + 1;
        let mut reader = std::io::Cursor::new(line[..split].to_vec());
        assert!(matches!(
            poller.poll(&mut reader, || false).unwrap(),
            TailPoll::Waiting
        ));
        reader.get_mut().extend_from_slice(&line[split..]);
        assert!(
            matches!(poller.poll(&mut reader, || false).unwrap(), TailPoll::Events(events) if events.iter().any(|event| matches!(event, summon_core::event_log::TailOut::Live(block) if block.text == "中文 fragment")))
        );
    }

    #[test]
    fn tail_pipe_waits_for_final_text_while_tty_refreshes_one_preview_line() {
        use summon_core::event_log::{TailBlock, TailBlockKind, TailOut};
        for tty in [false, true] {
            let mut output = Vec::new();
            let mut pending = None;
            let first = TailBlock {
                ts: 1000,
                kind: TailBlockKind::Reply,
                text: "first ".into(),
            };
            write_tail_event(
                &mut output,
                tty,
                false,
                80,
                &mut pending,
                TailOut::Live(first.clone()),
            )
            .unwrap();
            if tty {
                assert!(String::from_utf8_lossy(&output).starts_with("\r\x1b[2K"));
            } else {
                assert!(output.is_empty());
            }
            write_tail_event(
                &mut output,
                tty,
                false,
                80,
                &mut pending,
                TailOut::LiveClear,
            )
            .unwrap();
            write_tail_event(
                &mut output,
                tty,
                false,
                80,
                &mut pending,
                TailOut::Line("tool read".into()),
            )
            .unwrap();
            let final_block = TailBlock {
                text: "first line\nsecond line".into(),
                ..first
            };
            write_tail_event(
                &mut output,
                tty,
                false,
                80,
                &mut pending,
                TailOut::Live(final_block.clone()),
            )
            .unwrap();
            write_tail_event(&mut output, tty, false, 80, &mut pending, TailOut::Frozen).unwrap();
            let output = String::from_utf8(output).unwrap();
            assert!(output.contains(&final_block.render(false)));
            assert!(pending.is_none());
            if !tty {
                assert_eq!(
                    output,
                    format!("tool read\n{}\n", final_block.render(false))
                );
            }
        }
    }

    #[test]
    fn schedules_mv_rejects_an_unknown_target_agent() {
        let registry = summon_core::registry::Registry {
            agents: vec![agent("remote-codex")],
        };
        assert!(ensure_move_target_is_known(&registry, "remote-codex").is_ok());

        let err = ensure_move_target_is_known(&registry, "devbox-typo")
            .expect_err("registry 里没有的 agent 必须拒");
        let msg = err.to_string();
        assert!(msg.contains("devbox-typo"), "报错要点出是哪个名字:{msg}");

        // registry 里有、但投递不出去的同样要拒 —— 只查「存在」的话它会通过校验,
        // 然后在 tick 里被静默跳过,正是这条错误文案承诺挡住的后果。
        let with_unroutable = summon_core::registry::Registry {
            agents: vec![unroutable_agent("paused-bot")],
        };
        assert!(
            ensure_move_target_is_known(&with_unroutable, "paused-bot").is_err(),
            "不 routable 的 agent 必须拒"
        );
        assert!(msg.contains("静默"), "报错要说明后果是静默不触发:{msg}");
    }

    #[test]
    fn daemon_process_name_uses_d_suffix() {
        assert_eq!(DAEMON_PROCESS_NAME, "summond");
    }

    #[test]
    fn claude_quota_cli_is_discoverable_but_statusline_callback_is_hidden() {
        for action in ["install", "status", "uninstall"] {
            let cli = Cli::try_parse_from(["summon", "daemon", "claude-quota", action])
                .expect("公开 Claude quota 管理命令应可解析");
            assert!(matches!(
                cli.cmd,
                Cmd::Daemon {
                    action: DaemonAction::ClaudeQuota { .. }
                }
            ));
        }
        assert!(Cli::try_parse_from(["summon", "daemon", "claude-quota-hook"]).is_err());

        assert!(matches!(
            Cli::try_parse_from(["summon", "daemon", "tool-guard-hook"])
                .unwrap()
                .cmd,
            Cmd::Daemon {
                action: DaemonAction::ToolGuardHook
            }
        ));

        let mut command = Cli::command();
        let daemon = command.find_subcommand_mut("daemon").unwrap();
        let help = daemon.render_long_help().to_string();
        assert!(help.contains("claude-quota"));
        assert!(!help.contains("claude-quota-statusline"));
        // hook 由会话专属 settings 调用,不是用户命令面的一部分
        assert!(!help.contains("tool-guard-hook"));
        assert!(!help.contains("permission-hook"));
        assert!(!help.contains("compact-hook"));
        assert!(matches!(
            Cli::try_parse_from([
                "summon",
                "daemon",
                "compact-hook",
                "--agent",
                "arcadia",
                "--session",
                "soul-arcadia-g1"
            ])
            .unwrap()
            .cmd,
            Cmd::Daemon {
                action: DaemonAction::CompactHook { ref agent, ref session }
            } if agent == "arcadia" && session == "soul-arcadia-g1"
        ));
        assert!(matches!(
            Cli::try_parse_from([
                "summon",
                "daemon",
                "permission-hook",
                "--agent",
                "arcadia",
                "--session",
                "soul-arcadia-g1"
            ])
            .unwrap()
            .cmd,
            Cmd::Daemon {
                action: DaemonAction::PermissionHook { ref agent, ref session }
            } if agent == "arcadia" && session == "soul-arcadia-g1"
        ));
    }

    #[test]
    fn sentinel_cli_parses_status_and_set() {
        let st = Cli::try_parse_from(["summon", "sentinel", "status", "--json"]).unwrap();
        assert!(matches!(
            st.cmd,
            Cmd::Sentinel {
                action: SentinelAction::Status { json: true }
            }
        ));
        let set = Cli::try_parse_from([
            "summon",
            "sentinel",
            "set",
            "--enabled",
            "true",
            "--dry-run",
            "false",
        ])
        .unwrap();
        match set.cmd {
            Cmd::Sentinel {
                action: SentinelAction::Set { enabled, dry_run },
            } => {
                assert_eq!(enabled, Some(true));
                assert_eq!(dry_run, Some(false));
            }
            _ => panic!("解析形态错了"),
        }
        // 空 set 在运行期报错(clap 层不拦),status 默认人读。
        let plain = Cli::try_parse_from(["summon", "sentinel", "status"]).unwrap();
        assert!(matches!(
            plain.cmd,
            Cmd::Sentinel {
                action: SentinelAction::Status { json: false }
            }
        ));
    }

    #[test]
    fn menubar_handled_parses_agent_tag_and_reply() {
        let cli = Cli::try_parse_from([
            "summon", "menubar", "handled", "--agent", "saturn", "--tag", "g1a2", "--reply",
            "om_1", "--json",
        ])
        .unwrap();
        match cli.cmd {
            Cmd::Menubar {
                action:
                    MenubarAction::Handled {
                        agent,
                        tag,
                        reply,
                        json,
                    },
            } => {
                assert!(json);
                assert_eq!(
                    (agent.as_str(), tag.as_str(), reply.as_str()),
                    ("saturn", "g1a2", "om_1")
                );
            }
            _ => panic!("应解析成 menubar handled"),
        }
    }

    #[test]
    fn menubar_settings_and_set_parse() {
        let cli = Cli::try_parse_from(["summon", "menubar", "settings", "--json"]).unwrap();
        assert!(matches!(
            cli.cmd,
            Cmd::Menubar {
                action: MenubarAction::Settings { json: true }
            }
        ));
        let cli = Cli::try_parse_from(["summon", "menubar", "set", "sentinel", "dry-run"]).unwrap();
        match cli.cmd {
            Cmd::Menubar {
                action: MenubarAction::Set { key, value },
            } => assert_eq!((key.as_str(), value.as_str()), ("sentinel", "dry-run")),
            _ => panic!("应解析成 menubar set"),
        }
    }

    /// Menu Bar 拼的就是这几个参数;默认是退出、遇到在跑的轮不动手。
    #[test]
    fn menubar_end_session_parses_and_defaults_to_exit_without_interrupt() {
        let parse = |extra: &[&str]| {
            let mut args = vec![
                "summon",
                "menubar",
                "end-session",
                "--agent",
                "saturn",
                "--tag",
                "g1a2",
            ];
            args.extend_from_slice(extra);
            match Cli::try_parse_from(args).unwrap().cmd {
                Cmd::Menubar {
                    action:
                        MenubarAction::EndSession {
                            agent,
                            tag,
                            archive,
                            interrupt,
                            json,
                        },
                } => {
                    assert_eq!((agent.as_str(), tag.as_str()), ("saturn", "g1a2"));
                    (archive, interrupt, json)
                }
                _ => panic!("应解析成 menubar end-session"),
            }
        };
        assert_eq!(parse(&[]), (false, false, false));
        assert_eq!(
            parse(&["--archive", "--interrupt", "--json"]),
            (true, true, true)
        );
    }

    /// 本机控制面:原生 Menu Bar 与 daemon 托管的网页控制台(`summon dashboard open`),
    /// 命令面上没有别的本机前端。
    #[test]
    fn local_frontends_are_the_menubar_and_the_dashboard_command() {
        for action in ["install", "restart", "uninstall", "status"] {
            let cli = Cli::try_parse_from(["summon", "menubar", action])
                .expect("原生 Menu Bar 管理命令应可解析");
            assert!(matches!(cli.cmd, Cmd::Menubar { .. }));
        }
        let sessions = Cli::try_parse_from(["summon", "menubar", "sessions", "--json"])
            .expect("Menu Bar 外部会话 JSON 契约应可解析");
        assert!(matches!(sessions.cmd, Cmd::Menubar { .. }));
        for (args, want_print) in [
            (&["summon", "dashboard", "open"][..], false),
            (&["summon", "dashboard", "open", "--print"][..], true),
        ] {
            let cli = Cli::try_parse_from(args).expect("dashboard open 应可解析");
            match cli.cmd {
                Cmd::Dashboard {
                    action: DashboardAction::Open { print },
                } => assert_eq!(print, want_print, "{args:?}"),
                _ => panic!("应解析成 dashboard open"),
            }
        }
        assert!(
            Cli::try_parse_from(["summon", "dashboard"]).is_err(),
            "要说明打开方式,裸命令不猜"
        );
        let help = Cli::command().render_long_help().to_string();
        assert_eq!(help.contains("menubar"), cfg!(target_os = "macos"));
        assert!(!help.contains("desktop"));
        let line = help
            .lines()
            .find(|line| line.trim_start().starts_with("dashboard"))
            .unwrap_or_else(|| panic!("顶层帮助要列出 dashboard:{help}"));
        assert!(line.contains("控制台"), "帮助要说明它打开的是控制台:{line}");
        let mut sub = Cli::command();
        let open_help = sub
            .find_subcommand_mut("dashboard")
            .and_then(|dash| dash.find_subcommand_mut("open"))
            .expect("dashboard open 子命令")
            .render_long_help()
            .to_string();
        assert!(open_help.contains("--print"), "{open_help}");
    }

    #[test]
    fn english_help_and_guides_render_without_changing_command_spellings() {
        summon_core::i18n::with_language(summon_core::i18n::Language::English, || {
            let help = Cli::command().render_long_help().to_string();
            assert!(help.contains("dashboard"));
            assert!(
                !help.chars().any(|c| ('\u{3400}'..='\u{9fff}').contains(&c)),
                "{help}"
            );
            let mut command = Cli::command();
            let help = command
                .find_subcommand_mut("preview")
                .unwrap()
                .render_long_help()
                .to_string();
            assert!(help.contains("--turn-id"));
            assert!(help.contains("preview"));
            assert!(
                !help.chars().any(|c| ('\u{3400}'..='\u{9fff}').contains(&c)),
                "{help}"
            );
            let cli = Cli::try_parse_from(["summon", "guide", "config"]).unwrap();
            assert!(matches!(cli.cmd, Cmd::Guide { .. }));
        });
    }

    /// `--print` 只把本机码链接打到 stdout(Menu Bar 拿它去开浏览器),不起浏览器;身份是本机
    /// owner,意图是 `DashboardOpen`。
    #[test]
    fn cli_dashboard_open_print_outputs_loopback_url() {
        use summon_core::core_port::{Actor, CommandOutcome, Intent};
        let url = "http://127.0.0.1:7767/local/0123abcd";
        let submit = |cmd: &summon_core::core_port::Command| {
            assert_eq!(cmd.actor, Actor::Owner);
            assert_eq!(cmd.intent, Intent::DashboardOpen);
            Ok(CommandOutcome::Value(serde_json::Value::String(url.into())))
        };
        let mut out = Vec::new();
        dashboard_open(true, submit, |_| panic!("--print 不开浏览器"), &mut out).unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), format!("{url}\n"));

        let mut opened = Vec::new();
        let mut out = Vec::new();
        dashboard_open(
            false,
            submit,
            |u| {
                opened.push(u.to_string());
                Ok(())
            },
            &mut out,
        )
        .unwrap();
        assert_eq!(opened, [url]);
        assert!(
            !String::from_utf8(out).unwrap().contains("/local/"),
            "交给浏览器打开时一次性码不出现在终端里"
        );

        let denied = dashboard_open(
            true,
            |_| Ok(CommandOutcome::Denied("控制台服务没有启动:配置关着".into())),
            |_| panic!("拒绝时不开浏览器"),
            &mut Vec::new(),
        )
        .unwrap_err();
        assert!(format!("{denied:#}").contains("控制台服务没有启动"));
        let odd = dashboard_open(
            false,
            |_| {
                Ok(CommandOutcome::Value(serde_json::Value::String(
                    "file:///etc/passwd".into(),
                )))
            },
            |_| panic!("不是 http 链接不交给系统打开"),
            &mut Vec::new(),
        );
        assert!(odd.is_err());
    }

    #[test]
    fn annotate_tag_line_shows_group_name_or_kind() {
        // tail 多流报错时,tag 要配人读名字,用户才知道哪个 tag 是哪个群。
        let grp = ChatMeta::parse("chat_type=group\nchat_name=panic分析\ntag=gb87\n");
        assert_eq!(annotate_tag_line("gb87", Some(&grp)), "gb87  「panic分析」");
        // 私聊无群名 → (私聊)
        let p2p = ChatMeta::parse("chat_type=p2p\nchat_name=\ntag=p49\n");
        assert_eq!(annotate_tag_line("p49", Some(&p2p)), "p49  (私聊)");
        // 群但没记到名字 → (群,无名)
        let noname = ChatMeta::parse("chat_type=group\ntag=gx\n");
        assert_eq!(annotate_tag_line("gx", Some(&noname)), "gx  (群,无名)");
        // 完全无 .meta(threads 孤儿)
        assert_eq!(
            annotate_tag_line("gz", None),
            "gz  (无 .meta 记录,可能是 threads 孤儿)"
        );
    }

    #[test]
    fn doctor_cli_cmds_dedups_by_kind() {
        // U3:doctor 按 routable agents 的 cli_kind 去重出待验可执行清单(不再硬编码只查 claude)。
        let mk = |name: &str, cli: &str| -> Agent {
            serde_yaml::from_str(&format!(
                "name: {name}\nstatus: active\nfeishu:\n  app_id_env: {name}_APP\nruntime:\n  cli: {cli}\n"
            ))
            .unwrap()
        };
        let reg = Registry {
            agents: vec![mk("a1", "claude"), mk("a2", "cursor"), mk("a3", "cursor")],
        };
        let cmds = doctor_cli_cmds(&reg);
        assert_eq!(cmds.len(), 2, "同 kind 去重");
        assert_eq!(cmds[0].0, "claude");
        assert_eq!(cmds[1].0, "cursor");
        assert!(!cmds[1].1.is_empty(), "应给出可执行名供 --version 验证");
    }

    /// 密钥形态的假值:测试要断言它们**不会**以任何形式进 doctor 输出。仓库不存真实密钥。
    const FAKE_DISK_FP: &str = "cafebabe00000000000000000000000000000000000000000000000000000001";
    const FAKE_DAEMON_FP: &str = "cafebabe00000000000000000000000000000000000000000000000000000002";

    fn runtime_with(credentials: &[(&str, &str)]) -> summon_core::fleet::RuntimeStatus {
        summon_core::fleet::RuntimeStatus {
            pid: 1,
            version: None,
            uptime_secs: 0,
            busy: Vec::new(),
            credentials: credentials
                .iter()
                .map(
                    |(agent, fp)| summon_core::credential_identity::AgentCredentialIdentity {
                        agent: (*agent).to_string(),
                        fingerprint: (*fp).to_string(),
                    },
                )
                .collect(),
            live_headless: Some(Vec::new()),
            subagents: Vec::new(),
        }
    }

    #[test]
    fn doctor_flags_credential_drift_with_restart_hint_and_no_secret_material() {
        // 真机缺陷:owner 换了 arcadia 的 app secret,daemon 内存还是旧的 → 该 agent 聋了,
        // 而 doctor 报全绿。现在必须显眼报出漂移,并给出可操作指令。
        let disk = vec![
            ("arcadia".to_string(), FAKE_DISK_FP.to_string()),
            ("starlight".to_string(), FAKE_DISK_FP.to_string()),
        ];
        let rt = runtime_with(&[("arcadia", FAKE_DAEMON_FP), ("starlight", FAKE_DISK_FP)]);
        let rows = classify_credential_rows(&disk, Some(&rt));
        assert_eq!(rows[0].1, CredentialIdentity::Drifted);
        assert_eq!(
            rows[1].1,
            CredentialIdentity::Match,
            "漂移必须是 per-agent 的"
        );

        let text = format_credential_identity_section(&rows);
        assert!(
            text.contains("已漂移"),
            "漂移必须显眼,不能混在 OK 行里:{text}"
        );
        assert!(text.contains("⚠️"));
        assert!(text.contains("arcadia"));
        assert!(
            text.contains("summon daemon restart"),
            "必须给出可操作指令:{text}"
        );
        assert!(text.contains("fail-closed"), "要说清后果:入站被掐");
        // 绝不打印任何密钥或其片段/摘要 —— 指纹连进渲染函数的机会都不该有。
        assert!(!text.contains(FAKE_DISK_FP));
        assert!(!text.contains(FAKE_DAEMON_FP));
        assert!(!text.contains("cafebabe"));
    }

    #[test]
    fn doctor_credential_section_is_all_ok_when_identities_match() {
        let disk = vec![("arcadia".to_string(), FAKE_DISK_FP.to_string())];
        let rt = runtime_with(&[("arcadia", FAKE_DISK_FP)]);
        let text = format_credential_identity_section(&classify_credential_rows(&disk, Some(&rt)));
        assert!(text.contains("OK"));
        assert!(!text.contains("⚠️"));
        assert!(
            !text.contains("summon daemon restart"),
            "没漂移就别劝人重启"
        );
        assert!(!text.contains(FAKE_DISK_FP));
    }

    #[test]
    fn doctor_credential_section_degrades_when_daemon_absent() {
        // daemon 不在 → 「无法确认」,既不报错也不假绿。
        let disk = vec![("arcadia".to_string(), FAKE_DISK_FP.to_string())];
        let text = format_credential_identity_section(&classify_credential_rows(&disk, None));
        assert!(text.contains("无法确认"), "{text}");
        assert!(text.contains("daemon 未运行"), "{text}");
        assert!(!text.contains("一致"), "不能把不可知写成一致:{text}");
        assert!(!text.contains(FAKE_DISK_FP));
    }

    #[test]
    fn doctor_credential_section_marks_agent_missing_from_daemon_report() {
        // 旧 daemon(不带该字段)或 dispatcher 尚未起:降级为无法确认,不借别人的结论。
        let disk = vec![("arcadia".to_string(), FAKE_DISK_FP.to_string())];
        let rt = runtime_with(&[("starlight", FAKE_DISK_FP)]);
        let rows = classify_credential_rows(&disk, Some(&rt));
        assert_eq!(
            rows[0].1,
            CredentialIdentity::Unknown(IdentityUnknown::NotReported)
        );
        let text = format_credential_identity_section(&rows);
        assert!(text.contains("无法确认"), "{text}");
    }

    #[test]
    fn agent_choices_show_names_and_live_state() {
        let reg = Registry {
            agents: vec![agent("coder"), agent("starlight")],
        };
        let text =
            format_agent_choices(&reg, &["soul-starlight".to_string()], true, ChatCmd::Follow);

        assert!(text.contains("可用 agents:"));
        assert!(text.contains("coder"));
        assert!(text.contains("starlight"));
        assert!(text.contains("live"));
    }

    #[test]
    fn chat_choices_show_base_and_per_chat_tags() {
        let tmp = std::env::temp_dir().join(format!("summon-cli-chat-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let mut a = agent("starlight");
        a.dir = tmp.join("agents").join(&a.name);
        std::fs::create_dir_all(a.state_dir().join("runtime/chats")).unwrap();
        std::fs::write(
            a.state_dir().join("runtime/chats/soul-starlight-gdeadbeef.meta"),
            "chat_id=oc_group\nchat_type=group\nchat_name=研发群\ntag=gdeadbeef\nlast_sender=老板\n",
        )
        .unwrap();
        let text = format_chat_choices(
            &a,
            &[
                "soul-starlight".to_string(),
                "soul-starlight-gdeadbeef".to_string(),
            ],
        );

        assert!(text.contains("会话:"));
        // base/owner 会话无 .meta,CHAT 列回退到 tag —— 否则它在表里是一行认不出主的 `-`。
        assert!(text.contains("(base/owner)"), "{text}");
        assert!(text.contains("gdeadbeef"));
        assert!(text.contains("研发群"));
        // claude=Transcript → 跟随命令用 attach(能力由指令本身表达)。
        assert!(text.contains("summon agents attach starlight --chat gdeadbeef"));
        // chat_id 不进表(设计取舍),更不该泄漏 .meta 的原始行。
        assert!(!text.contains("oc_group"), "chat_id 不该出现在表里: {text}");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn agent_choices_include_active_chat_details() {
        let tmp =
            std::env::temp_dir().join(format!("summon-cli-agent-chat-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let mut a = agent("starlight");
        let agent_dir = tmp.join("agents").join(&a.name);
        std::fs::create_dir_all(&agent_dir).unwrap();
        a.dir = agent_dir;
        let chats = a.state_dir().join("runtime/chats");
        std::fs::create_dir_all(&chats).unwrap();
        std::fs::write(
            chats.join("soul-starlight-gfeed.meta"),
            "chat_id=oc_feed\nchat_type=topic_group\nchat_name=项目群\ntag=gfeed\nlast_sender=老板\n",
        )
        .unwrap();
        let reg = Registry { agents: vec![a] };
        let text = format_agent_choices(
            &reg,
            &["soul-starlight-gfeed".to_string()],
            true,
            ChatCmd::Follow,
        );

        assert!(text.contains("gfeed"), "{text}");
        assert!(text.contains("项目群"), "{text}");
        assert!(!text.contains("oc_feed"), "chat_id 不该出现在表里: {text}");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// 表格的各条边框行(数据行 / 分隔线 / 上下边),用于比对齐。
    fn frame_lines(text: &str) -> Vec<usize> {
        use unicode_width::UnicodeWidthStr as _;
        text.lines()
            .filter(|l| l.starts_with(['│', '├', '┌', '└']))
            .map(|l| l.width())
            .collect()
    }

    #[test]
    fn agent_choices_render_aligned_table() {
        // 回归:中文群名按**显示宽**算(不是字符数),且无会话的 agent 行、会话行、跨列指令行
        // 必须同宽 —— 任一处算错右边框就犬牙交错。
        let tmp = std::env::temp_dir().join(format!("summon-cli-tbl-align-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let mut a = agent("starlight");
        a.dir = tmp.join("agents").join(&a.name);
        std::fs::create_dir_all(a.state_dir().join("runtime/chats")).unwrap();
        std::fs::write(
            a.state_dir().join("runtime/chats/soul-starlight-gcjk.meta"),
            "chat_id=oc_cjk\nchat_type=group\nchat_name=周末读书分享会\ntag=gcjk\n",
        )
        .unwrap();
        // coder 无任何会话 → 只占一行、不挂指令续行。
        let reg = Registry {
            agents: vec![a, agent("coder")],
        };
        let text = format_agent_choices(
            &reg,
            &["soul-starlight-gcjk".to_string()],
            true,
            ChatCmd::Follow,
        );

        let widths = frame_lines(&text);
        assert!(widths.len() > 5, "应渲染成表格: {text}");
        assert!(
            widths.windows(2).all(|w| w[0] == w[1]),
            "表格行宽不齐: {widths:?}\n{text}"
        );
        assert!(text.contains("周末读书分享会"), "{text}");
        assert!(text.contains("coder"), "无会话 agent 也该占一行: {text}");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn kill_picker_table_numbers_rows_from_one() {
        // 编号即 views 下标+1(选择时直接索引回去),表格化不能把这条对应关系打断。
        let a = agent("starlight");
        let views = [
            view_for(&a, "gone", "soul-starlight-gone"),
            view_for(&a, "gtwo", "soul-starlight-gtwo"),
        ];
        let now = std::time::SystemTime::now();
        let mut t = session_table(true);
        for (idx, v) in views.iter().enumerate() {
            t.push(session_row(
                &a,
                v,
                now,
                ChatCmd::Kill { reset: false },
                Some(idx + 1),
            ));
        }
        let text = t.render();
        assert!(
            text.lines().any(|l| l.starts_with("│ # │")),
            "应有序号列: {text}"
        );
        assert!(text.lines().any(|l| l.starts_with("│ 1 │")), "{text}");
        assert!(text.lines().any(|l| l.starts_with("│ 2 │")), "{text}");
        let widths = frame_lines(&text);
        assert!(
            widths.windows(2).all(|w| w[0] == w[1]),
            "带序号列时行宽不齐: {widths:?}\n{text}"
        );
    }

    fn cursor_agent(name: &str) -> Agent {
        serde_yaml::from_str(&format!("name: {name}\nruntime:\n  cli: cursor\n")).unwrap()
    }

    /// 构造一条 SessionView(尾行指令视角的单测用,不碰盘)。
    fn view_for(agent: &Agent, tag: &str, session: &str) -> summon_core::session_view::SessionView {
        summon_core::session_view::SessionView {
            agent: agent.name.clone(),
            tag: tag.to_string(),
            session: session.to_string(),
            chat_id: Some("oc_x".to_string()),
            chat_type: Some("group".to_string()),
            chat_name: Some("测试群".to_string()),
            capture: agent.capture_mode(),
            liveness: summon_core::session_view::Liveness::LiveTmux,
            last_active: None,
        }
    }

    /// 单条会话渲染成表格文本(续行指令类断言用)。
    fn row_text(
        agent: &Agent,
        view: &summon_core::session_view::SessionView,
        kind: ChatCmd,
    ) -> String {
        let mut t = session_table(false);
        t.push(session_row(
            agent,
            view,
            std::time::SystemTime::now(),
            kind,
            None,
        ));
        t.render()
    }

    #[test]
    fn kill_picker_suggests_kill_not_attach() {
        // 回归:kill 选择器与 agents ls 共用会话渲染,续行曾无条件给 attach/tail ——
        // 在「选择要关闭的会话」里教用户 attach 是反向操作。
        let a = agent("starlight");
        let v = view_for(&a, "gfeed", "soul-starlight-gfeed");

        let block = row_text(&a, &v, ChatCmd::Kill { reset: false });
        assert!(
            block.contains("summon agents kill soul-starlight-gfeed"),
            "kill 视角应给 kill 指令: {block}"
        );
        assert!(
            !block.contains("summon agents attach"),
            "kill 视角不该建议 attach: {block}"
        );
        // 表格化后会话**能力**(attach|tail vs tail)由续行指令本身表达,不再单列一列;
        // CAP 列改载 agent 级的 capture/manual 配置。
        assert!(block.contains("capture"), "{block}");

        // ls/attach 视角不受影响,仍给跟随指令。
        let follow = row_text(&a, &v, ChatCmd::Follow);
        assert!(follow.contains("summon agents attach starlight --chat gfeed"));
    }

    /// 造一个「同群多 agent」registry:两个 agent 都在 oc_same 群里 → tag 必然相同。
    /// 这是真实拓扑(实测 g00000000a11ce04d 跨 4 个 agent),不是构造出来的边角。
    fn same_group_reg(tmp: &std::path::Path) -> Registry {
        let tag = summon_core::registry::chat_tag("group", "oc_same");
        let mut agents = Vec::new();
        for name in ["arcadia", "starlight"] {
            let mut a = agent(name);
            // agents_dir 落在 tempdir 子目录,state_root 才会在 tempdir 内(不跨测试串号)
            a.dir = tmp.join("agents").join(name);
            let state_dir = a.state_dir();
            std::fs::create_dir_all(state_dir.join("runtime/chats")).unwrap();
            std::fs::write(
                state_dir
                    .join("runtime/chats")
                    .join(format!("soul-{name}-{tag}.meta")),
                format!("chat_id=oc_same\nchat_type=group\nchat_name=同一个群\ntag={tag}\n"),
            )
            .unwrap();
            agents.push(a);
        }
        Registry { agents }
    }

    #[test]
    fn bare_tag_hitting_many_agents_resolves_ambiguous_not_first_or_all() {
        // 红线回归:tag/chat_id 是相对名,同群每个 agent 各有一条同 tag 会话。
        // 旧行为 —— open 用 .find() 静默取首条、kill 用 .filter() 全杀 —— 都是拿相对名当绝对名。
        let tmp = std::env::temp_dir().join(format!("summon-cli-ambig-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let reg = same_group_reg(&tmp);
        let tag = summon_core::registry::chat_tag("group", "oc_same");

        // 裸 tag → 歧义(2 个 agent)。
        match resolve_session_target(&reg, &[], &tag) {
            Resolved::Ambiguous(c) => assert_eq!(c.len(), 2, "同群两 agent 应都命中"),
            other => panic!("裸 tag 应报歧义,实得 {other:?}"),
        }
        // chat_id 同理 —— 它也是相对名。
        match resolve_session_target(&reg, &[], "oc_same") {
            Resolved::Ambiguous(c) => assert_eq!(c.len(), 2),
            other => panic!("裸 chat_id 应报歧义,实得 {other:?}"),
        }
        // 会话名是绝对名 → 唯一命中,且命中的就是它自己那个 agent。
        match resolve_session_target(&reg, &[], &format!("soul-starlight-{tag}")) {
            Resolved::Unique(v) => assert_eq!(v.agent, "starlight"),
            other => panic!("会话名应唯一命中,实得 {other:?}"),
        }
        match resolve_session_target(&reg, &[], "soul-nobody-gdead") {
            Resolved::NotFound => {}
            other => panic!("未知 target 应 NotFound,实得 {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn ambiguous_message_lists_only_candidates_with_absolute_names() {
        // 歧义报错要答「这几条里选一条」,而不是把全 fleet 糊上来;每条给会话名(绝对名),照抄即消歧。
        let tmp = std::env::temp_dir().join(format!("summon-cli-ambig-msg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let reg = same_group_reg(&tmp);
        let tag = summon_core::registry::chat_tag("group", "oc_same");
        let Resolved::Ambiguous(c) = resolve_session_target(&reg, &[], &tag) else {
            panic!("应歧义");
        };

        let msg = ambiguous_message(&reg, &tag, &c, ChatCmd::Kill { reset: false });
        assert!(msg.contains("命中 2 条"), "{msg}");
        assert!(msg.contains("跨 2 个 agent"), "{msg}");
        // 两条候选各给自己的绝对名 kill 指令。
        assert!(
            msg.contains(&format!("summon agents kill soul-arcadia-{tag}")),
            "{msg}"
        );
        assert!(
            msg.contains(&format!("summon agents kill soul-starlight-{tag}")),
            "{msg}"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn gone_session_hint_suggests_session_name_not_tag() {
        // 回归:这条提示原本给 `kill <tag> --reset`,而 tag 跨 agent 会撞 —— resolver 严格化后
        // 照抄必被判歧义。凡是**建议出去的 kill 指令**都必须是绝对名,否则提示自己就跑不通。
        let a = agent("starlight");
        let mut v = view_for(&a, "gffa", "soul-starlight-gffa");
        v.liveness = summon_core::session_view::Liveness::Gone;

        let hint = gone_session_hint(&v);
        assert!(
            hint.contains("summon agents kill soul-starlight-gffa --reset"),
            "应给绝对名: {hint}"
        );
        assert!(
            !hint.contains("kill gffa "),
            "绝不能建议裸 tag(会被判歧义): {hint}"
        );
    }

    #[test]
    fn kill_cmd_targets_session_name_and_carries_reset() {
        // kill 指令直指**会话名**而非 tag:同群两 agent 的 tag 相同,照抄 tag 会连别的 agent 一起杀。
        let a = agent("starlight");
        let v = view_for(&a, "gfeed", "soul-starlight-gfeed");
        assert_eq!(
            chat_cmd(&a, &v, ChatCmd::Kill { reset: false }),
            "summon agents kill soul-starlight-gfeed"
        );
        // --reset 透传:照抄尾行 == 本次调用,不悄悄降级成不重置。
        assert_eq!(
            chat_cmd(&a, &v, ChatCmd::Kill { reset: true }),
            "summon agents kill soul-starlight-gfeed --reset"
        );
        // 裸名(base/owner)会话也用会话名寻址 —— tag 本身不是合法 CLI 参数。
        let base = view_for(&a, "(base/owner)", "soul-starlight");
        assert_eq!(
            chat_cmd(&a, &base, ChatCmd::Kill { reset: false }),
            "summon agents kill soul-starlight"
        );
    }

    #[test]
    fn kill_picker_suggests_kill_for_headless_too() {
        // 无头会话在 ls 里给 tail,kill 视角同样必须给 kill(不是 tail)。
        let a = cursor_agent("nezha");
        let v = view_for(&a, "gbeef", "soul-nezha-gbeef");
        let block = row_text(&a, &v, ChatCmd::Kill { reset: false });
        assert!(
            block.contains("summon agents kill soul-nezha-gbeef"),
            "{block}"
        );
        assert!(!block.contains("summon agents tail"), "{block}");
    }

    #[test]
    fn read_chat_meta_parses_updated_at() {
        let tmp = std::env::temp_dir().join(format!("summon-cli-meta-ts-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let mut a = agent("starlight");
        a.dir = tmp.join("agents").join(&a.name);
        std::fs::create_dir_all(a.state_dir().join("runtime/chats")).unwrap();
        std::fs::write(
            a.state_dir()
                .join("runtime/chats/soul-starlight-gdeadbeef.meta"),
            "chat_id=oc_g\nchat_type=group\ntag=gdeadbeef\nupdated_at=2026-06-03 12:00:00\n",
        )
        .unwrap();
        let meta = read_chat_meta(&a, "soul-starlight-gdeadbeef");
        assert_eq!(meta.updated_at.as_deref(), Some("2026-06-03 12:00:00"));
        assert!(parse_meta_updated_at("2026-06-03 12:00:00").is_some());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn read_chat_meta_updated_at_absent_is_none() {
        // 旧 .meta 无 updated_at 行 → None(向后兼容)。
        let tmp = std::env::temp_dir().join(format!("summon-cli-meta-old-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let mut a = agent("starlight");
        a.dir = tmp.join("agents").join(&a.name);
        std::fs::create_dir_all(a.state_dir().join("runtime/chats")).unwrap();
        std::fs::write(
            a.state_dir().join("runtime/chats/soul-starlight-gold.meta"),
            "chat_id=oc_g\nchat_type=group\ntag=gold\n",
        )
        .unwrap();
        let meta = read_chat_meta(&a, "soul-starlight-gold");
        assert_eq!(meta.updated_at, None);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn headless_session_shows_in_ls_without_tmux() {
        // 核心回归:无头 cursor 会话无 tmux,但盘上 .meta + events.jsonl 在 → 仍列出,标 tail。
        let tmp = std::env::temp_dir().join(format!("summon-cli-headless-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let mut a = cursor_agent("nezha");
        a.dir = tmp.join("agents").join(&a.name);
        std::fs::create_dir_all(a.state_dir().join("runtime/chats")).unwrap();
        std::fs::create_dir_all(a.state_dir().join("threads/gbeef")).unwrap();
        std::fs::write(a.state_dir().join("threads/gbeef/events.jsonl"), "{}\n").unwrap();
        std::fs::write(
            a.state_dir().join("runtime/chats/soul-nezha-gbeef.meta"),
            "chat_id=oc_beef\nchat_type=group\nchat_name=封神群\ntag=gbeef\nupdated_at=2026-06-03 12:00:00\n",
        )
        .unwrap();

        let now = std::time::SystemTime::now();
        let ttl = std::time::Duration::from_secs(86400);
        // tmux 集合为空(无头无 tmux),仍应发现该会话。
        let views = discover_sessions(&a, &[], now, ttl);
        assert_eq!(views.len(), 1);
        let v = &views[0];
        assert_eq!(v.tag, "gbeef");
        assert_eq!(v.chat_id.as_deref(), Some("oc_beef"));
        // cursor 默认走 Headless CLI Session。它与 Rpc/Subprocess 在「无 tmux、只能 tail」
        // 这件事上同类，新增一等 transport 不应改变对外能力契约。
        assert_eq!(v.capture, summon_core::registry::CaptureMode::Rpc);
        // events.jsonl 刚写 → 最近活跃 → RecentHeadless;非 tmux 的 Gone。
        assert_eq!(
            v.liveness,
            summon_core::session_view::Liveness::RecentHeadless
        );

        let block = row_text(&a, v, ChatCmd::Follow);
        // 无头会话不能 attach —— 续行给 tail,这就是「能力」的表达(表格无独立 cap 列)。
        assert!(
            block.contains("summon agents tail nezha --chat gbeef"),
            "{block}"
        );
        assert!(!block.contains("summon agents attach"), "{block}");
        assert!(block.contains("封神群"), "群名应进 CHAT 列: {block}");
        // chat_id 不进表(设计取舍):35 字符占近半屏宽,而续行的 tag 已能唯一寻址。
        assert!(
            !block.contains("oc_beef"),
            "chat_id 不该出现在表里: {block}"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn agent_choices_flag_no_active_sessions_when_all_empty() {
        // C16 回归:裸 `agents ls` 删掉 tmux 空集 early-return 后,全空由 format_agent_choices 判定。
        // 无任何会话时必须给「无活跃会话」提示,而非只静默列一排 agent 名。
        let reg = Registry {
            agents: vec![cursor_agent("nezha")],
        };
        let text = format_agent_choices(&reg, &[], false, ChatCmd::Follow);
        assert!(
            text.contains("无活跃会话"),
            "全空应给提示,而非静默列 agent: {text}"
        );
    }

    #[test]
    fn agent_choices_list_headless_recent_without_tmux() {
        // C16 回归:纯无头 fleet 无任何 soul-* tmux,但盘上 recent 会话仍应被列出——
        // 裸 ls 据此不再误报「无活跃会话」(旧 early-return 用 tmux 空集直接返回)。
        let tmp =
            std::env::temp_dir().join(format!("summon-cli-c16-recent-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let mut a = cursor_agent("nezha");
        a.dir = tmp.join("agents").join(&a.name);
        std::fs::create_dir_all(a.state_dir().join("runtime/chats")).unwrap();
        std::fs::create_dir_all(a.state_dir().join("threads/gbeef")).unwrap();
        std::fs::write(a.state_dir().join("threads/gbeef/events.jsonl"), "{}\n").unwrap();
        std::fs::write(
            a.state_dir().join("runtime/chats/soul-nezha-gbeef.meta"),
            "chat_id=oc_beef\nchat_type=group\nchat_name=封神群\ntag=gbeef\nupdated_at=2026-06-03 12:00:00\n",
        )
        .unwrap();
        let reg = Registry { agents: vec![a] };
        // tmux 集合为空(无头无 tmux),仍应列出 recent 会话。
        let text = format_agent_choices(&reg, &[], false, ChatCmd::Follow);
        assert!(text.contains("gbeef"), "recent 无头会话应被列出: {text}");
        assert!(
            !text.contains("无活跃会话"),
            "有 recent 会话时不该报无活跃: {text}"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// 回归:owner 私聊会话用**裸名** `soul-<alias>`(向后兼容),但其 `.meta.tag` 仍是 `p<8hex>`
    /// (ls 据此显示 + 建议 `--chat <tag>`)。`attach --chat <那个 tag>` 必须经 discover_sessions
    /// 反查回裸名,而不是naively拼 `<base>-<tag>`(那条不存在 → 用户遇到的「会话 …-<tag> 未起」)。
    #[test]
    fn attach_chat_resolves_owner_p2p_bare_session_to_bare_name() {
        let tmp = std::env::temp_dir().join(format!("summon-cli-bare-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let mut a = agent("starlight");
        a.dir = tmp.join("agents").join(&a.name);
        std::fs::create_dir_all(a.state_dir().join("runtime/chats")).unwrap();
        // 裸名会话的 .meta:文件名是裸 `soul-starlight.meta`(无 -<tag> 后缀),meta.tag 仍是 p<8hex>。
        std::fs::write(
            a.state_dir().join("runtime/chats/soul-starlight.meta"),
            "chat_id=oc_priv\nchat_type=p2p\ntag=p00000000a11ce04e\nupdated_at=2026-06-03 12:00:00\n",
        )
        .unwrap();
        // 反查 tag → 真实裸名 `soul-starlight`(不是 `soul-starlight-p00000000a11ce04e`)。
        let got = session_name_for_tag(&a, "p00000000a11ce04e", &["soul-starlight".to_string()]);
        assert_eq!(
            got.as_deref(),
            Some("soul-starlight"),
            "owner 私聊裸名会话应被 tag 反查命中,而非拼 <base>-<tag>"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn down_chat_resolves_owner_bare_session_not_naive_concat() {
        // C17 回归:down <alias> --chat p<hex> 的 kill 目标与 reset 记录名,必须经 discover_sessions
        // 反查回真实裸名 soul-<alias>,而非 naive 拼 <base>-p<hex>(不存在 → 杀不掉、清错记录、假成功)。
        // 直接断言 down() 复用的纯函数 down_targets_and_records —— 把该函数的解析拼回去测试即红,
        // 真正钉死 down() 的行为(不再复读 session_name_for_tag)。
        let tmp = std::env::temp_dir().join(format!("summon-cli-c17-down-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let mut a = agent("starlight");
        a.dir = tmp.join("agents").join(&a.name);
        std::fs::create_dir_all(a.state_dir().join("runtime/chats")).unwrap();
        std::fs::write(
            a.state_dir().join("runtime/chats/soul-starlight.meta"),
            "chat_id=oc_priv\nchat_type=p2p\ntag=p00000000a11ce04e\nupdated_at=2026-06-03 12:00:00\n",
        )
        .unwrap();
        let (targets, records) = down_targets_and_records(
            &a,
            Some("p00000000a11ce04e"),
            &["soul-starlight".to_string()],
        );
        assert_eq!(
            targets,
            vec!["soul-starlight".to_string()],
            "kill 目标应反查到真实裸名会话,而非拼 <base>-<tag>"
        );
        // reset 记录名与 kill 目标共用同一解析结果 —— 保证「杀谁就清谁」。
        assert_eq!(records, targets, "reset 记录名应与 kill 目标同一解析结果");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn down_reset_clears_real_record_and_reports_zero_when_absent() {
        // C17 回归:down --reset 的「已清除」回执必须据实——只对真删了文件的记录计数。
        // ① 无对应 .session 时 cleared==0(不谎报已清、下次仍续接旧上下文的假成功);
        // ② 反查到裸名 soul-<alias> 后清的是真实记录 soul-<alias>.session(cleared==1)。
        // 若把解析拼回 <base>-<tag>,①仍 0 但 ②会清错名字 → cleared==1 断言变红。
        let tmp = std::env::temp_dir().join(format!("summon-cli-c17-reset-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let mut a = agent("starlight");
        a.dir = tmp.join("agents").join(&a.name);
        std::fs::create_dir_all(a.state_dir().join("runtime/chats")).unwrap();
        std::fs::write(
            a.state_dir().join("runtime/chats/soul-starlight.meta"),
            "chat_id=oc_priv\nchat_type=p2p\ntag=p00000000a11ce04e\nupdated_at=2026-06-03 12:00:00\n",
        )
        .unwrap();
        let live = ["soul-starlight".to_string()];
        // ① 尚无 .session 记录:records 解析到裸名,但文件不存在 → cleared==0。
        let (_t, records) = down_targets_and_records(&a, Some("p00000000a11ce04e"), &live);
        let cleared_absent = records
            .iter()
            .filter(|s| resume_store::clear(&a, s))
            .count();
        assert_eq!(cleared_absent, 0, "无对应 .session 记录时不能谎报已清除");
        // ② 造出真实记录 soul-starlight.session,再清 → cleared==1 且文件被真删。
        let rec_path = a.state_dir().join("runtime/chats/soul-starlight.session");
        std::fs::write(&rec_path, "cli=claude\nid=abc\n").unwrap();
        let (_t2, records2) = down_targets_and_records(&a, Some("p00000000a11ce04e"), &live);
        let cleared_real = records2
            .iter()
            .filter(|s| resume_store::clear(&a, s))
            .count();
        assert_eq!(
            cleared_real, 1,
            "反查到裸名后应清掉真实记录 soul-starlight.session"
        );
        assert!(!rec_path.exists(), "真实 resume 记录应被删除");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn discover_sessions_unions_disk_and_threads_only_orphan() {
        // .meta 会话 + threads-only 孤儿(无 .meta)都应出现;孤儿 chat_id 未知、type 从 tag 推。
        let tmp = std::env::temp_dir().join(format!("summon-cli-union-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let mut a = cursor_agent("nezha");
        a.dir = tmp.join("agents").join(&a.name);
        std::fs::create_dir_all(a.state_dir().join("runtime/chats")).unwrap();
        std::fs::write(
            a.state_dir().join("runtime/chats/soul-nezha-gmeta.meta"),
            "chat_id=oc_meta\nchat_type=group\ntag=gmeta\n",
        )
        .unwrap();
        // threads-only 孤儿:p 前缀,无 .meta。
        std::fs::create_dir_all(a.state_dir().join("threads/porphan")).unwrap();
        std::fs::write(a.state_dir().join("threads/porphan/events.jsonl"), "{}\n").unwrap();

        let now = std::time::SystemTime::now();
        let views = discover_sessions(&a, &[], now, std::time::Duration::from_secs(86400));
        let tags: Vec<&str> = views.iter().map(|v| v.tag.as_str()).collect();
        assert!(tags.contains(&"gmeta"));
        assert!(tags.contains(&"porphan"));
        let orphan = views.iter().find(|v| v.tag == "porphan").unwrap();
        assert_eq!(orphan.chat_id, None); // chat_id 未知
        assert_eq!(orphan.chat_type.as_deref(), Some("p2p")); // 从 tag 首字符推
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn last_active_prefers_events_mtime_over_meta() {
        // events.jsonl mtime(刚写,now 附近)应优先于 .meta.updated_at(写成很久以前)。
        let tmp =
            std::env::temp_dir().join(format!("summon-cli-lastactive-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let mut a = cursor_agent("nezha");
        a.dir = tmp.join("agents").join(&a.name);
        std::fs::create_dir_all(a.state_dir().join("threads/gx")).unwrap();
        std::fs::write(a.state_dir().join("threads/gx/events.jsonl"), "{}\n").unwrap();
        let meta = ChatMeta {
            updated_at: Some("2000-01-01 00:00:00".to_string()), // 远古回退源
            ..Default::default()
        };

        let la = session_last_active(&a, "gx", &meta).expect("应有 last_active");
        // mtime 必在最近(几秒内),远古回退不该被采用。
        let now = std::time::SystemTime::now();
        let age = now.duration_since(la).unwrap_or_default();
        assert!(age.as_secs() < 60, "应取 events.jsonl mtime 而非远古 meta");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn attach_target_resolves_chat_id_to_live_session() {
        let tmp = std::env::temp_dir().join(format!("summon-cli-attach-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let mut a = agent("starlight");
        a.dir = tmp.join("agents").join(&a.name);
        std::fs::create_dir_all(a.state_dir().join("runtime/chats")).unwrap();
        std::fs::write(
            a.state_dir().join("runtime/chats/soul-starlight-gdeadbeef.meta"),
            "chat_id=oc_group\nchat_type=group\nchat_name=研发群\ntag=gdeadbeef\nlast_sender=老板\n",
        )
        .unwrap();
        let reg = Registry { agents: vec![a] };

        let target = resolve_attach_target(
            &reg,
            "oc_group",
            None,
            &["soul-starlight-gdeadbeef".to_string()],
        )
        .unwrap();

        match target {
            AttachTarget::Chat { view } => assert_eq!(view.session, "soul-starlight-gdeadbeef"),
            AttachTarget::Agent { .. } => panic!("expected chat target"),
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn attach_chat_id_carries_capture_for_headless_downgrade() {
        // C18 回归:直指 chat_id 命中无头会话时,resolve_attach_target 返回带 capture=CliSession 的
        // SessionView,attach 据此降级 tail(与按 agent 名的 headless 分支一致),不再 bail「未知 agent/chat」。
        // 旧实现候选来自 tmux-only chat_entries,无头会话无 tmux → 恒 miss → 报「未知」自相矛盾。
        let tmp =
            std::env::temp_dir().join(format!("summon-cli-c18-headless-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let mut a = cursor_agent("nezha");
        a.dir = tmp.join("agents").join(&a.name);
        std::fs::create_dir_all(a.state_dir().join("runtime/chats")).unwrap();
        std::fs::create_dir_all(a.state_dir().join("threads/gbeef")).unwrap();
        std::fs::write(a.state_dir().join("threads/gbeef/events.jsonl"), "{}\n").unwrap();
        std::fs::write(
            a.state_dir().join("runtime/chats/soul-nezha-gbeef.meta"),
            "chat_id=oc_beef\nchat_type=group\nchat_name=封神群\ntag=gbeef\nupdated_at=2026-06-03 12:00:00\n",
        )
        .unwrap();
        let reg = Registry { agents: vec![a] };
        // 无任何 tmux,直指 chat_id。
        let target = resolve_attach_target(&reg, "oc_beef", None, &[]).unwrap();
        match target {
            AttachTarget::Chat { view } => {
                assert_eq!(view.capture, summon_core::registry::CaptureMode::Rpc);
                assert_eq!(view.tag, "gbeef");
                assert_eq!(view.agent, "nezha");
            }
            AttachTarget::Agent { .. } => panic!("expected chat target for headless chat_id"),
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn attach_target_keeps_agent_name_resolution() {
        let reg = Registry {
            agents: vec![agent("starlight")],
        };
        let target = resolve_attach_target(&reg, "starlight", None, &[]).unwrap();

        match target {
            AttachTarget::Agent { session, .. } => assert_eq!(session, "soul-starlight"),
            AttachTarget::Chat { .. } => panic!("expected agent target"),
        }
    }

    #[test]
    fn unknown_agent_error_lists_choices() {
        let reg = Registry {
            agents: vec![agent("starlight")],
        };
        let text = unknown_agent_message(&reg, "ghost", &[]);

        assert!(text.contains("未知 agent: ghost"));
        assert!(text.contains("starlight"));
    }

    #[test]
    fn missing_chat_error_lists_choices() {
        let a = agent("starlight");
        let text = missing_chat_message(&a, "gmissing", &["soul-starlight-gdeadbeef".to_string()]);

        assert!(text.contains("会话 soul-starlight-gmissing 未起"));
        assert!(text.contains("gdeadbeef"));
    }

    #[test]
    fn agents_positional_ignores_chat_value_when_agent_is_missing() {
        // 形如 `summon agents attach --chat gdeadbeef`(漏了 target):跳过 --chat 值后无实参
        let args = vec![
            "summon".to_string(),
            "agents".to_string(),
            "attach".to_string(),
            "--chat".to_string(),
            "gdeadbeef".to_string(),
        ];
        assert_eq!(agents_positional(&args), None);

        // 给了 target 则取到它(--chat 在前也跳过)
        let args = vec![
            "summon".to_string(),
            "agents".to_string(),
            "attach".to_string(),
            "starlight".to_string(),
        ];
        assert_eq!(agents_positional(&args), Some("starlight"));
    }

    #[test]
    fn agents_nests_all_session_verbs() {
        // 裸 agents 合法(= ls);旧顶层别名一律不再可用
        assert!(matches!(
            Cli::try_parse_from(["summon", "agents"]).unwrap().cmd,
            Cmd::Agents { action: None }
        ));
        for legacy in [
            ["summon", "up", "x"],
            ["summon", "down", "x"],
            ["summon", "attach", "x"],
            ["summon", "ls", "x"],
        ] {
            assert!(
                Cli::try_parse_from(legacy).is_err(),
                "legacy top-level {legacy:?} should be gone"
            );
        }

        let cli = Cli::try_parse_from(["summon", "agents", "ls"]).unwrap();
        assert!(matches!(
            cli.cmd,
            Cmd::Agents {
                action: Some(AgentsAction::Ls {
                    agent: None,
                    all: false
                })
            }
        ));

        let disable = Cli::try_parse_from(["summon", "agents", "disable", "agent-a"]).unwrap();
        assert!(matches!(disable.cmd, Cmd::Agents {
            action: Some(AgentsAction::Disable { agent }),
        } if agent == "agent-a"));
        let enable = Cli::try_parse_from(["summon", "agents", "enable", "agent-a"]).unwrap();
        assert!(matches!(enable.cmd, Cmd::Agents {
            action: Some(AgentsAction::Enable { agent }),
        } if agent == "agent-a"));
        assert!(Cli::try_parse_from(["summon", "agents", "disable"]).is_err());
        let cli = Cli::try_parse_from(["summon", "agents", "reload"]).unwrap();
        assert!(matches!(
            cli.cmd,
            Cmd::Agents {
                action: Some(AgentsAction::Reload)
            }
        ));

        // 改默认模型 / effort 不必再手改 ~/.summon:带值 = 写,省略值 = 只读(打印当前值+候选)。
        let cli = Cli::try_parse_from([
            "summon",
            "agents",
            "model",
            "black-alchemist",
            "gpt-5.6-terra",
        ])
        .unwrap();
        match cli.cmd {
            Cmd::Agents {
                action: Some(AgentsAction::Model { agent, model }),
            } => {
                assert_eq!(agent, "black-alchemist");
                assert_eq!(model.as_deref(), Some("gpt-5.6-terra"));
            }
            _ => panic!("expected agents model"),
        }
        let cli = Cli::try_parse_from(["summon", "agents", "model", "starlight"]).unwrap();
        match cli.cmd {
            Cmd::Agents {
                action: Some(AgentsAction::Model { agent, model }),
            } => {
                assert_eq!(agent, "starlight");
                assert_eq!(model, None); // 只读
            }
            _ => panic!("expected agents model 只读形态"),
        }
        let cli = Cli::try_parse_from(["summon", "agents", "effort", "black-alchemist", "ultra"])
            .unwrap();
        match cli.cmd {
            Cmd::Agents {
                action: Some(AgentsAction::Effort { agent, effort }),
            } => {
                assert_eq!(agent, "black-alchemist");
                assert_eq!(effort.as_deref(), Some("ultra"));
            }
            _ => panic!("expected agents effort"),
        }

        let cli = Cli::try_parse_from(["summon", "agents", "up", "blue-wing"]).unwrap();
        match cli.cmd {
            Cmd::Agents {
                action: Some(AgentsAction::Up { agent }),
            } => assert_eq!(agent, "blue-wing"),
            _ => panic!("expected agents up"),
        }

        let cli =
            Cli::try_parse_from(["summon", "agents", "down", "starlight", "--reset"]).unwrap();
        match cli.cmd {
            Cmd::Agents {
                action: Some(AgentsAction::Down { agent, chat, reset }),
            } => {
                assert_eq!(agent, "starlight");
                assert_eq!(chat, None);
                assert!(reset);
            }
            _ => panic!("expected agents down"),
        }

        let cli =
            Cli::try_parse_from(["summon", "agents", "attach", "starlight", "--chat", "gabc"])
                .unwrap();
        match cli.cmd {
            Cmd::Agents {
                action:
                    Some(AgentsAction::Attach {
                        target,
                        chat,
                        from_start,
                    }),
            } => {
                assert_eq!(target.as_deref(), Some("starlight"));
                assert_eq!(chat.as_deref(), Some("gabc"));
                assert!(!from_start);
            }
            _ => panic!("expected agents attach"),
        }

        // attach 接受 --from-start(透传给无头降级的 tail);target 可省(→ 列出可选)。
        let cli = Cli::try_parse_from(["summon", "agents", "attach", "blue-wing", "--from-start"])
            .unwrap();
        match cli.cmd {
            Cmd::Agents {
                action:
                    Some(AgentsAction::Attach {
                        target, from_start, ..
                    }),
            } => {
                assert_eq!(target.as_deref(), Some("blue-wing"));
                assert!(from_start, "--from-start 应被 attach 接受");
            }
            _ => panic!("expected agents attach --from-start"),
        }
        let cli = Cli::try_parse_from(["summon", "agents", "attach"]).unwrap();
        match cli.cmd {
            Cmd::Agents {
                action: Some(AgentsAction::Attach { target, .. }),
            } => assert!(target.is_none(), "attach 不给名字应可解析(→ 列表)"),
            _ => panic!("expected bare agents attach"),
        }

        let cli = Cli::try_parse_from(["summon", "agents", "kill", "oc_group"]).unwrap();
        match cli.cmd {
            Cmd::Agents {
                action: Some(AgentsAction::Kill { target, reset }),
            } => {
                assert_eq!(target, "oc_group");
                assert!(!reset, "默认 Clean(不 reset)");
            }
            _ => panic!("expected agents kill"),
        }

        let cli = Cli::try_parse_from(["summon", "agents", "kill", "oc_group", "--reset"]).unwrap();
        match cli.cmd {
            Cmd::Agents {
                action: Some(AgentsAction::Kill { target, reset }),
            } => {
                assert_eq!(target, "oc_group");
                assert!(reset, "--reset 映射 Reset");
            }
            _ => panic!("expected agents kill --reset"),
        }

        let cli = Cli::try_parse_from(["summon", "agents", "status", "--json"]).unwrap();
        match cli.cmd {
            Cmd::Agents {
                action: Some(AgentsAction::Status { json }),
            } => assert!(json, "--json 是 Menu Bar 的消费形态"),
            _ => panic!("expected agents status --json"),
        }

        let cli = Cli::try_parse_from(["summon", "agents", "status"]).unwrap();
        match cli.cmd {
            Cmd::Agents {
                action: Some(AgentsAction::Status { json }),
            } => assert!(!json, "裸 status = 人读"),
            _ => panic!("expected agents status"),
        }

        let cli = Cli::try_parse_from([
            "summon",
            "agents",
            "history",
            "soul-x-g1a2b3c4",
            "--limit",
            "25",
            "--before",
            "80",
            "--json",
        ])
        .unwrap();
        match cli.cmd {
            Cmd::Agents {
                action:
                    Some(AgentsAction::History {
                        target,
                        limit,
                        before,
                        json,
                    }),
            } => {
                assert_eq!(target, "soul-x-g1a2b3c4");
                assert_eq!(limit, 25);
                assert_eq!(before, Some(80));
                assert!(json);
            }
            _ => panic!("expected agents history"),
        }
        assert!(
            Cli::try_parse_from(["summon", "agents", "history", "soul-x-g1", "--limit", "201",])
                .is_err(),
            "历史页大小必须有上限"
        );

        let cli = Cli::try_parse_from(["summon", "agents", "open", "soul-x-g1a2b3c4"]).unwrap();
        match cli.cmd {
            Cmd::Agents {
                action: Some(AgentsAction::Open { target }),
            } => assert_eq!(target, "soul-x-g1a2b3c4"),
            _ => panic!("expected agents open"),
        }

        let cli = Cli::try_parse_from(["summon", "agents", "activate", "blue-wing"]).unwrap();
        match cli.cmd {
            Cmd::Agents {
                action: Some(AgentsAction::Activate { agent }),
            } => assert_eq!(agent, "blue-wing"),
            _ => panic!("expected agents activate"),
        }
    }

    #[test]
    fn fleet_human_output_marks_busy_and_reports_uptime() {
        use summon_core::fleet::{
            AgentStatus, ChatStatus, DaemonStatus, FleetSnapshot, VisibleState,
        };
        let snapshot = FleetSnapshot {
            comment_tasks: Vec::new(),
            comment_tasks_error: None,
            sentinel: None,
            daemon: DaemonStatus {
                running: true,
                pid: Some(4242),
                version: Some("0.1.0".into()),
                uptime_secs: Some(3 * 86400 + 2 * 3600),
            },
            agents: vec![
                AgentStatus {
                    alias: "starlight".into(),
                    busy: true,
                    sessions: 1,
                    last_active_secs: Some(12),
                    last_active_text: Some("12s前".into()),
                    last_active_text_en: None,
                    state: VisibleState::Working,
                    idle_tmux: false,
                    chats: vec![ChatStatus {
                        tag: "g1a2b3c4".into(),
                        target: "soul-starlight-g1a2b3c4".into(),
                        name: Some("研发群".into()),
                        chat_type: Some("group".into()),
                        chat_id: Some("oc_dev".into()),
                        busy: true,
                        capture: "transcript".into(),
                        liveness: "live".into(),
                        last_active_secs: Some(12),
                        last_active_text: Some("12s前".into()),
                        last_active_text_en: None,
                        waiting: false,
                        state: VisibleState::Working,
                        idle_tmux: false,
                    }],
                },
                AgentStatus {
                    alias: "sakura".into(),
                    busy: false,
                    sessions: 0,
                    last_active_secs: None,
                    last_active_text: None,
                    last_active_text_en: None,
                    // 无会话 → Exited(界面渲「未启动/已退出」)。
                    state: VisibleState::Exited,
                    idle_tmux: false,
                    chats: vec![],
                },
            ],
            groups: vec![],
            waiting: Vec::new(),
            menubar_icon: None,
            menubar_show_counts: None,
        };
        let out = format_fleet(&snapshot);
        assert!(out.contains("daemon 已运行 3天2小时(pid 4242)"));
        assert!(out.contains("1/2 个 agent 在跑"));
        assert!(out.contains("● starlight · 跑一轮中"));
        assert!(out.contains("研发群(soul-starlight-g1a2b3c4) · 跑一轮中"));
        assert!(out.contains("○ sakura · 空闲"));

        // 主轮收尾、子 agent 还在跑:状态是 Working,busy 为 false。
        let mut snapshot = snapshot;
        snapshot.agents[0].busy = false;
        snapshot.agents[0].chats[0].busy = false;
        let out = format_fleet(&snapshot);
        assert!(out.contains("● starlight · 子 agent 在跑"), "{out}");
        assert!(
            out.contains("研发群(soul-starlight-g1a2b3c4) · 子 agent 在跑"),
            "{out}"
        );
    }

    #[test]
    fn fleet_human_output_degrades_without_daemon() {
        // daemon 挂了不该白屏:盘上会话事实照样报,只是诚实说明 busy 不可知。
        let snapshot = summon_core::fleet::build_fleet(
            &[("sakura".to_string(), vec![])],
            None,
            std::time::SystemTime::now(),
        );
        let out = format_fleet(&snapshot);
        assert!(out.contains("daemon 未运行"));
        assert!(out.contains("busy 状态不可知"));
        assert!(out.contains("0/1 个 agent 在跑"));
    }

    #[test]
    fn history_reader_merges_rotated_and_current_logs() {
        fn line(ts: u64, kind: &str, text: Option<&str>) -> String {
            serde_json::to_string(&EventRecord {
                ts,
                seq: 1,
                kind: kind.to_string(),
                text: text.map(String::from),
                tool: None,
                tool_id: None,
                tool_input: None,
                tool_result: None,
                session_id: None,
                usage: None,
                stop_reason: None,
                partial: None,
                stream_source: None,
                source_item_id: None,
                agent: None,
                model: None,
                turn: None,
            })
            .unwrap()
        }

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        std::fs::write(
            format!("{}.1", path.display()),
            format!(
                "{}\n{}\n",
                line(1, "user_input", Some("旧问题")),
                line(2, "result", Some("旧回答"))
            ),
        )
        .unwrap();
        std::fs::write(
            &path,
            format!(
                "{{broken\n{}\n{}\n",
                line(3, "user_input", Some("新问题")),
                line(4, "result", Some("新回答"))
            ),
        )
        .unwrap();

        let turns = read_history_turns(&path).unwrap();
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0].user.as_deref(), Some("旧问题"));
        assert_eq!(turns[1].assistant.as_deref(), Some("新回答"));
    }

    /// 终端命令失败必须**报出来**。上一版 spawn 完就退出,cmux 子进程半路夭折、workspace
    /// 根本没建出来,而退出码是 0 —— 失败完全静默,人只会觉得「点了没反应」。
    #[test]
    fn failing_terminal_command_is_reported_not_swallowed() {
        let argv = vec!["/usr/bin/false".to_string()];
        let err = spawn_terminal(&argv, std::time::Duration::from_secs(5)).unwrap_err();
        assert!(err.to_string().contains("终端命令失败"), "实际错误:{err}");
    }

    /// 终端命令的真实 stderr(如 cmux 门禁的 "Access denied")必须透出到错误里,而不是被吞——
    /// 否则失败只剩无信息量的退出码,人得像查这次 bug 一样逆推半天才知道真因。
    #[test]
    fn failing_terminal_command_surfaces_stderr() {
        let argv = vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            "echo ACCESS_DENIED_XYZ >&2; exit 1".to_string(),
        ];
        let err = spawn_terminal(&argv, std::time::Duration::from_secs(5)).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("终端命令失败"), "实际错误:{msg}");
        assert!(msg.contains("ACCESS_DENIED_XYZ"), "stderr 未透出:{msg}");
    }

    #[test]
    fn terminal_command_that_succeeds_is_waited_for() {
        // cmux / osascript 属于这类:投递完指令就退。必须等到它退出,才知道指令真的送到了。
        let argv = vec!["/usr/bin/true".to_string()];
        spawn_terminal(&argv, std::time::Duration::from_secs(5)).unwrap();
    }

    #[test]
    fn long_lived_terminal_is_released_after_grace() {
        // 有人会把 menubar.terminal 配成终端进程本身(kitty --)。它不会退 —— 等够宽限期就放手,
        // 否则 `agents open` 永远不返回。
        let argv = vec!["/bin/sleep".to_string(), "30".to_string()];
        let started = std::time::Instant::now();
        spawn_terminal(&argv, std::time::Duration::from_millis(200)).unwrap();
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "长命终端必须在宽限期后放手,不能一直等"
        );
    }

    #[test]
    fn missing_terminal_program_is_reported() {
        let argv = vec!["/nonexistent/terminal-binary".to_string()];
        let err = spawn_terminal(&argv, std::time::Duration::from_secs(1)).unwrap_err();
        assert!(err.to_string().contains("起终端失败"), "实际错误:{err}");
    }

    #[test]
    fn send_image_command_parses_path_and_caption() {
        let cli = Cli::try_parse_from(["summon", "send-image", "--turn-id", "t-x", "/tmp/a.png"])
            .unwrap();
        assert!(
            matches!(cli.cmd, Cmd::SendImage { ref path, caption: None, .. } if path == "/tmp/a.png")
        );
        let cli = Cli::try_parse_from([
            "summon",
            "send-image",
            "--turn-id",
            "t-x",
            "/tmp/a.png",
            "--caption",
            "看这个",
        ])
        .unwrap();
        let Cmd::SendImage { path, caption, .. } = cli.cmd else {
            panic!("expected send-image");
        };
        assert_eq!(path, "/tmp/a.png");
        assert_eq!(caption.as_deref(), Some("看这个"));
        // 路径是必填:没有它无从知道发什么。
        assert!(Cli::try_parse_from(["summon", "send-image"]).is_err());
    }

    /// 硬闸:agent 会话内不能借 `--agent` 去看别人的会话。软闸(PRIMER 里那段「只在必要相关
    /// 时才读」)靠模型自觉,这条是唯一挡得住的 —— 跨 agent 读转录是身份越界,与飞书凭据隔离
    /// 同一条不变量。
    #[test]
    fn chats_refuses_cross_agent_access_inside_a_session() {
        let err = resolve_chats_agent(Some("arcadia"), Some("starlight"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("身份越界"), "{err}");
        // 指向自己 / 不指定 → 都是自己。
        assert_eq!(
            resolve_chats_agent(Some("arcadia"), Some("arcadia")).unwrap(),
            "arcadia"
        );
        assert_eq!(
            resolve_chats_agent(Some("arcadia"), None).unwrap(),
            "arcadia"
        );
        // 终端上(无 env):`--agent` 自由;什么都不给则拒绝猜。
        assert_eq!(
            resolve_chats_agent(None, Some("starlight")).unwrap(),
            "starlight"
        );
        assert!(resolve_chats_agent(None, None).is_err());
        assert!(
            resolve_chats_agent(Some(""), None).is_err(),
            "空 env 当作没有"
        );
    }

    #[test]
    fn chats_command_parses_list_read_and_grep() {
        // 裸 `summon chats` → 列索引(action=None)。
        let cli = Cli::try_parse_from(["summon", "chats"]).unwrap();
        assert!(matches!(
            cli.cmd,
            Cmd::Chats {
                turn_id: _,
                agent: None,
                action: None
            }
        ));
        let cli =
            Cli::try_parse_from(["summon", "chats", "read", "开发群", "--last", "5"]).unwrap();
        assert!(matches!(
            cli.cmd,
            Cmd::Chats { action: Some(ChatsAction::Read { ref target, last: 5 }), .. }
                if target == "开发群"
        ));
        // read 的 --last 有默认值,不给也能跑。
        let cli = Cli::try_parse_from(["summon", "chats", "read", "1"]).unwrap();
        assert!(matches!(
            cli.cmd,
            Cmd::Chats {
                action: Some(ChatsAction::Read { last: 20, .. }),
                ..
            }
        ));
        let cli =
            Cli::try_parse_from(["summon", "chats", "--agent", "sk", "grep", "worktree"]).unwrap();
        assert!(matches!(
            cli.cmd,
            Cmd::Chats { agent: Some(ref a), action: Some(ChatsAction::Grep { ref pattern, .. }), .. }
                if a == "sk" && pattern == "worktree"
        ));
    }

    /// 三条主动 CLI **必须**带 `--turn-id`,不带就解析失败。
    ///
    /// 这是这次修复的执行面:身份只认本轮凭证。少了这条,一组真实但陈旧的
    /// `SUMMON_AGENT`+`SUMMON_CHAT_ID`(子进程从上一个上下文继承、或持久 RPC/ACP 进程保留的
    /// 启动期环境)会自洽地通过旧校验,于是**以别的 bot 身份把内容发进别的群**
    /// (2026-08-14 实测)。
    #[test]
    fn active_cli_commands_require_a_turn_credential() {
        use clap::Parser;
        for argv in [
            vec!["summon", "preview", "/tmp/a.md"],
            vec!["summon", "send-image", "/tmp/a.png"],
            vec!["summon", "react", "DONE"],
            vec!["summon", "session", "end"],
        ] {
            let cmd = argv[1];
            assert!(
                Cli::try_parse_from(&argv).is_err(),
                "`summon {cmd}` 不带 --turn-id 必须解析失败(否则又回到只信 env 的老路)"
            );
        }
        // 带上就能解析,且 turn_id 被正确接住。
        let cli = Cli::try_parse_from(["summon", "preview", "--turn-id", "t-1", "/tmp/a.md"])
            .expect("带凭证应解析成功");
        assert!(matches!(cli.cmd, Cmd::Preview { ref turn_id, .. } if turn_id == "t-1"));
        let cli = Cli::try_parse_from(["summon", "react", "--turn-id", "t-2", "DONE"])
            .expect("带凭证应解析成功");
        assert!(matches!(cli.cmd, Cmd::React { ref turn_id, .. } if turn_id == "t-2"));
    }

    #[test]
    fn preview_checks_through_the_daemon_and_prints_the_reply_hint() {
        use clap::Parser;
        let cli =
            Cli::try_parse_from(["summon", "preview", "--turn-id", "t-1", "/tmp/a.md"]).unwrap();
        assert!(matches!(
            cli.cmd,
            Cmd::Preview { ref turn_id, ref path }
                if turn_id == "t-1" && path == "/tmp/a.md"
        ));
        let source = include_str!("cli.rs");
        let start = source.find("fn preview_cmd(").expect("preview_cmd");
        let end = source[start..]
            .find("\nfn workflow_cmd(")
            .expect("preview_cmd 收口");
        let body = &source[start..start + end];
        assert!(body.contains("Intent::CreateFilePreview"), "{body}");
        assert!(body.contains("CommandOutcome::Value(serde_json::Value::String(path))"));
        assert!(body.contains("preview_reply_hint(&path)"), "{body}");
    }

    #[test]
    fn react_command_parses_emoji_and_clear() {
        // 设置/替换:`summon react <emoji>` → Some(emoji), clear=false。
        let cli = Cli::try_parse_from(["summon", "react", "--turn-id", "t-x", "BUG"]).unwrap();
        assert!(matches!(cli.cmd, Cmd::React { emoji: Some(e), clear: false, .. } if e == "BUG"));
        // 撤掉:`summon react --clear` → None, clear=true。
        let cli = Cli::try_parse_from(["summon", "react", "--turn-id", "t-x", "--clear"]).unwrap();
        assert!(matches!(
            cli.cmd,
            Cmd::React {
                emoji: None,
                clear: true,
                ..
            }
        ));
        // 空入参:`summon react` → None, clear=false(IO 层据纯决策也作撤掉)。
        let cli = Cli::try_parse_from(["summon", "react", "--turn-id", "t-x"]).unwrap();
        assert!(matches!(
            cli.cmd,
            Cmd::React {
                emoji: None,
                clear: false,
                ..
            }
        ));
    }

    #[test]
    fn scheduled_turn_capability_cannot_be_used_for_ask() {
        let context = crate::runtime::current_turn_store::CurrentTurnContext {
            turn_id: "turn-schedule".into(),
            agent_alias: "arcadia".into(),
            delivery_scope: "arcadia-app".into(),
            chat_id: "oc_chat".into(),
            chat_type: "group".into(),
            conversation_tag: "g123".into(),
            message_id: "sched-1".into(),
            origin: crate::runtime::current_turn_store::CurrentTurnOrigin::Schedule,
            schedule_output: summon_core::ScheduleOutput::Progress,
            requester_open_id: String::new(),
            requester_union_id: String::new(),
            requester_is_bot: false,
            requester_is_peer: false,
            thread_id: None,
            root_id: None,
            started_at_ms: 1,
            reattached: false,
        };
        let error = ensure_turn_can_ask(&context).unwrap_err().to_string();
        assert!(error.contains("没有真实发起人"), "实际错误:{error}");
        // 定时轮可以自续,连续无人回复到上限就暂停。
        let limit = crate::runtime::self_followup::MAX_UNANSWERED_FOLLOWUPS;
        assert!(ensure_turn_can_remind(&context, limit - 1).is_ok());
        let error = ensure_turn_can_remind(&context, limit)
            .unwrap_err()
            .to_string();
        assert!(error.contains("自续暂停"), "实际错误:{error}");

        let mut result_only = context.clone();
        result_only.schedule_output = summon_core::ScheduleOutput::Result;
        let error = ensure_turn_allows_message_output(&result_only)
            .unwrap_err()
            .to_string();
        assert!(error.contains("仅结果"), "实际错误:{error}");
        assert!(ensure_turn_allows_message_output(&context).is_ok());

        let human = crate::runtime::current_turn_store::CurrentTurnContext {
            origin: crate::runtime::current_turn_store::CurrentTurnOrigin::Feishu,
            requester_open_id: "ou_owner".into(),
            ..context
        };
        assert!(
            ensure_turn_can_remind(&human, limit + 5).is_ok(),
            "真人触发的轮必须能建定时器,计数只约束自续"
        );
    }

    #[test]
    fn multi_select_parses_numbers_all_and_cancel() {
        assert_eq!(parse_multi_select("", 3).unwrap(), Vec::<usize>::new());
        assert_eq!(parse_multi_select("all", 3).unwrap(), vec![0, 1, 2]);
        assert_eq!(parse_multi_select("1, 3 2", 3).unwrap(), vec![0, 1, 2]);
        assert!(parse_multi_select("4", 3).is_err());
    }

    #[test]
    fn kill_entry_only_goes_interactive_on_agent_name_plus_tty() {
        // 回归:`agents kill <agent名>` 在非 TTY(脚本/CI/管道)下曾直接进 read_line 永久阻塞 ——
        // 5 分钟无输出、daemon.log 零日志,只能被外部 timeout 杀掉。四种组合全钉死。
        assert_eq!(kill_entry(true, true), KillEntry::Interactive);
        assert_eq!(kill_entry(true, false), KillEntry::FailFast);
        // 直指 chat_id/tag/session 不读 stdin,TTY 与否都照常执行。
        assert_eq!(kill_entry(false, true), KillEntry::DirectTarget);
        assert_eq!(kill_entry(false, false), KillEntry::DirectTarget);
    }

    #[test]
    fn attach_refuses_before_starting_base_session_only_when_headless_and_absent() {
        // 回归:`agents attach <claude-agent>` 在非终端下会先 `ensure_started` 起 base 会话,
        // 随后 `tmux attach` 必然失败(open terminal failed),留下一个谁都没 attach 上的
        // 孤儿会话(实测:tmux 从 2 涨到 3)。TTY 检查必须前置到起会话之前。
        assert!(
            attach_needs_tty_before_start(false, false),
            "会话未起 + 非终端 → 必须在起 base 之前就拒绝"
        );
        // 终端下正常起 base 并 attach,不能拦。
        assert!(!attach_needs_tty_before_start(false, true));
        // 已有会话:attach 失败就失败,不产生副作用,两种 TTY 状态都不该拦。
        assert!(!attach_needs_tty_before_start(true, false));
        assert!(!attach_needs_tty_before_start(true, true));
    }

    #[test]
    fn non_interactive_kill_message_lists_copyable_targets() {
        // 报错是脚本作者唯一能拿到会话名的地方:必须列候选,且每条给可照抄的绝对名指令。
        let a = agent("arcadia");
        let views = vec![
            view_for(&a, "gc0ffee01", "soul-arcadia-gc0ffee01"),
            view_for(&a, "gdeadbeef", "soul-arcadia-gdeadbeef"),
        ];

        let msg = non_interactive_kill_message(&a, &views, false);
        assert!(msg.contains("stdin 不是终端"), "{msg}");
        assert!(
            msg.contains("summon agents kill soul-arcadia-gc0ffee01"),
            "{msg}"
        );
        assert!(
            msg.contains("summon agents kill soul-arcadia-gdeadbeef"),
            "{msg}"
        );
        // --reset 透传:照抄尾行等价于本次调用,别让用户抄一条丢了 flag 的指令。
        let with_reset = non_interactive_kill_message(&a, &views, true);
        assert!(
            with_reset.contains("summon agents kill soul-arcadia-gc0ffee01 --reset"),
            "{with_reset}"
        );
    }

    #[test]
    fn non_interactive_kill_message_degrades_when_no_candidates() {
        // 无候选也照样报错(行为不随会话数飘忽),但正文换成「无可关会话」而不是列一张空表。
        let a = agent("arcadia");
        let msg = non_interactive_kill_message(&a, &[], false);
        assert!(msg.contains("无可关会话"), "{msg}");
        assert!(
            msg.contains("summon agents kill <chat_id|tag|session名>"),
            "{msg}"
        );
    }

    #[test]
    fn kill_route_branches_by_capture_liveness_and_daemon() {
        use summon_core::registry::CaptureMode;
        use summon_core::session_view::Liveness;
        // claude/Transcript + tmux 在(LiveTmux):本地 tmux 直杀(daemon 在不在都能杀)。
        assert_eq!(
            kill_route(CaptureMode::Transcript, Liveness::LiveTmux, true),
            KillRoute::Tmux
        );
        assert_eq!(
            kill_route(CaptureMode::Transcript, Liveness::LiveTmux, false),
            KillRoute::Tmux
        );
        // C4:claude 残骸(Gone,tmux 已死)→ 按盘归档清 .session,不再落到「Tmux 直杀空会话」。
        assert_eq!(
            kill_route(CaptureMode::Transcript, Liveness::Gone, true),
            KillRoute::OfflineArchive
        );
    }

    /// ⑤ 的核心保护:**kill 没成功就什么都不许落**。
    ///
    /// 写 Closed 的前提是"路由确实终止了"。失败了还写,判活下一轮就会看到
    /// 「lifecycle=Closed + tmux 还在」→ 报成 `ClosedButProcessLive` 泄漏事故 —— 而实际上
    /// 那只是一次 kill 失败,会话好端端跑着。**凭空造出的假事故比漏报更糟**:它会让人去
    /// 处理一个不存在的问题,并且冲淡真事故的可信度。
    #[test]
    fn a_failed_kill_must_not_commit_anything() {
        for route in [
            KillRoute::Tmux,
            KillRoute::Control,
            KillRoute::OfflineArchive,
        ] {
            let plan = close_commit_plan(route, false);
            assert!(!plan.write_closed, "{route:?}:kill 失败不得写 Closed");
            assert!(!plan.send_receipt, "{route:?}:kill 失败不得发退出回执");
        }
    }

    /// 成功后:Control 路不重复写(daemon 内部 `do_teardown` 已写过),其余两路要写;回执都发。
    #[test]
    fn a_successful_kill_commits_once_without_double_writing_control() {
        let via_control = close_commit_plan(KillRoute::Control, true);
        assert!(
            !via_control.write_closed,
            "Control 路由 daemon 侧已写 Closed,CLI 再写一遍是重复"
        );
        assert!(via_control.send_receipt);
        for route in [KillRoute::Tmux, KillRoute::OfflineArchive] {
            let plan = close_commit_plan(route, true);
            assert!(
                plan.write_closed,
                "{route:?}:CLI 直接关的路必须自己写 Closed"
            );
            assert!(plan.send_receipt);
        }
    }

    /// C5 单独成例:泄漏态(路由已退休、tmux 还活着)必须与 live 同路真杀。
    #[test]
    fn kill_route_sends_the_leaked_state_down_the_real_kill_path() {
        use summon_core::registry::CaptureMode;
        use summon_core::session_view::Liveness;
        // 它在候选列表里排第一位,它在候选列表里排第一位,
        // 若落进归档路,用户选了最该处理的那条却只改了磁盘状态、进程原地不动 —— 自相矛盾。
        for daemon_up in [true, false] {
            assert_eq!(
                kill_route(
                    CaptureMode::Transcript,
                    Liveness::ClosedButProcessLive,
                    daemon_up
                ),
                KillRoute::Tmux,
                "泄漏态要真杀 tmux(daemon_up={daemon_up})"
            );
        }
        // 无头 + daemon 在:经 control.sock 让 daemon 拆(无论 recent/idle/gone)。
        assert_eq!(
            kill_route(CaptureMode::Rpc, Liveness::RecentHeadless, true),
            KillRoute::Control
        );
        // 无头 + daemon 不在:只能盘上归档兜底。
        assert_eq!(
            kill_route(CaptureMode::Rpc, Liveness::IdleHeadless, false),
            KillRoute::OfflineArchive
        );
    }

    #[test]
    fn cli_kill_enqueues_receipt_for_tmux_and_offline_routes_only() {
        // Control 路由由 daemon 内部发回执,CLI 再入队就是双发;另两条必须入队,
        // 否则 daemon 重启后 kill 静默(本次修的 bug)。
        assert!(should_enqueue_exit_receipt(KillRoute::Tmux));
        assert!(should_enqueue_exit_receipt(KillRoute::OfflineArchive));
        assert!(!should_enqueue_exit_receipt(KillRoute::Control));
    }

    #[test]
    fn exit_chat_type_infers_from_tag_when_meta_lacks_it() {
        // 去重标记键 = chat_tag(chat_type, chat_id)。`.meta` 缺 chat_type 时若硬编码 "group",
        // 而 idle-scan 那侧用入站事件带来的真实 p2p → 键不匹配 → 去重失效 → 同一次退出发两条。
        // 故必须按 tag 首字符推断(与 discovery 构造 SessionView 的回退同源)。
        let mut view = summon_core::session_view::SessionView {
            agent: "arcadia".into(),
            tag: "p0011aabb".into(),
            session: "soul-arcadia".into(),
            chat_id: Some("oc_1".into()),
            chat_type: None,
            chat_name: None,
            capture: summon_core::registry::CaptureMode::Transcript,
            liveness: summon_core::session_view::Liveness::LiveTmux,
            last_active: None,
        };
        assert_eq!(
            exit_chat_type(&view),
            "p2p",
            "缺 chat_type 应按 tag 推断 p2p"
        );
        view.tag = "gdeadbeef".into();
        assert_eq!(exit_chat_type(&view), "group", "g 前缀应推断 group");
        // .meta 有 chat_type 时以它为准(topic_group 无法从 tag 前缀还原)。
        view.chat_type = Some("topic_group".into());
        assert_eq!(exit_chat_type(&view), "topic_group");
        // 连 tag 都无前缀信息(裸名 owner 会话)时才退到默认值。
        view.chat_type = None;
        view.tag = "(base/owner)".into();
        assert_eq!(exit_chat_type(&view), "group");
    }

    #[test]
    fn cli_kill_skips_enqueue_when_mark_present_or_no_chat() {
        // ① 标记已在(tmux 先被外部杀、idle-scan 已发过回执)→ 用户之后再 kill 清残骸不重复入队。
        //    这是标记双向的核心用例。
        assert!(!should_enqueue_for(
            KillRoute::OfflineArchive,
            true,
            Some("oc_1")
        ));
        // ② 无标记 → 正常入队。
        assert!(should_enqueue_for(
            KillRoute::OfflineArchive,
            false,
            Some("oc_1")
        ));
        // ③ threads-only 孤儿无 .meta、拿不到 chat_id → 没有可投递的目标,不入队。
        assert!(!should_enqueue_for(KillRoute::Tmux, false, None));
        // ④ Control 路由恒不入队,与标记/chat_id 无关(daemon 内部已发)。
        assert!(!should_enqueue_for(KillRoute::Control, false, Some("oc_1")));
    }

    #[test]
    fn kill_candidates_render_and_select_share_one_view_set() {
        // C3 回归:owner 裸名私聊(tag p<hex>,tmux 名 soul-<alias>)+ 群会话(tag g<hex>)并存时,
        // 旧实现展示按 tag 序、可选 entries 按 session 名序 → 序号错位杀错会话。修复后 kill 候选与
        // 展示共用 discover_sessions 单一 Vec,序号 i 恒指向展示第 i 行(prompt_and_kill 直接 views[i])。
        let tmp = std::env::temp_dir().join(format!("summon-cli-c3-order-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let mut a = agent("starlight");
        a.dir = tmp.join("agents").join(&a.name);
        std::fs::create_dir_all(a.state_dir().join("runtime/chats")).unwrap();
        // 裸名私聊会话:文件名裸 soul-starlight.meta,tag=p<hex>。
        std::fs::write(
            a.state_dir().join("runtime/chats/soul-starlight.meta"),
            "chat_id=oc_priv\nchat_type=p2p\ntag=p00000000a11ce04e\nupdated_at=2026-06-03 12:00:00\n",
        )
        .unwrap();
        // 群会话:tag=gdeadbeef。
        std::fs::write(
            a.state_dir().join("runtime/chats/soul-starlight-gdeadbeef.meta"),
            "chat_id=oc_group\nchat_type=group\nchat_name=研发群\ntag=gdeadbeef\nupdated_at=2026-06-03 12:00:00\n",
        )
        .unwrap();
        // 两个会话都有活 tmux。
        let live = vec![
            "soul-starlight".to_string(),
            "soul-starlight-gdeadbeef".to_string(),
        ];
        let cands = kill_candidates_for_agent(&a, &live);
        // 与 agents ls 同一 discover_sessions:BTreeMap 按 tag 序,'g'(0x67) < 'p'(0x70) → 群在私聊前。
        let tags: Vec<&str> = cands.iter().map(|v| v.tag.as_str()).collect();
        assert_eq!(
            tags,
            vec!["gdeadbeef", "p00000000a11ce04e"],
            "候选顺序 = 展示顺序(tag 序),序号即索引"
        );
        // 序号 1(cands[0])= 群会话,序号 2(cands[1])= 私聊裸名会话 —— 所见即所选,无错位。
        assert_eq!(cands[0].session, "soul-starlight-gdeadbeef");
        assert_eq!(cands[1].session, "soul-starlight");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn kill_candidates_hide_gone_and_order_by_current_state() {
        use std::time::{Duration, SystemTime};
        use summon_core::registry::CaptureMode;
        use summon_core::session_view::{Liveness, SessionView};

        let view = |tag: &str, liveness: Liveness, active_at: u64| SessionView {
            agent: "starlight".to_string(),
            tag: tag.to_string(),
            session: format!("soul-starlight-{tag}"),
            chat_id: Some(format!("oc_{tag}")),
            chat_type: Some("group".to_string()),
            chat_name: None,
            capture: match liveness {
                // 泄漏态只可能出自 Transcript:无头没有可查的进程事实,判不出这一态。
                Liveness::LiveTmux | Liveness::Gone | Liveness::ClosedButProcessLive => {
                    CaptureMode::Transcript
                }
                Liveness::RecentHeadless | Liveness::IdleHeadless => CaptureMode::Rpc,
            },
            liveness,
            last_active: Some(SystemTime::UNIX_EPOCH + Duration::from_secs(active_at)),
        };

        let candidates = visible_kill_candidates(vec![
            view("stale", Liveness::IdleHeadless, 500),
            view("live-old", Liveness::LiveTmux, 100),
            view("gone", Liveness::Gone, 600),
            view("recent", Liveness::RecentHeadless, 400),
            view("live-new", Liveness::LiveTmux, 300),
        ]);
        let tags: Vec<&str> = candidates.iter().map(|view| view.tag.as_str()).collect();

        assert_eq!(
            tags,
            vec!["live-new", "live-old", "recent", "stale"],
            "状态优先级应为 live > recent > stale，同状态内最近活跃优先，gone 不出现"
        );
    }

    #[test]
    fn gone_is_hidden_from_interactive_kill_but_kept_for_direct_target() {
        use summon_core::session_view::Liveness;

        let tmp = tempfile::tempdir().unwrap();
        let mut a = agent("starlight");
        a.dir = tmp.path().join("agents").join(&a.name);
        std::fs::create_dir_all(a.state_dir().join("runtime/chats")).unwrap();
        std::fs::write(
            a.state_dir().join("runtime/chats/soul-starlight-gone.meta"),
            "chat_id=oc_gone\nchat_type=group\ntag=gone\n",
        )
        .unwrap();

        assert!(
            kill_candidates_for_agent(&a, &[]).is_empty(),
            "按 agent 进入的交互列表不应展示 gone"
        );

        let reg = Registry { agents: vec![a] };
        let direct = discover_all_sessions(&reg, &[]);
        assert_eq!(direct.len(), 1);
        assert_eq!(direct[0].liveness, Liveness::Gone);
        assert!(session_view_matches(&direct[0], "soul-starlight-gone"));
    }

    #[test]
    fn kill_candidates_include_headless_without_tmux() {
        // C4 回归:无头会话无 tmux,旧 kill 候选来自 tmux-only chat_entries → 永远找不到。
        // 修复后候选来自 discover_sessions 全集,无头会话在内,且按 (capture, liveness, daemon_up) 路由。
        use summon_core::registry::CaptureMode;
        let tmp =
            std::env::temp_dir().join(format!("summon-cli-c4-headless-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let mut a = cursor_agent("nezha");
        a.dir = tmp.join("agents").join(&a.name);
        std::fs::create_dir_all(a.state_dir().join("runtime/chats")).unwrap();
        std::fs::create_dir_all(a.state_dir().join("threads/gbeef")).unwrap();
        std::fs::write(a.state_dir().join("threads/gbeef/events.jsonl"), "{}\n").unwrap();
        std::fs::write(
            a.state_dir().join("runtime/chats/soul-nezha-gbeef.meta"),
            "chat_id=oc_beef\nchat_type=group\nchat_name=封神群\ntag=gbeef\nupdated_at=2026-06-03 12:00:00\n",
        )
        .unwrap();
        // 无任何 tmux。
        let cands = kill_candidates_for_agent(&a, &[]);
        assert_eq!(cands.len(), 1, "无头会话应作为 kill 候选出现");
        assert_eq!(cands[0].capture, CaptureMode::Rpc);
        // daemon 在 → Control(control.sock);不在 → OfflineArchive。三路由被激活。
        assert_eq!(
            kill_route(cands[0].capture, cands[0].liveness, true),
            KillRoute::Control
        );
        assert_eq!(
            kill_route(cands[0].capture, cands[0].liveness, false),
            KillRoute::OfflineArchive
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn kill_by_chat_id_or_tag_finds_headless_session() {
        // C4 回归:kill <chat_id>/<tag>/<session> 直指,候选换成 discover_sessions 全集,
        // 无头会话可被 chat_id / tag / session 名任一命中(旧 tmux-only 匹配恒 bail「未找到」)。
        let tmp = std::env::temp_dir().join(format!("summon-cli-c4-match-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let mut a = cursor_agent("nezha");
        a.dir = tmp.join("agents").join(&a.name);
        std::fs::create_dir_all(a.state_dir().join("runtime/chats")).unwrap();
        std::fs::write(
            a.state_dir().join("runtime/chats/soul-nezha-gbeef.meta"),
            "chat_id=oc_beef\nchat_type=group\nchat_name=封神群\ntag=gbeef\nupdated_at=2026-06-03 12:00:00\n",
        )
        .unwrap();
        let reg = Registry { agents: vec![a] };
        let all = discover_all_sessions(&reg, &[]);
        // chat_id / tag / session 名三种直指都命中同一无头会话。
        assert!(all.iter().any(|v| session_view_matches(v, "oc_beef")));
        assert!(all.iter().any(|v| session_view_matches(v, "gbeef")));
        assert!(all
            .iter()
            .any(|v| session_view_matches(v, "soul-nezha-gbeef")));
        let matched: Vec<_> = all
            .iter()
            .filter(|v| session_view_matches(v, "oc_beef"))
            .collect();
        assert_eq!(matched.len(), 1);
        assert_eq!(matched[0].tag, "gbeef");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn schedules_cli_parses() {
        let cli = Cli::try_parse_from(["summon", "schedules", "ls"]).unwrap();
        assert!(matches!(
            cli.cmd,
            Cmd::Schedules {
                action: Some(SchedulesAction::Ls)
            }
        ));
        let cli = Cli::try_parse_from(["summon", "schedules", "rm", "SCH-1"]).unwrap();
        assert!(
            matches!(cli.cmd, Cmd::Schedules { action: Some(SchedulesAction::Rm { ref id }) } if id == "SCH-1")
        );
        // 裸 schedules = ls
        let cli = Cli::try_parse_from(["summon", "schedules"]).unwrap();
        assert!(matches!(cli.cmd, Cmd::Schedules { action: None }));

        let cli = Cli::try_parse_from([
            "summon",
            "schedules",
            "edit",
            "SCH-1",
            "--when",
            "daily 09:30",
            "--prompt",
            "生成日报",
            "--output",
            "result",
            "--context",
            "fresh",
            "--enabled",
            "true",
        ])
        .unwrap();
        assert!(matches!(
            cli.cmd,
            Cmd::Schedules {
                action: Some(SchedulesAction::Edit {
                    ref id,
                    when: Some(ref when),
                    prompt: Some(ref prompt),
                    output: Some(ref output),
                    context: Some(ref context),
                    enabled: Some(true),
                })
            } if id == "SCH-1" && when == "daily 09:30" && prompt == "生成日报" && output == "result" && context == "fresh"
        ));
    }

    #[test]
    fn agent_config_field_maps_to_matching_intent() {
        use summon_core::core_port::Intent;
        // 两个字段各自发自己的 Intent —— 接上 core_port 里已定义但此前无调用方的那条线。
        assert!(matches!(
            AgentConfigField::Model.intent("mago", "gpt-5.6-terra"),
            Intent::SetModel { alias, model } if alias == "mago" && model == "gpt-5.6-terra"
        ));
        assert!(matches!(
            AgentConfigField::Effort.intent("mago", "ultra"),
            Intent::SetEffort { alias, effort } if alias == "mago" && effort == "ultra"
        ));
    }

    #[test]
    fn agent_config_field_validates_changed_axis_against_current_other_axis() {
        let codex: summon_core::registry::Agent = serde_yaml::from_str(
            "name: mago\nmodel: gpt-5.6-sol\neffort: high\nruntime:\n  cli: codex\n",
        )
        .unwrap();
        assert_eq!(codex.cli_kind(), "codex");
        // 改 model:另一轴取现有 effort=high(codex 合法)→ 放行。
        assert!(AgentConfigField::Model
            .validate(&codex, "gpt-5.6-terra")
            .is_ok());
        // 改 effort:ultra 是 codex 新收的档位 → 放行;瞎写的档位 fail-closed。
        assert!(AgentConfigField::Effort.validate(&codex, "ultra").is_ok());
        assert!(AgentConfigField::Effort.validate(&codex, "warp").is_err());

        // cursor 无独立 effort 轴(档位烤在 model 名里):改 effort 必须被拒,别写出个起不来的组合。
        let cursor: summon_core::registry::Agent = serde_yaml::from_str(
            "name: ama\nmodel: cursor-grok-4.5-high\nruntime:\n  cli: cursor\n",
        )
        .unwrap();
        assert!(AgentConfigField::Effort.validate(&cursor, "high").is_err());
        assert!(AgentConfigField::Model
            .validate(&cursor, "composer-2.5")
            .is_ok());
    }

    #[test]
    fn agent_config_field_reads_current_value_per_axis() {
        let a: summon_core::registry::Agent =
            serde_yaml::from_str("name: mago\nmodel: gpt-5.6-sol\neffort: high\n").unwrap();
        assert_eq!(
            AgentConfigField::Model.current(&a),
            Some("gpt-5.6-sol".into())
        );
        assert_eq!(AgentConfigField::Effort.current(&a), Some("high".into()));
        // 没写 model 行 → None,只读形态据此提示「用 CLI 自己的默认」而不是显示空串。
        let bare: summon_core::registry::Agent = serde_yaml::from_str("name: mago\n").unwrap();
        assert_eq!(AgentConfigField::Model.current(&bare), None);
        assert_eq!(AgentConfigField::Effort.current(&bare), None);
    }

    #[test]
    fn first_run_detects_missing_or_empty_agents_dir() {
        let tmp = tempfile::tempdir().unwrap();
        // 目录不存在 → 首次运行
        assert!(is_first_run_at(&tmp.path().join("agents")));
        // 空 agents 目录 → 首次运行
        let agents = tmp.path().join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        assert!(is_first_run_at(&agents));
        // 有 agent.yaml → 不是首次运行(哪怕内容是空的)
        std::fs::create_dir_all(agents.join("someone")).unwrap();
        std::fs::write(agents.join("someone").join("agent.yaml"), "name: someone\n").unwrap();
        assert!(!is_first_run_at(&agents));
    }

    #[test]
    fn doctor_codex_section_stays_quiet_when_the_policy_is_sane() {
        let agent: summon_core::registry::Agent =
            serde_yaml::from_str("name: bot\nruntime:\n  cli: codex\n").unwrap();
        let out = format_codex_policy_section(&[&agent], &Default::default());
        assert_eq!(out, "codex 权限策略  : OK\n");
    }

    /// 非法值在运行时只落一行日志,doctor 是唯一能主动看见「我配的值其实没生效」的地方。
    #[test]
    fn doctor_codex_section_reports_illegal_values_with_the_legal_set() {
        let agent: summon_core::registry::Agent =
            serde_yaml::from_str("name: bot\nruntime:\n  cli: codex\n  codex_sandbox: read_only\n")
                .unwrap();
        let out = format_codex_policy_section(&[&agent], &Default::default());
        assert!(out.contains("非法值 `read_only`"), "实得:{out}");
        assert!(out.contains("danger-full-access"), "要把合法值列出来:{out}");
        assert!(out.contains("config.yaml"), "要告诉人去哪儿改:{out}");
    }

    /// 两个「配得上但会让 agent 干什么都失败」的组合。
    #[test]
    fn doctor_codex_section_flags_the_two_self_defeating_combinations() {
        let asks: summon_core::registry::Agent = serde_yaml::from_str(
            "name: bot\nruntime:\n  cli: codex\n  codex_approval_policy: on-request\n",
        )
        .unwrap();
        assert!(
            format_codex_policy_section(&[&asks], &Default::default()).contains("operator"),
            "非 never 的 approval 必须提示会等待人工裁决"
        );

        let blocked: summon_core::registry::Agent =
            serde_yaml::from_str("name: bot\nruntime:\n  cli: codex\n  codex_sandbox: read-only\n")
                .unwrap();
        assert!(
            format_codex_policy_section(&[&blocked], &Default::default())
                .contains("写不了任何文件"),
            "read-only + never 必须提示"
        );
    }

    /// 非 codex 的 agent 不该出现在这张面板上 —— 这两个键对它们没有意义。
    #[test]
    fn doctor_codex_section_ignores_non_codex_agents() {
        let claude: summon_core::registry::Agent = serde_yaml::from_str(
            "name: bot\nruntime:\n  cli: claude\n  codex_sandbox: read_only\n",
        )
        .unwrap();
        let out = format_codex_policy_section(&[&claude], &Default::default());
        assert_eq!(out, "codex 权限策略  : OK\n");
    }
    #[test]
    fn delegate_cli_delivers_unless_dry_run_and_has_no_agent_override() {
        let args = [
            "summon",
            "delegate",
            "--turn-id",
            "turn",
            "--to",
            "开发群",
            "--content",
            "-",
        ];
        let cli = Cli::try_parse_from(args).unwrap();
        assert!(matches!(cli.cmd, Cmd::Delegate { dry_run: false, .. }));
        let cli = Cli::try_parse_from(args.into_iter().chain(["--dry-run"])).unwrap();
        assert!(matches!(cli.cmd, Cmd::Delegate { dry_run: true, .. }));
        assert!(Cli::try_parse_from([
            "summon",
            "delegate",
            "--turn-id",
            "turn",
            "--to",
            "群",
            "--content",
            "任务",
            "--agent",
            "other"
        ])
        .is_err());
    }

    #[test]
    fn delegate_cli_keeps_public_summary_separate_from_private_body() {
        let cli = Cli::try_parse_from([
            "summon",
            "delegate",
            "--turn-id",
            "turn",
            "--to",
            "Target",
            "--content",
            "private body",
            "--summary",
            "public summary",
        ])
        .unwrap();
        assert!(
            matches!(cli.cmd, Cmd::Delegate { summary: Some(summary), content, .. } if summary == "public summary" && content == "private body")
        );
    }

    #[test]
    fn delegate_cli_requires_exactly_one_existing_or_new_target() {
        let base = [
            "summon",
            "delegate",
            "--turn-id",
            "turn",
            "--content",
            "task",
        ];
        assert!(Cli::try_parse_from(base).is_err());
        assert!(Cli::try_parse_from(base.into_iter().chain([
            "--to",
            "Target",
            "--new-chat",
            "New Target"
        ]))
        .is_err());
        let cli =
            Cli::try_parse_from(
                base.into_iter()
                    .chain(["--new-chat", "New Target", "--dry-run"]),
            )
            .unwrap();
        assert!(
            matches!(cli.cmd,Cmd::Delegate{to:None,new_chat:Some(name),dry_run:true,..} if name=="New Target")
        );
    }

    #[test]
    fn remind_parses_the_three_required_flags() {
        let cli = Cli::try_parse_from([
            "summon",
            "remind",
            "--turn-id",
            "t-1",
            "--when",
            "daily 09:00",
            "--content",
            "看一眼昨天的构建",
        ])
        .expect("三个 flag 齐了就该解析成功");
        match cli.cmd {
            Cmd::Remind {
                turn_id,
                when,
                content,
                output,
                context,
            } => {
                assert_eq!(turn_id, "t-1");
                assert_eq!(when, "daily 09:00");
                assert_eq!(content, "看一眼昨天的构建");
                assert_eq!(output, "progress");
                assert_eq!(context, "resume");
            }
            _ => panic!("解析成了别的子命令"),
        }
    }

    /// 三个 flag 都是必填。缺 turn-id 尤其重要:归属只认凭证,没有它就无从判断建给谁。
    #[test]
    fn remind_requires_every_flag() {
        for args in [
            vec![
                "summon",
                "remind",
                "--when",
                "daily 09:00",
                "--content",
                "x",
            ],
            vec!["summon", "remind", "--turn-id", "t", "--content", "x"],
            vec![
                "summon",
                "remind",
                "--turn-id",
                "t",
                "--when",
                "daily 09:00",
            ],
        ] {
            assert!(Cli::try_parse_from(&args).is_err(), "本该拒绝:{args:?}");
        }
    }

    #[test]
    fn schedule_mode_label_mentions_fresh_context_only_when_set() {
        let mut s: summon_core::Schedule = serde_yaml::from_str(
            "id: SCH-1\nkind: cron\nspec: \"30 14 * * 1-5\"\nagent: a\nchat_id: c\n\
             chat_type: group\nprompt: p\ncreated_by: ou\ncreated_at: \"2026-09-02T14:30:00+08:00\"\n\
             output: result\n",
        )
        .unwrap();
        assert_eq!(schedule_mode_label(&s), "仅结果");
        s.context = summon_core::ScheduleContext::Fresh;
        assert_eq!(schedule_mode_label(&s), "仅结果·新上下文");
    }

    #[test]
    fn remind_context_mode_is_typed_and_defaults_to_resume() {
        let parse = |extra: &[&str]| {
            let mut args = vec![
                "summon",
                "remind",
                "--turn-id",
                "t",
                "--when",
                "weekday 14:30",
                "--content",
                "巡检",
            ];
            args.extend_from_slice(extra);
            Cli::try_parse_from(args)
        };
        assert!(matches!(
            parse(&[]).unwrap().cmd,
            Cmd::Remind { ref context, .. } if context == "resume"
        ));
        assert!(matches!(
            parse(&["--context", "fresh"]).unwrap().cmd,
            Cmd::Remind { ref context, .. } if context == "fresh"
        ));
        assert!(
            parse(&["--context", "new"]).is_err(),
            "上下文模式只认 resume / fresh"
        );
    }

    #[test]
    fn remind_output_mode_is_typed_and_defaults_to_progress() {
        let parse = |extra: &[&str]| {
            let mut args = vec![
                "summon",
                "remind",
                "--turn-id",
                "t",
                "--when",
                "daily 09:00",
                "--content",
                "日报",
            ];
            args.extend_from_slice(extra);
            Cli::try_parse_from(args).unwrap()
        };
        assert!(matches!(
            parse(&[]).cmd,
            Cmd::Remind { ref output, .. } if output == "progress"
        ));
        assert!(matches!(
            parse(&["--output", "result"]).cmd,
            Cmd::Remind { ref output, .. } if output == "result"
        ));
        assert!(Cli::try_parse_from([
            "summon",
            "remind",
            "--turn-id",
            "t",
            "--when",
            "daily 09:00",
            "--content",
            "日报",
            "--output",
            "silent"
        ])
        .is_err());
    }

    /// 回执里那个「下一次几点」是用户唯一能当场核对的东西。**不能用 `due_plan().next_fire`**:
    /// 它对还没到期的一次性定时器返回 None(那是「本 tick 无事」,不是「没有下一次」),
    /// 于是回执会退化成只有节律、没有时刻。
    #[test]
    fn the_receipt_reports_a_next_fire_time_for_every_kind() {
        use chrono::TimeZone;
        use summon_core::schedule::Kind;
        let now = chrono::Local
            .with_ymd_and_hms(2026, 8, 21, 10, 0, 0)
            .unwrap();
        let make = |kind: Kind, spec: &str| summon_core::Schedule {
            id: "SCH-1".into(),
            kind,
            spec: spec.into(),
            agent: "a".into(),
            chat_id: "oc_x".into(),
            chat_type: "group".into(),
            prompt: "p".into(),
            created_by: "ou_x".into(),
            created_at: now.to_rfc3339(),
            output: summon_core::ScheduleOutput::Progress,
            context: summon_core::ScheduleContext::Resume,
            enabled: true,
            last_fired: None,
            revision: 0,
        };

        // 一次性定时器的时刻带时区;按本机时区写,回执按本机时区读,不随跑测试的机器变。
        let once_at = chrono::Local
            .with_ymd_and_hms(2026, 8, 22, 9, 0, 0)
            .unwrap()
            .to_rfc3339();
        let once = make(Kind::Once, &once_at);
        assert_eq!(
            next_fire_hint(&once, now)
                .expect("一次性也得报出时刻")
                .format("%Y-%m-%d %H:%M")
                .to_string(),
            "2026-08-22 09:00"
        );
        assert!(
            summon_core::due_plan(&once, now).next_fire.is_none(),
            "前提:due_plan 对未到期的一次性给 None,所以回执不能用它"
        );

        assert_eq!(
            next_fire_hint(&make(Kind::Interval, "7200"), now)
                .unwrap()
                .format("%H:%M")
                .to_string(),
            "12:00"
        );
        assert_eq!(
            next_fire_hint(&make(Kind::Cron, "0 9 * * *"), now)
                .unwrap()
                .format("%Y-%m-%d %H:%M")
                .to_string(),
            "2026-08-22 09:00"
        );
        // 坏 spec 不报时刻,但也不能崩 —— 回执退化成只有节律。
        assert!(next_fire_hint(&make(Kind::Cron, "not a cron"), now).is_none());
        assert!(next_fire_hint(&make(Kind::Interval, "0"), now).is_none());
    }

    /// 回执的节律描述必须和列表卡 / `schedules ls` 用同一个渲染函数,
    /// 否则同一条定时器在两个地方长得不一样。
    #[test]
    fn the_receipt_reuses_the_shared_rhythm_renderer() {
        let schedule = summon_core::Schedule {
            id: "SCH-1".into(),
            kind: summon_core::schedule::Kind::Cron,
            spec: "0 9 * * *".into(),
            agent: "a".into(),
            chat_id: "oc_x".into(),
            chat_type: "group".into(),
            prompt: "p".into(),
            created_by: "ou_x".into(),
            created_at: "2026-08-21T10:00:00+08:00".into(),
            output: summon_core::ScheduleOutput::Progress,
            context: summon_core::ScheduleContext::Resume,
            enabled: true,
            last_fired: None,
            revision: 0,
        };
        assert_eq!(
            summon_core::schedule::human_readable(&schedule),
            "每天 09:00"
        );
    }

    /// 自续建的定时器归到触发这一轮的定时器的创建者名下。
    #[test]
    fn self_followup_reminder_inherits_the_firing_schedule_creator() {
        let tmp = tempfile::tempdir().unwrap();
        let store = crate::schedule::schedule_store::ScheduleStore::new(tmp.path());
        store
            .add(&summon_core::Schedule {
                id: "SCH-109".into(),
                kind: summon_core::schedule::Kind::Cron,
                spec: "0 9 * * *".into(),
                agent: "a".into(),
                chat_id: "oc_x".into(),
                chat_type: "group".into(),
                prompt: "p".into(),
                created_by: "ou_owner".into(),
                created_at: "2026-10-12T14:21:00+08:00".into(),
                output: summon_core::ScheduleOutput::Progress,
                context: summon_core::ScheduleContext::Resume,
                enabled: true,
                last_fired: None,
                revision: 0,
            })
            .unwrap();
        assert_eq!(
            schedule_creator_of_turn(&store, "sched-SCH-109-1791779000000").as_deref(),
            Some("ou_owner")
        );
        assert_eq!(schedule_creator_of_turn(&store, "sched-SCH-1-0"), None);
        assert_eq!(schedule_creator_of_turn(&store, "om_message"), None);
    }
    /// **CLI 对 agent 友好度的常驻闸**:每个生产命令、子命令、参数都必须有一句说明。
    ///
    /// 为什么值得一条测试:调用方主要是 agent,它认识这套命令的**唯一**途径就是 `--help`
    /// 和报错文案。少一句 `<TARGET>` 的说明,对人只是"猜一下",对 agent 就是猜错、
    /// 然后把动作打到别的会话上。2026-08-21 首次审计时有 14 个生产参数是裸的
    /// (`<TARGET>` / `<AGENT>` / `--chat` 这类),全靠人去读源码才知道填什么。
    ///
    /// `dev` 整组豁免:它是 `#[cfg(debug_assertions)]` 的调试面,release 构建里不存在,
    /// agent 在生产上根本看不到它。
    #[test]
    fn every_production_command_and_argument_documents_itself() {
        use clap::CommandFactory;

        fn walk(cmd: &clap::Command, path: &str, problems: &mut Vec<String>) {
            if cmd.get_name() == "dev" {
                return;
            }
            let here = if path.is_empty() {
                cmd.get_name().to_string()
            } else {
                format!("{path} {}", cmd.get_name())
            };
            if cmd.get_about().is_none() && cmd.get_long_about().is_none() {
                problems.push(format!("命令 `{here}` 没有说明"));
            }
            for arg in cmd.get_arguments() {
                if arg.get_id() == "help" || arg.get_id() == "version" {
                    continue;
                }
                if arg.get_help().is_none() && arg.get_long_help().is_none() {
                    problems.push(format!("`{here}` 的参数 `{}` 没有说明", arg.get_id()));
                }
            }
            for sub in cmd.get_subcommands() {
                if sub.get_name() == "help" {
                    continue;
                }
                walk(sub, &here, problems);
            }
        }

        let cmd = Cli::command();
        let mut problems = Vec::new();
        walk(&cmd, "", &mut problems);
        assert!(
            problems.is_empty(),
            "agent 只能靠 --help 认识这些命令,下面这些它读不到:\n  {}",
            problems.join("\n  ")
        );
    }

    /// 有必填参数的命令,**裸调用要打全量 help**,而不是只报「缺哪个参数」。
    ///
    /// 差别对 agent 很实在:前者一次就拿到所有参数与取值形态,后者要再跑一次 `--help`,
    /// 而中间那次失败它可能已经拿去猜了。
    #[test]
    fn bare_invocation_of_a_command_with_required_args_prints_full_help() {
        use clap::CommandFactory;
        let cmd = Cli::command();
        for name in [
            "remind",
            "remember",
            "bind",
            "react",
            "preview",
            "send-image",
        ] {
            let sub = cmd
                .get_subcommands()
                .find(|c| c.get_name() == name)
                .unwrap_or_else(|| panic!("找不到子命令 {name}"));
            assert!(
                sub.is_arg_required_else_help_set(),
                "`summon {name}` 裸调用只会报「缺参数」,agent 拿不到取值形态"
            );
        }
    }

    /// 节律报错必须给**形态**而不只是名字 —— 只说「用 daily」,agent 仍然不知道后面跟什么。
    #[test]
    fn a_bad_rhythm_error_lists_concrete_forms_not_just_names() {
        let error = validate_when("tomorrow 9am").unwrap_err();
        for shape in [
            "in 30m",
            "daily 09:00",
            "weekly mon,fri 10:00",
            "cron 0 9 * * 1-5",
        ] {
            assert!(error.contains(shape), "报错里缺 `{shape}`:{error}");
        }
        // 空值也走同一份形态清单。
        assert!(validate_when("").unwrap_err().contains("daily 09:00"));
    }

    /// `--when` 在**解析期**就校验:错了当场报,不依赖 daemon,也不会被前面的鉴权错误盖住。
    #[test]
    fn the_rhythm_is_validated_before_any_credential_round_trip() {
        assert!(validate_when("daily 09:00").is_ok());
        assert!(validate_when("in 30m 开会")
            .unwrap_err()
            .contains("--content"));
    }
}

#[cfg(test)]
mod config_field_effect_tests {
    use super::{config_field_effect, AgentConfigField};

    /// `agents model / effort` 的回执按采集通道说真话:claude 的 model 就地切、effort 要冷启,
    /// Rpc 的 cursor 用面板那份。
    #[test]
    fn the_receipt_follows_the_capture_channel() {
        let agent = |cli: &str, extra: &str| -> summon_core::registry::Agent {
            serde_yaml::from_str(&format!("name: a\nruntime:\n  cli: {cli}\n{extra}")).unwrap()
        };
        let model = AgentConfigField::Model;
        assert!(config_field_effect(model, &agent("claude", "")).contains("/model 就地切换"));
        assert!(
            config_field_effect(AgentConfigField::Effort, &agent("claude", "")).contains("冷启")
        );
        assert!(config_field_effect(model, &agent("cursor", "")).contains("会话面板"));
        assert_eq!(
            config_field_effect(model, &agent("cursor", "  capture: cli\n")),
            "下一轮起生效"
        );
        assert_eq!(
            config_field_effect(model, &agent("codex", "")),
            "下一轮起生效"
        );
    }
}
