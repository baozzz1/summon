//! Frontend:把 turn-flow(一轮及其生命周期)的**出站副作用**从 `FeishuClient` 解耦出来的薄抽象。
//!
//! 范围:只收口「一轮里发给用户的东西」——流式卡片(打字机)、错误/兜底回复卡片、OnIt/DONE 表情。
//! setup/admin（bind/onboard/doctor、名字/群名解析）仍直调 `FeishuClient`,不在本切片内。
//! 核心收益:turn 流程变 frontend 无关、可 mock,行为对真实飞书路径**逐字节不变**
//!(`FeishuFrontend` 只转发到今天那几个 helper)。
//!
//! 飞书卡片 JSON / message_id **不进入** core 看到的 `Frontend` 调用签名——core 只持有不透明的
//! `StreamId`,message_id 由 adapter 内部维护。

pub mod client;
mod endpoints;
mod feishu;
pub(crate) mod outbound;
mod result_only;

pub(crate) use feishu::finalize_orphan_cot_messages;
pub use feishu::{CotCarrier, DetachedCot, FeishuFrontend};
pub(crate) use result_only::{ResultOnlyFrontend, ResultOnlyOutcome};

#[cfg(debug_assertions)]
// `OutboundRecord` 类型供 `#[cfg(test)]` 回放断言与 `dev record promote`(读 outbound.ndjson 脱敏)用;
// 生产录制只经 `RecordingFrontend`+`RecordSink` 间接产出它,故非 test 构建里类型名「未直接引用」。
#[allow(unused_imports)]
pub use recording::{OutboundRecord, RecordSink, RecordingFrontend};

/// 出站录制装饰器(RR2.3)。**整模块 `#[cfg(debug_assertions)]` gate** —— release 不编译,生产二进制
/// 零录制代码。包任意 `Frontend`,把每个出站调用 + 入参录成 `OutboundRecord` ndjson,再委派 inner;
/// 回放时挂 assert sink 把同序列对 golden 逐条断言。生产侧(daemon `record_frontend`)在 RR4 接上
/// (录制开关开时用 `Ndjson` sink 包 `FeishuFrontend`);`Buffer` sink 供 `#[cfg(test)]` 回放断言。
#[cfg(debug_assertions)]
mod recording {
    use super::{ChatRef, Frontend, MsgRef, StreamId, StreamPatchOutcome};
    use serde::{Deserialize, Serialize};
    use std::sync::Mutex;

    /// 一次出站调用的 cassette 记录:方法名 + 关键入参。`StreamId` 录其内部 u64(回放比对用,不进 core)。
    /// react 录返回的 reaction id(回放挂载时复现)。按 `Frontend` 方法一一对应。
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    #[serde(tag = "op")]
    pub enum OutboundRecord {
        SendMessage {
            chat_id: String,
            body: String,
        },
        StreamBegin {
            chat_id: String,
            content: String,
            id: u64,
        },
        StreamPatch {
            id: u64,
            content: String,
        },
        /// 交织 live 卡的首张:内容是预渲染卡片 JSON(与 StreamBegin 区分,回放比对稳定)。
        StreamBeginCard {
            chat_id: String,
            card_json: String,
            id: u64,
        },
        /// 交织 live 卡的原地刷:预渲染卡片 JSON(节流后真发的那次)。
        StreamPatchCard {
            id: u64,
            card_json: String,
        },
        React {
            message_id: String,
            key: String,
            reaction_id: String,
        },
        Unreact {
            message_id: String,
            reaction_id: String,
        },
        /// 任务5:发一张独立卡片(总结卡 / 交互菜单卡等)。`card_json` 是完整卡片 JSON——debug 回放
        /// 开关开时也要存下来(owner 反馈:总结卡得能回放/查证),不再只委派 inner 不记。
        SendCard {
            chat_id: String,
            card_json: String,
        },
        /// 任务5:turn 收尾把流式卡原地定格成任意卡片 JSON(折叠卡 / 暂停卡)。
        StreamFinishCard {
            id: u64,
            card_json: String,
        },
    }

    impl OutboundRecord {
        /// 带 chat_id 的出站(SendMessage/StreamBegin/SendCard):用于 timeline correlation 定位归属 chat。
        /// patch/react/unreact/stream_finish_card 无 chat_id(挂在已发卡片/消息上),返回 None。
        pub(super) fn chat_id(&self) -> Option<&str> {
            match self {
                OutboundRecord::SendMessage { chat_id, .. }
                | OutboundRecord::StreamBegin { chat_id, .. }
                | OutboundRecord::StreamBeginCard { chat_id, .. }
                | OutboundRecord::SendCard { chat_id, .. } => Some(chat_id),
                _ => None,
            }
        }
    }

    /// 录制落点:`Ndjson` append 到 `~/.summon/raw/<scenario>/outbound.ndjson`(生产录制);`Buffer`
    /// 收进内存 Vec(测试/回放 assert 模式,调用方事后取出对 golden 比对)。两态共用同一录制路径,
    /// 保证「录什么 = 断言什么」。
    pub enum RecordSink {
        Ndjson {
            scenario: String,
        },
        // Buffer 仅 `#[cfg(test)]` 回放/单测构造(assert 模式);生产录制走 Ndjson。
        #[allow(dead_code)]
        Buffer(Mutex<Vec<OutboundRecord>>),
    }

    impl RecordSink {
        /// 内存缓冲 sink(测试/回放 assert):事后 `drain` 取出录到的序列。仅 test 用。
        #[allow(dead_code)]
        pub fn buffer() -> Self {
            RecordSink::Buffer(Mutex::new(Vec::new()))
        }

        /// ndjson sink(生产录制):每条出站 append 到 `~/.summon/raw/<scenario>/outbound.ndjson`。
        pub fn ndjson(scenario: impl Into<String>) -> Self {
            RecordSink::Ndjson {
                scenario: scenario.into(),
            }
        }

        /// 录一条:Ndjson 经 RecordStore append(失败只 log 忽略,绝不影响出站);Buffer 入内存。
        fn record(&self, rec: &OutboundRecord) {
            match self {
                RecordSink::Ndjson { scenario } => {
                    let store = crate::runtime::record_store::RecordStore::new(
                        crate::runtime::paths::summon_home().join("raw"),
                    );
                    if let Err(e) = store.append_ndjson(scenario, "outbound.ndjson", rec) {
                        crate::observe::log::log(&summon_i18n::format!("r9dd2c17e23"));
                    }
                    // 同步记一条出站 timeline atom(带 chat_id 的出站才入流;patch/react 无 chat_id
                    // 不单独成 flow step——回放按 outbound.ndjson 全序读,timeline 只定 e2e 穿插点)。
                    if let Some(chat_id) = rec.chat_id() {
                        crate::runtime::daemon::record_outbound_timeline(chat_id);
                    }
                }
                RecordSink::Buffer(buf) => {
                    if let Ok(mut b) = buf.lock() {
                        b.push(rec.clone());
                    }
                }
            }
        }

