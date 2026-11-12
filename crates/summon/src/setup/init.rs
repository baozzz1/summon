//! `~/.summon` 运行时家目录的一次性初始化:把它做成 git 仓库 + 写好 `.gitignore`。
//!
//! 这个仓是**纯本地仓,不加 remote、不外推**。它只做一件事:给「不可再生的手写资产」
//! (SOUL/memory/agent.yaml/CLAUDE.md/config.yaml)留一条可 diff、可回滚的演化史。
//! 它**不是备份**——`rm -rf ~/.summon`、盘坏、机器丢,`.git` 与被保护的数据同生共死,
//! 一种都救不了。灾难恢复是备份工具(Time Machine / restic)的事,别指望这里。
//!
//! 之所以要写死这条定位:`~/.summon` 里躺着未脱敏的会话数据(`raw/` 的录制、`threads/` 的
//! 飞书转录、`inbound/` 的图片)与 agent 工作区(`workspace/`,内容不可预知)。哪天有人
//! (包括读到这段注释的 agent)好心 `git remote add` 一下,这些就全出去了。所以 `.gitignore`
//! 用白名单把它们挡在版本库外,注释也不该再写「可迁移、可分享」诱导外推。
//!
//! 幂等、best-effort:已是仓库不重 init、已有 `.gitignore` 不覆盖、失败只记不致命。

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// 默认全局配置模板。作为工程内 example 源文件维护,初始化时复制到 `~/.summon/config.yaml`。
pub fn config_example_yaml() -> &'static str {
    summon_i18n::include_text!("config.example.yaml")
}

/// `~/.summon` 的 `.gitignore`:**白名单**——默认全挡,只放行**不可再生**的东西。
///
/// 判据是「丢了能不能再生」,不是「是不是手写」:`threads/`(会话转录)与 `tasks/`(Task 与
/// 每轮 recap)都不是手写,但同样再生不出来;而 `runtime/`、`bin/`、`<alias>-feishu-scopes.json`
/// 看着像配置,其实都是生成物。用「手写」当判据会把前者漏掉、把后者收进来。
///
/// 为什么是白名单:手写资产与运行时产物**交错嵌套**在同一棵树下(`agents/<alias>/` 下
/// `SOUL.md` 与 `threads/` 并排)。黑名单要逐条枚举例外,每加一个新的运行时子目录就多一次
/// 漏网机会——`raw/`(30MB 未脱敏录制)和 `workspace/`(agent 的 cwd)就是这么漏进版本库的。
/// 白名单则默认全挡,新增任何目录都不可能自动入库。
pub const SUMMON_GITIGNORE: &str = "r895a7b20a8";

/// `~/.summon` 路径。
/// `.gitignore` 首行 marker 前缀:标记此文件由 summon 托管、随二进制升级重写。
/// 用户删掉带此前缀的首行 = 自行接管,summon 从此不再覆盖(见 `ensure_summon_repo`)。
pub const GITIGNORE_MARKER: &str = "# summon-managed-gitignore";

pub fn summon_home() -> PathBuf {
    dirs::home_dir().unwrap_or_default().join(".summon")
}

/// 对默认 `~/.summon` 跑一次初始化;best-effort,失败只打日志不中断调用方。
pub fn ensure() {
    let home = summon_home();
    if let Err(e) = ensure_summon_repo(&home) {
        eprintln!("{}", summon_i18n::format!("rf4fbf263b6"));
    }
}

