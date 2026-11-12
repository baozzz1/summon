//! sentinel——owner 级「任意会话召唤 agent」子系统的**纯逻辑层**。
//!
//! 触发解析([`trigger`])与闸门判定([`gate`])都是可单测纯函数;lark-cli 调用、轮询线程、
//! 动作执行等 IO 在 summon 侧 `sentinel/` 模块。设计与不变量见
//! `docs/spec/2026-08-07-owner-sentinel-design.md`——尤其是六条红线:误触的后果是把 bot
//! 或真人拉进真实群聊,所以这里的每个判定都必须 fail-closed(认不出/判不准 = 不动作)。

pub mod failure_log;
pub mod gate;
pub mod trigger;
