//! `summon bind` —— 快速绑定 / 新建一个飞书机器人 agent。
//!
//! 飞书**自建应用本身**无法通过 OpenAPI 凭空创建(只能在开放平台控制台建),
//! 所以 bind 做的是:① 引导/打开控制台去建应用 → ② 接收并验真 app_id/secret →
//! ③ 在 `~/.summon/agents/<alias>/` 落一套 agent 自我认知脚手架(SOUL/memory/.env/agent.yaml)。
//! 给了凭据即 `active`(可路由);没给则落 `draft` 脚手架,拿到凭据后再跑一次补上。

use anyhow::{bail, Context};
use std::path::Path;
use summon_core::config::Config;
use summon_core::driver::{CliDriver, ModelEffortError};

use crate::frontend::client::FeishuClient;

pub(crate) fn agent_example_yaml() -> &'static str {
    summon_i18n::include_text!("agent.example.yaml")
}
/// 管家身份的精简模板:没有 runtime / cognition / model —— 它不起会话也不思考。
pub(crate) fn steward_example_yaml() -> &'static str {
    summon_i18n::include_text!("steward.example.yaml")
}
pub(crate) fn soul_example_md() -> &'static str {
    summon_i18n::include_text!("SOUL.example.md")
}
pub(crate) fn memory_example_md() -> &'static str {
    summon_i18n::include_text!("MEMORY.example.md")
}

/// Lark 开发者后台「创建企业自建应用」入口。
pub(crate) const CREATE_APP_URL: &str = "https://open.larkoffice.com/page/launcher";

/// 已有应用的 Lark 后台基础路径，用于拼权限/事件订阅直链。
pub(crate) const CONSOLE_URL: &str = "https://open.larkoffice.com/app";

#[derive(clap::Args, Debug)]
pub struct BindArgs {
    #[arg(help = summon_i18n::text!("r8b855718a6"))]
    /// 英文 alias(目录名 / tmux 名 / env 变量名都用它):小写字母开头,字母数字 - _,2–32 位
    pub alias: String,
    #[arg(help = summon_i18n::text!("rf5af7d8ee7"))]
    /// 花名 / 显示名(如「土星」);缺省用 alias
    #[arg(long)]
    pub display: Option<String>,
    #[arg(help = summon_i18n::text!("reda15bd540"))]
    /// 一句话职责定位,写进 agent.yaml 的 `# 职责:` 注释
    #[arg(long)]
    pub role: Option<String>,
    #[arg(help = summon_i18n::text!("rddd1d381c5"))]
    /// 飞书 app_id;不给则落 draft 脚手架并打印建应用指引
    #[arg(long)]
    pub app_id: Option<String>,
    #[arg(help = summon_i18n::text!("rd5cc259840"))]
    /// 飞书 app_secret;不给但给了 --app-id 时从 stdin 读(避免进 shell history)
    #[arg(long)]
    pub app_secret: Option<String>,
    #[arg(help = summon_i18n::text!("r578919bc30"))]
    /// 干活目录;缺省 ~/.summon/agents/<alias>/workspace
    #[arg(long)]
    pub workspace: Option<String>,
    #[arg(help = summon_i18n::text!("rc4c77b8f94"))]
    /// 启动模型;不给则按 --cli 取 messaging 模型候选桶首项(见 messaging.default.yaml 的
    /// models_by_cli),桶空则不写 model 行、用 CLI 自己的默认
    #[arg(long)]
    pub model: Option<String>,
    #[arg(help = summon_i18n::text!("rf578dcc8ea"))]
    /// 逻辑 reasoning effort 档位(仅 claude/codex;cursor 把档位烤进 model 名,给了会被拒)
    #[arg(long)]
    pub effort: Option<String>,
    #[arg(help = summon_i18n::text!("rd7b3feacab"))]
    /// 启动 cli:claude / codex / cursor
    #[arg(long, default_value = "claude")]
    pub cli: String,
    #[arg(help = summon_i18n::text!("r35e2c1e8c8"))]
    /// 跳过凭据验真(离线 / 网络不通时)
    #[arg(long)]
    pub no_validate: bool,
    #[arg(help = summon_i18n::text!("r5d469e3960"))]
    /// 覆盖已存在的 SOUL.md / agent.yaml(默认拒绝重写身份文件)
    #[arg(long)]
    pub force: bool,
    #[arg(help = summon_i18n::text!("r5de47cd14e"))]
    /// 打开飞书开放平台控制台(macOS `open`)去新建应用
    #[arg(long)]
    pub open_console: bool,
}

/// 校验 alias:小写字母开头,后续字母/数字/`-`/`_`,长度 2–32。
pub(crate) fn validate_alias(alias: &str) -> anyhow::Result<()> {
    let n = alias.chars().count();
    if !(2..=32).contains(&n) {
        bail!(summon_i18n::format!("r1a21bf4c54"));
    }
    let mut chars = alias.chars();
    let first = chars.next().unwrap();
    if !first.is_ascii_lowercase() {
        bail!(summon_i18n::format!("r0b79642881"));
    }
    if let Some(bad) = alias
        .chars()
        .find(|c| !(c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '-' || *c == '_'))
    {
        bail!(summon_i18n::format!("r59beb6213e"));
    }
    Ok(())
}

/// 由 alias 推导 env 变量名:`FEISHU_<ALIAS>_APP_ID` / `_APP_SECRET`(大写,`-`→`_`)。
pub(crate) fn env_var_names(alias: &str) -> (String, String) {
    let up = alias.to_ascii_uppercase().replace('-', "_");
    (
        format!("FEISHU_{up}_APP_ID"),
        format!("FEISHU_{up}_APP_SECRET"),
    )
}

/// 写入 agent.yaml 前校验 model 的形态(`model_effort::check_model_id`)与 (model, effort) 在该 CLI
/// 上合法(fail-closed)。复用 core 的
/// `CliDriver::resolve_launch`(唯一解析点),不在 IO 层另写一套 CLI 知识。Err → 友好文案:
/// effort 不在合法集 → 列该 CLI 的 `legal_efforts`;cursor/Other 给了 effort → 提示「该 CLI 把
/// 档位烤进 model 名 / 不支持」,改用 model 后缀。
/// 这条同样给卡片动作复用（LOCKED 决策 3：共用 validate/resolve 接缝）。
pub(crate) fn validate_model_effort(
    cli: &str,
    model: Option<&str>,
    effort: Option<&str>,
) -> anyhow::Result<()> {
    if let Some(model) = model {
        summon_core::model_effort::check_model_id(model)?;
    }
    let driver = CliDriver::from_kind(cli);
    match driver.resolve_launch(model, effort) {
        Ok(_) => Ok(()),
        Err(ModelEffortError::InvalidEffort { value, allowed, .. }) => {
            bail!(summon_i18n::format!("rb820bb2deb"))
        }
        Err(ModelEffortError::EffortUnsupported { .. }) => {
            bail!(summon_i18n::format!("r9fa2539d1c"))
        }
    }
}

/// onboard/bind 共用:算出要写进 agent.yaml 的 model。显式给了(`--model` / 交互输入)就用显式值——
/// 桶是**候选不是白名单**(codex/claude 侧 model 都是透传给 CLI 的);没给则走 `default_model_for`
/// 的三层回退(钉选 → backend 自报 → 桶首项)。空串视同没给(交互里直接回车读出的就是空串)。
/// 返回 None = 不写 `model:` 行。
/// why 不硬编码默认:各 CLI 的模型命名空间互不通用,把某个 id 当全局默认会在换 cli 时写出起不来的
/// agent.yaml。
///
/// `upstream_default` 收的是**闭包**而不是值,两个理由:起进程问目录那一步不能藏在这个函数里
/// (否则单测会去拉真 CLI),而显式给了 `--model` 时又根本不该问 —— 传值的话那次进程起停必然
/// 发生,结果直接丢掉。
pub(crate) fn effective_model(
    explicit: Option<&str>,
    messaging: &summon_core::config::Messaging,
    cli: &str,
    upstream_default: impl FnOnce() -> Option<String>,
) -> Option<String> {
    explicit
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| {
            summon_core::model_effort::default_model_for(
                messaging,
                cli,
                upstream_default().as_deref(),
            )
            .value
        })
}

