//! 开发/调试命令:**仅 debug 构建编入**(`cargo build --release` 时整个模块连同
//! `summon dev` 子命令一起被 cfg 掉 —— help 里看不到、也跑不了)。
//! 这里放的是本地联调用的东西(注入消息、手动发表情/卡片、编译 sidecar),不属于发布面向用户的能力。

use anyhow::Context;
use clap::Subcommand;
use std::process::Command;
use summon_core::config::Config;

use crate::cli::cli::load_reg;
use crate::frontend::client::FeishuClient;
use crate::session::session::{Session, Start};

#[derive(Subcommand)]
pub enum DevAction {
    #[command(about = summon_i18n::text!("r88ac5d4c6e"))]
    /// 本地注入一条消息到会话,打印抓取到的回复(无飞书)
    Send { agent: String, text: Vec<String> },
    #[command(about = summon_i18n::text!("r4edcd9629e"))]
    /// 本地注入一条伪造消息事件到 events.sock(stub,无飞书)
    Inject { agent: String, text: Vec<String> },
    #[command(about = summon_i18n::text!("ra6e5a790fe"))]
    /// 给某条飞书消息加表情(--remove <reaction_id> 先撤旧再加)
    React {
        agent: String,
        message_id: String,
        emoji_type: String,
        #[arg(long)]
        remove: Option<String>,
    },
    #[command(about = summon_i18n::text!("r962b7781c3"))]
    /// 给某个飞书会话发一张无 header markdown 卡片
    Say {
        agent: String,
        chat_id: String,
        text: Vec<String>,
    },
    #[command(about = summon_i18n::text!("r7e5fe51f78"))]
    /// 发一张真·interactive 卡片(带按钮)联调:idle 空闲提示卡 / panel 会话控制面板 /
    /// sched-* 定时器卡片(验证新控件能否被飞书接受渲染)
    Card {
        agent: String,
        chat_id: String,
        #[arg(value_parser = ["idle", "panel", "sched-start", "sched-params", "sched-once", "sched-every"])]
        kind: String,
    },
    #[command(about = summon_i18n::text!("r9ca50d4110"))]
    /// Go sidecar 构建
    Sidecar {
        #[command(subcommand)]
        action: SidecarAction,
    },
    #[command(about = summon_i18n::text!("r946eb288ec"))]
    /// 由主 agent 对给定时间段的洞察汇总跑一次分析并打印(与控制台同一条通路,真机冒烟用)
    InsightsAnalysis {
        #[arg(long, value_name = "YYYY-MM-DD", help = summon_i18n::text!("r13ae8a8f1d"))]
        from: Option<chrono::NaiveDate>,
        #[arg(long, value_name = "YYYY-MM-DD", help = summon_i18n::text!("r7a21a7c0a0"))]
        to: Option<chrono::NaiveDate>,
        #[arg(long, help = summon_i18n::text!("r3eaf4823b4"))]
        agent: Option<String>,
        #[arg(long, default_value = "zh-CN", value_parser = ["zh-CN", "en"], help = summon_i18n::text!("rce7e3cbdb0"))]
        lang: String,
        #[arg(long, help = summon_i18n::text!("rdf49bd200f"))]
        force: bool,
    },
    #[command(about = summon_i18n::text!("rbc1a4c95ec"))]
    /// 录制-回放夹具开关:on/off 写清 run 标志,ls 列 cassette,promote 脱敏入仓 fixture
    Record {
        #[command(subcommand)]
        action: RecordAction,
    },
}

#[derive(Subcommand)]
pub enum RecordAction {
    #[command(about = summon_i18n::text!("rb5aaee27d6"))]
    /// 开录制:写 `~/.summon/run/record`(内容=场景名),daemon 下一条消息起即录(无需重启)
    On {
        #[arg(help = summon_i18n::text!("r478fb1b197"))]
        /// 场景名(raw 子目录名 + promote 默认 fixture 名);缺省 `default`
        scenario: Option<String>,
    },
    #[command(about = summon_i18n::text!("rd94251676a"))]
    /// 关录制:删 `~/.summon/run/record`(已录的 raw 不动)
    Off,
    #[command(about = summon_i18n::text!("r510efa12fd"))]
    /// 列 `~/.summon/raw/` 下已录的场景(cassette)
    Ls,
    #[command(about = summon_i18n::text!("re8d7ceb065"))]
    /// promote:把某场景脱敏(token/secret→占位、open_id→稳定别名)拷进 tests/fixtures/scenarios/<name>/
    Promote {
        scenario: String,
        #[arg(help = summon_i18n::text!("r33b7cbd75e"))]
        /// 强脱敏:连业务正文也抹成 [redacted](默认只脱 open_id,保回放语义最大)
        #[arg(long)]
        redact_content: bool,
        #[arg(help = summon_i18n::text!("reba28ebf5c"))]
        /// 入仓 fixture 名(缺省=场景名)
        #[arg(long)]
        name: Option<String>,
    },
}