/// 确保 `home`(~/.summon)是 git 仓库且 `.gitignore` 到位。幂等:已是仓库跳过 init、
/// 已有 `.gitignore` 不覆盖(尊重用户已改的)。
///
/// 已知限制:「不存在才写」意味着**存量用户升级二进制后拿不到新版 `SUMMON_GITIGNORE`**
/// ——他们的 `.gitignore` 早已存在,永远停在初装那版。要让托管规则能随版本演进,需引入
/// 「托管段 marker + 有条件覆盖」机制(在识别为旧托管版本时才重写,用户手改过的不碰)。
/// 暂未做,故改本常量只惠及全新安装;存量机器需手动同步 `.gitignore`。
pub fn ensure_summon_repo(home: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(home)?;
    // S3:~/.summon 下有凭据/会话/运行时,收紧到 0700,挡同机其他本地用户遍历。
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(home, std::fs::Permissions::from_mode(0o700));
    }
    // .gitignore:不存在才写(不覆盖用户已改的)
    // .gitignore 三态托管:不存在→写最新;首行带 marker 的旧托管版→升级到最新;
    // 无 marker(用户已接管,或 marker 机制之前的老文件)→不碰,尊重用户改动。
    let ignore = home.join(".gitignore");
    match std::fs::read_to_string(&ignore) {
        Err(_) => std::fs::write(&ignore, ::summon_core::i18n::label(SUMMON_GITIGNORE))?,
        Ok(cur) => {
            let managed = cur
                .lines()
                .next()
                .is_some_and(|l| l.starts_with(GITIGNORE_MARKER));
            if managed && cur != ::summon_core::i18n::label(SUMMON_GITIGNORE) {
                std::fs::write(&ignore, ::summon_core::i18n::label(SUMMON_GITIGNORE))?;
            }
        }
    }
    write_config_example_if_absent(home)?;
    // git init:已是仓库就跳过(幂等)
    if !home.join(".git").exists() {
        let ok = Command::new(summon_core::tools::resolve(summon_core::tools::Tool::Git))
            .arg("init")
            .arg(home)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok {
            anyhow::bail!(summon_i18n::format!("rc0e7679a4e", home.display()));
        }
    }
    Ok(())
}

