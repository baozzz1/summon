use crate::driver::{LaunchModel, ModelEffortError};
use crate::parse::scrubber::strip_ansi;
use std::time::Duration;

/// Cursor ACP 的会话模式 id。ACP 由 server 动态公布 mode catalog，故这里保留 id 而不是把
/// `agent / plan / ask` 烤成枚举；未来 server 增加模式时，summon 无需再发版扩枚举。
///
/// 持久化与卡片回调都只接受单个、短且无控制字符的 token。协议层仍会拿当前会话的动态
/// `availableModes` 复核；这里的校验只负责挡住空值、注入式内容和无界文件。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CursorSessionMode(String);

impl CursorSessionMode {
    pub const fn as_str(&self) -> &str {
        self.0.as_str()
    }

    /// 没有 live ACP catalog 时只允许 Cursor 的标准三态；等会话建立后可按动态 catalog 接受
    /// 其它 id。这个兜底避免离线时把拼错的任意值永久落盘、随后每轮都失败重试。
    pub fn is_standard(&self) -> bool {
        matches!(self.as_str(), "agent" | "plan" | "ask")
    }
}

impl std::fmt::Display for CursorSessionMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{}", summon_i18n::format!("r91344cce89", value = .value))]
pub struct ParseCursorSessionModeError {
    value: String,
}

impl std::str::FromStr for CursorSessionMode {
    type Err = ParseCursorSessionModeError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let valid = !value.is_empty()
            && value.chars().count() <= 64
            && value
                .chars()
                .all(|character| character.is_ascii_alphanumeric() || "-_.:/".contains(character));
        if valid {
            Ok(Self(value.to_string()))
        } else {
            Err(ParseCursorSessionModeError {
                value: value.to_string(),
            })
        }
    }
}

/// cursor:model→`--model <id>`;**无独立 effort 轴**——tier 在 bind 时已烤进 model 名,
/// 故传 effort 直接 `EffortUnsupported`(运行期只验 model 串,不做后缀拼接,见 LOCKED 决策 1)。
/// 无 model 无 effort → 空片段(用 CLI 默认)。
pub fn resolve_launch(
    model: Option<&str>,
    effort: Option<&str>,
) -> Result<LaunchModel, ModelEffortError> {
    if effort.is_some() {
        return Err(ModelEffortError::EffortUnsupported { cli: "cursor" });
    }
    let mut args = Vec::new();
    if let Some(m) = model {
        args.push("--model".to_string());
        args.push(m.to_string());
    }
    Ok(LaunchModel { args })
}

/// 无 tier 轴的 cursor model family 前缀(composer/grok/gemini/auto/kimi 没有 reasoning 档位,
/// 也没有 thinking/fast 修饰)。命中则 compose 忽略 tier/thinking/fast,返回 base 原样。
const NO_TIER_PREFIXES: &[&str] = &["composer", "grok", "gemini", "auto", "kimi"];

/// 把逻辑 tier 映射成某 base family 的 cursor 后缀词。只有 GPT-5.5 的 `xhigh` 使用历史命名
/// `extra-high`；GPT-5.6 catalog 已恢复原生 `xhigh`，其余 family/档位也原样。
fn tier_suffix(base: &str, tier: &str) -> String {
    if base.starts_with("gpt-5.5") && tier == "xhigh" {
        return "extra-high".to_string();
    }
    tier.to_string()
}

/// 见 `CliDriver::cursor_compose_model` 的文档:bind 时把 base+tier(+thinking/+fast)扁平成
/// cursor model id。这是纯数据拼接,穷举单测覆盖每条 family 规则。
pub fn compose_model(base: &str, tier: Option<&str>, thinking: bool, fast: bool) -> String {
    // 无 tier 轴的 family:忽略所有修饰,base 原样(它们没有 thinking/fast/tier 后缀)。
    if NO_TIER_PREFIXES.iter().any(|p| base.starts_with(p)) {
        return base.to_string();
    }
    let mut out = base.to_string();
    // 顺序:base[-thinking][-<tier>][-fast](thinking 在 tier 前、fast 在最后)。
    if thinking {
        out.push_str("-thinking");
    }
    if let Some(t) = tier {
        out.push('-');
        out.push_str(&tier_suffix(base, t));
    }
    if fast {
        out.push_str("-fast");
    }
    out
}

pub fn default_cmd() -> &'static str {
    "agent"
}

pub fn captures() -> bool {
    true
}

pub fn resume_cmd(base: &str, id: &str) -> String {
    format!("{base} --resume={id}")
}

pub fn fresh_cmd_with_id(base: &str, id: &str) -> String {
    format!("{base} --resume={id}")
}

pub fn model_flag() -> Option<&'static str> {
    Some("--model")
}