/// 把 `cursor-agent models` 的输出切成 model id 列表。
/// cursor 的 model 列表会随版本漂移,故只用于「不在表里 → 友好告警」,不 fail-closed。
pub(crate) fn parse_cursor_models(out: &str) -> Vec<String> {
    crate::codex::model_catalog::parse_cursor_models_output(out).models
}

/// cursor model 串是否在 `cursor-agent models` 的活列表里(membership 判定,纯函数)。
pub(crate) fn cursor_model_in_list(model: &str, list: &[String]) -> bool {
    list.iter().any(|m| m == model)
}

/// bind/onboard 写 cursor agent 前的 best-effort 校验:跑 `cursor-agent models` 拿活列表,
/// model 不在表里 → 友好告警(不 fail);二进制不在 PATH / 拉表失败 → 也只告警放行
/// (LOCKED 决策 1+本期要求:cursor model 由 bind 扁平化,运行期只验串,binary 缺失时容忍)。
pub(crate) fn warn_if_unknown_cursor_model(model: &str) {
    let out = std::process::Command::new(summon_core::tools::resolve(
        summon_core::tools::Tool::CursorAgent,
    ))
    .arg("models")
    .output();
    match out {
        Ok(o) if o.status.success() => {
            let list = parse_cursor_models(&String::from_utf8_lossy(&o.stdout));
            if !list.is_empty() && !cursor_model_in_list(model, &list) {
                println!("{}", summon_i18n::format!("r3594e55756", list.join(", ")));
            }
        }
        _ => println!("{}", summon_i18n::format!("r36c504059d")),
    }
}

/// render_agent_yaml 的入参(避免 too_many_arguments)。
pub(crate) struct AgentSpec<'a> {
    pub(crate) alias: &'a str,
    pub(crate) display: &'a str,
    pub(crate) role: &'a str,
    /// 启动 model。None=不写 `model:` 行(该 cli 无候选桶时),运行期落到 CLI 自己的默认。
    pub(crate) model: Option<&'a str>,
    /// 逻辑 effort 档位(仅 claude/codex;cursor 的 tier 已扁平进 model)。None=不写 effort 行。
    pub(crate) effort: Option<&'a str>,
    pub(crate) cli: &'a str,
    pub(crate) workspace: &'a str,
    pub(crate) id_env: &'a str,
    pub(crate) secret_env: &'a str,
    pub(crate) active: bool,
}

fn render_template(template: &str, values: &[(&str, &str)]) -> String {
    let mut out = template.to_string();
    for (key, value) in values {
        out = out.replace(&format!("{{{{{key}}}}}"), value);
    }
    out
}

/// 渲染 agent.yaml。给了凭据 → active;否则 draft(不可路由)。
pub(crate) fn render_agent_yaml(s: &AgentSpec) -> String {
    let AgentSpec {
        alias,
        display,
        role,
        model,
        effort,
        cli,
        workspace,
        id_env,
        secret_env,
        active,
    } = *s;
    let status = if active { "active" } else { "draft" };
    // model/effort 都是可选行:Some→整行 `<key>: <v>\n`(连尾换行,占位独占一行);None→空串整行
    // 消失。model 必须整行消失而不是留 `model: ` —— 后者会被 serde 读成 `Some("")`,把「用 CLI
    // 默认」变成「指定了空模型名」。
    let model_line = match model {
        Some(m) => format!("model: {m}\n"),
        None => String::new(),
    };
    let effort_line = match effort {
        Some(e) => format!("effort: {e}\n"),
        None => String::new(),
    };
    // 新建的 cursor agent 默认走 `cursor-agent -p`:模型名直接认 `cursor-agent models` 的目录,
    // 每轮带 token 用量。只写进新 agent.yaml;缺省值仍是 ACP,存量 agent 的会话不受影响
    // (两条通道的会话 id 不互通,改缺省会让它们全部断上下文)。
    let capture_line = if cli == "cursor" {
        "  capture: cli\n"
    } else {
        ""
    };
    render_template(
        agent_example_yaml(),
        &[
            ("alias", alias),
            ("display", display),
            ("status", status),
            ("model_line", &model_line),
            ("effort_line", &effort_line),
            ("role", role),
            ("id_env", id_env),
            ("secret_env", secret_env),
            ("workspace", workspace),
            ("cli", cli),
            ("capture_line", capture_line),
        ],
    )
}

/// 管家身份的 agent.yaml。**刻意不复用 [`render_agent_yaml`]**:那份模板的每一段
/// (runtime / cognition / model)对管家都是错的,填空值只会让人以为"配了但没生效",
/// 而少一段就得给模板加条件分支 —— 两份小模板比一份带分支的模板诚实。
pub(crate) fn render_steward_yaml(
    alias: &str,
    display: &str,
    id_env: &str,
    secret_env: &str,
    active: bool,
) -> String {
    render_template(
        steward_example_yaml(),
        &[
            ("alias", alias),
            ("display", display),
            ("status", if active { "active" } else { "draft" }),
            ("id_env", id_env),
            ("secret_env", secret_env),
        ],
    )
}

/// SOUL.md 初稿。模板是空文件:SOUL 会原样拼进 PRIMER,预填的占位文字只会变成噪音。
pub(crate) fn render_soul(alias: &str, display: &str, role: &str) -> String {
    render_template(
        soul_example_md(),
        &[("alias", alias), ("display", display), ("role", role)],
    )
}

/// memory/MEMORY.md 骨架。
pub(crate) fn render_memory(display: &str, alias: &str) -> String {
    render_template(
        memory_example_md(),
        &[("display", display), ("alias", alias)],
    )
}

/// .env 内容(凭据真值;文件已被 ~/.summon gitignore)。
pub(crate) fn render_env(
    display: &str,
    alias: &str,
    id_env: &str,
    id: &str,
    secret_env: &str,
    secret: &str,
) -> String {
    summon_i18n::format!("rbb88f5277f")
}

/// `.env` 的目标权限:只有 owner 可读写。明文 app_secret 落成默认 umask 的 0644,
/// 同机其他本地用户直接读走。
const ENV_FILE_MODE: u32 = 0o600;

/// S3:写 `.env`(明文 app_secret)。走 `atomic_write_mode`——tmp 文件从创建起就是 0600,
/// rename 提交,不留「先 0644 写明文、再 chmod」的可读窗口。bind 与 onboard 共用此收口,
/// 别再裸 `fs::write` 凭据文件。权限无条件按目标值落,存量 0644 重写一次即被收紧。
pub(crate) fn write_env_file(path: &std::path::Path, content: &str) -> anyhow::Result<()> {
    crate::runtime::fsio::atomic_write_mode(path, content.as_bytes(), ENV_FILE_MODE)
}

/// 存量修正:早期 bind 用裸 `fs::write` 落的 `.env` 永远停在 0644,除非重新绑凭据否则
/// 没人会收紧它。见到不是 0600 就 chmod 回去并打一行。返回是否真的改过(供测试与日志判定)。
/// 文件不存在不算错(尚未绑凭据的 draft agent)。
pub(crate) fn ensure_env_mode(path: &std::path::Path) -> anyhow::Result<bool> {
    use std::os::unix::fs::PermissionsExt;
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => {
            return Err(e).with_context(|| summon_i18n::format!("rcf5ad91d1c", path.display()))
        }
    };
    let mode = meta.permissions().mode() & 0o777;
    if mode == ENV_FILE_MODE {
        return Ok(false);
    }
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(ENV_FILE_MODE))
        .with_context(|| summon_i18n::format!("rb538042c1f", path.display()))?;
    println!(
        "{}",
        summon_i18n::format!("r8a321c2b6c", path.display(), mode)
    );
    Ok(true)
}

