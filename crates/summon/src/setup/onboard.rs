//! `summon onboard` —— 一条龙交互向导(新手首选)。
//!
//! 把从零到一个能用的飞书 agent 的多段手工串成一条龙:
//! 0) 确保 lark-cli 与它的 skills 已安装 → ① 选 CLI 并一次查齐它的依赖 → ② 问身份 → ② 复用 bind 脚手架(draft) → ③ 绑定方式二选一(新建应用:开创建页 + 终端
//! 二维码 / 已有应用:直接输凭据)→ ④ 凭据验真(先于配权限——验真只换 token 不依赖 scope)→
//! ⑤ 带 app_id **直链**的控制台引导(权限页:JSON `pbcopy` + 每个 scope 的用途说明;事件订阅页:
//! 四个事件清单;发布) → ⑥ capture-one 抓第一条入站消息拿 open_id(跳过联调时改查飞书应用所有者)→ ⑦ 写进 allowed_users +
//! 翻 active → ⑧ 起/重启 daemon + 往该会话回执卡片(收发双向验通) → ⑨ 收尾提示。
//!
//! 控制台那段无法 API 化(建应用 / 导权限 / 开事件订阅),只做「直链 + 清单 + 等回车」;
//! 真正的校验交给收尾的实时闭环(漏订事件 / 缺 scope 会在 capture 或回执处暴露)。
//! 权限全集与用途见 `SCOPE_USAGE`(与 assets/feishu-scopes.json 一致性由测试钉死)。

