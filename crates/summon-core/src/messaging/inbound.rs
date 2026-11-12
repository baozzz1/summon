//! 入站去重 + 时效闸:飞书长连接是 at-least-once 投递,网络抖动(broken pipe → 1006 → 重连)那段
//! 没 ACK 的事件会被飞书**缓存、重连后重投**,于是早就过期的旧消息(如很久前发的「继续」)被当成
//! 新消息推给 agent,造成乱序/迟到。连接层(单连接串行重连)只能减少抖动,治不了「补投的旧事件」,
//! 必须在消费层加这道闸——与出站 `outbox` 的 at-least-once 正好对称(出站去重靠 outbox,入站去重靠这里)。
//!
//! 本模块只做**纯决策**:给一条入站消息的 (message_id, chat_id, create_time) + 当前时间 + 已见状态,
//! 判 `Admit` / `DropDuplicate` / `DropStale`。状态(seen 集合、per-chat 水位)与时间由 `summon` 持有并
//! 注入,保持纯与可单测;真正的状态更新 / 落盘在 IO 层。

use std::collections::{HashMap, HashSet, VecDeque};

/// 判定一条入站消息所需的最小元信息(从 `ipc::Event::Message` 抽取)。
pub struct InboundMeta<'a> {
    /// 飞书消息 id(每条消息唯一;同一消息被重投 → 同一 message_id)。去重键。
    pub message_id: &'a str,
    /// 会话 id(per-chat 水位按它分桶)。
    pub chat_id: &'a str,
    /// 飞书消息创建时间(毫秒)。`0` = 未知(sidecar 没带 create_time),此时跳过时效判定、只做去重。
    pub create_time_ms: i64,
}

/// 水位滞后门槛:`create_time` 比该 chat 已处理的最新消息早过它,才判乱序陈旧。
///
/// 门槛与 `DEFAULT_MAX_AGE_MS` 取同一个值:走到这道闸的都是去重环认不出的消息,绝大
/// 多数就是从没跑过的那条(去重环在内存里,重启只从入站 WAL 的已消费记录预热),而误杀的代价
/// 不对称——多跑一条迟到的消息用户看得见、可以忽略,静默吞掉一条用户什么都不知道。飞书在长连接
/// 抖动后按固定退避重投,实测延迟在 5 分钟量级,门槛必须留在它之上。
///
/// **两道闸同门槛带来的可达区间**:时效先判,只在 age 落进 `(max_age, 24h]` 时丢,所以水位只在
/// 另外两种情形下会命中:
/// - `age <= max_age`:与 `create < wm - 本门槛` 相加得 `wm > now`,即**本机时钟落后于服务端**。
///   这时 `now-create` 低估 age、时效闸失灵,水位两端都是服务端钟,仍然准。
/// - `age > 24h`:时效闸 fail-open 放行,而该 chat 已有比它新 10 分钟以上的已处理消息 —— 隔天
///   才补投的旧事件在活跃的 chat 里由水位拦下。
///
/// 除此之外(健康时钟、24h 以内)水位不会先于时效命中。调 `DEFAULT_MAX_AGE_MS` 时这两个区间
/// 跟着一起动。
pub const WATERMARK_STALE_LAG_MS: i64 = DEFAULT_MAX_AGE_MS;
/// 时效闸年龄上限:age 超过它(疑似本机时钟严重超前 → 所有消息都显得很旧)不再判陈旧,fail-open。
pub const STALE_AGE_CEILING_MS: i64 = 24 * 60 * 60 * 1000;

/// 入站判定结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission {
    /// 放行,投给 agent(调用方随后 record 进 seen / 抬高 chat 水位)。
    Admit,
    /// 重复投递(message_id 已见)——飞书 at-least-once 重投,丢弃。
    DropDuplicate,
    /// 迟到的旧事件(超过最大时效,或比该 chat 已处理的最新消息更旧)——抖动后补投的陈旧消息,丢弃。
    DropStale,
}

