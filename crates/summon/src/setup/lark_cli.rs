//! lark-cli 默认配置(应用 + 用户授权)的检查与补全。
//!
//! agent 调 lark-cli 走的是 daemon 按各自机器人凭据建的 profile(`agent_env::ensure_lark_cli_profile`),
//! 用不到默认配置。用到它的只有 sentinel:它以 owner 本人的身份读会话列表与消息,所以默认配置
//! 必须有应用、并完成用户授权。这一步放在开 sentinel 的时候做,onboard 不强制。

use anyhow::{bail, Context};
use serde::Deserialize;
use std::path::Path;
use std::process::Command;

#[derive(Default, Deserialize)]
struct AuthStatus {
    #[serde(default)]
    identities: Identities,
}

#[derive(Default, Deserialize)]
struct Identities {
    bot: Option<Identity>,
    user: Option<Identity>,
}

#[derive(Deserialize)]
struct Identity {
    status: String,
    available: bool,
    verified: bool,
}

/// lark-cli 某个 profile 的配置与身份状态。
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct IdentityState {
    pub configured: bool,
    pub bot_ready: bool,
    pub user_ready: bool,
}

impl IdentityState {
    pub(crate) fn ready(self) -> bool {
        self.configured && self.bot_ready && self.user_ready
    }
}

fn identity_ready(identity: Option<&Identity>) -> bool {
    identity.is_some_and(|identity| {
        identity.status == "ready" && identity.available && identity.verified
    })
}

fn command(lark_cli: &Path, profile: Option<&str>) -> Command {
    let mut cmd = Command::new(lark_cli);
    if let Some(profile) = profile {
        cmd.arg("--profile").arg(profile);
    }
    cmd
}

/// 查 `profile`(None = 默认配置)的状态;lark-cli 跑不起来时为 `None`。
pub(crate) fn identity_state(lark_cli: &Path, profile: Option<&str>) -> Option<IdentityState> {
    let configured = command(lark_cli, profile)
        .args(["config", "show"])
        .output()
        .ok()
        .is_some_and(|output| output.status.success());
    let output = command(lark_cli, profile)
        .args(["auth", "status", "--json", "--verify"])
        .output()
        .ok()?;
    // 没配置时 auth status 在 stderr 报错、stdout 为空:仍说明二进制能跑,只是什么都没就绪。
    let status: AuthStatus = serde_json::from_slice(&output.stdout).unwrap_or_default();
    Some(IdentityState {
        configured,
        bot_ready: identity_ready(status.identities.bot.as_ref()),
        user_ready: identity_ready(status.identities.user.as_ref()),
    })
}

/// 补全这份状态还缺的步骤,按顺序给出要跑的命令。
pub(crate) fn missing_steps(state: IdentityState, profile: Option<&str>) -> Vec<String> {
    let prefix = match profile {
        Some(profile) => format!("lark-cli --profile {profile}"),
        None => "lark-cli".to_string(),
    };
    let mut steps = Vec::new();
    if !state.configured {
        steps.push(format!("{prefix} config init --new"));
    } else if !state.bot_ready {
        // 已有应用配置、机器人身份校验没过:多半是网络或凭据一时失效,先看校验结果;
        // 重新初始化会换掉现有应用,只有确认它不能用了才做。
        steps.push(format!("{prefix} auth status --verify"));
    }
    if !state.user_ready {
        steps.push(format!("{prefix} auth login --domain im"));
    }
    steps
}

/// sentinel 要用的那份配置还没就绪时,交互终端里带人走完 lark-cli 的应用初始化与授权;
/// 非交互时只列出要跑的命令。走完仍没就绪就报错。
pub(crate) fn ensure_identity(
    lark_cli: &Path,
    profile: Option<&str>,
    interactive: bool,
) -> anyhow::Result<()> {
    let state = identity_state(lark_cli, profile)
        .with_context(|| summon_i18n::format!("r7d5537af68", lark_cli.display()))?;
    if state.ready() {
        return Ok(());
    }
    let steps = missing_steps(state, profile);
    if !interactive {
        bail!(summon_i18n::format!("r13fac9c19e", steps.join("\n  ")));
    }
    if !state.configured {
        println!("{}", summon_i18n::format!("r6337d19bdc"));
        run(lark_cli, profile, &["config", "init", "--new"])?;
    }
    let state = identity_state(lark_cli, profile)
        .with_context(|| summon_i18n::format!("rd0054191a9", lark_cli.display()))?;
    if state.configured && !state.bot_ready {
        bail!(summon_i18n::format!(
            "r7f01cec23c",
            missing_steps(state, profile).join("\n  ")
        ));
    }
    if state.configured && state.bot_ready && !state.user_ready {
        println!("{}", summon_i18n::format!("ree22ff04ce"));
        run(lark_cli, profile, &["auth", "login", "--domain", "im"])?;
    }
    let state = identity_state(lark_cli, profile)
        .with_context(|| summon_i18n::format!("r6c610c3129", lark_cli.display()))?;
    if !state.ready() {
        bail!(summon_i18n::format!(
            "r9d3355b4d5",
            missing_steps(state, profile).join("\n  ")
        ));
    }
    Ok(())
}