        /// 取出 Buffer sink 录到的全部记录(回放 assert:与 golden 比对)。非 Buffer 返回空。仅 test 用。
        #[allow(dead_code)]
        pub fn drain(&self) -> Vec<OutboundRecord> {
            match self {
                RecordSink::Buffer(buf) => buf
                    .lock()
                    .map(|mut b| std::mem::take(&mut *b))
                    .unwrap_or_default(),
                RecordSink::Ndjson { .. } => Vec::new(),
            }
        }
    }

    /// 出站录制装饰器:包一个 `Frontend`,每个调用先委派 inner(拿真 `StreamId`/reaction id),再把
    /// 调用 + 入参录进 sink。对调用方完全透明(同 `Frontend` 签名),行为对 inner **逐字节不变**。
    pub struct RecordingFrontend<F: Frontend> {
        inner: F,
        sink: RecordSink,
    }

    impl<F: Frontend> RecordingFrontend<F> {
        pub fn new(inner: F, sink: RecordSink) -> Self {
            Self { inner, sink }
        }

        /// 取出录到的出站序列(Buffer sink;回放 assert 用)。仅 test 用。
        #[allow(dead_code)]
        pub fn recorded(&self) -> Vec<OutboundRecord> {
            self.sink.drain()
        }
    }

    impl<F: Frontend> Frontend for RecordingFrontend<F> {
        fn result_only(&self) -> bool {
            self.inner.result_only()
        }

        fn reset_result_buffer(&self) {
            self.inner.reset_result_buffer();
        }

        fn stage_result_body(&self, body: &str) {
            self.inner.stage_result_body(body);
        }

        fn send_message(&self, chat: &ChatRef, body: &str) -> anyhow::Result<()> {
            let r = self.inner.send_message(chat, body);
            if r.is_ok() {
                self.sink.record(&OutboundRecord::SendMessage {
                    chat_id: chat.chat_id.to_string(),
                    body: body.to_string(),
                });
            }
            r
        }

        // 纯规划方法不产生出站记录，但必须委派 inner；否则装上录制器后会退回 raw 字节分段，
        // 丢掉飞书 adapter 对转义与 mention 展开的真实预算。
        fn split_final_reply(&self, chat: &ChatRef, body: &str) -> Vec<String> {
            self.inner.split_final_reply(chat, body)
        }

        fn stream_begin(
            &self,
            chat: &ChatRef,
            content: &str,
            with_interrupt: bool,
        ) -> anyhow::Result<StreamId> {
            let id = self.inner.stream_begin(chat, content, with_interrupt)?;
            // 录制只记原始 content(按钮是渲染期细节,不进 golden 序列;回放比对内容稳定)。
            self.sink.record(&OutboundRecord::StreamBegin {
                chat_id: chat.chat_id.to_string(),
                content: content.to_string(),
                id: id.0,
            });
            Ok(id)
        }

        fn stream_patch(&self, id: StreamId, content: &str) -> anyhow::Result<()> {
            let r = self.inner.stream_patch(id, content);
            if r.is_ok() {
                self.sink.record(&OutboundRecord::StreamPatch {
                    id: id.0,
                    content: content.to_string(),
                });
            }
            r
        }

        // 交织 live 卡:显式委派 inner(拿真节流/StreamId),不吃默认实现(默认会折回文本版丢掉
        // 卡片渲染),成功后录预渲染卡片 JSON。
        fn stream_begin_card(&self, chat: &ChatRef, card_json: &str) -> anyhow::Result<StreamId> {
            let id = self.inner.stream_begin_card(chat, card_json)?;
            self.sink.record(&OutboundRecord::StreamBeginCard {
                chat_id: chat.chat_id.to_string(),
                card_json: card_json.to_string(),
                id: id.0,
            });
            Ok(id)
        }

        fn stream_patch_card(&self, id: StreamId, card_json: &str) -> anyhow::Result<()> {
            let r = self.inner.stream_patch_card(id, card_json);
            if r.is_ok() {
                self.sink.record(&OutboundRecord::StreamPatchCard {
                    id: id.0,
                    card_json: card_json.to_string(),
                });
            }
            r
        }

        fn stream_patch_card_observed(
            &self,
            id: StreamId,
            card_json: &str,
        ) -> anyhow::Result<StreamPatchOutcome> {
            let r = self.inner.stream_patch_card_observed(id, card_json);
            if r.is_ok() {
                self.sink.record(&OutboundRecord::StreamPatchCard {
                    id: id.0,
                    card_json: card_json.to_string(),
                });
            }
            r
        }

        // 结构化 live 卡:显式委派 inner(它才知道这张卡走的是 CoT 还是整卡 PATCH)。
        // 录制记空 card_json —— CoT 载体的内容由 cot_begin/cot_update 单独录制。
        fn live_begin(
            &self,
            chat: &ChatRef,
            view: &summon_core::cot::CotView,
        ) -> anyhow::Result<StreamId> {
            let id = self.inner.live_begin(chat, view)?;
            self.sink.record(&OutboundRecord::StreamBeginCard {
                chat_id: chat.chat_id.to_string(),
                card_json: String::new(),
                id: id.0,
            });
            Ok(id)
        }

        fn live_update(
            &self,
            id: StreamId,
            view: &summon_core::cot::CotView,
        ) -> anyhow::Result<StreamPatchOutcome> {
            let r = self.inner.live_update(id, view);
            if r.is_ok() {
                self.sink.record(&OutboundRecord::StreamPatchCard {
                    id: id.0,
                    card_json: String::new(),
                });
            }
            r
        }

        // 接管开卡必须委派 inner:默认实现会折回普通开卡,把「续同一条 CoT」这件事悄悄丢掉。
        fn stream_begin_reattached(
            &self,
            chat: &ChatRef,
            view: &summon_core::cot::CotView,
            cot: &summon_core::TurnAnchorCot,
        ) -> anyhow::Result<StreamId> {
            let id = self.inner.stream_begin_reattached(chat, view, cot)?;
            self.sink.record(&OutboundRecord::StreamBeginCard {
                chat_id: chat.chat_id.to_string(),
                card_json: String::new(),
                id: id.0,
            });
            Ok(id)
        }

        fn stream_cot_handle(&self, id: StreamId) -> Option<(String, String)> {
            self.inner.stream_cot_handle(id)
        }

        fn stream_is_process_only(&self, id: StreamId) -> bool {
            self.inner.stream_is_process_only(id)
        }

        fn send_process_supplement(
            &self,
            chat: &ChatRef,
            card: &str,
            uuid: &str,
        ) -> anyhow::Result<()> {
            self.inner.send_process_supplement(chat, card, uuid)
        }

        fn stream_confirmed_replies(
            &self,
            id: StreamId,
        ) -> Option<Vec<summon_core::cot::ReplyDelivery>> {
            self.inner.stream_confirmed_replies(id)
        }

        fn stream_append_failed(&self, id: StreamId) -> bool {
            self.inner.stream_append_failed(id)
        }

        fn settle_last_sent_as_reply(&self) {
            self.inner.settle_last_sent_as_reply()
        }

