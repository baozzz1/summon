//! 定位 Claude Code 自己的结构化转录(jsonl)的纯逻辑:把 cwd 编成工程目录名、拼出钉死 sid 的
//! 转录文件路径。Transcript 捕获源据此 tail 本轮输出(`summon` crate 的 IO 在 `capture::transcript`)。

use std::path::PathBuf;

/// 把绝对 cwd 编成 Claude 工程目录名:**凡不是 ASCII 字母/数字的字符一律 → `-`**。
/// 例:`/Users/me/.summon/run` → `-Users-me--summon-run`;`/Users/me/My_App` → `-Users-me-My-App`。
///
/// 这是**补集**规则,不是枚举。此前枚举 `/` `.`(后来补 `_`),每漏一个字符,凡路径含该字符的
/// agent 就永远定位不到自己的转录 —— 冷启 `wait_first_turn_end` 等不到 end_turn、傻等满 seed
/// 超时(现场:blue-wing 的 `My_App` 每次白等 600s)。枚举法的问题是「下一个漏的字符」永远在
/// 前面等着(空格、`+`、`@`、括号、中文…)。
///
/// 依据是实测而非猜测:扫本机 `~/.claude/projects` 全部目录名,出现过的字符**只有**字母、数字
/// 和 `-`,说明 Claude 就是把其余字符全归一成 `-`;再拿每个目录与其转录内记录的 cwd 逐一对账,
/// 148 个可比对目录里,旧枚举规则命中 72 个,本规则命中 146 个(余 2 个是 cwd 被 `/cd` 改过的
/// 历史数据,与编码规则无关)。
///
/// 入参先过 `normalize_work_dir` 做字符串归一。这里的入参是一路传下来
/// 的字符串,`/a/b/` 与 `/a/b` 是同一个目录却 munge 出两个名字。归一放在这里而不是只放在上游,
/// 是因为本函数是「cwd → Claude 目录名」的唯一翻译点:盘上已有的、别处手写的路径都经过它。
/// 保留原路径编码以查找历史目录；软链接真实路径由捕获层在冷启动预测时解析。
///
/// 即便如此这仍是对外部实现的逆向。定位侧不得只信它:见 `capture::transcript` 的按 SID 兜底搜。
pub fn project_dirname(abs_cwd: &str) -> String {
    crate::session::work_dir::normalize_work_dir(abs_cwd)
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// 钉死 session id 的转录文件路径:`~/.claude/projects/<project_dirname(work_dir)>/<sid>.jsonl`。
/// Transcript 捕获源 tail 此文件取本轮输出。**只开这一个钉死 sid 的文件,绝不取目录里「最新」的**
/// —— 多个 chat 共用同一 cwd(故同一 project 目录)时,「取最新」会读到别的 chat 的转录,造成跨会话
/// 串流(评审 G8)。返回 None 仅当拿不到 home(几乎不可能)。
pub fn claude_transcript_path(work_dir: &str, session_id: &str) -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    Some(
        home.join(".claude")
            .join("projects")
            .join(project_dirname(work_dir))
            .join(format!("{session_id}.jsonl")),
    )
}

/// 转录里的一条压缩边界。压缩完成时 claude 往转录追加一行 `compact_boundary`。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompactBoundary {
    /// `compactMetadata.postTokens`:压缩后上下文的起点占用。缺失或为 0 时是 `None`。
    pub post_tokens: Option<u64>,
    /// 这一行(连同换行)之后的字节位置,相对传入的那段字节。同一份转录里它唯一标识这条边界。
    pub end: usize,
    /// 这一行的 `timestamp`(Unix 毫秒),读不出时是 `None`。
    pub at_ms: Option<i64>,
}

