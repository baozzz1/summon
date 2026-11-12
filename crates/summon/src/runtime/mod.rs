/// 全局准入闸:daemon 换代之前让 chat 线程暂不开新的一轮。
pub(crate) mod admission;
pub(crate) mod agent_availability;
pub(crate) mod agent_retirement;
pub(crate) mod app_owner;
pub(crate) mod background_holder;
pub(crate) mod card_action_wal;
pub(crate) mod chat_model;
pub(crate) mod chat_tools;
pub(crate) mod claude_hook;
pub(crate) mod claude_usage;
pub mod core;
pub(crate) mod current_turn_store;
pub(crate) mod cursor_quota;
pub mod daemon;
/// 配置控制台服务:独立端口,点击认证 / 本机码进入,`POST /api/<方法>` JSON 接口。
pub(crate) mod dashboard;
pub(crate) mod document_comment_reader;
pub(crate) mod document_comments;
pub mod executor;
/// 会话退出回执的唯一组装入口(CLI 与 daemon 共用)。
pub(crate) mod exit_receipt;
mod feishu_legacy_claim;
pub(crate) mod fsio;
/// 与业务无关的 HTTP 连接层(浏览器终端与控制台共用)。
pub(crate) mod http_conn;
pub mod identity_cache;
pub mod inbox_wal;
pub mod inflight;
pub(crate) mod insights_analysis;
pub(crate) mod localization;
pub(crate) mod menubar_session;
pub mod outbox_store;
pub mod paths;
pub mod peers_store;
/// 文件预览服务器(token 链接、点击认证与 HTTP 服务,回复里文件按钮的运行时半边)。
pub(crate) mod preview_server;
pub(crate) mod prices;
pub(crate) mod process;
pub mod reaction_store;
/// stdio 上的 JSON-RPC 传输层原语(codex app-server / cursor ACP 两个客户端共用)。
pub mod rpc_stdio;
pub(crate) mod self_followup;
pub(crate) mod session_end;
pub(crate) mod status_snapshot;
/// PreToolUse 执行闸:外部发起人的轮次只读。由 summon 会话专属 settings 调用。
pub mod tool_guard;
pub(crate) mod turn_ledger;
/// waiting 标记(agent 回完、轮到 owner 处理)的盘上存取。
pub(crate) mod waiting_store;
/// 浏览器内终端服务:独立端口,落地页 + 进入 + 终端页 + WebSocket。
pub(crate) mod web_terminal;
// 录制 cassette 落盘 IO:整模块 debug-only(release 不编译,生产零录制代码)。
#[cfg(debug_assertions)]
pub mod record_store;
// `summon dev record {on|off|ls|promote}` 实现:debug-only(同 dev 命令组)。
#[cfg(debug_assertions)]
pub mod record_cmd;
// 回放执行器 + RR1 回放保真测:整模块 test-only,且只在 debug 编译——它依赖 `record_store`/
// `RecordingExecutor` 这些 `debug_assertions` gate 的录制设施,release 下这些类型不存在。
#[cfg(all(test, debug_assertions))]
pub mod replay;
