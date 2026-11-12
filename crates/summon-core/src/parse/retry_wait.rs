//! 识别 claude 在 tmux pane 底部状态行里报出的**「等 API 响应,稍后重试」**态——best-effort、无状态。
//!
//! 形如 `✱ Waiting for API response · will retry in 1m 18s · check your network`。
//! 这一态下转录一个字节都不产出,而倒计时又让整屏签名每秒都在变,所以既走不到菜单识别、也走不到
//! stall 兜底(画面在动 = 有存活证据),飞书侧完全静默。这里补上这个盲区。
//!
//! 设计前提同 [`super::interactive`]:被监察的 TUI 不稳定、文案会随 CLI 迭代变。**认不出就返回
//! `None`**,调用方原样退回改动前的行为,绝不能因为解析器而多出一条崩溃路径。

/// 识别出的「等 API 重试」态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetryWait {
    /// 距下次重试的剩余秒数。**认出了状态行但读不出时长时为 `None`** —— 文案里的时长格式比
    /// 「这是重试态」本身更易变,不该因为读不出数字就把整个状态判成没发生。
    pub remaining_secs: Option<u64>,
    /// 状态行原文(已剥 spinner 前缀与首尾空白),渲卡直接用。
    pub line: String,
}

/// 状态行只可能出现在 pane 底部(输入框上方)。限定尾部行数是**假阳的主要防线**:正文里
/// 讨论重试逻辑的句子几乎不会落在这个窗口里。
const TAIL_LINES: usize = 15;
/// claude 状态行的字段分隔符(U+00B7 前后带空格)。正常 prose 不用它,与「尾部行」一起构成
/// 双重判据 —— 单靠 `retry` 会把「我给它加了重试逻辑」这类正文误判成状态行。
const FIELD_SEP: &str = " · ";
/// 触发词。大小写不敏感、**只取词根**:实测同一个 claude 至少有两种措辞 ——
/// `… · will retry in 1m 18s · check your network`(等响应超时)与
/// `Unable to connect to API (ConnectionRefused) · Retrying in 0s · attempt 6/10`(连不上)。
/// 用 `retry in` 会漏掉后者(`retrying in` 中间多了 `ing`),这是 2026-08-07 真机探针抓到的。
/// 前缀部分(`Waiting for API response` / `Unable to connect…`)不参与判据:不同错误类型给不同前缀。
const RETRY_MARKER: &str = "retry";

/// **流中断**错误的判据词根。claude 把它作为 assistant 的**普通 text 块**写进转录,
/// `stop_reason=stop_sequence`、`message.error` 为 **None** —— 所以既躲过了只看 `error` 字段的
/// `TailStep::ApiError`,又因为 `stop_sequence` 不算 `end_turn` 而不收轮。结果是这一轮一直挂到
/// stall 兜底,错误文本还混进回复正文。这里补上识别。
///
/// 实测原文:`API Error: Response stalled mid-stream. The response above may be incomplete.`
/// 只取最稳的词根组合(`api error` + `stalled`),文案微调不至于漏判。
const STALL_MARKERS: [&str; 2] = ["api error", "stalled"];

/// 这段 assistant 文本是不是「流中断,回复可能不完整」。
///
/// 命中意味着两件事:① 本轮到此为止(该收轮,别等 stall 兜底);② 它是**可续的** —— 往会话里发
/// 一句「继续」就能接着跑,所以该给用户一个按钮而不是只贴个 ERROR。
///
/// 判据刻意保守:两个词根都要命中,且限制在**短文本**内 —— agent 正文里讨论这个错误(比如本仓的
/// 设计文档)动辄成百上千字,不该被误判成真的发生了流中断。
pub fn is_stream_stall(text: &str) -> bool {
    let t = text.trim();
    // 真错误就是一句话;长文本几乎必然是正文在讨论它。
    if t.chars().count() > 200 {
        return false;
    }
    let lower = t.to_lowercase();
    STALL_MARKERS.iter().all(|m| lower.contains(m))
}