#[derive(Subcommand)]
pub enum SidecarAction {
    #[command(about = summon_i18n::text!("rb39e02bfb3"))]
    /// 编译 Go sidecar 并安装到 ~/.summon/bin/summon-sidecar
    Build,
}

pub fn run(cfg: &Config, action: DevAction) -> anyhow::Result<()> {
    match action {
        DevAction::Send { agent, text } => send(cfg, &agent, &text.join(" ")),
        DevAction::Inject { agent, text } => {
            crate::runtime::daemon::inject(&agent, &text.join(" "))
        }
        DevAction::React {
            agent,
            message_id,
            emoji_type,
            remove,
        } => react_cmd(cfg, &agent, &message_id, &emoji_type, remove.as_deref()),
        DevAction::Say {
            agent,
            chat_id,
            text,
        } => say_cmd(cfg, &agent, &chat_id, &text.join(" ")),
        DevAction::Card {
            agent,
            chat_id,
            kind,
        } => card_cmd(cfg, &agent, &chat_id, &kind),
        DevAction::Sidecar { action } => match action {
            SidecarAction::Build => sidecar_build(),
        },
        DevAction::Record { action } => crate::runtime::record_cmd::run(action),
        DevAction::InsightsAnalysis {
            from,
            to,
            agent,
            lang,
            force,
        } => insights_analysis(cfg, from, to, agent, &lang, force),
    }
}

/// 与控制台的「分析」同一条通路:同一份汇总、同一个主 agent、同一个结果目录与缓存键。
/// Ctrl-C 置停机位,按停机的规则打断分析进程。
fn insights_analysis(
    cfg: &Config,
    from: Option<chrono::NaiveDate>,
    to: Option<chrono::NaiveDate>,
    agent: Option<String>,
    lang: &str,
    force: bool,
) -> anyhow::Result<()> {
    use crate::runtime::insights_analysis::{self as analysis, Plan};
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;
    let lang: summon_core::i18n::Language = serde_json::from_value(serde_json::json!(lang))?;
    let now = chrono::Local::now();
    let (range, from, to) = crate::cli::insights::dates(from, to, now.date_naive());
    crate::cli::insights::check(from, to, &now)?;
    let run_dir = crate::runtime::paths::run_dir();
    let view = crate::runtime::dashboard::api::insights::build(
        &run_dir,
        &now,
        range,
        from,
        to,
        agent.as_deref(),
    );
    let summary_json = serde_json::to_string(&view)?;
    let inbox = run_dir.join("inbox");
    let enabled = |alias: &str| {
        crate::runtime::agent_availability::AgentAvailability::lock(&inbox, alias)
            .and_then(|gate| gate.enabled())
            .unwrap_or(true)
    };
    let primary = analysis::primary(
        cfg.primary_agent.as_deref(),
        load_reg(cfg)?.agents,
        &enabled,
        &run_dir,
        now.timestamp_millis(),
    );
    if let Some(p) = &primary {
        println!("primary: {} ({:?})", p.agent.name, p.source);
    }
    let job = match analysis::plan(
        primary.as_ref(),
        analysis::Request {
            from: view.from.clone(),
            to: view.to.clone(),
            agent_filter: agent,
            lang,
            summary_json,
        },
    ) {
        Plan::Unsupported(reason) => anyhow::bail!(summon_i18n::format!("r0238105a1a")),
        Plan::Ready(job) => *job,
    };
    let stop = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(signal_hook::consts::SIGINT, Arc::clone(&stop))?;
    let dir = run_dir.join("insights-analysis");
    let service = analysis::Service::new(
        dir.clone(),
        crate::runtime::turn_ledger::outside_dir_in(&run_dir),
        stop,
    );
    let ran = service
        .run_blocking(job, force)
        .map_err(|_| anyhow::anyhow!(summon_i18n::text!("rb6477a3aaa")))?;
    let mut meta = serde_json::to_value(&ran.record)?;
    if let Some(map) = meta.as_object_mut() {
        map.remove("text");
    }
    println!("{}", serde_json::to_string_pretty(&meta)?);
    println!(
        "file: {}",
        dir.join(format!("{}.json", ran.record.slot)).display()
    );
    if let Some(text) = &ran.record.text {
        println!("\n{text}");
    }
    match ran.error {
        // 失败时槽里原有的成功结果保留着(上面打印的就是它)。
        Some(reason) => anyhow::bail!(summon_i18n::format!("r91a7b5428d")),
        None => Ok(()),
    }
}

