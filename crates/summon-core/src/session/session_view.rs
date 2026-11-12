//! 会话呈现态(纯逻辑):`agents ls` 的每会话一行如何判定 live/dead、给什么能力提示、
//! last-active 怎么人读相对化。无任何 IO —— CLI 侧负责发现盘上/ tmux 事实后调这里组装,
//! 单测直接喂构造好的事实断言。
//!
//! 为什么放 core:liveness 真值表 + 相对时间格式是纯决策,跨「真实跑」与「单测」共用,
//! 抽成纯函数才能不依赖 tmux/时钟稳定单测(对齐 CLAUDE.md:纯逻辑下沉 summon-core)。

use crate::config::registry::CaptureMode;
use std::time::{Duration, SystemTime};

/// 一个会话的呈现态(纯数据,CLI 渲染 + 单测共用)。
#[derive(Debug, Clone, PartialEq)]
pub struct SessionView {
    /// agent 名(`agent.name`)。
    pub agent: String,
    /// chat-tag(g/p+hex)或裸名会话的 `(base/owner)`。
    pub tag: String,
    /// tmux 逻辑名(命名键);无头会话也有(用于排序/去重),只是不存在对应 tmux 进程。
    pub session: String,
    /// 原始 chat_id(从 .meta 读;threads-only 孤儿无 .meta 时为 None)。
    pub chat_id: Option<String>,
    /// p2p / group / topic_group;.meta 缺失时从 tag 首字符回退推断。
    pub chat_type: Option<String>,
    /// 群名 / 备注;无则 None。
    pub chat_name: Option<String>,
    /// 该会话的捕获模式 —— 决定 attach vs tail。
    pub capture: CaptureMode,
    /// 判定后的 live/dead 态。
    pub liveness: Liveness,
    /// 最后活跃时刻(events.jsonl mtime 优先,回退 .meta.updated_at);拿不到为 None。
    pub last_active: Option<SystemTime>,
}

/// 按会话类型分流的 live/dead 态。无头(Subprocess)没有 tmux 可查,只能用盘上事件近似,
/// 故用 `recent`/`idle` 而非 `live`/`dead`——诚实标注「盘上近期有无活动」,不谎称进程在跑。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Liveness {
    /// claude/Transcript:对应 tmux 会话存在。
    LiveTmux,
    /// 无头:最近 TTL 内有事件写入。
    RecentHeadless,
    /// 无头:超过 TTL 未见事件(线程可能仍在,但盘上无近活动)。
    IdleHeadless,
    /// 盘上有 meta/threads 残留,但 tmux 不在且非无头(已退出的 claude 残骸)。
    Gone,
    /// **路由生命周期已退休,进程却还活着** —— teardown 失败或资源泄漏,是事故不是残骸。
    ///
    /// 刻意不并进 `Gone`:那会把一起事故从列表里藏掉,而列表恰恰是唯一会有人看的地方。
    /// owner 2026-08-13 拍板「按『别藏事故』,它该显眼地留着」,故 `should_display` 对它恒真。
    ClosedButProcessLive,
}

