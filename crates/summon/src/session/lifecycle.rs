//! chat 的**路由生命周期**事实:summon 还在不在为这个 chat 维持一条会话。
//!
//! ## 为什么不复用 `exit_mark`
//!
//! 上一版正是栽在这里。`exit_mark` 的语义写在它自己的模块注释第一句:「退出回执**已经有人负责了**」
//! —— 那是**回执去重**,不是生命周期。两者在三处对不上:
//!
//! - 面板 Clean/Reset 成功 PATCH 原卡时**不发回执、因此不写标记**(`teardown.rs` 里明确写了这个
//!   取舍),而那恰恰是最常见的关闭方式 —— 拿它判活会整片漏掉;
//! - 它会被 idle 探测在合成回执后清掉(去重用完就清),生命周期却还没变;
//! - 反过来,关闭后用 `/compact` 唤醒会话时 slash 路径提前 return,标记不会被清,活着的会话被误判成关闭。
//!
//! 一个为 A 语义设计的标记,拿去表达 B 语义,不会因为"看起来差不多"就变对。
//!
//! ## 这个事实**只**表达一件事
//!
//! 「路由生命周期是否已退休」。它刻意**不**表达「进程是否还在」—— 那是 `tmux_present` 的职责。
//! 两者必须分开,否则 `Closed + 进程还活着`(teardown 失败 / 资源泄漏)会被合并进 `Gone`,
//! 等于把一起事故从列表里藏掉。分开之后那种组合能被单独识别成 `ClosedButProcessLive` 并显眼呈现。
//!
//! 按这个定义,`do_teardown` 里那步销毁失败只记日志、不影响本事实的正确性:Closed 说的是
//! 「我们不再为这个 chat 维持路由」,kill 没成功是另一回事,而且正该由上面那个事故态暴露出来。
//!
//! ## 为什么不用时间戳
//!
//! 曾考虑过 `closed_at` 与「最后活动时刻」比大小来省掉清除点,被否掉了:那是拿**墙上时钟**表达
//! **因果顺序**。时钟回拨后新活动可能仍小于 `closed_at`;`.meta` 回退路径只有秒级精度,同秒关闭
//! 再开会相等;换文件系统、备份恢复、校时都会破坏这个假设。谁在谁之后,只能由显式状态迁移表达。

use crate::runtime::fsio::atomic_write;
use std::path::PathBuf;
use summon_core::registry::Agent;

/// 路由生命周期的三态。`Unknown` 不是"坏了",而是**没有记录**——升级前就存在的会话一律是它,
/// 判活时必须原样退回旧判据,绝不能把"没写过状态"当成"已关闭"。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChatLifecycle {
    Open,
    Closed,
    Unknown,
}

impl ChatLifecycle {
    /// 只有明确记着 Closed 才算已退休。`Unknown` 走旧判据,`Open` 是活的。
    pub(crate) fn is_closed(self) -> bool {
        matches!(self, ChatLifecycle::Closed)
    }
}

fn path(agent: &Agent, tag: &str) -> PathBuf {
    agent
        .state_dir()
        .join("runtime")
        .join("chats")
        .join(format!("{tag}.lifecycle"))
}

/// 记「这个 chat 的路由已退休」。收口在拆除侧(`do_teardown`),覆盖面板 Clean/Reset、群解散、
/// bot 被移除、KillTeardown 等全部拆除路径 —— 它们无一例外都要过那个函数。
///
/// best-effort:写失败只让判活退回旧行为(会话多留一会儿),绝不能中断拆除流程。
pub(crate) fn mark_closed(agent: &Agent, tag: &str) {
    // 写失败保持旧值(Open / 无记录):会话在列表里多留一会儿,是安全方向。
    let _ = write(agent, tag, "closed");
}

