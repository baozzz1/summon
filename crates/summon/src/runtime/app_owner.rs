//! 飞书应用所有者:daemon 后台查询,落一份盘上缓存,再写进 agent 的共享槽。
//!
//! 判权只读槽(`Agent::can_operate`),这里负责把槽填上。落盘的理由:PreToolUse 执行闸跑在
//! claude 的 hook 进程里,看不到 daemon 内存,而它与 prompt 里的「外部发起人」标记必须对同一个人
//! 给出同一个结论(见 `summon_core::authz::requester_is_external`),只能共用盘上这一份。
//! 缓存绑定写入时的 app_id:凭据换成别的应用后,上一个应用的所有者不算数。读不到、解析不了、
//! 应用对不上都按未知处理,判权随之 fail-closed。

use crate::frontend::client::FeishuClient;
use crate::observe::log::log;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use summon_core::config::registry::{Agent, AppOwner, AppOwnerState};

/// 查询失败后的重试间隔。
const RETRY: Duration = Duration::from_secs(5 * 60);
/// 查到之后的刷新间隔:所有者只会因控制台转让而变。
const REFRESH: Duration = Duration::from_secs(60 * 60);

/// 盘上只记已查到的结论;文件不存在即未知。
#[derive(Debug, Serialize, Deserialize)]
struct Cached {
    app_id: String,
    owner: Option<CachedOwner>,
}

#[derive(Debug, Serialize, Deserialize)]
struct CachedOwner {
    open_id: String,
    union_id: String,
}

fn cache_path(agent: &Agent) -> PathBuf {
    agent.state_dir().join("runtime").join("app-owner.json")
}

/// 按当前凭据的 app_id 读缓存。查询只产出两条 id 都非空的所有者,缓存按同一口径校验。
fn read_cached(agent: &Agent, app_id: &str) -> AppOwnerState {
    let Ok(text) = std::fs::read_to_string(cache_path(agent)) else {
        return AppOwnerState::Unknown;
    };
    let cached = match serde_json::from_str::<Cached>(&text) {
        Ok(cached) => cached,
        Err(error) => {
            log(&summon_i18n::format!("rb746984716", agent.name));
            return AppOwnerState::Unknown;
        }
    };
    if app_id.is_empty() || cached.app_id != app_id {
        return AppOwnerState::Unknown;
    }
    match cached.owner {
        None => AppOwnerState::Known(None),
        Some(owner) if owner.open_id.is_empty() || owner.union_id.is_empty() => {
            AppOwnerState::Unknown
        }
        Some(owner) => AppOwnerState::Known(Some(AppOwner {
            open_id: owner.open_id,
            union_id: owner.union_id,
        })),
    }
}

/// 把盘上缓存装进槽。`app_id` 取当前凭据;没有凭据就是未知。daemon 启动与热接入时各装一次,
/// 刷新完成前判权就能用上上一代查到的结论;hook 进程每次判权前装一次。
pub(crate) fn load_cached(agent: &Agent, app_id: Option<&str>) {
    agent
        .app_owner
        .set(read_cached(agent, app_id.unwrap_or_default()));
}

/// 所有者变化的名字级描述(不打印 id);没有变化返回 None。
fn describe_change(previous: &Option<AppOwner>, next: &Option<AppOwner>) -> Option<&'static str> {
    match (previous, next) {
        (previous, next) if previous == next => None,
        (Some(_), Some(_)) => Some(summon_i18n::text!("rbeb874145a")),
        (Some(_), None) => Some(summon_i18n::text!("r71605f09e3")),
        (None, Some(_)) => Some(summon_i18n::text!("raa805c7843")),
        (None, None) => None,
    }
}

