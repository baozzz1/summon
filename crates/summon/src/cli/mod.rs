// 主题目录名 `cli` 与内层 `cli.rs` 同名(布局要求),module_inception 是该结构的必然产物。
#[allow(clippy::module_inception)]
pub mod cli;
mod completion;
#[cfg(debug_assertions)]
pub mod dev;
mod guide;
mod insights;
mod menubar_usage;
mod session;
mod style;
mod table;
mod usage;

pub use cli::{parse, run};
