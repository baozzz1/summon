//! 一轮的**上下文预算**:当前占用、窗口上限,以及「该不该提醒用户上下文过重」的全部判据。
//!
//! 设计的形状来自 owner 2026-08-18 的一句话:**提醒与否只取决于当轮模型的窗口大小,与 backend
//! 无关;backend 的唯一差别是「怎么拿到这个数」。** 所以判据集中在本模块(纯逻辑、可单测),
//! 取值分三路散在各自的 capture 实现里,互不知道对方存在。
//!
//! 后端只负责填自己能证明的字段；缺失维度统一由 `Option` 降级，不按 CLI 猜默认窗口。

/// 提醒档位(owner 2026-08-18 定):跨过 60% 一次、跨过 80% 一次,每档只提醒一次。
///
/// **必须升序**且非空 —— [`ContextBudget::warn_action`] 依赖这个顺序取「已跨过的最高档」。
pub const WARN_STEPS: [u8; 2] = [60, 80];

/// 窗口小于此值的模型不提醒。
///
/// **why**:小窗口模型自己会压缩(codex 的 `model_auto_compact_token_limit` 就是干这个的),
/// 由 summon 再唠叨一遍是纯噪声。owner 2026-08-18 明确:判据是窗口大小本身,不是 backend ——
/// 同一个 codex 配 1M 窗口就该提醒,配 256k 就不该。
pub const WARN_MIN_WINDOW: u64 = 500_000;

/// 一轮结束时对「要不要提醒」的裁决。三态,调用方 match 一次即可,不必自己比大小。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WarnAction {
    /// 什么都不做。
    None,
    /// 提醒这一档(值取自 [`WARN_STEPS`])。调用方发完提醒后须把它记进已提醒档位。
    Warn(u8),
    /// 占用已回落到最低档以下 → 清掉已提醒记录。
    ///
    /// **why 由本函数裁决而不是让调用方在每条重置路径上记得清**:`/compact`、`/clear`、会话
    /// 重建、上游自己压缩……能让占用回落的路径不止一条,靠枚举它们必然漏。以「占用回落」这个
    /// 事实本身作判据,漏不掉。
    ResetWarned,
}

/// 一轮的上下文预算快照。
///
/// 三个字段都可缺 —— 缺项一律退化成「不显示 / 不提醒」,与 `TurnBadge` 既有的「每项取不到就省略」
/// 惯例一致。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ContextBudget {
    /// 当前上下文占用(token 绝对值)。
    ///
    /// codex 取 `thread/tokenUsage/updated` 的 **`last.input_tokens`** —— 那一轮送进模型的输入量
    /// 就是此刻的上下文长度。**不能拿 `total_*`**,那是整个 thread 的累计,会随会话越滚越大
    /// (`parse::codex_rpc` 里已有同样的警告)。Cursor 取 ACP `usage_update.used`，窗口取同帧
    /// `usage_update.size`。
    /// claude 的 statusLine 只给百分比,有窗口大小时可反算,仅用于显示。
    pub used_tokens: Option<u64>,
    /// 该模型的窗口上限(token)。**取不到就是 None,不猜、不填默认值。**
    ///
    /// **why 宁可 None**:填一个猜的窗口会让百分比变成「看起来精确、实际错误」的数字,而用户
    /// 没有任何办法发现它是猜的。缺项省略至少是诚实的。
    pub window_tokens: Option<u64>,
    /// 上游**直接给出**的已用百分比(claude statusLine 的 `context_window.used_percentage`)。
    ///
    /// **优先于 used/window 的计算值**:它是 CLI 自己算的,口径与它自己的压缩策略一致;我们
    /// 反算出来的那个可能因为窗口口径不同(是否含系统提示、是否含预留)而与用户在 CLI 里看到的
    /// 数字对不上。以上游为准,避免两处显示打架。
    pub reported_pct: Option<u8>,
}

impl ContextBudget {
    /// claude 路径:statusLine 直接给百分比,窗口大小可能同时有。
    pub fn from_reported_pct(pct: u8, window_tokens: Option<u64>) -> Self {
        let pct = pct.min(100);
        Self {
            // 有窗口就反算一个绝对值备用;没有就只留百分比。
            used_tokens: window_tokens.map(|w| w.saturating_mul(u64::from(pct)) / 100),
            window_tokens,
            reported_pct: Some(pct),
        }
    }

