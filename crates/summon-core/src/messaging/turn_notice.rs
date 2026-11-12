//! 一轮的通知卡:后台任务完成通知 + 跨会话消息,合在一张卡上滚动(纯件,无 IO)。
//!
//! 这两类事件都不是「这一轮的回复」,但都发生在这一轮里,而且都该在**发生那一刻**被看见:
//! `Bash(run_in_background)` / Agent / Workflow 跑完时 claude 往转录里注入一条
//! `<task-notification>`;`SendMessage @peer` 的收发同样只在转录里留一行。一条一张卡的话,
//! 一个 workflow 派十几个 agent 就是十几张卡砸进群里,而这一轮真正的回复被挤到屏幕外。
//!
//! 于是一轮的事件合并在**一张卡**上原地滚动,按到达顺序**一条一行**。卡上只留最近的几条,
//! 被滚掉的那些仍在 claude 自己的转录 jsonl 里 —— 这两类事件都不进 summon 的 events.jsonl
//! (它们在转录里是字符串型 content / queue-operation 行,typed 事件流解析不到)。

use std::collections::VecDeque;

/// 卡上单条正文的字符上界。一条派单的全文该去转录里读,卡上认得出「谁发来的、说了什么」即可;
/// 取一千是因为真实派单和任务 summary 都在这个量级以内,够用又不至于把卡撑成一屏。
const MAX_BODY_CHARS: usize = 1000;

/// 对端名字的字符上界。它和正文同样来自外部(入向取对端写的 `from-name`,出向取模型写的 `to`),
/// 只是短得多 —— 给个上界是因为「短」是惯例不是保证。
const MAX_WHO_CHARS: usize = 80;

/// 跨会话消息的方向。入向是别人发来的(带正文),出向是本会话发出去的(带一句摘要)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerDirection {
    In,
    Out,
}

/// 一条事件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnNotice {
    /// 后台任务完成通知的 summary(`Background command "…" completed (exit code 0)`)。
    Task(String),
    /// 一条跨会话消息。`who` 是对端会话名,`body` 是正文/摘要(可空 —— 出向未必带摘要)。
    Peer {
        direction: PeerDirection,
        who: String,
        body: String,
    },
}

impl TurnNotice {
    /// 一条后台任务完成通知。空白 summary 不成条:它和没有通知是同一个意思。
    pub fn task(summary: &str) -> Option<Self> {
        let summary = summary.trim();
        (!summary.is_empty()).then(|| Self::Task(fold_lines(summary, MAX_BODY_CHARS)))
    }

    /// 一条跨会话消息。正文可空(出向未必带摘要),但对端名字空就无从指认,整条不成条。
    pub fn peer(direction: PeerDirection, who: &str, body: &str) -> Option<Self> {
        let who = who.trim();
        (!who.is_empty()).then(|| Self::Peer {
            direction,
            who: clip(who, MAX_WHO_CHARS),
            body: fold_lines(body.trim(), MAX_BODY_CHARS),
        })
    }

    fn is_peer(&self) -> bool {
        matches!(self, Self::Peer { .. })
    }

    /// 卡上这一行的 markdown。**一条事件恰好一行** —— 正文里的换行在构造时就折掉了,
    /// 否则一条外部正文能在卡上伪造出一行 `✅ 任务完成`,而抬头的计数对不上。
    fn render(&self) -> String {
        match self {
            Self::Task(summary) => format!("✅ {}", sanitize(summary)),
            Self::Peer {
                direction,
                who,
                body,
            } => {
                let head = match direction {
                    PeerDirection::In => summon_i18n::format!("r77144cbe36", sanitize(who)),
                    PeerDirection::Out => summon_i18n::format!("rc93f87dd6c", sanitize(who)),
                };
                if body.is_empty() {
                    head
                } else {
                    format!("{head}:{}", sanitize(body))
                }
            }
        }
    }
}

/// 一轮内的事件窗口:两类**各自**留最近 `max` 条 + 各自的累计总数。
///
/// **配额必须分类,不能共用一个 LRU**:两类事件的密度差一个量级 —— 一个 workflow 轮几十条任务
/// 通知,跨会话消息一轮才三两条。共用窗口的话,后到的任务通知会把跨会话消息整个挤出去,
/// 卡上一条都不剩,而合并这张卡的目的正是让它们同时可见。
#[derive(Debug, Clone)]
pub struct TurnNoticeWindow {
    recent: VecDeque<TurnNotice>,
    peers: usize,
    tasks: usize,
    max: usize,
}

/// 拿不到会话配置的路径(会话镜像的转录尾随)用内置默认窗口。
impl Default for TurnNoticeWindow {
    fn default() -> Self {
        Self::new(crate::progress::DEFAULT_MAX_STEPS)
    }
}