/// 尽力把 pane 解析成「等 API 重试」态。判据(三条全满足):/// 尽力把 pane 解析成「等 API 重试」态。判据(三条全满足):
/// ① 位于 pane 尾部 [`TAIL_LINES`] 行内;② 行内含 `retry`(词根);③ 行内含 [`FIELD_SEP`]。
/// 认不出 → `None`。
pub fn parse_retry_wait(pane: &str) -> Option<RetryWait> {
    let lines: Vec<&str> = pane.lines().collect();
    let start = lines.len().saturating_sub(TAIL_LINES);
    // 从后往前找:同屏若真出现多处(极罕见),最下面那条才是当前状态行。
    let hit = lines[start..].iter().rev().find(|l| {
        let lower = l.to_lowercase();
        lower.contains(RETRY_MARKER) && l.contains(FIELD_SEP)
    })?;
    let line = strip_spinner(hit);
    let remaining_secs = parse_duration_after(&line, RETRY_MARKER);
    Some(RetryWait {
        remaining_secs,
        line,
    })
}

/// 剥掉行首的 spinner/装饰字符。spinner 逐帧换字符(`✱ ✳ ✽ *` …),留着会让卡片文字每拍抖动,
/// 也会污染「这行变没变」的比较。保守只剥非字母数字的前导符号,不动正文。
fn strip_spinner(line: &str) -> String {
    line.trim()
        .trim_start_matches(|c: char| !c.is_alphanumeric())
        .trim()
        .to_string()
}