/// 查一次并返回下次查询前的等待时长。先写盘、写盘成功再换槽:写盘失败时 daemon 与 hook 进程
/// 继续用同一份旧结论。查询或写盘失败都保留旧值,只在失败开始和恢复时各记一行。
fn refresh_once(
    agent: &Agent,
    app_id: &str,
    failing: &mut bool,
    fetch: impl FnOnce() -> anyhow::Result<Option<AppOwner>>,
) -> Duration {
    let written = fetch().and_then(|owner| {
        let cached = Cached {
            app_id: app_id.to_string(),
            owner: owner.as_ref().map(|owner| CachedOwner {
                open_id: owner.open_id.clone(),
                union_id: owner.union_id.clone(),
            }),
        };
        crate::runtime::fsio::atomic_write(&cache_path(agent), &serde_json::to_vec(&cached)?)?;
        Ok(owner)
    });
    match written {
        Ok(owner) => {
            if std::mem::take(failing) {
                log(&summon_i18n::format!("rfc73443e5f", agent.name));
            }
            if let AppOwnerState::Known(previous) = agent.app_owner.get() {
                if let Some(change) = describe_change(&previous, &owner) {
                    log(&summon_i18n::format!("r737754436e", agent.name));
                }
            }
            agent.app_owner.set(AppOwnerState::Known(owner));
            REFRESH
        }
        Err(error) => {
            if !std::mem::replace(failing, true) {
                log(&summon_i18n::format!("r7695bf23de", agent.name));
            }
            RETRY
        }
    }
}