use anyhow::{anyhow, bail, Context};
use serde::Deserialize;
use std::io::{BufRead, BufReader, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::channel;
use std::thread;
use std::time::Duration;
use summon_core::config::{Config, Messaging};

use crate::frontend::client::FeishuClient;
use crate::setup::bind;

/// 内置最小权限集(对齐飞书后台「批量导入/导出权限」schema)。编译期打进二进制。
const SCOPES_JSON: &str = include_str!("../../assets/feishu-scopes.json");

#[derive(clap::Args, Debug)]
pub struct OnboardArgs {
    #[arg(help = summon_i18n::text!("r42de09cf8c"))]
    /// 英文 alias(不给则向导问)
    pub alias: Option<String>,
    #[arg(help = summon_i18n::text!("r777e88ab43"))]
    /// 花名 / 显示名
    #[arg(long)]
    pub display: Option<String>,
    #[arg(help = summon_i18n::text!("rfdf27e4723"))]
    /// 一句话职责定位
    #[arg(long)]
    pub role: Option<String>,
    #[arg(help = summon_i18n::text!("r2803ecdce4"))]
    /// 飞书 app_id(不给则向导问)
    #[arg(long)]
    pub app_id: Option<String>,
    #[arg(help = summon_i18n::text!("r7d43049194"))]
    /// 飞书 app_secret(不给则向导问,从 stdin 读不进 history)
    #[arg(long)]
    pub app_secret: Option<String>,
    #[arg(help = summon_i18n::text!("r578919bc30"))]
    /// 干活目录;缺省 ~/.summon/agents/<alias>/workspace
    #[arg(long)]
    pub workspace: Option<String>,
    #[arg(help = summon_i18n::text!("r8b80373f89"))]
    /// 启动模型(写进 agent.yaml);不给则按选定 cli 取 messaging 模型候选桶首项
    /// (见 messaging.default.yaml 的 models_by_cli),桶空则不写 model 行、用 CLI 自己的默认
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
    #[arg(help = summon_i18n::text!("r0302c36dbd"), long_help = summon_i18n::text!("r38e2a93708"))]
    /// 建成**管家**身份(`role: steward`):只处理 `/fleet` 观测命令,不起 CLI 会话、
    /// 不碰终端会话层、不消耗模型额度。目录只落 `agent.yaml` + `.env` —— 不生成
    /// SOUL / memory / workspace,它不思考,没有自我认知要维护。
    ///
    /// 为什么不叫 `--role steward`:上面那个 `--role` 早就被「一句话职责定位」占了
    /// (它写进 agent.yaml 的职责注释),同名两义会让人以为能互相替代。
    #[arg(long)]
    pub steward: bool,
    #[arg(help = summon_i18n::text!("rf1c83d0fcb"))]
    /// 跳过凭据验真(离线 / 网络不通)
    #[arg(long)]
    pub no_validate: bool,
    #[arg(help = summon_i18n::text!("r37c915bf1e"))]
    /// 跳过实时联调闭环(白名单改写飞书应用所有者,查不到则留空 = 全放行;CI / 无人值守用)
    #[arg(long)]
    pub skip_capture: bool,
    #[arg(help = summon_i18n::text!("ra9bf8f9efd"))]
    /// capture 等待超时(秒)
    #[arg(long, default_value_t = 120)]
    pub capture_timeout: u64,
    #[arg(help = summon_i18n::text!("r01071c4bb1"))]
    /// 打开飞书控制台(macOS `open`)
    #[arg(long)]
    pub open_console: bool,
    #[arg(help = summon_i18n::text!("re223ab04c0"))]
    /// 非交互:能默认就默认、不再追问(capture 仍需人去飞书发消息)
    #[arg(long)]
    pub yes: bool,
}

pub fn run(cfg: &Config, args: OnboardArgs) -> anyhow::Result<()> {
    // 新手首选入口 = 初始化 ~/.summon:做成 git 仓库 + .gitignore 挡密钥(幂等、best-effort)。
    crate::setup::init::ensure();
    println!("{}", summon_i18n::format!("r9880e3db3c"));
    println!("{}", summon_i18n::format!("r8827112c8e"));
    println!("{}", summon_i18n::format!("raf21e407c6"));
    println!("{}", summon_i18n::format!("r8b37da047f"));
    ensure_lark_cli_installed(cfg, args.yes)?;
    persist_detected_tool_paths(cfg)?;

    // 1. 先选 CLI、立刻查它的依赖:缺什么一次列齐,不让人答完一串问题才发现装不了。codex/cursor
    // 无头 agent 不需要 claude 也不需要 tmux,只按选定的 CLI 查。
    let cli = pick_cli(&args)?;
    precheck_for_cli(&cli)?;

    // 2. 身份
    let alias = match args.alias.clone() {
        Some(a) => a,
        None => {
            println!("{}", summon_i18n::format!("rd0b6add3a6"));
            println!("{}", summon_i18n::format!("ra357a50bc4"));
            bind::read_line_stdin(summon_i18n::text!("r7439dcbeb0"))?
        }
    };
    bind::validate_alias(&alias)?;
    let display = opt_or_prompt(
        args.display.clone(),
        &summon_i18n::format!("rc9551123fb"),
        &alias,
        args.yes,
    )?;
    let role = opt_or_prompt(
        args.role.clone(),
        summon_i18n::text!("r82f08831e5"),
        summon_i18n::text!("rc227265794"),
        args.yes,
    )?;
    let (id_env, secret_env) = bind::env_var_names(&alias);
    let workspace = args
        .workspace
        .clone()
        .unwrap_or_else(|| format!("~/.summon/agents/{alias}/workspace"));

    // 2.5 模型 + effort 选择(交互;--yes 直接用 flag 值)。cursor 走 base→tier→thinking→fast
    // 组装 model 名,claude/codex 走「model 串 + 独立 effort 档位」。最终经 resolve_flags 校验。
    let (model, effort) = pick_model_effort(&args, &cli)?;

    // 3. 脚手架(先落 draft,联调通过后翻 active)
    let dir = cfg.agents_dir.join(&alias);
    let manifest_path = dir.join("agent.yaml");
    if manifest_path.is_file() {
        println!("{}", summon_i18n::format!("r29204f83ef", dir.display()));
    }
    scaffold(
        &dir,
        &alias,
        &display,
        &role,
        &cli,
        model.as_deref(),
        effort.as_deref(),
        &id_env,
        &secret_env,
        &workspace,
        args.steward,
    )?;
    if args.steward {
        println!("{}", summon_i18n::format!("r973cac3df7", dir.display()));
        // 空名单对管家是**全部拒绝**而不是全放行,不说清楚会让人以为"建完就能用"。
        println!("{}", summon_i18n::format!("r0de8e8f149"));
    } else {
        println!("{}", summon_i18n::format!("rdad15e5240", dir.display()));
    }

    // 4. 绑定方式二选一(新建应用 → 带去创建页 / 已有应用 → 直接输凭据),拿到凭据**先验真**——
    //    这样后面的控制台引导能用带 app_id 的直链页面(权限/事件订阅),不用翻菜单找。
    //    验真只换 tenant_access_token,不依赖任何 scope,放在配权限之前是安全的。
    let existing = existing_env_creds(&dir, &id_env, &secret_env);
    let (app_id, app_secret) = prompt_creds_with_path(&args, existing)?;
    // 配置体检:凭据验真(换 token,不依赖 scope)→ bot/v3/info 探测机器人能力是否启用。
    // 事件订阅配置**无自查询 API**,保留 loopback 实测;发布状态有无自查询 API 存疑(spec 标❓),
    // 探测失败统一按「能力未启用 / 未发布」提示。结果存 bot_ok 供 loopback 失败时分流。
    let bot_ok = if args.no_validate {
        None
    } else {
        let fc = FeishuClient::new(
            &cfg.default_domain,
            app_id.clone(),
            app_secret.clone(),
            false,
        );
        fc.check_auth().context(summon_i18n::text!("rba4769e1b3"))?;
        println!("{}", summon_i18n::format!("r1a16f4406a"));
        match fc.bot_v3_info() {
            Ok(info) => {
                println!("{}", summon_i18n::format!("r0ecd4a0e0c", info.app_name));
                Some(true)
            }
            Err(e) => {
                println!("{}", summon_i18n::format!("rff9359e41e"));
                println!("{}", summon_i18n::format!("r6318471ce3"));
                Some(false)
            }
        }
    };
    bind::write_env_file(
        &dir.join(".env"),
        &bind::render_env(&display, &alias, &id_env, &app_id, &secret_env, &app_secret),
    )?;
    bind::flip_status_active(&manifest_path)?; // 有凭据 → 可路由

    // 5. 控制台配置引导(带 app_id 直链):权限(用途清单 + JSON pbcopy)→ 事件订阅 → 发布
    let scopes_path = scopes_file(cfg, &alias)?;
    let copied = copy_to_clipboard(SCOPES_JSON);
    print_app_config_guide(&app_id, &scopes_path, copied);
    // --open-console 直接开;否则交互问一次(短路:--yes 不问)。
    let open_now =
        args.open_console || (!args.yes && ask_yes_no(summon_i18n::text!("ra185c3e033"))?);
    if open_now {
        open_config_pages(&app_id);
    }
    if !args.yes {
        let _ = bind::read_line_stdin(summon_i18n::text!("r35b7a6a1fa"));
    }

    // 6. 实时联调闭环:抓 open_id → 收紧白名单。跳过联调时改用飞书应用所有者收紧,
    //    结果与联调成功一致;验真都跳过了就不再联网。
    let app_owner = || -> anyhow::Result<Option<summon_core::config::registry::AppOwner>> {
        if args.no_validate {
            bail!(summon_i18n::format!("r6d3c5e5417"));
        }
        FeishuClient::new(
            &cfg.default_domain,
            app_id.clone(),
            app_secret.clone(),
            false,
        )
        .app_owner()
    };
    let mut confirm_chat: Option<String> = None;
    if args.skip_capture {
        println!("{}", summon_i18n::format!("rd96ec3111c"));
        print!(
            "{}",
            fill_lists_with_app_owner(&manifest_path, app_owner())?
        );
    } else {
        let bin = cfg
            .sidecar_bin
            .clone()
            .or_else(crate::runtime::daemon::default_sidecar_bin)
            .ok_or_else(|| {
                anyhow!(summon_i18n::format!(
                    "rf2b3354e6b",
                    summon_core::companion::SIDECAR,
                    ::summon_core::i18n::label(summon_core::companion::WHERE)
                ))
            })?;
        println!("{}", summon_i18n::format!("r62afaf5c20"));
        println!("{}", summon_i18n::format!("r567cd909ab"));
        println!("{}", summon_i18n::format!("r4bd17465d3"));
        // U5:capture 失败不再终止整个向导(此时 .env/脚手架都已就绪,重答一遍太亏)——
        // 循环里给「重试 / 跳过 / 退出」三选;--yes 失败直接降级成跳过。
        loop {
            println!(
                "{}",
                summon_i18n::format!("r3016e13d96", args.capture_timeout)
            );
            match capture_open_id(
                &bin,
                &app_id,
                &app_secret,
                &cfg.default_domain,
                Duration::from_secs(args.capture_timeout),
            ) {
                Ok(cap) => {
                    // 白名单优先写 union_id(同开发者下跨 app 稳定);抓不到才退回 open_id(per-app)。
                    // 这样换 app / 多 agent 复用同一身份时不会因 open_id 变了而点不动会话按钮。
                    let whitelist_id = if cap.union_id.is_empty() {
                        &cap.open_id
                    } else {
                        &cap.union_id
                    };
                    println!(
                        "{}",
                        summon_i18n::format!(
                            "re8815a6630",
                            cap.open_id,
                            if cap.union_id.is_empty() {
                                summon_i18n::text!("rf4b39d9b45")
                            } else {
                                &cap.union_id
                            },
                        )
                    );
                    // talk 名单与 operate 名单默认就是你;已有人工值的那张不动。
                    let filled = bind::fill_empty_member_lists(&manifest_path, whitelist_id)?;
                    if filled.is_empty() {
                        println!("{}", summon_i18n::format!("r9dad5064a5"));
                    } else {
                        println!(
                            "{}",
                            summon_i18n::format!("r0bac543afe", filled.join(" / "))
                        );
                    }
                    confirm_chat = Some(cap.chat_id);
                    break;
                }
                Err(e) => {
                    println!("{}", summon_i18n::format!("r6e9c60efac"));
                    println!("{}", loopback_failure_hint(bot_ok));
                    if args.yes {
                        println!("{}", summon_i18n::format!("rca060ec82c"));
                        print!(
                            "{}",
                            fill_lists_with_app_owner(&manifest_path, app_owner())?
                        );
                        break;
                    }
                    let c = bind::read_line_stdin(summon_i18n::text!("rfb52061d64"))?;
                    match c.trim().to_ascii_lowercase().as_str() {
                        "s" => {
                            println!("{}", summon_i18n::format!("r071c07d349"));
                            print!(
                                "{}",
                                fill_lists_with_app_owner(&manifest_path, app_owner())?
                            );
                            break;
                        }
                        "q" => bail!(summon_i18n::format!("rca0b2aa656")),
                        _ => continue,
                    }
                }
            }
        }
    }

    // 7. 让新 agent 上线;再往联调那个会话回执卡片。
    // daemon 已在跑 → 只 reload(扫盘接新 agent + 重启 sidecar)。**不再 restart daemon**:
    // 那会 SIGTERM 掉其它 agent 正在跑的无头轮,还丢掉全部内存态 —— 为了第 N 个 agent 上线
    // 打断另外 N-1 个正在干活的,agent 越多越荒谬。daemon 没在跑 → 直接 start。
    if crate::runtime::daemon::running_pid().is_some() {
        println!("{}", summon_i18n::format!("rf99e70da60"));
        crate::cli::cli::agents_reload()?;
    } else {
        println!("{}", summon_i18n::format!("r42f02ab10b"));
        crate::cli::cli::daemon_restart()?;
    }
    if let Some(chat) = confirm_chat {
        let fc = FeishuClient::new(
            &cfg.default_domain,
            app_id.clone(),
            app_secret.clone(),
            false,
        );
        let msg = summon_i18n::format!("rabde8dbbe8");
        match fc.send(&chat, &msg) {
            Ok(()) => println!("{}", summon_i18n::format!("r9a84fdf8ef")),
            Err(e) => println!("{}", summon_i18n::format!("r0ba2f38799")),
        }
    }

    if cfg!(target_os = "macos") {
        prompt_menubar_icon_once(args.yes);
    }

    print_summary(&alias, &display, &dir);
    Ok(())
}

/// 图标只在交互式 onboard 里问一次:`--yes` 的脚本化路径不问(Menu Bar 按默认图标画),选过的机器也不问。
fn should_ask_menubar_icon(yes: bool, already_set: bool) -> bool {
    !yes && !already_set
}

/// 第一次在这台 Mac 上 onboard 时选 Menu Bar 的 summon 图标(改主意直接改 `menubar.icon`,
/// Menu Bar 下次刷新生效)。编号只是向导里的展示顺序,config 存名字。图标是附加项:这一步
/// 出任何错都只提示,不让已经建好的 agent 以失败收场。
fn prompt_menubar_icon_once(yes: bool) {
    if !should_ask_menubar_icon(yes, Config::load().menubar.icon.is_some()) {
        return;
    }
    if let Err(error) = choose_menubar_icon() {
        println!("{}", summon_i18n::format!("ra1b8fbe8eb"));
    }
}

fn choose_menubar_icon() -> anyhow::Result<()> {
    use summon_core::menubar::{menubar_icon_for_choice, MENUBAR_ICONS};
    println!("{}", summon_i18n::format!("rbdc32f9135"));
    for (index, icon) in MENUBAR_ICONS.iter().enumerate() {
        let default = if index == 0 {
            summon_i18n::text!("r0ef211c6e3")
        } else {
            ""
        };
        println!(
            "  {}. {}{default} —— {}",
            index + 1,
            summon_core::i18n::label(icon.label),
            summon_core::i18n::label(icon.meaning)
        );
    }
    let icon = loop {
        let answer =
            bind::read_line_stdin(&summon_i18n::format!("r93be25ce7f", MENUBAR_ICONS.len()))?;
        match menubar_icon_for_choice(&answer) {
            Some(icon) => break icon,
            None => println!("{}", summon_i18n::format!("r710c050039", answer.trim())),
        }
    };
    write_menubar_icon(&summon_core::config::config_path(), icon.name)?;
    println!(
        "{}",
        summon_i18n::format!("r9bdc4bcd54", summon_core::i18n::label(icon.label))
    );
    Ok(())
}

/// 把选择写进 config.yaml,其余内容(含注释)原样保留。
fn write_menubar_icon(config_path: &Path, name: &str) -> anyhow::Result<()> {
    let raw = std::fs::read_to_string(config_path).unwrap_or_default();
    let updated = summon_core::menubar::patch_menubar_icon_yaml(&raw, name)
        .map_err(|error| anyhow!(error))?;
    crate::runtime::fsio::atomic_write(config_path, updated.as_bytes())
        .with_context(|| summon_i18n::format!("r3e61458926", config_path.display()))
}

/// agent 在会话里用 lark-cli 和它的 skills 操作飞书,onboard 保证它装好、能执行。agent 调它走的是
/// 各自机器人凭据建的 profile,所以这里不做 lark-cli 默认配置的应用初始化与用户授权 —— 那份
/// 只有 sentinel 用,开 sentinel 时再配(见 `setup::lark_cli`)。安装后重新找一遍二进制,避免
/// 安装器报成功、却找不到装在哪的假成功。
fn ensure_lark_cli_installed(cfg: &Config, yes: bool) -> anyhow::Result<()> {
    let npx = summon_core::tools::resolve_from(cfg, summon_core::tools::Tool::Npx);
    let configured = summon_core::tools::resolve_from(cfg, summon_core::tools::Tool::LarkCli);
    let mut found = locate_lark_cli(&configured, &npx);
    if found.is_none() {
        if let Some(prefix) = crate::setup::npm::global_prefix(&npx) {
            let prefix = if crate::setup::npm::prefix_writable(&prefix) {
                prefix
            } else {
                let home = dirs::home_dir().context(summon_i18n::text!("ra7f4451a27"))?;
                let shell = std::env::var_os("SHELL");
                move_npm_prefix_home(&npx, &prefix, &home, shell.as_deref(), || {
                    confirm_npm_prefix_move(yes, std::io::stdin().is_terminal(), || {
                        ask_yes_default_yes(summon_i18n::text!("rc0ba083dfa"))
                    })
                })?
            };
            // 刚改到家目录的前缀还是空的:npx 要先能 lstat 到 `<前缀>/lib`,否则在装之前就报
            // ENOENT。建出 `npm install -g` 本来就会建的那两个目录。
            crate::setup::npm::ensure_layout(&prefix)?;
        }
        println!("{}", summon_i18n::format!("r1d9482cedd"));
        // stdin 不给终端:安装器只装 lark-cli 与 skills,跳过它自带的应用初始化与授权两步。
        let status = Command::new(&npx)
            .args(["@larksuite/cli@latest", "install", "--lang", "zh"])
            .stdin(Stdio::null())
            .status()
            .with_context(|| summon_i18n::format!("ra7f00575b3", npx.display()))?;
        if !status.success() {
            bail!(summon_i18n::format!("reb405073af"));
        }
        println!("{}", summon_i18n::format!("r50d384bd6e"));
        found = locate_lark_cli(&configured, &npx);
    }
    let Some(lark_cli) = found else {
        bail!(summon_i18n::format!("r6e9c550b0a"));
    };
    if lark_cli != configured {
        let tool = summon_core::tools::Tool::LarkCli;
        let config_path = summon_core::config::config_path();
        if write_tool_paths(&config_path, &[(tool, lark_cli.clone())])? {
            println!(
                "{}",
                summon_i18n::format!("r25199f0b0a", lark_cli.display(), tool.key())
            );
        } else {
            println!(
                "{}",
                summon_i18n::format!(
                    "r77ca38f03a",
                    config_path.display(),
                    tool.key(),
                    lark_cli.display()
                )
            );
        }
    }
    println!(
        "{}",
        summon_i18n::format!("r558187e9b1", lark_cli.display())
    );
    Ok(())
}

/// 改 npm 全局目录时要补的说明:分发渠道可能在原目录里装着一份 summon(见 `setup::npm_dist`),
/// 改完之后提醒原目录里还剩什么。
#[cfg(npm_dist)]
use crate::setup::npm_dist::prefix_leftover;

#[cfg(not(npm_dist))]
fn prefix_leftover(_current: &Path) -> Option<String> {
    None
}

/// npm 全局目录对当前用户不可写时(系统自带的 node,目录属 root),官方安装器里的
/// `npm install -g` 必然失败。说清要改什么,同意就把 npm 全局目录改到家目录并让新开的 shell
/// 把它放进 PATH;不同意就列出其它做法后退出。返回改好的前缀。
fn move_npm_prefix_home(
    npx: &Path,
    current: &Path,
    home: &Path,
    shell: Option<&std::ffi::OsStr>,
    confirm: impl FnOnce() -> anyhow::Result<bool>,
) -> anyhow::Result<PathBuf> {
    let target = crate::setup::npm::user_prefix(home);
    let bin = target.join("bin");
    let rc = crate::setup::npm::shell_rc(home, shell);
    let leftover = prefix_leftover(current);
    let path_step = match &rc {
        Some(rc) => summon_i18n::format!("r13647f6d75", rc.display(), bin.display()),
        None => summon_i18n::format!("rf14906beb5", bin.display()),
    };
    println!(
        "{}",
        summon_i18n::format!(
            "rea5a5b260a",
            current = current.display(),
            target = target.display(),
        )
    );
    if !confirm()? {
        bail!("{}", unwritable_npm_prefix_guide(current));
    }
    std::fs::create_dir_all(&target)
        .with_context(|| summon_i18n::format!("rcb14990d57", target.display()))?;
    crate::setup::npm::set_global_prefix(npx, &target)?;
    let effective = crate::setup::npm::global_prefix(npx);
    if !effective
        .as_deref()
        .is_some_and(|dir| same_dir(dir, &target))
    {
        // 写进 ~/.npmrc 的那条撤掉:留着它,前缀一旦不再被盖住就悄悄生效,而 PATH 那一步没做。
        let undo = match crate::setup::npm::unset_global_prefix(npx) {
            Ok(()) => summon_i18n::text!("rfebcee84ec").to_string(),
            Err(error) => summon_i18n::format!("r7b893ac7fe"),
        };
        bail!(summon_i18n::format!(
            "r8aaa5f6835",
            target.display(),
            effective.map_or_else(
                || summon_i18n::text!("rc729c18aa0").to_string(),
                |dir| dir.display().to_string()
            )
        ));
    }
    // 前缀已经改好,rc 写不进去(只读、由 dotfiles 工具管理)也不中断:重跑时前缀已可写,不会再
    // 回到这一步,只能当场提示人自己加。
    match rc
        .as_deref()
        .map(|rc| (rc, crate::setup::npm::ensure_path_in_rc(rc, home, &bin)))
    {
        Some((rc, Ok(true))) => println!(
            "{}",
            summon_i18n::format!("r83271bd716", rc.display(), bin.display())
        ),
        Some((rc, Ok(false))) => println!(
            "{}",
            summon_i18n::format!("r2980890919", rc.display(), bin.display())
        ),
        Some((rc, Err(error))) => println!(
            "{}",
            summon_i18n::format!("r79ee665d3a", rc.display(), bin.display())
        ),
        None => println!("{}", summon_i18n::format!("r45aba76a15", bin.display())),
    }
    println!("{}", summon_i18n::format!("r4f581d8530", target.display()));
    if let Some(leftover) = &leftover {
        println!("  {leftover}");
    }
    println!();
    Ok(target)
}

/// 改 npm 全局目录要改用户的 dotfile:`--yes` 直接改;交互时问一句,回车即同意;stdin 不是终端
/// (管道、`</dev/null`)时没人能回答,不改。
fn confirm_npm_prefix_move(
    yes: bool,
    interactive: bool,
    ask: impl FnOnce() -> anyhow::Result<bool>,
) -> anyhow::Result<bool> {
    if yes {
        println!("{}", summon_i18n::format!("reb5693e272"));
        return Ok(true);
    }
    if !interactive {
        return Ok(false);
    }
    ask()
}

/// `npm prefix -g` 读回的路径可能经过符号链接解析,比较时两边都规范化。
fn same_dir(left: &Path, right: &Path) -> bool {
    let canonical = |path: &Path| path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    canonical(left) == canonical(right)
}

/// 不同意改 npm 全局目录时的说明:原因与其它做法,由人决定改哪里。
fn unwritable_npm_prefix_guide(prefix: &Path) -> String {
    summon_i18n::format!("r54984b6ff5", prefix = prefix.display())
}

/// 能执行的 lark-cli:先按配置 / PATH 找;找不到再看 npm 全局 bin。官方安装器装进
/// `$(npm prefix -g)/bin`,而这个目录不一定在 PATH 上。
fn locate_lark_cli(configured: &Path, npx: &Path) -> Option<PathBuf> {
    if runs(configured) {
        return Some(configured.to_path_buf());
    }
    let global = crate::setup::npm::global_bin(npx)?;
    crate::setup::npm::find_in_global_bin(&global, summon_core::tools::Tool::LarkCli.command())
        .filter(|path| runs(path))
}

fn runs(bin: &Path) -> bool {
    Command::new(bin)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// 把工具的绝对路径补进 config.yaml 的 `tools:`(只补缺项、不覆盖人工值),写了就让本进程立即
/// 用上。返回有没有写。
fn write_tool_paths(
    config_path: &Path,
    entries: &[(summon_core::tools::Tool, PathBuf)],
) -> anyhow::Result<bool> {
    let raw = match std::fs::read_to_string(config_path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => {
            return Err(error)
                .with_context(|| summon_i18n::format!("r6da9ed443d", config_path.display()))
        }
    };
    let updated = summon_core::tools::patch_tools_yaml(&raw, entries, false)
        .map_err(|error| anyhow!(error))?;
    if updated == raw {
        return Ok(false);
    }
    crate::runtime::fsio::atomic_write(config_path, updated.as_bytes())
        .with_context(|| summon_i18n::format!("r17be10aa4f", config_path.display()))?;
    summon_core::tools::configure(&Config::load());
    Ok(true)
}

/// onboard 首次运行时把已经能执行的依赖钉成绝对路径:先看当前干净 PATH,找不到再看 npm 全局
/// bin(claude / codex / lark-cli 的推荐装法都落在那里,而它常不在 daemon 与自启单元的 PATH 上)。
/// 只补缺项、不覆盖人工 `tools.*`;旧别名会被等值写入新表完成温和迁移,旧字段仍原样保留供
/// doctor 提示清理。
fn persist_detected_tool_paths(cfg: &Config) -> anyhow::Result<()> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    let npx = summon_core::tools::resolve_from(cfg, summon_core::tools::Tool::Npx);
    let detected = detected_tool_paths(cfg, &path, || crate::setup::npm::global_bin(&npx));
    if detected.is_empty() {
        return Ok(());
    }
    let config_path = summon_core::config::config_path();
    if write_tool_paths(&config_path, &detected)? {
        println!(
            "{}",
            summon_i18n::format!("r74581ec0ef", config_path.display())
        );
    }
    Ok(())
}

/// 没有人工 `tools.*` 的工具各自的可执行绝对路径:PATH 上没有就去 npm 全局 bin 找。npm 全局
/// bin 要起一次 npm 才知道,只在确有工具不在 PATH 上时才问。
fn detected_tool_paths(
    cfg: &Config,
    path: &std::ffi::OsStr,
    npm_global_bin: impl FnOnce() -> Option<PathBuf>,
) -> Vec<(summon_core::tools::Tool, PathBuf)> {
    let mut npm_global_bin = Some(npm_global_bin);
    let mut global_bin: Option<PathBuf> = None;
    let mut detected = Vec::new();
    for tool in summon_core::tools::Tool::ALL {
        let inspected = summon_core::tools::inspect(cfg, tool, path);
        if inspected.source == summon_core::tools::ToolSource::Tools {
            continue;
        }
        let found = inspected.actual.or_else(|| {
            if let Some(ask) = npm_global_bin.take() {
                global_bin = ask();
            }
            global_bin
                .as_deref()
                .and_then(|dir| crate::setup::npm::find_in_global_bin(dir, tool.command()))
        });
        if let Some(actual) = found {
            detected.push((tool, actual));
        }
    }
    detected
}

/// 装依赖用的包管理器。只用来给缺项配安装命令。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PackageManager {
    Brew,
    Apt,
    Dnf,
    Apk,
    Pacman,
    Unknown,
}

