//! 远程工具的配置与连接协议；进程、SSH 和持久化由应用层负责。

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub const PROTOCOL_VERSION: u32 = 1;
pub const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_SESSIONS: usize = 8;
pub const FILE_TOOLS: &[&str] = &[
    "Read",
    "Edit",
    "Write",
    "Bash",
    "Glob",
    "Grep",
    "NotebookEdit",
];

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Provider {
    #[default]
    ClaudeCode,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ExecutionTarget {
    pub provider: Provider,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionBinding {
    pub target: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_dir: Option<String>,
}

impl ExecutionBinding {
    pub fn validate(&self) -> Result<(), String> {
        validate_name(&self.target)?;
        if let Some(path) = &self.working_dir {
            validate_working_dir(path)?;
        }
        Ok(())
    }
}

pub fn validate_name(name: &str) -> Result<(), String> {
    if name.is_empty()
        || name.len() > 48
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(summon_i18n::text!("r8154205b3a").into());
    }
    Ok(())
}

pub fn validate_working_dir(path: &str) -> Result<(), String> {
    if !path.starts_with('/') || path.contains('\0') || path.len() > 4096 {
        return Err(summon_i18n::text!("r915efd57a8").into());
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Frame {
    Hello {
        protocol: u32,
        provider: Provider,
        provider_version: String,
        working_dir: String,
    },
    Welcome {
        protocol: u32,
    },
    Open {
        id: u64,
        working_dir: Option<String>,
    },
    Opened {
        id: u64,
        working_dir: String,
    },
    Data {
        id: u64,
        message: Value,
    },
    Close {
        id: u64,
        error: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum LocalRequest {
    Status,
    Attach { working_dir: Option<String> },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TargetStatus {
    pub name: String,
    pub protocol: u32,
    pub provider: Provider,
    pub provider_version: String,
    pub working_dir: String,
    pub sessions: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum LocalReply {
    Status { status: TargetStatus },
    Ready { working_dir: String },
    Error { message: String },
}

pub fn rpc_error(message: &Value, text: &str) -> Option<Value> {
    message
        .get("id")
        .map(|id| json!({"jsonrpc":"2.0", "id":id, "error":{"code":-32602,"message":text}}))
}

/// 描述和调用使用同一能力表，避免“看不见但仍能调用”的隐藏入口。
pub fn allowed_request(message: &Value) -> bool {
    match message.get("method").and_then(Value::as_str) {
        Some("tools/call") => message["params"]["name"]
            .as_str()
            .is_some_and(|name| FILE_TOOLS.contains(&name)),
        Some(
            "initialize"
            | "notifications/initialized"
            | "ping"
            | "tools/list"
            | "notifications/cancelled",
        ) => true,
        _ => false,
    }
}

pub fn filter_tools(response: &mut Value) {
    if let Some(tools) = response
        .pointer_mut("/result/tools")
        .and_then(Value::as_array_mut)
    {
        tools.retain(|tool| {
            tool["name"]
                .as_str()
                .is_some_and(|name| FILE_TOOLS.contains(&name))
        });
    }
    if let Some(capabilities) = response.pointer_mut("/result/capabilities") {
        *capabilities = json!({"tools":{}});
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_execution_binding_paths_and_names() {
        for name in ["", "../x", "a/b", "a b", "-R :22"] {
            assert!(validate_name(name).is_err(), "{name}");
        }
        assert!(validate_name("laptop-1").is_ok());
        assert!(validate_working_dir("/work/a b").is_ok());
        assert!(validate_working_dir("~/work").is_err());
        assert!(validate_working_dir("/work\0other").is_err());
    }

    #[test]
    fn filter_and_authorize_share_the_tool_contract() {
        let mut response =
            json!({"result":{"tools":[{"name":"Read"},{"name":"Agent"},{"name":"Bash"}]}});
        filter_tools(&mut response);
        assert_eq!(
            response["result"]["tools"],
            json!([{"name":"Read"},{"name":"Bash"}])
        );
        for tool in FILE_TOOLS {
            assert!(allowed_request(
                &json!({"method":"tools/call","params":{"name":tool}})
            ));
        }
        assert!(!allowed_request(
            &json!({"method":"tools/call","params":{"name":"Agent"}})
        ));
        assert!(!allowed_request(&json!({"method":"resources/read"})));
    }

    #[test]
    fn execution_config_defaults_and_unknown_fields_are_explicit() {
        assert_eq!(
            serde_json::from_value::<ExecutionTarget>(json!({}))
                .unwrap()
                .provider,
            Provider::ClaudeCode
        );
        assert!(serde_json::from_value::<ExecutionBinding>(
            json!({"target":"laptop","command":"evil"})
        )
        .is_err());
        assert!(serde_json::from_value::<Frame>(
            json!({"type":"open","id":1,"working_dir":null,"env":{"KEY":"secret"}})
        )
        .is_err());
    }
}
