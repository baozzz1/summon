//! `/summon-export`:导出本会话的排查材料,结果只私聊发起人。判权与 `/term`、`/dash` 同一口径(发起人过
//! 该 agent 在这个 chat 上的 operate-gate);dispatcher 就地处理,不排在在跑的那一轮后面 —— 最需要
//! 导出的时候往往正是那一轮卡住的时候。读盘、打包要一两秒,放到独立线程里做,不挡 dispatcher。
//!
//! 诊断包(不含对话内容)作为飞书文件私聊发出,用户可以直接转给维护者;完整包不上传飞书,私聊一张
//! 本机文件预览服务的下载卡,对话内容留在本机。群里发的命令不在群里应答,私聊本身发不出去时才在
//! 群里说一句原因。设计见 `docs/spec/2026-10-29-session-export-design.md`「交付」。

use std::path::Path;
use std::sync::Arc;

use summon_core::driver::slash_pool::{self, SlashBlacklist, SlashPlan, SlashToggles};
use summon_core::driver::CliDriver;
use summon_core::export::{Mode, WindowSpec, DEFAULT_TURNS};
use summon_core::registry::Agent;

use super::export;
use crate::frontend::client::FeishuClient;
use crate::frontend::outbound::{
    send_card_to_open_id_redacted_archived_id, send_file_to_open_id_redacted_archived,
    send_markdown_archived_anchored, send_markdown_to_open_id_archived,
};
use crate::observe::log::log;
use crate::runtime::preview_server::PreviewServer;

/// 无权限时说给发起人的话,与 `/term`、`/dash` 同一句式。
pub(super) const EXPORT_DENIED: &str = "r61255bda2d";
const EXPORT_SEND_FAILED: &str = "r83c96ace64";
const EXPORT_BUSY: &str = "redb023f002";

/// 正在导出的会话(`agent/chat_id`)。同一个会话同时只跑一份:每次导出都要通读日志、起几个子进程,
/// 连发不该把负载叠上去。
static RUNNING: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

/// 占住一个会话的导出;已被占住返回 `None`。守卫 drop 即释放。
struct Running(String);

impl Running {
    fn claim(key: String) -> Option<Running> {
        let mut running = RUNNING.lock().unwrap_or_else(|e| e.into_inner());
        if running.contains(&key) {
            return None;
        }
        running.push(key.clone());
        Some(Running(key))
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        let mut running = RUNNING.lock().unwrap_or_else(|e| e.into_inner());
        running.retain(|k| k != &self.0);
    }
}

/// exact `/summon-export` → 诊断包,exact `/summon-export full` → 完整包。判定与 chat 线程共用 `plan_slash`:
/// 同形变体与别的参数由规划器拒绝,落进 worker 回一句用法。
pub(super) fn export_mode(
    agent: &Agent,
    text: &str,
    mentions: &[summon_core::ipc::Mention],
    blacklist: &SlashBlacklist,
) -> Option<Mode> {
    let cli = CliDriver::from_kind(agent.cli_kind());
    let command = summon_core::ipc::slash_command_text(text, mentions, cli)?;
    match slash_pool::plan_slash(
        cli,
        agent.capture_mode(),
        &command,
        blacklist,
        &SlashToggles::default(),
    ) {
        SlashPlan::Export { full: true } => Some(Mode::Full),
        SlashPlan::Export { full: false } => Some(Mode::Diagnostic),
        _ => None,
    }
}

/// 一次已过 operate-gate 的 `/summon-export`。身份全部来自飞书入站事件。
pub(super) struct ExportJob {
    pub agent: Agent,
    pub chat_id: String,
    pub chat_type: String,
    pub sender_open_id: String,
    /// 群里发的命令:私聊发不出去时回到群里说一句,挂在这条消息下面。
    pub anchor: Option<String>,
    pub mode: Mode,
}