/// 记「这个 chat 又开始了一个新的生命周期」。**收口在语义 admission**,不是在 `spawn_chat_thread`
/// —— 线程因 channel 断开而重建属于同一个生命周期,那里写就等于把关闭状态错误地抹掉。
///
/// 写失败**不能一走了之**:文件里可能还留着上一次的 `closed`,那会让这个**正在开始**的会话被判
/// 成已关闭、甚至误报成泄漏事故。
///
/// 两级处理:先尝试删掉残留状态降级为 `Unknown`(判活退回旧判据,等价于本改动前的行为,安全);
/// 删也删不掉时返回 `Err` —— 此时磁盘上确凿留着 `closed`,而我们正要放行一轮新对话,两者矛盾。
/// 调用方必须 **fail-stop**:不放行、不启动 provider、把错误外显。宁可这条消息明确失败让人看见,
/// 也不要放它进去、事后在列表里被判成已关闭甚至泄漏事故(裁决:Flare 5 2026-08-13)。
pub(crate) fn mark_open(agent: &Agent, tag: &str) -> Result<(), String> {
    super::ended_archive::invalidate_checked(agent, tag)
        .map_err(|e| summon_i18n::format!("rcb18dea118"))?;
    if write(agent, tag, "open").is_ok() {
        return Ok(());
    }
    let path = path(agent, tag);
    if std::fs::remove_file(&path).is_ok() {
        crate::observe::log::log(&summon_i18n::format!("r7fd6fc80ef", agent.name));
        return Ok(());
    }
    // 删不掉时必须**确认**它到底还在不在。这里绝不能用 `Path::exists()`:它把
    // `PermissionDenied` 之类的 metadata 错误一律吞成 `false`,于是「读不到状态」会伪装成
    // 「没有状态」,fail-stop 被静默绕过、带着残留的 `closed` 放行 provider。
    // `try_exists()` 把三种情况分开:只有确知不存在(`Ok(false)`)才允许降级。
    match path.try_exists() {
        Ok(false) => {
            crate::observe::log::log(&summon_i18n::format!("r797d90763d", agent.name));
            Ok(())
        }
        Ok(true) => Err(summon_i18n::format!("r5237f9868a")),
        Err(e) => Err(summon_i18n::format!("r1d3871a942")),
    }
}

fn write(agent: &Agent, tag: &str, state: &str) -> std::io::Result<()> {
    let path = path(agent, tag);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // 原子写:半截文件会被读成 Unknown,那会让已关闭的会话重新冒出来。
    atomic_write(&path, state.as_bytes()).map_err(|e| {
        crate::observe::log::log(&summon_i18n::format!("r917b55d645", agent.name));
        std::io::Error::other(e.to_string())
    })
}

