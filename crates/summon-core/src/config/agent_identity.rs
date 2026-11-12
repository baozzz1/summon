//! 飞书身份隔离的纯决策:一个 agent 子进程的环境里只应出现**它自己**的飞书凭据,绝不能看到
//! 兄弟 agent 的(否则会拿错 app_id/secret、以别人身份做飞书操作)。本模块只算「清哪些键 / 注入
//! 哪些键」,不读环境、不读盘——IO(枚举继承环境、读 per-agent `.env`、拼 spawn 命令)留给 `summon`。
//!
//! 隔离两步:① 先把继承环境里**所有飞书身份键**清掉;② 再把**仅本 agent 自己**的凭据加回来,
//! 并额外暴露成规范名 `FEISHU_APP_ID` / `FEISHU_APP_SECRET`,让下游工具无歧义地认身份。

/// 飞书身份键前缀:凭据键一律 `FEISHU_<ALIAS>_APP_ID` / `_APP_SECRET`(见 bind 的 `env_var_names`),
/// 规范名 `FEISHU_APP_*` 也落在此前缀下。子进程隔离时按此前缀清继承环境(narrow:只碰飞书身份,
/// 不动 `PATH`/`*_TOKEN`/`ANTHROPIC_*` 等 CLI 要用的)。
pub const FEISHU_ENV_PREFIX: &str = "FEISHU_";

/// 规范(alias-agnostic)飞书身份键:隔离后只有本 agent 的值落在这两个名下,下游统一读它。
pub const FEISHU_CANONICAL_ID: &str = "FEISHU_APP_ID";
pub const FEISHU_CANONICAL_SECRET: &str = "FEISHU_APP_SECRET";

/// 是否飞书身份键。spawn 清继承环境时对每个键调它(true 即 `env_remove`)。
pub fn is_feishu_identity_key(key: &str) -> bool {
    key.starts_with(FEISHU_ENV_PREFIX)
}

/// 是否由 summon runtime 独占的环境键。
///
/// `runtime.env` 是用户给 Agent 配普通工具环境的入口，不是伪造 Actor / run / 飞书身份的入口。
/// 前缀级判定比在 chat/task/worker 各维护一份易漏的键名单更稳：以后新增内部键也天然受保护。
pub fn is_runtime_owned_env_key(key: &str) -> bool {
    key.starts_with("SUMMON_") || is_feishu_identity_key(key)
}

/// 本 agent 子进程要**注入**的飞书身份环境:规范名 + 它自己的 alias 名,值取本 agent 自己的凭据。
/// 值为 `None`(未配/缺凭据)的项跳过——隔离后宁可「没有飞书身份」(fail-closed)也不给错的。
/// alias 名与规范名重名时(理论上不会)不重复加。规范名在前,便于下游优先命中。
pub fn feishu_self_inject(
    id_env_name: Option<&str>,
    id_val: Option<&str>,
    secret_env_name: Option<&str>,
    secret_val: Option<&str>,
) -> Vec<(String, String)> {
    let mut out = Vec::new();
    if let Some(v) = id_val {
        out.push((FEISHU_CANONICAL_ID.to_string(), v.to_string()));
        if let Some(n) = id_env_name {
            if n != FEISHU_CANONICAL_ID {
                out.push((n.to_string(), v.to_string()));
            }
        }
    }
    if let Some(v) = secret_val {
        out.push((FEISHU_CANONICAL_SECRET.to_string(), v.to_string()));
        if let Some(n) = secret_env_name {
            if n != FEISHU_CANONICAL_SECRET {
                out.push((n.to_string(), v.to_string()));
            }
        }
    }
    out
}

