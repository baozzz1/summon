//! `!` 开头的 shell 直通:原样进 claude 的 bash 模式,不套飞书消息信封。

use crate::config::registry::CaptureMode;

/// 把一条飞书文本判成「shell 直通」命令,返回 `!` 之后的原文。
///
/// 保留 `!` 后的前导空格:shell 自己会忽略它,而原样保留能让注入行与用户所见逐字节一致,
/// 回执和转录比对都不必再考虑一次归一化。
///
/// 三条闸缺一不可:
/// - **单行** —— claude 的 bash 模式一行一条命令,换行即提交,多行会被拆成两条分别执行。
/// - **`!` 是首字符** —— 模式切换认的就是输入框第一个字符,前面有任何东西都不触发。
/// - **`!` 后要像个命令** —— 首个非空字符是 ASCII 字母数字或 `/._-`。少了这条,群里一句
///   「!!!」或「!这个不对」都会被当成 shell 命令送去鉴权,再被 operate 闸拒掉,平白多一条报错。
pub fn bang_command(text: &str) -> Option<&str> {
    let text = text.trim();
    if text.contains('\n') {
        return None;
    }
    let rest = text.strip_prefix('!')?;
    let first = rest.trim_start().chars().next()?;
    if !(first.is_ascii_alphanumeric() || matches!(first, '/' | '.' | '_' | '-')) {
        return None;
    }
    Some(rest)
}

/// 哪些 backend 有 `!` 可进的 bash 模式。只有 claude 的 tmux TUI 实测支持(2026-09-02);
/// 别处的 `!cmd` 只是一句普通话,必须按普通消息走 —— 拒绝它等于凭空多一条报错。
pub fn shell_passthrough_supported(cli: crate::driver::CliDriver, mode: CaptureMode) -> bool {
    cli == crate::driver::CliDriver::Claude && mode == CaptureMode::Transcript
}

/// 一条消息在 shell 直通上的准入结论。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BangAdmission {
    /// 不是 shell 直通(或这个 backend 没有 bash 模式):按普通消息走。
    NotApplicable,
    /// 是 shell 直通,但发起人不在 operate 名单:拒绝并回执,**绝不降级成普通消息** ——
    /// 那等于让模型代跑同一条命令,operate 这道闸就白设了。
    Denied,
    /// 准入通过:原文零信封注入。
    Admitted,
}

/// shell 直通的准入判定。`can_operate` 由调用方用**入站事件自带的身份**冻结后传入,
/// 这里不碰网络 —— 与 slash 的准入同一个理由:一次联系人 HTTP 会阻塞整个 chat 的派发。
pub fn bang_admission(
    text: &str,
    cli: crate::driver::CliDriver,
    mode: CaptureMode,
    can_operate: bool,
) -> BangAdmission {
    if bang_command(text).is_none() || !shell_passthrough_supported(cli, mode) {
        return BangAdmission::NotApplicable;
    }
    if can_operate {
        BangAdmission::Admitted
    } else {
        BangAdmission::Denied
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::driver::CliDriver;

    #[test]
    fn only_claude_on_a_tmux_tui_has_a_bash_mode_to_pass_through_to() {
        assert!(shell_passthrough_supported(
            CliDriver::Claude,
            CaptureMode::Transcript
        ));
        assert!(!shell_passthrough_supported(
            CliDriver::Codex,
            CaptureMode::Rpc
        ));
        assert!(!shell_passthrough_supported(
            CliDriver::Cursor,
            CaptureMode::Rpc
        ));
        assert!(!shell_passthrough_supported(
            CliDriver::Claude,
            CaptureMode::Rpc
        ));
    }

    /// operate 不通过时必须是 Denied 而不是 NotApplicable:后者会把命令降级成普通消息,
    /// 模型很可能照着执行一遍 —— 那条闸就等于没设。
    #[test]
    fn an_outsider_is_denied_rather_than_quietly_downgraded() {
        assert_eq!(
            bang_admission(
                "!rm -rf /tmp/x",
                CliDriver::Claude,
                CaptureMode::Transcript,
                false
            ),
            BangAdmission::Denied
        );
        assert_eq!(
            bang_admission(
                "!rm -rf /tmp/x",
                CliDriver::Claude,
                CaptureMode::Transcript,
                true
            ),
            BangAdmission::Admitted
        );
    }

    /// 没有 bash 模式的 backend 上,`!cmd` 只是一句普通话 —— 连鉴权都不该触发。
    #[test]
    fn a_backend_without_bash_mode_never_denies_anything() {
        assert_eq!(
            bang_admission("!ls", CliDriver::Codex, CaptureMode::Rpc, false),
            BangAdmission::NotApplicable
        );
    }

    #[test]
    fn an_ordinary_sentence_is_not_subject_to_the_operate_gate() {
        assert_eq!(
            bang_admission("!!!", CliDriver::Claude, CaptureMode::Transcript, false),
            BangAdmission::NotApplicable
        );
    }

    #[test]
    fn a_single_line_bang_is_a_shell_passthrough() {
        assert_eq!(bang_command("! gh auth login"), Some(" gh auth login"));
        assert_eq!(bang_command("!ls -la"), Some("ls -la"));
        assert_eq!(bang_command("!./script.sh"), Some("./script.sh"));
        assert_eq!(bang_command("  !ls  "), Some("ls"));
    }

    /// 中文聊天里 `!` 开头的语气句远比 shell 命令常见。判据认不出命令就必须放手,
    /// 让它按普通消息走 —— 否则用户只是想强调一句话,却收到一条 operate 权限报错。
    #[test]
    fn an_exclamation_that_is_not_a_command_stays_an_ordinary_message() {
        for text in ["!!!", "!你好", "! 这个不对", "!", "! ", "!?"] {
            assert_eq!(bang_command(text), None, "不该判成 shell 命令:{text}");
        }
    }

    /// bash 模式一行一条:多行放进去第二行会被当成另一条命令执行,而用户以为只发了一条。
    #[test]
    fn a_multiline_text_is_never_a_shell_passthrough() {
        assert_eq!(bang_command("!echo a\n!echo b"), None);
        assert_eq!(bang_command("!echo a\nrm -rf /tmp/x"), None);
    }

    #[test]
    fn text_without_a_leading_bang_is_untouched() {
        assert_eq!(bang_command("ls -la"), None);
        assert_eq!(bang_command("/help"), None);
        assert_eq!(bang_command("这条消息里有 ! 但不在开头"), None);
    }
}