/// 本地注入一行到会话并打印抓取到的回复(联调 tmux/scrubber 用,不经飞书)。
fn send(cfg: &Config, name: &str, text: &str) -> anyhow::Result<()> {
    let reg = load_reg(cfg)?;
    let agent = reg
        .get(name)
        .with_context(|| summon_i18n::format!("rddba4b85b3"))?;
    let sess = Session::for_agent(agent, cfg.poll_ms, cfg.debounce_ms);
    match crate::runtime::agent_availability::with_local_start(
        agent,
        &crate::runtime::paths::run_dir().join("inbox"),
        |fresh| Session::for_agent(fresh, cfg.poll_ms, cfg.debounce_ms).ensure_started(),
    )? {
        Start::ColdStarted => {
            eprintln!("{}", summon_i18n::format!("rdc53cb862e", sess.tmux.session))
        }
        Start::Resumed => eprintln!("{}", summon_i18n::format!("rd2c29bddfb", sess.tmux.session)),
        Start::AlreadyRunning => {}
    }
    let reply = sess.send_and_capture(text)?;
    println!("{reply}");
    Ok(())
}

/// 取某 agent 的 FeishuClient(凭据从 agent .env;缺则 dry-run)。
fn agent_feishu(cfg: &Config, agent_name: &str) -> anyhow::Result<FeishuClient> {
    let reg = load_reg(cfg)?;
    let agent = reg
        .get(agent_name)
        .with_context(|| summon_i18n::format!("r265c6a702e"))?;
    let (id, sec) = crate::runtime::daemon::resolve_creds(agent);
    let dry = id.is_none() || sec.is_none();
    Ok(FeishuClient::new(
        &cfg.default_domain,
        id.unwrap_or_default(),
        sec.unwrap_or_default(),
        dry,
    )
    .with_email_mask(crate::runtime::daemon::email_mask_switch(agent)))
}

/// 手动给飞书消息加表情(可先 --remove 撤旧)。日常派发已由 daemon 流式完成,这里只为联调。
fn react_cmd(
    cfg: &Config,
    agent_name: &str,
    message_id: &str,
    emoji_type: &str,
    remove: Option<&str>,
) -> anyhow::Result<()> {
    let valid = summon_core::config::builtin_messaging().reactions;
    if !valid.iter().any(|r| r == emoji_type) {
        anyhow::bail!(summon_i18n::format!("r3470b037b1", valid.join(" ")));
    }
    let fc = agent_feishu(cfg, agent_name)?;
    if let Some(rid) = remove {
        if let Err(e) = fc.unreact(message_id, rid) {
            eprintln!("{}", summon_i18n::format!("r7c18c3d6c2"));
        }
    }
    let rid = fc.react(message_id, emoji_type)?;
    println!("reacted {emoji_type} on {message_id} (reaction_id={rid})");
    Ok(())
}

/// 手动给飞书会话发一张无 header 卡片。日常派发已由 daemon 流式完成,这里只为联调。
fn say_cmd(cfg: &Config, agent_name: &str, chat_id: &str, text: &str) -> anyhow::Result<()> {
    let fc = agent_feishu(cfg, agent_name)?;
    fc.send(chat_id, text)?;
    println!("said to {chat_id}");
    Ok(())
}