/// 判定一条入站消息是否该投给 agent。**不改状态**;调用方在 `Admit` 后自行 record。
///
/// 顺序:
/// 1. 去重:`message_id` 已见 → `DropDuplicate`。
/// 2. 时效:`create_time` 已知(>0)且比 `now` 早超过 `max_age_ms` → `DropStale`
///    (这条挡住「很久前发、抖动后才补投」的旧「继续」——它不是字面重复,只靠去重抓不到)。
/// 3. 单调(可选,`chat_watermark_ms=Some` 才启):`create_time` 已知且比该 chat 已处理的最新
///    `create_time` 旧过 `WATERMARK_STALE_LAG_MS` → `DropStale`。它只在本机时钟落后于服务端、
///    或 age 超过 24h 上限时可达(推导见 `WATERMARK_STALE_LAG_MS`)。
///    `None` 关闭此项。
pub fn admit_inbound(
    meta: &InboundMeta,
    now_ms: i64,
    max_age_ms: i64,
    seen_message_ids: &HashSet<String>,
    chat_watermark_ms: Option<i64>,
) -> Admission {
    if !meta.message_id.is_empty() && seen_message_ids.contains(meta.message_id) {
        return Admission::DropDuplicate;
    }
    if meta.create_time_ms > 0 {
        let age = now_ms - meta.create_time_ms;
        // 时效闸(now-create 混本机/服务端两种时钟,易受偏移):仅在 age 落在「明显陈旧但不离谱」区间
        // (max_age, ceiling] 才丢。负 age(本机时钟落后于服务端)与超 ceiling 的离谱 age(疑似本机时钟
        // 严重超前 → 全部新消息都显得很旧)都 **fail-open 不丢**,避免误杀全部消息把 agent 静默失能。
        if age > max_age_ms && age <= STALE_AGE_CEILING_MS {
            return Admission::DropStale;
        }
        // 水位(create 比 create,均服务端时钟,无偏移):只在比已处理最新消息旧过滞后门槛才丢。
        // 时效闸先判,故这一支只在本机时钟落后或 age 超 24h 时可达 —— 推导见 `WATERMARK_STALE_LAG_MS`。
        if let Some(wm) = chat_watermark_ms {
            if meta.create_time_ms < wm - WATERMARK_STALE_LAG_MS {
                return Admission::DropStale;
            }
        }
    }
    Admission::Admit
}

/// 默认时效:入站时距消息创建超过此值(且不超过 24h 上限)→ 判陈旧。在健康时钟、24h 以内的
/// 区间里,它是「多旧算旧」唯一先生效的判据(水位同门槛,可达区间见 `WATERMARK_STALE_LAG_MS`)。
/// 给到 10 分钟是因为误杀的代价不对称。
pub const DEFAULT_MAX_AGE_MS: i64 = 10 * 60 * 1000;
/// 默认去重窗口容量:per-agent 最近这么多条 message_id 进环形 seen 集(够覆盖一次重投风暴)。
pub const DEFAULT_SEEN_CAPACITY: usize = 1024;

/// 有状态的入站准入闸(per-agent 一个,dispatcher 单线程持有,无需锁):包住纯决策 `admit_inbound`,
/// 外加环形去重集(message_id)与 per-chat 水位(create_time)。`check` 纯判定不改状态,`record` 在确定
/// 要处理该消息后记账(进 seen、抬高水位)——故被白名单/@ 门控丢掉的消息**不**污染水位。
pub struct InboundGate {
    max_age_ms: i64,
    capacity: usize,
    use_watermark: bool,
    seen_order: VecDeque<String>,
    seen: HashSet<String>,
    chat_watermark: HashMap<String, i64>,
}

impl InboundGate {
    pub fn new(max_age_ms: i64, capacity: usize, use_watermark: bool) -> Self {
        Self {
            max_age_ms,
            capacity: capacity.max(1),
            use_watermark,
            seen_order: VecDeque::new(),
            seen: HashSet::new(),
            chat_watermark: HashMap::new(),
        }
    }

