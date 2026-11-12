//! `status.*`:顶栏与概览 —— daemon 状态、agent 一览、各文件版本(页面每 15 秒取一次,发现变化
//! 就刷新或提示),以及「需要处理」的配置问题清单。
//!
//! 「需要处理」不另写判据:配置解析、未知键与废弃键来自原文校验(`raw_check`),地址与 Codex 策略
//! 来自 doctor 的同一组检查(`setup::config_findings`),需要重启的改动来自启动存档。

use std::collections::BTreeMap;

use serde_json::Value;
use summon_core::dashboard::{raw_check, RawKind};

use super::types::{DaemonInfo, Issue, IssueLink, IssuePage, Severity, StatusView};
use super::{reply, rfc3339, ApiEnv, Loaded};
use crate::runtime::dashboard::server::{ApiCtx, ApiError};
use crate::runtime::dashboard::writer;

/// 各文件的版本:`global`、`agent:<alias>`、`chat:<alias>:<chat_id>`、`schedules`、`doc:<路径>`。
fn revisions(env: &ApiEnv) -> BTreeMap<String, String> {
    let read = |path: &std::path::Path| writer::revision(std::fs::read(path).ok().as_deref());
    let mut out = BTreeMap::new();
    out.insert("global".to_string(), read(&env.config_path()));
    let mut schedules = Vec::new();
    for name in ["schedules.yaml", "schedules-archive.yaml"] {
        schedules.extend(std::fs::read(env.home.join(name)).unwrap_or_default());
        schedules.push(0);
    }
    out.insert("schedules".to_string(), writer::revision(Some(&schedules)));
    let global_doc = env.home.join("CLAUDE.md");
    out.insert(super::docs::revision_key(&global_doc), read(&global_doc));
    for loaded in env.scan() {
        match loaded {
            Loaded::Broken { alias, .. } => {
                out.insert(
                    format!("agent:{alias}"),
                    read(&env.agents_dir.join(&alias).join("agent.yaml")),
                );
            }
            Loaded::Ok(agent) => {
                out.insert(
                    format!("agent:{}", agent.name),
                    read(&agent.manifest_path()),
                );
                for name in ["CLAUDE.md", "SOUL.md"] {
                    let path = agent.dir.join(name);
                    out.insert(super::docs::revision_key(&path), read(&path));
                }
                // 按群开关:每个盘上记过的会话一份(同一个群多份时取最新那份,与群页面一致)。
                let dir = agent.state_dir().join("runtime").join("chats");
                let mut latest: BTreeMap<String, (Option<String>, String)> = BTreeMap::new();
                for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
                    let path = entry.path();
                    let Some(session) = path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .and_then(|n| n.strip_suffix(".meta"))
                    else {
                        continue;
                    };
                    let meta = summon_core::chat_meta::ChatMeta::parse(
                        &std::fs::read_to_string(&path).unwrap_or_default(),
                    );
                    let Some(chat_id) = meta.chat_id else {
                        continue;
                    };
                    let newer = latest
                        .get(&chat_id)
                        .is_none_or(|(at, _)| *at <= meta.updated_at);
                    if newer {
                        latest.insert(chat_id, (meta.updated_at, session.to_string()));
                    }
                }
                for (chat_id, (_, session)) in latest {
                    let settings = crate::runtime::daemon::dashboard_port::chat_settings_path(
                        &agent, &session,
                    );
                    out.insert(format!("chat:{}:{chat_id}", agent.name), read(&settings));
                    if let Some(doc) = super::chats::chat_doc_path(&agent, &chat_id) {
                        if doc.is_file() {
                            out.insert(super::docs::revision_key(&doc), read(&doc));
                        }
                    }
                }
            }
        }
    }
    out
}

pub(crate) fn get(ctx: &ApiCtx, _p: Value) -> Result<Value, ApiError> {
    let env = ctx.env()?;
    reply(&StatusView {
        language: summon_core::i18n::language(),
        daemon: DaemonInfo {
            version: crate::version::VERSION.to_string(),
            commit: crate::version::COMMIT.to_string(),
            started_at: rfc3339(env.started_at),
            pid: std::process::id(),
            autostart: env.port.autostart(),
        },
        hostname: ctx.hostname.clone(),
        restart_pending: env.restart_pending(),
        agents: super::agents::summaries(env),
        revisions: revisions(env),
    })
}