impl Liveness {
    /// 列宽稳定的人读标签(`agents ls` 对齐列用,长度 ≤ 6)。
    pub fn label(self) -> &'static str {
        match self {
            Liveness::LiveTmux => "live",
            Liveness::RecentHeadless => "recent",
            // 无头没有「idle」概念——进程已退出,只剩盘上残骸。用「stale」诚实表达
            // 「过期历史」,不误导成「进程还在只是闲着」。
            Liveness::IdleHeadless => "stale",
            Liveness::Gone => "gone",
            // 不叫 "closed":它的重点不是"关了",而是"关了却没关掉"。要让人一眼看出该去处理。
            Liveness::ClosedButProcessLive => "leaked",
        }
    }

    /// 是否「活跃」(tmux 在 / 无头近期有活动)。`agents ls` 默认只列活跃的,把 idle/gone 残骸
    /// 折叠掉(`--all` 才全列),避免长期堆积的旧会话把列表撑爆。
    ///
    /// `ClosedButProcessLive` **不算活跃** —— 它是事故态,不是可以正常对话的会话。它的展示由
    /// `should_display` 单独保证,不靠混进"活跃"里蹭一个位置。
    pub fn is_active(self) -> bool {
        matches!(self, Liveness::LiveTmux | Liveness::RecentHeadless)
    }

    /// 这条会话该不该展示给人看。**所有面向人的会话列表都先过这一关**——CLI 表格、fleet 快照、
    /// Menu Bar、飞书 steward。它是第一道闸:fleet 快照(Menu Bar 与 steward 读的就是它)在此之上
    /// 另加 `fleet::is_listed`,无头会话还要 daemon 证明会话进程活着或正在跑一轮。
    ///
    /// 三档:
    /// - `ClosedButProcessLive` **恒展示**,`all` 与否都不折叠。它是 teardown 失败的事故,
    ///   藏起来等于把故障藏起来(owner 2026-08-13 拍板)。
    /// - `Gone` 永不展示,`all` 也不例外:已退出的死历史,列出来只会让人以为还能点。
    /// - 其余按 `all` 收:默认只留活跃的,stale 折叠成一行提示。
    pub fn should_display(self, all: bool) -> bool {
        match self {
            Liveness::ClosedButProcessLive => true,
            Liveness::Gone => false,
            other => all || other.is_active(),
        }
    }
}

// 仅测试使用:生产路径未调用(2026-08-20 死代码盘点确认)。不进 release 二进制。
#[cfg(test)]
/// attach vs tail 能力提示(纯函数,按 capture 分流)。
/// Transcript(claude tmux)可接管 TUI → `attach`;无头只能跟事件流 → `tail`。
pub(crate) fn capability_hint(capture: CaptureMode) -> &'static str {
    match capture {
        CaptureMode::Transcript => "attach|tail",
        // 无头(CliSession / Rpc)没有 TTY 可接管,attach 只会骗人。
        CaptureMode::CliSession | CaptureMode::Rpc => "tail",
    }
}

/// liveness 判定(纯函数):见模块顶设计真值表。
/// - Transcript:tmux 在 → LiveTmux;不在 → Gone(claude 残骸)。
/// - Subprocess:最近 TTL 内有活动 → RecentHeadless;否则若 `stale_ttl` 给定且超
///   过该阈值 → Gone(过期历史,等同残骸);否则 IdleHeadless(stale,非 idle)。
///
/// `stale_ttl`:无头会话超过此时长未活动即归为 Gone。`None` = 永不归 Gone(保持
/// IdleHeadless),兼容旧行为。
///
/// `lifecycle_closed`:**路由生命周期已退休**(见 `session::lifecycle`)。它与 `tmux_present` 是
/// 两个**互不替代**的事实,所以分开传:前者说"summon 还认不认这个 chat",后者说"进程还在不在"。
/// 合并会丢掉一个必须被看见的组合 —— 已退休却还有进程(teardown 失败/泄漏),那是事故,
/// 不是残骸,必须能与 `Gone` 区分开。
///
/// 无头会话没有对应的进程事实:进程被外杀只是单轮失败或 RPC 断线,下一轮会重连,**不等于**
/// 生命周期关闭。所以无头侧只有"已退休 → Gone",没有"但进程还活着"这一态。
pub fn classify_liveness(
    tmux_present: bool,
    capture: CaptureMode,
    last_active: Option<SystemTime>,
    now: SystemTime,
    ttl: Duration,
    stale_ttl: Option<Duration>,
    lifecycle_closed: bool,
) -> Liveness {
    match capture {
        CaptureMode::Transcript => {
            match (lifecycle_closed, tmux_present) {
                // 已退休却还有 tmux:teardown 没做干净。这一格是本次改动存在的理由之一。
                (true, true) => Liveness::ClosedButProcessLive,
                (true, false) => Liveness::Gone,
                (false, true) => Liveness::LiveTmux,
                (false, false) => Liveness::Gone,
            }
        }
        // CliSession / Rpc 同判:活跃证据都只有「盘上最近事件」。
        CaptureMode::CliSession | CaptureMode::Rpc => {
            // 已退休就是已退休:无头没有可查的进程事实,不存在"但进程还活着"的可辨识事故态。
            if lifecycle_closed {
                return Liveness::Gone;
            }
            // tmux 进程存在的边角(显式 headless:false 又被当无头?)也优先按盘上活动判,
            // 不谎称 tmux live —— 无头语义恒以「最近事件」为准。
            if recent_within(last_active, now, ttl) {
                Liveness::RecentHeadless
            } else if stale_ttl
                .map(|s| !recent_within(last_active, now, s))
                .unwrap_or(false)
            {
                // 超过 stale_ttl 未活动:过期历史,等同残骸。
                Liveness::Gone
            } else {
                Liveness::IdleHeadless
            }
        }
    }
}