impl TurnNoticeWindow {
    /// `max` 是**每类**的窗口大小。`0` 视作 1:一条不留的话事件一到就被裁掉,卡上永远空着 ——
    /// 那不是「配置成不显示」,是把功能配坏。
    pub fn new(max: usize) -> Self {
        Self {
            recent: VecDeque::new(),
            peers: 0,
            tasks: 0,
            max: max.max(1),
        }
    }

    /// 记一条事件。构造(含空白校验与截断)在 [`TurnNotice::task`] / [`TurnNotice::peer`],
    /// 好让实时刷卡那条路和轮收尾那条路押上**同一份**规范化结果。
    pub fn push(&mut self, notice: TurnNotice) {
        let is_peer = notice.is_peer();
        if is_peer {
            self.peers += 1;
        } else {
            self.tasks += 1;
        }
        self.recent.push_back(notice);
        // 只淘汰同类里最老的那条 —— 顺序照旧由 `recent` 承载,两类仍按到达先后混排。
        while self
            .recent
            .iter()
            .filter(|n| n.is_peer() == is_peer)
            .count()
            > self.max
        {
            let victim = self
                .recent
                .iter()
                .position(|n| n.is_peer() == is_peer)
                .expect(summon_i18n::text!("r8312be9bf3"));
            self.recent.remove(victim);
        }
    }

    pub fn is_empty(&self) -> bool {
        self.total() == 0
    }

    pub fn total(&self) -> usize {
        self.peers + self.tasks
    }

    pub fn recent(&self) -> impl Iterator<Item = &TurnNotice> {
        self.recent.iter()
    }

    /// 卡片正文(markdown)。抬头分类报数,好让人看出**哪一类**被滚掉了几条 —— 只报一个总数的话,
    /// 「跨会话 2 条」和「后台任务 2 个」在卡上分不出来。
    pub fn render(&self) -> String {
        let lines = self.lines_within_budget();
        let shown_peers = lines.iter().filter(|(is_peer, _)| *is_peer).count();
        let shown_tasks = lines.len() - shown_peers;
        let mut counts = Vec::new();
        if self.peers > 0 {
            counts.push(count_phrase(
                summon_i18n::text!("r385ea2980e"),
                self.peers,
                summon_i18n::text!("rf004f1d84c"),
                shown_peers,
            ));
        }
        if self.tasks > 0 {
            counts.push(count_phrase(
                summon_i18n::text!("r4c46c90439"),
                self.tasks,
                summon_i18n::text!("r3a116815bc"),
                shown_tasks,
            ));
        }
        let mut body = summon_i18n::format!("rf81e850840", counts.join(" · "));
        for (_, line) in lines {
            body.push('\n');
            body.push_str(&line);
        }
        body
    }

    /// 从最新一条往回收,收到装不下为止。窗口本身已按类限量,这一刀是给「`max` 被配大 + 每条正文
    /// 都顶到上界」那种最坏情形兜底:超预算的卡飞书会整张拒收,那时用户一条事件都看不到,
    /// 而少显示两条只是少显示两条。
    fn lines_within_budget(&self) -> Vec<(bool, String)> {
        let mut budget = crate::feishu::CARD_TEXT_SAFE_LIMIT;
        let mut lines: Vec<(bool, String)> = Vec::new();
        for notice in self.recent.iter().rev() {
            let line = notice.render();
            let cost = line.len() + 1;
            if cost > budget {
                break;
            }
            budget -= cost;
            lines.push((notice.is_peer(), line));
        }
        lines.reverse();
        lines
    }
}

/// 卡上每一处外部文本的出站处理。**卡上没有一个字来自 summon 自己**(命令原文、对端会话名、
/// 对端说的话),所以两件事都要做,而且两类事件一视同仁:
///
/// - 标签形态转义:`<at user_id=…>` 不转义就能凭一条命令或一条派单在群里 @ 到真人;
/// - 裸 `@名字` 断开匹配:下游渲染器会把它替换成真实提及(镜像那条路认真人、主路径认 agent
///   别名),于是一条第三方写的正文能 @ 到人、甚至触发别的 bot 应答。这张卡是留痕不是投递,
///   要真提及的语义在回复正文那条路上,不在这里。
fn sanitize(text: &str) -> String {
    crate::feishu::escape_lark_md(text)
        .replace('@', &format!("@{}", crate::feishu::LARK_MD_TAG_BREAKER))
}

/// 抬头里的一段计数。只有真被滚掉的那一类才带括注 —— 一类被裁就给两类都写「各显示最近 N 条」的话,
/// 抬头会对着一条没少的那类说谎,而这张卡唯一的职责就是把数报准。
fn count_phrase(label: &str, total: usize, unit: &str, shown: usize) -> String {
    if total > shown {
        summon_i18n::format!("rdea408b4a5")
    } else {
        format!("{label} {total} {unit}")
    }
}

