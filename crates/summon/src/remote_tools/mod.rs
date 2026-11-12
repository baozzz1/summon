//! 出站 SSH 上的远程工具执行；与飞书遥控现有终端的 remote 模块职责独立。

mod client;
pub(crate) mod integration;
mod platform;
mod process;
mod proxy;
mod registry;
mod server;
mod shell;
mod wire;

use anyhow::Result;
use clap::Subcommand;
use std::ffi::OsString;
use std::path::PathBuf;
use summon_core::config::Config;

#[derive(Debug, Subcommand)]
pub enum RemoteToolsCommand {
    #[command(about = summon_i18n::text!("r611acbde94"))]
    /// 从执行机器主动连接云端；SSH stdin/stdout 承载工具请求，无需本机 SSH 服务。
    Connect {
        #[arg(help = summon_i18n::text!("r13aef9a792"))]
        /// SSH Host 别名或 user@host。
        host: String,
        #[arg(help = summon_i18n::text!("r029425d5af"))]
        /// 云端 execution_targets 中登记的目标名。
        #[arg(long)]
        name: String,
        #[arg(help = summon_i18n::text!("r9b1b1f0d68"))]
        /// 云端 summon 可执行文件的路径或命令名。
        #[arg(long, default_value = "summon")]
        remote_summon: String,
        #[arg(help = summon_i18n::text!("rb714a52c1f"))]
        /// 本机 Claude 可执行文件；省略则使用 tools.claude / PATH。
        #[arg(long)]
        claude: Option<PathBuf>,
        #[arg(help = summon_i18n::text!("r02add0d90a"))]
        /// 本机默认工作目录；省略使用启动目录。
        #[arg(long)]
        working_dir: Option<PathBuf>,
    },
    #[command(about = summon_i18n::text!("r70e19a9506"))]
    /// 查询已配置执行目标的实时在线状态。
    List,
    #[command(about = summon_i18n::text!("r07f6effd22"))]
    /// 用指定执行目标启动 Claude；-- 后参数原样透传。
    Launch {
        #[arg(help = summon_i18n::text!("r66519f1092"))]
        /// 已在线的执行目标名。
        #[arg(long)]
        target: String,
        #[arg(help = summon_i18n::text!("r32d4d267c8"))]
        /// 执行机器上的绝对工作目录；省略使用连接默认目录。
        #[arg(long)]
        working_dir: Option<String>,
        #[arg(help = summon_i18n::text!("r12e40028f0"))]
        /// 云端 Claude 可执行文件；省略使用 tools.claude / PATH。
        #[arg(long)]
        claude: Option<PathBuf>,
        #[arg(help = summon_i18n::text!("r2a591754df"))]
        /// -- 后逐项传给 Claude 的原生参数。
        #[arg(last = true, allow_hyphen_values = true)]
        args: Vec<OsString>,
    },
    #[command(about = summon_i18n::text!("rbd8c62a32f"))]
    /// 在 SSH 命令通道中运行云端连接服务。
    #[command(hide = true)]
    Server {
        #[arg(help = summon_i18n::text!("r291724d2df"))]
        /// 已在 execution_targets 登记的目标名。
        #[arg(long)]
        name: String,
    },
    #[command(about = summon_i18n::text!("r49966af2c3"))]
    /// 把 MCP stdin/stdout 接到在线执行目标。
    #[command(hide = true)]
    Proxy {
        #[arg(help = summon_i18n::text!("rfc1d3fa89e"))]
        /// 在线执行目标名。
        #[arg(long)]
        target: String,
        #[arg(help = summon_i18n::text!("rbcd38976e1"))]
        /// 执行机器上的绝对工作目录。
        #[arg(long)]
        working_dir: Option<String>,
    },
    #[command(about = summon_i18n::text!("rf62e09f2d0"))]
    /// 提供经 turn 凭证鉴权的云端 summon MCP 工具。
    #[command(hide = true)]
    Platform,
    #[command(about = summon_i18n::text!("raca35db7f7"))]
    /// 执行机器上承载 Claude shell prefix 的本机监督入口。
    #[command(hide = true)]
    Shell {
        #[arg(help = summon_i18n::text!("rf8ebfe9a3a"))]
        /// 当前执行会话的本机监督 socket。
        #[arg(long)]
        socket: PathBuf,
        #[arg(help = summon_i18n::text!("rc0ffad0675"))]
        /// Claude 交付的完整 shell 命令。
        #[arg(last = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
}

pub fn run(config: &Config, action: RemoteToolsCommand) -> Result<()> {
    use RemoteToolsCommand::*;
    match action {
        Connect {
            host,
            name,
            remote_summon,
            claude,
            working_dir,
        } => client::connect(
            &host,
            &name,
            &remote_summon,
            claude.unwrap_or_else(|| summon_core::tools::resolve(summon_core::tools::Tool::Claude)),
            working_dir,
        ),
        Server { name } => server::run(&name, config),
        Proxy {
            target,
            working_dir,
        } => proxy::run(&target, working_dir),
        Platform => platform::run(),
        Shell { socket, args } => shell::run(&socket, args),
        Launch {
            target,
            working_dir,
            claude,
            args,
        } => integration::launch(
            config,
            summon_core::remote_tools::ExecutionBinding {
                target,
                working_dir,
            },
            claude.unwrap_or_else(|| summon_core::tools::resolve(summon_core::tools::Tool::Claude)),
            args,
        ),
        List => {
            let root = registry::root()?;
            let mut targets: Vec<_> = config.execution_targets.keys().collect();
            targets.sort();
            let statuses: Vec<_> = targets
                .into_iter()
                .map(|name| match registry::status(&root, name) {
                    Ok(status) => serde_json::json!({"name":name,"online":true,"status":status}),
                    Err(error) => {
                        serde_json::json!({"name":name,"online":false,"error":error.to_string()})
                    }
                })
                .collect();
            println!("{}", serde_json::to_string_pretty(&statuses)?);
            Ok(())
        }
    }
}
