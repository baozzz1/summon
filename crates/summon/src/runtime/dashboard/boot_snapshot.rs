//! daemon 启动时实际用的那几项配置的存档,用来判断「已保存、但要重启 daemon 才生效」。
//!
//! 盘上没有 daemon 启动时配置的副本(boot 里的 `Config` 是局部变量),所以启动时登记一份。
//! 登记哪些项不在这里列:字段表里生效时机是「重启」的就是 —— 加一个启动期字段只需在字段表里
//! 声明它的生效时机。agent.yaml 的那几项由 [`agent_value`] 取值,测试钉住它覆盖字段表里的全部。

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde_json::Value;
use summon_core::config::Config;
use summon_core::dashboard::fields::{Effect, Layer, FIELDS};
use summon_core::registry::Agent;

use super::api::types::RestartItem;

/// `RestartItem.file` 的写法与 `StatusView.revisions` 的键相同,页面可以据此跳到那份文件。
pub(crate) const GLOBAL_FILE: &str = "global";

pub(crate) fn agent_file(alias: &str) -> String {
    format!("agent:{alias}")
}

#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct BootSnapshot {
    global: BTreeMap<&'static str, Value>,
    /// 按 agent 目录登记:改了 `name` 的 agent 也认得出是启动时的那一个。
    agents: BTreeMap<PathBuf, BTreeMap<&'static str, Value>>,
}

/// 存档的键:agent 目录;手搓的 agent(没有目录)退回名字。
fn agent_key(agent: &Agent) -> PathBuf {
    if agent.dir.as_os_str().is_empty() {
        PathBuf::from(&agent.name)
    } else {
        agent.dir.clone()
    }
}

fn restart_keys(layer: Layer) -> impl Iterator<Item = &'static str> {
    FIELDS
        .iter()
        .filter(move |f| f.effect == Effect::Restart && f.layers == [layer])
        .map(|f| f.key)
}

/// 点分路径在 JSON 里的值;不在为 null。
pub(crate) fn json_at(root: &Value, dotted: &str) -> Value {
    dotted
        .split('.')
        .try_fold(root, |cur, seg| cur.get(seg))
        .cloned()
        .unwrap_or(Value::Null)
}

fn global_values(cfg: &Config) -> BTreeMap<&'static str, Value> {
    let json = serde_json::to_value(cfg).unwrap_or(Value::Null);
    restart_keys(Layer::Global)
        .map(|key| (key, json_at(&json, key)))
        .collect()
}

/// agent.yaml 里启动期那几项的当前值(`Agent` 只能反序列化,逐项取)。
pub(crate) fn agent_value(agent: &Agent, key: &str) -> Option<Value> {
    let feishu = agent.feishu.as_ref();
    let text = |s: Option<&str>| Value::from(s.unwrap_or_default());
    Some(match key {
        "name" => Value::from(agent.name.as_str()),
        "status" => Value::from(agent.status.as_str()),
        "role" => Value::from(if agent.is_steward() {
            "steward"
        } else {
            "agent"
        }),
        "feishu.app_id_env" => text(feishu.and_then(|f| f.app_id_env.as_deref())),
        "feishu.app_secret_env" => text(feishu.and_then(|f| f.app_secret_env.as_deref())),
        "feishu.scope" => text(feishu.map(|f| f.scope.as_str())),
        "runtime.tmux_name" => text(agent.runtime.as_ref().and_then(|r| r.tmux_name.as_deref())),
        _ => return None,
    })
}

fn agent_values(agent: &Agent) -> BTreeMap<&'static str, Value> {
    restart_keys(Layer::Agent)
        .filter_map(|key| agent_value(agent, key).map(|v| (key, v)))
        .collect()
}

impl BootSnapshot {
    pub(crate) fn capture(cfg: &Config, agents: &[Agent]) -> Self {
        Self {
            global: global_values(cfg),
            agents: agents
                .iter()
                .map(|a| (agent_key(a), agent_values(a)))
                .collect(),
        }
    }

    /// 盘上与启动时不一致的启动期字段。本代启动之后才上线的 agent 没有存档,不报。
    pub(crate) fn restart_pending(&self, cfg: &Config, agents: &[Agent]) -> Vec<RestartItem> {
        let mut out: Vec<RestartItem> = global_values(cfg)
            .into_iter()
            .filter(|(key, value)| self.global.get(key) != Some(value))
            .map(|(key, _)| RestartItem {
                key: key.to_string(),
                file: GLOBAL_FILE.to_string(),
            })
            .collect();
        for agent in agents {
            let Some(booted) = self.agents.get(&agent_key(agent)) else {
                continue;
            };
            for (key, value) in agent_values(agent) {
                if booted.get(key) != Some(&value) {
                    out.push(RestartItem {
                        key: key.to_string(),
                        file: agent_file(&agent.name),
                    });
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(yaml: &str) -> Agent {
        serde_yaml::from_str(yaml).unwrap()
    }

    #[test]
    fn every_restart_field_of_agent_yaml_has_a_value() {
        let a = agent("name: a\n");
        for key in restart_keys(Layer::Agent) {
            assert!(
                agent_value(&a, key).is_some(),
                "字段表里 {key} 要重启生效,boot 存档取不到它的值"
            );
        }
        assert!(restart_keys(Layer::Global).any(|k| k == "dashboard.port"));
    }

    #[test]
    fn restart_pending_lists_changed_startup_fields_only() {
        let mut cfg = Config::default();
        let a = agent("name: a\nfeishu:\n  app_id_env: A_ID\n  app_secret_env: A_SECRET\n");
        let snapshot = BootSnapshot::capture(&cfg, std::slice::from_ref(&a));
        assert!(snapshot
            .restart_pending(&cfg, std::slice::from_ref(&a))
            .is_empty());

        // 启动期字段:端口、监听地址;非启动期字段(消息层开关、sentinel)不算。
        cfg.dashboard.port = 9999;
        cfg.preview_server.bind = "0.0.0.0".into();
        cfg.messaging.cot_visible = Some(false);
        cfg.sentinel.enabled = true;
        let changed =
            agent("name: a\nfeishu:\n  app_id_env: B_ID\n  app_secret_env: A_SECRET\nmodel: x\n");
        let newcomer = agent("name: b\n");
        let pending = snapshot.restart_pending(&cfg, &[changed, newcomer]);
        let keys: Vec<(String, String)> = pending
            .into_iter()
            .map(|item| (item.file, item.key))
            .collect();
        assert_eq!(
            keys,
            vec![
                ("global".to_string(), "dashboard.port".to_string()),
                ("global".to_string(), "preview_server.bind".to_string()),
                ("agent:a".to_string(), "feishu.app_id_env".to_string()),
            ]
        );
    }
}
