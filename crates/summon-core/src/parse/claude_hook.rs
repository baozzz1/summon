//! Claude hook 的纯件。PermissionRequest:hook 输入解析、请求分类、AskUserQuestion 题面与答案
//! 映射、回给 Claude 的 stdout,以及从转录里认出「这一次调用」并判断它是否已在别处结束。
//! PostCompact:哪些 claude 版本认得这个事件、hook 输入里的触发方式。
//!
//! 进程、socket、文件读取都在 `summon` 层;这里只处理字符串与 JSON。

use std::collections::HashSet;

use serde_json::{json, Value};

use crate::core_port::{AskQuestion, ClaudeHookRequest};

/// hook stdin 里 summon 用得上的字段。
#[derive(Clone, Debug, PartialEq)]
pub struct HookInput {
    pub session_id: String,
    pub transcript_path: String,
    pub request: ClaudeHookRequest,
}

/// 解析 hook stdin。缺工具名或 `tool_input` 不是对象就认不出请求,返回 None —— 调用方据此
/// 不作答,让 Claude 走自己的对话框。
pub fn parse_hook_input(raw: &str) -> Option<HookInput> {
    let value: Value = serde_json::from_str(raw).ok()?;
    let text = |key: &str| {
        value
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    let tool_name = text("tool_name")?;
    let tool_input = value.get("tool_input").filter(|v| v.is_object())?.clone();
    Some(HookInput {
        session_id: text("session_id").unwrap_or_default(),
        transcript_path: text("transcript_path").unwrap_or_default(),
        request: ClaudeHookRequest {
            tool_name,
            tool_input,
            agent_id: text("agent_id"),
            agent_type: text("agent_type"),
        },
    })
}

/// 这次请求由谁接。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HookKind {
    /// 工具授权:卡上「允许 / 拒绝」。
    Permission,
    /// AskUserQuestion:提问卡,答案回 `updatedInput.answers`。
    Ask,
    /// 必须在终端里交互的工具:hook 不作答。
    Unsupported,
}

/// AskUserQuestion 之外、Claude 要求人亲自交互的工具。对它们回 allow 不带 updatedInput 会被
/// Claude 忽略,卡上却显示「已允许」,所以干脆不接。
const TERMINAL_ONLY_TOOLS: [&str; 2] = ["ExitPlanMode", "EnterPlanMode"];

pub fn classify(tool_name: &str) -> HookKind {
    if tool_name == "AskUserQuestion" {
        HookKind::Ask
    } else if TERMINAL_ONLY_TOOLS.contains(&tool_name) {
        HookKind::Unsupported
    } else {
        HookKind::Permission
    }
}

/// AskUserQuestion 的题面 → 提问卡的题。Claude 总给用户留「其他」自由输入,所以每题都开
/// `allow_other`;没有候选的题(扩展题型的 text / number)渲染成纯输入框。题面不成形 → None。
pub fn ask_questions(tool_input: &Value) -> Option<Vec<AskQuestion>> {
    let questions = tool_input.get("questions")?.as_array()?;
    if questions.is_empty() {
        return None;
    }
    questions
        .iter()
        .map(|q| {
            let question = q.get("question")?.as_str()?.trim().to_string();
            if question.is_empty() {
                return None;
            }
            let choice = q
                .get("kind")
                .and_then(Value::as_str)
                .is_none_or(|kind| kind == "choice");
            let raw_options = q
                .get("options")
                .and_then(Value::as_array)
                .filter(|_| choice)
                .cloned()
                .unwrap_or_default();
            let mut options = Vec::with_capacity(raw_options.len());
            let mut descriptions = Vec::with_capacity(raw_options.len());
            for option in &raw_options {
                options.push(option.get("label")?.as_str()?.to_string());
                descriptions.push(
                    option
                        .get("description")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                );
            }
            Some(AskQuestion {
                question,
                header: q.get("header").and_then(Value::as_str).map(str::to_string),
                multi_select: !options.is_empty()
                    && q.get("multiSelect").and_then(Value::as_bool) == Some(true),
                allow_other: !options.is_empty(),
                options,
                descriptions,
            })
        })
        .collect()
}