/// 首次初始化时把工程内 example 复制成用户配置;已有 `config.yaml` 则绝不覆盖。
pub fn write_config_example_if_absent(home: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(home)?;
    let config = home.join("config.yaml");
    if !config.exists() {
        std::fs::write(config, config_example_yaml())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_example_keys_parse_after_uncomment() {
        // D2:example 是新用户认识可配项的唯一入口。把其中被注释的**配置行**揭开后必须能被
        // serde 解析成 Config,且代表性键名真实命中字段——字段改名/删除时这条会红,防再漂移。
        // prose 注释(`# 中文说明…`/二级 `# #`)不参与揭注。
        let mut yaml = String::new();
        for line in config_example_yaml().lines() {
            let Some(rest) = line.strip_prefix('#') else {
                yaml.push_str(line);
                yaml.push('\n');
                continue;
            };
            let rest = rest.strip_prefix(' ').unwrap_or(rest);
            let t = rest.trim_start();
            let is_cfg = t.starts_with("- ")
                || t.split_once(':').is_some_and(|(k, _)| {
                    !k.is_empty()
                        && k.chars()
                            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
                });
            if is_cfg {
                yaml.push_str(rest);
                yaml.push('\n');
            }
        }
        let cfg: summon_core::config::Config =
            serde_yaml::from_str(&yaml).expect("example 揭注释后应可被 Config 解析");
        // 代表性键名钉死(键名漂移 → Override 落 None / 默认值 → 断言红)。
        assert!(cfg.resume_sessions);
        assert_eq!(cfg.messaging.session_mirror, Some(true));
        assert_eq!(cfg.messaging.default_reaction.as_deref(), Some("OK"));
    }

    #[test]
    fn gitignore_is_whitelist_with_secret_backstop() {
        // 白名单基调:首条规则必须是 `/*`(默认全挡)。退回黑名单会让新增的运行时目录自动入库。
        let first = ::summon_core::i18n::label(SUMMON_GITIGNORE)
            .lines()
            .find(|l| !l.trim().is_empty() && !l.starts_with('#'))
            .expect("应有规则行");
        assert_eq!(first, "/*", "首条规则须为 /*(默认全挡)");
        // 兜底行:任何 `!` 放行之后仍要挡住密钥。缺了它,某天有人放行 agents/*/* 就会漏 .env。
        assert!(::summon_core::i18n::label(SUMMON_GITIGNORE).contains("\n.env\n"));
        assert!(::summon_core::i18n::label(SUMMON_GITIGNORE).contains("\n.DS_Store\n"));
    }

    /// 生成物与瞬态一律不收:收了会让仓被噪音淹没,且它们本就能重建。
    #[test]
    fn gitignore_still_excludes_generated_and_transient() {
        for rule in [
            "\n/tasks/.locks\n",
            "\n/tasks/*.lock\n",
            "\n/schedules/*.lock\n",
        ] {
            assert!(
                ::summon_core::i18n::label(SUMMON_GITIGNORE).contains(rule),
                "应排除 {rule:?}"
            );
        }
        // runtime/ 与 sessions/ 靠 `/agents/*/*` 兜底,不该出现放行规则
        assert!(!::summon_core::i18n::label(SUMMON_GITIGNORE).contains("!/agents/*/runtime/"));
        assert!(!::summon_core::i18n::label(SUMMON_GITIGNORE).contains("!/agents/*/sessions/"));
        assert!(!::summon_core::i18n::label(SUMMON_GITIGNORE).contains("!/agents/*/inbound/"));
    }

    /// 放行 threads/ 把「事件流可能捕获环境变量」变成了真实风险,警告必须留在模板里。
    #[test]
    fn gitignore_warns_about_credential_capture_in_events() {
        assert!(
            ::summon_core::i18n::label(SUMMON_GITIGNORE).contains("凭据警告"),
            "放行 threads/ 后,模板必须提示 events.jsonl 可能捕获注入的环境变量"
        );
        // 绝不能出现放行 .env 的规则。
        assert!(!::summon_core::i18n::label(SUMMON_GITIGNORE)
            .lines()
            .any(|l| l.starts_with('!') && l.contains(".env")));
    }

    /// 用真实 `git check-ignore` 钉死白名单语义:该收的收、该挡的挡。
    /// 不用字符串断言——那看不出 `agents/.DS_Store` 这类「规则没覆盖到的层级」的洞。
    /// `core.excludesfile=/dev/null`:屏蔽本机全局 gitignore,否则 `.DS_Store` 会被用户的
    /// `~/.gitignore_global` 挡住,测试在本机绿、换机器红(这个洞就是这么藏住的)。
    /// 需本机 git;默认 ignore,跑:cargo test -p summon -- --ignored
    #[test]
    #[ignore]
    fn gitignore_whitelist_admits_identity_and_blocks_runtime() {
        let home = std::env::temp_dir().join(format!("summon-ignore-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let a = home.join("agents/arcadia");
        for d in [
            "memory",
            "threads/g1",
            "sessions/g1",
            "inbound/quotes",
            "runtime",
            "workspace/.claude",
        ] {
            std::fs::create_dir_all(a.join(d)).unwrap();
        }
        std::fs::create_dir_all(home.join("raw/baseline")).unwrap();
        std::fs::create_dir_all(home.join("run/outbox")).unwrap();
        std::fs::create_dir_all(home.join("bin")).unwrap();
        std::fs::create_dir_all(home.join("tasks/SUM-1/runs")).unwrap();
        std::fs::create_dir_all(home.join("tasks/.locks")).unwrap();
        std::fs::create_dir_all(home.join("schedules")).unwrap();
        std::fs::create_dir_all(home.join("runtime")).unwrap();
        std::fs::write(
            home.join(".gitignore"),
            ::summon_core::i18n::label(SUMMON_GITIGNORE),
        )
        .unwrap();
        for f in ["config.yaml", "CLAUDE.md", "AGENTS.md", ".DS_Store"] {
            std::fs::write(home.join(f), "").unwrap();
        }
        std::fs::write(home.join("agents/.DS_Store"), "").unwrap();
        for f in [
            "SOUL.md",
            "CLAUDE.md",
            "AGENTS.md",
            "agent.yaml",
            ".env",
            ".DS_Store",
        ] {
            std::fs::write(a.join(f), "").unwrap();
        }
        std::fs::write(a.join("memory/MEMORY.md"), "").unwrap();
        std::fs::write(a.join("threads/g1/transcript.md"), "").unwrap();
        std::fs::write(a.join("sessions/g1/session_id"), "").unwrap();
        std::fs::write(a.join("inbound/quotes/om_x.md"), "").unwrap();
        std::fs::write(a.join("runtime/emoji.md"), "").unwrap();
        std::fs::write(a.join("workspace/company-code.rs"), "").unwrap();
        std::fs::write(home.join("raw/baseline/outbound.ndjson"), "").unwrap();
        std::fs::write(home.join("run/outbox/p.json"), "").unwrap();
        std::fs::write(home.join("bin/summon-sidecar"), "").unwrap();
        std::fs::write(home.join("tasks/SUM-1/task.yaml"), "").unwrap();
        std::fs::write(home.join("tasks/SUM-1/runs/run-x.meta.yaml"), "").unwrap();
        std::fs::write(home.join("tasks/.locks/x"), "").unwrap();
        std::fs::write(home.join("schedules.yaml"), "").unwrap();
        std::fs::write(home.join("schedules-archive.yaml"), "").unwrap();
        std::fs::write(home.join("schedules/.counter"), "").unwrap();
        std::fs::write(home.join("schedules/.state.lock"), "").unwrap();
        std::fs::write(home.join("runtime/emoji-reactions.md"), "").unwrap();

        let git = |args: &[&str]| {
            Command::new("git")
                .current_dir(&home)
                .args(args)
                .output()
                .expect("git 应在 PATH")
        };
        assert!(git(&["init", "-q", "."]).status.success());
        assert!(git(&["config", "core.excludesfile", "/dev/null"])
            .status
            .success());

        let ignored = |p: &str| git(&["check-ignore", "-q", p]).status.success();

        // 挡住:密钥、resume 锚点、引用缓存、agent 工作区、未脱敏录制、生成物、锁、各层 .DS_Store。
        // 注意 threads/ 不在此列 —— v2 起放行(见下方「放行」列表)。
        for p in [
            "agents/arcadia/.env",
            "agents/arcadia/sessions/g1/session_id",
            "agents/arcadia/inbound/quotes/om_x.md",
            "agents/arcadia/runtime/emoji.md",
            "agents/arcadia/workspace/company-code.rs",
            "raw/baseline/outbound.ndjson",
            "run/outbox/p.json",
            "bin/summon-sidecar",
            "runtime/emoji-reactions.md",
            "tasks/.locks/x",
            "schedules/.state.lock",
            ".DS_Store",
            "agents/.DS_Store",
            "agents/arcadia/.DS_Store",
        ] {
            assert!(ignored(p), "应挡住但漏了: {p}");
        }
        // 放行:不可再生的东西 —— 含非手写的会话转录与 Task recap。
        for p in [
            "config.yaml",
            "CLAUDE.md",
            "agents/arcadia/SOUL.md",
            "agents/arcadia/agent.yaml",
            "agents/arcadia/CLAUDE.md",
            "agents/arcadia/memory/MEMORY.md",
            "agents/arcadia/threads/g1/transcript.md",
            "tasks/SUM-1/task.yaml",
            "tasks/SUM-1/runs/run-x.meta.yaml",
            "schedules.yaml",
            "schedules-archive.yaml",
            "schedules/.counter",
        ] {
            assert!(!ignored(p), "应放行但挡住了: {p}");
        }

        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn config_example_is_copied_once() {
        let home =
            std::env::temp_dir().join(format!("summon-config-example-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);

        write_config_example_if_absent(&home).unwrap();
        let config = home.join("config.yaml");
        let initial = std::fs::read_to_string(&config).unwrap();
        assert!(initial.contains("summon 全局配置"));
        assert!(initial.contains("messaging:"));

        std::fs::write(&config, "custom\n").unwrap();
        write_config_example_if_absent(&home).unwrap();
        assert_eq!(std::fs::read_to_string(&config).unwrap(), "custom\n");

        let _ = std::fs::remove_dir_all(&home);
    }

    // 需要本机 git;默认 ignore,跑:cargo test -p summon -- --ignored
    #[test]
    #[ignore]
    fn ensure_summon_repo_inits_and_respects_existing_gitignore() {
        let home = std::env::temp_dir().join(format!("summon-init-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);

        ensure_summon_repo(&home).unwrap();
        assert!(home.join(".git").exists(), "应 git init");
        assert!(std::fs::read_to_string(home.join(".gitignore"))
            .unwrap()
            .contains(".env"));

        // 幂等 + 不覆盖:用户改了 .gitignore,再调一次保持原样、不报错
        std::fs::write(home.join(".gitignore"), "custom\n").unwrap();
        ensure_summon_repo(&home).unwrap();
        assert_eq!(
            std::fs::read_to_string(home.join(".gitignore")).unwrap(),
            "custom\n"
        );

        let _ = std::fs::remove_dir_all(&home);
    }
}
