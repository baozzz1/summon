use crate::driver::{LaunchModel, ModelEffortError};

/// claude 的独立 effort 合法集(CLI 级粗粒度;per-model 限制如 haiku 拒 effort、remote 无 max
/// 不在此硬校,交 CLI 自报,见设计 §3.2)。effort 走独立 `--effort` flag,与 model 正交。
pub const EFFORTS: &[&str] = &["low", "medium", "high", "xhigh", "max"];

/// claude 内置接受的模型别名:`/model` 与 `--model` 不做服务端探测就认。取自 claude 2.1.304 二进制
/// 里的别名表(与 `claude-opus-4-8` 这类全名表相邻的那一张);随 claude 升级可能增减。
pub const MODEL_ALIASES: &[&str] = &[
    "sonnet",
    "opus",
    "haiku",
    "fable",
    "best",
    "sonnet[1m]",
    "opus[1m]",
    "fable[1m]",
    "opusplan",
];

/// claude:model→`--model <id>`,effort→`--effort <v>`(两个正交 top-level flag)。
pub fn resolve_launch(
    model: Option<&str>,
    effort: Option<&str>,
) -> Result<LaunchModel, ModelEffortError> {
    let mut args = Vec::new();
    if let Some(m) = model {
        args.push("--model".to_string());
        args.push(m.to_string());
    }
    if let Some(e) = effort {
        if !EFFORTS.contains(&e) {
            return Err(ModelEffortError::InvalidEffort {
                cli: "claude",
                value: e.to_string(),
                allowed: EFFORTS.join("/"),
            });
        }
        args.push("--effort".to_string());
        args.push(e.to_string());
    }
    Ok(LaunchModel { args })
}

pub fn default_cmd() -> &'static str {
    "claude"
}

pub fn captures() -> bool {
    true
}

/// claude TUI 的输入注入走 **bracketed paste**(tmux `paste-buffer -p`),不是逐键 `send-keys`。
/// 一条 prompt 是一段要原样收下的正文,不是一串按键 —— 粘贴语义把它作为一个整体交给 TUI,
/// 不必依赖「逐键流按什么节奏被读走」。2026-08-28 有三条飞书消息掐掉开头才进 claude
/// (转录里只剩尾部,切点落在 UTF-8 字符中间),逐键路径给不出这不会再发生的保证。
/// 真机实测:`/status` 这类命令经粘贴进去照样弹补全面板,3 KB 正文逐字节进转录。
pub fn injects_by_paste() -> bool {
    true
}

pub fn resume_cmd(base: &str, id: &str) -> String {
    format!("{base} --resume {id}")
}

pub fn fresh_cmd_with_id(base: &str, id: &str) -> String {
    format!("{base} --session-id {id}")
}

/// 从 `parent` fork 出一条**交互式**会话,并把它钉在预铸的 `id` 上。
///
/// 与 [`fork_headless_command_args`] 是同一个 CLI 能力的两种形态,差别只有「跑在 TUI 里还是
/// 无头一轮」。真机验过(devbox 2026-09-03,带凭据的真 TTY):TUI 正常起、父会话历史完整继承、
/// 新 sid 生效,且**父转录文件 sha256 前后一致** —— fork 只读父。
///
/// `--session-id` 必须与 `--fork-session` 同时给:后者单独用会让 claude 自己生成新 id,
/// 而 summon 需要那个 id 是自己预铸的(要写进 `.session` 才接得回来)。
pub fn fork_cmd_with_id(base: &str, parent: &str, id: &str) -> String {
    format!("{base} --session-id {id} --resume {parent} --fork-session")
}

pub fn system_prompt_file_flag() -> Option<&'static str> {
    Some("--append-system-prompt-file")
}

/// 交互式压缩上下文的 slash 命令(claude 跑 tmux 时往会话发这条)。
pub fn compact_command() -> &'static str {
    "/compact"
}

pub fn interrupts_followup() -> bool {
    true
}