/// 给一个 agent 起后台刷新线程。线程随 daemon 进程退出;dry_run 或没有 app_id 时查不了,不起。
pub(crate) fn spawn_refresher(
    agent: Agent,
    fc: Arc<FeishuClient>,
    app_id: Option<String>,
    dry_run: bool,
) {
    let epoch = super::agent_retirement::activate(&agent.dir);
    let Some(app_id) = app_id.filter(|id| !dry_run && !id.is_empty()) else {
        return;
    };
    let name = format!("app-owner-{}", agent.name);
    let spawned = std::thread::Builder::new().name(name).spawn(move || {
        let mut failing = false;
        loop {
            let retired = epoch.read().unwrap_or_else(|e| e.into_inner());
            if *retired {
                break;
            }
            let wait = refresh_once(&agent, &app_id, &mut failing, || fc.app_owner());
            drop(retired);
            std::thread::sleep(wait);
        }
    });
    // 起不来只是少了应用所有者这条放行,判权照旧 fail-closed。
    if let Err(error) = spawned {
        log(&summon_i18n::format!("r79bb8da0d1"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owner() -> AppOwner {
        AppOwner {
            open_id: "ou_owner".into(),
            union_id: "on_owner".into(),
        }
    }

    fn agent() -> (Agent, tempfile::TempDir) {
        crate::test_support::agent_dir("claude", "feishu:\n  app_id_env: A\n")
    }

    /// 另一个进程从盘上装出的结论(与 hook 进程同路径)。
    fn loaded_elsewhere(agent: &Agent, app_id: &str) -> AppOwnerState {
        let (reg, _) =
            summon_core::registry::Registry::load_tolerant(agent.dir.parent().unwrap()).unwrap();
        let loaded = reg
            .agents
            .into_iter()
            .find(|a| a.name == agent.name)
            .unwrap();
        assert_eq!(loaded.app_owner.get(), AppOwnerState::Unknown);
        load_cached(&loaded, Some(app_id));
        loaded.app_owner.get()
    }

    #[test]
    fn refresh_writes_cache_then_slot_and_keeps_value_on_failure() {
        let (agent, _root) = agent();
        let mut failing = false;
        let wait = refresh_once(&agent, "cli_x", &mut failing, || Ok(Some(owner())));
        assert_eq!(wait, REFRESH);
        assert_eq!(agent.app_owner.get(), AppOwnerState::Known(Some(owner())));
        assert_eq!(
            loaded_elsewhere(&agent, "cli_x"),
            AppOwnerState::Known(Some(owner()))
        );

        // 查询失败保留旧值,短间隔重试。
        let wait = refresh_once(&agent, "cli_x", &mut failing, || anyhow::bail!("network"));
        assert_eq!(wait, RETRY);
        assert!(failing);
        assert_eq!(agent.app_owner.get(), AppOwnerState::Known(Some(owner())));

        // 所有者不是企业成员也是查到的结论:写成 None。
        assert_eq!(
            refresh_once(&agent, "cli_x", &mut failing, || Ok(None)),
            REFRESH
        );
        assert!(!failing);
        assert_eq!(
            loaded_elsewhere(&agent, "cli_x"),
            AppOwnerState::Known(None)
        );
    }

    #[test]
    fn refresh_keeps_slot_when_cache_cannot_be_written() {
        let (agent, _root) = agent();
        let mut failing = false;
        refresh_once(&agent, "cli_x", &mut failing, || Ok(Some(owner())));
        // 缓存路径被目录占住,写盘必然失败:槽与盘上都还是旧结论。
        let path = cache_path(&agent);
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir_all(path.join("blocker")).unwrap();
        let wait = refresh_once(&agent, "cli_x", &mut failing, || Ok(None));
        assert_eq!(wait, RETRY);
        assert!(failing);
        assert_eq!(agent.app_owner.get(), AppOwnerState::Known(Some(owner())));
    }

    #[test]
    fn cache_from_another_app_or_with_an_empty_id_is_unknown() {
        let (agent, _root) = agent();
        let mut failing = false;
        refresh_once(&agent, "cli_x", &mut failing, || Ok(Some(owner())));
        assert_eq!(loaded_elsewhere(&agent, "cli_y"), AppOwnerState::Unknown);
        load_cached(&agent, None);
        assert_eq!(
            agent.app_owner.get(),
            AppOwnerState::Unknown,
            "没有凭据就是未知"
        );

        std::fs::write(
            cache_path(&agent),
            r#"{"app_id":"","owner":{"open_id":"ou_owner","union_id":"on_owner"}}"#,
        )
        .unwrap();
        load_cached(&agent, None);
        assert_eq!(
            agent.app_owner.get(),
            AppOwnerState::Unknown,
            "空 app_id 的缓存不对应任何凭据"
        );

        for owner in [
            r#"{"open_id":"ou_owner","union_id":""}"#,
            r#"{"open_id":"","union_id":"on_owner"}"#,
        ] {
            std::fs::write(
                cache_path(&agent),
                format!(r#"{{"app_id":"cli_x","owner":{owner}}}"#),
            )
            .unwrap();
            assert_eq!(
                loaded_elsewhere(&agent, "cli_x"),
                AppOwnerState::Unknown,
                "{owner}"
            );
        }
    }

    #[test]
    fn unreadable_cache_leaves_owner_unknown() {
        let (agent, _root) = agent();
        load_cached(&agent, Some("cli_x"));
        assert_eq!(agent.app_owner.get(), AppOwnerState::Unknown);
        std::fs::create_dir_all(cache_path(&agent).parent().unwrap()).unwrap();
        for text in [
            "not json",
            r#"{"owner":{"open_id":"ou_owner","union_id":"on_owner"}}"#,
        ] {
            std::fs::write(cache_path(&agent), text).unwrap();
            load_cached(&agent, Some("cli_x"));
            assert_eq!(agent.app_owner.get(), AppOwnerState::Unknown, "{text}");
        }
    }

    #[test]
    fn owner_changes_are_described_without_ids() {
        let other = AppOwner {
            open_id: "ou_b".into(),
            union_id: "on_b".into(),
        };
        assert_eq!(describe_change(&Some(owner()), &Some(owner())), None);
        assert_eq!(describe_change(&None, &None), None);
        assert_eq!(
            describe_change(&Some(owner()), &Some(other)),
            Some("换成了另一位企业成员")
        );
        assert_eq!(
            describe_change(&Some(owner()), &None),
            Some("由企业成员变为非企业成员")
        );
        assert_eq!(
            describe_change(&None, &Some(owner())),
            Some("由非企业成员变为企业成员")
        );
    }
}