    /// token usage 路径：绝对值必填，窗口按上游能力可选。
    pub fn from_tokens(used_tokens: u64, window_tokens: Option<u64>) -> Self {
        Self {
            used_tokens: Some(used_tokens),
            window_tokens,
            reported_pct: None,
        }
    }

    /// 已用百分比 0..=100。上游给了就用上游的;否则用 used/window 算;都不成 → None。
    ///
    /// 窗口为 0 时返回 None 而不是除零 —— 0 窗口是上游给了坏值,当作「不知道」处理。
    pub fn used_pct(&self) -> Option<u8> {
        if let Some(pct) = self.reported_pct {
            return Some(pct.min(100));
        }
        let (used, window) = (self.used_tokens?, self.window_tokens?);
        if window == 0 {
            return None;
        }
        // 先乘后除,u64 下 used 要到 1.8e17 才溢出,不可能。
        Some(((used.saturating_mul(100) / window).min(100)) as u8)
    }

    /// 该不该纳入过重提醒。**判据只有窗口大小这一条**(见 [`WARN_MIN_WINDOW`])。
    ///
    /// 窗口未知 → false。fail-closed:证不了就不提醒,宁可漏也不误扰 —— 提醒是会主动往群里
    /// 发东西的路径,按仓规默认保守。
    pub fn warnable(&self) -> bool {
        self.window_tokens.is_some_and(|w| w > WARN_MIN_WINDOW)
    }

    /// 一轮结束时的裁决。`warned_up_to` 是这个会话此前已提醒到的最高档(没提醒过传 None)。
    ///
    /// 一次跨两档(如从 50% 直接到 85%)**只提醒最高那一档**,不连发两条。
    pub fn warn_action(&self, warned_up_to: Option<u8>) -> WarnAction {
        if !self.warnable() {
            return WarnAction::None;
        }
        let Some(pct) = self.used_pct() else {
            return WarnAction::None;
        };
        let lowest = WARN_STEPS[0];
        // 回落到最低档以下:清记录,让后续重新涨上来时还能提醒。
        if pct < lowest {
            return if warned_up_to.is_some() {
                WarnAction::ResetWarned
            } else {
                WarnAction::None
            };
        }
        // 已跨过的最高档;没有比已提醒档更高的就不重复提醒。
        match WARN_STEPS.iter().rev().find(|&&s| pct >= s) {
            Some(&step) if warned_up_to.is_none_or(|w| step > w) => WarnAction::Warn(step),
            _ => WarnAction::None,
        }
    }
}

/// 进度条宽度(格)。每格 10%。
const BAR_CELLS: usize = 10;

/// 把百分比渲染成进度条,如 `████░░░░░░ 42%`。
///
/// **只在拿得到百分比时调用** —— 调用方对 `used_pct()` 为 None 的情形应整项省略,而不是渲染一个
/// 空槽进度条:空条会被读成「0%」,那是错误信息,比不显示更糟。
pub fn render_bar(pct: u8) -> String {
    let pct = pct.min(100);
    // 四舍五入到格:让 5% 也能点亮一格,避免「有占用却显示全空」。
    let filled = ((usize::from(pct) * BAR_CELLS).div_ceil(100)).min(BAR_CELLS);
    format!(
        "{}{} {pct}%",
        "█".repeat(filled),
        "░".repeat(BAR_CELLS - filled)
    )
}

/// 把 token 数渲染成紧凑文本(`238k` / `1.2M`)。
pub fn render_tokens(n: u64) -> String {
    match n {
        n if n >= 1_000_000 => format!("{:.1}M", n as f64 / 1_000_000.0),
        n if n >= 1_000 => format!("{}k", n / 1_000),
        n => n.to_string(),
    }
}

/// 同时渲染上下文绝对使用量与百分比。缺少某一维时保留可证实的值，并明确标出不可用项。
pub fn render_usage(budget: &ContextBudget) -> Option<String> {
    match (budget.used_tokens, budget.window_tokens, budget.used_pct()) {
        (Some(used), Some(window), Some(pct)) => Some(format!(
            "{} / {} · {}",
            render_used(budget, used),
            render_tokens(window),
            render_bar(pct)
        )),
        (Some(used), _, None) => Some(summon_i18n::format!("r300230cf65", render_tokens(used))),
        (None, _, Some(pct)) => Some(summon_i18n::format!("r9f1881dd04", render_bar(pct))),
        _ => None,
    }
}