        fn send_message_with_feed(
            &self,
            chat: &ChatRef,
            body: &str,
            feed: &str,
        ) -> anyhow::Result<()> {
            let r = self.inner.send_message_with_feed(chat, body, feed);
            if r.is_ok() {
                self.sink.record(&OutboundRecord::SendMessage {
                    chat_id: chat.chat_id.to_string(),
                    body: body.to_string(),
                });
            }
            r
        }

        fn stream_flush(&self, id: StreamId) -> anyhow::Result<()> {
            self.inner.stream_flush(id)
        }

        fn stream_detach_cot(&self, id: StreamId) -> Option<crate::frontend::DetachedCot> {
            self.inner.stream_detach_cot(id)
        }

        fn stream_steer_boundary(&self, id: StreamId, note: &str) -> anyhow::Result<()> {
            self.inner.stream_steer_boundary(id, note)
        }

        fn stream_restart_boundary(&self, id: StreamId, note: &str) -> anyhow::Result<()> {
            self.inner.stream_restart_boundary(id, note)
        }

        fn stream_fail(&self, id: StreamId, content: &str) -> anyhow::Result<()> {
            self.inner.stream_fail(id, content)
        }

        fn stream_timeout(&self, id: StreamId, content: &str) -> anyhow::Result<()> {
            self.inner.stream_timeout(id, content)
        }

        // 必须显式委派 inner.stream_finish(不能吃默认实现):默认会折回 self.stream_patch,
        // 把 inner(飞书前端)的「绕节流 + 重试」语义丢掉。录制上不新增类型,仍记 StreamPatch。
        fn stream_finish(&self, id: StreamId, content: &str) -> anyhow::Result<()> {
            let r = self.inner.stream_finish(id, content);
            if r.is_ok() {
                self.sink.record(&OutboundRecord::StreamPatch {
                    id: id.0,
                    content: content.to_string(),
                });
            }
            r
        }

        // 非权威终态冻结必须显式委派，不能吃 trait 默认后折回 terminal finish。
        // 录制 wire 形态仍是同一类必达 PATCH，不新增 golden 类型。
        fn stream_freeze(&self, id: StreamId, content: &str) -> anyhow::Result<()> {
            let r = self.inner.stream_freeze(id, content);
            if r.is_ok() {
                self.sink.record(&OutboundRecord::StreamPatch {
                    id: id.0,
                    content: content.to_string(),
                });
            }
            r
        }

        fn react(&self, msg: &MsgRef, key: &str) -> anyhow::Result<String> {
            let rid = self.inner.react(msg, key)?;
            self.sink.record(&OutboundRecord::React {
                message_id: msg.message_id.to_string(),
                key: key.to_string(),
                reaction_id: rid.clone(),
            });
            Ok(rid)
        }

        fn unreact(&self, msg: &MsgRef, reaction_id: &str) -> anyhow::Result<()> {
            let r = self.inner.unreact(msg, reaction_id);
            if r.is_ok() {
                self.sink.record(&OutboundRecord::Unreact {
                    message_id: msg.message_id.to_string(),
                    reaction_id: reaction_id.to_string(),
                });
            }
            r
        }

        // 任务5:发独立卡片(总结卡/交互卡)——投递成功后录一条 SendCard(debug 回放开关开时存卡片
        // JSON,供查证/回放;owner 反馈)。失败不录(与其他出站记录一致:只录真发出的)。
        fn send_card(&self, chat: &ChatRef, card_json: &str) -> anyhow::Result<()> {
            let r = self.inner.send_card(chat, card_json);
            if r.is_ok() {
                self.sink.record(&OutboundRecord::SendCard {
                    chat_id: chat.chat_id.to_string(),
                    card_json: card_json.to_string(),
                });
            }
            r
        }

        // 任务5:过程卡定格(折叠/暂停卡)——委派 inner 拿绕节流/重试语义,成功后录 StreamFinishCard。
        fn stream_finish_card(&self, id: StreamId, card_json: &str) -> anyhow::Result<()> {
            let r = self.inner.stream_finish_card(id, card_json);
            if r.is_ok() {
                self.sink.record(&OutboundRecord::StreamFinishCard {
                    id: id.0,
                    card_json: card_json.to_string(),
                });
            }
            r
        }

        fn stream_end_session(&self, id: StreamId, partial: &str) -> anyhow::Result<()> {
            let result = self.inner.stream_end_session(id, partial);
            if result.is_ok() {
                self.sink.record(&OutboundRecord::StreamFinishCard {
                    id: id.0,
                    card_json: summon_core::feishu::card_markdown_v2(summon_i18n::text!(
                        "r3c30e0b840"
                    )),
                });
            }
            result
        }

        fn stream_freeze_card(&self, id: StreamId, card_json: &str) -> anyhow::Result<()> {
            let r = self.inner.stream_freeze_card(id, card_json);
            if r.is_ok() {
                self.sink.record(&OutboundRecord::StreamFinishCard {
                    id: id.0,
                    card_json: card_json.to_string(),
                });
            }
            r
        }

        // 重启恢复:委派 inner 真回填,不录制(不产生出站副作用)。

        // 卡片失败贴表情:委派 inner 真贴,成功后录 React(复用现有 React 记录类型,message_id 不可得
        // 时留空——录制是 best-effort,不影响行为)。
        fn react_stream_card(&self, id: StreamId, key: &str) -> anyhow::Result<String> {
            let r = self.inner.react_stream_card(id, key);
            if let Ok(ref rid) = r {
                self.sink.record(&OutboundRecord::React {
                    message_id: String::new(),
                    key: key.to_string(),
                    reaction_id: rid.clone(),
                });
            }
            r
        }
    }
}

/// 一个 chat 的最小定位信息(发新卡片用)。承载归档落盘需要的 agent 目录/名字,避免把整个
/// `Agent`/`FeishuClient` 漏进 `Frontend` 签名。
pub struct ChatRef<'a> {
    pub chat_id: &'a str,
    pub chat_type: &'a str,
    pub agent_dir: &'a std::path::Path,
    pub agent_name: &'a str,
    /// daemon epoch 冻结的飞书 storage scope；durable outbox producer 必须原样保存。
    pub delivery_scope: &'a str,
    /// 本次出站要挂进哪条 thread —— 值是**触发这一轮的那条入站消息 id**。
    ///
    /// 飞书没有「往 thread 里发」的收件人类型(`receive_id_type` 只认 chat/user),进 thread
    /// 的唯一途径是 `messages/:id/reply` + `reply_in_thread`。所以这里存的是回复锚点,不是
    /// thread_id 本身。
    ///
    /// `None` = 照常发进群主干。**只有入站消息本身就在 thread 里时才置 Some**:对不在 thread
    /// 的消息带 `reply_in_thread` 会**新建**一条 thread,把普通对话切成一地话题串。
    pub thread_anchor: Option<&'a str>,
}

/// 一条已发消息的最小定位信息(贴/撤表情用)。
pub struct MsgRef<'a> {
    pub message_id: &'a str,
}

/// core 分配的不透明流式卡片 id;adapter 内部把它映射成飞书 message_id(message_id 永不进 core)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamId(pub u64);