    /// 纯判定(不改状态):是否放行这条入站消息。`now_ms` 由调用方注入(本机时钟)。
    pub fn check(
        &self,
        message_id: &str,
        chat_id: &str,
        create_time_ms: i64,
        now_ms: i64,
    ) -> Admission {
        let meta = InboundMeta {
            message_id,
            chat_id,
            create_time_ms,
        };
        let wm = if self.use_watermark {
            self.chat_watermark.get(chat_id).copied()
        } else {
            None
        };
        admit_inbound(&meta, now_ms, self.max_age_ms, &self.seen, wm)
    }

    /// 确定要处理该消息后记账:message_id 入环形 seen(超容量淘汰最旧)、抬高该 chat 的水位。
    /// 在通过白名单/@ 等「要不要应答」门控之后调用,避免被丢掉的消息污染水位/去重集。
    pub fn record(&mut self, message_id: &str, chat_id: &str, create_time_ms: i64) {
        self.remember_seen(message_id);
        if create_time_ms > 0 {
            let wm = self.chat_watermark.entry(chat_id.to_string()).or_insert(0);
            if create_time_ms > *wm {
                *wm = create_time_ms;
            }
        }
    }

    /// 重启后预热去重集:把近期已消费的 message_id(由 `summon` 从入站 WAL `consumed/` 恢复)灌进 seen,
    /// 使**跨重启去重**与出站 outbox 的 at-least-once 对称(R5)。daemon 重启正是飞书把未 ACK 事件重投的
    /// 高发时刻,内存 seen 清零会让重投消息被二次执行(重复回复/表情)。超容量按插入序淘汰最旧(同
    /// `record`),故调用方按「新→旧」喂、或喂不超过容量条即可保留最近的。不动水位(水位重建走正常入站)。
    pub fn preheat_seen(&mut self, message_ids: impl IntoIterator<Item = String>) {
        for id in message_ids {
            self.remember_seen(&id);
        }
    }

    /// message_id 入环形 seen(超容量淘汰最旧)。`record`/`preheat_seen` 共用,避免两处各写一份淘汰逻辑。
    fn remember_seen(&mut self, message_id: &str) {
        if !message_id.is_empty() && self.seen.insert(message_id.to_string()) {
            self.seen_order.push_back(message_id.to_string());
            while self.seen_order.len() > self.capacity {
                if let Some(old) = self.seen_order.pop_front() {
                    self.seen.remove(&old);
                }
            }
        }
    }
}

impl Default for InboundGate {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_AGE_MS, DEFAULT_SEEN_CAPACITY, true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seen(ids: &[&str]) -> HashSet<String> {
        ids.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn fresh_unseen_message_admitted() {
        let m = InboundMeta {
            message_id: "om_1",
            chat_id: "c",
            create_time_ms: 1_000_000,
        };
        // now 比 create 晚 5s,max_age 60s → 放行。
        assert_eq!(
            admit_inbound(&m, 1_005_000, 60_000, &seen(&[]), None),
            Admission::Admit
        );
    }

    #[test]
    fn duplicate_message_id_dropped() {
        let m = InboundMeta {
            message_id: "om_dup",
            chat_id: "c",
            create_time_ms: 1_000_000,
        };
        assert_eq!(
            admit_inbound(&m, 1_001_000, 60_000, &seen(&["om_dup"]), None),
            Admission::DropDuplicate
        );
    }

    #[test]
    fn stale_old_message_dropped_by_age() {
        // 30 分钟前发的「继续」,抖动后才补投:create 比 now 早 1.8e6 ms,远超 60s → DropStale。
        let m = InboundMeta {
            message_id: "om_old",
            chat_id: "c",
            create_time_ms: 1_000_000,
        };
        assert_eq!(
            admit_inbound(&m, 1_000_000 + 1_800_000, 60_000, &seen(&[]), None),
            Admission::DropStale
        );
    }

