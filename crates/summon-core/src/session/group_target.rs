//! 群目标的输入与展示契约。用户输入名称，稳定 ID 只来自服务端候选。

use serde::{Deserialize, Serialize};
use serde_json::json;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum GroupTarget {
    Existing(String),
    New { name: String },
}

impl From<&str> for GroupTarget {
    fn from(value: &str) -> Self {
        Self::Existing(value.into())
    }
}
impl From<String> for GroupTarget {
    fn from(value: String) -> Self {
        Self::Existing(value)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ForkTargetInput {
    Picker,
    New(String),
    Existing(String),
    Code(String),
}

pub const FORK_USAGE: &str = "re30bb55a2f";

pub fn validate_name(name: &str) -> Result<&str, &'static str> {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > 100 || name.chars().any(char::is_control) {
        return Err(summon_i18n::text!("r6aef655697"));
    }
    if name.starts_with("oc_") {
        return Err(summon_i18n::text!("ra4e3520346"));
    }
    Ok(name)
}

pub fn parse_fork_target(input: &str) -> Result<ForkTargetInput, &'static str> {
    let input = input.trim();
    if input.is_empty() {
        return Ok(ForkTargetInput::Picker);
    }
    if let Some(code) = super::fork::parse_invite_code(input) {
        return Ok(ForkTargetInput::Code(code));
    }
    let (verb, name) = input
        .split_once(char::is_whitespace)
        .ok_or(::summon_core::i18n::label(FORK_USAGE))?;
    let name = validate_name(name)?.to_string();
    match verb {
        "new" => Ok(ForkTargetInput::New(name)),
        "to" => Ok(ForkTargetInput::Existing(name)),
        _ => Err(::summon_core::i18n::label(FORK_USAGE)),
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupCandidate {
    pub chat_id: String,
    pub name: String,
    pub description: String,
    pub member_count: Option<u64>,
}

pub fn exact_matches(name: &str, candidates: Vec<GroupCandidate>) -> Vec<GroupCandidate> {
    let mut ids = std::collections::HashSet::new();
    candidates
        .into_iter()
        .filter(|g| g.name == name && ids.insert(g.chat_id.clone()))
        .collect()
}

pub fn fork_picker_card(ticket: &str, source: &str) -> String {
    let button = |label, mode| {
        json!({
            "tag":"button", "name":mode, "type":"primary", "form_action_type":"submit",
            "text":{"tag":"plain_text","content":label},
            "behaviors":[{"type":"callback","value":{"action":"fork_submit","ticket":ticket,"mode":mode}}]
        })
    };
    json!({"schema":"2.0","body":{"elements":[
        {"tag":"markdown","content":summon_i18n::format!("r54dfe562a0", crate::messaging::feishu::escape_lark_md(source))},
        {"tag":"form","name":"fork_form","elements":[
            {"tag":"input","name":"fork_name","required":true,"placeholder":{"tag":"plain_text","content":summon_i18n::text!("r8d85bccf30")}},
            button(summon_i18n::text!("r2c823984d7"), "new"), button(summon_i18n::text!("rddf689dfe4"), "existing")
        ]}
    ]}}).to_string()
}

pub fn fork_candidates_card(ticket: &str, candidates: &[GroupCandidate], host: &str) -> String {
    let mut elements = vec![json!({"tag":"markdown","content":summon_i18n::text!("r6e5b4d23e0")})];
    for (index, group) in candidates.iter().enumerate() {
        let name = crate::messaging::feishu::escape_lark_md(&group.name);
        let description = crate::messaging::feishu::escape_lark_md(
            &group.description.chars().take(200).collect::<String>(),
        );
        let count = group
            .member_count
            .map(|n| summon_i18n::format!("r45fb5a7215"))
            .unwrap_or_default();
        let link = crate::observe::waiting::feishu_chat_link(host, &group.chat_id, None);
        elements.push(json!({"tag":"column_set","flex_mode":"none","columns":[
            {"tag":"column","width":"weighted","weight":1,"elements":[
                {"tag":"markdown","content":format!("{}．{name}{count}\n{description}",index+1)},
                {"tag":"button","type":"primary","text":{"tag":"plain_text","content":summon_i18n::text!("r730c06b149")},"behaviors":[{"type":"callback","value":{"action":"fork_choose","ticket":ticket,"index":index}}]},
                {"tag":"button","text":{"tag":"plain_text","content":summon_i18n::text!("re9a4d01819")},"behaviors":[{"type":"open_url","default_url":link}]}
            ]}
        ]}));
    }
    json!({"schema":"2.0","body":{"elements":elements}}).to_string()
}

pub fn fork_ready_card(name: &str, chat_id: &str, host: &str) -> String {
    let link = crate::observe::waiting::feishu_chat_link(host, chat_id, None);
    json!({"schema":"2.0","body":{"elements":[
        {"tag":"markdown","content":summon_i18n::format!("r88b502a3b1",crate::messaging::feishu::escape_lark_md(name))},
        {"tag":"button","text":{"tag":"plain_text","content":summon_i18n::text!("r4aa7e32ebd")},"type":"primary","behaviors":[{"type":"open_url","default_url":link}]}
    ]}}).to_string()
}

pub fn fork_pending_card(ticket: &str, name: &str) -> String {
    json!({"schema":"2.0","body":{"elements":[
        {"tag":"markdown","content":summon_i18n::format!("r336dcb1523",crate::messaging::feishu::escape_lark_md(name))},
        {"tag":"button","text":{"tag":"plain_text","content":summon_i18n::text!("rd724048450")},"behaviors":[{"type":"callback","value":{"action":"fork_execute","ticket":ticket}}]}
    ]}}).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fork_commands_preserve_full_names_and_keep_code_redemption() {
        assert_eq!(parse_fork_target(""), Ok(ForkTargetInput::Picker));
        assert_eq!(
            parse_fork_target("new Project Alpha"),
            Ok(ForkTargetInput::New("Project Alpha".into()))
        );
        assert_eq!(
            parse_fork_target("to 项目 Alpha"),
            Ok(ForkTargetInput::Existing("项目 Alpha".into()))
        );
        assert_eq!(
            parse_fork_target("#abcd"),
            Ok(ForkTargetInput::Code("abcd".into()))
        );
        for input in [
            "new",
            "to",
            "new oc_secret",
            "to oc_secret",
            "to a\nb",
            "NEW example",
            "项目",
        ] {
            assert!(parse_fork_target(input).is_err(), "{input:?}");
        }
    }

    #[test]
    fn exact_matches_keep_duplicates_and_exclude_substrings() {
        let group = |id: &str, name: &str| GroupCandidate {
            chat_id: id.into(),
            name: name.into(),
            description: String::new(),
            member_count: None,
        };
        let found = exact_matches(
            "Alpha",
            vec![
                group("oc_1", "Alpha Beta"),
                group("oc_2", "Alpha"),
                group("oc_3", "Alpha"),
                group("oc_2", "Alpha"),
            ],
        );
        assert_eq!(
            found.iter().map(|g| g.chat_id.as_str()).collect::<Vec<_>>(),
            ["oc_2", "oc_3"]
        );
    }

    #[test]
    fn picker_submits_names_and_selection_uses_only_candidate_indices() {
        let card: serde_json::Value =
            serde_json::from_str(&fork_picker_card("ticket", "源群")).unwrap();
        let wire = card.to_string();
        assert!(wire.contains("fork_name"));
        assert!(wire.contains("fork_submit"));
        assert!(!wire.contains("chat_id"));
        let options = vec![GroupCandidate {
            chat_id: "oc_private".into(),
            name: "Alpha".into(),
            description: "design".into(),
            member_count: Some(2),
        }];
        let card = fork_candidates_card("ticket", &options, "applink.feishu.cn");
        let parsed: serde_json::Value = serde_json::from_str(&card).unwrap();
        let value =
            &parsed["body"]["elements"][1]["columns"][0]["elements"][1]["behaviors"][0]["value"];
        assert_eq!(value["index"], 0);
        assert!(value.get("chat_id").is_none());
    }
}