impl PackageManager {
    /// 本机该用哪个:macOS 用 Homebrew;Linux 按 /etc/os-release 的 ID / ID_LIKE 判。
    pub(crate) fn detect() -> Self {
        if cfg!(target_os = "macos") {
            return Self::Brew;
        }
        std::fs::read_to_string("/etc/os-release")
            .map(|text| Self::from_os_release(&text))
            .unwrap_or(Self::Unknown)
    }

    fn from_os_release(text: &str) -> Self {
        let ids: Vec<String> = text
            .lines()
            .filter_map(|line| {
                line.strip_prefix("ID=")
                    .or_else(|| line.strip_prefix("ID_LIKE="))
            })
            .flat_map(|value| {
                value
                    .trim_matches('"')
                    .split_whitespace()
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            })
            .collect();
        let has = |names: &[&str]| ids.iter().any(|id| names.contains(&id.as_str()));
        if has(&["debian", "ubuntu"]) {
            Self::Apt
        } else if has(&["fedora", "rhel", "centos"]) {
            Self::Dnf
        } else if has(&["alpine"]) {
            Self::Apk
        } else if has(&["arch"]) {
            Self::Pacman
        } else {
            Self::Unknown
        }
    }

    /// 用系统包管理器装一个包的命令。
    pub(crate) fn install(self, package: &str) -> String {
        match self {
            Self::Brew => format!("brew install {package}"),
            Self::Apt => format!("sudo apt-get install -y {package}"),
            Self::Dnf => format!("sudo dnf install -y {package}"),
            Self::Apk => format!("sudo apk add {package}"),
            Self::Pacman => format!("sudo pacman -S {package}"),
            Self::Unknown => summon_i18n::format!("rb76ada2973"),
        }
    }
}

/// 一个外部命令的安装方式。agent CLI 走各自官方的装法,系统工具走包管理器。
pub(crate) fn install_hint(tool: summon_core::tools::Tool, pm: PackageManager) -> String {
    use summon_core::tools::Tool;
    match tool {
        Tool::Claude => "npm install -g @anthropic-ai/claude-code".to_string(),
        Tool::Codex => "npm install -g @openai/codex".to_string(),
        Tool::CursorAgent => "curl https://cursor.com/install -fsS | bash".to_string(),
        Tool::LarkCli => "npx @larksuite/cli@latest install".to_string(),
        other => pm.install(other.command()),
    }
}

/// 选定 CLI 跑起来要的外部命令里,哪些执行不了:CLI 本身,以及 claude 要的 tmux(它跑在 tmux
/// 持久会话里)。只看能不能执行,不查任何登录态。
fn missing_dependencies(
    cli: &str,
    executable: impl Fn(summon_core::tools::Tool, &Path) -> bool,
) -> Vec<(summon_core::tools::Tool, PathBuf)> {
    use summon_core::tools::Tool;
    let driver = summon_core::driver::CliDriver::from_kind(cli);
    let mut needed: Vec<Tool> = Tool::for_driver(driver).into_iter().collect();
    if matches!(driver, summon_core::driver::CliDriver::Claude) {
        needed.push(Tool::Tmux);
    }
    needed
        .into_iter()
        .map(|tool| (tool, summon_core::tools::resolve(tool)))
        .filter(|(tool, path)| !executable(*tool, path))
        .collect()
}

fn render_missing_dependencies(
    cli: &str,
    missing: &[(summon_core::tools::Tool, PathBuf)],
    pm: PackageManager,
) -> String {
    let lines: Vec<String> = missing
        .iter()
        .map(|(tool, path)| {
            let why = if *tool == summon_core::tools::Tool::Tmux {
                summon_i18n::text!("r569f838964")
            } else {
                ""
            };
            summon_i18n::format!(
                "r0c44e83aa8",
                tool.command(),
                path.display(),
                install_hint(*tool, pm)
            )
        })
        .collect();
    summon_i18n::format!("r34f96da760", lines.join("\n"))
}

/// 前置自检:选完 CLI 立刻查它跑起来要的外部命令,一次列齐缺项和安装命令。
fn precheck_for_cli(cli: &str) -> anyhow::Result<()> {
    let missing = missing_dependencies(cli, |tool, bin| {
        let probe = if tool == summon_core::tools::Tool::Tmux {
            "-V"
        } else {
            "--version"
        };
        Command::new(bin)
            .arg(probe)
            .output()
            .is_ok_and(|output| output.status.success())
    });
    if !missing.is_empty() {
        bail!(
            "{}",
            render_missing_dependencies(cli, &missing, PackageManager::detect())
        );
    }
    Ok(())
}