    #[test]
    fn unknown_create_time_skips_staleness_only_dedup() {
        // create_time=0(sidecar 没带):不做时效判定,仍能去重;新消息放行。
        let m = InboundMeta {
            message_id: "om_x",
            chat_id: "c",
            create_time_ms: 0,
        };
        assert_eq!(
            admit_inbound(&m, 9_999_999_999, 60_000, &seen(&[]), None),
            Admission::Admit
        );
        let dup = InboundMeta {
            message_id: "om_x",
            chat_id: "c",
            create_time_ms: 0,
        };
        assert_eq!(
            admit_inbound(&dup, 9_999_999_999, 60_000, &seen(&["om_x"]), None),
            Admission::DropDuplicate
        );
    }

    #[test]
    fn watermark_drops_out_of_order_when_enabled() {
        // 该 chat 已处理到 create=2_000_000;来一条旧过滞后门槛的 create=1_000_000 → 启用水位时
        // DropStale。now 取 1_500_050(本机时钟慢于服务端)使 age 落在 max_age 内,单测水位这一条。
        let older = InboundMeta {
            message_id: "om_b",
            chat_id: "c",
            create_time_ms: 1_000_000,
        };
        assert_eq!(
            admit_inbound(&older, 1_500_050, 600_000, &seen(&[]), Some(2_000_000)),
            Admission::DropStale
        );
        // 水位关闭(None)时同一条放行(只靠时效 + 去重)。
        assert_eq!(
            admit_inbound(&older, 1_500_050, 600_000, &seen(&[]), None),
            Admission::Admit
        );
    }

    #[test]
    fn future_clock_skew_not_dropped() {
        // create 在 now 之后(时钟偏移):now-create<0,不该判 stale。
        let m = InboundMeta {
            message_id: "om_f",
            chat_id: "c",
            create_time_ms: 2_000_000,
        };
        assert_eq!(
            admit_inbound(&m, 1_999_000, 60_000, &seen(&[]), None),
            Admission::Admit
        );
    }

    #[test]
    fn gate_dedups_after_record() {
        let mut g = InboundGate::new(60_000, 8, true);
        // 首次:check 放行 → record。
        assert_eq!(g.check("om_1", "c", 1000, 1500), Admission::Admit);
        g.record("om_1", "c", 1000);
        // 重投同一 message_id:check 判重复。
        assert_eq!(g.check("om_1", "c", 1000, 1600), Admission::DropDuplicate);
    }

    #[test]
    fn gate_watermark_drops_out_of_order_after_newer_processed() {
        let mut g = InboundGate::new(10 * 60 * 1000, 8, true);
        // 本机时钟比服务端慢 50 万 ms:下面每条的 age 都算得偏小,时效闸一条都拦不住,
        // 水位(两边都是服务端时钟)是此时唯一还准的判据。
        let now = 1_500_000;
        // 先处理一条较新的(create=2_000_000)。
        assert_eq!(g.check("om_new", "c", 2_000_000, now), Admission::Admit);
        g.record("om_new", "c", 2_000_000);
        // 补投的比水位旧过滞后门槛(早 100 万 ms)→ 水位判 DropStale。
        assert_eq!(g.check("om_old", "c", 1_000_000, now), Admission::DropStale);
        // 门槛内的乱序(比水位早 30 万 ms)→ 放行:它从没被处理过,用户可能还在等。
        assert_eq!(g.check("om_slight", "c", 1_700_000, now), Admission::Admit);
        // 另一个 chat 的旧消息不受本 chat 水位影响。
        assert_eq!(
            g.check("om_old2", "c2", 1_900_000, 2_050_000),
            Admission::Admit
        );
    }