/// 要从子进程环境里**清掉**的全部飞书身份键(tmux 路径用 `env -u` 显式清:它继承的是 tmux server
/// 的环境,无法在 Rust 里按前缀枚举,故按「所有已知 agent 的身份键名」精确清)。入参是所有 agent 的
/// `app_id_env`/`app_secret_env` 名(含本 agent 自己,清掉后会再注入回来);输出去重 + 排序(稳定,
/// 便于断言),并保证含规范名 `FEISHU_APP_ID`/`FEISHU_APP_SECRET`(防有人手动 export 了规范名)。
pub fn feishu_unset_keys(all_identity_names: &[String]) -> Vec<String> {
    let mut set: std::collections::BTreeSet<String> = all_identity_names
        .iter()
        .filter(|n| !n.is_empty())
        .cloned()
        .collect();
    set.insert(FEISHU_CANONICAL_ID.to_string());
    set.insert(FEISHU_CANONICAL_SECRET.to_string());
    set.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_key_matches_feishu_prefix_only() {
        assert!(is_feishu_identity_key("FEISHU_AMBROSE_APP_ID"));
        assert!(is_feishu_identity_key("FEISHU_APP_SECRET"));
        // 不碰 CLI 要用的非飞书键。
        assert!(!is_feishu_identity_key("PATH"));
        assert!(!is_feishu_identity_key("ANTHROPIC_API_KEY"));
        assert!(!is_feishu_identity_key("SUMMON_CHAT_ID"));
    }

    #[test]
    fn runtime_owned_key_covers_summon_and_feishu_namespaces() {
        assert!(is_runtime_owned_env_key("SUMMON_CHAT_ID"));
        assert!(is_runtime_owned_env_key("SUMMON_FUTURE_INTERNAL_KEY"));
        assert!(is_runtime_owned_env_key("FEISHU_APP_SECRET"));
        assert!(!is_runtime_owned_env_key("ANTHROPIC_API_KEY"));
        assert!(!is_runtime_owned_env_key("SAFE_EXTRA"));
    }

    #[test]
    fn self_inject_emits_canonical_and_alias() {
        let got = feishu_self_inject(
            Some("FEISHU_STARLIGHT_APP_ID"),
            Some("cli_x"),
            Some("FEISHU_STARLIGHT_APP_SECRET"),
            Some("sec_y"),
        );
        assert_eq!(
            got,
            vec![
                ("FEISHU_APP_ID".to_string(), "cli_x".to_string()),
                ("FEISHU_STARLIGHT_APP_ID".to_string(), "cli_x".to_string()),
                ("FEISHU_APP_SECRET".to_string(), "sec_y".to_string()),
                (
                    "FEISHU_STARLIGHT_APP_SECRET".to_string(),
                    "sec_y".to_string()
                ),
            ]
        );
    }

    #[test]
    fn self_inject_skips_missing_values() {
        // 只有 id 有值、secret 缺:只注入 id(规范 + alias),secret 一项不加。
        let got = feishu_self_inject(
            Some("FEISHU_X_APP_ID"),
            Some("id"),
            Some("FEISHU_X_APP_SECRET"),
            None,
        );
        assert_eq!(
            got,
            vec![
                ("FEISHU_APP_ID".to_string(), "id".to_string()),
                ("FEISHU_X_APP_ID".to_string(), "id".to_string()),
            ]
        );
        // 全缺 → 空(fail-closed:无飞书身份)。
        assert!(feishu_self_inject(None, None, None, None).is_empty());
    }

    #[test]
    fn self_inject_no_dup_when_name_is_canonical() {
        let got = feishu_self_inject(Some("FEISHU_APP_ID"), Some("v"), None, None);
        assert_eq!(got, vec![("FEISHU_APP_ID".to_string(), "v".to_string())]);
    }

    #[test]
    fn unset_keys_dedup_sorted_with_canonical() {
        let names = vec![
            "FEISHU_AMBROSE_APP_ID".to_string(),
            "FEISHU_AMBROSE_APP_SECRET".to_string(),
            "FEISHU_STARLIGHT_APP_ID".to_string(),
            "FEISHU_AMBROSE_APP_ID".to_string(), // 重复
            "".to_string(),                      // 空跳过
        ];
        let got = feishu_unset_keys(&names);
        assert_eq!(
            got,
            vec![
                "FEISHU_AMBROSE_APP_ID".to_string(),
                "FEISHU_AMBROSE_APP_SECRET".to_string(),
                "FEISHU_APP_ID".to_string(),     // 规范名恒含
                "FEISHU_APP_SECRET".to_string(), // 规范名恒含
                "FEISHU_STARLIGHT_APP_ID".to_string(),
            ]
        );
    }
}