/// 一次 live-card PATCH 在 adapter 内的真实去向。`Ok(())` 无法区分已发与本地节流跳过，
/// 观测层需要这个差异才能判断「重复内容是否真的到过飞书」。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamPatchOutcome {
    Applied,
    Throttled,
    MissingStream,
}

impl StreamPatchOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Applied => "applied",
            Self::Throttled => "throttled",
            Self::MissingStream => "missing_stream",
        }
    }
}

/// turn-flow 的出站抽象。所有方法对应今天 daemon 直调 `FeishuClient` 的一处 turn-flow 副作用。
/// 一张**已定格**的卡:message_id + 定格那一刻的成品 JSON。
///
/// 存成品而不是渲染材料,是为了后续刷新时能直接在它上面挂区块——不必把整轮 segments/progress
/// 都留在内存里,也不必担心重渲逻辑与当初定格时漂移。
/// 这张已定格的卡是什么形态 —— 决定刷新时怎么把新的子代理状态放进去。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettledCardKind {
    /// 一轮对话的卡(live 定格 / 终态回复)。它的骨架里**没有**子代理区域,
    /// 靠 `card_with_subagent_tail` 在成品 JSON 上挂一个。
    Turn,
    /// `/session` 会话面板卡。它**原生就有**子代理区域(`card_session_panel` 的入参),
    /// 刷新要**整卡重渲**;若也去挂区块,会和原生区域并存、同一份状态显示两遍。
    SessionPanel,
    /// 原生 CoT 下新发的终态回复消息。子代理状态在 CoT 跟随里呈现,不往回复上挂;登记它是为了
    /// 轮末之后还能在回复末尾原地追加说明(如会话已归档)。
    Reply,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettledCard {
    pub message_id: String,
    pub card_json: String,
    pub kind: SettledCardKind,
}

/// 跨轮共享的「最近定格卡」槽。frontend 每轮新建、存不住状态,所以由 chat 线程持有。
pub type SharedSettledCard = std::sync::Arc<std::sync::Mutex<Option<SettledCard>>>;

/// 本轮终态回复的原文(未摘末尾标签)。只有要读回复末尾标签的轮才挂:`/handoff` 写交接那一轮
/// 的交接文件路径就写在这里。
pub type SharedFinalReply = std::sync::Arc<std::sync::Mutex<Option<String>>>;

/// 不支持原生 CoT 的前端开出来的那张 live 卡:一张空壳,不带任何过程内容。
///
/// 有名字是为了让「它就该是空的」可被断言(见 `the_default_live_card_stays_an_empty_shell`),
/// 而不是散在两处的字面量 —— 那样谁把它换成真渲染都没人拦。
pub const EMPTY_LIVE_CARD: &str = "{}";

pub trait Frontend {
    /// 该前端是否缓冲过程、只在轮末交付结果。默认前端实时投递。
    fn result_only(&self) -> bool {
        false
    }

    /// 错误终态将取代此前缓存的部分回复；实时前端无需处理。
    fn reset_result_buffer(&self) {}

    /// 把权威终态正文交给只展示结果的前端，供终态卡失败回退与失败说明合并。
    fn stage_result_body(&self, _body: &str) {}

    /// 整条消息(错误回执 / 流式未发出时的兜底回复)→ 发一张无 header 的 markdown 卡片。
    fn send_message(&self, chat: &ChatRef, body: &str) -> anyhow::Result<()>;

    /// 独立过程补充使用冻结载荷与幂等键，不登记为最终回复。
    fn send_process_supplement(
        &self,
        chat: &ChatRef,
        card: &str,
        _uuid: &str,
    ) -> anyhow::Result<()> {
        self.send_card(chat, card)
    }

    /// 把终态回复切成当前 adapter 可安全投递的原始文本段。默认前端没有额外渲染膨胀，沿用
    /// `CARD_TEXT_SAFE_LIMIT` 原文分段；飞书前端会按转义、mention 展开后的真实 markdown/card
    /// JSON 预算覆盖。返回段必须满足 `concat() == body`，空文本仍保留一个空段。
    fn split_final_reply(&self, _chat: &ChatRef, body: &str) -> Vec<String> {
        summon_core::feishu::split_for_card(body, summon_core::feishu::CARD_TEXT_SAFE_LIMIT)
    }

    /// 把回复正文里的内联图片(`![alt](本地路径)`)上传到前端,返回 `path → 前端图片 id`。
    ///
    /// **agent 对此完全无感知**——它只是按 markdown 正常写图片引用,上传与嵌卡都由 summon 做。
    /// 这也是本仓 2026-06 删掉 `[[im]]` marker 子系统之后重新支持图文混排的方式:用标准语法,
    /// 不再发明私有标记、不必在 PRIMER 里维护一份协议。
    ///
    /// 默认**返回空 map**(无图片能力的前端与测试桩):调用方据此把引用原样留成文本,行为与从前一致。
    /// 上传失败的项同样不进 map、退化成文本——图没附上可惜,但绝不能让这条回复发不出去。
    fn resolve_inline_images(
        &self,
        _chat: &ChatRef,
        _body: &str,
    ) -> std::collections::BTreeMap<String, String> {
        Default::default()
    }

    /// 流式回复:首次建卡返回 `StreamId`,后续 `stream_patch` 原地刷同一张卡片(打字机)。
    /// `with_interrupt`(任务4):正在输入的卡内嵌「⏸ 打断」按钮;终态/回退建卡传 false。
    fn stream_begin(
        &self,
        chat: &ChatRef,
        content: &str,
        with_interrupt: bool,
    ) -> anyhow::Result<StreamId>;
    fn stream_patch(&self, id: StreamId, content: &str) -> anyhow::Result<()>;

    /// 交织 live 卡的「首张/原地刷」:与 `stream_begin`/`stream_patch` 同生命周期(同 StreamId、同
    /// 节流器、同 schema),但内容是**预渲染好的卡片 JSON**(交织 thinking/reply 折叠+直显,非纯
    /// markdown)。on_event 按到达顺序累段后整卡刷;节流沿用 `stream_patch` 的窗口。默认委派回
    /// 文本版(content=card_json),让无映射的测试桩仍记录到一次 begin/patch。
    fn stream_begin_card(&self, chat: &ChatRef, card_json: &str) -> anyhow::Result<StreamId> {
        self.stream_begin(chat, card_json, false)
    }
    fn stream_patch_card(&self, id: StreamId, card_json: &str) -> anyhow::Result<()> {
        self.stream_patch(id, card_json)
    }

    /// 与 `stream_patch_card` 行为相同，但返回 adapter 的真实投递结果。默认前端没有节流/映射，
    /// 因而成功即 Applied；飞书前端覆盖为精确结果。
    fn stream_patch_card_observed(
        &self,
        id: StreamId,
        card_json: &str,
    ) -> anyhow::Result<StreamPatchOutcome> {
        self.stream_patch_card(id, card_json)?;
        Ok(StreamPatchOutcome::Applied)
    }