/// [`render_usage`] 的窄版,给回复卡角标那一行用:`430k/1.0M █████░░░░░ 43%`。角标一行挤着
/// 模型、档位与额度,有进度条时它自己就说明了这是上下文,省掉 `ctx` 与分隔空格,也不标 `≈` ——
/// 角标是一眼扫的读数,宽度比「这个数是估出来的」更要紧;没有进度条时一个孤零零的 `238k` 读不出
/// 是什么,留着 `ctx`。缺哪一维就只留能证实的那一半,不写「不可用」。
pub fn render_usage_compact(budget: &ContextBudget) -> Option<String> {
    match (budget.used_tokens, budget.window_tokens, budget.used_pct()) {
        (Some(used), Some(window), Some(pct)) => Some(format!(
            "{}/{} {}",
            render_tokens(used),
            render_tokens(window),
            render_bar(pct)
        )),
        (Some(used), _, None) => Some(format!("ctx {}", render_tokens(used))),
        (None, _, Some(pct)) => Some(render_bar(pct)),
        _ => None,
    }
}

/// 已用 token 数;由上游百分比反推出来的值带 `≈`。
fn render_used(budget: &ContextBudget, used: u64) -> String {
    if budget.reported_pct.is_some() {
        format!("≈{}", render_tokens(used))
    } else {
        render_tokens(used)
    }
}

/// 上下文提醒卡按会话有没有手动压缩通道分两种(owner 2026-10-15 / 2026-10-20):`Compact` 的卡
/// 上另有 Compact,Handoff 写完交接后原地压缩;`Handoff` 的卡只有 Handoff,写完交接后归档当前会话、
/// 新会话从交接文件接着做。
///
/// **缺省是 Handoff**:交接在三个 backend 上都能跑,压缩只有 tmux 会话能跑;任何忘了赋值的构造
/// 路径落在 Handoff,按钮点下去总有效果。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ContextRemedy {
    Compact,
    #[default]
    Handoff,
}

impl ContextRemedy {
    /// 写进卡片按钮值的名字;收尾按它重绘同一种布局。
    pub fn wire(self) -> &'static str {
        match self {
            Self::Compact => "compact",
            Self::Handoff => "handoff",
        }
    }

    pub fn from_wire(name: &str) -> Option<Self> {
        match name {
            "compact" => Some(Self::Compact),
            "handoff" => Some(Self::Handoff),
            _ => None,
        }
    }
}

/// 下一次提醒的时机,如「到 80% 之前不会再提醒」。最高档之后要等占用回落、重新计档。
pub fn next_warning_hint(step: u8) -> String {
    match WARN_STEPS.iter().find(|&&s| s > step) {
        Some(next) => summon_i18n::format!("ree42a1a264"),
        None => summon_i18n::text!("r8db25788f5").to_string(),
    }
}

/// 压缩后的上下文占用。`summary_tokens` 是压缩边界报的摘要与保留消息体量,不含系统提示、工具定义
/// 这些每轮都带的基础占用;`base_tokens` 按本会话第一轮请求的输入量估。两者都知道才给得出总量。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompactedContext {
    pub summary_tokens: u64,
    pub base_tokens: Option<u64>,
    pub window_tokens: Option<u64>,
}

impl CompactedContext {
    /// 估算的总占用;不知道基础占用时为 `None`,只有摘要体量不能当总量报。
    pub fn budget(&self) -> Option<ContextBudget> {
        self.base_tokens
            .map(|base| ContextBudget::from_tokens(self.summary_tokens + base, self.window_tokens))
    }
}

/// 占用的一行展示:有窗口时是进度条加「约 已用 / 窗口」,只知道已用时是「约 已用」。
pub fn usage_detail(budget: &ContextBudget) -> String {
    match (budget.used_tokens, budget.window_tokens) {
        (Some(used), Some(window)) => summon_i18n::format!(
            "rb7530cdc10",
            budget.used_pct().map(render_bar).unwrap_or_default(),
            render_tokens(used),
            render_tokens(window)
        ),
        (Some(used), None) if budget.used_pct().is_none() => {
            summon_i18n::format!("rc91a70afa9", render_tokens(used))
        }
        _ => budget.used_pct().map(render_bar).unwrap_or_default(),
    }
}