/// 把提问卡收回的答案(按题序,未答为 None)写回 AskUserQuestion 的入参。Claude 的约定是
/// `answers: {题干: 答案}`,键用入参里的题干原文;一题都没答返回 None,由调用方当作跳过。
pub fn ask_updated_input(tool_input: &Value, answers: &[Option<String>]) -> Option<Value> {
    let questions = tool_input.get("questions")?.as_array()?;
    let answered: serde_json::Map<String, Value> = questions
        .iter()
        .zip(answers)
        .filter_map(|(question, answer)| {
            let key = question.get("question")?.as_str()?;
            let answer = answer.as_deref()?.trim();
            (!answer.is_empty()).then(|| (key.to_string(), Value::String(answer.to_string())))
        })
        .collect();
    if answered.is_empty() {
        return None;
    }
    let mut updated = tool_input.clone();
    updated
        .as_object_mut()?
        .insert("answers".into(), Value::Object(answered));
    Some(updated)
}

/// hook 的裁决。
#[derive(Clone, Debug, PartialEq)]
pub enum HookDecision {
    Allow { updated_input: Option<Value> },
    Deny { message: String },
}

/// 人在飞书卡上点了「拒绝」时告诉模型的话:它得知道这是人的决定,不是检查器的。
pub const PERMISSION_DENIED_MESSAGE: &str = "r56e7030101";

/// 提问卡被整张跳过(或一题没答)时告诉模型的话。
pub const ASK_SKIPPED_MESSAGE: &str = "rd41182eddc";

/// 回给 Claude 的 stdout(一行 JSON)。
pub fn hook_stdout(decision: &HookDecision) -> String {
    let decision = match decision {
        HookDecision::Allow {
            updated_input: Some(input),
        } => json!({"behavior": "allow", "updatedInput": input}),
        HookDecision::Allow {
            updated_input: None,
        } => json!({"behavior": "allow"}),
        HookDecision::Deny { message } => json!({"behavior": "deny", "message": message}),
    };
    json!({
        "hookSpecificOutput": {
            "hookEventName": "PermissionRequest",
            "decision": decision,
        }
    })
    .to_string()
}

/// 这次调用在转录里的状态。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CallState {
    /// 还没在转录里认出它(hook 触发时 tool_use 可能尚未落盘)。
    Unseen,
    /// 认出来了,还没有结果。
    Pending(String),
    /// 它的 tool_result 已经写进转录:在终端里答过、被自动拒绝或本轮被打断。
    Finished(String),
}

/// 从转录行里认出 hook 正在等的这一次调用。
///
/// hook 输入没有 tool_use_id,只能按工具名 + 入参认。同样的命令可能早先跑过,所以只认**还没有
/// 结果**的那一条;一旦认定就不再换。转录是按时间追加的,结果总在调用之后出现。
#[derive(Debug)]
pub struct CallWatch {
    tool_name: String,
    tool_input: Value,
    /// 入参一致的 tool_use id,按出现顺序。
    matched: Vec<String>,
    results: HashSet<String>,
    target: Option<String>,
}

impl CallWatch {
    pub fn new(request: &ClaudeHookRequest) -> Self {
        Self {
            tool_name: request.tool_name.clone(),
            tool_input: request.tool_input.clone(),
            matched: Vec::new(),
            results: HashSet::new(),
            target: None,
        }
    }

    /// 喂一行转录(JSONL 的一行)。半行、坏行、无关行都直接跳过。
    pub fn feed_line(&mut self, line: &str) {
        if !line.contains("tool_use") && !line.contains("tool_result") {
            return;
        }
        let Ok(entry) = serde_json::from_str::<Value>(line) else {
            return;
        };
        let Some(blocks) = entry
            .get("message")
            .and_then(|m| m.get("content"))
            .and_then(Value::as_array)
        else {
            return;
        };
        for block in blocks {
            match block.get("type").and_then(Value::as_str) {
                Some("tool_use")
                    if block.get("name").and_then(Value::as_str) == Some(&self.tool_name)
                        && block
                            .get("input")
                            .is_some_and(|input| same_call_input(input, &self.tool_input)) =>
                {
                    if let Some(id) = block.get("id").and_then(Value::as_str) {
                        self.matched.push(id.to_string());
                    }
                }
                Some("tool_result") => {
                    if let Some(id) = block.get("tool_use_id").and_then(Value::as_str) {
                        self.results.insert(id.to_string());
                    }
                }
                _ => {}
            }
        }
    }

    /// 当前状态。**调用方应在喂完此刻读得到的全部行之后再问**:认定目标发生在这里而不是
    /// 逐行喂的时候 —— 一次读进来的尾部里,较早那条同样的调用和它的结果可能都在,逐行认定会
    /// 在看到结果之前就把它当成目标。
    pub fn state(&mut self) -> CallState {
        if self.target.is_none() {
            self.target = self
                .matched
                .iter()
                .rev()
                .find(|id| !self.results.contains(*id))
                .cloned();
        }
        match &self.target {
            None => CallState::Unseen,
            Some(id) if self.results.contains(id) => CallState::Finished(id.clone()),
            Some(id) => CallState::Pending(id.clone()),
        }
    }
}