fn link(page: IssuePage, agent: Option<&str>, key: Option<&str>) -> Option<IssueLink> {
    Some(IssueLink {
        page,
        agent: agent.map(str::to_string),
        chat_id: None,
        key: key.map(str::to_string),
    })
}

/// 原文校验的错误与警告 → 条目。
fn raw_issues(kind: RawKind, text: &str, agent: Option<&str>, out: &mut Vec<Issue>) {
    let check = raw_check::check(kind, text, text);
    let (page, file) = match agent {
        Some(alias) => (IssuePage::Agent, format!("agents/{alias}/agent.yaml")),
        None => (IssuePage::Global, "config.yaml".to_string()),
    };
    for (severity, issue) in check
        .errors
        .iter()
        .map(|i| (Severity::Error, i))
        .chain(check.warnings.iter().map(|i| (Severity::Warn, i)))
    {
        let at = match (issue.line, &issue.key) {
            (Some(line), Some(key)) => summon_i18n::format!("r6f44345b1c"),
            (Some(line), None) => summon_i18n::format!("r62d2a2f659"),
            (None, Some(key)) => summon_i18n::format!("r43370d967e"),
            (None, None) => file.clone(),
        };
        out.push(Issue {
            severity,
            title: match severity {
                Severity::Error => summon_i18n::format!("r91b61e7b88"),
                Severity::Warn => summon_i18n::format!("r7e9797204e"),
            },
            detail: format!("{at}：{}", issue.message),
            fix: match &issue.suggestion {
                Some(s) => summon_i18n::format!("r98fb86791e"),
                None => summon_i18n::text!("rd657fe5284").to_string(),
            },
            link: link(page, agent, issue.key.as_deref()),
        });
    }
}

pub(crate) fn issues_of(env: &ApiEnv) -> Result<Vec<Issue>, ApiError> {
    let cfg = env.read_config()?;
    let mut out = Vec::new();
    if let Some(error) = &cfg.error {
        out.push(Issue {
            severity: Severity::Error,
            title: summon_i18n::text!("redc3713257").into(),
            detail: error.clone(),
            fix: summon_i18n::text!("rd70b84dd28").into(),
            link: link(IssuePage::Global, None, None),
        });
    }
    if let Some(text) = &cfg.text {
        raw_issues(RawKind::Config, text, None, &mut out);
    }
    let mut agents = Vec::new();
    for loaded in env.scan() {
        match loaded {
            Loaded::Broken { alias, error } => out.push(Issue {
                severity: Severity::Error,
                title: summon_i18n::format!("rfcc34b31a5"),
                detail: error,
                fix: summon_i18n::text!("rc36dac8752").into(),
                link: link(IssuePage::Agent, Some(&alias), None),
            }),
            Loaded::Ok(agent) => {
                if let Ok(text) = std::fs::read_to_string(agent.manifest_path()) {
                    raw_issues(RawKind::Agent, &text, Some(&agent.name), &mut out);
                }
                if agent.is_routable() {
                    let (id, secret) = crate::runtime::daemon::resolve_creds(&agent);
                    if id.is_none() || secret.is_none() {
                        out.push(Issue {
                            severity: Severity::Error,
                            title: summon_i18n::format!("r82904dfbaf", agent.name),
                            detail: summon_i18n::text!("r4f9805106f").into(),
                            fix: summon_i18n::format!(
                                "r23cbe09807",
                                super::shown_path(&agent.dir),
                                agent.name
                            ),
                            link: link(
                                IssuePage::Agent,
                                Some(&agent.name),
                                Some("feishu.app_id_env"),
                            ),
                        });
                    }
                }
                agents.push(*agent);
            }
        }
    }
    let routable: Vec<&summon_core::registry::Agent> =
        agents.iter().filter(|a| a.is_routable()).collect();
    let probe = env.port.public_host();
    for finding in crate::setup::config_findings::config_findings(&cfg.parsed, &routable, probe) {
        let view = finding.view();
        out.push(Issue {
            severity: if view.error {
                Severity::Error
            } else {
                Severity::Warn
            },
            title: view.title,
            detail: view.detail,
            fix: view.fix,
            link: match &view.agent {
                Some(alias) => link(IssuePage::Agent, Some(alias), view.key),
                None => link(IssuePage::Global, None, view.key),
            },
        });
    }
    let pending = env.restart_pending();
    if let Some(first) = pending.first() {
        let keys: Vec<String> = pending
            .iter()
            .map(|item| format!("{}({})", item.key, item.file))
            .collect();
        out.push(Issue {
            severity: Severity::Warn,
            title: summon_i18n::format!("ra433fdffa8", pending.len()),
            detail: keys.join("、"),
            fix: summon_i18n::text!("rcef28d7aa6").into(),
            link: match first.file.strip_prefix("agent:") {
                Some(alias) => link(IssuePage::Agent, Some(alias), Some(&first.key)),
                None => link(IssuePage::Global, None, Some(&first.key)),
            },
        });
    }
    Ok(out)
}

