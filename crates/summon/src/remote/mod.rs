//! cmux 遥控:群消息打进 cmux 窗口里的 Claude 会话,每轮最终回复发回群里。
//! 设计见 `docs/spec/2026-10-04-cmux-remote-control-design.md`。

pub(crate) mod bind;
pub(crate) mod cmux;
pub(crate) mod inbound;
pub(crate) mod inject;
pub(crate) mod relay;
pub(crate) mod runtime;
pub(crate) mod store;
