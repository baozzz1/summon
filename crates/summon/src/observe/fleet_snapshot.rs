//! fleet 快照的**组装**(盘上会话发现 + 运行时真值 → [`FleetSnapshot`])。
//!
//! 为什么单独成一处:这段逻辑原本长在 `cli.rs` 的 `agents_status` 里,只有 CLI 够得着。
//! 飞书 steward 跑在 daemon 内,要同一份快照 —— 而在 daemon 里抄第二份组装,正是
//! 2026-08-09 刚把三态推导下沉时消除的那个毛病(多份手抄、漂了没人发现)。
//!
//! **`runtime` 由调用方给**,这是刻意的:CLI 经 control.sock 问 daemon 要,daemon 自己直接
//! 读内存(`observe::runtime_status::snapshot`)。把取法收进来的话,daemon 会绕一圈问自己。
//! 传 `None` = 拿不到运行时真值 → 降级成纯盘上事实(busy 全 false),这是天然降级不是补丁。

use std::time::SystemTime;

use summon_core::config::Config;
use summon_core::fleet::{build_fleet, FleetSnapshot, RuntimeStatus};
use summon_core::registry::Registry;

/// sentinel 三态徽标:Rust 渲染词,各展示端只显示(与 `last_active_text` 同哲学)。
pub fn sentinel_mode(s: &summon_core::config::Sentinel) -> &'static str {
    if !s.enabled {
        "off"
    } else if s.dry_run {
        "dry-run"
    } else {
        "on"
    }
}

/// 该 registry 下所有 agent 的活跃 tmux 会话名(去重排序)。
pub fn live_sessions_for_agents(reg: &Registry) -> Vec<String> {
    let all = crate::session::tmux::list_all_sessions();
    let mut sessions: Vec<String> = reg
        .agents
        .iter()
        .flat_map(|agent| crate::session::tmux::filter_names(&all, &agent.tmux_name()))
        .collect();
    sessions.sort();
    sessions.dedup();
    sessions
}

/// CLI 侧的运行时真值:经 control.sock 问 daemon。拿不到(没起 / 拒了 / 应答坏了)一律 `None`
/// = 降级,不是错误。
pub fn fetch_runtime_status() -> Option<RuntimeStatus> {
    use summon_core::core_port::{Actor, Command, CommandOutcome, Intent};
    let cmd = Command {
        actor: Actor::Owner,
        intent: Intent::RuntimeStatus,
    };
    match crate::runtime::daemon::submit_command(&cmd) {
        Ok(CommandOutcome::Value(v)) => serde_json::from_value(v).ok(),
        _ => None,
    }
}

/// 组装一份完整快照(含 sentinel 徽标)。
///
/// **steward 身份被排除在列表之外**:它没有也不该有会话,列进去恒为「未启动/已退出」,
/// 而它恰恰是正在看这份列表的那个身份 —— 纯噪音。
pub fn build(
    cfg: &Config,
    reg: &Registry,
    runtime: Option<&RuntimeStatus>,
    now: SystemTime,
) -> FleetSnapshot {
    let live_sessions = live_sessions_for_agents(reg);
    let sessions: Vec<(String, Vec<summon_core::session_view::SessionView>)> = reg
        .agents
        .iter()
        .filter(|agent| !agent.is_steward())
        .map(|agent| {
            let ttl = crate::runtime::daemon::resolve_messaging(agent).idle_ttl();
            (
                agent.name.clone(),
                crate::session::discovery::discover_sessions(agent, &live_sessions, now, ttl),
            )
        })
        .collect();
    let marks: Vec<(String, String, summon_core::waiting::WaitingMark)> = reg
        .agents
        .iter()
        .filter(|agent| !agent.is_steward())
        .flat_map(|agent| {
            crate::runtime::waiting_store::list(&agent.state_dir())
                .into_iter()
                .map(move |(tag, mark)| (agent.name.clone(), tag, mark))
        })
        .collect();
    let mut snapshot = build_fleet(&sessions, runtime, now);
    let now_unix = now
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // `sessions` 是会话发现的原样结果(含已退出的):agent 自己归档后 waiting 仍要列出名字。
    summon_core::waiting::attach_waiting(&mut snapshot, &sessions, &marks, now_unix);
    summon_core::waiting::attach_group_links(
        &mut snapshot,
        summon_core::waiting::applink_host(&cfg.default_domain),
    );
    let comment_agents = reg
        .agents
        .iter()
        .filter_map(|agent| {
            crate::runtime::daemon::resolve_creds(agent).0.map(|app| {
                crate::runtime::document_comments::TaskIdentity {
                    agent: agent.name.clone(),
                    app,
                    epoch: crate::runtime::daemon::feishu_identity_fingerprint(agent),
                    owner: agent
                        .feishu
                        .as_ref()
                        .and_then(|f| f.allowed_users.first())
                        .cloned()
                        .unwrap_or_default(),
                }
            })
        })
        .collect::<Vec<_>>();
    match crate::runtime::document_comments::task_snapshot(
        &crate::runtime::document_comments::Store::at_run_dir(),
        &comment_agents,
        runtime.map(|r| r.pid),
        i64::try_from(now_unix).unwrap_or(i64::MAX / 1000) * 1000,
    ) {
        Ok(tasks) => snapshot.comment_tasks = tasks,
        Err(_) => snapshot.comment_tasks_error = Some(summon_i18n::text!("rf7868d504f").into()),
    }
    snapshot.sentinel = Some(sentinel_mode(&cfg.sentinel).to_string());
    snapshot.menubar_icon =
        Some(summon_core::menubar::resolve_menubar_icon(cfg.menubar.icon.as_deref()).to_string());
    snapshot.menubar_show_counts = Some(cfg.menubar.show_counts());
    snapshot
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_with(enabled: bool, dry_run: bool) -> summon_core::config::Sentinel {
        summon_core::config::Sentinel {
            enabled,
            dry_run,
            ..Default::default()
        }
    }

    /// 三态徽标的词就三个,别处只显示不推导。
    #[test]
    fn sentinel徽标三态() {
        assert_eq!(sentinel_mode(&cfg_with(false, false)), "off");
        assert_eq!(sentinel_mode(&cfg_with(false, true)), "off", "关着就是关着");
        assert_eq!(sentinel_mode(&cfg_with(true, true)), "dry-run");
        assert_eq!(sentinel_mode(&cfg_with(true, false)), "on");
    }
}