/// 上下文过重提醒卡的正文(markdown)。卡上的按钮由调用方按 `remedy` 摆,正文只说明每个按钮
/// 会做什么。
pub fn warning_text(step: u8, budget: &ContextBudget, remedy: ContextRemedy) -> String {
    let detail = usage_detail(budget);
    let action = match remedy {
        ContextRemedy::Compact => {
            summon_i18n::text!("r8c2b6dc891")
        }
        ContextRemedy::Handoff => {
            summon_i18n::text!("r6715519a17")
        }
    };
    summon_i18n::format!("r9a550bf397", next_warning_hint(step))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn used_pct_prefers_upstream_reported_value() {
        // 上游给了 35%,即便 used/window 算出来是别的数,也以上游为准。
        let b = ContextBudget {
            used_tokens: Some(900_000),
            window_tokens: Some(1_000_000),
            reported_pct: Some(35),
        };
        assert_eq!(b.used_pct(), Some(35));
    }

    #[test]
    fn used_pct_computes_from_tokens_when_not_reported() {
        let b = ContextBudget::from_tokens(250_000, Some(1_000_000));
        assert_eq!(b.used_pct(), Some(25));
    }

    #[test]
    fn used_pct_is_none_when_window_missing_or_zero() {
        assert_eq!(ContextBudget::from_tokens(1000, None).used_pct(), None);
        assert_eq!(ContextBudget::from_tokens(1000, Some(0)).used_pct(), None);
    }

    #[test]
    fn warnable_only_depends_on_window_size() {
        // 恰好等于下限 → 不提醒(严格大于)。
        assert!(!ContextBudget::from_tokens(1, Some(WARN_MIN_WINDOW)).warnable());
        assert!(ContextBudget::from_tokens(1, Some(WARN_MIN_WINDOW + 1)).warnable());
        // 窗口未知 → fail-closed。
        assert!(!ContextBudget::from_tokens(999_999, None).warnable());
    }

    /// 任一后端缺窗口时都按同一规则降级。
    #[test]
    fn missing_window_degrades_without_special_casing() {
        let b = ContextBudget::from_tokens(238_000, None);
        assert_eq!(b.used_pct(), None, "不显示百分比");
        assert!(!b.warnable(), "不提醒");
        assert_eq!(b.warn_action(None), WarnAction::None);
        // 但绝对值仍可显示。
        assert_eq!(render_tokens(b.used_tokens.unwrap()), "238k");
    }

    #[test]
    fn warn_action_fires_once_per_step() {
        let w = |pct: u64, warned| {
            ContextBudget::from_tokens(pct * 10_000, Some(1_000_000)).warn_action(warned)
        };
        assert_eq!(w(59, None), WarnAction::None, "未到最低档");
        assert_eq!(w(60, None), WarnAction::Warn(60), "恰好到档即提醒");
        assert_eq!(w(65, Some(60)), WarnAction::None, "同档不重复");
        assert_eq!(w(80, Some(60)), WarnAction::Warn(80), "涨到下一档再提醒");
        assert_eq!(w(95, Some(80)), WarnAction::None, "最高档之后不再提醒");
    }

    #[test]
    fn warn_action_skips_intermediate_step_when_jumping() {
        // 50% 直接跳到 85%:只提醒最高的 80,不连发 60 和 80 两条。
        let b = ContextBudget::from_tokens(850_000, Some(1_000_000));
        assert_eq!(b.warn_action(None), WarnAction::Warn(80));
    }

    #[test]
    fn warn_action_resets_when_usage_falls_back() {
        let low = ContextBudget::from_tokens(200_000, Some(1_000_000));
        // compact 之后掉回 20%:清记录,让之后涨回来还能再提醒。
        assert_eq!(low.warn_action(Some(80)), WarnAction::ResetWarned);
        // 本来就没提醒过,不用清。
        assert_eq!(low.warn_action(None), WarnAction::None);
    }

    #[test]
    fn warn_action_stays_silent_for_small_window_models() {
        // 256k 窗口:即使 95% 也不提醒 —— 它自己会压缩。
        let b = ContextBudget::from_tokens(243_000, Some(256_000));
        assert_eq!(b.used_pct(), Some(94));
        assert_eq!(b.warn_action(None), WarnAction::None);
    }

    #[test]
    fn from_reported_pct_back_computes_tokens_for_display() {
        let b = ContextBudget::from_reported_pct(42, Some(1_000_000));
        assert_eq!(b.used_pct(), Some(42));
        assert_eq!(b.used_tokens, Some(420_000));
        // 没有窗口时只留百分比,不瞎编绝对值。
        let b2 = ContextBudget::from_reported_pct(42, None);
        assert_eq!(b2.used_pct(), Some(42));
        assert_eq!(b2.used_tokens, None);
    }

    #[test]
    fn render_bar_lights_at_least_one_cell_for_nonzero() {
        assert_eq!(render_bar(0), "░░░░░░░░░░ 0%");
        assert_eq!(render_bar(5), "█░░░░░░░░░ 5%");
        assert_eq!(render_bar(42), "█████░░░░░ 42%");
        assert_eq!(render_bar(100), "██████████ 100%");
    }

    #[test]
    fn warning_text_mentions_step_and_next_step() {
        let b = ContextBudget::from_tokens(600_000, Some(1_000_000));
        let t = warning_text(60, &b, ContextRemedy::Compact);
        assert!(t.contains("60%"), "要说清当前跨的是哪一档");
        assert!(t.contains("600k / 1.0M"), "带绝对值,让人知道离满还有多远");
        assert!(t.contains("80%"), "预告下一档,让人知道不会被反复打扰");

        // 最高档之后没有「下一档」,改说回落重计。
        let t2 = warning_text(
            80,
            &ContextBudget::from_tokens(850_000, Some(1_000_000)),
            ContextRemedy::Compact,
        );
        assert!(t2.contains("回落"), "最高档要说明何时会再提醒");
    }

    #[test]
    fn warning_text_points_at_the_buttons_on_the_card() {
        let b = ContextBudget::from_tokens(600_000, Some(1_000_000));
        let compact = warning_text(60, &b, ContextRemedy::Compact);
        assert!(compact.contains("**Compact**"));
        assert!(compact.contains("**Handoff**") && compact.contains("原地压缩"));
        assert!(!compact.contains("/resume"), "原地交接不归档会话");
        let handoff = warning_text(60, &b, ContextRemedy::Handoff);
        assert!(handoff.contains("**Handoff**"));
        assert!(handoff.contains("/resume"), "说清旧会话还能找回来");
        assert!(
            !handoff.contains("Compact"),
            "无头 backend 没有手动压缩通道"
        );
        for text in [compact, handoff] {
            assert!(text.contains("**Ignore**"));
            assert!(
                !text.contains("/session"),
                "提醒卡自带入口,不指向 /session 面板"
            );
        }
    }

    #[test]
    fn next_warning_hint_names_the_next_step_or_the_fallback() {
        assert_eq!(next_warning_hint(60), "到 80% 之前不会再提醒");
        assert!(next_warning_hint(80).contains("回落"));
    }

    #[test]
    fn context_remedy_round_trips_through_its_wire_name() {
        for remedy in [ContextRemedy::Compact, ContextRemedy::Handoff] {
            assert_eq!(ContextRemedy::from_wire(remedy.wire()), Some(remedy));
        }
        assert_eq!(ContextRemedy::from_wire("archive"), None);
        assert_eq!(
            ContextRemedy::default(),
            ContextRemedy::Handoff,
            "缺省落在三个 backend 都能用的那一个"
        );
    }

    #[test]
    fn usage_detail_shows_what_is_known() {
        assert_eq!(
            usage_detail(&ContextBudget::from_tokens(20_854, Some(1_000_000))),
            "█░░░░░░░░░ 2%(约 20k / 1.0M)"
        );
        assert_eq!(
            usage_detail(&ContextBudget::from_tokens(20_854, None)),
            "约 20k"
        );
        assert_eq!(usage_detail(&ContextBudget::default()), "");
    }

    #[test]
    fn render_tokens_is_compact() {
        assert_eq!(render_tokens(999), "999");
        assert_eq!(render_tokens(238_000), "238k");
        assert_eq!(render_tokens(1_250_000), "1.2M");
    }
}