    /// live 卡的**结构化**开卡/刷卡。`view` 是渲染材料(段 + 进行中形状)。
    ///
    /// 飞书前端覆盖它走原生 CoT。默认实现开一张**空壳卡**并丢掉 `view` —— 这不是没写完,
    /// 是 owner 2026-09-03 的决策:原生 CoT 是唯一的过程载体,没有第二条渲染路径。所以这里
    /// 不许把 `view` 渲染成整卡当兜底,那正是被否掉的那个选项。
    ///
    /// 走到这条默认实现的只有三种前端,它们都不需要过程卡的内容:result-only 按定义只发终态,
    /// 回放与测试桩要的是「开了几张卡、序列对不对」。要观测卡里有什么,自己覆盖它记 `view`,
    /// 别让生产多出一条渲染路径来迁就测试。
    fn live_begin(
        &self,
        chat: &ChatRef,
        _view: &summon_core::cot::CotView,
    ) -> anyhow::Result<StreamId> {
        self.stream_begin_card(chat, EMPTY_LIVE_CARD)
    }

    fn live_update(
        &self,
        id: StreamId,
        _view: &summon_core::cot::CotView,
    ) -> anyhow::Result<StreamPatchOutcome> {
        self.stream_patch_card_observed(id, EMPTY_LIVE_CARD)
    }

    /// daemon 换代后接管上一代那一轮时的开卡:`cot` 是锚点记下的载体,还开着就在同一条里续。
    /// 默认实现退化成普通开卡 —— 「续接」只有飞书原生 CoT 有,别的前端本来就是每轮一张新卡。
    fn stream_begin_reattached(
        &self,
        chat: &ChatRef,
        view: &summon_core::cot::CotView,
        _cot: &summon_core::TurnAnchorCot,
    ) -> anyhow::Result<StreamId> {
        self.live_begin(chat, view)
    }

    /// 这条流背后的 CoT 载体 `(cot_id, message_id)`。默认 `None` = 没有这个概念。
    /// 正常轮据它把载体回填进接管锚点,重启后才认得回来。
    fn stream_cot_handle(&self, _id: StreamId) -> Option<(String, String)> {
        None
    }

    /// 该 stream 是否只承载过程展示、最终回复必须另发独立消息。默认卡片流本身就是最终载体。
    fn stream_is_process_only(&self, _id: StreamId) -> bool {
        false
    }

    /// 过程流的推送是否失败过(之后的过程内容都到不了用户)。默认载体没有这种失败。
    /// 飞书已成功确认的正文范围；None 表示前端不提供逐批确认。
    fn stream_confirmed_replies(
        &self,
        _id: StreamId,
    ) -> Option<Vec<summon_core::cot::ReplyDelivery>> {
        None
    }

    fn stream_append_failed(&self, _id: StreamId) -> bool {
        false
    }

    /// 把本轮最后新发出的那条消息登记为本轮的终态回复(见 [`SettledCardKind::Reply`])。
    /// 终态回复投递完后调用;默认前端没有可登记的。
    fn settle_last_sent_as_reply(&self) {}

    /// 同 [`Self::send_message`],并把 `feed` 设成这条消息在会话列表里的预览。终态回复的最后一条
    /// 走这里:会话列表展示的、退出说明修改的都是它。默认前端没有预览可设,照常发送。
    fn send_message_with_feed(
        &self,
        chat: &ChatRef,
        body: &str,
        _feed: &str,
    ) -> anyhow::Result<()> {
        self.send_message(chat, body)
    }

    /// 把过程流上节流攒着的内容立即推出去。默认载体每次更新都直接落地,无可推。
    fn stream_flush(&self, _id: StreamId) -> anyhow::Result<()> {
        Ok(())
    }

    /// 把这条 CoT 从本轮前端摘出来,交给主轮结束后还要往里写的持有者(子 agent 跟随)。
    /// 摘出后本前端不再认这个 id。默认 `None` = 没有可摘的 CoT,调用方照常收口。
    fn stream_detach_cot(&self, _id: StreamId) -> Option<DetachedCot> {
        None
    }

    /// 轮内追加消息被消费时,在过程流里打一个**段边界**。默认无动作 —— 卡片路径靠切卡表达
    /// 同一件事(见 `LiveProjector::content_boundary`),只有整轮共用一个载体的 CoT 需要它。
    fn stream_steer_boundary(&self, _id: StreamId, _note: &str) -> anyhow::Result<()> {
        Ok(())
    }

    /// 同一条过程流交给重启续跑的下一代(Cursor CLI steer):打段边界,并让过程投影按下一代
    /// 从 0 重新编号的 segments 续投。默认同 [`Self::stream_steer_boundary`]。
    fn stream_restart_boundary(&self, id: StreamId, note: &str) -> anyhow::Result<()> {
        self.stream_steer_boundary(id, note)
    }

    /// 过程流以错误结束。普通卡片仍按中止态定格；支持结构化错误终态的前端可覆盖。
    fn stream_fail(&self, id: StreamId, content: &str) -> anyhow::Result<()> {
        self.stream_freeze(id, content)
    }

    /// 过程流以超时结束。默认沿用错误定格；CoT 前端保留 timeout wire 语义。
    fn stream_timeout(&self, id: StreamId, content: &str) -> anyhow::Result<()> {
        self.stream_fail(id, content)
    }

    /// 「必须落卡」的 patch(U1):只承载 turn 的权威终态全文对账。语义同 `stream_patch`,但实现
    /// 须**绕过节流**(飞书前端另带 429 退避重试)。打断/暂停等未完成快照走 `stream_freeze`。
    /// 默认 = `stream_patch`(无节流概念的前端等价)。
    fn stream_finish(&self, id: StreamId, content: &str) -> anyhow::Result<()> {
        self.stream_patch(id, content)
    }

    /// 流式卡在**非权威终态**被冻结：打断/暂停/重跑/分卡边界。与 `stream_finish` 一样绕过
    /// 节流并结束本卡生命周期，但内容仍是未完成快照，不能触发终态才允许的副作用。
    /// 默认前端没有渲染阶段差异，委派 `stream_finish`；飞书前端覆盖以禁止真实 mention。
    fn stream_freeze(&self, id: StreamId, content: &str) -> anyhow::Result<()> {
        self.stream_finish(id, content)
    }

    /// 「过程卡定格」(任务3):turn 完成后用任意卡片 JSON(✅已完成 + 折叠完整回复)原地刷流式卡,
    /// 绕节流必发。默认 no-op(无流式卡映射的前端/测试桩忽略);飞书前端按 StreamId 查 message_id PATCH。
    fn stream_finish_card(&self, _id: StreamId, _card_json: &str) -> anyhow::Result<()> {
        Ok(())
    }

    /// `stream_freeze` 的预渲染卡片版。过程卡定格、暂停和分卡用它；权威最终回复才走
    /// `stream_finish_card`。默认前端无阶段差异，飞书前端覆盖以禁止真实 mention。
    /// `/new` 的会话终态说明与当前过程载体一起收口。
    fn stream_end_session(&self, id: StreamId, _partial: &str) -> anyhow::Result<()> {
        self.stream_freeze(id, summon_i18n::text!("r3c30e0b840"))
    }

    fn stream_freeze_card(&self, id: StreamId, card_json: &str) -> anyhow::Result<()> {
        self.stream_finish_card(id, card_json)
    }

