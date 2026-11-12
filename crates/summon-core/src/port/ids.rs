//! chat / 闸门的标识 newtype 与飞书消息定位。纯逻辑,编译期隔离命名空间,
//! 防止「这个 id 是 chat 还是 gate」的混淆(spec §3.3)。
//! `MsgRef` 是消息层的定位标识,chat / outbox 路径同样依赖它。

use serde::{Deserialize, Serialize};

macro_rules! str_newtype {
    ($name:ident) => {
        #[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        pub struct $name(String);
        impl $name {
            pub fn new(s: impl Into<String>) -> Self {
                Self(s.into())
            }
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }
        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}
str_newtype!(ChatId);
str_newtype!(GateId);

/// 飞书消息定位(出生地 / 活卡片)。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MsgRef {
    #[serde(default)]
    pub delivery_scope: String,
    #[serde(default)]
    pub chat_id: String,
    #[serde(default)]
    pub chat_type: String,
    #[serde(default)]
    pub message_id: String,
    #[serde(default)]
    pub thread_id: Option<String>,
    #[serde(default)]
    pub root_id: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_msg_ref_defaults_new_source_fields() {
        let old: MsgRef = serde_yaml::from_str("chat_id: oc_x\nmessage_id: om_x\n").unwrap();
        assert_eq!(old.delivery_scope, "");
        assert_eq!(old.thread_id, None);
        assert_eq!(old.root_id, None);
    }
}