/// 读当前状态。文件不存在 / 读失败 / 内容不认识 → `Unknown`(退回旧判据),**不猜**。
pub(crate) fn read(agent: &Agent, tag: &str) -> ChatLifecycle {
    match std::fs::read_to_string(path(agent, tag)) {
        Ok(raw) => match raw.trim() {
            "closed" => ChatLifecycle::Closed,
            "open" => ChatLifecycle::Open,
            _ => ChatLifecycle::Unknown,
        },
        Err(_) => ChatLifecycle::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_until_written_then_roundtrips_both_ways() {
        let (agent, _tmp) = crate::test_support::agent_dir("claude", "");
        // 没写过 = Unknown,**不是** Closed。升级前的存量会话全走这条,判错就是整片会话凭空消失。
        assert_eq!(read(&agent, "g1"), ChatLifecycle::Unknown);
        assert!(!read(&agent, "g1").is_closed());

        mark_closed(&agent, "g1");
        assert_eq!(read(&agent, "g1"), ChatLifecycle::Closed);
        assert!(read(&agent, "g1").is_closed());

        // 复活:新生命周期 admission 覆盖回 Open,不需要删文件。
        mark_open(&agent, "g1").expect("正常磁盘上写 open 必须成功");
        assert_eq!(read(&agent, "g1"), ChatLifecycle::Open);
        assert!(!read(&agent, "g1").is_closed());
    }

    /// 内容不认识(将来加了新状态、或文件被写坏)一律 Unknown —— 退回旧判据比猜一个方向安全。
    #[test]
    fn unrecognized_content_falls_back_to_unknown() {
        let (agent, _tmp) = crate::test_support::agent_dir("claude", "");
        std::fs::create_dir_all(agent.state_dir().join("runtime").join("chats")).unwrap();
        std::fs::write(path(&agent, "g2"), b"retired-ish").unwrap();
        assert_eq!(read(&agent, "g2"), ChatLifecycle::Unknown);
    }

    /// ⑥ 的 fail-stop 分支:写不进去、又删不掉残留时必须返回 `Err`,由 admission 拒绝放行。
    ///
    /// 用「把状态文件的位置占成一个目录」来构造真实故障:`atomic_write` 的 rename 会失败,
    /// `remove_file` 对目录也失败 —— 双失败成立,不需要 mock 文件系统。
    ///
    /// 为什么必须硬失败:此刻磁盘上确凿留着上一次的 `closed`,而我们正要放行一轮新对话。
    /// 放进去 = 明知这个会话会被判成已关闭(甚至误报成泄漏事故)还继续启动 provider。
    /// 让这条消息明确失败给人看见,好过事后在列表里以错误状态出现。
    #[test]
    fn mark_open_fails_stop_when_stale_closed_cannot_be_removed() {
        let (agent, _tmp) = crate::test_support::agent_dir("claude", "");
        let chats = agent.state_dir().join("runtime").join("chats");
        std::fs::create_dir_all(&chats).unwrap();
        // 先留下一个真实的 closed 残留,再把同名路径占成目录(模拟磁盘异常/权限问题)。
        mark_closed(&agent, "g1");
        assert_eq!(read(&agent, "g1"), ChatLifecycle::Closed);
        std::fs::remove_file(path(&agent, "g1")).unwrap();
        std::fs::create_dir(path(&agent, "g1")).unwrap();

        let err = mark_open(&agent, "g1").expect_err("写不进又删不掉时必须 fail-stop");
        assert!(err.contains("g1"), "错误要带 tag 便于定位: {err}");

        // 关键不变量:失败之后状态**没有**变成 open —— 调用方据此拒绝放行才是安全的。
        assert_ne!(read(&agent, "g1"), ChatLifecycle::Open);
    }

    /// 终审阻塞 ⑥ 的回归:**「读不到状态」不等于「没有状态」**。
    ///
    /// 上一版用 `!path.exists()` 做降级判据,而 `Path::exists()` 会把 `PermissionDenied` 之类的
    /// metadata 错误一律吞成 `false` —— 于是三重失败(写不进、删不掉、读不到)时函数反而返回
    /// `Ok(())`,fail-stop 被静默绕过,带着残留的 `closed` 放行 provider。改用 `try_exists()` 后
    /// 只有确知不存在才降级。
    ///
    /// 这里用**真实的权限拒绝**构造,不 mock:把 `runtime/chats` 整个目录设成 0o000,写入、
    /// 删除、metadata 三样一起失败。
    #[test]
    #[cfg(unix)]
    fn mark_open_fails_stop_when_state_cannot_even_be_read() {
        use std::os::unix::fs::PermissionsExt;
        let (agent, _tmp) = crate::test_support::agent_dir("claude", "");
        let chats = agent.state_dir().join("runtime").join("chats");
        std::fs::create_dir_all(&chats).unwrap();
        mark_closed(&agent, "g1");
        assert_eq!(read(&agent, "g1"), ChatLifecycle::Closed, "先造出真实残留");

        let restore = |mode: u32| {
            let mut perms = std::fs::metadata(&chats).unwrap().permissions();
            perms.set_mode(mode);
            std::fs::set_permissions(&chats, perms).unwrap();
        };
        restore(0o000);
        // root 会绕过权限检查,那种环境下这个故障构造不出来 —— 跳过而不是误判。
        // 探测必须趁权限还是 0o000 时做:此时仍读得到状态文件的只有 root;
        // 恢复权限之后谁都读得到,条件恒真,root 下会走到 expect_err 反而挂掉。
        if std::fs::read_to_string(path(&agent, "g1")).is_ok() {
            restore(0o755);
            return;
        }
        let outcome = mark_open(&agent, "g1");
        // 先恢复权限,保证断言失败时 TempDir 仍能清理。
        restore(0o755);

        let err = outcome.expect_err("读不到状态时必须 fail-stop,不能当作『没有状态』放行");
        assert!(err.contains("g1"), "错误要带 tag 便于定位: {err}");
    }

    /// 写失败但残留能删掉 → 降级为 `Unknown` 并放行(判活退回旧判据,等价于本改动前的行为)。
    /// 这一档不该 fail-stop:没有残留状态就不会误判,硬拦反而白白拒掉用户的消息。
    #[test]
    fn mark_open_downgrades_to_unknown_when_no_stale_state_blocks_it() {
        let (agent, _tmp) = crate::test_support::agent_dir("claude", "");
        // 没有任何残留时,写入正常成功,更谈不上降级 —— 这里钉的是"成功路径不受 fail-stop 影响"。
        mark_open(&agent, "g9").expect("干净磁盘上必须成功");
        assert_eq!(read(&agent, "g9"), ChatLifecycle::Open);
    }

    /// 两个 tag 互不干扰(文件名按 tag 分)。
    #[test]
    fn state_is_per_chat_tag() {
        let (agent, _tmp) = crate::test_support::agent_dir("claude", "");
        mark_closed(&agent, "g1");
        assert_eq!(read(&agent, "g2"), ChatLifecycle::Unknown);
        assert_eq!(read(&agent, "g1"), ChatLifecycle::Closed);
    }
}