/// last_active 是否在 now 往前 ttl 之内(None / 未来时刻保守处理)。
fn recent_within(last_active: Option<SystemTime>, now: SystemTime, ttl: Duration) -> bool {
    match last_active {
        // 未来时刻(时钟漂移)当作刚活跃。
        Some(t) if t >= now => true,
        Some(t) => now.duration_since(t).map(|d| d <= ttl).unwrap_or(true),
        None => false,
    }
}

/// now - then 的秒数;`then` 在未来(时钟漂移/回拨)钳到 0 =「刚刚」。
///
/// 为什么钳而不是丢成 `None`:在 fleet 契约里 `last_active_secs` 缺席的语义是「从没活跃过」
/// (排序垫底 + 各端显示「无活动记录」)。拿它表达时钟回拨,等于把「刚动过」谎报成「从没动过」,
/// 比 0 秒的误差失真得多。所有产 `last_active_secs` 的地方都必须用这一个口径。
pub fn elapsed_secs_clamped(then: SystemTime, now: SystemTime) -> u64 {
    now.duration_since(then).map(|d| d.as_secs()).unwrap_or(0)
}

/// now - then 的人读相对串:刚刚 / Ns前 / Nm前 / Nh前 / Nd前 / >30d。
/// then 在未来(时钟漂移)→「刚刚」。None →「-」。
pub fn humanize_since(then: Option<SystemTime>, now: SystemTime) -> String {
    let Some(then) = then else {
        return "-".to_string();
    };
    humanize_elapsed_secs(elapsed_secs_clamped(then, now))
}

/// 同上,但直接吃「已过去多少秒」——`fleet` 快照存的是秒数(JSON 契约),人读时复用同一套分桶,
/// 免得两处各写一份、日久漂移。
pub fn humanize_elapsed_secs(secs: u64) -> String {
    const MIN: u64 = 60;
    const HOUR: u64 = 60 * MIN;
    const DAY: u64 = 24 * HOUR;
    if secs < 10 {
        summon_i18n::text!("rde6785d99e").to_string()
    } else if secs < MIN {
        summon_i18n::format!("rb62c46e355")
    } else if secs < HOUR {
        summon_i18n::format!("rded0d93392", secs / MIN)
    } else if secs < DAY {
        summon_i18n::format!("r82fbde068e", secs / HOUR)
    } else if secs <= 30 * DAY {
        summon_i18n::format!("r741577c09f", secs / DAY)
    } else {
        ">30d".to_string()
    }
}

/// [`humanize_elapsed_secs`] 的英文版,给全英文的 Menu Bar 用。分桶逐档相同,只换措辞 ——
/// 两份分桶各写一套迟早漂,所以阈值由同一组测试一起钉住。
pub fn humanize_elapsed_secs_en(secs: u64) -> String {
    const MIN: u64 = 60;
    const HOUR: u64 = 60 * MIN;
    const DAY: u64 = 24 * HOUR;
    if secs < 10 {
        "just now".to_string()
    } else if secs < MIN {
        format!("{secs}s ago")
    } else if secs < HOUR {
        format!("{}m ago", secs / MIN)
    } else if secs < DAY {
        format!("{}h ago", secs / HOUR)
    } else if secs <= 30 * DAY {
        format!("{}d ago", secs / DAY)
    } else {
        ">30d".to_string()
    }
}