/// 读已落盘的 `<dir>/.env` 里这对凭据(U5:重跑向导时回车复用,免得 alias 到 secret 全部重答;
/// 无文件 / 缺键 / 空值 → None)。
fn existing_env_creds(dir: &Path, id_env: &str, secret_env: &str) -> Option<(String, String)> {
    let s = std::fs::read_to_string(dir.join(".env")).ok()?;
    let mut id = None;
    let mut sec = None;
    for line in s.lines() {
        let line = line.trim().trim_start_matches("export ");
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let v = v.trim();
        if v.is_empty() {
            continue;
        }
        if k.trim() == id_env {
            id = Some(v.to_string());
        } else if k.trim() == secret_env {
            sec = Some(v.to_string());
        }
    }
    Some((id?, sec?))
}

/// 该 cli 是否有独立 effort 轴(= 走 `effort:` 字段)。cursor 把档位烤进 model 名 → 否。
/// 复用 core 的 `legal_efforts`(claude/codex 非空、cursor/Other 空),不在此另列 CLI 知识。
fn cli_takes_effort(cli: &str) -> bool {
    !summon_core::driver::CliDriver::from_kind(cli)
        .legal_efforts()
        .is_empty()
}

/// 非交互(flag)路径:补上按 cli 的默认 model → 校验 (model, effort) 在该 cli 上合法(fail-closed),
/// 返回写进 agent.yaml 的 `(model, effort)`。`model=None` = 不写 model 行,用 CLI 自己的默认。
/// cursor 不接受独立 effort(已在 validate 里 bail),claude/codex 原样带出。
/// 这是 bind/onboard 共用的纯决策点(无 stdin、不起进程),交互向导收集完输入后也走它做最终把关。
/// `upstream_default` 由调用方备好 —— 问 backend 要目录得起一个 app-server,那一步留在外面,
/// 这里才能被单测直接调。
fn resolve_flags(
    cli: &str,
    model: Option<&str>,
    effort: Option<&str>,
    messaging: &Messaging,
    upstream_default: impl FnOnce() -> Option<String>,
) -> anyhow::Result<(Option<String>, Option<String>)> {
    let model = bind::effective_model(model, messaging, cli, upstream_default);
    bind::validate_model_effort(cli, model.as_deref(), effort)?;
    Ok((model, effort.map(str::to_string)))
}

/// 选启动 CLI:--yes 用 flag 值,交互时列 claude/codex/cursor(默认 args.cli)。
fn pick_cli(args: &OnboardArgs) -> anyhow::Result<String> {
    if args.yes {
        return Ok(args.cli.clone());
    }
    opt_or_prompt(
        None,
        &summon_i18n::format!("r59abcb0a2c", args.cli),
        &args.cli,
        false,
    )
}

/// 为选定的 cli 交互选 model → effort,组装出写进 agent.yaml 的 `(model, effort)`。
/// --yes(非交互)直接用 flag 值并经 `resolve_flags` 校验。model 对 cursor 走
/// base→tier→thinking→fast 组装(`cursor_compose_model`)、claude/codex 直接输 model 串;effort 对
/// claude/codex 列 `legal_efforts`(默认留空=CLI 默认)、cursor 跳过(档位已在 model 名)。
/// 末尾统一 `resolve_flags` 把关;非交互非法直接 bail,交互非法回到 effort/model 重问。
fn pick_model_effort(
    args: &OnboardArgs,
    cli: &str,
) -> anyhow::Result<(Option<String>, Option<String>)> {
    let messaging = crate::runtime::daemon::resolve_messaging_global();
    if args.yes {
        let (model, effort) = resolve_flags(
            cli,
            args.model.as_deref(),
            args.effort.as_deref(),
            &messaging,
            || crate::codex::model_catalog::upstream_default_model(cli),
        )?;
        if cli == "cursor" {
            if let Some(m) = model.as_deref() {
                bind::warn_if_unknown_cursor_model(m);
            }
        }
        return Ok((model, effort));
    }
    let cli = cli.to_string();
    // 默认 model 必须**在选定 cli 之后**才算得出来:各 CLI 模型命名空间互不通用,拿 clap 的静态
    // 默认值当所有 cli 的默认,选 codex 时会把 claude 的 id 写进 agent.yaml。
    let default_model = bind::effective_model(args.model.as_deref(), &messaging, &cli, || {
        crate::codex::model_catalog::upstream_default_model(&cli)
    });
    loop {
        let (model, effort) = if cli == "cursor" {
            // cursor 无独立 effort 轴:走 base→tier→thinking→fast 组装 model 名。
            (pick_cursor_model(default_model.as_deref())?, None)
        } else {
            let model = prompt_model(&cli, default_model.as_deref(), &messaging)?;
            // claude/codex 有独立 effort 轴;Other 无(cli_takes_effort=false 时 pick_effort 返回 None)。
            let effort = if cli_takes_effort(&cli) {
                pick_effort(&cli, args.effort.as_deref())?
            } else {
                None
            };
            (model, effort)
        };
        match resolve_flags(
            &cli,
            model.as_deref(),
            effort.as_deref(),
            &messaging,
            || crate::codex::model_catalog::upstream_default_model(&cli),
        ) {
            Ok((m, e)) => {
                if cli == "cursor" {
                    if let Some(m) = m.as_deref() {
                        bind::warn_if_unknown_cursor_model(m);
                    }
                }
                return Ok((m, e));
            }
            Err(err) => println!("{}", summon_i18n::format!("r3f864cec50")),
        }
    }
}

/// claude/codex(及未知 cli)的 model 交互输入:提示里列出该 cli 的候选,回车 = 用默认。
/// 候选为空且无默认时,回车 = 不写 model 行(落到 CLI 自己的默认),而不是逼用户凭空编一个 id。
///
/// 候选优先用 backend 自报的目录:上一步算默认模型时已经问过一次,这里读同一份缓存,
/// 不额外起进程。拿不到才退回配置桶。
fn prompt_model(
    cli: &str,
    default_model: Option<&str>,
    messaging: &Messaging,
) -> anyhow::Result<Option<String>> {
    let dynamic = crate::codex::model_catalog::cached_or_probe(cli)
        .map(|catalog| catalog.models)
        .unwrap_or_default();
    let candidates: &[String] = if dynamic.is_empty() {
        messaging.models_for_cli(cli)
    } else {
        &dynamic
    };
    let listed = if candidates.is_empty() {
        summon_i18n::format!("raa2cefbeec")
    } else {
        summon_i18n::format!("r24c6f57baf", candidates.join(" / "))
    };
    let tail = match default_model {
        Some(d) => summon_i18n::format!("r14080870b4"),
        None => summon_i18n::text!("r647be2f463").to_string(),
    };
    let v = bind::read_line_stdin(&summon_i18n::format!("r19ae13ce76"))?;
    Ok(if v.is_empty() {
        default_model.map(str::to_string)
    } else {
        Some(v)
    })
}

/// claude/codex 的 effort 交互选:列该 cli 的合法档位,回车留空 = 不写 effort(用 CLI 默认)。
/// cursor/Other 无独立 effort 轴 → 直接返回 None(不该走到这,调用方已分流)。
fn pick_effort(cli: &str, default: Option<&str>) -> anyhow::Result<Option<String>> {
    let legal = summon_core::driver::CliDriver::from_kind(cli).legal_efforts();
    if legal.is_empty() {
        return Ok(None);
    }
    let dft = default.unwrap_or("");
    let prompt = summon_i18n::format!(
        "rc553bfcf39",
        legal.join("/"),
        if dft.is_empty() {
            String::new()
        } else {
            summon_i18n::format!("rc1abaafa06")
        }
    );
    let v = bind::read_line_stdin(&prompt)?;
    let v = if v.is_empty() { dft.to_string() } else { v };
    Ok(if v.is_empty() { None } else { Some(v) })
}

/// cursor 的 model 交互组装:base → tier → thinking? → fast?,经 `cursor_compose_model` 扁平成
/// cursor model id(运行期只验串,不再拼)。回车默认沿用 `default_base`(直接当成完整 model 用)。
fn pick_cursor_model(default_base: Option<&str>) -> anyhow::Result<Option<String>> {
    let tail = match default_base {
        Some(d) => summon_i18n::format!("r14080870b4"),
        None => summon_i18n::text!("r649734394a").to_string(),
    };
    let base = bind::read_line_stdin(&summon_i18n::format!("r9e3179bcb9"))?;
    let base = if base.is_empty() {
        default_base.map(str::to_string)
    } else {
        Some(base)
    };
    // 没 base 就没得拼:直接不写 model 行,别拿 tier 单独组一个非法串。
    let Some(base) = base else { return Ok(None) };
    // tier:cursor 烤进 model 名的档位词(low/medium/high/xhigh/max);回车留空=不挂档位。
    let tier = bind::read_line_stdin(summon_i18n::text!("r0f4627fa95"))?;
    let tier = if tier.is_empty() { None } else { Some(tier) };
    let thinking = ask_yes_no(summon_i18n::text!("rfe833dab4d"))?;
    let fast = ask_yes_no(summon_i18n::text!("r8802ed64d0"))?;
    Ok(Some(summon_core::driver::CliDriver::cursor_compose_model(
        &base,
        tier.as_deref(),
        thinking,
        fast,
    )))
}