fn clip(text: &str, max: usize) -> String {
    let mut chars = text.chars();
    let head: String = chars.by_ref().take(max).collect();
    if chars.next().is_some() {
        format!("{head}…")
    } else {
        head
    }
}

/// 折掉换行再截断:一条事件占一行是这张卡的形状,多行正文会让 `✅` / `⇄` 这些行首标记失去意义。
fn fold_lines(text: &str, max: usize) -> String {
    let folded = text.split_whitespace().collect::<Vec<_>>().join(" ");
    clip(&folded, max)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(summary: &str) -> TurnNotice {
        TurnNotice::task(summary).expect("非空")
    }

    fn peer_in(who: &str, body: &str) -> TurnNotice {
        TurnNotice::peer(PeerDirection::In, who, body).expect("非空")
    }

    fn peer_out(who: &str, body: &str) -> TurnNotice {
        TurnNotice::peer(PeerDirection::Out, who, body).expect("非空")
    }

    #[test]
    fn window_keeps_only_the_newest_and_counts_everything() {
        let mut window = TurnNoticeWindow::new(3);
        assert!(window.is_empty());
        for n in 1..=5 {
            window.push(task(&format!("任务 {n} 完成")));
        }
        assert_eq!(window.total(), 5);
        assert_eq!(
            window.recent().map(TurnNotice::render).collect::<Vec<_>>(),
            ["✅ 任务 3 完成", "✅ 任务 4 完成", "✅ 任务 5 完成"]
        );
    }

    /// **合并这张卡的目的就是两类同时可见**,所以配额必须分类:一轮里两条跨会话消息在前、
    /// 五条任务通知在后(2026-09-03 owner 那一轮的真实顺序),共用 LRU 的话跨会话消息会被
    /// 挤得一条不剩 —— 那等于把需求做反了。
    #[test]
    fn a_burst_of_task_notices_never_evicts_the_peer_messages() {
        let mut window = TurnNoticeWindow::new(3);
        window.push(peer_out("aa5d6a99", "按 review 修复更新测试用例"));
        window.push(peer_out("aa5d6a99", "SPM 覆盖不到 AFM 模板"));
        for n in 1..=5 {
            window.push(task(&format!("任务 {n} 完成")));
        }
        let body = window.render();
        assert!(
            body.starts_with("**通知** · 跨会话 2 条 · 后台任务 5 个(显示最近 3 条)\n"),
            "{body}"
        );
        assert!(
            body.contains("⇄ 发往 `aa5d6a99`:按 review 修复更新测试用例"),
            "{body}"
        );
        assert!(
            body.contains("⇄ 发往 `aa5d6a99`:SPM 覆盖不到 AFM 模板"),
            "{body}"
        );
        assert!(!body.contains("任务 1 完成"), "同类里最老的照旧滚掉:{body}");
        assert!(body.contains("✅ 任务 5 完成"), "{body}");
    }

    /// 空白不成条:它和什么都没发生是同一个意思,不该占掉窗口里的一格、也不该计进总数。
    #[test]
    fn blank_events_do_not_become_notices() {
        assert_eq!(TurnNotice::task("   "), None);
        assert_eq!(TurnNotice::task(""), None);
        assert_eq!(
            TurnNotice::peer(PeerDirection::In, "  ", "有正文但没有对端"),
            None
        );
    }

    #[test]
    fn zero_window_still_shows_the_latest_one() {
        let mut window = TurnNoticeWindow::new(0);
        window.push(task("第一条"));
        window.push(task("第二条"));
        assert_eq!(
            window.recent().map(TurnNotice::render).collect::<Vec<_>>(),
            ["✅ 第二条"]
        );
        assert_eq!(window.total(), 2);
    }

    /// 两类事件按发生顺序混排 —— 分组会让「先收到派单、再跑完任务」这个顺序失真,
    /// 而顺序正是这张卡唯一能传达的东西。
    #[test]
    fn task_and_peer_events_roll_in_arrival_order() {
        let mut window = TurnNoticeWindow::new(3);
        window.push(peer_in("starlight", "帮我跑一下回归"));
        window.push(task(
            "Background command \"cargo test\" completed (exit code 0)",
        ));
        window.push(peer_out("aa5d6a99", "回归全绿"));
        assert_eq!(
            window.render(),
            "**通知** · 跨会话 2 条 · 后台任务 1 个\n\
             ⇄ 来自 `starlight`:帮我跑一下回归\n\
             ✅ Background command \"cargo test\" completed (exit code 0)\n\
             ⇄ 发往 `aa5d6a99`:回归全绿"
        );
    }

    /// 括注跟着**被裁的那一类**走:一类满一类没满时给两类都写「各显示最近 N 条」,
    /// 抬头就会对着一条没少的那类说谎。
    #[test]
    fn only_the_clipped_kind_carries_the_window_note() {
        let mut window = TurnNoticeWindow::new(3);
        for n in 1..=5 {
            window.push(peer_out("p", &format!("第 {n} 条")));
        }
        window.push(task("任务甲"));
        window.push(task("任务乙"));
        let body = window.render();
        assert!(
            body.starts_with("**通知** · 跨会话 5 条(显示最近 3 条) · 后台任务 2 个\n"),
            "{body}"
        );
    }

    /// 没滚掉就只报数,别让三条以内的卡也带一句废话。
    #[test]
    fn nothing_clipped_means_no_window_note() {
        let mut window = TurnNoticeWindow::new(3);
        window.push(task(
            "Background command \"cargo test\" completed (exit code 0)",
        ));
        window.push(task("Agent \"研究任务\" completed"));
        assert_eq!(
            window.render(),
            "**通知** · 后台任务 2 个\n\
             ✅ Background command \"cargo test\" completed (exit code 0)\n\
             ✅ Agent \"研究任务\" completed"
        );
    }

    /// 卡上没有一个字来自 summon 自己:命令原文、对端会话名、对端说的话,三处都要挡住
    /// 「凭一行文本在群里 @ 到人」—— 标签形态转义,裸 `@` 断开下游的提及匹配。
    #[test]
    fn every_external_field_is_sanitized() {
        let mut window = TurnNoticeWindow::new(3);
        window.push(task(
            "Background command \"cat <at user_id=\\\"ou_x\\\">a</at>\" completed",
        ));
        window.push(peer_in(
            "<at user_id=\"ou_evil\">老板</at>",
            "转给 @炼金师 看看",
        ));
        let body = window.render();
        assert!(!body.contains("<at "), "对端名字与正文都不得留标签:{body}");
        assert!(!body.contains("</at>"), "{body}");
        assert!(
            !body.contains("@炼金师"),
            "裸 @ 要断开,否则下游会渲成真提及并惊动那个 bot:{body}"
        );
        assert!(body.starts_with("**通知**"), "标题自己的加粗要保留:{body}");
    }

    /// 一条事件恰好一行:正文里的换行折掉,否则外部文本能伪造一行 `✅ 任务完成`,
    /// 而抬头的计数与卡上的 `✅` 行数会对不上。
    #[test]
    fn a_notice_never_spans_more_than_one_line() {
        let mut window = TurnNoticeWindow::new(3);
        window.push(peer_in("p", "第一行\n✅ 伪造的任务完成行\n第三行"));
        let body = window.render();
        assert_eq!(body.lines().count(), 2, "抬头一行 + 事件一行:{body}");
        assert_eq!(
            body.lines().filter(|l| l.starts_with("✅")).count(),
            0,
            "外部正文不该伪造出事件行:{body}"
        );
    }

    /// 正文与对端名字都有上界:一条派单的全文该去转录里读,卡上只要认得出大意。
    #[test]
    fn long_fields_are_clipped() {
        let mut window = TurnNoticeWindow::new(3);
        let long_body: String = "甲".repeat(MAX_BODY_CHARS + 50);
        let long_who: String = "乙".repeat(MAX_WHO_CHARS + 50);
        window.push(peer_in(&long_who, &long_body));
        let body = window.render();
        assert_eq!(body.chars().filter(|c| *c == '甲').count(), MAX_BODY_CHARS);
        assert_eq!(body.chars().filter(|c| *c == '乙').count(), MAX_WHO_CHARS);
    }

    /// 窗口大小是用户可配的(`messaging.progress_max_steps`),配大之后「每类 N 条 × 每条顶到
    /// 正文上界」可以撑破飞书的卡片预算 —— 那时整张卡发不出去,用户一条事件都看不到。
    /// 宁可少显示最老的几条。
    #[test]
    fn an_oversized_window_is_trimmed_to_fit_the_card_budget() {
        let mut window = TurnNoticeWindow::new(64);
        let long: String = "甲".repeat(MAX_BODY_CHARS);
        for n in 1..=64 {
            window.push(peer_in(&format!("p{n}"), &long));
        }
        let body = window.render();
        assert!(
            body.len() <= crate::feishu::CARD_TEXT_SAFE_LIMIT,
            "渲染结果 {} 字节,超出卡片预算",
            body.len()
        );
        assert!(
            body.starts_with("**通知** · 跨会话 64 条(显示最近 "),
            "抬头仍要报出总数与实际显示数:{}",
            body.lines().next().unwrap_or_default()
        );
        assert!(body.contains("`p64`"), "最新的一条必须在:{body}");
    }
}