/// `bytes` 里最后一条压缩边界。段首被截断的半行、写入中的半行解析不了,自然跳过。
pub fn last_compact_boundary(bytes: &[u8]) -> Option<CompactBoundary> {
    const MARK: &[u8] = b"compact_boundary";
    // 从尾部逐行往前找:转录可能有几 MB,只解析含边界标记的行。
    let mut line_end = bytes.len();
    loop {
        let line_start = bytes[..line_end]
            .iter()
            .rposition(|&b| b == b'\n')
            .map_or(0, |at| at + 1);
        let line = &bytes[line_start..line_end];
        if line.windows(MARK.len()).any(|window| window == MARK) {
            if let Ok(entry) = serde_json::from_slice::<serde_json::Value>(line) {
                if entry.get("type").and_then(serde_json::Value::as_str) == Some("system")
                    && entry.get("subtype").and_then(serde_json::Value::as_str)
                        == Some("compact_boundary")
                {
                    return Some(CompactBoundary {
                        post_tokens: entry
                            .pointer("/compactMetadata/postTokens")
                            .and_then(serde_json::Value::as_u64)
                            .filter(|&tokens| tokens > 0),
                        end: (line_end + 1).min(bytes.len()),
                        at_ms: entry
                            .get("timestamp")
                            .and_then(serde_json::Value::as_str)
                            .and_then(|at| chrono::DateTime::parse_from_rfc3339(at).ok())
                            .map(|at| at.timestamp_millis()),
                    });
                }
            }
        }
        if line_start == 0 {
            return None;
        }
        line_end = line_start - 1;
    }
}

/// 会话第一轮请求的输入量(input + 两块缓存):系统提示、工具定义、记忆文件这些每轮都带的基础
/// 占用,加上第一条消息。`head` 是转录开头一段;子 agent 的记录不算。首条带用量的 assistant 之前
/// 出现压缩边界或压缩摘要行时,那一轮已经带着摘要,当不了基础占用,返回 `None`。
pub fn first_turn_context_tokens(head: &[u8]) -> Option<u64> {
    for entry in head
        .split(|&b| b == b'\n')
        .filter_map(|line| serde_json::from_slice::<serde_json::Value>(line).ok())
    {
        let kind = entry.get("type").and_then(serde_json::Value::as_str);
        let compacted = (kind == Some("system")
            && entry.get("subtype").and_then(serde_json::Value::as_str)
                == Some("compact_boundary"))
            || entry
                .get("isCompactSummary")
                .and_then(serde_json::Value::as_bool)
                == Some(true);
        if compacted {
            return None;
        }
        if kind != Some("assistant")
            || entry
                .get("isSidechain")
                .and_then(serde_json::Value::as_bool)
                == Some(true)
        {
            continue;
        }
        let Some(usage) = entry.pointer("/message/usage") else {
            continue;
        };
        let pick = |key: &str| {
            usage
                .get(key)
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0)
        };
        let total = pick("input_tokens")
            + pick("cache_read_input_tokens")
            + pick("cache_creation_input_tokens");
        if total > 0 {
            return Some(total);
        }
    }
    None
}

/// claude 对注入的 `/model <id>` 的回应，取自转录里这条命令之后的 `local-command-stdout`。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ModelSwitchReply {
    /// 「Set model to …」:已切换。
    Applied(String),
    /// 「Model '…' not found」「… is not in the list of available models」:claude 不认，没切。
    Refused(String),
    /// 别的输出，判不出切没切。
    Other(String),
}

/// `appended` 是注入 `/model <model>` 之前记下的位置之后追加的转录。找到参数恰为 `model` 的
/// `/model` 命令记录，取它之后第一条命令输出判切换结果;还没写出来时返回 `None`。
///
/// claude 2.1.304 两种结果的落盘形态不同：失败是 `system`/`local_command` 记录，成功是 `user`
/// 记录;两者的正文都是 `<command-name>` 一行加一行 `<local-command-stdout>`,所以只认正文。
pub fn model_switch_reply(appended: &[u8], model: &str) -> Option<ModelSwitchReply> {
    let mut command_seen = false;
    for entry in appended
        .split(|&b| b == b'\n')
        .filter_map(|line| serde_json::from_slice::<serde_json::Value>(line).ok())
    {
        let Some(text) = entry_text(&entry) else {
            continue;
        };
        if !command_seen {
            command_seen = tag_text(&text, "command-name").as_deref() == Some("/model")
                && tag_text(&text, "command-args").as_deref().map(str::trim) == Some(model);
            continue;
        }
        if let Some(output) = tag_text(&text, "local-command-stdout") {
            let output = output.trim().to_string();
            return Some(if output.starts_with("Set model to") {
                ModelSwitchReply::Applied(output)
            } else if output.contains("not found")
                || output.contains("is not in the list of available models")
            {
                ModelSwitchReply::Refused(output)
            } else {
                ModelSwitchReply::Other(output)
            });
        }
    }
    None
}