/// 一个简单的 y/N 提问,默认 N。
fn ask_yes_no(prompt: &str) -> anyhow::Result<bool> {
    let v = bind::read_line_stdin(prompt)?;
    Ok(matches!(
        v.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

/// Y/n 提问,回车即同意。
fn ask_yes_default_yes(prompt: &str) -> anyhow::Result<bool> {
    let v = bind::read_line_stdin(prompt)?;
    Ok(!matches!(
        v.trim().to_ascii_lowercase().as_str(),
        "n" | "no"
    ))
}

/// loopback 抓消息失败时的分流提示。bot/v3/info 体检结果把「三条一锅端」收窄成两支:
///
/// - 体检失败(`Some(false)`)→ 机器人能力未启用/未发布,事件链路还轮不到;
/// - 体检通过(`Some(true)`)/ 跳过验真(`None`)→ 事件订阅(长连接 + im.message.receive_v1)
///   或发布问题。
///
/// 权限(scope)缺失没有自查询 API,留在事件链路这支里一起提示。
fn loopback_failure_hint(bot_ok: Option<bool>) -> &'static str {
    match bot_ok {
        Some(false) => summon_i18n::text!("r9c889ce782"),
        Some(true) | None => summon_i18n::text!("r08890a5be6"),
    }
}

/// 有值用值,否则交互问(--yes 时直接用 default)。
fn opt_or_prompt(
    given: Option<String>,
    prompt: &str,
    default: &str,
    yes: bool,
) -> anyhow::Result<String> {
    if let Some(v) = given {
        if !v.is_empty() {
            return Ok(v);
        }
    }
    if yes {
        return Ok(default.to_string());
    }
    let v = bind::read_line_stdin(prompt)?;
    Ok(if v.is_empty() { default.to_string() } else { v })
}

fn prompt_creds(
    args: &OnboardArgs,
    existing: Option<(String, String)>,
) -> anyhow::Result<(String, String)> {
    let will_prompt = args.app_id.as_deref().unwrap_or("").is_empty()
        || args.app_secret.as_deref().unwrap_or("").is_empty();
    // U5:重跑场景——.env 已有(多半验真过的)凭据,回车直接复用,不必全部重答。
    if will_prompt {
        if let Some((id, sec)) = existing {
            let ans = bind::read_line_stdin(&summon_i18n::format!("r1207ecbc07"))?;
            if !ans.trim().eq_ignore_ascii_case("n") {
                return Ok((id, sec));
            }
        }
    }
    if will_prompt {
        println!("{}", summon_i18n::format!("rf4c5141f69"));
    }
    let app_id = match &args.app_id {
        Some(s) if !s.is_empty() => s.clone(),
        _ => bind::read_line_stdin("App ID(cli_...): ")?,
    };
    if app_id.is_empty() {
        bail!(summon_i18n::format!("r6840341bab"));
    }
    let app_secret = match &args.app_secret {
        Some(s) if !s.is_empty() => s.clone(),
        _ => bind::read_secret_stdin(summon_i18n::text!("r6d4c26650c"))?,
    };
    if app_secret.is_empty() {
        bail!(summon_i18n::format!("r3bcaeea89a"));
    }
    Ok((app_id, app_secret))
}

#[allow(clippy::too_many_arguments)] // 模板占位逐个具名,合并成结构体只是换个地方列
fn scaffold(
    dir: &Path,
    alias: &str,
    display: &str,
    role: &str,
    cli: &str,
    model: Option<&str>,
    effort: Option<&str>,
    id_env: &str,
    secret_env: &str,
    workspace: &str,
    steward: bool,
) -> anyhow::Result<()> {
    // 管家:只落 agent.yaml。**不建 memory/workspace、不写 SOUL** —— 它不起会话也不思考,
    // 那些目录永远不会被读,留下空壳只会让下一个看到的人以为这是个会思考的身份。
    if steward {
        std::fs::create_dir_all(dir)?;
        return bind::write_if_absent_or_forced(
            &dir.join("agent.yaml"),
            || bind::render_steward_yaml(alias, display, id_env, secret_env, false),
            false,
        );
    }
    std::fs::create_dir_all(dir.join("memory/history"))?;
    std::fs::create_dir_all(summon_core::registry::expand_tilde(workspace))?;
    bind::write_if_absent_or_forced(
        &dir.join("SOUL.md"),
        || bind::render_soul(alias, display, role),
        false,
    )?;
    bind::write_if_absent_or_forced(
        &dir.join("agent.yaml"),
        || {
            bind::render_agent_yaml(&bind::AgentSpec {
                alias,
                display,
                role,
                model,
                effort,
                cli,
                workspace,
                id_env,
                secret_env,
                active: false, // 先 draft,联调通过后 flip
            })
        },
        false,
    )?;
    bind::write_if_absent_or_forced(
        &dir.join("memory/MEMORY.md"),
        || bind::render_memory(display, alias),
        false,
    )?;
    Ok(())
}

/// 把权限 JSON 落到 ~/.summon/<alias>-feishu-scopes.json,返回路径。
fn scopes_file(cfg: &Config, alias: &str) -> anyhow::Result<PathBuf> {
    let base = cfg
        .agents_dir
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| cfg.agents_dir.clone());
    std::fs::create_dir_all(&base)?;
    let p = base.join(format!("{alias}-feishu-scopes.json"));
    std::fs::write(&p, SCOPES_JSON)?;
    Ok(p)
}

/// 复制到剪贴板(macOS `pbcopy`)。成功返回 true;无 pbcopy / 失败返回 false。
fn copy_to_clipboard(text: &str) -> bool {
    if !cfg!(target_os = "macos") {
        return false;
    }
    let child = Command::new(summon_core::tools::resolve(
        summon_core::tools::Tool::Pbcopy,
    ))
    .stdin(Stdio::piped())
    .spawn();
    let mut child = match child {
        Ok(c) => c,
        Err(_) => return false,
    };
    if let Some(mut sin) = child.stdin.take() {
        if sin.write_all(text.as_bytes()).is_err() {
            return false;
        }
    }
    child.wait().map(|s| s.success()).unwrap_or(false)
}

/// 跳过联调时用飞书应用所有者收紧两份名单,返回给人看的提示;查不到所有者时名单不动(留空 = 全放行)。
fn fill_lists_with_app_owner(
    manifest_path: &Path,
    owner: anyhow::Result<Option<summon_core::config::registry::AppOwner>>,
) -> anyhow::Result<String> {
    let owner = match owner {
        Ok(Some(owner)) => owner,
        Ok(None) => return Ok(summon_i18n::text!("re7185477a1").into()),
        Err(e) => return Ok(summon_i18n::format!("r4bd2c71fe0")),
    };
    // 与联调同规矩:union_id 跨 app 稳定,优先写它。
    let id = if owner.union_id.is_empty() {
        &owner.open_id
    } else {
        &owner.union_id
    };
    let filled = bind::fill_empty_member_lists(manifest_path, id)?;
    Ok(match filled.as_slice() {
        [] => summon_i18n::text!("r9dad5064a5").into(),
        [_] => summon_i18n::format!("rb99ac790cc"),
        _ => summon_i18n::format!("r90a0346b33", filled.join(" / ")),
    })
}

/// scope → 用途说明(给人看的「为什么要申请」)。与 assets/feishu-scopes.json **一一对应**,
/// 加/删权限时两处同步(`scope_usage_matches_scopes_json` 测试钉死一致性)。
const SCOPE_USAGE: &[(&str, &str)] = &[
    ("application:application:self_manage", "r6db6be8439"),
    ("application:bot.basic_info:read", "r7166b72f87"),
    ("application:bot.menu:write", "r8102cf6580"),
    ("cardkit:card:read", "r71d2b3120b"),
    ("cardkit:card:write", "r37881456dd"),
    ("contact:contact.base:readonly", "r09b0c6c8eb"),
    ("docs:document.comment:create", "r0f3b46d4a5"),
    ("docs:document.comment:delete", "r7a75e3cde9"),
    ("docs:document.comment:read", "r07382b3b4b"),
    ("docs:document.comment:update", "ra67fb13d98"),
    ("docs:document.comment:write_only", "reb516a3566"),
    ("docx:document.block:convert", "r75071cdcd6"),
    ("docx:document:create", "ra6ef6289c5"),
    ("docx:document:readonly", "r43680f9e3b"),
    ("docx:document:write_only", "rc601b292d2"),
    ("drive:drive.metadata:readonly", "rbb46f864d7"),
    ("im:chat.members:bot_access", "r79964af74b"),
    ("im:chat:create", "r675ee6eef7"),
    ("im:chat:read", "rdbe5e5d2ce"),
    ("im:chat:update", "r1c78f9ed5e"),
    (
        "im:message.group_at_msg.include_bot:readonly",
        "r775f7d718e",
    ),
    ("im:message.group_at_msg:readonly", "re30c4bb46e"),
    ("im:message.p2p_msg:readonly", "r12ec93fa0a"),
    ("im:message.pins:read", "r54cf69b03b"),
    ("im:message.pins:write_only", "r9b58ba4e53"),
    ("im:message.reactions:read", "r8a0f0c53f3"),
    ("im:message.reactions:write_only", "r0c3351f6f9"),
    ("im:message:readonly", "rc4cec04493"),
    ("im:message:send_as_bot", "r347500a6ab"),
    ("im:message:send_multi_users", "r613daf8438"),
    ("im:message:send_sys_msg", "r96e1e26776"),
    ("im:message:update", "r144c62e00b"),
    ("im:resource", "r279955d69a"),
    ("wiki:node:read", "r3107a55d86"),
    ("offline_access", "rfe48630fc0"),
];

/// 绑定方式二选一:还没有应用 → 带去创建(浏览器 + 终端二维码);已有 → 直接输凭据。
/// flag 已给全凭据或 --yes 时不问,直接走 `prompt_creds`(保持脚本化路径不变)。
fn prompt_creds_with_path(
    args: &OnboardArgs,
    existing: Option<(String, String)>,
) -> anyhow::Result<(String, String)> {
    let flags_complete = args.app_id.as_deref().is_some_and(|s| !s.is_empty())
        && args.app_secret.as_deref().is_some_and(|s| !s.is_empty());
    if flags_complete || args.yes {
        return prompt_creds(args, existing);
    }
    // 重跑场景(.env 已有凭据):不再推「创建应用」引导,直接进复用/重输流程。
    if existing.is_some() {
        return prompt_creds(args, existing);
    }
    println!("{}", summon_i18n::format!("r021d498042"));
    println!("{}", summon_i18n::format!("r0a6ce6e559"));
    println!("{}", summon_i18n::format!("ra90d94a7ab"));
    let pick = bind::read_line_stdin(summon_i18n::text!("r18e67759f2"))?;
    if pick.trim() != "2" {
        guide_create_app();
    }
    prompt_creds(args, None)
}

/// 「创建新应用」分支:打开创建页 + 终端二维码,三步说明,建好直接回来输凭据。
fn guide_create_app() {
    println!("{}", summon_i18n::format!("r04303b1f22"));
    println!("{}", summon_i18n::format!("r6390d2511f"));
    println!("     {}", bind::CREATE_APP_URL);
    println!("{}", summon_i18n::format!("r4348a00a82"));
    println!("{}", summon_i18n::format!("r62332321bc"));
    open_url(bind::CREATE_APP_URL);
    print_qr(bind::CREATE_APP_URL);
}

/// 尽力用本机浏览器打开 URL。失败不阻断向导，因为调用方已经把 URL 打印到终端；但必须明确
/// 告知降级，不能像旧实现那样在非 macOS 或命令失效时静默吞掉。
pub(crate) fn open_url(url: &str) {
    let tool = if cfg!(target_os = "macos") {
        Some(summon_core::tools::Tool::Open)
    } else if cfg!(target_os = "linux") {
        Some(summon_core::tools::Tool::XdgOpen)
    } else {
        None
    };
    let Some(tool) = tool else {
        eprintln!("{}", summon_i18n::format!("r12023713d7"));
        return;
    };
    let bin = summon_core::tools::resolve(tool);
    match Command::new(&bin).arg(url).status() {
        Ok(status) if status.success() => {}
        Ok(status) => eprintln!("{}", summon_i18n::format!("r54b844d353", bin.display())),
        Err(error) => eprintln!("{}", summon_i18n::format!("r545d8cf689", bin.display())),
    }
}

/// 终端二维码(手机扫码跳转)。黑底终端居多,用反色渲染(dark→亮块)才可扫;
/// 生成失败静默跳过——上面始终有可点的 URL,二维码只是锦上添花。
fn print_qr(url: &str) {
    use qrcode::render::unicode;
    let Ok(code) = qrcode::QrCode::new(url) else {
        return;
    };
    let s = code
        .render::<unicode::Dense1x2>()
        .dark_color(unicode::Dense1x2::Light)
        .light_color(unicode::Dense1x2::Dark)
        .build();
    println!("{s}");
}

/// 凭据验真后的控制台配置引导:全部用带 app_id 的**直链**(不必在后台翻菜单找页面)。
/// auth/event 的 path 与当前飞书后台一致;万一后台改版直链失效,会落到应用首页,按左侧
/// 菜单同名页找即可(文案已写)。
fn print_app_config_guide(app_id: &str, scopes_path: &Path, copied: bool) {
    let base = format!("{}/{app_id}", bind::CONSOLE_URL);
    println!("{}", summon_i18n::format!("rfec7223f92"));
    println!("{}", summon_i18n::format!("r84eec4a542"));
    println!("{}", summon_i18n::format!("rb18c68b3d0"));
    if copied {
        println!(
            "{}",
            summon_i18n::format!("rd73d8c6394", scopes_path.display())
        );
    } else {
        // 没有剪贴板可写(Linux、ssh 上):直接把 JSON 印出来,终端里选中复制即可。
        println!(
            "{}",
            summon_i18n::format!("r5b5d6a641d", scopes_path.display())
        );
        println!("{}", SCOPES_JSON.trim_end());
    }
    println!("{}", summon_i18n::format!("r71904867eb"));
    for (scope, why) in SCOPE_USAGE {
        let why = summon_core::i18n::label(why);
        println!("       • {scope:<36} {why}");
    }
    println!("{}", summon_i18n::format!("r0de1cf998d"));
    println!("{}", summon_i18n::format!("rfc1d87480a"));
    println!("{}", summon_i18n::format!("r9f3353505f"));
    println!("{}", summon_i18n::format!("r1e7783ab23"));
    println!("{}", summon_i18n::format!("r6cb25dba77"));
    println!("{}", summon_i18n::format!("rd33da88f35"));
    println!("{}", summon_i18n::format!("r1011ea54d3"));
    println!("{}", summon_i18n::format!("raa35f6f000"));
    println!("{}", summon_i18n::format!("re42cc12a1a"));
    println!("{}", summon_i18n::format!("r5f35724040"));
    println!("{}", summon_i18n::format!("re71aef4a4d"));
    println!("{}", summon_i18n::format!("r7c1b2c5fcb"));
}

/// 用浏览器打开「权限管理」「事件订阅」两个直链页；失败时明确提示手动打开。
fn open_config_pages(app_id: &str) {
    let base = format!("{}/{app_id}", bind::CONSOLE_URL);
    for page in ["auth", "event"] {
        open_url(&format!("{base}/{page}"));
    }
}

fn print_summary(alias: &str, display: &str, dir: &Path) {
    println!("{}", summon_i18n::format!("r2a216fb1c8"));
    println!("{}", summon_i18n::format!("r00becb9128", dir.display()));
    println!("{}", summon_i18n::format!("r6e4ddc16bb"));
    println!("{}", summon_i18n::format!("r3b28348e39", dir.display()));
    println!("{}", summon_i18n::format!("r7b686700e8", dir.display()));
    println!("{}", summon_i18n::format!("r831b8ee4ea"));
    // 已经开了,或这台机器装不了(没有在跑的 systemd)就不提。
    if !crate::setup::autostart::status().0.enabled
        && crate::setup::autostart::unavailable_reason().is_none()
    {
        println!("{}", autostart_hint(cfg!(target_os = "linux")));
    }
    // 交接块:JIT 知识交付 —— 不往 agent 目录写常驻文件,用户把这段发给 agent 的那一刻
    // 它才知道「summon 已就绪 + 去哪学」。见 docs/spec/2026-08-20-oss-onboarding-experience-design.md。
    println!();
    println!("{}", handoff_block(display));
}

/// 收尾时的开机自启提示。Linux 上 daemon 归 systemd --user 托管,还要开 linger:不开的话最后一个
/// 登录会话(比如 ssh)一断,user 实例连同 daemon 和全部 tmux 会话一起停。
fn autostart_hint(linux: bool) -> String {
    if linux {
        summon_i18n::text!("r45c63541c8").to_string()
    } else {
        summon_i18n::text!("rfeddbd11c5").to_string()
    }
}

/// 交接块文本(paste-ready,用户发给自己的 agent)。抽成函数便于单测钉住关键要素。
pub(crate) fn handoff_block(display: &str) -> String {
    summon_i18n::format!("r8a11de3398")
}

/// capture-one 打到 stdout 的一行 JSON。
#[derive(Deserialize)]
pub(crate) struct Captured {
    pub open_id: String,
    /// 同开发者下跨 app 稳定;白名单优先写它(空则退回 open_id)。
    #[serde(default)]
    pub union_id: String,
    #[serde(default)]
    pub chat_id: String,
    #[serde(default)]
    #[allow(dead_code)]
    pub text: String,
}

/// 从 sidecar capture-one 的一行解析 open_id。非 JSON(如 SDK 日志)或空 open_id → None。
pub(crate) fn parse_capture_line(line: &str) -> Option<Captured> {
    let c: Captured = serde_json::from_str(line.trim()).ok()?;
    if c.open_id.is_empty() {
        None
    } else {
        Some(c)
    }
}

/// 起 sidecar `-capture-one`,等第一条入站消息拿 open_id/chat_id;超时由本函数 kill 控制。
fn capture_open_id(
    bin: &Path,
    app_id: &str,
    app_secret: &str,
    domain: &str,
    timeout: Duration,
) -> anyhow::Result<Captured> {
    use std::os::unix::process::CommandExt;
    let cfg_json = serde_json::json!({
        "events_sock": "",
        "apps": [{ "app_id": app_id, "app_secret": app_secret, "domain": domain }]
    })
    .to_string();

    let mut child = Command::new(bin)
        .arg0("summon-sidecar")
        .env("SIDECAR_CAPTURE_ONE", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null()) // SDK 日志走 stderr,丢掉别干扰
        .spawn()
        .context(summon_i18n::text!("rda0643a6fe"))?;

    // 保持 stdin 打开(sin 不提前 drop):sidecar 死随父,EOF 会让它自杀。
    let mut sin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow!(summon_i18n::format!("r94823c8c82")))?;
    sin.write_all(cfg_json.as_bytes())?;
    sin.write_all(b"\n")?;
    sin.flush()?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!(summon_i18n::format!("r8283722866")))?;
    let (tx, rx) = channel::<Captured>();
    let reader = thread::spawn(move || {
        let r = BufReader::new(stdout);
        for line in r.lines().map_while(Result::ok) {
            if let Some(c) = parse_capture_line(&line) {
                let _ = tx.send(c);
                return;
            }
        }
    });

    let res = rx.recv_timeout(timeout);
    let _ = child.kill();
    let _ = child.wait();
    drop(sin);
    let _ = reader.join();
    res.map_err(|_| anyhow!(summon_i18n::format!("re36d1b0ac9", timeout.as_secs())))
}

