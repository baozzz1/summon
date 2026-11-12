// 主题目录名 `config` 与其内层 `config.rs` 同名(布局要求),module_inception 是该结构的必然产物。
pub mod agent_identity;
pub mod autostart;
pub mod codex_policy;
#[allow(clippy::module_inception)]
pub mod config;
pub mod menubar;
pub mod model_effort;
pub mod registry;
pub mod settings;

// 主题层把内层 `config` 模块的公开项平铺出来,使 `summon_core::config::Config`(跨 crate)
// 与 `crate::config::config::Config`(crate 内真实路径)都成立。
pub use config::*;
