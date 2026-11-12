//! tmux 冷启命令的「环境不进 argv」原语(纯字符串,不碰进程/文件)。
//!
//! why:`tmux new-session … <cmd>` 的整条命令行会成为 **tmux server 进程的 argv**——macOS 上
//! tmux 改不掉自己的 argv,而 server 是长驻进程(`ps` 里 ppid=1 那个),于是
//! `env -i PATH="…" FEISHU_APP_SECRET="…" claude …` 这种前缀会把 agent 的飞书 app_secret
//! **永久**摊在 `ps -eo command` 里。agent 子进程与 daemon 同 uid、PATH 里必有 `ps`,任何一个
//! agent 都能顺手读到别人的凭据 —— `env -i` 在环境层面做到的身份隔离被这条旁路整个绕开。
//!
//! 改法:键值对写进一份 0600 的**一次性** env 文件,命令行里只留**文件路径**;pane 里的 `sh`
//! 先 `set -a` 把它载进环境、随即删掉文件,再 `exec` 真正的 CLI。于是
//! - argv(tmux server / sh / CLI 三者)里只剩路径,无任何凭据值;
//! - `env -i` 清空继承这条语义原样保留(载入的是文件里那份,只含本 agent 自己的凭据);
//! - `exec` 链不新增常驻进程,pane_pid 仍是 CLI 本身。
//!
//! 残留风险(明说):文件在盘上存在几十~几百毫秒,同 uid 可读。但同一份 secret 本来就长期躺在
//! `~/.summon/agents/<alias>/.env`(同样同 uid 可读),所以这不新增静态暴露面;真正被消除的是
//! `ps` 这条**广播**信道(agent 随手 `ps aux` 就会把别人的密钥抄进自己的上下文/转录/飞书消息)。

/// POSIX sh 单引号转义:把 `'` 换成 `'\''` 再整体包单引号。对任意字节串都成立,
/// 故命令/路径里出现引号、空格、`$`、换行都不会被二次解析。
pub fn sh_single_quote(raw: &str) -> String {
    format!("'{}'", raw.replace('\'', "'\\''"))
}

/// 是否合法的 shell 变量名(`[A-Za-z_][A-Za-z0-9_]*`)。env 文件靠 `set -a` + `.` 载入,
/// 非法名会让整个 `.` 变成语法错误、连累整次冷启,故渲染时必须先筛掉。
pub fn is_env_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes
        .next()
        .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
        && bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// 渲染 env 文件正文(每行 `KEY='value'`,配合 `set -a` 自动导出)。
/// 同名键按入参顺序**后写覆盖**,与原先 `env K=V …` 前缀的语义一致(调用方靠顺序表达优先级)。
/// 返回 `(正文, 被跳过的非法键名)`——跳过而非报错:一个手写坏了的 `runtime.env` 键不该让整个
/// 会话起不来,但必须让调用方能记日志,不能静默吞掉。
pub fn render_launch_env_file(pairs: &[(String, String)]) -> (String, Vec<String>) {
    let mut body = String::new();
    let mut skipped = Vec::new();
    for (key, value) in pairs {
        if !is_env_name(key) {
            skipped.push(key.clone());
            continue;
        }
        body.push_str(key);
        body.push('=');
        body.push_str(&sh_single_quote(value));
        body.push('\n');
    }
    (body, skipped)
}

/// argv 形态的加载器脚本(给 `sh -c <SCRIPT> <env 文件> <program> <args…>` 用):
/// `$0` = env 文件,`"$@"` = 要 exec 的 program + args。程序与参数**逐项走 argv**,不进 shell 解析。
///
/// `. "$0"` 打不开(文件已被删/写失败)时,非交互 sh 直接以非零退出 → pane 立刻消失 →
/// 上层 `wait_ready`/`has_session` 判定起会话失败。fail-closed:宁可起不来,不可裸着环境起来。
/// `rm` 用绝对路径:此刻 PATH 才刚从文件载入,不赌它一定可用。
pub const ENV_FILE_LOADER_SCRIPT: &str = "set -a; . \"$0\"; set +a; /bin/rm -f \"$0\"; exec \"$@\"";

/// 命令串形态:把一条**已经拼好的 shell 命令**包成「先载入 env 文件、删掉它,再 exec」。
/// 返回值整体作为 `tmux new-session` 的最后一个参数(tmux 会用 `/bin/sh -c` 跑它)。
///
/// 两层引号都由 [`sh_single_quote`] 兜底:外层保证整段脚本原样传进内层 sh,内层再解析 `cmd`
/// (`cmd` 本就是按「交给一个 shell 解析」的约定拼的,这里只多一次原样传递,不改变其语义)。
pub fn wrap_command_with_env_file(env_file: &str, cmd: &str) -> String {
    let quoted = sh_single_quote(env_file);
    let script = format!("set -a; . {quoted}; set +a; /bin/rm -f {quoted}; exec {cmd}");
    format!("env -i /bin/sh -c {}", sh_single_quote(&script))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_quote_survives_embedded_quotes() {
        assert_eq!(sh_single_quote("plain"), "'plain'");
        assert_eq!(sh_single_quote("a'b"), "'a'\\''b'");
        assert_eq!(sh_single_quote("$(x) `y` \"z\""), "'$(x) `y` \"z\"'");
    }

    #[test]
    fn env_name_accepts_identifiers_only() {
        assert!(is_env_name("PATH"));
        assert!(is_env_name("_x9"));
        assert!(!is_env_name(""));
        assert!(!is_env_name("9A"));
        assert!(!is_env_name("A-B"));
        assert!(!is_env_name("A B"));
    }

    #[test]
    fn render_quotes_values_and_skips_bad_keys() {
        let pairs = vec![
            ("PATH".to_string(), "/bin:/usr/bin".to_string()),
            ("FEISHU_APP_SECRET".to_string(), "it's-a-value".to_string()),
            ("BAD-KEY".to_string(), "x".to_string()),
        ];
        let (body, skipped) = render_launch_env_file(&pairs);
        assert_eq!(
            body,
            "PATH='/bin:/usr/bin'\nFEISHU_APP_SECRET='it'\\''s-a-value'\n"
        );
        assert_eq!(skipped, vec!["BAD-KEY".to_string()]);
    }

    #[test]
    fn wrapped_command_keeps_secret_out_and_still_execs() {
        let out = wrap_command_with_env_file("/tmp/a b/launch.env", "claude --session-id x");
        assert!(out.starts_with("env -i /bin/sh -c "), "{out}");
        assert!(out.contains("/tmp/a b/launch.env"), "{out}");
        assert!(out.contains("exec claude --session-id x"), "{out}");
        // 载入后必须删文件,别把凭据留在盘上。
        assert!(out.contains("/bin/rm -f"), "{out}");
    }

    /// 加载器脚本的 `$0`/`"$@"` 契约:`sh -c <script> <envfile> <program> <args…>` 下
    /// `$0` 是 env 文件、`"$@"` 恰好是 program + args(**不需要 shift**)。写死在测试里防手滑改错。
    #[test]
    fn loader_script_uses_arg0_as_env_file_and_rest_as_command() {
        assert!(ENV_FILE_LOADER_SCRIPT.contains(". \"$0\""));
        assert!(ENV_FILE_LOADER_SCRIPT.contains("/bin/rm -f \"$0\""));
        assert!(ENV_FILE_LOADER_SCRIPT.ends_with("exec \"$@\""));
        assert!(
            !ENV_FILE_LOADER_SCRIPT.contains("shift"),
            "$0 不在 $@ 里,加 shift 会吃掉 program"
        );
    }
}