/// 从 chat-tag 首字符回退推断 chat_type(.meta 缺失的 threads-only 孤儿用)。
/// `g`→group、`p`→p2p、其它(裸 owner 名 `(base/owner)` 等)→ None。
pub fn chat_type_from_tag(tag: &str) -> Option<&'static str> {
    match tag.chars().next() {
        Some('g') => Some("group"),
        Some('p') => Some("p2p"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(now: SystemTime, ago: u64) -> SystemTime {
        now - Duration::from_secs(ago)
    }

    #[test]
    fn classify_liveness_transcript_tmux_present_is_live() {
        let now = SystemTime::now();
        assert_eq!(
            classify_liveness(
                true,
                CaptureMode::Transcript,
                None,
                now,
                Duration::from_secs(60),
                None,
                false
            ),
            Liveness::LiveTmux
        );
    }

    #[test]
    fn classify_liveness_transcript_no_tmux_is_gone() {
        let now = SystemTime::now();
        // 即使盘上最近有活动,Transcript 无 tmux 仍判 Gone(claude 残骸,无进程接管)。
        assert_eq!(
            classify_liveness(
                false,
                CaptureMode::Transcript,
                Some(t(now, 1)),
                now,
                Duration::from_secs(60),
                None,
                false
            ),
            Liveness::Gone
        );
    }

    /// 生命周期已退休 **但 tmux 还在** = teardown 没做干净,是**事故**不是残骸。
    ///
    /// 这一格是本次改动存在的理由:把它并进 `Gone` 等于把一起事故从列表里藏掉,而列表是唯一
    /// 会有人看的地方。owner 2026-08-13 拍板「按『别藏事故』,它该显眼地留着」。
    #[test]
    fn closed_lifecycle_with_a_live_tmux_is_a_leak_not_a_corpse() {
        let now = SystemTime::now();
        assert_eq!(
            classify_liveness(
                true, // tmux 还活着
                CaptureMode::Transcript,
                Some(t(now, 1)),
                now,
                Duration::from_secs(600),
                Some(Duration::from_secs(7 * 86400)),
                true, // 但路由已退休
            ),
            Liveness::ClosedButProcessLive
        );
        // 而且它必须在任何模式下都被展示,连 --all 折叠都不该碰它。
        assert!(Liveness::ClosedButProcessLive.should_display(false));
        assert!(Liveness::ClosedButProcessLive.should_display(true));
        // 但它不是「活跃」——不能混进正常会话里蹭位置。
        assert!(!Liveness::ClosedButProcessLive.is_active());
    }

    /// 已退休且进程也没了 = 正常的关闭结局,`Gone`。无头侧没有可查的进程事实,所以只有这一种。
    #[test]
    fn closed_lifecycle_without_process_is_gone_for_both_captures() {
        let now = SystemTime::now();
        let just_now = Some(t(now, 1)); // 一秒前还在活动:时间推断会说「活跃」
        assert_eq!(
            classify_liveness(
                false,
                CaptureMode::Transcript,
                just_now,
                now,
                Duration::from_secs(600),
                Some(Duration::from_secs(7 * 86400)),
                true,
            ),
            Liveness::Gone
        );
        let capture = CaptureMode::Rpc;
        assert_eq!(
            classify_liveness(
                false,
                capture,
                just_now,
                now,
                Duration::from_secs(600),
                Some(Duration::from_secs(7 * 86400)),
                true,
            ),
            Liveness::Gone,
            "{capture:?}:已退休就是已退休,不看它上一秒是不是还在说话"
        );
    }

    /// **没有生命周期记录 ≠ 已关闭**。升级前存量会话全是这一档,判错就是整片会话凭空消失。
    #[test]
    fn unknown_lifecycle_falls_back_to_the_original_criteria() {
        let now = SystemTime::now();
        let just_now = Some(t(now, 1));
        assert_eq!(
            classify_liveness(
                true,
                CaptureMode::Transcript,
                just_now,
                now,
                Duration::from_secs(600),
                Some(Duration::from_secs(7 * 86400)),
                false,
            ),
            Liveness::LiveTmux
        );
        assert_eq!(
            classify_liveness(
                false,
                CaptureMode::Rpc,
                just_now,
                now,
                Duration::from_secs(600),
                Some(Duration::from_secs(7 * 86400)),
                false,
            ),
            Liveness::RecentHeadless,
            "无生命周期记录时必须与本改动前逐字等价"
        );
    }

    /// 展示判据的三档真值表。这条判据曾在 CLI 里被逐字抄了四遍,而 `fleet::build_agent` 手写了
    /// 第五遍 —— 抄第 N 份的代价不是重复,是各消费端迟早给出互相矛盾的答案。
    #[test]
    fn display_criterion_has_exactly_three_tiers() {
        // 事故态:恒展示。
        assert!(Liveness::ClosedButProcessLive.should_display(false));
        // 死历史:恒不展示,--all 也不例外。
        assert!(!Liveness::Gone.should_display(false));
        assert!(!Liveness::Gone.should_display(true));
        // 活跃:两种模式都展示。
        for live in [Liveness::LiveTmux, Liveness::RecentHeadless] {
            assert!(live.should_display(false));
            assert!(live.should_display(true));
        }
        // stale:默认折叠,--all 才出来 —— 这正是「被折叠数」的定义域。
        assert!(!Liveness::IdleHeadless.should_display(false));
        assert!(Liveness::IdleHeadless.should_display(true));
    }

    #[test]
    fn classify_liveness_headless_recent_activity_is_recent() {
        let now = SystemTime::now();
        assert_eq!(
            classify_liveness(
                false,
                CaptureMode::Rpc,
                Some(t(now, 30)),
                now,
                Duration::from_secs(60),
                None,
                false
            ),
            Liveness::RecentHeadless
        );
    }

    #[test]
    fn classify_liveness_headless_stale_activity_is_idle() {
        let now = SystemTime::now();
        // 超 TTL 但未超 stale_ttl → IdleHeadless(stale 标签)。
        assert_eq!(
            classify_liveness(
                false,
                CaptureMode::Rpc,
                Some(t(now, 600)),
                now,
                Duration::from_secs(60),
                None,
                false
            ),
            Liveness::IdleHeadless
        );
    }

    #[test]
    fn classify_liveness_headless_beyond_stale_ttl_is_gone() {
        let now = SystemTime::now();
        // 超 stale_ttl(7d)→ Gone,过期历史等同残骸。
        let stale = Duration::from_secs(7 * 86400);
        assert_eq!(
            classify_liveness(
                false,
                CaptureMode::Rpc,
                Some(t(now, 10 * 86400)),
                now,
                Duration::from_secs(60),
                Some(stale),
                false
            ),
            Liveness::Gone
        );
    }

    #[test]
    fn classify_liveness_headless_within_stale_ttl_stays_idle() {
        let now = SystemTime::now();
        // 超 TTL 但未超 stale_ttl → 仍 IdleHeadless,不归 Gone。
        let stale = Duration::from_secs(7 * 86400);
        assert_eq!(
            classify_liveness(
                false,
                CaptureMode::Rpc,
                Some(t(now, 3 * 86400)),
                now,
                Duration::from_secs(60),
                Some(stale),
                false
            ),
            Liveness::IdleHeadless
        );
    }

    #[test]
    fn is_active_only_for_live_and_recent() {
        // `agents ls` 默认折叠的依据:idle/gone 不算活跃。
        assert!(Liveness::LiveTmux.is_active());
        assert!(Liveness::RecentHeadless.is_active());
        assert!(!Liveness::IdleHeadless.is_active());
        assert!(!Liveness::Gone.is_active());
    }

    #[test]
    fn classify_liveness_headless_no_activity_is_idle() {
        let now = SystemTime::now();
        assert_eq!(
            classify_liveness(
                false,
                CaptureMode::Rpc,
                None,
                now,
                Duration::from_secs(60),
                None,
                false
            ),
            Liveness::IdleHeadless
        );
    }

    #[test]
    fn classify_liveness_headless_with_tmux_still_uses_activity() {
        // tmux_present 对无头不改变结论:仍看盘上活动。
        let now = SystemTime::now();
        assert_eq!(
            classify_liveness(
                true,
                CaptureMode::Rpc,
                Some(t(now, 5)),
                now,
                Duration::from_secs(60),
                None,
                false
            ),
            Liveness::RecentHeadless
        );
    }

    #[test]
    fn capability_hint_maps_by_capture() {
        assert_eq!(capability_hint(CaptureMode::Transcript), "attach|tail");
        assert_eq!(capability_hint(CaptureMode::Rpc), "tail");
        assert_eq!(capability_hint(CaptureMode::CliSession), "tail");
    }

    #[test]
    fn liveness_label_widths_stable() {
        for l in [
            Liveness::LiveTmux,
            Liveness::RecentHeadless,
            Liveness::IdleHeadless,
            Liveness::Gone,
        ] {
            assert!(l.label().len() <= 6, "label {} too wide", l.label());
            assert!(!l.label().is_empty());
        }
    }

    #[test]
    fn humanize_since_buckets() {
        let now = SystemTime::now();
        assert_eq!(humanize_since(None, now), "-");
        assert_eq!(humanize_since(Some(now), now), "刚刚");
        assert_eq!(humanize_since(Some(t(now, 3)), now), "刚刚");
        assert_eq!(humanize_since(Some(t(now, 45)), now), "45s前");
        assert_eq!(humanize_since(Some(t(now, 90)), now), "1m前");
        assert_eq!(humanize_since(Some(t(now, 3 * 3600)), now), "3h前");
        assert_eq!(humanize_since(Some(t(now, 2 * 86400)), now), "2d前");
        assert_eq!(humanize_since(Some(t(now, 40 * 86400)), now), ">30d");
    }

    #[test]
    fn humanize_since_future_is_just_now() {
        let now = SystemTime::now();
        let future = now + Duration::from_secs(120);
        assert_eq!(humanize_since(Some(future), now), "刚刚");
    }

    #[test]
    fn elapsed_secs_clamps_future_to_zero() {
        // 时钟回拨:宁可报 0 秒(「刚刚」),也不能退化成「从没活跃过」。
        let now = SystemTime::now();
        assert_eq!(elapsed_secs_clamped(now + Duration::from_secs(120), now), 0);
        assert_eq!(elapsed_secs_clamped(t(now, 42), now), 42);
    }

    #[test]
    fn chat_type_from_tag_infers_prefix() {
        assert_eq!(chat_type_from_tag("gdeadbeef"), Some("group"));
        assert_eq!(chat_type_from_tag("p0011aabb"), Some("p2p"));
        assert_eq!(chat_type_from_tag("(base/owner)"), None);
    }

    #[test]
    fn english_elapsed_text_uses_the_same_buckets_as_chinese() {
        let cases = [
            (0, "刚刚", "just now"),
            (9, "刚刚", "just now"),
            (10, "10s前", "10s ago"),
            (59, "59s前", "59s ago"),
            (60, "1m前", "1m ago"),
            (3_599, "59m前", "59m ago"),
            (3_600, "1h前", "1h ago"),
            (86_399, "23h前", "23h ago"),
            (86_400, "1d前", "1d ago"),
            (30 * 86_400, "30d前", "30d ago"),
            (30 * 86_400 + 1, ">30d", ">30d"),
        ];
        for (secs, zh, en) in cases {
            assert_eq!(humanize_elapsed_secs(secs), zh, "secs={secs}");
            assert_eq!(humanize_elapsed_secs_en(secs), en, "secs={secs}");
        }
    }
}