/// 交互式压缩上下文的 slash 命令。前瞻:cursor 当前跑 headless 子进程(无交互 REPL),
/// 这条仅在 cursor 将来改走 tmux 时才会被投递(见 `compact_spec`)。
pub fn compact_command() -> &'static str {
    "/compress"
}

pub fn clear_input_before_send() -> bool {
    true
}

pub fn interrupts_followup() -> bool {
    false
}

pub fn startup_grace() -> Duration {
    Duration::from_millis(3000)
}

/// 无头一轮的 flag(不含 `-p <prompt>`,由 runner 在前面补)。
/// cursor 没有 system-prompt-file flag,sys_prompt_file 参数被忽略(PRIMER 由调用方前置到 prompt)。
/// resume=Some → 以单个 `--resume=<id>` 续接(cursor 用 `=` 语法,不拆两个 arg)。
/// `--stream-partial-output`:以单个文本 delta 流式输出(仅与 --print + stream-json 一起生效),
/// 让打字机卡片能按 token 逐块刷新,而非等整段生成完才推送。缓存中立:不影响 prompt 前缀。
pub fn headless_args(resume: Option<&str>, _sys_prompt_file: Option<&str>) -> Vec<String> {
    let mut a = vec![
        "--output-format".to_string(),
        "stream-json".to_string(),
        "--yolo".to_string(),
        "--stream-partial-output".to_string(),
    ];
    if let Some(sid) = resume {
        // cursor resume 用 `=` 连体写法:一个 arg,不能拆成两个
        a.push(format!("--resume={sid}"));
    }
    a
}

fn is_braille(c: char) -> bool {
    ('\u{2800}'..='\u{28FF}').contains(&c)
}

fn is_cursor_working_line(line: &str) -> bool {
    let t = line.trim();
    t.chars().next().is_some_and(is_braille) && t.chars().any(|c| c.is_ascii_alphabetic())
}

pub fn is_cursor_ready(pane: &str) -> bool {
    !strip_ansi(pane).lines().any(is_cursor_working_line)
}

fn cursor_last_prompt_line(pane: &str) -> Option<String> {
    strip_ansi(pane)
        .lines()
        .rfind(|l| l.trim_start().starts_with('→'))
        .map(|l| l.trim().to_string())
}

fn cursor_prompt_line_has_pending_text(line: &str) -> bool {
    if line.contains("Add a follow-up") {
        return false;
    }
    let lower = line.to_ascii_lowercase();
    lower.contains("ctrl+c to stop") || (lower.contains("enter") && lower.contains("send"))
}

/// Cursor 输入框是否已清空(= 上一次注入已提交)。Cursor 完成后仍可能在底部回显上一条
/// `→ prompt`,所以只把带输入提示(chrome)的非占位行视为“仍在编辑框里”。
pub fn cursor_input_is_clear(pane: &str) -> bool {
    cursor_last_prompt_line(pane)
        .as_deref()
        .is_none_or(|line| !cursor_prompt_line_has_pending_text(line))
}

fn is_cursor_rule_line(line: &str) -> bool {
    let t = line.trim();
    !t.is_empty() && t.chars().all(|c| matches!(c, '▄' | '▀' | '█' | '─' | ' '))
}

fn is_cursor_chrome_line(line: &str) -> bool {
    let t = line.trim();
    t.is_empty()
        || t == "Cursor Agent"
        || t.starts_with("v20")
        || t.starts_with("Tip:")
        || t.starts_with("Use /skills")
        || t.starts_with('→')
        || t.starts_with("~/")
        || t.contains("Auto-run")
        || is_cursor_rule_line(t)
        || is_cursor_working_line(t)
}

pub fn scrub_cursor_chrome(raw: &str) -> String {
    let plain = strip_ansi(raw);
    let mut out: Vec<String> = Vec::new();
    let mut prev_blank = false;
    for line in plain.lines() {
        if is_cursor_chrome_line(line) {
            if !prev_blank {
                out.push(String::new());
                prev_blank = true;
            }
            continue;
        }
        out.push(line.trim_end().to_string());
        prev_blank = false;
    }
    out.join("\n").trim().to_string()
}

#[cfg(test)]
mod mode_tests {
    use super::CursorSessionMode;

    #[test]
    fn cursor_session_mode_keeps_dynamic_ids_but_rejects_unsafe_values() {
        let dynamic: CursorSessionMode = "review-v2".parse().unwrap();
        assert_eq!(dynamic.as_str(), "review-v2");
        assert!(!dynamic.is_standard());
        assert!("agent".parse::<CursorSessionMode>().unwrap().is_standard());

        for invalid in ["", "two words", "line\nbreak", "bad`id", "<at>"] {
            assert!(invalid.parse::<CursorSessionMode>().is_err(), "{invalid:?}");
        }
        assert!("x".repeat(65).parse::<CursorSessionMode>().is_err());
    }
}