/// 结果交到发起人手里的方式。
#[derive(Debug, Clone, PartialEq, Eq)]
enum Handoff {
    /// 诊断包作为飞书文件私聊发出。
    File,
    /// 完整包:私聊一张下载卡。
    Link {
        url: String,
        label: String,
        ttl_secs: u64,
    },
    /// 完整包拿不到下载链接(预览服务没起、出不了可路由地址、超过预览大小上限):只告诉他包在
    /// 本机哪里。
    LocalOnly,
}

/// 包的体积上限:诊断包要能作为飞书文件发出去;完整包要能经预览服务下载。
fn max_bytes(mode: Mode, preview_max: Option<u64>) -> u64 {
    match (mode, preview_max) {
        (Mode::Full, Some(limit)) => limit.min(export::DEFAULT_MAX_BYTES),
        _ => export::DEFAULT_MAX_BYTES,
    }
}

/// 完整包怎么交:签得出下载链接就给链接,否则只给本机路径。
fn full_handoff(previews: Option<&PreviewServer>, path: &Path) -> Handoff {
    let link = previews.and_then(|p| {
        let locator = p.locator_token(&path.to_string_lossy(), None)?;
        p.open_link(&locator)
    });
    match link {
        Some(link) => Handoff::Link {
            url: link.url,
            label: link.label,
            ttl_secs: link.ttl_secs,
        },
        None => Handoff::LocalOnly,
    }
}

/// 在独立线程里导出,结果私聊发起人。
pub(super) fn spawn(fc: Arc<FeishuClient>, previews: Option<Arc<PreviewServer>>, job: ExportJob) {
    let name = job.agent.name.clone();
    let Some(claim) = Running::claim(format!("{name}/{}", job.chat_id)) else {
        if let Err(error) = send_markdown_to_open_id_archived(
            &fc,
            &job.agent.state_dir(),
            &job.sender_open_id,
            "export_notice",
            ::summon_core::i18n::label(EXPORT_BUSY),
        ) {
            log(&summon_i18n::format!("r603d07f7c2", job.chat_id));
        }
        return;
    };
    let spawned = std::thread::Builder::new()
        .name(format!("export-{name}"))
        .spawn(move || {
            let _claim = claim;
            run(&fc, previews.as_deref(), &job)
        });
    if let Err(error) = spawned {
        log(&summon_i18n::format!("r086910ebdd"));
    }
}

fn run(fc: &FeishuClient, previews: Option<&PreviewServer>, job: &ExportJob) {
    let agent = &job.agent;
    let state = agent.state_dir();
    let tag = summon_core::registry::chat_tag(&job.chat_type, &job.chat_id);
    let target = export::target_for(agent, &tag);
    let names: Vec<String> = summon_core::registry::Registry::load_tolerant(
        &summon_core::config::Config::load().agents_dir,
    )
    .map(|(registry, _)| registry.agents.iter().map(|a| a.name.clone()).collect())
    .unwrap_or_else(|_| vec![agent.name.clone()]);
    let req = export::Request {
        mode: job.mode,
        window: WindowSpec::Turns(DEFAULT_TURNS),
        max_bytes: max_bytes(job.mode, previews.map(PreviewServer::max_file_bytes)),
    };
    let home = dirs::home_dir()
        .map(|h| h.to_string_lossy().into_owned())
        .unwrap_or_default();
    let dm_note = |text: &str| {
        send_markdown_to_open_id_archived(fc, &state, &job.sender_open_id, "export_notice", text)
    };
    let sent = match export::export(&target, &names, &req) {
        Err(error) => {
            log(&summon_i18n::format!(
                "r1e892a8a96",
                agent.name,
                job.chat_id
            ));
            dm_note(&summon_i18n::format!("rd5342e9fa5"))
        }
        Ok(bundle) => {
            let summary = export::summary(&bundle, &home);
            let handoff = match job.mode {
                Mode::Diagnostic => Handoff::File,
                Mode::Full => full_handoff(previews, &bundle.path),
            };
            log(&summon_i18n::format!(
                "rb322efd01c",
                agent.name,
                job.chat_id,
                job.mode,
                match &handoff {
                    Handoff::File => "file",
                    Handoff::Link { .. } => "link",
                    Handoff::LocalOnly => "local",
                }
            ));
            deliver(
                fc,
                &state,
                &job.sender_open_id,
                &bundle.path,
                &summary,
                handoff,
            )
        }
    };
    if let Err(error) = sent {
        log(&summon_i18n::format!(
            "rdc9f785c6e",
            agent.name,
            job.chat_id,
            job.sender_open_id
        ));
        let _ = send_markdown_archived_anchored(
            fc,
            &state,
            &job.chat_type,
            &job.chat_id,
            job.anchor.as_deref(),
            ::summon_core::i18n::label(EXPORT_SEND_FAILED),
        );
    }
}