    #[test]
    fn watermark_catches_day_old_redelivery_that_the_age_ceiling_lets_through() {
        // 时钟正常。隔天才补投的旧事件 age 超过 24h 上限,时效闸 fail-open 放行;该 chat 一分钟前
        // 还处理过消息,水位把它拦下。
        let now = 1_789_541_730_000_i64;
        let day_old = InboundMeta {
            message_id: "om_day_old",
            chat_id: "c",
            create_time_ms: now - 25 * 60 * 60 * 1000,
        };
        assert_eq!(
            admit_inbound(
                &day_old,
                now,
                DEFAULT_MAX_AGE_MS,
                &seen(&[]),
                Some(now - 60_000)
            ),
            Admission::DropStale
        );
        assert_eq!(
            admit_inbound(&day_old, now, DEFAULT_MAX_AGE_MS, &seen(&[]), None),
            Admission::Admit
        );
    }

    #[test]
    fn late_redelivered_unseen_message_is_admitted() {
        // 飞书长连接抖动后按固定退避重投,实测延迟 5 分钟量级:用户发了 A,没反应,隔几十秒到
        // 几分钟重发 B;B 先到并被处理,A 才姗姗来迟。A 不在 seen(从没跑过),用户还在等它。
        let mut g = InboundGate::default();
        let sent_a = 1_789_541_730_000_i64;
        for lag_ms in [54_000_i64, 150_000] {
            let sent_b = sent_a + lag_ms;
            let delivered_a = sent_a + 328_000;
            let mut g_case = InboundGate::default();
            assert_eq!(
                g_case.check("om_b", "c", sent_b, sent_b + 3_000),
                Admission::Admit
            );
            g_case.record("om_b", "c", sent_b);
            assert_eq!(
                g_case.check("om_a", "c", sent_a, delivered_a),
                Admission::Admit,
                "间隔 {lag_ms}ms 重发时,迟到的第一条必须照跑"
            );
        }
        // 真正该丢的仍然丢:同一条被重投(message_id 已见)。
        g.record("om_a", "c", sent_a);
        assert_eq!(
            g.check("om_a", "c", sent_a, sent_a + 328_000),
            Admission::DropDuplicate
        );
    }

    #[test]
    fn absurd_age_fails_open_not_stale() {
        // 本机时钟严重落后 → age 超过 24h ceiling → 不判 stale(fail-open,避免把全部新消息误杀)。
        let m = InboundMeta {
            message_id: "om_z",
            chat_id: "c",
            create_time_ms: 1_000_000,
        };
        let huge_now = 1_000_000 + STALE_AGE_CEILING_MS + 1;
        assert_eq!(
            admit_inbound(&m, huge_now, 60_000, &seen(&[]), None),
            Admission::Admit
        );
    }

    #[test]
    fn gate_seen_ring_evicts_oldest() {
        let mut g = InboundGate::new(60_000, 2, false); // 容量 2,关水位只看去重
        g.record("a", "c", 0);
        g.record("b", "c", 0);
        g.record("c", "c", 0); // 淘汰 a
        assert_eq!(
            g.check("a", "c", 0, 0),
            Admission::Admit,
            "a 已被淘汰,不再判重复"
        );
        assert_eq!(g.check("b", "c", 0, 0), Admission::DropDuplicate);
        assert_eq!(g.check("c", "c", 0, 0), Admission::DropDuplicate);
    }

