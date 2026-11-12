//! `preview.render`:按页面所在那一层的生效设置,叠上未保存的草稿,用 summon 真实的卡片与过程
//! 构造函数渲染一段示例对话(`summon_core::dashboard::preview`)。不联网、不发送、不写盘。
//!
//! 「每一项管哪一块」的探针来自字段表:声明了预览对照值(`preview_contrast`)的项各探一次。

use serde_json::Value;
use summon_core::config::{builtin_messaging, Messaging};
use summon_core::dashboard::fields::{self, Contrast, FieldDecl, Layer, FIELDS};
use summon_core::dashboard::preview::{self, RegionProbe};
use summon_core::settings::{ChatOverrides, ResolvedSettings};

use super::config::{agent_messaging, chat_layer, global_messaging};
use super::types::{PreviewRequest, Scope};
use super::{params, reply};
use crate::runtime::dashboard::server::{ApiCtx, ApiError};

fn field_value(m: &Messaging, field: &FieldDecl) -> Value {
    let json = serde_json::to_value(m).unwrap_or(Value::Null);
    field
        .key
        .strip_prefix("messaging.")
        .and_then(|name| json.get(name).cloned())
        .unwrap_or(Value::Null)
}

/// 页面语义 → 盘上语义(只有取反项要翻)。
fn to_disk(field: &FieldDecl, value: &Value) -> Value {
    match value {
        Value::Bool(b) if field.inverted => Value::Bool(!b),
        other => other.clone(),
    }
}

/// 字段表里能在预览里指出区块的项,对照值按此刻的生效值算。
fn probes(m: &Messaging, layer: Layer) -> Vec<RegionProbe<'static>> {
    FIELDS
        .iter()
        .filter(|f| f.key.starts_with("messaging.") && f.has_layer(layer))
        .filter_map(|f| {
            let contrast = match f.preview_contrast? {
                Contrast::Flip => None,
                alt @ Contrast::Alt(..) => {
                    let current: serde_yaml::Value =
                        serde_yaml::to_value(field_value(m, f)).unwrap_or(serde_yaml::Value::Null);
                    Some(serde_json::to_value(alt.against(&current)).unwrap_or(Value::Null))
                }
            };
            Some(RegionProbe {
                key: f.key,
                contrast,
            })
        })
        .collect()
}

pub(crate) fn render(ctx: &ApiCtx, p: Value) -> Result<Value, ApiError> {
    let env = ctx.env()?;
    let req: PreviewRequest = params(p)?;
    let cfg = env.read_config()?;
    let global = global_messaging(&cfg);
    // 每一层的生效设置、它上一层的消息层(草稿写 null = 恢复继承时用),与预览栏标题。
    let (layer, mut m, mut s, parent, title) = match &req.scope {
        Scope::Global => {
            let s = ResolvedSettings::resolve(&global, &ChatOverrides::default());
            (
                Layer::Global,
                global,
                s,
                builtin_messaging(),
                summon_i18n::text!("r9d06e431d1").to_string(),
            )
        }
        Scope::Agent { agent } => {
            let a = env.agent(agent)?;
            let m = agent_messaging(&cfg, &a);
            let s = ResolvedSettings::resolve(&m, &ChatOverrides::default());
            (
                Layer::Agent,
                m,
                s,
                global,
                summon_i18n::format!("rbeb3c26e2b", display(&a)),
            )
        }
        Scope::Chat { agent, chat_id } => {
            let a = env.agent(agent)?;
            let m = agent_messaging(&cfg, &a);
            let chat = chat_layer(env, &a, chat_id, &m);
            let s = ResolvedSettings::resolve(&m, &chat.overrides);
            let name = super::chats::chat_name_of(&a, chat_id).unwrap_or_else(|| chat_id.clone());
            let parent = m.clone();
            (
                Layer::Chat,
                m,
                s,
                parent,
                summon_i18n::format!("rc2c692837a", display(&a)),
            )
        }
    };
    for change in &req.draft {
        // 草稿可能正在输入中(空串、半个数):认不出的项跳过,不让整个预览失败。
        let Some(field) = fields::field(&change.key)
            .filter(|f| f.key.starts_with("messaging.") && f.has_layer(layer))
        else {
            continue;
        };
        let disk = if change.value.is_null() {
            field_value(&parent, field)
        } else {
            to_disk(field, &change.value)
        };
        if let Some((next_m, next_s)) = preview::with_value(&m, &s, field.key, &disk) {
            m = next_m;
            s = next_s;
        }
    }
    let mut view = preview::render(&m, &s, &title);
    view.regions = preview::regions(&m, &s, &probes(&m, layer));
    reply(&view)
}

fn display(agent: &summon_core::registry::Agent) -> String {
    if agent.display.trim().is_empty() {
        agent.name.clone()
    } else {
        agent.display.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::super::testkit::*;
    use super::*;
    use serde_json::json;

    fn cot_items(view: &Value) -> Vec<String> {
        view["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|m| m["kind"] == "cot")
            .flat_map(|m| m["items"].as_array().unwrap().clone())
            .map(|i| i["type"].as_str().unwrap().to_string())
            .collect()
    }

    #[test]
    fn draft_changes_preview_without_saving() {
        let kit = Kit::new();
        let a = kit.add_agent("starlight", &agent_yaml("starlight", "星光", "claude", ""));
        kit.add_chat(&a, "group", GROUP, "示例群");
        let ctx = kit.owner();
        let before = kit.read("config.yaml");
        let scope = json!({"level": "chat", "agent": "starlight", "chat_id": GROUP});
        let base = kit
            .call(&ctx, render, json!({"scope": scope, "draft": []}))
            .unwrap();
        assert_eq!(base["title"], "按 星光 · 示例群 的设置");
        assert!(cot_items(&base).iter().any(|t| t == "thinking"));
        // 草稿:关掉「显示思考」(页面语义)。
        let drafted = kit
            .call(
                &ctx,
                render,
                json!({"scope": scope, "draft": [{"key": "messaging.hide_thinking", "value": false}]}),
            )
            .unwrap();
        assert!(!cot_items(&drafted).iter().any(|t| t == "thinking"));
        assert_eq!(kit.read("config.yaml"), before, "预览不写盘");
        let session = a.tmux_name_for_chat("group", GROUP, false);
        assert!(!crate::runtime::daemon::dashboard_port::chat_settings_path(&a, &session).exists());
        // 每一项管的区块由服务端按字段表探出来。
        let regions = base["regions"].as_object().unwrap();
        assert!(regions.contains_key("messaging.cot_visible"));
        assert!(regions.contains_key("messaging.badge_effort"));
        assert!(
            !regions.contains_key("messaging.idle_ttl_secs"),
            "不影响长相的项没有区块"
        );
        // 写 null = 恢复继承:与没有草稿相同。
        let inherit = kit
            .call(
                &ctx,
                render,
                json!({"scope": scope, "draft": [{"key": "messaging.hide_thinking", "value": null}]}),
            )
            .unwrap();
        assert_eq!(inherit["messages"], base["messages"]);
        // 半截草稿不让预览失败。
        kit.call(
            &ctx,
            render,
            json!({"scope": {"level": "global"}, "draft": [{"key": "messaging.progress_max_steps", "value": ""}]}),
        )
        .unwrap();
    }
}