fn deliver(
    fc: &FeishuClient,
    state: &Path,
    open_id: &str,
    path: &Path,
    summary: &str,
    handoff: Handoff,
) -> anyhow::Result<()> {
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    match handoff {
        Handoff::File => {
            send_markdown_to_open_id_archived(fc, state, open_id, "export_notice", summary)?;
            let bytes = std::fs::read(path)?;
            send_file_to_open_id_redacted_archived(
                fc,
                state,
                open_id,
                "export_file",
                bytes,
                &file_name,
            )
            .map(drop)
        }
        Handoff::Link {
            url,
            label,
            ttl_secs,
        } => {
            send_markdown_to_open_id_archived(fc, state, open_id, "export_notice", summary)?;
            send_card_to_open_id_redacted_archived_id(
                fc,
                state,
                open_id,
                "export_link",
                &summon_core::feishu::card_preview_link(&label, false, &url, ttl_secs),
            )
            .map(drop)
        }
        Handoff::LocalOnly => send_markdown_to_open_id_archived(
            fc,
            state,
            open_id,
            "export_notice",
            &summon_i18n::format!("r763076e540"),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(cli: &str) -> Agent {
        serde_yaml::from_str(&format!("name: a\nruntime:\n  cli: {cli}\n")).unwrap()
    }

    #[test]
    fn export_and_export_full_pick_the_two_modes() {
        for cli in ["claude", "codex", "cursor"] {
            let a = agent(cli);
            let blacklist = SlashBlacklist::default();
            assert_eq!(
                export_mode(&a, "/summon-export", &[], &blacklist),
                Some(Mode::Diagnostic)
            );
            assert_eq!(
                export_mode(&a, "/summon-export full", &[], &blacklist),
                Some(Mode::Full)
            );
            for text in ["/summon-export x", "/SUMMON-EXPORT", "/dash", "export"] {
                assert_eq!(export_mode(&a, text, &[], &blacklist), None, "{cli} {text}");
            }
        }
    }

    #[test]
    fn full_bundles_stay_within_what_the_preview_service_serves() {
        assert_eq!(
            max_bytes(Mode::Diagnostic, Some(1)),
            export::DEFAULT_MAX_BYTES
        );
        assert_eq!(max_bytes(Mode::Full, Some(10 << 20)), 10 << 20);
        assert_eq!(
            max_bytes(Mode::Full, Some(u64::MAX)),
            export::DEFAULT_MAX_BYTES
        );
        assert_eq!(max_bytes(Mode::Full, None), export::DEFAULT_MAX_BYTES);
    }

    #[test]
    fn one_export_per_session_at_a_time() {
        let first = Running::claim("a/oc_claim_test".into()).expect("空闲时能占住");
        assert!(
            Running::claim("a/oc_claim_test".into()).is_none(),
            "同一会话不并发"
        );
        assert!(
            Running::claim("a/oc_claim_other".into()).is_some(),
            "别的会话不受影响"
        );
        drop(first);
        assert!(
            Running::claim("a/oc_claim_test".into()).is_some(),
            "跑完即释放"
        );
    }

    #[test]
    fn without_a_preview_service_the_full_bundle_stays_local() {
        assert_eq!(
            full_handoff(None, Path::new("/tmp/x.tar.gz")),
            Handoff::LocalOnly
        );
    }
}