/// 最近 24 小时失败的轮:汇成一条,链到最近一次失败的那个会话。只报类别,不带对话内容。
fn failed_turns_issue(env: &ApiEnv, ctx: &ApiCtx) -> Option<Issue> {
    use summon_core::turn_ledger::TurnEnd;
    let now_ms = ctx.now.timestamp_millis();
    let failed: Vec<summon_core::turn_ledger::TurnRecord> =
        crate::runtime::turn_ledger::read_range(
            &crate::runtime::turn_ledger::dir_in(&env.run_dir()),
            None,
            now_ms - 24 * 3600 * 1000,
            now_ms + 1,
        )
        .into_iter()
        .filter(|r| r.end == TurnEnd::Failed)
        .collect();
    let latest = failed.first()?;
    let sessions: std::collections::HashSet<(&str, &str)> = failed
        .iter()
        .map(|r| (r.agent.as_str(), r.tag.as_str()))
        .collect();
    let agent = env.agent(&latest.agent).ok();
    let who = agent
        .as_ref()
        .map(|a| a.display.clone())
        .filter(|d| !d.is_empty())
        .unwrap_or_else(|| latest.agent.clone());
    let chat = agent
        .as_ref()
        .and_then(|a| super::chats::chat_name_of(a, &latest.chat_id))
        .unwrap_or_else(|| latest.chat_id.clone());
    let at = chrono::TimeZone::timestamp_millis_opt(&chrono::Local, latest.ended_at_ms)
        .single()
        .map(|t| t.format("%m-%d %H:%M").to_string())
        .unwrap_or_default();
    let kind = latest
        .error
        .as_deref()
        .map(|e| summon_i18n::format!("r6a79d38f67"))
        .unwrap_or_default();
    Some(Issue {
        severity: Severity::Warn,
        title: summon_i18n::format!("rfb9eee8c90", failed.len()),
        detail: summon_i18n::format!("r93b8e4b453", sessions.len()),
        fix: summon_i18n::text!("r627c1c2f10").into(),
        link: Some(IssueLink {
            page: IssuePage::Session,
            agent: Some(latest.agent.clone()),
            chat_id: Some(latest.chat_id.clone()),
            key: None,
        }),
    })
}

pub(crate) fn issues(ctx: &ApiCtx, _p: Value) -> Result<Value, ApiError> {
    let env = ctx.env()?;
    let mut out = issues_of(env)?;
    out.extend(failed_turns_issue(env, ctx));
    reply(&out)
}

#[cfg(test)]
mod tests {
    use super::super::testkit::*;
    use super::*;
    use serde_json::json;

