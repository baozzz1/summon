//! `summon agents add`:非交互创建 agent —— 单条(`agents add <alias>`)或批量
//! (`agents add --file agents.yaml`,fleet-as-code)。
//!
//! 分工(见 `docs/spec/2026-08-20-oss-onboarding-experience-design.md`):
//! - `onboard`:交互向导,新手默认路径;
//! - `agents add`:脚本化/批量创建,凭据从**环境变量**读(清单里只有 env 名,不落真值);
//! - `bind`:给已存在的目录绑/补字面凭据。
//!
//! agents.yaml **不是运行时配置源**:daemon 不读它,它只解决「一口气建 N 个」;
//! 创建后各 agent 独立演化,改配置直接改各自的 agent.yaml。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context};
use serde::Deserialize;
use summon_core::config::Config;

use crate::setup::bind::{self, ScaffoldOutcome, ScaffoldSpec};

/// agents.yaml 批量清单:alias 是 map 的 key(同名声明在 YAML 里就不可能重复)。
#[derive(Deserialize)]
struct Manifest {
    agents: BTreeMap<String, EntrySpec>,
}

/// 单条声明。全字段可选;缺省值见 [`create_one`]。
#[derive(Deserialize, Default)]
struct EntrySpec {
    /// 显示名;缺省用 alias
    display: Option<String>,
    /// 一句话职责,写进 agent.yaml 的 `# 职责:` 注释;缺省「TODO 待 owner 定位」
    role: Option<String>,
    /// 启动 cli;缺省 claude。合法值:claude / codex / cursor / traex
    cli: Option<String>,
    /// 启动模型;缺省按 cli 取候选桶首项(见 messaging.default.yaml)
    model: Option<String>,
    /// effort 档位(仅 claude/codex;cursor 把档位烤进 model 名)
    effort: Option<String>,
    /// 干活目录;缺省 ~/.summon/agents/<alias>/workspace
    workspace: Option<String>,
    /// 凭据 env 变量名。要么都给要么都不给;都不给时按 alias 推导
    /// (FEISHU_<ALIAS>_APP_ID / _APP_SECRET)。**清单里永远只有 env 名,没有真值。**
    id_env: Option<String>,
    secret_env: Option<String>,
}

/// cli 层传进来的参数(对应 `AgentsAction::Add` 的字段)。
pub struct AddArgs {
    pub alias: Option<String>,
    pub file: Option<PathBuf>,
    pub force: bool,
    pub display: Option<String>,
    pub role: Option<String>,
    pub cli: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub workspace: Option<String>,
}

#[derive(Debug)]
enum EntryResult {
    /// 全新创建(或 force 重写)。active = 凭据是否齐全
    Created { active: bool },
    /// 已存在目录的补绑:只写了 .env + 翻 active
    Supplemented,
    /// 已存在且无凭据可补:未写任何东西
    Skipped,
}

pub fn run(cfg: &Config, args: AddArgs) -> anyhow::Result<()> {
    // 与 bind 一致:新建 agent 前确保 ~/.summon 是 git 仓 + .gitignore 挡密钥(幂等、best-effort)。
    crate::setup::init::ensure();
    match (&args.alias, &args.file) {
        (Some(_), Some(_)) => bail!(summon_i18n::format!("raa0618d05c")),
        (None, None) => bail!(summon_i18n::format!("r314acfeb9e")),
        (Some(alias), None) => run_single(cfg, &args, alias),
        (None, Some(file)) => run_batch(cfg, &args, file),
    }
}

fn run_single(cfg: &Config, args: &AddArgs, alias: &str) -> anyhow::Result<()> {
    let entry = EntrySpec {
        display: args.display.clone(),
        role: args.role.clone(),
        cli: args.cli.clone(),
        model: args.model.clone(),
        effort: args.effort.clone(),
        workspace: args.workspace.clone(),
        id_env: None,
        secret_env: None,
    };
    let display = entry.display.clone().unwrap_or_else(|| alias.to_string());
    let (id_env, secret_env) = bind::env_var_names(alias);
    match create_one(cfg, alias, &entry, args.force)? {
        EntryResult::Created { active } => {
            let dir = cfg.agents_dir.join(alias);
            bind::print_summary(alias, &display, active, &dir, &id_env, &secret_env);
            if active {
                crate::cli::cli::agents_reload()?;
            }
        }
        EntryResult::Supplemented => {
            println!("{}", summon_i18n::format!("r169e1cf3cb"));
            crate::cli::cli::agents_reload()?;
        }
        EntryResult::Skipped => bail!(summon_i18n::format!("ree7f575c2e")),
    }
    Ok(())
}

