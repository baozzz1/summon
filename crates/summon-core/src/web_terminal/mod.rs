//! 浏览器内终端的纯逻辑:一次性码与浏览器会话、WebSocket 帧、路由与 cookie、页面、滚动。
//! IO(监听、pty、tmux)在 `summon::runtime::web_terminal`。
//! 设计见 `docs/spec/2026-09-22-web-terminal-design.md`。

pub mod grants;
pub mod http;
pub mod page;
pub mod scroll;
pub mod ws;