/// 从 stdin 读一行(去掉换行)。
pub(crate) fn read_line_stdin(prompt: &str) -> anyhow::Result<String> {
    use std::io::Write;
    print!("{prompt}");
    std::io::stdout().flush().ok();
    let mut s = String::new();
    std::io::stdin().read_line(&mut s)?;
    Ok(s.trim().to_string())
}

/// 读一行密钥:stdin 是终端时关掉回显,屏幕上和终端回滚里都不留明文;不是终端(管道、脚本)
/// 时照常读。不进 shell history 是因为它不在命令行参数里。
pub(crate) fn read_secret_stdin(prompt: &str) -> anyhow::Result<String> {
    use std::io::Write;
    print!("{prompt}");
    std::io::stdout().flush().ok();
    let stdin = std::io::stdin();
    let secret = read_without_echo(std::os::fd::AsFd::as_fd(&stdin), &mut stdin.lock())?;
    println!();
    Ok(secret)
}

fn read_without_echo(
    fd: std::os::fd::BorrowedFd<'_>,
    input: &mut impl std::io::BufRead,
) -> anyhow::Result<String> {
    use nix::sys::termios::{tcgetattr, tcsetattr, LocalFlags, SetArg};
    let saved = tcgetattr(fd).ok();
    if let Some(saved) = &saved {
        let mut silent = saved.clone();
        silent.local_flags.remove(LocalFlags::ECHO);
        tcsetattr(fd, SetArg::TCSANOW, &silent).context(summon_i18n::text!("r5022954840"))?;
    }
    let mut line = String::new();
    let read = input.read_line(&mut line);
    if let Some(saved) = &saved {
        // 恢复失败也要把读到的结果交出去;终端停在不回显状态时,人会看到自己敲的字不显示。
        let _ = tcsetattr(fd, SetArg::TCSANOW, saved);
    }
    read?;
    Ok(line.trim().to_string())
}

pub fn run(cfg: &Config, args: BindArgs) -> anyhow::Result<()> {
    validate_alias(&args.alias)?;
    // --model 缺省 → 按 --cli 取候选桶首项(不再硬编码 claude 的 id 当全局默认)。
    let messaging = crate::runtime::daemon::resolve_messaging_global();
    let model = effective_model(args.model.as_deref(), &messaging, &args.cli, || {
        crate::codex::model_catalog::upstream_default_model(&args.cli)
    });
    // 写文件前 fail-closed 校验 (model, effort) 在该 cli 上合法(复用 core 的 resolve_launch)。
    validate_model_effort(&args.cli, model.as_deref(), args.effort.as_deref())?;
    // cursor:model 串已扁平化(含 tier 后缀),best-effort 比对活列表给告警(不 fail)。
    if args.cli == "cursor" {
        if let Some(m) = model.as_deref() {
            warn_if_unknown_cursor_model(m);
        }
    }
    // 新建 agent = 初始化 ~/.summon:做成 git 仓库 + .gitignore 挡密钥(幂等、best-effort)。
    crate::setup::init::ensure();

    if args.open_console {
        println!("{}", summon_i18n::format!("rd9a2eb698f"));
        println!("{CREATE_APP_URL}");
        crate::setup::onboard::open_url(CREATE_APP_URL);
    }

    let display = args.display.clone().unwrap_or_else(|| args.alias.clone());
    let role = args
        .role
        .clone()
        .unwrap_or_else(|| summon_i18n::text!("rc227265794").to_string());
    let (id_env, secret_env) = env_var_names(&args.alias);

    // 凭据:--app-id 给了但 --app-secret 没给 → 从 stdin 读(避免进 history)
    let app_id = args.app_id.clone();
    let app_secret = match (&app_id, &args.app_secret) {
        (Some(_), Some(s)) => Some(s.clone()),
        (Some(_), None) => Some(read_secret_stdin(summon_i18n::text!("rec24c4c0eb"))?),
        (None, _) => None,
    };
    let have_creds = app_id.is_some() && app_secret.as_ref().is_some_and(|s| !s.is_empty());

    // 验真:构造一个真实 client 换 token
    if have_creds && !args.no_validate {
        let fc = FeishuClient::new(
            &cfg.default_domain,
            app_id.clone().unwrap(),
            app_secret.clone().unwrap(),
            false,
        );
        fc.check_auth().context(summon_i18n::text!("r185294bbf7"))?;
        println!("{}", summon_i18n::format!("r1a16f4406a"));
    }

    // 目录脚手架 + .env(bind 与 agents add 共用 scaffold_agent;此函数只保留 bind 特有的
    // 已存在裁决:没带凭据 → bail,而不是 agents add 的「跳过」)。
    let workspace = args
        .workspace
        .clone()
        .unwrap_or_else(|| format!("~/.summon/agents/{}/workspace", args.alias));
    let spec = ScaffoldSpec {
        alias: &args.alias,
        display: &display,
        role: &role,
        cli: &args.cli,
        model: model.as_deref(),
        effort: args.effort.as_deref(),
        workspace: &workspace,
        id_env: &id_env,
        secret_env: &secret_env,
    };
    let creds = if have_creds {
        Some((app_id.clone().unwrap(), app_secret.clone().unwrap()))
    } else {
        None
    };
    let dir = cfg.agents_dir.join(&args.alias);
    match scaffold_agent(cfg, &spec, creds, args.force)? {
        ScaffoldOutcome::Exists => bail!(summon_i18n::format!("red95f5532b", dir.display())),
        ScaffoldOutcome::Created | ScaffoldOutcome::Supplemented => {}
    }

    print_summary(
        &args.alias,
        &display,
        have_creds,
        &dir,
        &id_env,
        &secret_env,
    );
    if have_creds {
        crate::cli::cli::bring_agent_online(&args.alias)?;
    }
    Ok(())
}

/// 内置 agent.yaml 写者共用的非阻塞锁，guard 须覆盖整个读改写过程。
/// 固定保留锁文件，让不同进程始终协调同一个 inode。
pub(crate) fn manifest_write_lock(path: &Path) -> anyhow::Result<std::fs::File> {
    let parent = path.parent().context(summon_i18n::text!("rabcb73a876"))?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(parent.join(".agent.yaml.lock"))?;
    lock.try_lock()
        .map_err(|error| anyhow::anyhow!(summon_i18n::format!("r050968abcc")))?;
    Ok(lock)
}

pub(crate) fn write_if_absent_or_forced(
    path: &Path,
    render: impl FnOnce() -> String,
    force: bool,
) -> anyhow::Result<()> {
    if path.exists() && !force {
        return Ok(());
    }
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p)?;
    }
    let _manifest_lock = if path.file_name().is_some_and(|name| name == "agent.yaml") {
        Some(manifest_write_lock(path)?)
    } else {
        None
    };
    if path.exists() && !force {
        return Ok(());
    }
    std::fs::write(path, render())?;
    Ok(())
}

/// 落盘一个 agent 的身份文件 + .env 的共用核心,bind 与 `agents add` 都走这里。
/// 不读 stdin、不联网:凭据由调用方收集/验真后传入(`None` = 落 draft + 占位 .env)。
/// - 全新目录:建 memory/history + workspace,写 SOUL / agent.yaml / MEMORY / .env;
/// - 已存在 + force=false + 无凭据:返回 [`ScaffoldOutcome::Exists`],**一个字节都不写**
///   (bind 据此 bail、agents add 据此跳过);
/// - 已存在 + 有凭据(补绑):身份文件不动,写 .env 并把 status 翻 active;
/// - force=true:身份文件按模板重写(.env 仍按上面规则,不拿占位覆盖已有真值)。
pub(crate) struct ScaffoldSpec<'a> {
    pub alias: &'a str,
    pub display: &'a str,
    pub role: &'a str,
    pub cli: &'a str,
    pub model: Option<&'a str>,
    pub effort: Option<&'a str>,
    pub workspace: &'a str,
    pub id_env: &'a str,
    pub secret_env: &'a str,
}