fn run_batch(cfg: &Config, args: &AddArgs, file: &Path) -> anyhow::Result<()> {
    let raw = std::fs::read_to_string(file)
        .with_context(|| summon_i18n::format!("rc40d4b922a", file.display()))?;
    let manifest: Manifest = serde_yaml::from_str(&raw)
        .with_context(|| summon_i18n::format!("r43eb383d7f", file.display()))?;
    if manifest.agents.is_empty() {
        bail!(summon_i18n::format!("r1e81c69485"));
    }
    println!(
        "{}",
        summon_i18n::format!("rcc323fa769", file.display(), manifest.agents.len())
    );
    let mut any_active = false;
    let mut failures = 0usize;
    for (alias, entry) in &manifest.agents {
        // 单条失败不拖垮整批:收集成一行汇报,继续建后面的。
        match create_one(cfg, alias, entry, args.force) {
            Ok(EntryResult::Created { active }) => {
                any_active |= active;
                println!(
                    "{}",
                    summon_i18n::format!(
                        "rf409b97889",
                        if active {
                            "active"
                        } else {
                            summon_i18n::text!("r4952eaeb74")
                        }
                    )
                );
            }
            Ok(EntryResult::Supplemented) => {
                any_active = true;
                println!("{}", summon_i18n::format!("r8f1e02f1f2"));
            }
            Ok(EntryResult::Skipped) => {
                println!("{}", summon_i18n::format!("r491ec04bd2"));
            }
            Err(err) => {
                failures += 1;
                println!("{}", summon_i18n::format!("r528b810103"));
            }
        }
    }
    if any_active {
        // 热加载把新建的 active agent 接上线(只新增,不动存量;daemon 没跑时函数自己会提示)。
        println!("{}", summon_i18n::format!("r5290749047"));
        crate::cli::cli::agents_reload()?;
    } else {
        println!("{}", summon_i18n::format!("rb891d4610e"));
    }
    if failures > 0 {
        bail!(summon_i18n::format!("rda11dd4771"));
    }
    Ok(())
}