    #[test]
    fn failed_turns_in_the_last_day_become_one_issue_linking_the_latest_session() {
        use summon_core::turn_ledger::{TurnEnd, TurnOrigin, TurnRecord};
        let kit = Kit::new();
        let a = kit.add_agent("starlight", &agent_yaml("starlight", "星光", "claude", ""));
        kit.add_chat(&a, "group", GROUP, "示例群");
        let ctx = kit.owner();
        let now = ctx.now.timestamp_millis();
        let turn = |ended_at_ms: i64, end: TurnEnd| TurnRecord {
            agent: "starlight".into(),
            chat_id: GROUP.into(),
            tag: summon_core::registry::chat_tag("group", GROUP),
            turn: None,
            origin: TurnOrigin::Human,
            schedule: None,
            requester: None,
            message_id: "om_x".into(),
            prompt_head: "不该进问题清单".into(),
            started_at_ms: ended_at_ms - 1_000,
            ended_at_ms,
            end,
            error: Some("timed_out".into()),
            reattached: false,
            tool_steps: 0,
            cli: None,
            model: None,
            effort: None,
            ctx: None,
            usage: Vec::new(),
            quota: None,
            feed: None,
        };
        let none = kit.call(&ctx, issues, json!({})).unwrap();
        assert!(!none.to_string().contains("轮次失败"));
        let dir = kit.home().join("run/turns/starlight");
        std::fs::create_dir_all(&dir).unwrap();
        let lines: String = [
            turn(now - 25 * 3600 * 1000, TurnEnd::Failed),
            turn(now - 3_600_000, TurnEnd::Failed),
            turn(now - 60_000, TurnEnd::Done),
            turn(now - 30_000, TurnEnd::Failed),
        ]
        .iter()
        .map(|r| serde_json::to_string(r).unwrap() + "\n")
        .collect();
        // 25 小时前那一轮可能在上个月的文件里:这里只看最近一天的两轮,写在当月文件即可。
        std::fs::write(dir.join(summon_core::turn_ledger::file_name(now)), lines).unwrap();
        let out = kit.call(&ctx, issues, json!({})).unwrap();
        let issue = out
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["title"].as_str().unwrap_or("").contains("轮次失败"))
            .unwrap()
            .clone();
        assert_eq!(issue["title"], "最近 24 小时有 2 个轮次失败");
        assert_eq!(issue["link"]["page"], "session");
        assert_eq!(issue["link"]["chat_id"], GROUP);
        assert!(issue["detail"].as_str().unwrap().contains("示例群"));
        assert!(issue["detail"].as_str().unwrap().contains("timed_out"));
        assert!(!out.to_string().contains("不该进问题清单"));
    }

    #[test]
    fn status_reports_agents_and_revisions() {
        let kit = Kit::new();
        let a = kit.add_agent("starlight", &agent_yaml("starlight", "星光", "claude", ""));
        kit.add_chat(&a, "group", GROUP, "示例群");
        let status = kit.call(&kit.owner(), get, json!({})).unwrap();
        assert_eq!(status["daemon"]["autostart"], "on");
        assert_eq!(status["daemon"]["version"], crate::version::VERSION);
        assert_eq!(status["agents"][0]["alias"], "starlight");
        let revs = status["revisions"].as_object().unwrap();
        assert_eq!(
            revs["global"],
            writer::revision(Some(kit.read("config.yaml").as_bytes()))
        );
        assert!(revs.contains_key("agent:starlight"));
        assert_eq!(revs[&format!("chat:starlight:{GROUP}")], "absent");
        assert!(revs.contains_key("schedules"));
        assert!(revs
            .keys()
            .any(|k| k.starts_with("doc:") && k.ends_with("SOUL.md")));
        assert_eq!(status["restart_pending"], json!([]));
    }

    #[test]
    fn issues_cover_unknown_keys_broken_files_and_missing_credentials() {
        let kit = Kit::new();
        std::fs::write(
            kit.home().join("config.yaml"),
            "messaging:\n  cot_visable: true\nquota_recovery:\n  enabled: true\n",
        )
        .unwrap();
        let a = kit.add_agent("starlight", &agent_yaml("starlight", "星光", "claude", ""));
        std::fs::remove_file(a.dir.join(".env")).unwrap();
        let broken = kit.agents_dir().join("saturn");
        std::fs::create_dir_all(&broken).unwrap();
        std::fs::write(broken.join("agent.yaml"), "name: [\n").unwrap();
        let issues = kit.call(&kit.owner(), super::issues, json!({})).unwrap();
        let text = issues.to_string();
        let titles: Vec<&str> = issues
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["title"].as_str().unwrap())
            .collect();
        assert!(text.contains("cot_visible"), "最接近的合法键:{text}");
        assert!(text.contains("quota_recovery"), "废弃段:{text}");
        assert!(titles.iter().any(|t| t.contains("saturn")), "{titles:?}");
        assert!(titles.iter().any(|t| t.contains("凭据缺失")), "{titles:?}");
        assert!(
            titles.iter().any(|t| t.contains("本机打开")),
            "回环监听:{titles:?}"
        );
        let broken = issues
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["title"].as_str().unwrap().contains("saturn"))
            .unwrap();
        assert_eq!(broken["link"], json!({"page": "agent", "agent": "saturn"}));
    }
}