/// 手动发一张带按钮的 interactive 卡片(idle 空闲提示 / ask 选项)联调用,复用已测的卡片构造器。
/// 按钮回调要真生效需该 chat 有活跃 ChatHandle(即近期有入站消息);否则 daemon 记「chat 不在线」。
fn card_cmd(cfg: &Config, agent_name: &str, chat_id: &str, kind: &str) -> anyhow::Result<()> {
    let fc = agent_feishu(cfg, agent_name)?;
    let content = match kind {
        "idle" => summon_core::feishu::card_idle_prompt("demo", 10_800),
        "panel" => {
            // 用该 agent 的真实 cli/model/work_dir + 生效的 messaging.models 渲染会话控制面板,
            // 方便真机点按钮联调(回调要生效需该 chat 在线,即近期有入站消息建过 ChatHandle)。
            let reg = load_reg(cfg)?;
            let agent = reg
                .get(agent_name)
                .with_context(|| summon_i18n::format!("r265c6a702e"))?;
            let messaging = crate::runtime::daemon::resolve_messaging(agent);
            let mut opts = summon_core::model_effort::options_for(&messaging, agent.cli_kind());
            // 联调 demo:无运行会话(无实时值),model 按 agent 配置 + 默认值链解析并标来源。
            // 候选和默认都取**默认账号**的目录 —— 这条命令跑在 CLI 进程里,够不着 daemon 按
            // agent 分的那份缓存。配了自己 CODEX_HOME 的 agent 在这里看到的候选会与生产不同。
            let catalog = crate::codex::model_catalog::cached_or_probe(agent.cli_kind());
            let upstream = catalog.as_ref().and_then(|c| c.default_model.clone());
            if let Some(models) = catalog.map(|c| c.models).filter(|m| !m.is_empty()) {
                opts.models = models;
            }
            let default_model = summon_core::model_effort::default_model_for(
                &messaging,
                agent.cli_kind(),
                upstream.as_deref(),
            );
            let resolved_model = summon_core::model_effort::resolve_model(
                None,
                agent.model().or_else(|| agent.rpc_model()),
                &default_model,
            );
            let resolved_effort =
                summon_core::model_effort::resolve_effort(agent.cli_kind(), agent.effort());
            let channel =
                summon_core::model_effort::channel_label(agent.cli_kind(), agent.capture_mode());
            // A(隐私):面板不带工作目录路径(路径不进群)。
            summon_core::feishu::card_session_panel(
                "demo",
                agent.cli_kind(),
                channel,
                &resolved_model,
                &resolved_effort,
                &opts.models,
                &opts.efforts,
                &summon_core::subagent::SubagentDisplay::default(),
                false, // 联调示例:默认关,与生产默认一致
            )
        }
        // 定时器卡片联调:四个选择器控件(multi_select_static / picker_time / date_picker /
        // select_static)是首次引入,**飞书接不接受这张 JSON** 只有真机答得出——发出去即验证。
        "sched-start" => summon_core::feishu::card_schedule_start("", None),
        "sched-params" => {
            let pre = summon_core::schedule::form::Prefill {
                prompt: summon_i18n::text!("ra7e6418ac9").into(),
                time: "09:00".into(),
                days: vec![1, 3],
                ..Default::default()
            };
            summon_core::feishu::card_schedule_params(&summon_core::feishu::ScheduleParamsCard {
                freq: summon_core::schedule::form::CardFreq::Weekly,
                pre: &pre,
                edit_id: None,
                current: Some(summon_i18n::text!("r371ae46a7d")),
                error: None,
            })
        }
        "sched-once" => {
            let pre = summon_core::schedule::form::Prefill {
                prompt: summon_i18n::text!("re25cc0893f").into(),
                date: "2026-08-10".into(),
                time: "15:30".into(),
                ..Default::default()
            };
            summon_core::feishu::card_schedule_params(&summon_core::feishu::ScheduleParamsCard {
                freq: summon_core::schedule::form::CardFreq::Once,
                pre: &pre,
                edit_id: None,
                current: None,
                error: None,
            })
        }
        "sched-every" => {
            let pre = summon_core::schedule::form::Prefill {
                prompt: summon_i18n::text!("rbad9884712").into(),
                every_n: "2".into(),
                every_unit: "h".into(),
                ..Default::default()
            };
            summon_core::feishu::card_schedule_params(&summon_core::feishu::ScheduleParamsCard {
                freq: summon_core::schedule::form::CardFreq::Every,
                pre: &pre,
                edit_id: None,
                current: None,
                error: None,
            })
        }
        other => anyhow::bail!(summon_i18n::format!("ra46df9928d")),
    };
    fc.send_card(chat_id, &content)?;
    println!("sent {kind} card to {chat_id}");
    Ok(())
}

/// 从 current_exe 向上找 `sidecar/feishu-connector/go.mod`(源码目录),`go build` 到
/// ~/.summon/bin/**summon-sidecar**(产物按功能命名,带 summon 前缀;进程真名/日志路径都带前缀)。
fn sidecar_build() -> anyhow::Result<()> {
    let exe = std::env::current_exe()?;
    let src = exe
        .ancestors()
        .map(|a| a.join("sidecar/feishu-connector"))
        .find(|p| p.join("go.mod").is_file())
        .context(summon_i18n::text!("r12c5175984"))?;
    let out_dir = dirs::home_dir()
        .context(summon_i18n::text!("r83264228b8"))?
        .join(".summon/bin");
    std::fs::create_dir_all(&out_dir)?;
    let out = out_dir.join("summon-sidecar");
    println!("go build {} → {}", src.display(), out.display());
    let status = Command::new("go")
        .args(["build", "-o"])
        .arg(&out)
        .arg(".")
        .current_dir(&src)
        .status()?;
    if !status.success() {
        anyhow::bail!(summon_i18n::format!("ra581aebbbc"));
    }
    println!("{}", summon_i18n::format!("r69f7b6b4af", out.display()));
    Ok(())
}