    /// 给流式卡片贴表情(卡片更新连续失败时通知用户用)。按 StreamId 查内部 message_id 映射。
    /// 默认 no-op(测试桩忽略);飞书前端真贴。返回 reaction_id(撤表情用),失败/无映射返回空串。
    fn react_stream_card(&self, _id: StreamId, _key: &str) -> anyhow::Result<String> {
        Ok(String::new())
    }

    /// 表情:贴(返回 reaction id 供撤)/ 撤(OnIt/DONE/error)。
    fn react(&self, msg: &MsgRef, key: &str) -> anyhow::Result<String>;
    fn unreact(&self, msg: &MsgRef, reaction_id: &str) -> anyhow::Result<()>;

    /// 发一张任意交互卡片 JSON(Q1 交互菜单中介:单选/降级文本卡)。回调按 chat_id 路由,不需返回
    /// message_id。默认 no-op —— 测试/Null 前端忽略;飞书前端真发,RecordingFrontend 委派 inner。
    /// 终态失败通知直接投递，返回实际发送结果，供调用方持久化重试。
    fn send_failure_card(&self, chat: &ChatRef, card_json: &str) -> anyhow::Result<()> {
        self.send_card(chat, card_json)
    }

    fn send_card(&self, _chat: &ChatRef, _card_json: &str) -> anyhow::Result<()> {
        Ok(())
    }
}