/// 一条转录记录的正文：`content` 字符串，或 `message.content` 的字符串 / 文本块。
fn entry_text(entry: &serde_json::Value) -> Option<String> {
    if let Some(text) = entry.get("content").and_then(serde_json::Value::as_str) {
        return Some(text.to_string());
    }
    match entry.pointer("/message/content")? {
        serde_json::Value::String(text) => Some(text.clone()),
        serde_json::Value::Array(blocks) => Some(
            blocks
                .iter()
                .filter_map(|block| block.get("text").and_then(serde_json::Value::as_str))
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        _ => None,
    }
}

/// `<tag>…</tag>` 之间的文字。
fn tag_text(text: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let start = text.find(&open)? + open.len();
    let end = text[start..].find(&format!("</{tag}>"))? + start;
    Some(text[start..end].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// claude 2.1.304 的两种落盘形态，取自真实转录(去掉了与判定无关的字段)。
    fn not_found_lines(model: &str) -> String {
        format!(
            "{{\"type\":\"system\",\"subtype\":\"local_command\",\"content\":\"<command-name>/model</command-name>\\n            <command-message>model</command-message>\\n            <command-args>{model}</command-args>\"}}\n\
             {{\"type\":\"system\",\"subtype\":\"local_command\",\"content\":\"<local-command-stdout>Model '{model}' not found</local-command-stdout>\",\"commandRun\":{{\"command\":\"model\",\"args\":\"{model}\"}}}}\n"
        )
    }

    fn applied_lines(model: &str) -> String {
        format!(
            "{{\"type\":\"user\",\"isMeta\":true,\"message\":{{\"role\":\"user\",\"content\":\"<local-command-caveat>The command below was run directly in Claude Code</local-command-caveat>\"}}}}\n\
             {{\"type\":\"user\",\"message\":{{\"role\":\"user\",\"content\":\"<command-name>/model</command-name>\\n            <command-message>model</command-message>\\n            <command-args>{model}</command-args>\"}}}}\n\
             {{\"type\":\"user\",\"message\":{{\"role\":\"user\",\"content\":\"<local-command-stdout>Set model to `Opus 5.5` and saved as your default for new sessions</local-command-stdout>\"}}}}\n"
        )
    }

    #[test]
    fn model_switch_reply_reads_both_outcomes_claude_writes() {
        assert_eq!(
            model_switch_reply(applied_lines("opus").as_bytes(), "opus"),
            Some(ModelSwitchReply::Applied(
                "Set model to `Opus 5.5` and saved as your default for new sessions".into()
            ))
        );
        assert_eq!(
            model_switch_reply(not_found_lines("gpt-x").as_bytes(), "gpt-x"),
            Some(ModelSwitchReply::Refused("Model 'gpt-x' not found".into()))
        );
        // 服务端探测不认时会附一个建议;组织策略不允许时是另一句。
        for refusal in [
            "Model 'gpt-x' not found. Try 'gpt-y' instead",
            "Model 'gpt-x' is not in the list of available models",
        ] {
            let lines = not_found_lines("gpt-x").replace("Model 'gpt-x' not found", refusal);
            assert_eq!(
                model_switch_reply(lines.as_bytes(), "gpt-x"),
                Some(ModelSwitchReply::Refused(refusal.into()))
            );
        }
    }

    #[test]
    fn model_switch_reply_waits_for_this_command_and_its_output() {
        // 命令记录还没写出来，或只写出了命令、输出还没到。
        assert_eq!(model_switch_reply(b"", "opus"), None);
        let applied = applied_lines("opus");
        let command_only: String = applied.lines().take(2).map(|l| format!("{l}\n")).collect();
        assert_eq!(model_switch_reply(command_only.as_bytes(), "opus"), None);
        // 别的模型的 /model 记录不算这一次的回应。
        assert_eq!(model_switch_reply(applied.as_bytes(), "sonnet"), None);
        // 输出认不出时如实交回，不当成成功。
        let odd = applied.replace(
            "Set model to `Opus 5.5` and saved as your default for new sessions",
            "Something else",
        );
        assert_eq!(
            model_switch_reply(odd.as_bytes(), "opus"),
            Some(ModelSwitchReply::Other("Something else".into()))
        );
    }

    #[test]
    fn first_turn_context_tokens_skips_side_chains_and_empty_usage() {
        let head = concat!(
            r#"{"type":"user","message":{"content":"hi"}}"#,
            "\n",
            r#"{"type":"assistant","isSidechain":true,"message":{"usage":{"input_tokens":5}}}"#,
            "\n",
            r#"{"type":"assistant","message":{"usage":{"input_tokens":0}}}"#,
            "\n",
            r#"{"type":"assistant","message":{"usage":{"input_tokens":3,"cache_creation_input_tokens":55000,"cache_read_input_tokens":3000}}}"#,
            "\n",
            r#"{"type":"assistant","message":{"usage":{"input_tokens":90000}}}"#,
            "\n",
        );
        assert_eq!(first_turn_context_tokens(head.as_bytes()), Some(58_003));
        assert_eq!(first_turn_context_tokens(b"{\"type\":\"user\"}\n"), None);
    }

    #[test]
    fn first_turn_context_tokens_refuses_a_transcript_that_starts_compacted() {
        let first = r#"{"type":"assistant","message":{"usage":{"input_tokens":80000}}}"#;
        let boundary = r#"{"type":"system","subtype":"compact_boundary","compactMetadata":{"postTokens":20000}}"#;
        let summary = r#"{"type":"user","isCompactSummary":true,"message":{"content":"summary"}}"#;
        for opening in [boundary, summary] {
            let head = format!("{opening}\n{first}\n");
            assert_eq!(
                first_turn_context_tokens(head.as_bytes()),
                None,
                "{opening}"
            );
        }
        // 压缩发生在第一轮之后,不影响第一轮的取值。
        let later = format!("{first}\n{boundary}\n");
        assert_eq!(first_turn_context_tokens(later.as_bytes()), Some(80_000));
    }

    #[test]
    fn last_compact_boundary_reads_the_last_boundary_and_where_it_ends() {
        let boundary = |post: u64| {
            format!(
                r#"{{"type":"system","subtype":"compact_boundary","timestamp":"2026-10-20T10:00:46.342Z","compactMetadata":{{"trigger":"manual","preTokens":916923,"postTokens":{post}}}}}"#
            )
        };
        let assistant = r#"{"type":"assistant","message":{"usage":{"input_tokens":900000}}}"#;
        let head = format!("tail of a line\n{assistant}\n{}\n", boundary(30_000));
        let last = format!("{}\n", boundary(20_854));
        let appended =
            format!("{head}{last}{{\"type\":\"user\",\"isCompactSummary\":true}}\n{{\"type\":");
        assert_eq!(
            last_compact_boundary(appended.as_bytes()),
            Some(CompactBoundary {
                post_tokens: Some(20_854),
                end: head.len() + last.len(),
                at_ms: Some(1_792_490_446_342),
            })
        );
        assert_eq!(
            last_compact_boundary(format!("{assistant}\n").as_bytes()),
            None
        );
        let unterminated = boundary(0);
        assert_eq!(
            last_compact_boundary(unterminated.as_bytes()),
            Some(CompactBoundary {
                post_tokens: None,
                end: unterminated.len(),
                at_ms: Some(1_792_490_446_342),
            }),
            "0 不是可信占用;还没写换行的整行照样认,结束位置就是段尾"
        );
        assert_eq!(last_compact_boundary(b""), None);
    }

    #[test]
    fn transcript_path_composes_pinned_sid_suffix() {
        // 钉死 sid 的后缀必须是 .claude/projects/<编码目录>/<sid>.jsonl,且文件名恰为传入 sid。
        let p = claude_transcript_path("/Users/me/Desktop/personal/summon", "abc-123")
            .expect("有 home");
        let s = p.to_string_lossy();
        assert!(
            s.ends_with(".claude/projects/-Users-me-Desktop-personal-summon/abc-123.jsonl"),
            "实得: {s}"
        );
        // 文件名 = sid.jsonl(绝不取目录最新)
        assert_eq!(
            p.file_name().and_then(|n| n.to_str()),
            Some("abc-123.jsonl")
        );
    }

    #[test]
    fn project_dirname_replaces_slash_and_dot() {
        assert_eq!(project_dirname("/Users/me/workflow"), "-Users-me-workflow");
        assert_eq!(
            project_dirname("/Users/me/.summon/run"),
            "-Users-me--summon-run"
        );
        assert_eq!(project_dirname("/a/b-c/d.e"), "-a-b-c-d-e");
    }

    /// 回归(2026-07-22 现场):blue-wing 每次冷启都要傻等满 600s seed 超时才降级,第一条消息
    /// 被拖十分钟。根因就在这里——工作目录 `~/Desktop/My_App/code-review` 带下划线,而本函数
    /// 只换 `/` 和 `.`,算出 `-…-My_App-code-review`;claude 实际写在 `-…-My-App-code-review`。
    /// 路径不存在 → `wait_first_turn_end` 永远等不到 end_turn。
    ///
    /// 不是个例:拿本机 `~/.claude/projects` 全量目录与各自转录里记的 cwd 对账,148 个可比对目录
    /// 里旧规则只命中 72 个,补上 `_` 后命中 146 个(余 2 个是 cwd 被 /cd 改过的历史数据,非规则问题)。
    #[test]
    fn project_dirname_also_replaces_underscore() {
        assert_eq!(
            project_dirname("/Users/me/Desktop/My_App/code-review"),
            "-Users-me-Desktop-My-App-code-review"
        );
        assert_eq!(project_dirname("/a/b_c_d"), "-a-b-c-d");
    }

    /// 回归(2026-09-08 现场):Demo开发群的 `.cwd` 里存着 `/Users/example/Desktop/My_App/`,
    /// 尾斜杠被 munge 成尾 `-`,summon 于是死盯 `-Users-example-Desktop-My-App-`,而 claude
    /// 拿到的是内核规范化后的 cwd、写在 `-Users-example-Desktop-My-App`。冷启首轮转录尚未创建,
    /// 存在性自愈无从校验,只能保守采信启动目录 —— 于是整轮等到超时,产出发不回群里。
    ///
    /// 同一条链路第三次栽在「summon 算的目录名 ≠ claude 写的目录名」上(前两次是漏字符),
    /// 所以判据钉成:同一目录的任何合法写法必须算出同一个名字。
    #[test]
    fn project_dirname_ignores_trailing_slash() {
        assert_eq!(
            project_dirname("/Users/example/Desktop/My_App/"),
            project_dirname("/Users/example/Desktop/My_App")
        );
        assert_eq!(
            project_dirname("/Users/example/Desktop/My_App/"),
            "-Users-example-Desktop-My-App"
        );
        assert_eq!(project_dirname("/a/b//"), project_dirname("/a/b"));
        assert_eq!(project_dirname("/"), "-", "根目录仍是单个 -");
    }

    /// 规则必须是**补集**(非字母数字一律 `-`),不能退回枚举:枚举漏掉哪个字符,路径含该字符的
    /// agent 就定位不到转录、冷启白等到超时。这里钉住几个枚举法必漏、而现实中确实会出现的字符。
    #[test]
    fn project_dirname_maps_every_non_alphanumeric_char() {
        assert_eq!(project_dirname("/a/b c"), "-a-b-c"); // 空格
        assert_eq!(project_dirname("/a/b+c@d"), "-a-b-c-d"); // + @
        assert_eq!(project_dirname("/a/(b)/c"), "-a--b--c"); // 括号:每个字符各占一位
        assert_eq!(project_dirname("/a/项目"), "-a---"); // 非 ASCII:每个字符一个 -
        assert_eq!(project_dirname("/a/b1-C2"), "-a-b1-C2"); // 字母数字与 - 原样保留
    }
}