#[cfg(test)]
mod tests {
    /// 跳过联调时用飞书应用所有者收紧名单:写进两张空表、优先 union_id;查不到或已有人工值都不动。
    #[test]
    fn skipped_capture_fills_member_lists_with_app_owner() {
        use summon_core::config::registry::AppOwner;
        const SCAFFOLD: &str = "name: x\nfeishu:\n  app_id_env: A\n  allowed_users: []   # talk 名单\n  operators: []       # operate 名单\n";
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent.yaml");
        let owner = |open_id: &str, union_id: &str| {
            Ok(Some(AppOwner {
                open_id: open_id.into(),
                union_id: union_id.into(),
            }))
        };

        std::fs::write(&path, SCAFFOLD).unwrap();
        let note = super::fill_lists_with_app_owner(&path, owner("ou_owner", "on_owner")).unwrap();
        assert!(
            note.contains("on_owner") && note.contains("只有 TA 能对话和操作"),
            "{note}"
        );
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(
            written.contains("allowed_users: [\"on_owner\"]"),
            "{written}"
        );
        assert!(written.contains("operators: [\"on_owner\"]"), "{written}");

        std::fs::write(&path, SCAFFOLD).unwrap();
        super::fill_lists_with_app_owner(&path, owner("ou_owner", "")).unwrap();
        assert!(std::fs::read_to_string(&path)
            .unwrap()
            .contains("allowed_users: [\"ou_owner\"]"));

        for (result, hint) in [
            (Ok(None), "不是企业成员"),
            (Err(anyhow!("access denied")), "access denied"),
        ] {
            std::fs::write(&path, SCAFFOLD).unwrap();
            let note = super::fill_lists_with_app_owner(&path, result).unwrap();
            assert!(
                note.contains(hint) && note.contains("留空(=全放行)"),
                "{note}"
            );
            assert_eq!(std::fs::read_to_string(&path).unwrap(), SCAFFOLD);
        }

        let manual =
            "name: x\nfeishu:\n  app_id_env: A\n  allowed_users: [on_mate]\n  operators: []\n";
        std::fs::write(&path, manual).unwrap();
        let note = super::fill_lists_with_app_owner(&path, owner("ou_owner", "on_owner")).unwrap();
        assert!(note.contains("已有人工值"), "{note}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), manual);
    }

    /// 官方安装向导把 lark-cli 装进 `$(npm prefix -g)/bin`,这个目录不在 PATH 上时(Linux 常见的
    /// `~/.npm-global`)也要找得到;npm 取与 npx 同目录的那一个。全局 bin 里也没有才算找不到。
    #[test]
    fn lark_cli_in_an_npm_global_bin_off_path_is_found() {
        let dir = tempfile::tempdir().unwrap();
        let node_bin = dir.path().join("node/bin");
        let prefix = dir.path().join("npm-global");
        std::fs::create_dir_all(&node_bin).unwrap();
        std::fs::create_dir_all(prefix.join("bin")).unwrap();
        let script = |path: &std::path::Path, body: &str| {
            crate::test_support::write_test_executable(path, &format!("#!/bin/sh\n{body}\n"));
        };
        // 前缀从脚本自己的位置推(`<dir>/node/bin/npm` → `<dir>/npm-global`)。
        script(
            &node_bin.join("npm"),
            "d=${0%/*}; [ \"$1 $2\" = 'prefix -g' ] && echo \"${d%/node/bin}/npm-global\"",
        );
        let lark_cli = prefix.join("bin/lark-cli");
        script(&lark_cli, "[ \"$1\" = auth ] && echo '{}'; exit 0");
        let missing = dir.path().join("not-on-path/lark-cli");
        let npx = node_bin.join("npx");

        let found = super::locate_lark_cli(&missing, &npx).expect("应在 npm 全局 bin 里找到");
        assert_eq!(found, lark_cli);

        std::fs::remove_file(&lark_cli).unwrap();
        assert!(super::locate_lark_cli(&missing, &npx).is_none());
    }

    /// claude / codex / lark-cli 装在 npm 全局 bin、不在 PATH 上时,也要探测到并钉成绝对路径;
    /// 人工配过的不动。
    #[test]
    fn tools_off_path_are_found_in_the_npm_global_bin() {
        use std::os::unix::fs::PermissionsExt;
        use summon_core::tools::Tool;
        let dir = tempfile::tempdir().unwrap();
        let global = dir.path().join("npm-global/bin");
        std::fs::create_dir_all(&global).unwrap();
        for name in ["claude", "lark-cli", "codex"] {
            let bin = global.join(name);
            std::fs::write(&bin, "#!/bin/sh\n").unwrap();
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let mut cfg = summon_core::config::Config::default();
        cfg.tools.codex = Some(std::path::PathBuf::from("/opt/manual/codex"));
        let empty_path = std::ffi::OsString::new();
        let detected = super::detected_tool_paths(&cfg, &empty_path, || Some(global.clone()));
        assert!(
            detected.contains(&(Tool::Claude, global.join("claude"))),
            "{detected:?}"
        );
        assert!(
            detected.contains(&(Tool::LarkCli, global.join("lark-cli"))),
            "{detected:?}"
        );
        assert!(
            !detected.iter().any(|(tool, _)| *tool == Tool::Codex),
            "人工值不动"
        );
    }

    /// 选定 CLI 后一次列齐缺的依赖:claude 要 CLI 本身和 tmux,codex 只要它自己;安装命令按
    /// 包管理器给,不查任何登录态。
    #[test]
    fn missing_dependencies_are_listed_together_with_install_commands() {
        use summon_core::tools::Tool;
        let none = |_: Tool, _: &std::path::Path| false;
        let claude: Vec<Tool> = super::missing_dependencies("claude", none)
            .into_iter()
            .map(|(tool, _)| tool)
            .collect();
        assert_eq!(claude, [Tool::Claude, Tool::Tmux]);
        let codex: Vec<Tool> = super::missing_dependencies("codex", none)
            .into_iter()
            .map(|(tool, _)| tool)
            .collect();
        assert_eq!(codex, [Tool::Codex]);
        assert!(super::missing_dependencies("claude", |_, _| true).is_empty());

        let text = super::render_missing_dependencies(
            "claude",
            &super::missing_dependencies("claude", none),
            super::PackageManager::Apt,
        );
        assert!(
            text.contains("npm install -g @anthropic-ai/claude-code"),
            "{text}"
        );
        assert!(text.contains("sudo apt-get install -y tmux"), "{text}");
        assert!(!text.contains("login") && !text.contains("登录"), "{text}");
    }

    #[test]
    fn the_package_manager_follows_os_release() {
        use super::PackageManager;
        assert_eq!(
            PackageManager::from_os_release("ID=debian\nVERSION_ID=\"12\"\n"),
            PackageManager::Apt
        );
        assert_eq!(
            PackageManager::from_os_release("ID=ubuntu\nID_LIKE=debian\n"),
            PackageManager::Apt
        );
        assert_eq!(
            PackageManager::from_os_release("ID=\"rocky\"\nID_LIKE=\"rhel centos fedora\"\n"),
            PackageManager::Dnf
        );
        assert_eq!(
            PackageManager::from_os_release("ID=alpine\n"),
            PackageManager::Apk
        );
        assert_eq!(
            PackageManager::from_os_release("ID=nixos\n"),
            PackageManager::Unknown
        );
        assert_eq!(PackageManager::Brew.install("tmux"), "brew install tmux");
    }

    /// lark-cli 官方安装器不接终端:只装 lark-cli 与 skills,不触发它自带的应用初始化与授权;
    /// 应用初始化与授权只在开启 sentinel 时做。npm 全局目录不可写时在跑安装器之前先处理前缀。
    #[test]
    fn onboard_installs_lark_cli_without_creating_an_app() {
        let src = include_str!("onboard.rs");
        let body = &src[src.find("fn ensure_lark_cli_installed(").unwrap()..];
        let body = &body[..body.find("\n}\n").unwrap()];
        let squashed: String = body.split_whitespace().collect();
        assert!(
            squashed
                .contains("letprefix=ifcrate::setup::npm::prefix_writable(&prefix){prefix}else{"),
            "前缀不可写就先改前缀,不跑必然失败的安装器: {body}"
        );
        let writable = body.find("prefix_writable(&prefix)").expect("先判前缀可写");
        let moved = body
            .find("move_npm_prefix_home(")
            .expect("不可写时改到家目录");
        let installer = body
            .find("\"@larksuite/cli@latest\", \"install\"")
            .expect("走官方安装器");
        assert!(writable < moved && moved < installer);
        assert!(
            body[installer..].contains(".stdin(Stdio::null())"),
            "{body}"
        );
        assert!(!body.contains("\"config\", \"init\""), "{body}");
        assert!(!body.contains("\"auth\", \"login\""), "{body}");
    }

    /// 一份记住 `config set prefix` 的假 npm,放在 npx 旁边。`honor_set=false` 模拟前缀被
    /// `NPM_CONFIG_PREFIX` 钉住:写了也读不回来。
    fn fake_npm(dir: &std::path::Path, current: &std::path::Path, honor_set: bool) -> PathBuf {
        let node_bin = dir.join("node/bin");
        std::fs::create_dir_all(&node_bin).unwrap();
        let state = node_bin.join("prefix");
        std::fs::write(&state, format!("{}\n", current.display())).unwrap();
        let set = if honor_set {
            "echo \"$4\" > \"$state\""
        } else {
            "echo \"$4\" > \"$state.requested\""
        };
        let npm = node_bin.join("npm");
        crate::test_support::write_test_executable(
            &npm,
            &format!("#!/bin/sh\nstate=\"$(dirname \"$0\")/prefix\"\ncase \"$1 $2 $3\" in\n  'config set prefix') {set} ;;\n  'config delete prefix') rm -f \"$state.requested\" ;;\n  'prefix -g '*) cat \"$state\" ;;\nesac\n"),
        );
        node_bin.join("npx")
    }

    /// 同意:npm 全局目录改到 `~/.npm-global`,rc 把它的 bin 放进 PATH。
    #[test]
    fn an_unwritable_npm_prefix_is_moved_home_once_agreed() {
        let dir = tempfile::tempdir().unwrap();
        let current = dir.path().join("usr-local");
        std::fs::create_dir_all(current.join("lib/node_modules")).unwrap();
        let home = dir.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let npx = fake_npm(dir.path(), &current, true);

        let moved = super::move_npm_prefix_home(
            &npx,
            &current,
            &home,
            Some(std::ffi::OsStr::new("/bin/bash")),
            || Ok(true),
        )
        .unwrap();

        assert_eq!(moved, home.join(".npm-global"));
        assert_eq!(crate::setup::npm::global_prefix(&npx), Some(moved.clone()));
        assert!(moved.is_dir());
        assert!(std::fs::read_to_string(home.join(".bashrc"))
            .unwrap()
            .contains("export PATH=\"$HOME/.npm-global/bin:$PATH\""));
    }

    /// rc 写不进去(这里它是个目录):前缀已经改好,onboard 接着往下走。
    #[test]
    fn an_unwritable_rc_does_not_stop_a_finished_prefix_move() {
        let dir = tempfile::tempdir().unwrap();
        let current = dir.path().join("usr-local");
        let home = dir.path().join("home");
        std::fs::create_dir_all(home.join(".bashrc")).unwrap();
        let npx = fake_npm(dir.path(), &current, true);

        let moved = super::move_npm_prefix_home(
            &npx,
            &current,
            &home,
            Some(std::ffi::OsStr::new("/bin/bash")),
            || Ok(true),
        )
        .unwrap();

        assert_eq!(crate::setup::npm::global_prefix(&npx), Some(moved));
    }

    /// fish 这类不读 bash / zsh rc 的 shell:前缀照改,不往任何 rc 里写。
    #[test]
    fn a_shell_without_a_known_rc_gets_the_prefix_but_no_rc_edit() {
        let dir = tempfile::tempdir().unwrap();
        let current = dir.path().join("usr-local");
        let home = dir.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let npx = fake_npm(dir.path(), &current, true);

        let moved = super::move_npm_prefix_home(
            &npx,
            &current,
            &home,
            Some(std::ffi::OsStr::new("/usr/bin/fish")),
            || Ok(true),
        )
        .unwrap();

        assert_eq!(crate::setup::npm::global_prefix(&npx), Some(moved));
        let dotfiles: Vec<_> = std::fs::read_dir(&home)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .filter(|name| name != ".npm-global")
            .collect();
        assert!(dotfiles.is_empty(), "{dotfiles:?}");
    }

    /// 不同意:npm 配置与 rc 都不碰,说明其它做法后退出。
    #[test]
    fn a_declined_npm_prefix_move_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let current = dir.path().join("usr-local");
        let home = dir.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let npx = fake_npm(dir.path(), &current, true);

        let error = super::move_npm_prefix_home(&npx, &current, &home, None, || Ok(false))
            .unwrap_err()
            .to_string();

        assert!(error.contains("npm 全局目录没改"), "{error}");
        assert_eq!(crate::setup::npm::global_prefix(&npx), Some(current));
        assert!(!home.join(".profile").exists());
        assert!(!home.join(".npm-global").exists());
    }

    /// 改了读不回来(前缀被环境变量钉住)就停下说明,不往 rc 里写一个不生效的 PATH。
    #[test]
    fn a_prefix_change_npm_does_not_take_stops_before_touching_the_rc() {
        let dir = tempfile::tempdir().unwrap();
        let current = dir.path().join("usr-local");
        let home = dir.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let npx = fake_npm(dir.path(), &current, false);

        let error = super::move_npm_prefix_home(&npx, &current, &home, None, || Ok(true))
            .unwrap_err()
            .to_string();

        assert!(error.contains("NPM_CONFIG_PREFIX"), "{error}");
        assert!(error.contains("prefix 已撤回"), "{error}");
        assert!(!home.join(".profile").exists());
        assert!(
            !npx.with_file_name("prefix.requested").exists(),
            "写进 ~/.npmrc 的 prefix 要撤回"
        );
    }

    /// 撤回也失败时照实说,并给出手动撤回的命令。
    #[test]
    fn a_failed_undo_of_the_npmrc_prefix_says_how_to_undo_it() {
        let dir = tempfile::tempdir().unwrap();
        let current = dir.path().join("usr-local");
        let home = dir.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let npx = fake_npm(dir.path(), &current, false);
        let npm = npx.with_file_name("npm");
        let script = std::fs::read_to_string(&npm)
            .unwrap()
            .replace("rm -f \"$state.requested\"", "exit 1");
        crate::test_support::write_test_executable(&npm, &script);

        let error = super::move_npm_prefix_home(&npx, &current, &home, None, || Ok(true))
            .unwrap_err()
            .to_string();

        assert!(error.contains("没能撤回"), "{error}");
        assert!(error.contains("npm config delete prefix"), "{error}");
    }

    /// `--yes` 不问就改;交互时听回答;stdin 不是终端时没人能回答,不改。
    #[test]
    fn the_npm_prefix_move_is_confirmed_by_yes_or_a_person_at_the_terminal() {
        let never = || -> anyhow::Result<bool> { panic!("不该问") };
        assert!(super::confirm_npm_prefix_move(true, false, never).unwrap());
        assert!(!super::confirm_npm_prefix_move(false, false, never).unwrap());
        assert!(super::confirm_npm_prefix_move(false, true, || Ok(true)).unwrap());
        assert!(!super::confirm_npm_prefix_move(false, true, || Ok(false)).unwrap());
    }

    /// 在 npm 全局 bin 里找到的 lark-cli 要写进 `tools.lark_cli`,daemon 与 agent 会话才用得上;
    /// 已有人工值时不覆盖。
    #[test]
    fn located_lark_cli_is_written_to_tools_without_overriding_a_manual_value() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.yaml");
        let found = std::path::PathBuf::from("/home/u/.npm-global/bin/lark-cli");
        let entry = [(summon_core::tools::Tool::LarkCli, found.clone())];

        assert!(super::write_tool_paths(&config, &entry).unwrap());
        let written = std::fs::read_to_string(&config).unwrap();
        assert!(
            written.contains("lark_cli: '/home/u/.npm-global/bin/lark-cli'"),
            "{written}"
        );

        std::fs::write(&config, "tools:\n  lark_cli: '/opt/manual/lark-cli'\n").unwrap();
        assert!(!super::write_tool_paths(&config, &entry).unwrap());
        assert!(std::fs::read_to_string(&config)
            .unwrap()
            .contains("/opt/manual/lark-cli"));

        // 找到的路径与配置不同时一定要走这一步;只测函数,接线被摘掉也不会红。
        let src = include_str!("onboard.rs");
        let ready = &src[src.find("fn ensure_lark_cli_installed(").unwrap()..];
        let ready = &ready[..ready.find("\nfn ").unwrap()];
        assert!(ready.contains("if lark_cli != configured {"));
        assert!(ready.contains("write_tool_paths(&config_path, &[(tool, lark_cli.clone())])"));
    }