/// 转录里的入参与 hook 给的入参是不是同一次调用。实测两边逐字段相同;一侧多出字段(某个版本
/// 只在一边补默认值)也认作同一次 —— 认不出的代价是卡片作废不及时,比误认大。
fn same_call_input(transcript: &Value, hook: &Value) -> bool {
    if transcript == hook {
        return true;
    }
    let (Some(a), Some(b)) = (transcript.as_object(), hook.as_object()) else {
        return false;
    };
    let contained = |small: &serde_json::Map<String, Value>,
                     big: &serde_json::Map<String, Value>| {
        !small.is_empty() && small.iter().all(|(k, v)| big.get(k) == Some(v))
    };
    contained(a, b) || contained(b, a)
}

/// 子 agent 的转录:`<主转录所在目录>/<session_id>/subagents/agent-<agent_id>.jsonl`。
/// 主 agent 就是 hook 给的 transcript_path。
pub fn call_transcript_path(input: &HookInput) -> Option<std::path::PathBuf> {
    let main = std::path::Path::new(&input.transcript_path);
    if input.transcript_path.is_empty() {
        return None;
    }
    match input.request.agent_id.as_deref() {
        None => Some(main.to_path_buf()),
        Some(agent_id) if !input.session_id.is_empty() => Some(
            main.parent()?
                .join(&input.session_id)
                .join("subagents")
                .join(format!("agent-{agent_id}.jsonl")),
        ),
        Some(_) => None,
    }
}

/// 认得 `PostCompact` hook 事件的最低 claude 版本(2.1.76 起)。更早的版本按枚举校验 settings 里的
/// hook 事件名,认不出的事件会让整份 `--settings` 校验失败、整份被丢弃 —— 执行闸与授权 hook 跟着
/// 一起没了。
pub const POST_COMPACT_MIN_VERSION: (u64, u64, u64) = (2, 1, 76);

/// `claude --version` 的输出(如 `2.1.304 (Claude Code)`)认不认得 `PostCompact`。认不出版本号时
/// 返回 None,调用方按不认得处理。
pub fn post_compact_supported(version_output: &str) -> Option<bool> {
    let token = version_output.split_whitespace().next()?;
    let mut parts = token.split('.').map(|part| part.parse::<u64>().ok());
    let version = (parts.next()??, parts.next()??, parts.next()??);
    if parts.next().is_some() {
        return None;
    }
    Some(version >= POST_COMPACT_MIN_VERSION)
}