pub(crate) enum ScaffoldOutcome {
    /// 全新创建(或 force 重写)。是否 active 由调用方按凭据是否齐全判断。
    Created,
    /// 已存在目录的补绑:只写了 .env + 翻 active,身份文件未动。
    Supplemented,
    /// 已存在且无凭据可补:未写任何东西。
    Exists,
}

pub(crate) fn scaffold_agent(
    cfg: &Config,
    s: &ScaffoldSpec,
    creds: Option<(String, String)>,
    force: bool,
) -> anyhow::Result<ScaffoldOutcome> {
    let dir = cfg.agents_dir.join(s.alias);
    let manifest_path = dir.join("agent.yaml");
    let existed = manifest_path.is_file();
    if existed && !force && creds.is_none() {
        return Ok(ScaffoldOutcome::Exists);
    }
    std::fs::create_dir_all(dir.join("memory/history"))?;
    std::fs::create_dir_all(summon_core::registry::expand_tilde(s.workspace))?;
    let active = creds.is_some();
    if !existed || force {
        write_if_absent_or_forced(
            &dir.join("SOUL.md"),
            || render_soul(s.alias, s.display, s.role),
            force,
        )?;
        write_if_absent_or_forced(
            &manifest_path,
            || {
                render_agent_yaml(&AgentSpec {
                    alias: s.alias,
                    display: s.display,
                    role: s.role,
                    model: s.model,
                    effort: s.effort,
                    cli: s.cli,
                    workspace: s.workspace,
                    id_env: s.id_env,
                    secret_env: s.secret_env,
                    active,
                })
            },
            force,
        )?;
        write_if_absent_or_forced(
            &dir.join("memory/MEMORY.md"),
            || render_memory(s.display, s.alias),
            force,
        )?;
    }
    let env_path = dir.join(".env");
    match creds {
        Some((id, secret)) => {
            write_env_file(
                &env_path,
                &render_env(s.display, s.alias, s.id_env, &id, s.secret_env, &secret),
            )?;
            // 补绑路径(已存在且未 force):身份文件没重写,status 还是 draft,需要翻 active。
            // force 路径的 agent.yaml 刚按 active=true 渲染过,不用再翻。
            if existed && !force {
                flip_status_active(&manifest_path)?;
                ensure_env_mode(&env_path)?;
                return Ok(ScaffoldOutcome::Supplemented);
            }
        }
        None if !env_path.exists() => {
            // 占位 .env 也走 0600:用户随后会把真 secret 填进同一个文件。
            write_env_file(
                &env_path,
                &render_env(s.display, s.alias, s.id_env, "", s.secret_env, ""),
            )?;
        }
        None => {}
    }
    ensure_env_mode(&env_path)?;
    Ok(ScaffoldOutcome::Created)
}

/// agent.yaml 的运行时改写统一收口:同目录 tmp → fsync → rename,不做就地覆盖。
/// agent.yaml 是身份 + 鉴权名单的唯一真相源,读者(daemon/CLI)随时可能并发读,
/// 就地覆盖会让读者看到半截文件。
/// 保留原文件权限位:`atomic_write` 建的是新 inode,权限会退回 umask 默认(通常 0644),
/// 不能因为换了写法就把用户手动收紧过的 agent.yaml 放宽。
fn rewrite_manifest(manifest_path: &Path, expected: &str, content: &str) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let metadata = std::fs::metadata(manifest_path)?;
    let mode = metadata.permissions().mode() & 0o777;
    let mtime = metadata.modified()?;
    crate::runtime::fsio::atomic_write_mode_checked(manifest_path, content.as_bytes(), mode, || {
        let current = std::fs::metadata(manifest_path)?;
        anyhow::ensure!(
            std::fs::read_to_string(manifest_path)? == expected
                && current.modified()? == mtime
                && current.permissions().mode() & 0o777 == mode,
            summon_i18n::format!("r1015af2c38")
        );
        Ok(())
    })
}

/// 把已存在 agent.yaml 的 `status: draft` 翻成 `status: active`(补绑凭据后)。
pub(crate) fn flip_status_active(manifest_path: &Path) -> anyhow::Result<()> {
    let _manifest_lock = manifest_write_lock(manifest_path)?;
    let s = std::fs::read_to_string(manifest_path)?;
    let out = s.replace("status: draft", "status: active");
    if out != s {
        rewrite_manifest(manifest_path, &s, &out)?;
    }
    Ok(())
}

/// agent.yaml 里 `feishu:` 下的 talk 名单(`allowed_users`)还空着时,把它和 operate 名单
/// (`operators`)填成给定的这一个人(onboard 联调抓到的当前用户);operate 名单这一行不存在(早期
/// 脚手架)就补在 talk 名单后面。talk 名单已有人工值时两张都不动 —— 空的 operate 名单回退到
/// talk 名单,这时填它会把原来能操作的人挤出去。返回实际填了哪几张表。
pub(crate) fn fill_empty_member_lists(
    manifest_path: &Path,
    id: &str,
) -> anyhow::Result<Vec<&'static str>> {
    let _manifest_lock = manifest_write_lock(manifest_path)?;
    let s = std::fs::read_to_string(manifest_path)?;
    let mut lines: Vec<String> = s.lines().map(str::to_string).collect();
    let Some(feishu) = lines.iter().position(|line| line.trim_end() == "feishu:") else {
        return Ok(Vec::new());
    };
    let block_end = lines[feishu + 1..]
        .iter()
        .position(|line| !line.trim().is_empty() && !line.starts_with(char::is_whitespace))
        .map_or(lines.len(), |offset| feishu + 1 + offset);
    let find = |lines: &[String], key: &str| {
        (feishu + 1..block_end).find(|&i| lines[i].trim_start().starts_with(&format!("{key}:")))
    };
    let mut filled = Vec::new();
    let Some(talk_line) = find(&lines, "allowed_users") else {
        return Ok(filled);
    };
    let Some(line) = empty_list_line(&lines, talk_line, "allowed_users", id) else {
        return Ok(filled);
    };
    lines[talk_line] = line;
    filled.push("allowed_users");
    match find(&lines, "operators") {
        Some(at) => {
            if let Some(line) = empty_list_line(&lines, at, "operators", id) {
                lines[at] = line;
                filled.push("operators");
            }
        }
        None => {
            let indent: String = lines[talk_line]
                .chars()
                .take_while(|c| c.is_whitespace())
                .collect();
            lines.insert(talk_line + 1, format!("{indent}operators: [\"{id}\"]"));
            filled.push("operators");
        }
    }
    let out = rejoin_like(&s, &lines);
    if out != s {
        rewrite_manifest(manifest_path, &s, &out)?;
    }
    Ok(filled)
}

/// `key:` 这一行若是空名单(`[]` 或空值且下面没有 `- ` 列表项),给出填好后的整行(保留缩进与
/// 行尾注释);否则 `None`。
fn empty_list_line(lines: &[String], at: usize, key: &str, id: &str) -> Option<String> {
    let line = &lines[at];
    let indent: String = line.chars().take_while(|c| c.is_whitespace()).collect();
    let rest = line.trim_start()[key.len() + 1..].to_string();
    let (value, comment) = match rest.find(" #") {
        Some(hash) => {
            let value_end = rest[..hash].trim_end().len();
            (
                rest[..value_end].trim().to_string(),
                rest[value_end..].to_string(),
            )
        }
        None => (rest.trim().to_string(), String::new()),
    };
    let empty = match value.as_str() {
        "[]" => true,
        "" => !lines
            .get(at + 1)
            .is_some_and(|next| next.trim_start().starts_with("- ")),
        _ => false,
    };
    empty.then(|| format!("{indent}{key}: [\"{id}\"]{comment}"))
}