fn run(lark_cli: &Path, profile: Option<&str>, args: &[&str]) -> anyhow::Result<()> {
    let status = command(lark_cli, profile)
        .args(args)
        .status()
        .with_context(|| summon_i18n::format!("rd939e54096", lark_cli.display(), args.join(" ")))?;
    if !status.success() {
        bail!(summon_i18n::format!(
            "r8882d11796",
            lark_cli.display(),
            args.join(" ")
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ready_requires_a_verified_bot_and_user() {
        let ready: AuthStatus = serde_json::from_str(
            r#"{"identities":{"bot":{"status":"ready","available":true,"verified":true},"user":{"status":"ready","available":true,"verified":true}}}"#,
        )
        .unwrap();
        assert!(identity_ready(ready.identities.bot.as_ref()));
        assert!(identity_ready(ready.identities.user.as_ref()));

        let expired_user: AuthStatus = serde_json::from_str(
            r#"{"identities":{"bot":{"status":"ready","available":true,"verified":true},"user":{"status":"ready","available":true,"verified":false}}}"#,
        )
        .unwrap();
        assert!(identity_ready(expired_user.identities.bot.as_ref()));
        assert!(!identity_ready(expired_user.identities.user.as_ref()));

        let all = IdentityState {
            configured: true,
            bot_ready: true,
            user_ready: true,
        };
        assert!(all.ready());
        for partial in [
            IdentityState {
                configured: false,
                ..all
            },
            IdentityState {
                bot_ready: false,
                ..all
            },
            IdentityState {
                user_ready: false,
                ..all
            },
        ] {
            assert!(!partial.ready(), "{partial:?}");
        }
    }

    #[test]
    fn missing_steps_follow_the_state_and_carry_the_profile() {
        let blank = IdentityState::default();
        assert_eq!(
            missing_steps(blank, None),
            [
                "lark-cli config init --new",
                "lark-cli auth login --domain im"
            ]
        );
        let no_user = IdentityState {
            configured: true,
            bot_ready: true,
            user_ready: false,
        };
        assert_eq!(
            missing_steps(no_user, Some("owner")),
            ["lark-cli --profile owner auth login --domain im"]
        );
        let bad_app = IdentityState {
            configured: true,
            bot_ready: false,
            user_ready: true,
        };
        assert_eq!(
            missing_steps(bad_app, None),
            ["lark-cli auth status --verify"]
        );
        let ready = IdentityState {
            configured: true,
            bot_ready: true,
            user_ready: true,
        };
        assert!(ready.ready());
        assert!(missing_steps(ready, None).is_empty());
    }

    /// 已有应用配置、只是机器人身份这次没校验过(网络抖动):交互路径也不拉起新建应用的向导,
    /// 否则会换掉现有应用。
    #[test]
    fn a_failed_bot_check_never_starts_a_new_app() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("calls");
        let lark_cli = dir.path().join("lark-cli");
        // 调用记录落在脚本旁边的 `calls`(即 `log`)。
        crate::test_support::write_test_executable(
            &lark_cli,
            &format!(
                "#!/bin/sh\necho \"$@\" >> \"${{0%/*}}/calls\"\ncase \"$1 $2\" in\n  \"auth status\") echo '{}';;\nesac\nexit 0\n",
                r#"{"identities":{"bot":{"status":"error","available":false,"verified":false},"user":{"status":"ready","available":true,"verified":true}}}"#
            ),
        );
        let error = ensure_identity(&lark_cli, None, true).unwrap_err();
        assert!(
            format!("{error:#}").contains("auth status --verify"),
            "{error:#}"
        );
        let calls = std::fs::read_to_string(&log).unwrap();
        assert!(!calls.contains("config init"), "{calls}");
    }

    /// 非交互(脚本、Menu Bar)不能拉起浏览器向导:只说要跑哪几条命令,且不当成已就绪。
    #[test]
    fn a_non_interactive_caller_gets_the_commands_instead_of_a_wizard() {
        let dir = tempfile::tempdir().unwrap();
        let lark_cli = dir.path().join("lark-cli");
        crate::test_support::write_test_executable(&lark_cli, "#!/bin/sh\nexit 1\n");
        let error = ensure_identity(&lark_cli, None, false).unwrap_err();
        let text = format!("{error:#}");
        assert!(text.contains("lark-cli config init --new"), "{text}");
        assert!(text.contains("lark-cli auth login --domain im"), "{text}");
    }
}