/// PostCompact hook stdin 里的触发方式(`manual` / `auto`),只用于日志;缺了或认不出是 None。
pub fn compact_trigger(raw: &str) -> Option<String> {
    let value: Value = serde_json::from_str(raw).ok()?;
    value
        .get("trigger")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|trigger| !trigger.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hook_json(tool_name: &str, tool_input: Value, agent: Option<&str>) -> String {
        let mut v = json!({
            "session_id": "sid-1",
            "transcript_path": "/p/proj/sid-1.jsonl",
            "permission_mode": "bypassPermissions",
            "hook_event_name": "PermissionRequest",
            "tool_name": tool_name,
            "tool_input": tool_input,
            "permission_suggestions": [],
        });
        if let Some(agent) = agent {
            v["agent_id"] = json!(agent);
            v["agent_type"] = json!("general-purpose");
        }
        v.to_string()
    }

    #[test]
    fn post_compact_is_written_only_for_claude_versions_that_know_it() {
        assert_eq!(
            post_compact_supported("2.1.304 (Claude Code)\n"),
            Some(true)
        );
        assert_eq!(post_compact_supported("2.1.76 (Claude Code)"), Some(true));
        assert_eq!(post_compact_supported("2.1.75 (Claude Code)"), Some(false));
        assert_eq!(post_compact_supported("2.0.99"), Some(false));
        assert_eq!(post_compact_supported("3.0.0"), Some(true));
        for unknown in ["", "Claude Code", "2.1", "2.1.x (Claude Code)", "2.1.76.1"] {
            assert_eq!(post_compact_supported(unknown), None, "{unknown}");
        }
    }

    #[test]
    fn compact_trigger_is_optional() {
        assert_eq!(
            compact_trigger(r#"{"hook_event_name":"PostCompact","trigger":"auto"}"#),
            Some("auto".into())
        );
        assert_eq!(
            compact_trigger(r#"{"hook_event_name":"PostCompact"}"#),
            None
        );
        assert_eq!(compact_trigger("not json"), None);
    }

    #[test]
    fn parses_main_and_subagent_requests() {
        let main = parse_hook_input(&hook_json("Bash", json!({"command": "ls"}), None)).unwrap();
        assert_eq!(main.request.tool_name, "Bash");
        assert_eq!(main.request.agent_id, None);
        assert_eq!(main.session_id, "sid-1");
        let sub =
            parse_hook_input(&hook_json("Bash", json!({"command": "ls"}), Some("a1"))).unwrap();
        assert_eq!(sub.request.agent_id.as_deref(), Some("a1"));
        assert_eq!(sub.request.agent_type.as_deref(), Some("general-purpose"));
    }

    #[test]
    fn unparseable_input_is_not_a_request() {
        assert_eq!(parse_hook_input("not json"), None);
        assert_eq!(parse_hook_input(r#"{"tool_name":"Bash"}"#), None);
        assert_eq!(
            parse_hook_input(r#"{"tool_name":"Bash","tool_input":"ls"}"#),
            None
        );
        assert_eq!(
            parse_hook_input(r#"{"tool_name":"  ","tool_input":{}}"#),
            None
        );
    }

    #[test]
    fn classifies_ask_terminal_only_and_permission() {
        assert_eq!(classify("AskUserQuestion"), HookKind::Ask);
        assert_eq!(classify("ExitPlanMode"), HookKind::Unsupported);
        assert_eq!(classify("Bash"), HookKind::Permission);
        assert_eq!(classify("mcp__x__y"), HookKind::Permission);
    }

    #[test]
    fn ask_questions_keep_labels_descriptions_and_multi_select() {
        let input = json!({"questions": [
            {"question": "Which color?", "header": "Color", "multiSelect": false,
             "options": [{"label": "Red", "description": "warm"}, {"label": "Blue"}]},
            {"question": "Which toppings?", "multiSelect": true,
             "options": [{"label": "A"}, {"label": "B"}]},
            {"question": "Anything else?", "kind": "text"},
        ]});
        let qs = ask_questions(&input).unwrap();
        assert_eq!(qs.len(), 3);
        assert_eq!(qs[0].options, vec!["Red", "Blue"]);
        assert_eq!(qs[0].descriptions, vec!["warm", ""]);
        assert_eq!(qs[0].header.as_deref(), Some("Color"));
        assert!(qs[0].allow_other && !qs[0].multi_select);
        assert!(qs[1].multi_select);
        assert!(qs[2].options.is_empty() && !qs[2].allow_other && !qs[2].multi_select);
    }

    #[test]
    fn malformed_ask_input_is_rejected() {
        assert!(ask_questions(&json!({})).is_none());
        assert!(ask_questions(&json!({"questions": []})).is_none());
        assert!(ask_questions(&json!({"questions": [{"question": " "}]})).is_none());
        assert!(
            ask_questions(&json!({"questions": [{"question": "q", "options": [{"x": 1}]}]}))
                .is_none()
        );
    }

    #[test]
    fn answers_map_question_text_to_answer_and_skip_unanswered() {
        let input = json!({"questions": [
            {"question": "Q1", "options": [{"label": "a"}, {"label": "b"}]},
            {"question": "Q2", "options": [{"label": "c"}, {"label": "d"}]},
        ]});
        let updated = ask_updated_input(&input, &[None, Some(" d ".into())]).expect("答了一题");
        assert_eq!(updated["answers"], json!({"Q2": "d"}));
        assert_eq!(updated["questions"], input["questions"], "原题面原样保留");
        assert!(ask_updated_input(&input, &[None, Some("  ".into())]).is_none());
        assert!(ask_updated_input(&input, &[]).is_none());
        let padded = json!({"questions": [{"question": " Q3 ", "options": [{"label": "a"}]}]});
        assert_eq!(
            ask_updated_input(&padded, &[Some("a".into())]).unwrap()["answers"],
            json!({" Q3 ": "a"}),
            "键是入参里的题干原文"
        );
    }

    #[test]
    fn stdout_matches_claude_permission_request_contract() {
        let allow: Value = serde_json::from_str(&hook_stdout(&HookDecision::Allow {
            updated_input: None,
        }))
        .unwrap();
        assert_eq!(
            allow,
            json!({"hookSpecificOutput": {"hookEventName": "PermissionRequest",
                "decision": {"behavior": "allow"}}})
        );
        let deny: Value = serde_json::from_str(&hook_stdout(&HookDecision::Deny {
            message: "no".into(),
        }))
        .unwrap();
        assert_eq!(
            deny["hookSpecificOutput"]["decision"],
            json!({"behavior": "deny", "message": "no"})
        );
        let ask: Value = serde_json::from_str(&hook_stdout(&HookDecision::Allow {
            updated_input: Some(json!({"answers": {"Q": "A"}})),
        }))
        .unwrap();
        assert_eq!(
            ask["hookSpecificOutput"]["decision"]["updatedInput"]["answers"]["Q"],
            "A"
        );
    }

    fn use_line(id: &str, name: &str, input: Value) -> String {
        json!({"type": "assistant", "message": {"content": [
            {"type": "tool_use", "id": id, "name": name, "input": input}
        ]}})
        .to_string()
    }

    fn result_line(id: &str) -> String {
        json!({"type": "user", "message": {"content": [
            {"type": "tool_result", "tool_use_id": id, "content": "x"}
        ]}})
        .to_string()
    }

    fn bash(command: &str) -> ClaudeHookRequest {
        ClaudeHookRequest {
            tool_name: "Bash".into(),
            tool_input: json!({"command": command}),
            agent_id: None,
            agent_type: None,
        }
    }

    #[test]
    fn call_watch_waits_for_the_call_then_its_result() {
        let mut watch = CallWatch::new(&bash("rm x"));
        assert_eq!(watch.state(), CallState::Unseen);
        watch.feed_line(&use_line("t1", "Bash", json!({"command": "rm x"})));
        assert_eq!(watch.state(), CallState::Pending("t1".into()));
        watch.feed_line(&result_line("t1"));
        assert_eq!(watch.state(), CallState::Finished("t1".into()));
    }

    #[test]
    fn earlier_identical_call_with_a_result_is_not_the_target() {
        let mut watch = CallWatch::new(&bash("rm x"));
        watch.feed_line(&use_line("old", "Bash", json!({"command": "rm x"})));
        watch.feed_line(&result_line("old"));
        assert_eq!(watch.state(), CallState::Unseen);
        watch.feed_line(&use_line("new", "Bash", json!({"command": "rm x"})));
        assert_eq!(watch.state(), CallState::Pending("new".into()));
    }

    #[test]
    fn other_tools_inputs_and_garbage_are_ignored() {
        let mut watch = CallWatch::new(&bash("rm x"));
        watch.feed_line(&use_line("a", "Bash", json!({"command": "rm y"})));
        watch.feed_line(&use_line("b", "Write", json!({"command": "rm x"})));
        watch.feed_line(r#"{"type":"assistant","message":{"content":[{"type":"tool_use""#);
        watch.feed_line("plain text");
        watch.feed_line(&result_line("a"));
        assert_eq!(watch.state(), CallState::Unseen);
    }

    #[test]
    fn extra_fields_on_either_side_still_match_but_different_values_do_not() {
        let mut watch = CallWatch::new(&bash("rm x"));
        watch.feed_line(&use_line(
            "t1",
            "Bash",
            json!({"command": "rm x", "timeout": 30000}),
        ));
        assert_eq!(watch.state(), CallState::Pending("t1".into()));
        let mut watch = CallWatch::new(&ClaudeHookRequest {
            tool_input: json!({"command": "rm x", "description": "d"}),
            ..bash("rm x")
        });
        watch.feed_line(&use_line("t2", "Bash", json!({"command": "rm x"})));
        assert_eq!(watch.state(), CallState::Pending("t2".into()));
        let mut watch = CallWatch::new(&bash("rm x"));
        watch.feed_line(&use_line("t3", "Bash", json!({"command": "rm y", "x": 1})));
        watch.feed_line(&use_line("t4", "Bash", json!({})));
        assert_eq!(watch.state(), CallState::Unseen);
    }

    #[test]
    fn target_is_fixed_once_seen() {
        let mut watch = CallWatch::new(&bash("rm x"));
        watch.feed_line(&use_line("first", "Bash", json!({"command": "rm x"})));
        watch.feed_line(&use_line("second", "Bash", json!({"command": "rm x"})));
        watch.feed_line(&result_line("second"));
        assert_eq!(watch.state(), CallState::Pending("first".into()));
    }

    #[test]
    fn transcript_path_points_at_subagent_file_for_subagents() {
        let main = parse_hook_input(&hook_json("Bash", json!({}), None)).unwrap();
        assert_eq!(
            call_transcript_path(&main).unwrap(),
            std::path::PathBuf::from("/p/proj/sid-1.jsonl")
        );
        let sub = parse_hook_input(&hook_json("Bash", json!({}), Some("a9"))).unwrap();
        assert_eq!(
            call_transcript_path(&sub).unwrap(),
            std::path::PathBuf::from("/p/proj/sid-1/subagents/agent-a9.jsonl")
        );
    }
}
