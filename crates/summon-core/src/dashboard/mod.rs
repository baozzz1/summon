//! 控制台(Dashboard)的纯逻辑:字段表、判权、原文校验、脱敏、飞书效果预览。不碰文件、网络与进程,
//! IO 与接口接线在 `summon` crate 的 `runtime::dashboard`。
//! 设计见 `docs/spec/2026-10-20-dashboard-console-design.md`「配置模型」「权限」两节。

pub mod authz;
pub mod fields;
pub mod preview;
pub mod raw_check;
pub mod redact;

/// 原文模式能编辑的两种 YAML 文件。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RawKind {
    /// `~/.summon/config.yaml`
    Config,
    /// `agents/<alias>/agent.yaml`
    Agent,
}

/// 拿本机真实配置的副本跑一遍脱敏往返与原文校验:`SUMMON_YAML_SAMPLES=<目录>`,目录里
/// `config.yaml` 按整机配置、其余 `*.yaml` 按 agent.yaml 处理。副本不入库;只打印键路径与
/// 提示文案,不打印任何值。
#[cfg(test)]
mod local_samples {
    use super::{raw_check, redact, RawKind};

    #[test]
    #[ignore]
    fn redact_and_check_local_samples() {
        let Ok(dir) = std::env::var("SUMMON_YAML_SAMPLES") else {
            return;
        };
        for entry in std::fs::read_dir(&dir).expect("样本目录") {
            let file = entry.expect("目录项").path();
            if file.extension().and_then(|e| e.to_str()) != Some("yaml") {
                continue;
            }
            let src = std::fs::read_to_string(&file).expect("读样本");
            let kind = if file.file_name().and_then(|n| n.to_str()) == Some("config.yaml") {
                RawKind::Config
            } else {
                RawKind::Agent
            };
            let (text, hidden) = redact::redact(kind, &src).expect("真实文件应能脱敏");
            assert_eq!(
                redact::restore(kind, &text, &hidden).unwrap(),
                src,
                "{file:?}"
            );
            for h in &hidden {
                if let Ok(serde_yaml::Value::String(secret)) = serde_yaml::from_str(h.raw.trim()) {
                    assert!(
                        secret.len() < 4 || !text.contains(&secret),
                        "{file:?} 的 {} 漏出来了",
                        h.key
                    );
                }
            }
            let check = raw_check::check(kind, &src, &src);
            eprintln!(
                "{}:藏了 {} 项;错误 {} 条,警告 {} 条",
                file.display(),
                hidden.len(),
                check.errors.len(),
                check.warnings.len()
            );
            for issue in check.errors.iter().chain(&check.warnings) {
                eprintln!(
                    "  {:?} {:?} {} {:?}",
                    issue.line, issue.key, issue.message, issue.suggestion
                );
            }
            assert!(check.ok(), "{file:?} 现有文件不应有错误");
        }
    }
}