/// 把 agent.yaml 顶层 `model:` 行整行替换成给定模型名(飞书面板切模型 / 命令行改配置用)。
/// 顶层(列 0)`model:` 行整行替换;没有则在 `status:` 行后插一行;再没有就置顶。保留尾部换行风格。
/// 注意 `models:`(消息层模型预设)以 `model:` 为前缀但不相等,不会被误伤。
pub(crate) fn set_model(manifest_path: &Path, model: &str) -> anyhow::Result<()> {
    let _manifest_lock = manifest_write_lock(manifest_path)?;
    let s = std::fs::read_to_string(manifest_path)?;
    let mut lines: Vec<String> = s.lines().map(str::to_string).collect();
    if let Some(pos) = lines.iter().position(|l| l.starts_with("model:")) {
        lines[pos] = format!("model: {model}");
    } else {
        let at = lines
            .iter()
            .position(|l| l.trim_start().starts_with("status:"))
            .map(|p| p + 1)
            .unwrap_or(0);
        lines.insert(at, format!("model: {model}"));
    }
    let out = rejoin_like(&s, &lines);
    if out != s {
        rewrite_manifest(manifest_path, &s, &out)?;
    }
    Ok(())
}

/// 把 agent.yaml 顶层 `effort:` 行整行替换成给定档位(飞书面板切 effort / 命令行改配置用)。
/// 顶层(列 0)`effort:` 行整行替换;没有则在 `model:` 行后插一行,再没有就在 `status:` 后,
/// 都没有就置顶。保留尾部换行风格。注意 `efforts:`(消息层 effort 预设)以 `effort` 为前缀但
/// 带 `s:`,用精确 `effort:` 前缀匹配避免误伤。
/// 卡片 effort 动作(`handle_card_set_effort`)已接线到此,是 daemon 进程内的活路径。
pub(crate) fn set_effort(manifest_path: &Path, effort: &str) -> anyhow::Result<()> {
    let _manifest_lock = manifest_write_lock(manifest_path)?;
    let s = std::fs::read_to_string(manifest_path)?;
    let mut lines: Vec<String> = s.lines().map(str::to_string).collect();
    if let Some(pos) = lines.iter().position(|l| l.starts_with("effort:")) {
        lines[pos] = format!("effort: {effort}");
    } else {
        let at = lines
            .iter()
            .position(|l| l.starts_with("model:"))
            .or_else(|| {
                lines
                    .iter()
                    .position(|l| l.trim_start().starts_with("status:"))
            })
            .map(|p| p + 1)
            .unwrap_or(0);
        lines.insert(at, format!("effort: {effort}"));
    }
    let out = rejoin_like(&s, &lines);
    if out != s {
        rewrite_manifest(manifest_path, &s, &out)?;
    }
    Ok(())
}

/// 把 agent.yaml `runtime.working_dir` 行整行替换成给定路径(飞书面板改工作目录 / 命令行改配置用)。
/// 缩进的 `working_dir:` 行整行替换(保留缩进、值加引号);没有则在 `runtime:` 行后插一行(缩进 2 空格);
/// 连 `runtime:` 都没有则在末尾补一整段。保留尾部换行风格。
// 保留:agent 级 working_dir 写入器。面板改工作目录已下沉成 per-chat(`.cwd` 覆盖,见
// `handle_card_set_workdir`),此函数当前仅单测覆盖;留作未来 agent 级默认的命令行/绑定入口。
#[allow(dead_code)]
pub(crate) fn set_working_dir(manifest_path: &Path, dir: &str) -> anyhow::Result<()> {
    let _manifest_lock = manifest_write_lock(manifest_path)?;
    let s = std::fs::read_to_string(manifest_path)?;
    let mut lines: Vec<String> = s.lines().map(str::to_string).collect();
    if let Some(pos) = lines
        .iter()
        .position(|l| l.trim_start().starts_with("working_dir"))
    {
        let indent: String = lines[pos]
            .chars()
            .take_while(|c| c.is_whitespace())
            .collect();
        lines[pos] = format!("{indent}working_dir: \"{dir}\"");
    } else if let Some(pos) = lines
        .iter()
        .position(|l| l.trim_start().starts_with("runtime:"))
    {
        lines.insert(pos + 1, format!("  working_dir: \"{dir}\""));
    } else {
        lines.push("runtime:".to_string());
        lines.push(format!("  working_dir: \"{dir}\""));
    }
    let out = rejoin_like(&s, &lines);
    if out != s {
        rewrite_manifest(manifest_path, &s, &out)?;
    }
    Ok(())
}

/// 把改写后的行重新拼回,保留原文本是否以换行结尾的风格。
fn rejoin_like(orig: &str, lines: &[String]) -> String {
    let out = lines.join("\n");
    if orig.ends_with('\n') {
        format!("{out}\n")
    } else {
        out
    }
}

