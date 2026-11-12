//! owner sentinel——用 owner 身份把任意会话召唤给 agent(daemon IO 层)。
//!
//! 纯逻辑(触发解析/闸门)在 `summon_core::sentinel`;本模块四件套:
//! - [`lark`]——lark-cli shell-out(owner 用户身份的唯一通道,凭据绝不进 agent 子进程);
//! - [`poller`]——常驻轮询线程(tail + 搜索双通道,骨架同 scheduler);
//! - [`actions`]——动作链(拉 bot 进群 / 三方建群提案),对外副作用全部收在这里;
//! - [`proposals`]——三方建群确认卡与执行之间的持久握手存根。
//!
//! 身份纪律(2026-08-07 真机两次 `open_id cross app` 的教训):lark-cli 命名空间与各 agent app
//! 命名空间的 open_id **互不相通**。凡走 lark-cli 的参数用 lark-cli 侧 id(owner 触发过滤、
//! 拉对方真人);凡走 agent `FeishuClient` 的参数用该 agent 侧 id(建群 owner、确认卡收件人、
//! ChatTask who_id)。两边绝不混用。
//!
//! 设计与六条红线见 `docs/spec/2026-08-07-owner-sentinel-design.md`。

pub(crate) mod actions;
pub(crate) mod lark;
pub(crate) mod poller;
pub(crate) mod proposals;
