// 主题目录名 `session` 与内层 `session.rs` 同名(布局要求),module_inception 是该结构的必然产物。
/// 跨会话索引/检索的落盘侧(冷启 PRIMER 索引段 + `summon chats` 命令共用)。
pub mod cross_chat;
pub(crate) mod discovery;
pub(crate) mod ended_archive;
/// 退出回执的去重标记(chat 维度):谁发出/入队了回执谁写,谁准备发先看。
pub(crate) mod exit_mark;
/// 注入闸:向 pane 发任何按键之前的输入态证明(spec 2026-08-14 第二节的不变量收口点)。
pub mod input_gate;
pub(crate) mod lifecycle;
/// per-chat CLI resume 记录的单一收口(取代旧的双存储 session.rs `.session` + session_store `session_id`)。
pub mod resume_store;
#[allow(clippy::module_inception)]
pub mod session;
/// 白名单 slash 的执行器:注入 → 按类别等收尾 → 取回显(零 marker、at-most-once)。
pub mod slash_exec;
pub mod tmux;