    #[test]
    fn preheat_seen_dedups_across_restart() {
        // R5:重启后预热的已消费 message_id 应跨重启去重——即便时效/水位本会放行,也判 DropDuplicate。
        let mut g = InboundGate::new(10 * 60 * 1000, 1024, true);
        g.preheat_seen(["om_consumed1".to_string(), "om_consumed2".to_string()]);
        let now = 1_000_000;
        // 预热过的 id(create_time 在时效窗内、水位空 → 本会 Admit)被去重拦下,不二次执行。
        assert_eq!(
            g.check("om_consumed1", "oc1", now, now),
            Admission::DropDuplicate
        );
        assert_eq!(
            g.check("om_consumed2", "oc1", now, now),
            Admission::DropDuplicate
        );
        // 没预热的新 id 仍放行。
        assert_eq!(g.check("om_fresh", "oc1", now, now), Admission::Admit);
        // 预热超容量按插入序淘汰最旧(同 record 语义)。
        let mut g2 = InboundGate::new(60_000, 2, false);
        g2.preheat_seen(["x".to_string(), "y".to_string(), "z".to_string()]); // 淘汰 x
        assert_eq!(g2.check("x", "c", 0, 0), Admission::Admit, "超容量淘汰最旧");
        assert_eq!(g2.check("z", "c", 0, 0), Admission::DropDuplicate);
    }

    /// **真实入站 WAL(脱敏)回放回归**:数据来源是 owner 机上真实落盘的入站消息
    /// (`~/.summon/run/inbox/consumed/`),open_id/chat_id/正文已脱敏成稳定占位、结构原样保留 ——
    /// 锁住「真实世界的消息形态」被入站闸 + mention 管线正确处理(合成 fixture 容易漏的真实组合:
    /// 私聊裸 hello、群里回复某条消息 @bot 派活带 parent_id、at-least-once 重投、抖动补投的旧事件)。
    #[test]
    fn real_wal_replay_dedup_and_mentions() {
        use crate::ipc::{mentions_contain, render_mentions, strip_mention_by_open_id, Mention};

        // —— 入站去重 + per-chat 水位(真实序列:p2p hello → 群派活 → hello 重投 → 旧事件补投)——
        let mut gate = InboundGate::new(DEFAULT_MAX_AGE_MS, DEFAULT_SEEN_CAPACITY, true);
        let now = 1_781_010_600_000_i64; // 略晚于下面两条 create_time,age 在阈值内
        let p2p = ("om_p2p_hello", "oc_p2p1", 1_781_010_576_000_i64);
        let grp = ("om_grp_review", "oc_group1", 1_781_010_576_557_i64);
        // 首次都放行 + 记账。
        assert_eq!(gate.check(p2p.0, p2p.1, p2p.2, now), Admission::Admit);
        gate.record(p2p.0, p2p.1, p2p.2);
        assert_eq!(gate.check(grp.0, grp.1, grp.2, now), Admission::Admit);
        gate.record(grp.0, grp.1, grp.2);
        // 飞书 at-least-once 重投(同 message_id)→ 去重丢。
        assert_eq!(
            gate.check(p2p.0, p2p.1, p2p.2, now),
            Admission::DropDuplicate
        );
        // 同 chat 里旧过门槛的补投 → 陈旧丢(本机时钟正常,这里是时效闸命中)。
        assert_eq!(
            gate.check("om_stale", grp.1, grp.2 - WATERMARK_STALE_LAG_MS - 1, now),
            Admission::DropStale
        );

        // —— mention 渲染/剥离(真实群 @bot 派活形态:`@_user_1 review这个MR`)——
        let bot = "ou_bot1";
        let mentions = vec![Mention {
            open_id: bot.into(),
            key: "@_user_1".into(), // 飞书正文里的 mention 占位 token
            name: "Bot".into(),
            ..Default::default()
        }];
        let text = "@_user_1 review这个MR";
        assert!(mentions_contain(&mentions, bot), "群里 @ 了本 bot");
        // 抹掉唤起 bot 的 @ → 只剩纯正文(供 `!`/命令/正文解析,不被前缀 @ 挡)。
        assert_eq!(
            strip_mention_by_open_id(text, &mentions, bot),
            "review这个MR"
        );
        // 渲染路径:@占位 → @名字(不带 open_id,回 @ 由出站层解析)。
        assert_eq!(render_mentions(text, &mentions), "@Bot review这个MR");
        // 这条不是会话命令(抹 @ 后 ≠ /session)。
        assert!(crate::ipc::session_command(text, &mentions).is_none());
    }
}