pub(crate) fn print_summary(
    alias: &str,
    display: &str,
    active: bool,
    dir: &Path,
    id_env: &str,
    secret_env: &str,
) {
    println!("{}", summon_i18n::format!("rf36d2381b1", dir.display()));
    println!(
        "{}",
        summon_i18n::format!(
            "r641f3194d4",
            if active {
                summon_i18n::text!("rcf873b9641")
            } else {
                summon_i18n::text!("rb9a7b9024b")
            }
        )
    );
    println!("{}", summon_i18n::format!("r9344a49d87"));
    if active {
        println!("{}", summon_i18n::format!("r85710f6141"));
        println!("{}", summon_i18n::format!("rb5ee3d860c"));
        println!("{}", summon_i18n::format!("r039f1a4a86"));
    } else {
        println!("{}", summon_i18n::format!("rf3e6de20d8"));
        println!("{CREATE_APP_URL}");
        println!("{}", summon_i18n::format!("re34be72cd0"));
        println!("  summon bind {alias} --app-id <id> --app-secret <secret>");
        println!("{}", summon_i18n::format!("r9790343a1c"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_env_file_sets_owner_only_mode() {
        // S3:.env 含明文 app_secret,落盘必须 0600(默认 umask 是 0644,同机他人可读)。
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join(".env");
        write_env_file(&p, "FEISHU_APP_SECRET=shh\n").unwrap();
        let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, ".env 权限应为 0600");
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            "FEISHU_APP_SECRET=shh\n"
        );
    }

    /// 「对 agent.yaml 做一次运行时改写」的统一签名,供下面表驱动地遍历全部改写器。
    type ManifestRewriter = Box<dyn Fn(&Path) -> anyhow::Result<()>>;

    /// agent.yaml 的 5 个运行时改写器都必须走 tmp→rename 提交:rename 换 inode,就地
    /// `fs::write` 不换。拿 inode 变化当「确实原子提交」的可观测证据,顺带断言内容完整、
    /// 目录里不留 `.agent.yaml.*.tmp` 残骸。
    #[test]
    fn manifest_rewriters_commit_by_rename_leaving_no_tmp() {
        use std::os::unix::fs::MetadataExt;

        let base = "name: x\nstatus: draft\nmodel: opus\neffort: low\nruntime:\n  \
                    working_dir: \"~/old\"\nfeishu:\n  allowed_users: []\n";
        // (改写器名, 改写器, 断言改完后 agent.yaml 里应出现的串)
        let cases: Vec<(&str, ManifestRewriter, &str)> = vec![
            (
                "flip_status_active",
                Box::new(flip_status_active),
                "status: active",
            ),
            (
                "fill_empty_member_lists",
                Box::new(|p| fill_empty_member_lists(p, "ou_x").map(drop)),
                "allowed_users: [\"ou_x\"]",
            ),
            (
                "set_model",
                Box::new(|p| set_model(p, "sonnet-4.5")),
                "model: sonnet-4.5",
            ),
            (
                "set_effort",
                Box::new(|p| set_effort(p, "high")),
                "effort: high",
            ),
            (
                "set_working_dir",
                Box::new(|p| set_working_dir(p, "~/new")),
                "working_dir: \"~/new\"",
            ),
        ];
        for (name, rewrite, expect) in cases {
            let dir = tempfile::tempdir().unwrap();
            let p = dir.path().join("agent.yaml");
            std::fs::write(&p, base).unwrap();
            let ino_before = std::fs::metadata(&p).unwrap().ino();

            rewrite(&p).unwrap();

            let ino_after = std::fs::metadata(&p).unwrap().ino();
            assert_ne!(ino_before, ino_after, "{name} 应经 rename 提交(inode 变化)");
            let s = std::fs::read_to_string(&p).unwrap();
            assert!(s.contains(expect), "{name} 改写结果不含 {expect}: {s}");
            assert!(s.contains("name: x"), "{name} 其它行应原样保留: {s}");
            assert!(s.ends_with('\n'), "{name} 应保留尾换行");
            let mut files: Vec<_> = std::fs::read_dir(dir.path())
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .collect();
            files.sort();
            assert_eq!(
                files,
                [
                    std::ffi::OsString::from(".agent.yaml.lock"),
                    std::ffi::OsString::from("agent.yaml")
                ],
                "{name} 只保留清单和共享锁文件"
            );
        }
    }

    /// 原子写建的是新 inode,权限会退回 umask 默认(0644)。agent.yaml 是身份 + 鉴权名单的
    /// 事实源,不能因为换了写法就把用户手动收紧过的权限放宽。
    #[test]
    fn manifest_rewrite_preserves_existing_mode() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("agent.yaml");
        std::fs::write(&p, "name: x\nstatus: draft\n").unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).unwrap();
        flip_status_active(&p).unwrap();
        assert_eq!(
            std::fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn write_env_file_tightens_preexisting_world_readable_file() {
        // 存量 .env 早期用裸 fs::write 落成 0644;重写(补绑凭据)必须无条件收紧回 0600。
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join(".env");
        std::fs::write(&p, "OLD=1\n").unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
        write_env_file(&p, "NEW=2\n").unwrap();
        assert_eq!(
            std::fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "NEW=2\n");
    }

    #[test]
    fn ensure_env_mode_repairs_legacy_world_readable_env() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join(".env");
        std::fs::write(&p, "FEISHU_X_APP_SECRET=shh\n").unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();

        assert!(ensure_env_mode(&p).unwrap(), "0644 应被判为需要修正");
        assert_eq!(
            std::fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o600
        );
        // 内容不动
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            "FEISHU_X_APP_SECRET=shh\n"
        );
        // 幂等:已是 0600 → 不再改、不再刷日志
        assert!(!ensure_env_mode(&p).unwrap(), "0600 应判为无需修正");
    }

    #[test]
    fn ensure_env_mode_tolerates_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!ensure_env_mode(&dir.path().join(".env")).unwrap());
    }

    #[test]
    fn alias_rules() {
        assert!(validate_alias("saturn").is_ok());
        assert!(validate_alias("nimbus-7").is_ok());
        assert!(validate_alias("a_b").is_ok());
        assert!(validate_alias("A").is_err()); // 大写
        assert!(validate_alias("1abc").is_err()); // 数字开头
        assert!(validate_alias("ab c").is_err()); // 空格
        assert!(validate_alias("x").is_err()); // 太短
        assert!(validate_alias("说话").is_err()); // 非 ascii
    }

    #[test]
    fn env_names_derive() {
        assert_eq!(
            env_var_names("saturn"),
            (
                "FEISHU_SATURN_APP_ID".into(),
                "FEISHU_SATURN_APP_SECRET".into()
            )
        );
        assert_eq!(
            env_var_names("nimbus-7").0,
            "FEISHU_NIMBUS_7_APP_ID".to_string()
        );
    }

    #[test]
    fn yaml_active_vs_draft_and_parses() {
        let mut spec = AgentSpec {
            alias: "saturn",
            display: "土星",
            role: "测试",
            model: Some("claude-opus-4-8"),
            effort: None,
            cli: "claude",
            workspace: "~/ws",
            id_env: "FEISHU_SATURN_APP_ID",
            secret_env: "FEISHU_SATURN_APP_SECRET",
            active: true,
        };
        let active = render_agent_yaml(&spec);
        assert!(active.contains("status: active"));
        assert!(active.contains("app_id_env: FEISHU_SATURN_APP_ID"));
        assert!(active.contains("tmux_name: soul-saturn"));
        // 渲染出的 YAML 必须能被 Agent 解析(catch 缩进/转义错)
        let a: summon_core::registry::Agent = serde_yaml::from_str(&active).unwrap();
        assert_eq!(a.name, "saturn");
        assert_eq!(a.display, "土星");
        assert_eq!(a.tmux_name(), "soul-saturn");
        assert!(a.is_routable());

        spec.active = false;
        assert!(render_agent_yaml(&spec).contains("status: draft"));
    }

    #[test]
    fn new_cursor_agents_default_to_the_cli_capture_and_others_do_not() {
        let spec = |cli| AgentSpec {
            alias: "nemo",
            display: "nemo",
            role: "测试",
            model: None,
            effort: None,
            cli,
            workspace: "~/ws",
            id_env: "FEISHU_NEMO_APP_ID",
            secret_env: "FEISHU_NEMO_APP_SECRET",
            active: true,
        };
        let cursor: summon_core::registry::Agent =
            serde_yaml::from_str(&render_agent_yaml(&spec("cursor"))).unwrap();
        assert_eq!(
            cursor.capture_mode(),
            summon_core::registry::CaptureMode::CliSession
        );
        for cli in ["claude", "codex"] {
            let y = render_agent_yaml(&spec(cli));
            assert!(!y.contains("capture:"), "{cli} 不该写 capture 行:\n{y}");
            serde_yaml::from_str::<summon_core::registry::Agent>(&y).unwrap();
        }
    }

    #[test]
    fn yaml_omits_model_line_when_no_default_available() {
        let spec = AgentSpec {
            alias: "nemo",
            display: "nemo",
            role: "测试",
            model: None,
            effort: None,
            cli: "claude",
            workspace: "~/ws",
            id_env: "FEISHU_NEMO_APP_ID",
            secret_env: "FEISHU_NEMO_APP_SECRET",
            active: true,
        };
        let y = render_agent_yaml(&spec);
        // 没默认可用时整行消失(不是 `model: ` 空值,那会被 serde 读成 Some(""))。
        assert!(!y.contains("model:"), "model 行应整行消失:\n{y}");
        let a: summon_core::registry::Agent = serde_yaml::from_str(&y).unwrap();
        assert_eq!(a.model(), None);
    }

    /// 显式给了 `--model` 就不该去问 backend:那次问要起一个 app-server 子进程,结果又直接丢掉。
    #[test]
    fn an_explicit_model_never_triggers_an_upstream_probe() {
        let m = summon_core::config::builtin_messaging();
        let mut probed = false;
        let model = effective_model(Some("gpt-5.5"), &m, "codex", || {
            probed = true;
            None
        });
        assert_eq!(model.as_deref(), Some("gpt-5.5"));
        assert!(!probed, "显式值已经够了,不该再起进程问目录");
    }

    #[test]
    fn effective_model_prefers_explicit_then_per_cli_bucket_head() {
        let mut m = summon_core::config::builtin_messaging();
        m.models.clear();
        m.models_by_cli.clear();
        m.models_by_cli
            .insert("codex".into(), vec!["gpt-5.6-sol".into()]);
        // 显式给了就用显式值(哪怕不在桶里——桶是候选不是白名单,codex 侧 model 本就透传)。
        assert_eq!(
            effective_model(Some("gpt-5.5"), &m, "codex", || None),
            Some("gpt-5.5".into())
        );
        // 没给 → 取该 cli 桶首项,而不是某个硬编码的 claude id。
        assert_eq!(
            effective_model(None, &m, "codex", || None),
            Some("gpt-5.6-sol".into())
        );
        // 空串视同没给(交互里直接回车会读出空串)。
        assert_eq!(
            effective_model(Some("  "), &m, "codex", || None),
            Some("gpt-5.6-sol".into())
        );
        // 该 cli 无桶且扁平表空 → None(不写 model 行)。
        assert_eq!(effective_model(None, &m, "claude", || None), None);
    }

    #[test]
    fn generated_identity_files_come_from_example_templates() {
        assert!(agent_example_yaml().contains("{{alias}}"));
        // SOUL 初稿为空:人设由 owner 自己写,模板不预填任何内容(它会原样进 PRIMER)。
        assert!(soul_example_md().is_empty());
        assert!(memory_example_md().contains("{{alias}}"));

        let spec = AgentSpec {
            alias: "blue-wing",
            display: "蓝翼飞龙",
            role: "压场",
            model: Some("gpt-5.5-high"),
            effort: None,
            cli: "cursor",
            workspace: "~/projects",
            id_env: "FEISHU_BLUE_WING_APP_ID",
            secret_env: "FEISHU_BLUE_WING_APP_SECRET",
            active: true,
        };
        let agent_yaml = render_agent_yaml(&spec);
        let soul = render_soul(spec.alias, spec.display, spec.role);
        let memory = render_memory(spec.display, spec.alias);

        for rendered in [agent_yaml, soul, memory] {
            assert!(!rendered.contains("{{"), "模板占位符应全部被替换");
        }
    }

    #[test]
    fn a_secret_typed_into_a_terminal_is_not_echoed_and_echo_comes_back() {
        use nix::sys::termios::{tcgetattr, LocalFlags};
        use std::io::{Read, Write};
        use std::os::fd::AsFd;
        let pty = nix::pty::openpty(None, None).unwrap();
        let mut master = std::fs::File::from(pty.master);
        let slave = std::fs::File::from(pty.slave);
        assert!(tcgetattr(slave.as_fd())
            .unwrap()
            .local_flags
            .contains(LocalFlags::ECHO));
        let reader = std::thread::spawn(move || {
            let mut input = std::io::BufReader::new(slave.try_clone().unwrap());
            let secret = read_without_echo(slave.as_fd(), &mut input).unwrap();
            let echo_back = tcgetattr(slave.as_fd())
                .unwrap()
                .local_flags
                .contains(LocalFlags::ECHO);
            (secret, echo_back, slave)
        });
        std::thread::sleep(std::time::Duration::from_millis(200));
        master.write_all(b"s3cret\n").unwrap();
        let (secret, echo_back, _slave) = reader.join().unwrap();
        assert_eq!(secret, "s3cret");
        assert!(echo_back, "读完必须恢复回显");
        nix::fcntl::fcntl(
            &master,
            nix::fcntl::FcntlArg::F_SETFL(nix::fcntl::OFlag::O_NONBLOCK),
        )
        .unwrap();
        let mut echoed = Vec::new();
        let _ = master.read_to_end(&mut echoed);
        assert!(
            !String::from_utf8_lossy(&echoed).contains("s3cret"),
            "密钥被回显到了终端: {:?}",
            String::from_utf8_lossy(&echoed)
        );
    }

    #[test]
    fn member_lists_default_to_the_onboarding_user_without_overriding_manual_values() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("agent.yaml");
        std::fs::write(
            &p,
            "name: x\nfeishu:\n  app_id_env: A\n  allowed_users: []   # talk 名单\n  operators: []       # operate 名单\n\nruntime:\n  cli: claude\n",
        )
        .unwrap();
        assert_eq!(
            fill_empty_member_lists(&p, "on_owner").unwrap(),
            ["allowed_users", "operators"]
        );
        let written = std::fs::read_to_string(&p).unwrap();
        assert!(
            written.contains("  allowed_users: [\"on_owner\"]   # talk 名单\n"),
            "{written}"
        );
        assert!(
            written.contains("  operators: [\"on_owner\"]       # operate 名单\n"),
            "{written}"
        );
        let agent: summon_core::registry::Agent = serde_yaml::from_str(&written).unwrap();
        assert!(agent.allows("", "on_owner"));
        assert!(agent.can_operate(
            "",
            "on_owner",
            true,
            summon_core::registry::OperateScope::Agent
        ));
        assert!(!agent.allows("ou_stranger", ""));

        // 已有人工值:原样保留,不追加也不覆盖。
        std::fs::write(
            &p,
            "name: x\nfeishu:\n  app_id_env: A\n  allowed_users:\n    - ou_mate\n  operators: [\"ou_lead\"]\n",
        )
        .unwrap();
        assert!(fill_empty_member_lists(&p, "on_owner").unwrap().is_empty());
        let kept = std::fs::read_to_string(&p).unwrap();
        assert!(kept.contains("    - ou_mate\n") && kept.contains("operators: [\"ou_lead\"]"));
        assert!(!kept.contains("on_owner"));

        // talk 名单有人工值、operate 名单空着或没有这一行:都不动,operate 仍回退到 talk 名单。
        for manifest in [
            "name: x\nfeishu:\n  app_id_env: A\n  allowed_users: [\"ou_mate\", \"ou_lead\"]\n  operators: []\n",
            "name: x\nfeishu:\n  app_id_env: A\n  allowed_users: [\"ou_mate\", \"ou_lead\"]\n",
        ] {
            std::fs::write(&p, manifest).unwrap();
            assert!(fill_empty_member_lists(&p, "on_owner").unwrap().is_empty());
            assert_eq!(std::fs::read_to_string(&p).unwrap(), manifest);
        }

        // 早期脚手架没有 operators 行:补在 talk 名单后面。
        std::fs::write(
            &p,
            "name: x\nfeishu:\n  app_id_env: A\n  allowed_users: []\n",
        )
        .unwrap();
        assert_eq!(
            fill_empty_member_lists(&p, "on_owner").unwrap(),
            ["allowed_users", "operators"]
        );
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            "name: x\nfeishu:\n  app_id_env: A\n  allowed_users: [\"on_owner\"]\n  operators: [\"on_owner\"]\n"
        );
    }

    #[test]
    fn flip_draft_to_active() {
        let dir = std::env::temp_dir().join(format!("summon-bind-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("agent.yaml");
        std::fs::write(&p, "name: x\nstatus: draft\n").unwrap();
        flip_status_active(&p).unwrap();
        let s = std::fs::read_to_string(&p).unwrap();
        assert!(s.contains("status: active"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn set_model_replaces_top_level_line_and_spares_messaging_models() {
        let dir = std::env::temp_dir().join(format!("summon-setmodel-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("agent.yaml");
        std::fs::write(
            &p,
            "name: x\nstatus: active\nmodel: opus-4.8\nmessaging:\n  models:\n    - a\n    - b\n",
        )
        .unwrap();
        set_model(&p, "gpt-5.5-high").unwrap();
        let s = std::fs::read_to_string(&p).unwrap();
        assert!(s.contains("model: gpt-5.5-high"));
        assert!(!s.contains("model: opus-4.8"));
        // 不误伤 messaging.models
        assert!(s.contains("models:"));
        assert!(s.contains("- a"));
        assert!(s.ends_with('\n'));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn set_model_inserts_after_status_when_absent() {
        let dir = std::env::temp_dir().join(format!("summon-setmodel2-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("agent.yaml");
        std::fs::write(&p, "name: x\nstatus: active\nfeishu:\n  scope: thread\n").unwrap();
        set_model(&p, "sonnet-4.5").unwrap();
        let s = std::fs::read_to_string(&p).unwrap();
        assert!(s.contains("status: active\nmodel: sonnet-4.5\n"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn set_working_dir_replaces_indented_line_preserving_indent() {
        let dir = std::env::temp_dir().join(format!("summon-setwd-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("agent.yaml");
        std::fs::write(
            &p,
            "name: x\nruntime:\n  working_dir: \"~/old\"\n  cli: cursor\n",
        )
        .unwrap();
        set_working_dir(&p, "~/projects").unwrap();
        let s = std::fs::read_to_string(&p).unwrap();
        assert!(s.contains("  working_dir: \"~/projects\""));
        assert!(!s.contains("~/old"));
        assert!(s.contains("  cli: cursor")); // 其它行不动
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn set_working_dir_inserts_under_runtime_when_absent() {
        let dir = std::env::temp_dir().join(format!("summon-setwd2-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("agent.yaml");
        std::fs::write(&p, "name: x\nruntime:\n  cli: cursor\n").unwrap();
        set_working_dir(&p, "~/projects").unwrap();
        let s = std::fs::read_to_string(&p).unwrap();
        assert!(s.contains("runtime:\n  working_dir: \"~/projects\"\n  cli: cursor"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn validate_model_effort_accepts_legal_and_rejects_illegal() {
        // claude:合法 effort 通过
        assert!(validate_model_effort("claude", Some("claude-opus-4-8"), Some("high")).is_ok());
        // claude:非法 effort 报错且文案列合法集
        let err =
            validate_model_effort("claude", Some("claude-opus-4-8"), Some("minimal")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("minimal"), "错误应含坏值: {msg}");
        assert!(
            msg.contains("low/medium/high/xhigh/max"),
            "错误应列合法集: {msg}"
        );
        // codex:GPT-5.6 新增的 max 合法。
        assert!(validate_model_effort("codex", Some("gpt-5.6-sol"), Some("max")).is_ok());
        // 未知档位仍拒写。
        assert!(validate_model_effort("codex", Some("gpt-5.6-sol"), Some("warp")).is_err());
        // 无 effort 永远通过(完全向后兼容)
        assert!(validate_model_effort("claude", Some("claude-opus-4-8"), None).is_ok());
        assert!(validate_model_effort("cursor", Some("gpt-5.5-high"), None).is_ok());
        // 不像模型 id 的输入在写盘前拦下。
        assert!(validate_model_effort("claude", Some("切到 sonnet 吧"), None).is_err());
        assert!(validate_model_effort("codex", Some("gpt 5.6"), None).is_err());
    }

    #[test]
    fn validate_model_effort_rejects_effort_for_cursor() {
        // cursor:--effort 不允许(tier 烤进 model 名),错误提示改用 model 后缀
        let err = validate_model_effort("cursor", Some("gpt-5.5"), Some("high")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("cursor"), "错误应点名 cursor: {msg}");
    }

    #[test]
    fn set_effort_replaces_top_level_line_not_messaging_efforts() {
        let dir = std::env::temp_dir().join(format!("summon-seteffort-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("agent.yaml");
        std::fs::write(
            &p,
            "name: x\nstatus: active\nmodel: opus\neffort: low\nmessaging:\n  efforts:\n    - a\n    - b\n",
        )
        .unwrap();
        set_effort(&p, "high").unwrap();
        let s = std::fs::read_to_string(&p).unwrap();
        assert!(s.contains("effort: high"));
        assert!(!s.contains("effort: low"));
        // 不误伤 messaging.efforts
        assert!(s.contains("efforts:"));
        assert!(s.contains("- a"));
        assert!(s.ends_with('\n'));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn set_effort_inserts_after_model_when_absent() {
        let dir = std::env::temp_dir().join(format!("summon-seteffort2-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("agent.yaml");
        std::fs::write(&p, "name: x\nstatus: active\nmodel: opus\n").unwrap();
        set_effort(&p, "high").unwrap();
        let s = std::fs::read_to_string(&p).unwrap();
        assert!(s.contains("model: opus\neffort: high\n"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn cursor_models_parse_and_membership() {
        let out = "Available models\n\nclaude-opus-4-8-high - Opus 4.8 High\n  \
gpt-5.5-extra-high - GPT-5.5 Extra High (default)  \n\ncomposer-1 - Composer 1\n\n\
Tip: use --model <id> to switch.\n";
        let list = parse_cursor_models(out);
        assert_eq!(
            list,
            vec!["claude-opus-4-8-high", "gpt-5.5-extra-high", "composer-1"]
        );
        assert!(cursor_model_in_list("gpt-5.5-extra-high", &list));
        assert!(!cursor_model_in_list("gpt-5.5-xhigh", &list)); // 错误命名不在表
    }

    /// 管家模板**不许出现会话相关的段**。填空值比不填更糟:会让人以为"配了但没生效"。
    #[test]
    fn 管家模板不含会话相关配置() {
        let y = render_steward_yaml("commander", "管家", "A_ID", "A_SECRET", false);
        assert!(y.contains("role: steward"));
        assert!(y.contains("status: draft"));
        assert!(y.contains("A_ID") && y.contains("A_SECRET"));
        for forbidden in [
            "runtime:",
            "cognition:",
            "model:",
            "cli:",
            "tmux_name",
            "working_dir",
        ] {
            assert!(!y.contains(forbidden), "管家配置不该有 {forbidden}:\n{y}");
        }
        // 占位必须全部替换掉,别把 {{...}} 漏进真配置。
        assert!(!y.contains("{{"), "有未替换的占位:\n{y}");
    }

    /// 空名单对管家是 fail-closed,模板必须**说出来** —— 否则新建的管家看起来"建完就能用"。
    #[test]
    fn 管家模板提示空名单等于全部拒绝() {
        let y = render_steward_yaml("c", "c", "I", "S", true);
        assert!(y.contains("allowed_users: []"));
        assert!(y.contains("全部拒绝"), "必须写明空名单不是全放行:\n{y}");
    }

    #[test]
    fn render_agent_yaml_emits_effort_line_only_when_set() {
        let spec = AgentSpec {
            alias: "nara",
            display: "奈良",
            role: "测试",
            model: Some("claude-opus-4-8"),
            effort: Some("high"),
            cli: "claude",
            workspace: "~/ws",
            id_env: "FEISHU_NARA_APP_ID",
            secret_env: "FEISHU_NARA_APP_SECRET",
            active: true,
        };
        let with = render_agent_yaml(&spec);
        assert!(with.contains("effort: high"), "应渲染 effort 行: {with}");
        // 渲染产物可被解析
        let a: summon_core::registry::Agent = serde_yaml::from_str(&with).unwrap();
        assert_eq!(a.effort(), Some("high"));

        // effort 为 None → 整行不出现(避免残留空 effort: 行)
        let mut spec_none = spec;
        spec_none.effort = None;
        let without = render_agent_yaml(&spec_none);
        assert!(
            !without.contains("effort:"),
            "无 effort 时不应出现 effort 行: {without}"
        );
        // 仍是合法 YAML
        let a: summon_core::registry::Agent = serde_yaml::from_str(&without).unwrap();
        assert_eq!(a.effort(), None);
    }
    #[test]
    fn manifest_writers_share_the_nonblocking_configuration_lock() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("agent.yaml");
        let original = "name: test\nstatus: draft\nmodel: x\neffort: low\nruntime:\n  working_dir: old\nfeishu:\n  allowed_users: []\n";
        std::fs::write(&path, original).unwrap();
        let _guard = manifest_write_lock(&path).unwrap();
        let cases: Vec<ManifestRewriter> = vec![
            Box::new(flip_status_active),
            Box::new(|p| fill_empty_member_lists(p, "ou_new").map(drop)),
            Box::new(|p| set_model(p, "new")),
            Box::new(|p| set_effort(p, "high")),
            Box::new(|p| set_working_dir(p, "new")),
            Box::new(|p| write_if_absent_or_forced(p, || "name: overwritten\n".into(), true)),
        ];
        for writer in cases {
            assert!(writer(&path)
                .unwrap_err()
                .to_string()
                .contains("配置正在修改"));
        }
        assert_eq!(std::fs::read_to_string(path).unwrap(), original);
    }
}