/// 从 `marker` 之后抽出形如 `1h 2m 18s` / `45s` / `2m` 的时长,折算成秒。
/// 认不出任何 `<数字><单位>` 片段 → `None`(见 [`RetryWait::remaining_secs`] 的取舍)。
fn parse_duration_after(line: &str, marker: &str) -> Option<u64> {
    let lower = line.to_lowercase();
    let pos = lower.find(marker)? + marker.len();
    // 只在触发词之后、下一个字段分隔符之前找,避免把后续字段里的数字(如 `retry 3/5`)算进来。
    let rest = &lower[pos..];
    let rest = rest.split(FIELD_SEP).next().unwrap_or(rest);

    let mut total = 0u64;
    let mut num: Option<u64> = None;
    let mut found = false;
    for c in rest.chars() {
        if let Some(d) = c.to_digit(10) {
            num = Some(num.unwrap_or(0).saturating_mul(10).saturating_add(d as u64));
            continue;
        }
        let Some(n) = num else {
            continue;
        };
        // 单位字母才消费掉累积的数字;其它字符(空格/标点)保留数字等下一个字符。
        let unit = match c {
            'h' => Some(3600),
            'm' => Some(60),
            's' => Some(1),
            _ => None,
        };
        if let Some(mult) = unit {
            total = total.saturating_add(n.saturating_mul(mult));
            num = None;
            found = true;
        } else if !c.is_whitespace() {
            // 遇到既非数字也非单位的实体字符 → 时长片段结束,别再往后吞。
            break;
        }
    }
    found.then_some(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// owner 截图的原样状态行:应命中,并读出 1m18s = 78s。
    #[test]
    fn parses_owner_screenshot_line() {
        let pane = "\
⏺ 我先去代码里核实。

✱ Waiting for API response · will retry in 1m 18s · check your network

❯ ";
        let st = parse_retry_wait(pane).expect("应识别为重试等待");
        assert_eq!(st.remaining_secs, Some(78));
        assert!(
            st.line.starts_with("Waiting for API response"),
            "{}",
            st.line
        );
    }

    /// **真机 fixture**(2026-08-07 探针:把 `ANTHROPIC_BASE_URL` 指到不可达地址抓到的原样 pane)。
    /// 措辞与 owner 截图那条**不同** —— `Retrying in` 而非 `will retry in`,还多一个 `attempt` 字段。
    /// 判据当初按截图写成 `retry in`,漏掉这条;这个测试就是那次漏检的钉子。
    #[test]
    fn parses_connection_refused_form_from_a_live_probe() {
        let pane = "\
> hi

✻ Unable to connect to API (ConnectionRefused) · Retrying in 0s · attempt 6/10

────────────────────────────────────────
❯
────────────────────────────────────────
    Model: Opus 5    Thinking: xhigh";
        let st = parse_retry_wait(pane).expect("真机形态必须认出来");
        assert_eq!(st.remaining_secs, Some(0), "0s 也是合法倒计时");
        assert!(st.line.starts_with("Unable to connect"), "{}", st.line);
        // `attempt 6/10` 在下一个字段里,不能被当成时长吞进来。
    }

    /// 退避到几十秒后的同族形态。
    #[test]
    fn parses_retrying_in_with_a_real_backoff() {
        let pane = "✻ Unable to connect to API (ConnectionRefused) · Retrying in 8s · attempt 9/10";
        assert_eq!(parse_retry_wait(pane).unwrap().remaining_secs, Some(8));
    }

    /// 只有秒的形态。
    #[test]
    fn parses_seconds_only() {
        let pane = "✱ Waiting for API response · will retry in 45s · check your network";
        assert_eq!(parse_retry_wait(pane).unwrap().remaining_secs, Some(45));
    }

    /// 只有分钟的形态。
    #[test]
    fn parses_minutes_only() {
        let pane = "✱ Waiting for API response · will retry in 2m · check your network";
        assert_eq!(parse_retry_wait(pane).unwrap().remaining_secs, Some(120));
    }

    /// 正文里讨论重试的句子**不带字段分隔符**,绝不能误判(本仓自己就在写这种句子)。
    #[test]
    fn prose_about_retrying_is_not_a_status_line() {
        let pane = "\
我给它加了退避:失败后 will retry in 5s,最多三次。
这段是正文,不是状态行。";
        assert!(parse_retry_wait(pane).is_none());
    }

    /// 带分隔符但**在屏幕上方**(已被后续输出推走)→ 不算当前状态。
    #[test]
    fn old_status_line_scrolled_out_of_tail_is_ignored() {
        let mut pane = String::from("✱ Waiting for API response · will retry in 1m 18s · check\n");
        for i in 0..30 {
            pane.push_str(&format!("输出第 {i} 行\n"));
        }
        assert!(parse_retry_wait(&pane).is_none());
    }

    /// 认出了状态行、但时长格式变了 → 仍算命中,只是没有倒计时。
    #[test]
    fn unknown_duration_format_still_counts_as_retry_wait() {
        let pane = "✱ Waiting for API response · will retry in a moment · check your network";
        let st = parse_retry_wait(pane).expect("状态行本身应命中");
        assert_eq!(st.remaining_secs, None);
    }

    /// 同屏多条时取最下面那条(当前状态)。
    #[test]
    fn takes_the_lowest_status_line() {
        let pane = "\
✱ Waiting for API response · will retry in 2m · check your network
中间输出
✱ Waiting for API response · will retry in 30s · check your network";
        assert_eq!(parse_retry_wait(pane).unwrap().remaining_secs, Some(30));
    }

    /// 空 pane / 无关内容不崩、不误判。
    #[test]
    fn empty_or_unrelated_pane_is_none() {
        assert!(parse_retry_wait("").is_none());
        assert!(parse_retry_wait("❯ 1. Yes\n  2. No").is_none());
    }
}

#[cfg(test)]
mod stall_tests {
    use super::*;

    /// 实测原文必须命中 —— 它是 assistant 的普通 text 块,error 字段为 None,
    /// 现有 ApiError 判据(只看 error 字段)认不出它。
    #[test]
    fn real_stall_text_is_recognized() {
        assert!(is_stream_stall(
            "API Error: Response stalled mid-stream. The response above may be incomplete."
        ));
    }

    /// 正文里讨论这个错误不能被误判 —— 本仓的设计文档就会成段引用它。
    #[test]
    fn prose_discussing_the_error_is_not_a_stall() {
        let prose = "我们需要处理 API Error: Response stalled mid-stream 这种情况。\
            它落在 assistant 的 text 块里,stop_reason 是 stop_sequence,而 message.error 为 None,\
            所以既躲过了只看 error 字段的 ApiError 判据,又因为 stop_sequence 不算 end_turn 而不收轮,\
            结果这一轮会一直挂到 stall 兜底,错误文本还会混进回复正文里去,体验非常差。";
        assert!(!is_stream_stall(prose), "长正文不该被判成真发生了流中断");
    }

    /// 少一个词根就不算(保守):只说 stalled 可能是别的语境。
    #[test]
    fn partial_markers_do_not_match() {
        assert!(!is_stream_stall("the process stalled"));
        assert!(!is_stream_stall("API Error: overloaded"));
        assert!(!is_stream_stall(""));
    }
}