// 本模块的测全都拿 `RecordingFrontend`+`RecordSink` 当断言手段(录出站调用序列再逐条比对),
// 而录制装饰器是 `debug_assertions` gate 的,release 下不存在 —— 故测也只在 debug 编译。
#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// 最小 Frontend 测试桩:stream_begin 返回自增 StreamId(从 1 起,证明 message_id 不进 core),
    /// 其余调用无副作用。用作 `RecordingFrontend` 的 inner —— 这样 turn 流程经录制装饰器跑、断言录到
    /// 的出站序列(`OutboundRecord`),而不再靠手拼字符串。
    pub(crate) struct NullFrontend {
        next_id: RefCell<u64>,
    }

    impl NullFrontend {
        pub fn new() -> Self {
            NullFrontend {
                next_id: RefCell::new(0),
            }
        }
    }

    impl Frontend for NullFrontend {
        fn send_message(&self, _chat: &ChatRef, _body: &str) -> anyhow::Result<()> {
            Ok(())
        }
        fn stream_begin(
            &self,
            _chat: &ChatRef,
            _content: &str,
            _with_interrupt: bool,
        ) -> anyhow::Result<StreamId> {
            *self.next_id.borrow_mut() += 1;
            Ok(StreamId(*self.next_id.borrow()))
        }
        fn stream_patch(&self, _id: StreamId, _content: &str) -> anyhow::Result<()> {
            Ok(())
        }
        fn react(&self, _msg: &MsgRef, key: &str) -> anyhow::Result<String> {
            Ok(format!("rid-{key}"))
        }
        fn unreact(&self, _msg: &MsgRef, _reaction_id: &str) -> anyhow::Result<()> {
            Ok(())
        }
    }

    /// 构造一条 assistant 事件(交织 live 卡现由 on_event 的 thinking/assistant 块驱动,不再靠 on_stream)。
    fn assistant_event(text: &str) -> summon_core::agent_msg::AgentMessage {
        let mut m =
            summon_core::agent_msg::AgentMessage::of(summon_core::agent_msg::AgentKind::Assistant);
        m.text = Some(text.to_string());
        m
    }

    /// 原生 CoT 是唯一的过程载体 —— owner 2026-09-03 选的方案,并明确接受它的代价:
    /// 飞书 CoT 抽风或客户端不支持时,那一轮就完全没有过程展示。
    ///
    /// 这条测试守的就是「没有第二条路」:默认实现开出来的必须是空壳,任何把 `view` 渲染成
    /// 整卡的改动都会让它变红。它红了不代表要放宽断言 —— 那说明有人把被否掉的兜底加了回来。
    #[test]
    fn the_default_live_card_stays_an_empty_shell() {
        struct Spy(std::cell::RefCell<Vec<String>>);
        impl Frontend for Spy {
            fn send_message(&self, _chat: &ChatRef, _body: &str) -> anyhow::Result<()> {
                Ok(())
            }
            fn stream_begin(
                &self,
                _chat: &ChatRef,
                _content: &str,
                _with_interrupt: bool,
            ) -> anyhow::Result<StreamId> {
                Ok(StreamId(1))
            }
            fn stream_patch(&self, _id: StreamId, _content: &str) -> anyhow::Result<()> {
                Ok(())
            }
            fn stream_finish(&self, _id: StreamId, _content: &str) -> anyhow::Result<()> {
                Ok(())
            }
            fn stream_begin_card(
                &self,
                _chat: &ChatRef,
                card_json: &str,
            ) -> anyhow::Result<StreamId> {
                self.0.borrow_mut().push(card_json.to_string());
                Ok(StreamId(1))
            }
            fn stream_patch_card(&self, _id: StreamId, card_json: &str) -> anyhow::Result<()> {
                self.0.borrow_mut().push(card_json.to_string());
                Ok(())
            }
            fn react(&self, _msg: &MsgRef, _key: &str) -> anyhow::Result<String> {
                Ok(String::new())
            }
            fn unreact(&self, _msg: &MsgRef, _reaction_id: &str) -> anyhow::Result<()> {
                Ok(())
            }
        }

        let spy = Spy(std::cell::RefCell::new(Vec::new()));
        let chat = ChatRef {
            chat_id: "oc_guard",
            chat_type: "p2p",
            agent_dir: std::path::Path::new("/tmp"),
            agent_name: "guard",
            delivery_scope: "test",
            thread_anchor: None,
        };
        // 段里塞着正文与思考:如果哪天默认实现把它们渲进卡,下面两条断言立刻挂。
        let segments = vec![
            summon_core::segment::TurnSegment::Thinking("不该出现的思考".into()),
            summon_core::segment::TurnSegment::Reply("不该出现的正文".into()),
        ];
        let view = summon_core::cot::CotView::new(&segments, None);

        let id = spy.live_begin(&chat, &view).unwrap();
        spy.live_update(id, &view).unwrap();

        let seen = spy.0.borrow().clone();
        assert_eq!(
            seen,
            vec![EMPTY_LIVE_CARD.to_string(), EMPTY_LIVE_CARD.to_string()],
            "默认 live 卡必须是空壳:CoT 之外没有第二条过程渲染路径"
        );
    }

    /// 驱动 card_stream_consume(无头/Transcript 共享的流式消费器)走一轮假源,经 `RecordingFrontend`
    /// 装饰器断言出站序列:首块 stream_begin_card(交织卡)、后续 stream_patch_card——证明核心 turn 流程
    /// 已 frontend 无关且装饰器忠实录到调用 + 入参。live 卡现由 on_event 的 assistant 块驱动。
    #[test]
    fn card_stream_consume_drives_frontend_stream_sequence() {
        let fe = RecordingFrontend::new(NullFrontend::new(), RecordSink::buffer());
        let dir = tempfile::tempdir().unwrap();
        let chat = ChatRef {
            thread_anchor: None,
            chat_id: "oc_test",
            chat_type: "p2p",
            agent_dir: dir.path(),
            agent_name: "tester",
            delivery_scope: "test-scope",
        };
        // 假源:两块 assistant 事件(后块为前块增长快照,归并进同一 Reply 段)+ 一次终态回复。
        let out = crate::runtime::daemon::card_stream_consume(
            &fe,
            &chat,
            false,
            summon_core::progress::DEFAULT_MAX_STEPS,
            false,
            false, // tool_trail: 关(测试默认)
            None,
            None,
            None, // badge_seed:测试不校验角标
            &crate::runtime::outbox_store::OutboxStore::new(dir.path().join("outbox")),
            &|| None,
            None, // 测试/回放:无 subagent 状态源
            &mut |_: &[String]| {},
            |on_msg, _on_str, _on_tool, on_ev, _on_int, _on_tick, _on_steer| {
                on_ev(&assistant_event("你好"));
                on_ev(&assistant_event("你好,世界"));
                on_msg("你好,世界");
                Ok(crate::capture::TurnOutcome {
                    session_id: Some("sid-x".to_string()),
                    reply: Some("你好,世界".to_string()),
                    ..Default::default()
                })
            },
        );
        assert!(out.0.is_ok());
        assert!(out.1, "应已向用户发出内容");
        let recs = fe.recorded();
        // 流式渲染缓冲(U17):首块无卡 → 必发 StreamBeginCard;中间 delta 若字数/时间未超阈值
        // 可能被缓冲(不发 StreamPatchCard);终态对账 on_message 无条件 patch(StreamPatch)。
        // 本测试用短文本(「你好」2 字 → 「你好,世界」5 字,增量 3 字<40),中间 patch 被缓冲跳过,
        // 终态对账仍送达全文。
        assert!(!recs.is_empty(), "至少有终态对账记录");
        match &recs[0] {
            OutboundRecord::StreamBeginCard {
                chat_id,
                card_json,
                id,
            } => {
                assert_eq!(chat_id, "oc_test");
                assert_eq!(*id, 1);
                // live 卡在录制层记空 —— 它的内容由 cot_begin / cot_update 单独录,不重复进
                // 这条记录。正文能不能送达看下面终态那条 patch。
                assert!(card_json.is_empty(), "live 卡的录制不带正文:{card_json}");
            }
            other => panic!("首条应为 StreamBeginCard,实为 {other:?}"),
        }
        // 终态对账:最后一条必为 StreamPatch(绕节流、无条件 patch 全文)。
        assert_eq!(
            recs.last(),
            Some(&OutboundRecord::SendMessage {
                chat_id: "oc_test".into(),
                body: "你好,世界".to_string(),
            }),
            "终态独立投递"
        );
    }

    /// U1 终态对账:流式已发过卡后,on_message 的权威全文必须无条件再 patch 一次(stream_finish,
    /// 绕节流)——否则最后几次 stream_patch 失败/被节流时,卡片永久冻结在中途内容、回复丢尾。
    #[test]
    fn card_stream_consume_reconciles_final_text_after_stream() {
        let fe = RecordingFrontend::new(NullFrontend::new(), RecordSink::buffer());
        let dir = tempfile::tempdir().unwrap();
        let chat = ChatRef {
            thread_anchor: None,
            chat_id: "oc_reconcile",
            chat_type: "p2p",
            agent_dir: dir.path(),
            agent_name: "tester",
            delivery_scope: "test-scope",
        };
        // 模拟「尾部 patch 没送达」:流式只发出了前半(经 on_event 建交织卡),终态全文比卡上内容多出尾巴。
        let out = crate::runtime::daemon::card_stream_consume(
            &fe,
            &chat,
            false,
            summon_core::progress::DEFAULT_MAX_STEPS,
            false,
            false, // tool_trail: 关(测试默认)
            None,
            None,
            None, // badge_seed:测试不校验角标
            &crate::runtime::outbox_store::OutboxStore::new(dir.path().join("outbox")),
            &|| None,
            None, // 测试/回放:无 subagent 状态源
            &mut |_: &[String]| {},
            |on_msg, _on_str, _on_tool, on_ev, _on_int, _on_tick, _on_steer| {
                on_ev(&assistant_event("前半"));
                on_msg("前半,以及只在终态全文里的尾巴");
                Ok(crate::capture::TurnOutcome::default())
            },
        );
        assert!(out.0.is_ok());
        let recs = fe.recorded();
        assert_eq!(
            recs.last(),
            Some(&OutboundRecord::SendMessage {
                chat_id: "oc_reconcile".into(),
                body: "前半,以及只在终态全文里的尾巴".to_string(),
            }),
            "终态全文单独发送，完整保留尾部"
        );
    }

    /// U2:超长终态回复分段——首段对账 patch 到流式卡,其余按序补发独立卡片,逐字节不丢。
    #[test]
    fn card_stream_consume_segments_oversized_final_reply() {
        let fe = RecordingFrontend::new(NullFrontend::new(), RecordSink::buffer());
        let dir = tempfile::tempdir().unwrap();
        let chat = ChatRef {
            thread_anchor: None,
            chat_id: "oc_long",
            chat_type: "p2p",
            agent_dir: dir.path(),
            agent_name: "tester",
            delivery_scope: "test-scope",
        };
        let body = format!("{}\n", "a".repeat(99)).repeat(250); // 25_000B > 20KB 上限
        let body2 = body.clone();
        let out = crate::runtime::daemon::card_stream_consume(
            &fe,
            &chat,
            false,
            summon_core::progress::DEFAULT_MAX_STEPS,
            false,
            false, // tool_trail: 关(测试默认)
            None,
            None,
            None, // badge_seed:测试不校验角标
            &crate::runtime::outbox_store::OutboxStore::new(dir.path().join("outbox")),
            &|| None,
            None, // 测试/回放:无 subagent 状态源
            &mut |_: &[String]| {},
            move |on_msg, _on_str, _on_tool, on_ev, _on_int, _on_tick, _on_steer| {
                on_ev(&assistant_event("开头"));
                on_msg(&body2);
                Ok(crate::capture::TurnOutcome::default())
            },
        );
        assert!(out.0.is_ok());
        let expected =
            summon_core::feishu::split_for_card(&body, summon_core::feishu::CARD_TEXT_SAFE_LIMIT);
        assert!(expected.len() > 1, "前置:测试体必须超限分段");
        let recs = fe.recorded();
        assert!(matches!(
            recs.first(),
            Some(OutboundRecord::StreamBeginCard { .. })
        ));
        assert!(
            matches!(recs.get(1), Some(OutboundRecord::StreamPatch { content, .. }) if content.is_empty())
        );
        let delivered: Vec<_> = recs
            .iter()
            .filter_map(|record| match record {
                OutboundRecord::SendMessage { body, .. } => Some(body.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            delivered,
            expected.iter().map(String::as_str).collect::<Vec<_>>()
        );
        assert_eq!(delivered.concat(), body);
    }

    /// 无流式、只有终态回复时:on_message 兜底走 send_message 发一张终态卡片(emitted=true)。
    #[test]
    fn card_stream_consume_falls_back_to_single_card_when_no_stream() {
        let fe = RecordingFrontend::new(NullFrontend::new(), RecordSink::buffer());
        let dir = tempfile::tempdir().unwrap();
        let chat = ChatRef {
            thread_anchor: None,
            chat_id: "oc_only_final",
            chat_type: "p2p",
            agent_dir: dir.path(),
            agent_name: "tester",
            delivery_scope: "test-scope",
        };
        let out = crate::runtime::daemon::card_stream_consume(
            &fe,
            &chat,
            false,
            summon_core::progress::DEFAULT_MAX_STEPS,
            false,
            false, // tool_trail: 关(测试默认)
            None,
            None,
            None, // badge_seed:测试不校验角标
            &crate::runtime::outbox_store::OutboxStore::new(dir.path().join("outbox")),
            &|| None,
            None, // 测试/回放:无 subagent 状态源
            &mut |_: &[String]| {},
            |on_msg, _on_str, _on_tool, _on_ev, _on_int, _on_tick, _on_steer| {
                on_msg("仅终态");
                Ok(crate::capture::TurnOutcome::default())
            },
        );
        assert!(out.0.is_ok());
        assert!(out.1);
        assert_eq!(
            fe.recorded(),
            vec![OutboundRecord::SendMessage {
                chat_id: "oc_only_final".to_string(),
                body: "仅终态".to_string(),
            }],
            "无流式时 on_message 必须走终态发送，不能借流式入口而漏掉终态 mention"
        );
    }

    /// 装饰器对全部 `Frontend` 方法都录调用 + 入参,且委派 inner(react 录到 inner 返回的 reaction id)。
    /// 同时验 `OutboundRecord` serde round-trip(回放靠它读 golden)。
    #[test]
    fn failed_stream_recovery_falls_back_to_new_final_card() {
        let fe = RecordingFrontend::new(NullFrontend::new(), RecordSink::buffer());
        let dir = tempfile::tempdir().unwrap();
        let chat = ChatRef {
            thread_anchor: None,
            chat_id: "oc_recover_fail",
            chat_type: "p2p",
            agent_dir: dir.path(),
            agent_name: "tester",
            delivery_scope: "test-scope",
        };
        let saved = summon_core::session::stream_card::StreamCardState {
            message_id: "om_old".into(),
            chat_tag: summon_core::registry::chat_tag("p2p", "oc_recover_fail"),
            with_interrupt: true,
            created_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64,
        };
        let out = crate::runtime::daemon::card_stream_consume(
            &fe,
            &chat,
            false,
            summon_core::progress::DEFAULT_MAX_STEPS,
            false,
            false, // tool_trail: 关(测试默认)
            Some(&saved),
            None,
            None, // badge_seed:测试不校验角标
            &crate::runtime::outbox_store::OutboxStore::new(dir.path().join("outbox")),
            &|| None,
            None, // 测试/回放:无 subagent 状态源
            &mut |_: &[String]| {},
            |on_msg, _on_str, _on_tool, _on_ev, _on_int, _on_tick, _on_steer| {
                on_msg("恢复失败后的终态");
                Ok(crate::capture::TurnOutcome::default())
            },
        );
        assert!(out.0.is_ok());
        assert!(out.1, "恢复失败不能误标旧卡已发，必须成功建新卡");
        assert_eq!(
            fe.recorded(),
            vec![OutboundRecord::SendMessage {
                chat_id: "oc_recover_fail".into(),
                body: "恢复失败后的终态".into(),
            }]
        );
    }

    #[test]
    fn recording_frontend_delegates_final_reply_splitter() {
        struct SplitFrontend;

        impl Frontend for SplitFrontend {
            fn send_message(&self, _chat: &ChatRef, _body: &str) -> anyhow::Result<()> {
                Ok(())
            }
            fn split_final_reply(&self, _chat: &ChatRef, body: &str) -> Vec<String> {
                assert_eq!(body, "leftright");
                vec!["left".into(), "right".into()]
            }
            fn stream_begin(
                &self,
                _chat: &ChatRef,
                _content: &str,
                _with_interrupt: bool,
            ) -> anyhow::Result<StreamId> {
                Ok(StreamId(1))
            }
            fn stream_patch(&self, _id: StreamId, _content: &str) -> anyhow::Result<()> {
                Ok(())
            }
            fn react(&self, _msg: &MsgRef, _key: &str) -> anyhow::Result<String> {
                Ok(String::new())
            }
            fn unreact(&self, _msg: &MsgRef, _reaction_id: &str) -> anyhow::Result<()> {
                Ok(())
            }
        }

        let fe = RecordingFrontend::new(SplitFrontend, RecordSink::buffer());
        let dir = tempfile::tempdir().unwrap();
        let chat = ChatRef {
            thread_anchor: None,
            chat_id: "oc_split",
            chat_type: "p2p",
            agent_dir: dir.path(),
            agent_name: "tester",
            delivery_scope: "test-scope",
        };

        assert_eq!(
            fe.split_final_reply(&chat, "leftright"),
            vec!["left".to_string(), "right".to_string()]
        );
        assert!(fe.recorded().is_empty(), "纯分段规划不应产生出站记录");
    }

    #[test]
    fn recording_frontend_records_all_ops_and_delegates() {
        let fe = RecordingFrontend::new(NullFrontend::new(), RecordSink::buffer());
        let dir = tempfile::tempdir().unwrap();
        let chat = ChatRef {
            thread_anchor: None,
            chat_id: "oc_x",
            chat_type: "p2p",
            agent_dir: dir.path(),
            agent_name: "t",
            delivery_scope: "test-scope",
        };
        let msg = MsgRef { message_id: "om_1" };
        fe.send_message(&chat, "正文").unwrap();
        let id = fe.stream_begin(&chat, "首块", true).unwrap();
        fe.stream_patch(id, "更多").unwrap();
        let rid = fe.react(&msg, "OnIt").unwrap();
        fe.unreact(&msg, &rid).unwrap();

        let recs = fe.recorded();
        assert_eq!(
            recs,
            vec![
                OutboundRecord::SendMessage {
                    chat_id: "oc_x".to_string(),
                    body: "正文".to_string(),
                },
                OutboundRecord::StreamBegin {
                    chat_id: "oc_x".to_string(),
                    content: "首块".to_string(),
                    id: 1,
                },
                OutboundRecord::StreamPatch {
                    id: 1,
                    content: "更多".to_string(),
                },
                OutboundRecord::React {
                    message_id: "om_1".to_string(),
                    key: "OnIt".to_string(),
                    reaction_id: "rid-OnIt".to_string(),
                },
                OutboundRecord::Unreact {
                    message_id: "om_1".to_string(),
                    reaction_id: "rid-OnIt".to_string(),
                },
            ]
        );
        // serde round-trip(回放读 golden ndjson)。
        let json = serde_json::to_string(&recs[3]).unwrap();
        let back: OutboundRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(back, recs[3]);
    }
}