/// 落盘一条声明。校验 fail-closed(alias / cli / model / effort),凭据从环境变量读。
fn create_one(
    cfg: &Config,
    alias: &str,
    e: &EntrySpec,
    force: bool,
) -> anyhow::Result<EntryResult> {
    bind::validate_alias(alias)?;
    let cli = e.cli.as_deref().unwrap_or("claude");
    // from_kind 对未知 cli 静默落 Other —— 批量场景里一个拼写错误会造出起不来的 agent,
    // 这里 fail-closed 拦在创建前。
    if !matches!(
        cli,
        "claude" | "codex" | "cursor" | "agent" | "traex" | "traecli"
    ) {
        bail!(summon_i18n::format!("ra2803cf6e0"));
    }
    let messaging = crate::runtime::daemon::resolve_messaging_global();
    // 批量导入**不问 backend 要默认模型**:manifest 是声明式的,同一份文件在两台机器上该产出
    // 同样的 agent.yaml,而上游默认取决于这台机器 codex 装没装、登没登录。要跟上游就在
    // manifest 里显式写 model,或者用 onboard/bind 那两条人在终端前的路径。
    let model = bind::effective_model(e.model.as_deref(), &messaging, cli, || None);
    bind::validate_model_effort(cli, model.as_deref(), e.effort.as_deref())?;
    if cli == "cursor" {
        if let Some(m) = model.as_deref() {
            bind::warn_if_unknown_cursor_model(m);
        }
    }
    let display = e.display.clone().unwrap_or_else(|| alias.to_string());
    let role = e
        .role
        .clone()
        .unwrap_or_else(|| summon_i18n::text!("rc227265794").to_string());
    let (id_env, secret_env) = match (&e.id_env, &e.secret_env) {
        (None, None) => bind::env_var_names(alias),
        (Some(i), Some(s)) => (i.clone(), s.clone()),
        _ => bail!(summon_i18n::format!("ra560647e6b")),
    };
    let workspace = e
        .workspace
        .clone()
        .unwrap_or_else(|| format!("~/.summon/agents/{alias}/workspace"));
    // 凭据只从环境变量读:清单/命令行里永远只有 env 名,没有真值。
    let creds = match (std::env::var(&id_env), std::env::var(&secret_env)) {
        (Ok(id), Ok(sec)) if !id.is_empty() && !sec.is_empty() => Some((id, sec)),
        _ => None,
    };
    let active = creds.is_some();
    let spec = ScaffoldSpec {
        alias,
        display: &display,
        role: &role,
        cli,
        model: model.as_deref(),
        effort: e.effort.as_deref(),
        workspace: &workspace,
        id_env: &id_env,
        secret_env: &secret_env,
    };
    match bind::scaffold_agent(cfg, &spec, creds, force)? {
        ScaffoldOutcome::Created => Ok(EntryResult::Created { active }),
        ScaffoldOutcome::Supplemented => Ok(EntryResult::Supplemented),
        ScaffoldOutcome::Exists => Ok(EntryResult::Skipped),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// env 变量是进程级全局:两个测试同时 set/remove 会互相串。env 相关测试串行化。
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn test_cfg(dir: &Path) -> Config {
        Config {
            agents_dir: dir.to_path_buf(),
            ..Default::default()
        }
    }

    fn manifest_yaml() -> &'static str {
        // 凭据只写 env 名,不写真值 —— 这是 agents.yaml 的核心安全约束。
        "\
agents:
  saturn:
    display: 土星
    role: 代码审查
    cli: claude
  nimbus:
    display: 雨云
    role: 打杂
    cli: codex
    effort: high
"
    }

    #[test]
    fn batch_creates_each_agent_as_draft_without_env() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_cfg(dir.path());
        let manifest: Manifest = serde_yaml::from_str(manifest_yaml()).unwrap();
        assert_eq!(manifest.agents.len(), 2);
        for (alias, entry) in &manifest.agents {
            let got = create_one(&cfg, alias, entry, false).unwrap();
            assert!(matches!(got, EntryResult::Created { active: false }));
        }
        for alias in ["saturn", "nimbus"] {
            let d = dir.path().join(alias);
            assert!(d.join("agent.yaml").is_file(), "{alias} agent.yaml");
            assert!(d.join("SOUL.md").is_file(), "{alias} SOUL.md");
            assert!(d.join("memory/MEMORY.md").is_file(), "{alias} MEMORY.md");
            assert!(d.join("memory/history").is_dir(), "{alias} memory/history");
            assert!(d.join(".env").is_file(), "{alias} .env 占位");
            let yaml = std::fs::read_to_string(d.join("agent.yaml")).unwrap();
            assert!(yaml.contains("status: draft"), "{alias} 无凭据应落 draft");
        }
    }

    #[test]
    fn batch_is_idempotent_second_run_skips() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_cfg(dir.path());
        let manifest: Manifest = serde_yaml::from_str(manifest_yaml()).unwrap();
        for (alias, entry) in &manifest.agents {
            assert!(matches!(
                create_one(&cfg, alias, entry, false).unwrap(),
                EntryResult::Created { .. }
            ));
        }
        // 重跑:全部跳过,文件不动(mtime 不变)。
        let mtime_before = std::fs::metadata(dir.path().join("saturn/agent.yaml"))
            .unwrap()
            .modified()
            .unwrap();
        for (alias, entry) in &manifest.agents {
            assert!(matches!(
                create_one(&cfg, alias, entry, false).unwrap(),
                EntryResult::Skipped
            ));
        }
        let mtime_after = std::fs::metadata(dir.path().join("saturn/agent.yaml"))
            .unwrap()
            .modified()
            .unwrap();
        assert_eq!(mtime_before, mtime_after, "跳过路径不应重写文件");
    }

    #[test]
    fn creds_come_from_env_not_manifest() {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_cfg(dir.path());
        let entry = EntrySpec::default();
        // 清单里没有 secret;真值从环境变量来。
        std::env::set_var("FEISHU_ENVBOT_APP_ID", "cli_test123");
        std::env::set_var("FEISHU_ENVBOT_APP_SECRET", "sec_test456");
        let got = create_one(&cfg, "envbot", &entry, false).unwrap();
        std::env::remove_var("FEISHU_ENVBOT_APP_ID");
        std::env::remove_var("FEISHU_ENVBOT_APP_SECRET");
        assert!(matches!(got, EntryResult::Created { active: true }));
        let env_content = std::fs::read_to_string(dir.path().join("envbot/.env")).unwrap();
        assert!(
            env_content.contains("cli_test123"),
            ".env 应含环境变量里的 app_id"
        );
        assert!(
            env_content.contains("sec_test456"),
            ".env 应含环境变量里的 secret"
        );
        let yaml = std::fs::read_to_string(dir.path().join("envbot/agent.yaml")).unwrap();
        assert!(yaml.contains("status: active"), "凭据齐全应落 active");
    }

    #[test]
    fn force_overwrites_identity_files() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_cfg(dir.path());
        let entry = EntrySpec {
            display: Some("旧名".to_string()),
            ..Default::default()
        };
        create_one(&cfg, "forcetest", &entry, false).unwrap();
        let entry2 = EntrySpec {
            display: Some("新名".to_string()),
            ..Default::default()
        };
        // 不带 force:跳过,文件不变
        assert!(matches!(
            create_one(&cfg, "forcetest", &entry2, false).unwrap(),
            EntryResult::Skipped
        ));
        // SOUL 初稿是空文件,认不出新旧;display 同时写进 agent.yaml,拿它判重写与否。
        let yaml = std::fs::read_to_string(dir.path().join("forcetest/agent.yaml")).unwrap();
        assert!(yaml.contains("旧名"));
        // 带 force:重写
        assert!(matches!(
            create_one(&cfg, "forcetest", &entry2, true).unwrap(),
            EntryResult::Created { .. }
        ));
        let yaml = std::fs::read_to_string(dir.path().join("forcetest/agent.yaml")).unwrap();
        assert!(yaml.contains("新名"), "force 应重写身份文件");
        assert!(dir.path().join("forcetest/SOUL.md").is_file());
    }

    #[test]
    fn bad_alias_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_cfg(dir.path());
        // 长度合法但首字符大写 → 命中「小写字母开头」检查
        let err = create_one(&cfg, "XY", &EntrySpec::default(), false).unwrap_err();
        assert!(err.to_string().contains("小写字母"));
        assert!(create_one(&cfg, "a", &EntrySpec::default(), false).is_err()); // 太短
    }

    #[test]
    fn unknown_cli_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_cfg(dir.path());
        let entry = EntrySpec {
            cli: Some("cluade".to_string()), // 拼写错误
            ..Default::default()
        };
        let err = create_one(&cfg, "clitest", &entry, false).unwrap_err();
        assert!(err.to_string().contains("cluade"), "错误应点出非法 cli 值");
    }

    #[test]
    fn invalid_effort_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_cfg(dir.path());
        let entry = EntrySpec {
            effort: Some("ultra".to_string()),
            ..Default::default()
        };
        assert!(create_one(&cfg, "efftest", &entry, false).is_err());
    }

    #[test]
    fn custom_id_env_pair_used() {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_cfg(dir.path());
        let entry = EntrySpec {
            id_env: Some("CUSTOM_ID".to_string()),
            secret_env: Some("CUSTOM_SECRET".to_string()),
            ..Default::default()
        };
        std::env::set_var("CUSTOM_ID", "cli_custom");
        std::env::set_var("CUSTOM_SECRET", "sec_custom");
        create_one(&cfg, "custombot", &entry, false).unwrap();
        std::env::remove_var("CUSTOM_ID");
        std::env::remove_var("CUSTOM_SECRET");
        let env_content = std::fs::read_to_string(dir.path().join("custombot/.env")).unwrap();
        assert!(env_content.contains("CUSTOM_ID=cli_custom"));
        assert!(env_content.contains("CUSTOM_SECRET=sec_custom"));
    }

    #[test]
    fn partial_id_env_pair_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_cfg(dir.path());
        let entry = EntrySpec {
            id_env: Some("CUSTOM_ID".to_string()),
            ..Default::default()
        };
        let err = create_one(&cfg, "partialbot", &entry, false).unwrap_err();
        assert!(err.to_string().contains("要么都给要么都不给"));
    }
}