/// 从已有 Claude 会话 fork 一条**无头**新会话并发送首条 prompt 的完整 argv。
///
/// 这条路径专供额度恢复等「保留父会话、在新 session 继续」的调用方，刻意不复用普通
/// [`headless_args`]：后者是 summon 自有 agent 的执行路径，历史上默认开启
/// `bypassPermissions`；外部会话恢复若照搬会把用户原权限静默提升。这里 `None`、空串和 Claude
/// hook 上报的 `default` 都表示不传权限 flag，只有调用方显式传入具体模式时才下发。
pub fn fork_headless_command_args(
    parent: &str,
    prompt: &str,
    permission_mode: Option<&str>,
) -> Vec<String> {
    let mut args = vec![
        "-p".to_string(),
        prompt.to_string(),
        "--resume".to_string(),
        parent.to_string(),
        "--fork-session".to_string(),
        "--output-format".to_string(),
        "stream-json".to_string(),
        "--verbose".to_string(),
    ];
    let permission_mode = permission_mode
        .map(str::trim)
        .filter(|mode| !mode.is_empty() && *mode != "default");
    if let Some(mode) = permission_mode {
        args.push("--permission-mode".to_string());
        args.push(mode.to_string());
    }
    args
}

/// 无头一轮的 flag(不含 `-p <prompt>`,由 runner 在前面补;prompt 走 `-p` 位置参数)。
/// resume=Some → 续接已有 session;sys_prompt_file=Some → 每轮附加系统提示文件(不污染历史)。
pub fn headless_args(resume: Option<&str>, sys_prompt_file: Option<&str>) -> Vec<String> {
    let mut a = vec![
        "--output-format".to_string(),
        "stream-json".to_string(),
        "--verbose".to_string(),
        "--permission-mode".to_string(),
        "bypassPermissions".to_string(),
    ];
    if let Some(sid) = resume {
        a.push("--resume".to_string());
        a.push(sid.to_string());
    }
    if let Some(f) = sys_prompt_file {
        a.push("--append-system-prompt-file".to_string());
        a.push(f.to_string());
    }
    a
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 无头 claude 的 argv 在这里独立构造,**不经过** `Session::augment_launch` ——
    /// PreToolUse 执行闸的 `--settings` 因此挂不上去,外部发起人在这种形态下不受执行闸限制。
    /// 这是刻意不补的已知缺口(今天零个 agent 配 headless claude,见
    /// `docs/spec/2026-08-05-feishu-identity-and-tool-guard-design.md` 的「已知代价」)。
    /// 断言在这里,是为了让「哪天真加上了」这件事必须连带改 spec,而不是悄悄漂移。
    #[test]
    fn headless_args_carry_no_tool_guard_settings() {
        for args in [
            headless_args(None, None),
            headless_args(Some("sid-1"), Some("/tmp/primer.txt")),
        ] {
            assert!(
                !args.iter().any(|arg| arg == "--settings"),
                "无头路今天挂不上执行闸;真要支持请同步改 spec 的已知代价: {args:?}"
            );
            assert!(!args.iter().any(|arg| arg.contains("guard-settings.json")));
        }
    }

    #[test]
    fn fork_headless_defaults_to_no_permission_override() {
        let args = fork_headless_command_args("parent-1", "继续", None);
        assert_eq!(
            args,
            [
                "-p",
                "继续",
                "--resume",
                "parent-1",
                "--fork-session",
                "--output-format",
                "stream-json",
                "--verbose",
            ]
        );
        assert!(!args.iter().any(|arg| arg == "bypassPermissions"));
    }

    #[test]
    fn fork_headless_treats_default_and_blank_as_no_override() {
        for mode in [Some("default"), Some(""), Some("  ")] {
            let args = fork_headless_command_args("parent-1", "继续", mode);
            assert!(!args.iter().any(|arg| arg == "--permission-mode"));
        }
    }

    #[test]
    fn fork_headless_only_bypasses_when_explicitly_requested() {
        let args = fork_headless_command_args("parent-1", "继续", Some("bypassPermissions"));
        assert_eq!(
            &args[args.len() - 2..],
            ["--permission-mode", "bypassPermissions"]
        );
    }
}