    #[test]
    fn menubar_icon_is_asked_only_interactively_and_once() {
        assert!(super::should_ask_menubar_icon(false, false));
        assert!(
            !super::should_ask_menubar_icon(true, false),
            "--yes 不问,按默认图标画"
        );
        assert!(!super::should_ask_menubar_icon(false, true), "选过就不再问");
    }

    #[test]
    fn writes_menubar_icon_and_keeps_existing_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        std::fs::write(&path, "# 我的注释\ndefault_domain: feishu.cn\n").unwrap();
        super::write_menubar_icon(&path, "synchro").unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "# 我的注释\ndefault_domain: feishu.cn\n\nmenubar:\n  icon: synchro\n"
        );
    }

    use super::*;

    /// 回归钉:daemon 已在跑时 onboard 只热加载,**绝不 restart**。
    /// restart 会 SIGTERM 掉所有在跑的无头子进程组(CLAUDE.md 硬规则的连带代价)——等于为了让
    /// 第 N 个 agent 上线,打断另外 N-1 个正在干活的 agent,还丢掉 daemon 全部内存态。
    /// 用源码断言而非行为测:这一步要真起 daemon + 真连飞书,不适合单测,但分支选择必须钉住。
    #[test]
    fn onboard_hot_loads_instead_of_restarting_a_running_daemon() {
        let src = include_str!("onboard.rs");
        let start = src
            .find("// 7. 让新 agent 上线")
            .expect("找不到 onboard 第 7 步");
        let tail = &src[start..];
        let end = tail
            .find("if let Some(chat) = confirm_chat")
            .expect("第 7 步后应紧跟回执卡片");
        let step7 = &tail[..end];
        assert!(
            step7.contains("running_pid()"),
            "第 7 步必须先判 daemon 在不在,才能决定 reload 还是 start"
        );
        assert!(
            step7.contains("agents_reload()"),
            "daemon 在跑时必须走热加载(agents_reload),不能 restart"
        );
        // daemon_restart 只允许出现在「没在跑」那条分支里(此时它等价于 start,无人可打断)。
        let restart_at = step7
            .find("daemon_restart()")
            .expect("没在跑时仍需拉起 daemon");
        let else_at = step7.find("} else {").expect("应有 daemon 未运行的分支");
        assert!(
            restart_at > else_at,
            "daemon_restart 只能在「daemon 未运行」分支里出现"
        );
    }

    #[test]
    fn scopes_json_valid_and_minimal() {
        let v: serde_json::Value = serde_json::from_str(SCOPES_JSON).expect("scopes JSON 合法");
        let tenant = v["scopes"]["tenant"].as_array().expect("tenant 是数组");
        let ids: Vec<&str> = tenant.iter().filter_map(|s| s.as_str()).collect();
        assert_eq!(
            ids,
            vec![
                "application:application:self_manage",
                "application:bot.basic_info:read",
                "application:bot.menu:write",
                "cardkit:card:read",
                "cardkit:card:write",
                "contact:contact.base:readonly",
                "docs:document.comment:create",
                "docs:document.comment:delete",
                "docs:document.comment:read",
                "docs:document.comment:update",
                "docs:document.comment:write_only",
                "docx:document.block:convert",
                "docx:document:create",
                "docx:document:readonly",
                "docx:document:write_only",
                "drive:drive.metadata:readonly",
                "im:chat.members:bot_access",
                "im:chat:create",
                "im:chat:read",
                "im:chat:update",
                "im:message.group_at_msg.include_bot:readonly",
                "im:message.group_at_msg:readonly",
                "im:message.p2p_msg:readonly",
                "im:message.pins:read",
                "im:message.pins:write_only",
                "im:message.reactions:read",
                "im:message.reactions:write_only",
                "im:message:readonly",
                "im:message:send_as_bot",
                "im:message:send_multi_users",
                "im:message:send_sys_msg",
                "im:message:update",
                "im:resource",
                "wiki:node:read",
            ]
        );
        let user: Vec<&str> = v["scopes"]["user"]
            .as_array()
            .expect("user 是数组")
            .iter()
            .filter_map(|s| s.as_str())
            .collect();
        assert_eq!(user, vec!["offline_access"]);
        for need in [
            "im:message:send_as_bot",
            "im:message:readonly", // 引用回复拉被引消息原文
            "im:message.p2p_msg:readonly",
            "im:message.reactions:write_only",
            "im:resource", // 下载消息附件(图片/文件)
            "contact:contact.base:readonly",
        ] {
            assert!(ids.contains(&need), "scopes 缺关键项 {need}");
        }
    }

    #[test]
    fn app_creation_uses_lark_launcher_without_decorating_the_link() {
        let onboard = include_str!("onboard.rs");

        assert_eq!(
            bind::CREATE_APP_URL,
            "https://open.larkoffice.com/page/launcher"
        );
        assert_eq!(bind::CONSOLE_URL, "https://open.larkoffice.com/app");
        assert!(onboard.contains("println!(\"     {}\", bind::CREATE_APP_URL);"));
        let decorated_link = ["{}", "(已尝试用浏览器打开"].concat();
        assert!(!onboard.contains(&decorated_link));
    }

    #[test]
    fn scope_usage_matches_scopes_json() {
        // 权限用途清单(打给用户看的申请理由)必须与导入 JSON 一一对应:
        // 加/删 scope 时漏改另一边,这里红。
        let v: serde_json::Value = serde_json::from_str(SCOPES_JSON).unwrap();
        let json_ids: Vec<&str> = ["tenant", "user"]
            .into_iter()
            .flat_map(|scope_type| {
                v["scopes"][scope_type]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter_map(|s| s.as_str())
            })
            .collect();
        let usage_ids: Vec<&str> = SCOPE_USAGE.iter().map(|(s, _)| *s).collect();
        for id in &json_ids {
            assert!(
                usage_ids.contains(id),
                "JSON 里的 {id} 缺用途说明(SCOPE_USAGE)"
            );
        }
        for id in &usage_ids {
            assert!(
                json_ids.contains(id),
                "SCOPE_USAGE 里的 {id} 不在导入 JSON 里"
            );
        }
    }

    #[test]
    fn default_config_template_is_all_comments() {
        // 模板必须全注释(或空行)→ 写下去解析为空 = 全默认,无意外活动键
        for line in crate::setup::init::config_example_yaml().lines() {
            let t = line.trim();
            assert!(
                t.is_empty() || t.starts_with('#'),
                "模板应全注释,出现活动行: {line}"
            );
        }
        assert!(crate::setup::init::config_example_yaml().contains("messaging:"));
        assert!(crate::setup::init::config_example_yaml().contains("reactions"));
        assert!(crate::setup::init::config_example_yaml().contains("system_prompt"));
        assert!(crate::setup::init::config_example_yaml().contains("prompt_template"));
        assert!(!crate::setup::init::config_example_yaml().contains("primer"));
    }

    #[test]
    fn cli_takes_effort_only_for_claude_codex() {
        assert!(cli_takes_effort("claude"));
        assert!(cli_takes_effort("codex"));
        // cursor 把档位烤进 model 名,无独立 effort 轴
        assert!(!cli_takes_effort("cursor"));
        assert!(!cli_takes_effort("other"));
    }

    #[test]
    fn resolve_flags_validates_and_drops_cursor_effort() {
        let m = summon_core::config::builtin_messaging();
        // claude:合法 model+effort 原样带出
        let r =
            resolve_flags("claude", Some("claude-opus-4-8"), Some("high"), &m, || None).unwrap();
        assert_eq!(r.0.as_deref(), Some("claude-opus-4-8"));
        assert_eq!(r.1.as_deref(), Some("high"));

        // claude:非法 effort → fail-closed
        assert!(resolve_flags(
            "claude",
            Some("claude-opus-4-8"),
            Some("minimal"),
            &m,
            || None
        )
        .is_err());

        // cursor:给了 --effort → 拒绝(应把档位写进 --model)
        assert!(resolve_flags("cursor", Some("gpt-5.5"), Some("high"), &m, || None).is_err());

        // cursor:只 model(档位已在 model 名里)→ effort 为 None 带出
        let r = resolve_flags("cursor", Some("gpt-5.5-high"), None, &m, || None).unwrap();
        assert_eq!(r.0.as_deref(), Some("gpt-5.5-high"));
        assert_eq!(r.1, None);
    }

    #[test]
    fn resolve_flags_fills_default_model_per_cli_when_flag_absent() {
        // --model 缺省且问不到 backend 时按 cli 取各自桶首项:选 codex 不会拿到 claude 的 id
        // (跨命名空间的默认值会写出一个 codex 起不来的 agent.yaml)。
        let m = summon_core::config::builtin_messaging();
        let codex = resolve_flags("codex", None, None, &m, || None).unwrap().0;
        let claude = resolve_flags("claude", None, None, &m, || None).unwrap().0;
        assert_eq!(codex.as_deref(), Some("gpt-5.6-sol"));
        assert_eq!(claude.as_deref(), Some("claude-opus-5"));
        assert_ne!(codex, claude);

        // backend 报了默认模型就用它:上游新出的模型不必先进配置表才能当默认。
        assert_eq!(
            resolve_flags("codex", None, None, &m, || Some("gpt-6-astra".into()))
                .unwrap()
                .0
                .as_deref(),
            Some("gpt-6-astra")
        );

        // 无候选桶的未知 cli(内置扁平表为空)→ None = 不写 model 行,而不是猜一个 id。
        assert_eq!(
            resolve_flags("nonesuch", None, None, &m, || None)
                .unwrap()
                .0,
            None
        );
    }

    #[test]
    fn cursor_compose_via_driver_for_picker() {
        // 交互向导 cursor 分支组装 model id 走 core 的纯函数,这里锁住接缝可用
        use summon_core::driver::CliDriver;
        assert_eq!(
            CliDriver::cursor_compose_model("claude-opus-4-8", Some("high"), true, false),
            "claude-opus-4-8-thinking-high"
        );
    }

    #[test]
    fn parse_capture_line_extracts_open_id() {
        let c = parse_capture_line(r#"{"open_id":"ou_x","chat_id":"oc_y","text":"hi"}"#).unwrap();
        assert_eq!(c.open_id, "ou_x");
        assert_eq!(c.chat_id, "oc_y");
        assert_eq!(c.union_id, ""); // 缺 union_id 时 default 空,退回 open_id
                                    // 带 union_id 时也能解析(白名单优先用它)
        let u =
            parse_capture_line(r#"{"open_id":"ou_x","union_id":"on_z","chat_id":"oc_y"}"#).unwrap();
        assert_eq!(u.union_id, "on_z");
        // 非 JSON(SDK 日志行)不误判
        assert!(parse_capture_line("[Info] connected to wss://msg-frontier...").is_none());
        // 空 open_id 不算
        assert!(parse_capture_line(r#"{"open_id":"","chat_id":"oc_y"}"#).is_none());
    }

    #[test]
    fn the_wrap_up_mentions_autostart_and_linger_on_linux() {
        let linux = super::autostart_hint(true);
        assert!(linux.contains("summon autostart enable") && linux.contains("linger"));
        let mac = super::autostart_hint(false);
        assert!(mac.contains("summon autostart enable") && !mac.contains("linger"));
    }

    /// 没有剪贴板时权限 JSON 原文要出现在引导里,不能只给一个文件路径。
    #[test]
    fn the_scopes_json_is_printed_when_it_cannot_be_copied() {
        let src = include_str!("onboard.rs");
        let guide = &src[src.find("fn print_app_config_guide(").unwrap()..];
        let guide = &guide[..guide.find("\n}\n").unwrap()];
        let fallback = &guide[guide.find("} else {").unwrap()..];
        assert!(fallback.contains("SCOPES_JSON"), "{fallback}");
    }

    #[test]
    fn handoff_block_points_to_guide_and_doctor() {
        let block = handoff_block("小助");
        assert!(block.contains("小助"), "交接块要带 agent 显示名");
        assert!(block.contains("summon guide"), "交接块要指向 guide");
        assert!(block.contains("summon doctor"), "交接块要指向 doctor");
    }

    #[test]
    fn loopback_failure_hint_triages_by_bot_probe() {
        // 体检失败 → 指向机器人能力/发布,不再三条一锅端
        let hint = loopback_failure_hint(Some(false));
        assert!(
            hint.contains("机器人"),
            "体检失败应指向机器人能力,实得:{hint}"
        );
        assert!(
            !hint.contains("im.message.receive_v1"),
            "体检失败不该扯事件订阅"
        );
        // 体检通过 → 指向事件链路
        let hint = loopback_failure_hint(Some(true));
        assert!(
            hint.contains("im.message.receive_v1"),
            "体检通过应指向事件订阅"
        );
        // 跳过验真(--no-validate)→ 没有体检结果,退回完整三支
        let hint = loopback_failure_hint(None);
        assert!(hint.contains("im.message.receive_v1"));
        assert!(hint.contains("发布"));
    }
}
