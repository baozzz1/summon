pub mod agents_add;
pub(crate) mod auto_upgrade;
pub mod autostart;
pub mod bind;
pub mod claude_quota;
pub(crate) mod config_edit;
pub(crate) mod config_findings;
pub mod env;
pub mod github_upgrade;
pub mod init;
pub(crate) mod lark_cli;
pub(crate) mod launchd;
pub mod menubar;
pub(crate) mod menubar_settings;
pub(crate) mod npm;
// 可整目录摘掉的模块(见 build.rs)。rustfmt 不看 cfg,声明上挂 skip,目录不在时它才不报错;
// 带这个目录的源码树要在自己的检查里单独对它跑 rustfmt。
#[cfg(npm_dist)]
#[rustfmt::skip]
pub mod npm_dist;
pub mod onboard;
pub mod self_install;
pub mod sentinel_config;
pub(crate) mod upgrade_restart;
