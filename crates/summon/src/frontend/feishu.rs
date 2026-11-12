//! `FeishuFrontend`:`Frontend` 的飞书实现。**只转发**到 `frontend::outbound` 投递原语与
//! `FeishuClient` —— 行为对真实飞书路径逐字节不变。它独占 `StreamId → message_id` 映射(message_id 永不进 core):`stream_begin`
//! 发首张卡片拿到飞书 message_id、分配自增 `StreamId` 入表;`stream_patch` 凭 `StreamId` 查回
//! message_id 调 `update_card`,且**带时间窗节流**(U1:飞书更新卡片 5 QPS/消息,逐行直打必撞频控;
//! 窗口内跳过不丢内容——每次 patch 都是累积全文,尾部由 `stream_finish` 终态对账兜底)。

use super::outbound::append_outbound;
use super::{ChatRef, Frontend, MsgRef, StreamId};
use crate::frontend::client::FeishuClient;
use crate::runtime::identity_cache::IdentityCacheStore;
use crate::runtime::peers_store::PeersStore;
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;
use summon_core::feishu::{
    render_external_lark_md, render_outbound_mentions, PreviewButton, RenderedLarkMd,
};
use summon_core::registry::chat_tag;
use summon_core::throttle::PatchThrottle;

/// 流式 patch 的最小间隔。飞书频控 5 QPS/消息(200ms);取 500ms 留余量,打字机观感仍流畅。
const STREAM_PATCH_MIN_INTERVAL_MS: u64 = 500;

/// CoT 事件流的合并窗口。官方限频远高于这里，但 600ms 是已真机验证过的观感/请求量平衡点。
const COT_UPDATE_MIN_INTERVAL_MS: u64 = 600;
const COT_EVENTS_PER_REQUEST: usize = 50;
static COT_RECOVERY_DRAIN: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn active_cot_recoveries(
) -> &'static std::sync::Mutex<std::collections::HashSet<std::path::PathBuf>> {
    static ACTIVE: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashSet<std::path::PathBuf>>,
    > = std::sync::OnceLock::new();
    ACTIVE.get_or_init(|| std::sync::Mutex::new(std::collections::HashSet::new()))
}

fn cot_process_generation() -> &'static str {
    static GENERATION: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    GENERATION
        .get_or_init(|| uuid::Uuid::new_v4().to_string())
        .as_str()
}

fn confirm_reply_events(
    confirmed: &mut Vec<summon_core::cot::ReplyDelivery>,
    events: &[summon_core::cot::CotEvent],
) {
    for progress in events.iter().filter_map(|event| event.reply_delivery) {
        match confirmed
            .iter_mut()
            .find(|old| old.generation == progress.generation && old.segment == progress.segment)
        {
            Some(old) => old.bytes = old.bytes.max(progress.bytes),
            None => confirmed.push(progress),
        }
    }
}

fn log_cot_batch(
    chat: &str,
    cot: &str,
    batch: usize,
    events: &[summon_core::cot::CotEvent],
    confirmed: &[summon_core::cot::ReplyDelivery],
    success: bool,
) {
    crate::observe::log::log(&format!(
        "CoT delivery chat={chat} cot={cot} batch={batch} events={} outcome={} confirmed={:?}",
        events.len(),
        if success { "confirmed" } else { "uncertain" },
        confirmed
            .iter()
            .max_by_key(|reply| (reply.generation, reply.segment)),
    ));
}

const _: () = assert!(
    COT_UPDATE_MIN_INTERVAL_MS >= 600,
    summon_i18n::source!("r6174a9f9b2")
);

struct CotState {
    carrier: CotCarrier,
    projector: summon_core::cot::CotProjector,
    pending: Vec<summon_core::cot::CotEvent>,
    append_failed: bool,
    confirmed_replies: Vec<summon_core::cot::ReplyDelivery>,
    next_batch: usize,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct CotRecoveryRecord {
    generation: String,
    cot_id: String,
    message_id: String,
    /// 升级时撤回旧版本遗留的独立控制消息；新记录恒为 `None`。
    #[serde(default, rename = "companion_mid")]
    legacy_control_mid: Option<String>,
    cot_closed: bool,
    #[serde(default)]
    append_failed: bool,
    /// 记录归属的 chat。启动收口据它问「这个 chat 是不是正在被接管」。
    #[serde(default)]
    chat_tag: Option<String>,
    /// 投影器快照,每次 push 成功后重写。没有它,新一代要么重推已发过的段、要么关不掉
    /// 上一代开着的 message —— 段边界只是文案,真正让续接成立的是这份游标。
    #[serde(default)]
    projector: Option<summon_core::cot::CotProjector>,
    #[serde(default)]
    page: Option<CotPage>,
}

#[derive(Clone, Copy)]
enum CotTerminal<'a> {
    Finished(&'a str),
    SessionEnded,
    Error(&'a str),
    Timeout,
}

enum CotBeginError {
    BeforeCreate(anyhow::Error),
}

/// 一张流式卡走哪条传输。
enum StreamTransport {
    /// 文本流:整卡 PATCH `im/v1/messages/{id}`。承载后台任务通知与子代理这类旁路卡。
    Text,
    /// 飞书原生 CoT 消息。事件只追加，最终回复另发独立消息。
    Cot(Box<CotState>),
}

/// 从本轮前端摘出来的一条 CoT(见 [`Frontend::stream_detach_cot`]):飞书上的载体 + 投影器。
///
/// [`Frontend::stream_detach_cot`]: crate::frontend::Frontend::stream_detach_cot
#[derive(Debug)]
pub struct DetachedCot {
    pub carrier: CotCarrier,
    pub projector: summon_core::cot::CotProjector,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct CotPage {
    cot_id: String,
    message_id: String,
    closed: bool,
    #[serde(default)]
    creation_pending: bool,
    wire: summon_core::cot::paging::CotWireState,
}

#[derive(Debug, Clone)]
struct CotRoute {
    chat_id: String,
    reply_to: Option<(String, bool)>,
}

/// The root identity stays stable for restart ownership; `page` is the active wire carrier.
#[derive(Debug)]
pub struct CotCarrier {
    cot_id: String,
    message_id: String,
    chat_tag: String,
    recovery_path: Option<std::path::PathBuf>,
    route: Option<CotRoute>,
    page: CotPage,
}

impl CotCarrier {
    fn new(
        handle: super::client::CotHandle,
        chat_tag: String,
        recovery_path: Option<std::path::PathBuf>,
        route: Option<CotRoute>,
    ) -> Self {
        Self {
            page: CotPage {
                cot_id: handle.cot_id.clone(),
                message_id: handle.message_id.clone(),
                closed: false,
                creation_pending: false,
                wire: Default::default(),
            },
            cot_id: handle.cot_id,
            message_id: handle.message_id,
            chat_tag,
            recovery_path,
            route,
        }
    }

    #[cfg(test)]
    pub(crate) fn for_test(cot_id: &str, message_id: &str) -> Self {
        Self::new(
            super::client::CotHandle {
                cot_id: cot_id.into(),
                message_id: message_id.into(),
            },
            String::new(),
            None,
            None,
        )
    }

    /// Mark an append in flight before networking. A crash must never resume from an older cursor.
    fn checkpoint(&self) -> anyhow::Result<()> {
        let Some(path) = self.recovery_path.as_deref() else {
            return Ok(());
        };
        let _guard = COT_RECOVERY_DRAIN
            .lock()
            .map_err(|_| anyhow::anyhow!("CoT recovery lifecycle lock poisoned"))?;
        let mut record = FeishuFrontend::read_cot_recovery(path)
            .ok_or_else(|| anyhow::anyhow!("CoT recovery checkpoint missing"))?;
        record.append_failed = true;
        record.page = Some(self.page.clone());
        crate::runtime::fsio::atomic_write_mode(path, &serde_json::to_vec(&record)?, 0o600)
    }

    fn rotate(
        &mut self,
        fc: &FeishuClient,
        accepted: &mut dyn FnMut(&[summon_core::cot::CotEvent]),
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.recovery_path.is_some(),
            summon_i18n::text!("cot.missing_checkpoint")
        );
        let route = self
            .route
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!(summon_i18n::text!("cot.missing_route")))?;
        self.checkpoint()?;
        if !self.page.closed {
            fc.cot_complete(
                &self.page.cot_id,
                &self.page.message_id,
                super::client::CotCompleteReason::Done,
            )?;
            self.page.closed = true;
            self.checkpoint()?;
        }
        // Creation is attempted once. An ambiguous response must not create another carrier.
        self.page.creation_pending = true;
        self.checkpoint()?;
        let handle = fc.cot_create(
            &route.chat_id,
            route
                .reply_to
                .as_ref()
                .map(|(id, thread)| (id.as_str(), *thread)),
        )?;
        let (prelude, next_wire) = self
            .page
            .wire
            .next_page(&FeishuFrontend::unix_ms().to_string());
        self.page.cot_id = handle.cot_id;
        self.page.message_id = handle.message_id;
        self.page.closed = false;
        self.page.creation_pending = false;
        self.checkpoint()?;
        fc.cot_push(&self.page.cot_id, &self.page.message_id, &prelude)?;
        self.page.wire = next_wire;
        accepted(&prelude);
        self.checkpoint()?;
        Ok(())
    }

    fn push_with(
        &mut self,
        fc: &FeishuClient,
        events: &[summon_core::cot::CotEvent],
        mut accepted: impl FnMut(&[summon_core::cot::CotEvent]),
    ) -> anyhow::Result<()> {
        if events.is_empty() {
            return Ok(());
        }
        anyhow::ensure!(!self.page.closed, "CoT carrier is closed");
        self.checkpoint()?;
        let mut sent = 0;
        while sent < events.len() {
            let (batch, consumed, next_wire) = self.page.wire.batch(&events[sent..]);
            if consumed == 0 {
                self.rotate(fc, &mut accepted)?;
                continue;
            }
            fc.cot_push(&self.page.cot_id, &self.page.message_id, &batch)?;
            self.page.wire = next_wire;
            self.page.closed = batch
                .iter()
                .any(|event| matches!(event.event_type.as_str(), "RUN_FINISHED" | "RUN_ERROR"));
            accepted(&batch);
            self.checkpoint()?;
            sent += consumed;
        }
        Ok(())
    }

    pub(crate) fn push(
        &mut self,
        fc: &FeishuClient,
        events: &[summon_core::cot::CotEvent],
    ) -> anyhow::Result<()> {
        self.push_with(fc, events, |_| {})
    }

    pub(crate) fn complete(
        &mut self,
        fc: &FeishuClient,
        reason: super::client::CotCompleteReason,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.page.creation_pending,
            summon_i18n::text!("cot.creation_uncertain")
        );
        if !self.page.closed {
            fc.cot_complete(&self.page.cot_id, &self.page.message_id, reason)?;
            self.page.closed = true;
            self.checkpoint()?;
        }
        Ok(())
    }

    pub(crate) fn snapshot(&self, projector: &summon_core::cot::CotProjector) {
        if let Some(path) = self.recovery_path.as_deref() {
            FeishuFrontend::rewrite_cot_recovery(
                path,
                &CotRecoveryRecord {
                    generation: cot_process_generation().to_string(),
                    cot_id: self.cot_id.clone(),
                    message_id: self.message_id.clone(),
                    legacy_control_mid: None,
                    cot_closed: false,
                    append_failed: false,
                    chat_tag: Some(self.chat_tag.clone()),
                    projector: Some(projector.clone()),
                    page: Some(self.page.clone()),
                },
            );
        }
    }

    pub(crate) fn release(&self, closed: bool) {
        let Some(path) = self.recovery_path.as_deref() else {
            return;
        };
        if closed {
            FeishuFrontend::rewrite_cot_recovery(
                path,
                &CotRecoveryRecord {
                    generation: cot_process_generation().to_string(),
                    cot_id: self.cot_id.clone(),
                    message_id: self.message_id.clone(),
                    legacy_control_mid: None,
                    cot_closed: true,
                    append_failed: false,
                    chat_tag: Some(self.chat_tag.clone()),
                    projector: None,
                    page: Some(self.page.clone()),
                },
            );
        }
        FeishuFrontend::retire_cot_recovery(path, closed);
    }
}

/// 读一张本地图片、过本地校验、上传拿 `image_key`。
///
/// 校验(存在性/格式/大小)一律在上传**之前**做完:不拿飞书的 400 当校验器,那要等一次完整上传
/// 才知道错、还白占带宽。`~` 在这里展开——正文里写 `~/a.png` 是很自然的事。
fn upload_local_image(fc: &FeishuClient, path: &str) -> anyhow::Result<String> {
    let expanded = summon_core::registry::expand_tilde(path);
    let file = std::path::Path::new(&expanded);
    let meta = std::fs::metadata(file)?;
    if !meta.is_file() {
        anyhow::bail!(summon_i18n::format!("rd156c5d9c3"));
    }
    let ext = file.extension().and_then(|e| e.to_str());
    if let Err(e) = summon_core::image::validate(ext, meta.len()) {
        anyhow::bail!("{e}");
    }
    let bytes = std::fs::read(file)?;
    let filename = file
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("image.png");
    fc.upload_image(bytes, filename)
}

/// 一张在刷的流式卡片:飞书 message_id + 它自己的 patch 节流器(频控按消息计,节流也按卡计)。
/// `with_interrupt`(任务4):本卡是否内嵌「⏸ 打断」按钮——begin 时定,patch 沿用(按钮持续在),
/// finish/定格卡覆盖时去掉。
/// `chat_tag`:本卡所属 chat 的 tag(只供 finish 查 peers 做终态 @mention 替换)。
struct StreamState {
    mid: String,
    chat_tag: String,
    throttle: PatchThrottle,
    with_interrupt: bool,
    transport: StreamTransport,
}

pub struct FeishuFrontend<'a> {
    fc: &'a FeishuClient,
    /// StreamId.0 → 卡片状态;adapter 私有,core 只见 StreamId。
    stream_map: RefCell<HashMap<u64, StreamState>>,
    next_id: RefCell<u64>,
    /// 节流时间基准(单调钟);PatchThrottle 只吃注入的毫秒数,保持纯件可测。
    epoch: std::time::Instant,
    /// 全局 alias → name 映射(从 agents 目录建),用于出站 @mention 替换。
    alias_map: HashMap<String, String>,
    /// per-chat peers 存储(读 peers 文件拿本 chat 的 bot 列表)。
    peers_store: Option<PeersStore>,
    /// 与 peers 相同 agent+app scope 的身份缓存，绝不能回退到全局 open_id 集合。
    identity_cache: IdentityCacheStore,
    /// 本 chat **最近一张已定格的卡**(message_id + 定格时的卡 JSON)。跨轮共享,由 chat 线程
    /// 持有 —— frontend 本身每轮新建,存不住这个。用途只有一个:main agent 已推送完、子代理
    /// 还在跑的那段窗口里,继续把子代理状态刷到这张卡上(owner 2026-08-14)。
    /// `None` = 不需要该能力的调用方(outbox drain / 测试),行为与本字段引入前逐字一致。
    settled_card: Option<crate::frontend::SharedSettledCard>,
    /// 本轮终态回复的原文槽(见 [`crate::frontend::SharedFinalReply`]);`None` = 本轮不需要。
    final_reply: Option<crate::frontend::SharedFinalReply>,
    /// 本轮最近一条新发出的消息:message_id + 发出时的成品卡 JSON。终态回复投递完后由
    /// `settle_last_sent_as_reply` 登记进 `settled_card`。
    last_sent: RefCell<Option<(String, String)>>,
    /// 文件预览服务:`Some` 时把终态回复里提到的本地文件出成卡片末尾的按钮。
    /// `None`(默认)= 不出按钮,正文原样。
    preview_server: Option<Arc<crate::runtime::preview_server::PreviewServer>>,
    /// 预览相对化基准(agent working_dir):mint 时记下,预览页头部据此显示项目相对路径。
    /// `None` = 不缩,预览页显示绝对路径。
    preview_base: Option<String>,
    /// live 进度是否优先走飞书原生 CoT。创建失败时只在本轮向下退一级。
    /// false 时 CoT 只保留固定运行状态与控制按钮，禁止追加上下文等过程正文。
    cot_content_visible: bool,
    /// `keep(chat_tag, cot_id)` = 这条 CoT 正被接管轮认领,开新卡前的反熵一个字节都不发、
    /// 也不删记录。表由启动那次接管扫描算出、由 daemon 侧注入,与启动收口查的是同一份 ——
    /// frontend 自己没有接管判据,拿不到表时(outbox drain / 测试)一律不留。
    cot_keep: Option<CotKeep<'a>>,
    /// 触发本轮的原始消息。主干消息只做引用回复；原消息已在 thread 内时才把回复继续放进 thread。
    reply_target: Option<(String, bool)>,
}

/// 「这条 CoT 正被接管轮认领」的判据表:`(chat_tag, cot_id) → 留不留`。
pub type CotKeep<'a> = Box<dyn Fn(&str, &str) -> bool + 'a>;

impl<'a> FeishuFrontend<'a> {
    pub fn new(
        fc: &'a FeishuClient,
        alias_map: HashMap<String, String>,
        peers_store: Option<PeersStore>,
        identity_cache: IdentityCacheStore,
    ) -> Self {
        FeishuFrontend {
            fc,
            stream_map: RefCell::new(HashMap::new()),
            next_id: RefCell::new(0),
            epoch: std::time::Instant::now(),
            alias_map,
            peers_store,
            identity_cache,
            settled_card: None,
            final_reply: None,
            last_sent: RefCell::new(None),
            preview_server: None,
            preview_base: None,
            cot_content_visible: true,
            cot_keep: None,
            reply_target: None,
        }
    }

    pub fn with_cot_content_visible(mut self, visible: bool) -> Self {
        self.cot_content_visible = visible;
        self
    }

    pub fn with_cot_keep(mut self, keep: CotKeep<'a>) -> Self {
        self.cot_keep = Some(keep);
        self
    }

    pub fn with_reply_target(mut self, message_id: &str, reply_in_thread: bool) -> Self {
        self.reply_target =
            (!message_id.is_empty()).then(|| (message_id.to_string(), reply_in_thread));
        self
    }

    fn reply_target(&self) -> Option<(&str, bool)> {
        self.reply_target
            .as_ref()
            .map(|(message_id, in_thread)| (message_id.as_str(), *in_thread))
    }

    fn reply_target_for<'b>(&'b self, chat: &'b ChatRef<'_>) -> Option<(&'b str, bool)> {
        self.reply_target()
            .or_else(|| chat.thread_anchor.map(|message_id| (message_id, true)))
    }

    /// 挂上跨轮共享的「最近定格卡」记账槽。链式设置而非塞进 `new`:既有调用点(outbox drain、
    /// 测试)不需要这个能力,签名不该被它污染。
    pub fn with_settled_card(mut self, slot: crate::frontend::SharedSettledCard) -> Self {
        self.settled_card = Some(slot);
        self
    }

    /// 挂上终态回复原文槽。链式设置:只有 `/handoff` 写交接那一轮需要。
    pub fn with_final_reply(mut self, slot: Option<crate::frontend::SharedFinalReply>) -> Self {
        self.final_reply = slot;
        self
    }

    /// 挂上文件预览服务。链式设置:只有 chat 线程的 turn 路径需要,outbox drain / 测试
    /// 不传 = 不出文件按钮。
    pub fn with_preview_server(
        mut self,
        server: Option<Arc<crate::runtime::preview_server::PreviewServer>>,
    ) -> Self {
        self.preview_server = server;
        self
    }

    /// 挂上预览相对化基准(agent working_dir)。链式设置:与 `with_preview_server` 配对,
    /// 只有 chat 线程的 turn 路径需要;outbox drain / 测试不传 = 预览页显示绝对路径。
    pub fn with_preview_base(mut self, base: Option<String>) -> Self {
        self.preview_base = base;
        self
    }

    fn now_ms(&self) -> u64 {
        self.epoch.elapsed().as_millis() as u64
    }

    fn unix_ms() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_millis() as u64)
            .unwrap_or(0)
    }

    /// adapter 是最后一个看得到 mention 展开后完整 JSON 的边界；所有卡片在真正联网前都在这里
    /// fail-closed，避免上层按原文预算误判为“已完整投递”。
    fn ensure_card_budget(operation: &str, card: &str) -> anyhow::Result<()> {
        if card.len() > summon_core::feishu::CARD_TOTAL_SAFE_LIMIT {
            anyhow::bail!(summon_i18n::format!(
                "r6432f3d7bb",
                card.len(),
                summon_core::feishu::CARD_TOTAL_SAFE_LIMIT,
            ));
        }
        Ok(())
    }

    /// 合并 per-chat peers + 全局身份缓存里的条目,供出站 @mention 替换用。
    /// peers 优先(它们是 `/introduce` 正式认识的成员),身份缓存作回退(见过但没 introduce 的人/bot)。
    fn merged_peers(&self, chat_tag: &str) -> Vec<summon_core::peers::Peer> {
        let mut peers = self
            .peers_store
            .as_ref()
            .and_then(|store| store.load(chat_tag))
            .map(|pf| pf.peers)
            .unwrap_or_default();
        // 身份缓存回退:把缓存里的条目转成 Peer,追加到 peers 后面(低优先级)。
        // 已在 peers 里的 name 不重复加(peers 优先)。
        let cache = self.identity_cache.load();
        let existing_names: std::collections::HashSet<String> =
            peers.iter().map(|p| p.name.clone()).collect();
        for (oid, entry) in &cache.entries {
            if !existing_names.contains(&entry.name) {
                peers.push(summon_core::peers::Peer {
                    open_id: oid.clone(),
                    name: entry.name.clone(),
                    ..Default::default()
                });
            }
        }
        peers
    }

    /// 终态正文的出站渲染:提到的本地文件做成回复末尾的按钮(正文里的文字链接只留文字),再做
    /// @mention 替换 —— 根据 chat_tag 加载本 chat 的 peers + 身份缓存回退,把 @name/@alias 替换成
    /// 飞书 `<at>` 标签。纯字符串操作,O(n) 不阻塞。
    fn render_reply(&self, chat_tag: &str, text: &str) -> (RenderedLarkMd, Vec<PreviewButton>) {
        let peers = self.merged_peers(chat_tag);
        // 先找本地路径再转义:转义会插零宽字符,之后就认不出路径了。
        let (text, buttons) = self.preview_buttons(text);
        // 即使 peers 为空也必须走外部文本转义；mention 插入发生在转义之后。
        (
            render_external_lark_md(&text, &peers, &self.alias_map),
            buttons,
        )
    }

    /// 流式正文只做外部 markdown 转义，不产生带投递副作用的真实 `<at>`,也不生成文件按钮 ——
    /// 本地路径在流式快照里是普通文字,按钮在终态挂上。
    ///
    /// mention 只在 `stream_finish` / `send_message` 这些终态边界展开：流式快照既可能仍在
    /// thinking，也可能只是未完成的正文，提前把其中的 `@bot` 变成 `<at>` 会真实触发对方。
    fn render_streaming_text(&self, _chat_tag: &str, text: &str) -> RenderedLarkMd {
        render_external_lark_md(text, &[], &self.alias_map)
    }

    /// 文本里提到的本地文件 → 回复末尾的按钮;没挂预览服务时零成本返回借用、没有按钮。
    fn preview_buttons<'b>(
        &self,
        text: &'b str,
    ) -> (std::borrow::Cow<'b, str>, Vec<PreviewButton>) {
        match &self.preview_server {
            Some(previews) => {
                let (text, buttons) = previews.link_buttons(text, self.preview_base.as_deref());
                (std::borrow::Cow::Owned(text), buttons)
            }
            None => (std::borrow::Cow::Borrowed(text), Vec::new()),
        }
    }

    /// 对一张终态飞书卡片 JSON 字符串里所有 markdown 元素的 content 做 @mention 替换。
    /// 卡片已由调用方预渲染成 JSON，需要在终态发送前把其中的 @name/@alias 替换成 <at> 标签。
    fn render_mentions_in_card(&self, chat_tag: &str, card_json: &str) -> String {
        let card_json = self.preview_buttons_in_card(card_json);
        let peers = self.merged_peers(chat_tag);
        let Ok(mut v) = serde_json::from_str::<serde_json::Value>(&card_json) else {
            return card_json;
        };
        if v.pointer("/config/summary/content")
            .and_then(serde_json::Value::as_str)
            .is_none()
        {
            let summary = cardkit_terminal_summary(&card_json)
                .unwrap_or_else(|| summon_i18n::text!("rcf945e21dd").to_string());
            let Some(root) = v.as_object_mut() else {
                return card_json;
            };
            let config = root
                .entry("config")
                .or_insert_with(|| serde_json::json!({}));
            if !config.is_object() {
                *config = serde_json::json!({});
            }
            config["summary"] = serde_json::json!({"content": summary});
        }
        if !peers.is_empty() {
            render_mentions_in_value(&mut v, &peers, &self.alias_map);
        }
        v.to_string()
    }

    /// 交织 live 卡由 core 预渲染，流式阶段逐字节透传，绝不产生真实 `<at>`,也不生成文件按钮。
    /// 终态定格再由 `render_mentions_in_card` 展开正文里的 mention、挂上文件按钮。
    fn render_streaming_card(&self, _chat_tag: &str, card_json: &str) -> String {
        card_json.to_string()
    }

    /// 卡片 JSON 里提到的本地文件:遍历所有 markdown 元素的 content 摘掉文字链接,文件按钮挂在
    /// 卡片末尾。没挂预览服务 / JSON 解析不了 / 不是 2.0 卡(挂不了按钮,摘了链接就只剩文字)
    /// 时原样返回(fail-closed,不破坏既有卡片)。
    fn preview_buttons_in_card(&self, card_json: &str) -> String {
        let Some(previews) = &self.preview_server else {
            return card_json.to_string();
        };
        let Ok(mut v) = serde_json::from_str::<serde_json::Value>(card_json) else {
            return card_json.to_string();
        };
        if !v
            .pointer("/body/elements")
            .is_some_and(serde_json::Value::is_array)
        {
            return card_json.to_string();
        }
        let mut buttons = Vec::new();
        collect_buttons_in_value(&mut v, previews, self.preview_base.as_deref(), &mut buttons);
        summon_core::feishu::card_with_preview_buttons(&v.to_string(), &buttons)
            .unwrap_or_else(|| v.to_string())
    }

    /// 发一张由 markdown 正文渲染成的 2.0 卡(带文件按钮或会话列表预览时走这里),归档正文。
    fn send_rendered_card(
        &self,
        chat: &ChatRef,
        rendered: &RenderedLarkMd,
        card: String,
        operation: &str,
    ) -> anyhow::Result<()> {
        Self::ensure_card_budget(operation, &card)?;
        let message_id =
            self.fc
                .send_card_id_replying(chat.chat_id, self.reply_target_for(chat), &card)?;
        append_outbound(
            chat.agent_dir,
            chat.chat_type,
            chat.chat_id,
            "markdown_card",
            serde_json::json!({"message_id": message_id, "markdown": rendered.as_str()}),
        );
        *self.last_sent.borrow_mut() = Some((message_id, card));
        Ok(())
    }

    fn stream_target(&self, id: StreamId, operation: &str) -> anyhow::Result<(String, String)> {
        self.stream_map
            .borrow()
            .get(&id.0)
            .map(|st| (st.mid.clone(), st.chat_tag.clone()))
            .ok_or_else(|| anyhow::anyhow!(summon_i18n::format!("r7c0cb687a2", id.0)))
    }

    /// 绕节流把卡片生命周期收掉。mention 是否展开由调用方在传入前决定；只有远端 PATCH
    /// 成功后才摘映射，失败保留给上层重试/降级。
    fn settle_stream_card(
        &self,
        id: StreamId,
        mid: &str,
        operation: &str,
        card: &str,
    ) -> anyhow::Result<()> {
        Self::ensure_card_budget(operation, card)?;
        self.fc.update_card(mid, card)?;
        self.stream_map.borrow_mut().remove(&id.0);
        // 当前定格卡是最新投递，终态正文的调用方可将它登记为 Reply。
        *self.last_sent.borrow_mut() = Some((mid.to_string(), card.to_string()));
        // 记下这张刚定格的卡:**所有**定格路径(finish/freeze,卡片版与纯文本版)都收口在这里,
        // 挂一处即全覆盖。存的是渲染后的成品,后续刷子代理尾部时直接在它上面挂区块,不必重渲。
        if let Some(slot) = &self.settled_card {
            if let Ok(mut guard) = slot.lock() {
                *guard = Some(crate::frontend::SettledCard {
                    message_id: mid.to_string(),
                    card_json: card.to_string(),
                    kind: crate::frontend::SettledCardKind::Turn,
                });
            }
        }
        Ok(())
    }

    fn cot_recovery_dir(agent_dir: &std::path::Path) -> std::path::PathBuf {
        agent_dir.join("runtime").join("cot-recovery")
    }

    /// 恢复记录的落点。由 `cot_id` + `message_id` 决定,所以接管轮拿着锚点就能直接找回它。
    fn cot_recovery_path(
        agent_dir: &std::path::Path,
        cot_id: &str,
        message_id: &str,
    ) -> std::path::PathBuf {
        let key = format!("{:x}", md5::compute(format!("{cot_id}:{message_id}")));
        Self::cot_recovery_dir(agent_dir).join(format!("{key}.json"))
    }

    fn persist_cot_recovery(
        agent_dir: &std::path::Path,
        record: &CotRecoveryRecord,
    ) -> anyhow::Result<std::path::PathBuf> {
        std::fs::create_dir_all(Self::cot_recovery_dir(agent_dir))?;
        let path = Self::cot_recovery_path(agent_dir, &record.cot_id, &record.message_id);
        let body = serde_json::to_vec(record)?;
        crate::runtime::fsio::atomic_write_mode(&path, &body, 0o600)?;
        Ok(path)
    }

    /// 开新 CoT 前收掉没有本进程活跃 owner 的远端载体。失败记录保留，下一次开卡继续尝试。
    ///
    /// 用与启动收口同一张 keep 表:同 agent 的另一个 chat 在「启动收口放行」到「接管轮真正
    /// 认领」之间开卡,拿空表扫一遍就把待接管那条收走了 —— 接管轮只能退回新 CoT。
    fn drain_stale_cot_recoveries(&self, agent_dir: &std::path::Path) {
        let keep = self.cot_keep.as_deref();
        finalize_orphan_cot_messages(self.fc, agent_dir, &|tag, cot_id| {
            keep.is_some_and(|keep| keep(tag, cot_id))
        });
    }

    fn publish_cot_recovery(
        agent_dir: &std::path::Path,
        record: &CotRecoveryRecord,
    ) -> anyhow::Result<std::path::PathBuf> {
        let _guard = COT_RECOVERY_DRAIN
            .lock()
            .map_err(|_| anyhow::anyhow!("CoT recovery lifecycle lock poisoned"))?;
        let path = Self::persist_cot_recovery(agent_dir, record)?;
        active_cot_recoveries()
            .lock()
            .map_err(|_| anyhow::anyhow!("CoT recovery active set poisoned"))?
            .insert(path.clone());
        Ok(path)
    }

    fn retire_cot_recovery(path: &std::path::Path, remove_file: bool) {
        let Ok(_guard) = COT_RECOVERY_DRAIN.lock() else {
            return;
        };
        if let Ok(mut active) = active_cot_recoveries().lock() {
            active.remove(path);
        }
        if remove_file {
            let _ = std::fs::remove_file(path);
        }
    }

    fn rewrite_cot_recovery(path: &std::path::Path, record: &CotRecoveryRecord) {
        let Ok(_guard) = COT_RECOVERY_DRAIN.lock() else {
            return;
        };
        if let Ok(body) = serde_json::to_vec(record) {
            let _ = crate::runtime::fsio::atomic_write_mode(path, &body, 0o600);
        }
    }

    fn cot_recovery_path_is_active(path: &std::path::Path, generation: &str) -> bool {
        if generation != cot_process_generation() {
            return false;
        }
        active_cot_recoveries()
            .lock()
            .map_or(true, |active| active.contains(path))
    }

    fn cot_route(&self, chat: &ChatRef) -> CotRoute {
        CotRoute {
            chat_id: chat.chat_id.to_string(),
            reply_to: self
                .reply_target_for(chat)
                .map(|(id, thread)| (id.to_string(), thread)),
        }
    }

    fn cot_begin(
        &self,
        chat: &ChatRef,
        view: &summon_core::cot::CotView,
    ) -> Result<StreamId, CotBeginError> {
        self.drain_stale_cot_recoveries(chat.agent_dir);
        let handle = self
            .fc
            .cot_create(chat.chat_id, self.reply_target_for(chat))
            .map_err(CotBeginError::BeforeCreate)?;
        let tag = chat_tag(chat.chat_type, chat.chat_id);
        let recovery = CotRecoveryRecord {
            generation: cot_process_generation().to_string(),
            cot_id: handle.cot_id.clone(),
            message_id: handle.message_id.clone(),
            legacy_control_mid: None,
            cot_closed: false,
            append_failed: false,
            chat_tag: Some(tag.clone()),
            projector: None,
            page: None,
        };
        let recovery_path = match Self::publish_cot_recovery(chat.agent_dir, &recovery) {
            Ok(path) => Some(path),
            Err(error) => {
                crate::observe::log::log(&summon_i18n::format!("r80ced55ad0"));
                None
            }
        };
        let mut projector = summon_core::cot::CotProjector::new(
            chat.chat_id,
            view.progress
                .map(summon_core::progress::TurnProgress::turn_id)
                .unwrap_or(""),
            view.query.unwrap_or(summon_i18n::text!("r17ee27d0c3")),
        );
        let initial = projector.project_with_replies(
            view.segments,
            view.released_replies,
            view.progress,
            &Self::unix_ms().to_string(),
        );
        *self.next_id.borrow_mut() += 1;
        let id = StreamId(*self.next_id.borrow());
        self.stream_map.borrow_mut().insert(
            id.0,
            StreamState {
                mid: handle.message_id.clone(),
                chat_tag: tag,
                throttle: PatchThrottle::primed(COT_UPDATE_MIN_INTERVAL_MS, self.now_ms()),
                with_interrupt: true,
                transport: StreamTransport::Cot(Box::new(CotState {
                    carrier: CotCarrier::new(
                        handle,
                        recovery.chat_tag.unwrap(),
                        recovery_path,
                        Some(self.cot_route(chat)),
                    ),
                    projector,
                    pending: initial,
                    confirmed_replies: Vec::new(),
                    next_batch: 0,
                    append_failed: false,
                })),
            },
        );
        if let Err(error) = self.flush_cot(id) {
            let mut map = self.stream_map.borrow_mut();
            if let Some(StreamState {
                transport: StreamTransport::Cot(state),
                ..
            }) = map.get_mut(&id.0)
            {
                let _ = state
                    .carrier
                    .complete(self.fc, super::client::CotCompleteReason::Error);
            }
            crate::observe::log::log(&summon_i18n::format!("rd8617696d4"));
        }
        Ok(id)
    }

    fn cot_update(
        &self,
        id: StreamId,
        view: &summon_core::cot::CotView,
    ) -> anyhow::Result<crate::frontend::StreamPatchOutcome> {
        let timestamp = Self::unix_ms().to_string();
        let now = self.now_ms();
        {
            let mut map = self.stream_map.borrow_mut();
            let Some(st) = map.get_mut(&id.0) else {
                return Ok(crate::frontend::StreamPatchOutcome::MissingStream);
            };
            let StreamTransport::Cot(state) = &mut st.transport else {
                return Ok(crate::frontend::StreamPatchOutcome::MissingStream);
            };
            if state.append_failed {
                return Ok(crate::frontend::StreamPatchOutcome::Throttled);
            }
            state.pending.extend(state.projector.project_with_replies(
                view.segments,
                view.released_replies,
                view.progress,
                &timestamp,
            ));
            if state.pending.is_empty() || !st.throttle.admit(now) {
                return Ok(crate::frontend::StreamPatchOutcome::Throttled);
            }
        }
        self.flush_cot(id)?;
        Ok(crate::frontend::StreamPatchOutcome::Applied)
    }

    /// 清空一条 CoT 的待发事件。单次 API 最多 50 条；同步前端天然提供单写者与严格顺序。
    fn flush_cot(&self, id: StreamId) -> anyhow::Result<()> {
        let mut map = self.stream_map.borrow_mut();
        let stream = map
            .get_mut(&id.0)
            .ok_or_else(|| anyhow::anyhow!(summon_i18n::format!("rdbea37ab84", id.0)))?;
        let StreamTransport::Cot(state) = &mut stream.transport else {
            anyhow::bail!(summon_i18n::format!("r7ae9ffdde8", id.0));
        };
        if state.append_failed {
            state.pending.clear();
            anyhow::bail!(summon_i18n::text!("r5fb868cc78"));
        }
        let events = std::mem::take(&mut state.pending);
        let CotState {
            carrier,
            confirmed_replies,
            next_batch,
            ..
        } = state.as_mut();
        let root_id = carrier.cot_id.clone();
        let result = carrier.push_with(self.fc, &events, |batch| {
            confirm_reply_events(confirmed_replies, batch);
            log_cot_batch(
                &stream.chat_tag,
                &root_id,
                *next_batch,
                batch,
                confirmed_replies,
                true,
            );
            *next_batch += 1;
        });
        if let Err(error) = result {
            state.append_failed = true;
            log_cot_batch(
                &stream.chat_tag,
                &root_id,
                state.next_batch,
                &events,
                &state.confirmed_replies,
                false,
            );
            return Err(error);
        }
        state.carrier.snapshot(&state.projector);
        Ok(())
    }

    fn settle_cot(&self, id: StreamId, terminal: CotTerminal<'_>) -> anyhow::Result<()> {
        let timestamp = Self::unix_ms().to_string();
        let fallback_reason = match terminal {
            CotTerminal::Finished("done") => super::client::CotCompleteReason::Done,
            CotTerminal::Finished(_) | CotTerminal::SessionEnded | CotTerminal::Error(_) => {
                super::client::CotCompleteReason::Error
            }
            CotTerminal::Timeout => super::client::CotCompleteReason::Timeout,
        };
        let append_failed = {
            let mut map = self.stream_map.borrow_mut();
            let Some(StreamTransport::Cot(state)) =
                map.get_mut(&id.0).map(|stream| &mut stream.transport)
            else {
                anyhow::bail!(summon_i18n::format!("r23d1960032", id.0));
            };
            let events = match terminal {
                CotTerminal::Finished(status) => state.projector.finish(status, &timestamp),
                CotTerminal::SessionEnded => state.projector.finish_session(&timestamp),
                CotTerminal::Error(message) => {
                    state.projector.error(message, "PROVIDER_ERROR", &timestamp)
                }
                CotTerminal::Timeout => {
                    state
                        .projector
                        .error(summon_i18n::text!("rfea81a75a4"), "TIMEOUT", &timestamp)
                }
            };
            state.pending.extend(events);
            state.append_failed
        };
        let push_error = if append_failed {
            None
        } else {
            self.flush_cot(id).err()
        };
        if append_failed || push_error.is_some() {
            let mut map = self.stream_map.borrow_mut();
            let Some(StreamTransport::Cot(state)) =
                map.get_mut(&id.0).map(|stream| &mut stream.transport)
            else {
                anyhow::bail!(summon_i18n::format!("r23d1960032", id.0));
            };
            if let Err(complete_error) = state.carrier.complete(self.fc, fallback_reason) {
                let push_error = push_error
                    .as_ref()
                    .map(ToString::to_string)
                    .unwrap_or_default();
                anyhow::bail!(summon_i18n::format!("r0f4aa5d304"));
            }
        }
        let stream = self
            .stream_map
            .borrow_mut()
            .remove(&id.0)
            .ok_or_else(|| anyhow::anyhow!(summon_i18n::format!("rf78d330e3c", id.0)))?;
        let StreamTransport::Cot(state) = stream.transport else {
            anyhow::bail!(summon_i18n::format!("rb5910c2dda", id.0));
        };
        state.carrier.release(true);
        Ok(())
    }

    /// 接管上一代那条 CoT:从恢复记录里的投影器快照续投,首批带一个段边界。
    ///
    /// 身份、快照和段落证据成立时保留同一载体；追加结果不确定时仅允许显式收口。
    fn cot_resume(
        &self,
        chat: &ChatRef,
        view: &summon_core::cot::CotView,
        cot: &summon_core::TurnAnchorCot,
    ) -> Result<StreamId, CotBeginError> {
        let path = Self::cot_recovery_path(chat.agent_dir, &cot.cot_id, &cot.message_id);
        if Self::read_cot_recovery(&path).is_some_and(|record| {
            record.cot_id == cot.cot_id
                && record.message_id == cot.message_id
                && (record.append_failed || record.page.is_some_and(|page| page.creation_pending))
        }) {
            return Err(CotBeginError::BeforeCreate(anyhow::anyhow!(
                summon_i18n::text!("cot.delivery_uncertain")
            )));
        }
        match self.cot_resume_claimed(chat, view, cot) {
            Some(id) => Ok(id),
            None => {
                self.close_reattached_cot(chat.agent_dir, cot);
                self.cot_begin(chat, view)
            }
        }
    }

    /// 先认领记录再推事件，确保同代反熵扫描尊重本轮的输出所有权。
    fn cot_resume_claimed(
        &self,
        chat: &ChatRef,
        view: &summon_core::cot::CotView,
        cot: &summon_core::TurnAnchorCot,
    ) -> Option<StreamId> {
        let path = Self::cot_recovery_path(chat.agent_dir, &cot.cot_id, &cot.message_id);
        let record = Self::read_cot_recovery(&path)
            .filter(|record| record.cot_id == cot.cot_id && record.message_id == cot.message_id)
            .filter(|record| !record.cot_closed && !record.append_failed)?;
        let mut projector = record.projector?;
        if view.segments.len() < projector.segment_cursor() {
            return None;
        }
        let pristine = projector.clone();
        let tag = chat_tag(chat.chat_type, chat.chat_id);
        let timestamp = Self::unix_ms().to_string();
        let mut pending = if self.cot_content_visible {
            projector.resume_boundary(
                ::summon_core::i18n::label(summon_core::cot::CotProjector::RESUME_BOUNDARY_NOTE),
                &timestamp,
            )
        } else {
            Vec::new()
        };
        pending.extend(projector.project_with_replies(
            view.segments,
            view.released_replies,
            view.progress,
            &timestamp,
        ));
        let handle = super::client::CotHandle {
            cot_id: cot.cot_id.clone(),
            message_id: cot.message_id.clone(),
        };
        let mut carrier = CotCarrier::new(handle, tag.clone(), None, Some(self.cot_route(chat)));
        carrier.page = record.page.unwrap_or_else(|| CotPage {
            cot_id: cot.cot_id.clone(),
            message_id: cot.message_id.clone(),
            closed: false,
            creation_pending: false,
            wire: pristine.unknown_wire_state(),
        });
        if carrier.page.closed {
            return None;
        }
        let claimed = CotRecoveryRecord {
            generation: cot_process_generation().to_string(),
            cot_id: cot.cot_id.clone(),
            message_id: cot.message_id.clone(),
            legacy_control_mid: None,
            cot_closed: false,
            append_failed: false,
            chat_tag: Some(tag.clone()),
            projector: Some(pristine.clone()),
            page: Some(carrier.page.clone()),
        };
        let path = Self::publish_cot_recovery(chat.agent_dir, &claimed).ok()?;
        carrier.recovery_path = Some(path);
        *self.next_id.borrow_mut() += 1;
        let id = StreamId(*self.next_id.borrow());
        self.stream_map.borrow_mut().insert(
            id.0,
            StreamState {
                mid: cot.message_id.clone(),
                chat_tag: tag.clone(),
                throttle: PatchThrottle::primed(COT_UPDATE_MIN_INTERVAL_MS, self.now_ms()),
                with_interrupt: true,
                transport: StreamTransport::Cot(Box::new(CotState {
                    carrier,
                    confirmed_replies: pristine.projected_replies(),
                    next_batch: 0,
                    projector,
                    pending,
                    append_failed: false,
                })),
            },
        );
        if let Err(error) = self.flush_cot(id) {
            crate::observe::log::log(&summon_i18n::format!("r8dbbcbf72e", cot.cot_id));
        }
        Some(id)
    }

    /// 续接不成立时收掉旧载体:先说明一句为什么停在这儿,再 `done` 收口、删记录。
    fn close_reattached_cot(&self, agent_dir: &std::path::Path, cot: &summon_core::TurnAnchorCot) {
        let path = Self::cot_recovery_path(agent_dir, &cot.cot_id, &cot.message_id);
        let Some(record) = Self::read_cot_recovery(&path) else {
            return;
        };
        // 收口失败就把记录留着:下一趟反熵扫描会再试一次。删掉等于这条 CoT 永远转下去。
        let closed = record.cot_closed || close_orphan_cot(self.fc, &record);
        Self::retire_cot_recovery(&path, closed);
    }

    fn read_cot_recovery(path: &std::path::Path) -> Option<CotRecoveryRecord> {
        serde_json::from_slice(&std::fs::read(path).ok()?).ok()
    }

    fn is_cot(&self, id: StreamId) -> bool {
        matches!(
            self.stream_map
                .borrow()
                .get(&id.0)
                .map(|state| &state.transport),
            Some(StreamTransport::Cot(_))
        )
    }
}

/// daemon 启动与开卡前共用的 CoT 反熵。生命周期锁把“发布文件+登记 owner”与扫描串行化，
/// 同代记录只有不在 active set 时才可回收；跨代记录天然没有本进程 owner。
/// `keep(chat_tag, cot_id)` = 这条 CoT 正被接管轮认领,一个字节都不发、也不删记录。
pub(crate) fn finalize_orphan_cot_messages(
    fc: &FeishuClient,
    agent_dir: &std::path::Path,
    keep: &dyn Fn(&str, &str) -> bool,
) {
    let Ok(_guard) = COT_RECOVERY_DRAIN.lock() else {
        return;
    };
    let dir = FeishuFrontend::cot_recovery_dir(agent_dir);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(raw) = std::fs::read(&path) else {
            continue;
        };
        let Ok(mut record) = serde_json::from_slice::<CotRecoveryRecord>(&raw) else {
            continue;
        };
        if FeishuFrontend::cot_recovery_path_is_active(&path, &record.generation) {
            continue;
        }
        // 接管轮要在这条上续跑:关掉就等于把它的过程账本提前封了口。
        if record
            .chat_tag
            .as_deref()
            .is_some_and(|tag| keep(tag, &record.cot_id))
        {
            continue;
        }
        if !record.cot_closed {
            if !close_orphan_cot(fc, &record) {
                continue;
            }
            record.cot_closed = true;
            if let Ok(body) = serde_json::to_vec(&record) {
                let _ = crate::runtime::fsio::atomic_write_mode(&path, &body, 0o600);
            }
        }
        if let Some(mid) = record.legacy_control_mid.as_deref() {
            if fc.recall_message(mid).is_err() {
                continue;
            }
        }
        let _ = std::fs::remove_file(path);
    }
}

/// 收掉一条没人认领的 CoT:先推一句「为什么停在这儿」,再 `done` 收口。
///
/// 收口理由是 `done` 不是 `error` —— `error` 是给「`RUN_FINISHED` 写不进去」准备的,停机不是
/// 错误。说明句只在有快照时推得出(投影器知道当前段开在哪),推失败也不影响收口:
/// 收口才是必须做到的那一步。返回值 = 收口成功与否。
fn close_orphan_cot(fc: &FeishuClient, record: &CotRecoveryRecord) -> bool {
    let (cot_id, message_id) = match &record.page {
        Some(page) => {
            if page.creation_pending {
                return false;
            }
            if page.closed {
                return true;
            }
            (&page.cot_id, &page.message_id)
        }
        None => (&record.cot_id, &record.message_id),
    };
    if let Some(mut projector) = record.projector.clone().filter(|_| !record.append_failed) {
        let events = projector.closing_note(
            ::summon_core::i18n::label(summon_core::cot::CotProjector::SHUTDOWN_CLOSE_NOTE),
            &FeishuFrontend::unix_ms().to_string(),
        );
        if let Some(page) = &record.page {
            let (batch, consumed, _) = page.wire.batch(&events);
            if consumed == events.len() && !batch.is_empty() {
                let _ = fc.cot_push(cot_id, message_id, &batch);
            }
        } else {
            for batch in events.chunks(COT_EVENTS_PER_REQUEST) {
                if fc.cot_push(cot_id, message_id, batch).is_err() {
                    break;
                }
            }
        }
    }
    fc.cot_complete(cot_id, message_id, super::client::CotCompleteReason::Done)
        .is_ok()
}

impl Drop for FeishuFrontend<'_> {
    fn drop(&mut self) {
        let Ok(_guard) = COT_RECOVERY_DRAIN.lock() else {
            return;
        };
        let Ok(mut active) = active_cot_recoveries().lock() else {
            return;
        };
        for stream in self.stream_map.get_mut().values() {
            if let StreamTransport::Cot(state) = &stream.transport {
                if let Some(path) = state.carrier.recovery_path.as_ref() {
                    active.remove(path);
                }
            }
        }
    }
}

fn cardkit_terminal_summary(card_json: &str) -> Option<String> {
    fn first_markdown_content(value: &serde_json::Value) -> Option<&str> {
        match value {
            serde_json::Value::Object(map) => {
                if map.get("tag").and_then(serde_json::Value::as_str) == Some("markdown") {
                    let is_badge = map.get("text_size").and_then(serde_json::Value::as_str)
                        == Some("notation");
                    if is_badge {
                        return None;
                    }
                    return map
                        .get("content")
                        .and_then(serde_json::Value::as_str)
                        .filter(|content| !content.trim().is_empty());
                }
                map.values().find_map(first_markdown_content)
            }
            serde_json::Value::Array(items) => items.iter().find_map(first_markdown_content),
            _ => None,
        }
    }

    let card: serde_json::Value =
        serde_json::from_str(card_json).unwrap_or_else(|_| serde_json::json!({}));
    let source = card["config"]["summary"]["content"]
        .as_str()
        .filter(|content| !content.trim().is_empty())
        .or_else(|| first_markdown_content(&card["body"]["elements"]));
    source
        .and_then(summon_core::feishu::feed_summary)
        .or_else(|| Some(summon_i18n::text!("rcf945e21dd").to_string()))
}

/// 递归遍历卡片 JSON value,对所有 `{"tag": "markdown", "content": "..."}` 对象的
/// content 字段做 @mention 替换。collapsible_panel 里嵌套的 markdown 也会被覆盖。
fn render_mentions_in_value(
    v: &mut serde_json::Value,
    peers: &[summon_core::peers::Peer],
    alias_map: &std::collections::HashMap<String, String>,
) {
    use serde_json::Value;
    match v {
        Value::Object(map) => {
            if let Some(Value::String(tag)) = map.get("tag") {
                // **引述性质的区块不解析 @** —— 折叠面板装的是思考/工具过程,那里出现的
                // `@某bot` 是 agent 在**谈论**它,不是在**呼叫**它。渲染成真实 `<at>` 会把
                // 对方 app 真的触发一次:2026-08-12 owner 实测到 bot 之间靠这个来回互相唤起
                // (agent 甲 @ agent 乙请它 review → 乙的思考卡被镜像回群 → 里面的 @ 又触发甲)。
                // 整个子树跳过(含 header 标题),不再往下递归。
                if tag == "collapsible_panel" {
                    return;
                }
                if tag == "markdown" {
                    if let Some(Value::String(content)) = map.get_mut("content") {
                        // 底注/上下文行(「回复于 …· 关于:X」)同理:它复述的是**别人**说过的话,
                        // 里面的 @ 属于引文,不该二次呼叫。判据用渲染模板的灰字前缀 —— 这一行是
                        // 本仓自己拼的固定模板(见 `card_turn_summary_pieces`),不是外部文本。
                        if !content.starts_with("<font color='grey'>") {
                            *content = render_outbound_mentions(content, peers, alias_map);
                        }
                    }
                }
            }
            for val in map.values_mut() {
                render_mentions_in_value(val, peers, alias_map);
            }
        }
        Value::Array(arr) => {
            for item in arr.iter_mut() {
                render_mentions_in_value(item, peers, alias_map);
            }
        }
        _ => {}
    }
}

/// 递归遍历卡片 JSON value,对所有 `{"tag": "markdown", "content": "..."}` 对象收集提到的本地
/// 文件(按出现顺序去重)并摘掉文字链接。与 [`render_mentions_in_value`] 不同:**不**跳过
/// collapsible_panel —— 思考/工具过程里出现的文件同样要有打开入口。
fn collect_buttons_in_value(
    v: &mut serde_json::Value,
    previews: &crate::runtime::preview_server::PreviewServer,
    base: Option<&str>,
    buttons: &mut Vec<PreviewButton>,
) {
    use serde_json::Value;
    match v {
        Value::Object(map) => {
            if let Some(Value::String(tag)) = map.get("tag") {
                if tag == "markdown" {
                    if let Some(Value::String(content)) = map.get_mut("content") {
                        let (text, found) = previews.link_buttons(content, base);
                        *content = text;
                        for button in found {
                            if !buttons.iter().any(|known| known.token == button.token) {
                                buttons.push(button);
                            }
                        }
                    }
                }
            }
            for val in map.values_mut() {
                collect_buttons_in_value(val, previews, base, buttons);
            }
        }
        Value::Array(arr) => {
            for item in arr.iter_mut() {
                collect_buttons_in_value(item, previews, base, buttons);
            }
        }
        _ => {}
    }
}

impl Frontend for FeishuFrontend<'_> {
    fn stage_result_body(&self, body: &str) {
        if let Some(slot) = &self.final_reply {
            if let Ok(mut reply) = slot.lock() {
                *reply = Some(body.to_string());
            }
        }
    }

    fn send_message(&self, chat: &ChatRef, body: &str) -> anyhow::Result<()> {
        // 出站 @mention 替换:把 @name/@alias 换成 <at user_id="ou_xxx">名字</at>。
        let tag = chat_tag(chat.chat_type, chat.chat_id);
        let (rendered, buttons) = self.render_reply(&tag, body);
        let card = summon_core::feishu::card_markdown_v2_rendered(&rendered);
        if !summon_core::feishu::rendered_final_reply_fits(&rendered) {
            anyhow::bail!(summon_i18n::format!(
                "r7bd7c73ad1",
                rendered.as_str().len(),
                card.len(),
                summon_core::feishu::CARD_TEXT_SAFE_LIMIT,
                summon_core::feishu::CARD_TOTAL_SAFE_LIMIT,
            ));
        }
        if !buttons.is_empty() {
            let card =
                summon_core::feishu::card_with_preview_buttons(&card, &buttons).unwrap_or(card);
            return self.send_rendered_card(chat, &rendered, card, "send_message");
        }
        // 错误回执 / 兜底回复(纯文本)→ 无 header markdown 卡片。marker 子系统已删,不再有图文混合。
        let message_id = super::outbound::send_rendered_markdown_archived_id(
            self.fc,
            chat.agent_dir,
            chat.chat_type,
            chat.chat_id,
            self.reply_target_for(chat),
            &rendered,
        )?;
        *self.last_sent.borrow_mut() = Some((message_id, card));
        Ok(())
    }

    /// 会话列表预览只认 2.0 卡的 `config.summary`,所以这一条发成 2.0 markdown 卡,正文渲染与
    /// [`Self::send_message`] 同一份(mention 展开、转义、单卡预算)。
    fn send_message_with_feed(&self, chat: &ChatRef, body: &str, feed: &str) -> anyhow::Result<()> {
        let tag = chat_tag(chat.chat_type, chat.chat_id);
        let (rendered, buttons) = self.render_reply(&tag, body);
        if !summon_core::feishu::rendered_final_reply_fits(&rendered) {
            anyhow::bail!(summon_i18n::format!(
                "r2dc8577962",
                rendered.as_str().len(),
                summon_core::feishu::CARD_TEXT_SAFE_LIMIT,
            ));
        }
        let card = summon_core::feishu::card_with_feed_summary(
            &summon_core::feishu::card_markdown_v2_rendered(&rendered),
            feed,
        );
        let card = summon_core::feishu::card_with_preview_buttons(&card, &buttons).unwrap_or(card);
        self.send_rendered_card(chat, &rendered, card, "send_message_with_feed")
    }

    fn send_process_supplement(
        &self,
        chat: &ChatRef,
        card: &str,
        uuid: &str,
    ) -> anyhow::Result<()> {
        Self::ensure_card_budget("process_supplement", card)?;
        super::outbound::send_process_supplement_archived(self.fc, chat, card, uuid)
    }

    fn split_final_reply(&self, chat: &ChatRef, body: &str) -> Vec<String> {
        let tag = chat_tag(chat.chat_type, chat.chat_id);
        // peers/身份缓存只读一次；二分探测每个候选切点时复用同一快照，避免 O(log n) 次磁盘读取，
        // 也保证整次分段看到一致的 mention 集合。
        let peers = self.merged_peers(&tag);
        summon_core::feishu::split_for_card_by_budget(body, |segment| {
            let rendered = render_external_lark_md(segment, &peers, &self.alias_map);
            summon_core::feishu::rendered_final_reply_fits(&rendered)
        })
    }

    /// 解析正文里的 `![](本地路径)` 并逐张上传,返回上传成功的 `path → image_key`。
    ///
    /// **绝大多数回复没有图**:`parse_inline_images` 扫一遍返回空,这里立刻短路,不碰网络。
    /// 任何一张失败(文件不存在 / 格式不对 / 超限 / 上传报错)只 log 并跳过——那一处会退化成原样
    /// 文本,回复照发。**不允许因为图而让文字发不出去。**
    fn resolve_inline_images(
        &self,
        _chat: &ChatRef,
        body: &str,
    ) -> std::collections::BTreeMap<String, String> {
        let mut out = std::collections::BTreeMap::new();
        for image in summon_core::image::parse_inline_images(body) {
            if out.contains_key(&image.path) {
                continue; // 同一张图被引用多次:只传一次
            }
            match upload_local_image(self.fc, &image.path) {
                Ok(key) => {
                    out.insert(image.path, key);
                }
                Err(e) => {
                    crate::observe::log::log(&summon_i18n::format!("rfd806c3ec8", image.path))
                }
            }
        }
        out
    }

    fn stream_begin(
        &self,
        chat: &ChatRef,
        content: &str,
        with_interrupt: bool,
    ) -> anyhow::Result<StreamId> {
        // live 快照只转义、不展开 @mention；真实提及留到 stream_finish 终态对账。
        let tag = chat_tag(chat.chat_type, chat.chat_id);
        let rendered = self.render_streaming_text(&tag, content);
        // 任务4:正在输入的流式卡内嵌「⏸ 打断」按钮(with_interrupt);否则纯 2.0 markdown。整条流式
        // 生命周期统一 2.0(begin/patch/finish 同 schema,飞书 PATCH 不允许 v1↔v2 换)。
        let card = if with_interrupt {
            summon_core::feishu::card_stream_with_interrupt_rendered(&rendered)
        } else {
            summon_core::feishu::card_markdown_v2_rendered(&rendered)
        };
        if !with_interrupt && !summon_core::feishu::rendered_final_reply_fits(&rendered) {
            anyhow::bail!(summon_i18n::format!("ree14152220"));
        }
        Self::ensure_card_budget("stream_begin", &card)?;
        let mid = self
            .fc
            .send_card_id_anchored(chat.chat_id, chat.thread_anchor, &card)?;
        *self.next_id.borrow_mut() += 1;
        let id = *self.next_id.borrow();
        // primed:begin 本身已对该消息写过一次,首个 patch 也等满窗口。
        let st = StreamState {
            mid,
            chat_tag: tag,
            throttle: PatchThrottle::primed(STREAM_PATCH_MIN_INTERVAL_MS, self.now_ms()),
            with_interrupt,
            transport: StreamTransport::Text,
        };
        self.stream_map.borrow_mut().insert(id, st);
        Ok(StreamId(id))
    }

    fn stream_patch(&self, id: StreamId, content: &str) -> anyhow::Result<()> {
        // StreamId 必有对应 message_id(stream_begin 才发的);查不到说明逻辑错位,忽略(不中断本轮)。
        let (mid, chat_tag_str, with_interrupt) = {
            let mut map = self.stream_map.borrow_mut();
            let Some(st) = map.get_mut(&id.0) else {
                return Ok(());
            };
            // 节流窗口内跳过(Ok 而非 Err:不是失败):内容是累积全文,后续 patch / 终态对账会带上。
            if !st.throttle.admit(self.now_ms()) {
                return Ok(());
            }
            (st.mid.clone(), st.chat_tag.clone(), st.with_interrupt)
        };
        // live patch 只转义、不展开 @mention；即使本块已经是正文也仍不是终态。
        let rendered = self.render_streaming_text(&chat_tag_str, content);
        // U2:流式累积超过单卡上限 → 截到上限并带「后续另卡」提示,让打字机优雅停在上限附近
        // 而不是 PATCH 被 API 整体拒绝;尾部由终态分段补卡送达。
        let body = rendered.clipped_for_stream(summon_core::feishu::CARD_TEXT_SAFE_LIMIT);
        // 按钮持续在(沿用 begin 时的 with_interrupt),正文随 patch 更新。
        let card = if with_interrupt {
            summon_core::feishu::card_stream_with_interrupt_rendered(&body)
        } else {
            summon_core::feishu::card_markdown_v2_rendered(&body)
        };
        Self::ensure_card_budget("stream_patch", &card)?;
        self.fc.update_card(&mid, &card)
    }

    fn stream_begin_card(&self, chat: &ChatRef, card_json: &str) -> anyhow::Result<StreamId> {
        // 交织 live 卡首张逐字节透传，不展开 @mention；只建立映射 + 节流器。
        let tag = chat_tag(chat.chat_type, chat.chat_id);
        let rendered = self.render_streaming_card(&tag, card_json);
        Self::ensure_card_budget("stream_begin_card", &rendered)?;
        let mid = self
            .fc
            .send_card_id_anchored(chat.chat_id, chat.thread_anchor, &rendered)?;
        *self.next_id.borrow_mut() += 1;
        let id = *self.next_id.borrow();
        let st = StreamState {
            mid,
            chat_tag: tag,
            throttle: PatchThrottle::primed(STREAM_PATCH_MIN_INTERVAL_MS, self.now_ms()),
            with_interrupt: true,
            transport: StreamTransport::Text,
        };
        self.stream_map.borrow_mut().insert(id, st);
        Ok(StreamId(id))
    }

    /// live 卡开卡。原生 CoT 是唯一载体，创建失败即本轮没有过程展示——不降级、也不在后台重试轰群。
    fn live_begin(
        &self,
        chat: &ChatRef,
        view: &summon_core::cot::CotView,
    ) -> anyhow::Result<StreamId> {
        self.cot_begin(chat, view)
            .map_err(|CotBeginError::BeforeCreate(error)| {
                error.context(summon_i18n::text!("r1328593d56"))
            })
    }

    /// 接管轮起流:上一代那条 CoT 还开着就在同一条里续。
    fn stream_begin_reattached(
        &self,
        chat: &ChatRef,
        view: &summon_core::cot::CotView,
        cot: &summon_core::TurnAnchorCot,
    ) -> anyhow::Result<StreamId> {
        self.cot_resume(chat, view, cot)
            .map_err(|CotBeginError::BeforeCreate(error)| {
                error.context(summon_i18n::text!("r1328593d56"))
            })
    }

    fn stream_cot_handle(&self, id: StreamId) -> Option<(String, String)> {
        let map = self.stream_map.borrow();
        let state = map.get(&id.0)?;
        let StreamTransport::Cot(cot) = &state.transport else {
            return None;
        };
        Some((cot.carrier.cot_id.clone(), state.mid.clone()))
    }

    /// live 卡刷卡。
    fn live_update(
        &self,
        id: StreamId,
        view: &summon_core::cot::CotView,
    ) -> anyhow::Result<crate::frontend::StreamPatchOutcome> {
        self.cot_update(id, view)
    }

    /// CoT 只承载过程:事件流以 `RUN_FINISHED` 收口,装不下最终回复,那必须是一条独立消息。
    fn stream_is_process_only(&self, id: StreamId) -> bool {
        self.is_cot(id)
    }

    fn stream_flush(&self, id: StreamId) -> anyhow::Result<()> {
        if self.is_cot(id) {
            self.flush_cot(id)
        } else {
            Ok(())
        }
    }

    fn stream_confirmed_replies(
        &self,
        id: StreamId,
    ) -> Option<Vec<summon_core::cot::ReplyDelivery>> {
        let map = self.stream_map.borrow();
        let StreamTransport::Cot(state) = &map.get(&id.0)?.transport else {
            return None;
        };
        Some(
            state
                .confirmed_replies
                .iter()
                .copied()
                .filter(|reply| reply.generation == state.projector.reply_generation())
                .collect(),
        )
    }

    fn stream_append_failed(&self, id: StreamId) -> bool {
        matches!(
            self.stream_map.borrow().get(&id.0).map(|stream| &stream.transport),
            Some(StreamTransport::Cot(state)) if state.append_failed
        )
    }

    /// 摘出前先把待发事件推出去:摘出后只有持有者在写,留在这里的事件没人发。推送失败(或
    /// 早已失败)的载体处在不确定终态,不摘,照常由本轮收口。恢复记录保持 active:本轮前端
    /// 析构与下一轮开卡前的反熵都不收它。
    fn stream_detach_cot(&self, id: StreamId) -> Option<crate::frontend::DetachedCot> {
        if !self.is_cot(id) || self.flush_cot(id).is_err() {
            return None;
        }
        let stream = self.stream_map.borrow_mut().remove(&id.0)?;
        let StreamTransport::Cot(state) = stream.transport else {
            return None;
        };
        Some(DetachedCot {
            carrier: state.carrier,
            projector: state.projector,
        })
    }

    fn stream_steer_boundary(&self, id: StreamId, note: &str) -> anyhow::Result<()> {
        // 思考内容不外显时,段边界连同思考一起不渲 —— 用户看不到过程,给他一条孤零零的
        // 「收到追加」分隔线只是噪音。
        if !self.cot_content_visible {
            return Ok(());
        }
        let timestamp = Self::unix_ms().to_string();
        let mut map = self.stream_map.borrow_mut();
        let Some(StreamTransport::Cot(state)) =
            map.get_mut(&id.0).map(|stream| &mut stream.transport)
        else {
            return Ok(());
        };
        if state.append_failed {
            return Ok(());
        }
        state
            .pending
            .extend(state.projector.steer_boundary(note, &timestamp));
        Ok(())
    }

    fn stream_restart_boundary(&self, id: StreamId, note: &str) -> anyhow::Result<()> {
        let timestamp = Self::unix_ms().to_string();
        let mut map = self.stream_map.borrow_mut();
        let Some(StreamTransport::Cot(state)) =
            map.get_mut(&id.0).map(|stream| &mut stream.transport)
        else {
            return Ok(());
        };
        if state.append_failed {
            state.projector.restart_segments();
            return Ok(());
        }
        // 游标换代与外不外显无关:不归零,下一代的前若干段会被当成已推过。
        if self.cot_content_visible {
            state
                .pending
                .extend(state.projector.restart_boundary(note, &timestamp));
        } else {
            state.projector.restart_segments();
        }
        Ok(())
    }

    fn stream_fail(&self, id: StreamId, content: &str) -> anyhow::Result<()> {
        if self.is_cot(id) {
            return self.settle_cot(id, CotTerminal::Error(content));
        }
        self.stream_freeze(id, content)
    }

    fn stream_timeout(&self, id: StreamId, content: &str) -> anyhow::Result<()> {
        if self.is_cot(id) {
            return self.settle_cot(id, CotTerminal::Timeout);
        }
        self.stream_fail(id, content)
    }

    fn stream_patch_card(&self, id: StreamId, card_json: &str) -> anyhow::Result<()> {
        self.stream_patch_card_observed(id, card_json).map(|_| ())
    }

    fn stream_patch_card_observed(
        &self,
        id: StreamId,
        card_json: &str,
    ) -> anyhow::Result<crate::frontend::StreamPatchOutcome> {
        // 交织 live 卡原地刷:流式阶段不展开 @mention，节流后 PATCH。
        let (mid, chat_tag_str) = {
            let mut map = self.stream_map.borrow_mut();
            let Some(st) = map.get_mut(&id.0) else {
                return Ok(crate::frontend::StreamPatchOutcome::MissingStream);
            };
            if !st.throttle.admit(self.now_ms()) {
                return Ok(crate::frontend::StreamPatchOutcome::Throttled);
            }
            (st.mid.clone(), st.chat_tag.clone())
        };
        let rendered = self.render_streaming_card(&chat_tag_str, card_json);
        Self::ensure_card_budget("stream_patch_card", &rendered)?;
        self.fc.update_card(&mid, &rendered)?;
        Ok(crate::frontend::StreamPatchOutcome::Applied)
    }

    fn stream_finish(&self, id: StreamId, content: &str) -> anyhow::Result<()> {
        // 终态对账:绕节流必发(429 退避重试在 update_card 内)。终态**去掉按钮**(turn 收尾)、仍 2.0。
        let (mid, chat_tag_str) = self.stream_target(id, "stream_finish")?;
        if self.is_cot(id) {
            return self.settle_cot(id, CotTerminal::Finished("done"));
        }
        // 出站 @mention 替换(终态对账的文本是权威全文,必须渲染),文件按钮也在这时挂上。
        let (rendered, buttons) = self.render_reply(&chat_tag_str, content);
        let card = summon_core::feishu::card_markdown_v2_rendered(&rendered);
        if !summon_core::feishu::rendered_final_reply_fits(&rendered) {
            anyhow::bail!(summon_i18n::format!(
                "r1f6330c1e7",
                rendered.as_str().len(),
                card.len(),
                summon_core::feishu::CARD_TEXT_SAFE_LIMIT,
                summon_core::feishu::CARD_TOTAL_SAFE_LIMIT,
            ));
        }
        let card = summon_core::feishu::card_with_preview_buttons(&card, &buttons).unwrap_or(card);
        self.settle_stream_card(id, &mid, "stream_finish", &card)
    }

    fn stream_freeze(&self, id: StreamId, content: &str) -> anyhow::Result<()> {
        // 暂停/重跑只是结束当前卡片，不代表 agent 已完成派单意图；绕节流但不展开 mention。
        let (mid, chat_tag_str) = self.stream_target(id, "stream_freeze")?;
        if self.is_cot(id) {
            return self.settle_cot(id, CotTerminal::Finished("interrupted"));
        }
        let rendered = self.render_streaming_text(&chat_tag_str, content);
        let card = summon_core::feishu::card_markdown_v2_rendered(&rendered);
        if !summon_core::feishu::rendered_final_reply_fits(&rendered) {
            anyhow::bail!(summon_i18n::format!(
                "rab569b045f",
                rendered.as_str().len(),
                card.len(),
                summon_core::feishu::CARD_TEXT_SAFE_LIMIT,
                summon_core::feishu::CARD_TOTAL_SAFE_LIMIT,
            ));
        }
        self.settle_stream_card(id, &mid, "stream_freeze", &card)
    }

    fn stream_finish_card(&self, id: StreamId, card_json: &str) -> anyhow::Result<()> {
        // 权威终态卡:绕节流必发，并在此时展开正文里的真实 mention。
        let (mid, chat_tag_str) = self.stream_target(id, "stream_finish_card")?;
        let rendered = self.render_mentions_in_card(&chat_tag_str, card_json);
        if self.is_cot(id) {
            return self.settle_cot(id, CotTerminal::Finished("done"));
        }
        self.settle_stream_card(id, &mid, "stream_finish_card", &rendered)
    }

    fn stream_end_session(&self, id: StreamId, _partial: &str) -> anyhow::Result<()> {
        if self.is_cot(id) {
            self.settle_cot(id, CotTerminal::SessionEnded)
        } else {
            self.stream_freeze(id, summon_i18n::text!("r3c30e0b840"))
        }
    }

    fn stream_freeze_card(&self, id: StreamId, card_json: &str) -> anyhow::Result<()> {
        // 过程卡定格/暂停/分卡:绕节流结束当前卡，但未完成快照只保留字面 mention。
        let (mid, chat_tag_str) = self.stream_target(id, "stream_freeze_card")?;
        if self.is_cot(id) {
            return self.settle_cot(id, CotTerminal::Finished("interrupted"));
        }
        let rendered = self.render_streaming_card(&chat_tag_str, card_json);
        self.settle_stream_card(id, &mid, "stream_freeze_card", &rendered)
    }

    fn react_stream_card(&self, id: StreamId, key: &str) -> anyhow::Result<String> {
        // 卡片更新失败时贴表情通知用户:按 StreamId 查内部 message_id 映射;查不到 → 空串忽略。
        let mid = match self.stream_map.borrow().get(&id.0) {
            Some(st) => st.mid.clone(),
            None => return Ok(String::new()),
        };
        self.fc.react(&mid, key)
    }

    fn react(&self, msg: &MsgRef, key: &str) -> anyhow::Result<String> {
        // 空 id = 这一轮没有可锚的飞书消息(定时 / sentinel 的合成 id)。**不打网络**:
        // 拿合成 id 调必然 400,还会在日志里留一串「表情失败」噪音。
        if msg.message_id.is_empty() {
            return Ok(String::new());
        }
        self.fc.react(msg.message_id, key)
    }

    fn unreact(&self, msg: &MsgRef, reaction_id: &str) -> anyhow::Result<()> {
        if msg.message_id.is_empty() {
            return Ok(());
        }
        self.fc.unreact(msg.message_id, reaction_id)
    }

    fn send_card(&self, chat: &ChatRef, card_json: &str) -> anyhow::Result<()> {
        // 交互菜单中介卡片 / 总结卡:先对卡片 JSON 里的 markdown content 做 @mention 替换,
        // 再发到该 chat 并归档。与流式路径对齐,避免 @name 原样出现在卡里或已渲染的 <at> 标签
        // 指向无效用户。
        let tag = chat_tag(chat.chat_type, chat.chat_id);
        let rendered = self.render_mentions_in_card(&tag, card_json);
        Self::ensure_card_budget("send_card", &rendered)?;
        let message_id =
            self.fc
                .send_card_id_replying(chat.chat_id, self.reply_target_for(chat), &rendered)?;
        append_outbound(
            chat.agent_dir,
            chat.chat_type,
            chat.chat_id,
            "tui_prompt",
            serde_json::json!({"message_id": message_id, "card": super::outbound::card_payload(&rendered)}),
        );
        *self.last_sent.borrow_mut() = Some((message_id, rendered));
        Ok(())
    }

    fn settle_last_sent_as_reply(&self) {
        let (Some(slot), Some((message_id, card_json))) =
            (&self.settled_card, self.last_sent.borrow_mut().take())
        else {
            return;
        };
        if let Ok(mut guard) = slot.lock() {
            *guard = Some(crate::frontend::SettledCard {
                message_id,
                card_json,
                kind: crate::frontend::SettledCardKind::Reply,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cot_event(label: &str) -> summon_core::cot::CotEvent {
        summon_core::cot::CotEvent {
            event_type: label.to_string(),
            content: "{}".to_string(),
            timestamp: "1".to_string(),
            reply_delivery: None,
        }
    }

    #[test]
    fn a_final_reply_in_a_text_stream_is_registered_on_its_existing_message() {
        for badge in [false, true] {
            let (base, server) = recording_feishu(2);
            let dir = tempfile::tempdir().unwrap();
            let fc = FeishuClient::with_test_base_connection_close(base);
            let slot: crate::frontend::SharedSettledCard = Default::default();
            let frontend = test_frontend(&fc, dir.path()).with_settled_card(slot.clone());
            let id = install_pending_cot(&frontend, Vec::new());
            frontend
                .stream_map
                .borrow_mut()
                .get_mut(&id.0)
                .unwrap()
                .transport = StreamTransport::Text;
            if badge {
                frontend
                    .stream_finish_card(id, &summon_core::feishu::card_markdown_v2("结论"))
                    .unwrap();
            } else {
                frontend.stream_finish(id, "结论").unwrap();
            }
            frontend.settle_last_sent_as_reply();
            let settled = slot.lock().unwrap().clone().unwrap();
            assert_eq!(settled.message_id, "om_cot");
            assert_eq!(settled.kind, crate::frontend::SettledCardKind::Reply);
            assert!(settled.card_json.contains("结论"));
            let calls = server.join().unwrap();
            assert_eq!(calls.len(), 1);
            assert_eq!(calls[0].0, "PATCH");
        }
    }

    #[test]
    fn native_cot_session_end_pushes_the_notice_and_terminal_on_the_same_carrier() {
        let (base, server) = recording_feishu(2);
        let dir = tempfile::tempdir().unwrap();
        let fc = FeishuClient::with_test_base_connection_close(base);
        let frontend = test_frontend(&fc, dir.path());
        let id = install_pending_cot(&frontend, Vec::new());
        frontend.stream_end_session(id, "正文").unwrap();
        let calls = server.join().unwrap();
        assert_eq!(calls.len(), 1);
        assert!(calls[0].2.contains("cot_1"));
        assert!(calls[0].2.contains("该会话已结束"));
        assert!(calls[0].2.contains("TEXT_MESSAGE_CONTENT"));
        assert!(calls[0].2.contains("RUN_FINISHED"));
        assert!(!frontend.stream_map.borrow().contains_key(&id.0));
    }

    fn test_carrier(
        cot: &str,
        mid: &str,
        tag: &str,
        path: Option<std::path::PathBuf>,
    ) -> CotCarrier {
        let mut carrier = CotCarrier::for_test(cot, mid);
        carrier.chat_tag = tag.into();
        carrier.recovery_path = path;
        carrier
    }

    fn install_pending_cot(
        frontend: &FeishuFrontend<'_>,
        events: Vec<summon_core::cot::CotEvent>,
    ) -> StreamId {
        let id = StreamId(77);
        frontend.stream_map.borrow_mut().insert(
            id.0,
            StreamState {
                mid: "om_cot".into(),
                chat_tag: "g-test".into(),
                throttle: PatchThrottle::primed(COT_UPDATE_MIN_INTERVAL_MS, 0),
                with_interrupt: true,
                transport: StreamTransport::Cot(Box::new(CotState {
                    carrier: test_carrier("cot_1", "om_cot", "g-test", None),
                    projector: summon_core::cot::CotProjector::new("oc_test", "turn-1", "query"),
                    pending: events,
                    confirmed_replies: Vec::new(),
                    next_batch: 0,
                    append_failed: false,
                })),
            },
        );
        id
    }

    /// 「只承载过程」的判据焊在传输上,不能靠 `Frontend` 的默认实现。轮末收尾据它二选一:
    /// process_only 走 `stream_finish`(CoT 记 `done`、最终回复另发独立消息),否则走
    /// `stream_freeze_card` —— 而 freeze 在 CoT 上就是 `interrupted`,干净跑完的轮会被
    /// 标成中断。默认实现恒 `false`,所以判据缺失既不报错也不掉测试,只能在这里钉住。
    #[test]
    fn only_the_cot_transport_is_process_only() {
        let dir = tempfile::tempdir().unwrap();
        let fc = FeishuClient::new("feishu.cn", String::new(), String::new(), true);
        let frontend = test_frontend(&fc, dir.path());

        let cot = install_pending_cot(&frontend, Vec::new());
        assert!(
            frontend.stream_is_process_only(cot),
            "CoT 流必须报 process_only,否则轮末收尾把干净跑完的轮标成 interrupted"
        );

        let text = StreamId(78);
        frontend.stream_map.borrow_mut().insert(
            text.0,
            StreamState {
                mid: "om_text".into(),
                chat_tag: "g-test".into(),
                throttle: PatchThrottle::primed(COT_UPDATE_MIN_INTERVAL_MS, 0),
                with_interrupt: true,
                transport: StreamTransport::Text,
            },
        );
        assert!(
            !frontend.stream_is_process_only(text),
            "文本流自己就是最终载体,报 process_only 会让终态正文没人发"
        );
    }

    #[test]
    fn flush_cot_sends_101_events_as_ordered_50_50_1_requests() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let base = format!("http://{}", server.server_addr().to_ip().unwrap());
        let server_thread = std::thread::spawn(move || {
            let mut batches = Vec::new();
            for index in 0..4 {
                let mut request = server
                    .recv_timeout(std::time::Duration::from_secs(2))
                    .unwrap()
                    .expect("token + 三批 CoT 事件");
                if index == 0 {
                    request
                        .respond(tiny_http::Response::from_string(
                            r#"{"tenant_access_token":"token","expire":7200}"#,
                        ))
                        .unwrap();
                    continue;
                }
                assert_eq!(request.method(), &tiny_http::Method::Put);
                let mut body = String::new();
                request.as_reader().read_to_string(&mut body).unwrap();
                let value: serde_json::Value = serde_json::from_str(&body).unwrap();
                batches.push(
                    value["events"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|event| event["event_type"].as_str().unwrap().to_string())
                        .collect::<Vec<_>>(),
                );
                request
                    .respond(tiny_http::Response::from_string(r#"{"code":0}"#))
                    .unwrap();
            }
            batches
        });
        let dir = tempfile::tempdir().unwrap();
        let fc = FeishuClient::with_test_base_connection_close(base);
        let frontend = test_frontend(&fc, dir.path());
        let id = install_pending_cot(
            &frontend,
            (0..101)
                .map(|index| cot_event(&format!("old-{index}")))
                .collect(),
        );
        frontend.flush_cot(id).unwrap();
        let batches = server_thread.join().unwrap();
        assert_eq!(
            batches.iter().map(Vec::len).collect::<Vec<_>>(),
            [50, 50, 1]
        );
        assert_eq!(batches[0][0], "old-0");
        assert_eq!(batches[1][0], "old-50");
        assert_eq!(batches[2][0], "old-100");
    }

    #[test]
    fn flush_cot_never_replays_an_ambiguous_failed_batch() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let base = format!("http://{}", server.server_addr().to_ip().unwrap());
        let server_thread = std::thread::spawn(move || {
            let mut batches = Vec::new();
            for index in 0..3 {
                let mut request = server
                    .recv_timeout(std::time::Duration::from_secs(2))
                    .unwrap()
                    .expect("token + 首次两批");
                if index == 0 {
                    request
                        .respond(tiny_http::Response::from_string(
                            r#"{"tenant_access_token":"token","expire":7200}"#,
                        ))
                        .unwrap();
                    continue;
                }
                let mut body = String::new();
                request.as_reader().read_to_string(&mut body).unwrap();
                let value: serde_json::Value = serde_json::from_str(&body).unwrap();
                batches.push(
                    value["events"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|event| event["event_type"].as_str().unwrap().to_string())
                        .collect::<Vec<_>>(),
                );
                let response = if index == 2 {
                    r#"{"code":230001,"msg":"retry"}"#
                } else {
                    r#"{"code":0}"#
                };
                request
                    .respond(tiny_http::Response::from_string(response))
                    .unwrap();
            }
            batches
        });
        let dir = tempfile::tempdir().unwrap();
        let fc = FeishuClient::with_test_base_connection_close(base);
        let frontend = test_frontend(&fc, dir.path());
        let id = install_pending_cot(
            &frontend,
            (0..101)
                .map(|index| cot_event(&format!("old-{index}")))
                .collect(),
        );
        assert!(frontend.flush_cot(id).is_err());
        if let StreamTransport::Cot(state) = &mut frontend
            .stream_map
            .borrow_mut()
            .get_mut(&id.0)
            .unwrap()
            .transport
        {
            state.pending.push(cot_event("new-101"));
        }
        assert!(frontend.flush_cot(id).is_err());
        let batches = server_thread.join().unwrap();
        assert_eq!(batches.iter().map(Vec::len).collect::<Vec<_>>(), [50, 50]);
        let map = frontend.stream_map.borrow();
        let StreamTransport::Cot(state) = &map[&id.0].transport else {
            panic!("应保留 CoT 状态供显式收口")
        };
        assert!(state.append_failed);
        assert!(state.pending.is_empty());
    }

    #[test]
    fn long_reply_confirms_successful_batches_before_an_uncertain_response() {
        for response_lost in [false, true] {
            let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
            let base = format!("http://{}", server.server_addr().to_ip().unwrap());
            let server_thread = std::thread::spawn(move || {
                let mut accepted = String::new();
                for index in 0..3 {
                    let mut request = server
                        .recv_timeout(std::time::Duration::from_secs(3))
                        .unwrap()
                        .expect("token and two batches");
                    if index == 0 {
                        request
                            .respond(tiny_http::Response::from_string(
                                r#"{"tenant_access_token":"token","expire":7200}"#,
                            ))
                            .unwrap();
                        continue;
                    }
                    let mut body = String::new();
                    request.as_reader().read_to_string(&mut body).unwrap();
                    let value: serde_json::Value = serde_json::from_str(&body).unwrap();
                    assert!(value["events"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .all(|event| event.get("reply_delivery").is_none()));
                    if index == 1 {
                        for event in value["events"].as_array().unwrap() {
                            if event["event_type"] == "TEXT_MESSAGE_CONTENT" {
                                let content: serde_json::Value =
                                    serde_json::from_str(event["content"].as_str().unwrap())
                                        .unwrap();
                                accepted.push_str(content["delta"].as_str().unwrap());
                            }
                        }
                    }
                    let response = if index == 1 {
                        r#"{"code":0}"#
                    } else if response_lost {
                        ""
                    } else {
                        r#"{"code":230001,"msg":"unavailable"}"#
                    };
                    request
                        .respond(tiny_http::Response::from_string(response))
                        .unwrap();
                }
                assert!(
                    server
                        .recv_timeout(std::time::Duration::from_millis(100))
                        .unwrap()
                        .is_none(),
                    "uncertain batch must not be replayed"
                );
                accepted
            });
            let dir = tempfile::tempdir().unwrap();
            let fc = FeishuClient::with_test_base_connection_close(base);
            let frontend = test_frontend(&fc, dir.path());
            let raw = format!(
                "{}\n\n<summon-feed>进度</summon-feed>",
                "甲🙂乙".repeat(12000)
            );
            let text = summon_core::feed_tag::strip_feed_tag(&raw);
            let mut projector = summon_core::cot::CotProjector::new("oc_test", "turn-1", "任务");
            let events = projector.project_with_replies(
                &[summon_core::segment::TurnSegment::Reply(raw.clone())],
                Some(1),
                None,
                "1000",
            );
            let id = install_pending_cot(&frontend, events);
            assert!(frontend.flush_cot(id).is_err());
            let confirmed = frontend.stream_confirmed_replies(id).unwrap();
            assert_eq!(confirmed.len(), 1);
            assert!(frontend.flush_cot(id).is_err());
            let accepted = server_thread.join().unwrap();
            assert_eq!(confirmed[0].bytes, accepted.len());
            assert!(text.starts_with(&accepted));
            assert!(confirmed[0].bytes > 0 && confirmed[0].bytes < text.len());
            assert!(text.is_char_boundary(confirmed[0].bytes));
            assert_eq!(format!("{}{}", accepted, &text[confirmed[0].bytes..]), text);
        }
    }

    #[test]
    fn failed_stream_restart_uses_a_fresh_confirmation_generation() {
        let dir = tempfile::tempdir().unwrap();
        let fc = FeishuClient::new("feishu.cn", String::new(), String::new(), true);
        let frontend = test_frontend(&fc, dir.path());
        let id = install_pending_cot(&frontend, Vec::new());
        {
            let mut map = frontend.stream_map.borrow_mut();
            let StreamTransport::Cot(state) = &mut map.get_mut(&id.0).unwrap().transport else {
                panic!("CoT");
            };
            state.append_failed = true;
            state
                .confirmed_replies
                .push(summon_core::cot::ReplyDelivery {
                    generation: 0,
                    segment: 0,
                    bytes: 1024,
                });
        }
        frontend.stream_restart_boundary(id, "继续").unwrap();
        assert_eq!(frontend.stream_confirmed_replies(id), Some(Vec::new()));
        let map = frontend.stream_map.borrow();
        let StreamTransport::Cot(state) = &map[&id.0].transport else {
            panic!("CoT");
        };
        assert_eq!(state.projector.reply_generation(), 1);
        assert!(state.pending.is_empty());
        assert!(state.append_failed);
    }

    #[test]
    fn uncertain_orphan_is_completed_without_appending_a_closing_note() {
        let (base, server) = recording_feishu(2);
        let fc = FeishuClient::with_test_base_connection_close(base);
        let record = CotRecoveryRecord {
            generation: "old".into(),
            cot_id: "cot_uncertain".into(),
            message_id: "om_uncertain".into(),
            legacy_control_mid: None,
            cot_closed: false,
            append_failed: true,
            chat_tag: Some("g_uncertain".into()),
            projector: Some(projector_after("过程正文")),
            page: None,
        };
        assert!(close_orphan_cot(&fc, &record));
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].1.contains("/complete/cot_uncertain"));
        assert!(!requests[0].2.contains("过程正文"));
    }

    #[test]
    fn uncertain_orphan_retries_completion_with_its_failure_latch_intact() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let base = format!("http://{}", server.server_addr().to_ip().unwrap());
        let handle = std::thread::spawn(move || {
            for index in 0..3 {
                let request = server
                    .recv_timeout(std::time::Duration::from_secs(3))
                    .unwrap()
                    .unwrap();
                if index == 0 {
                    request
                        .respond(tiny_http::Response::from_string(
                            r#"{"tenant_access_token":"token","expire":7200}"#,
                        ))
                        .unwrap();
                    continue;
                }
                assert_eq!(request.method(), &tiny_http::Method::Post);
                assert!(request
                    .url()
                    .contains("/message_cot/complete/cot_uncertain"));
                request
                    .respond(tiny_http::Response::from_string(if index == 1 {
                        r#"{"code":230001,"msg":"temporary failure"}"#
                    } else {
                        r#"{"code":0}"#
                    }))
                    .unwrap();
            }
        });
        let dir = tempfile::tempdir().unwrap();
        let fc = FeishuClient::with_test_base_connection_close(base);
        let record = CotRecoveryRecord {
            generation: "old-generation".into(),
            cot_id: "cot_uncertain".into(),
            message_id: "om_uncertain".into(),
            legacy_control_mid: None,
            cot_closed: false,
            append_failed: true,
            chat_tag: Some("g_uncertain".into()),
            projector: Some(projector_after("过程正文")),
            page: None,
        };
        let path = FeishuFrontend::persist_cot_recovery(dir.path(), &record).unwrap();
        finalize_orphan_cot_messages(&fc, dir.path(), &|_, _| false);
        assert!(
            FeishuFrontend::read_cot_recovery(&path)
                .unwrap()
                .append_failed
        );
        finalize_orphan_cot_messages(&fc, dir.path(), &|_, _| false);
        assert!(!path.exists());
        handle.join().unwrap();
    }

    #[test]
    fn resumed_batch_failure_retains_confirmations_and_latches_the_recovery() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let base = format!("http://{}", server.server_addr().to_ip().unwrap());
        let handle = std::thread::spawn(move || {
            let mut confirmed = String::new();
            for index in 0..3 {
                let mut request = server
                    .recv_timeout(std::time::Duration::from_secs(3))
                    .unwrap()
                    .unwrap();
                if index == 0 {
                    request
                        .respond(tiny_http::Response::from_string(
                            r#"{"tenant_access_token":"token","expire":7200}"#,
                        ))
                        .unwrap();
                    continue;
                }
                assert_eq!(request.method(), &tiny_http::Method::Put);
                let mut body = String::new();
                request.as_reader().read_to_string(&mut body).unwrap();
                let body: serde_json::Value = serde_json::from_str(&body).unwrap();
                if index == 1 {
                    for event in body["events"].as_array().unwrap() {
                        if event["event_type"] == "TEXT_MESSAGE_CONTENT" {
                            let content: serde_json::Value =
                                serde_json::from_str(event["content"].as_str().unwrap()).unwrap();
                            confirmed.push_str(content["delta"].as_str().unwrap());
                        }
                    }
                }
                request
                    .respond(tiny_http::Response::from_string(if index == 1 {
                        r#"{"code":0}"#
                    } else {
                        ""
                    }))
                    .unwrap();
            }
            assert!(server
                .recv_timeout(std::time::Duration::from_millis(100))
                .unwrap()
                .is_none());
            confirmed
        });
        let dir = tempfile::tempdir().unwrap();
        let record = CotRecoveryRecord {
            generation: "old".into(),
            cot_id: "cot_old".into(),
            message_id: "om_old".into(),
            legacy_control_mid: None,
            cot_closed: false,
            append_failed: false,
            chat_tag: Some(chat_tag("group", "oc_reattach")),
            projector: Some(summon_core::cot::CotProjector::new(
                "oc_reattach",
                "turn",
                "任务",
            )),
            page: Some(CotPage {
                cot_id: "cot_old".into(),
                message_id: "om_old".into(),
                closed: false,
                creation_pending: false,
                wire: Default::default(),
            }),
        };
        let path = FeishuFrontend::persist_cot_recovery(dir.path(), &record).unwrap();
        let fc = FeishuClient::with_test_base_connection_close(base);
        let frontend = test_frontend(&fc, dir.path());
        let chat = reattach_chat(dir.path());
        let text = "甲🙂乙".repeat(12000);
        let segments = [summon_core::segment::TurnSegment::Reply(text.clone())];
        let view = summon_core::cot::CotView::new(&segments, None).with_released_replies(Some(1));
        let anchor = summon_core::TurnAnchorCot {
            cot_id: "cot_old".into(),
            message_id: "om_old".into(),
        };
        let id = frontend.cot_resume_claimed(&chat, &view, &anchor).unwrap();
        assert!(frontend.stream_append_failed(id));
        assert!(
            FeishuFrontend::read_cot_recovery(&path)
                .unwrap()
                .append_failed
        );
        let confirmed = frontend.stream_confirmed_replies(id).unwrap();
        assert_eq!(confirmed.len(), 1);
        assert_eq!(frontend.stream_cot_handle(id).unwrap().0, "cot_old");
        assert!(frontend.cot_resume_claimed(&chat, &view, &anchor).is_none());
        assert!(frontend.stream_flush(id).is_err());
        let confirmed_text = handle.join().unwrap();
        assert_eq!(confirmed[0].bytes, confirmed_text.len());
        assert!(text.starts_with(&confirmed_text));
    }

    #[test]
    fn stale_cot_recovery_is_completed_and_removed_before_a_new_generation() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let base = format!("http://{}", server.server_addr().to_ip().unwrap());
        let server_thread = std::thread::spawn(move || {
            for index in 0..2 {
                let request = server
                    .recv_timeout(std::time::Duration::from_secs(2))
                    .unwrap()
                    .expect("token + complete");
                let body = if index == 0 {
                    r#"{"tenant_access_token":"token","expire":7200}"#
                } else {
                    r#"{"code":0,"data":{}}"#
                };
                request
                    .respond(tiny_http::Response::from_string(body))
                    .unwrap();
            }
        });
        let dir = tempfile::tempdir().unwrap();
        let record = CotRecoveryRecord {
            generation: "old-generation".into(),
            cot_id: "cot_old".into(),
            message_id: "om_old".into(),
            legacy_control_mid: None,
            cot_closed: false,
            append_failed: false,
            chat_tag: Some("g-test".into()),
            projector: None,
            page: None,
        };
        let path = FeishuFrontend::persist_cot_recovery(dir.path(), &record).unwrap();
        let fc = FeishuClient::with_test_base_connection_close(base);
        let frontend = test_frontend(&fc, dir.path());
        frontend.drain_stale_cot_recoveries(dir.path());
        server_thread.join().unwrap();
        assert!(!path.exists());
    }

    /// 开新卡前的那趟反熵与启动收口查同一张 keep 表。同 agent 的另一个 chat 恰在「启动收口
    /// 放行」到「接管轮真正认领」之间开卡,拿空表扫一遍就把待接管那条收走了 —— 接管轮只能
    /// 退回新 CoT,用户看到停机前那条停在原地、答案另起一条。
    #[test]
    fn opening_a_new_cot_keeps_the_carrier_a_reattach_turn_is_about_to_claim() {
        let (base, server) = recording_feishu(3);
        let dir = tempfile::tempdir().unwrap();
        let kept = CotRecoveryRecord {
            generation: "old-generation".into(),
            cot_id: "cot_keep".into(),
            message_id: "om_keep".into(),
            legacy_control_mid: None,
            cot_closed: false,
            append_failed: false,
            chat_tag: Some("g-keep".into()),
            projector: Some(projector_after("接管轮要接着写")),
            page: None,
        };
        let closed = CotRecoveryRecord {
            generation: "old-generation".into(),
            cot_id: "cot_close".into(),
            message_id: "om_close".into(),
            legacy_control_mid: None,
            cot_closed: false,
            append_failed: false,
            chat_tag: Some("g-close".into()),
            projector: Some(projector_after("停机时想到一半")),
            page: None,
        };
        let kept_path = FeishuFrontend::persist_cot_recovery(dir.path(), &kept).unwrap();
        let closed_path = FeishuFrontend::persist_cot_recovery(dir.path(), &closed).unwrap();
        let fc = FeishuClient::with_test_base_connection_close(base);
        let frontend = test_frontend(&fc, dir.path()).with_cot_keep(Box::new(|tag, cot_id| {
            tag == "g-keep" && cot_id == "cot_keep"
        }));

        frontend.drain_stale_cot_recoveries(dir.path());
        let calls = server.join().unwrap();

        assert!(kept_path.exists(), "接管中的记录不得被开新卡的反熵收走");
        assert!(!closed_path.exists(), "其余孤儿照收");
        assert!(
            calls
                .iter()
                .all(|(_, url, body)| !url.contains("cot_keep") && !body.contains("cot_keep")),
            "接管中的 CoT 一个请求都不该发:{calls:?}"
        );
    }

    #[test]
    fn published_current_generation_recovery_is_not_reaped_until_owner_retires() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let base = format!("http://{}", server.server_addr().to_ip().unwrap());
        let (skip_verified_tx, skip_verified_rx) = std::sync::mpsc::channel();
        let server_thread = std::thread::spawn(move || {
            assert!(
                server
                    .recv_timeout(std::time::Duration::from_millis(150))
                    .unwrap()
                    .is_none(),
                "文件发布与 active owner 登记必须对扫描原子可见"
            );
            skip_verified_tx.send(()).unwrap();
            for index in 0..2 {
                let request = server
                    .recv_timeout(std::time::Duration::from_secs(2))
                    .unwrap()
                    .expect("owner 退役后应发 token + complete");
                let body = if index == 0 {
                    r#"{"tenant_access_token":"token","expire":7200}"#
                } else {
                    r#"{"code":0,"data":{}}"#
                };
                request
                    .respond(tiny_http::Response::from_string(body))
                    .unwrap();
            }
        });
        let dir = tempfile::tempdir().unwrap();
        let record = CotRecoveryRecord {
            generation: cot_process_generation().to_string(),
            cot_id: "cot_live".into(),
            message_id: "om_live".into(),
            legacy_control_mid: None,
            cot_closed: false,
            append_failed: false,
            chat_tag: Some("g-test".into()),
            projector: None,
            page: None,
        };
        let path = FeishuFrontend::publish_cot_recovery(dir.path(), &record).unwrap();
        let fc = FeishuClient::with_test_base_connection_close(base);

        finalize_orphan_cot_messages(&fc, dir.path(), &|_, _| false);
        assert!(path.exists(), "同代 active owner 的 CoT 不得被启动扫描收口");
        skip_verified_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap();

        FeishuFrontend::retire_cot_recovery(&path, false);
        finalize_orphan_cot_messages(&fc, dir.path(), &|_, _| false);
        server_thread.join().unwrap();
        assert!(!path.exists());
    }

    use summon_core::peers::{Peer, PeersFile};

    /// **引述区块里的 @ 不得渲染成真实 at** —— 否则 bot 之间会互相唤起。
    ///
    /// 2026-08-12 owner 实测到回声:agent 按要求 @ 另一个 agent 请它 review → 对方的**思考卡**
    /// 被镜像回群 → 卡里复述的 `@<发起方>` 被渲染成真实 at → 又触发发起方一轮。
    /// 折叠面板装的是思考/工具过程,底注装的是「关于:别人说过的话」,两者里的 @ 都是**在谈论**
    /// 而不是**在呼叫**。
    ///
    /// 正文里的 @ 必须照常工作 —— 那是 agent 真的要派单(owner 的 `/introduce` 就是为它)。
    #[test]
    fn quoted_blocks_do_not_render_real_mentions() {
        let peers = vec![Peer {
            open_id: "ou_bot".into(),
            name: "星光".into(),
            ..Default::default()
        }];
        let alias = std::collections::HashMap::new();
        let mut card = serde_json::json!({
            "elements": [
                // 正文:必须渲染成真实 at
                {"tag": "markdown", "content": "请 @星光 看一下"},
                // 思考/过程折叠块:里面复述的 @ 不得触发
                {"tag": "collapsible_panel", "expanded": false,
                 "header": {"title": {"tag": "markdown", "content": "💭 思考 @星光"}},
                 "elements": [{"tag": "markdown", "content": "我在想要不要 @星光 帮忙"}]},
                // 底注/上下文行:引文,不得触发
                {"tag": "markdown", "content": "<font color='grey'>关于:@星光 对抗review</font>"}
            ]
        });
        render_mentions_in_value(&mut card, &peers, &alias);
        let els = card["elements"].as_array().unwrap();

        let body = els[0]["content"].as_str().unwrap();
        assert!(
            body.contains("<at user_id=\"ou_bot\">"),
            "正文里的 @ 必须照常渲染成真实 at:{body}"
        );

        let panel_title = els[1]["header"]["title"]["content"].as_str().unwrap();
        let panel_body = els[1]["elements"][0]["content"].as_str().unwrap();
        assert!(
            !panel_title.contains("<at "),
            "折叠面板标题不得产生 at:{panel_title}"
        );
        assert!(
            !panel_body.contains("<at "),
            "折叠面板正文不得产生 at:{panel_body}"
        );
        assert!(
            panel_body.contains("@星光"),
            "但字面 @名字 要留着,读的人还得看懂"
        );

        let footer = els[2]["content"].as_str().unwrap();
        assert!(!footer.contains("<at "), "底注/引用行不得产生 at:{footer}");
    }

    fn test_frontend<'a>(fc: &'a FeishuClient, root: &std::path::Path) -> FeishuFrontend<'a> {
        let peers_store = PeersStore::new(root.join("peers"));
        peers_store
            .save(
                &chat_tag("group", "oc_split"),
                &PeersFile {
                    peers: vec![Peer {
                        open_id: "ou_bot_with_a_long_open_id".into(),
                        name: "星光".into(),
                        ..Default::default()
                    }],
                    introduced_at: None,
                    note: None,
                },
            )
            .unwrap();
        let scope = format!(
            "split-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        FeishuFrontend::new(
            fc,
            HashMap::from([("bot".to_string(), "星光".to_string())]),
            Some(peers_store),
            IdentityCacheStore::at_run_dir(&scope, false),
        )
    }

    #[test]
    fn streaming_text_keeps_mentions_literal_until_terminal_rendering() {
        let dir = tempfile::tempdir().unwrap();
        let fc = FeishuClient::new("feishu.cn", String::new(), String::new(), true);
        let frontend = test_frontend(&fc, dir.path());
        let tag = chat_tag("group", "oc_split");

        let live = frontend.render_streaming_text(&tag, "请 @bot review <draft>");
        assert!(
            live.as_str().contains("@bot"),
            "流式正文应保留字面 mention:{live:?}"
        );
        assert!(
            !live.as_str().contains("<at "),
            "流式正文不得产生真实 mention:{live:?}"
        );
        assert!(
            live.as_str().contains("<\u{200b}draft>"),
            "流式正文仍须执行外部 markdown 转义:{live:?}"
        );

        let (terminal, _) = frontend.render_reply(&tag, "请 @bot review <draft>");
        assert!(
            terminal
                .as_str()
                .contains("<at user_id=\"ou_bot_with_a_long_open_id\">星光</at>"),
            "终态正文必须产生真实 mention:{terminal:?}"
        );
    }

    #[test]
    fn streaming_card_keeps_mentions_literal_until_terminal_rendering() {
        let dir = tempfile::tempdir().unwrap();
        let fc = FeishuClient::new("feishu.cn", String::new(), String::new(), true);
        let frontend = test_frontend(&fc, dir.path());
        let tag = chat_tag("group", "oc_split");
        let card = serde_json::json!({
            "schema": "2.0",
            "body": {"elements": [{"tag": "markdown", "content": "请 @bot review"}]},
        })
        .to_string();

        let live = frontend.render_streaming_card(&tag, &card);
        assert!(live.contains("@bot"), "流式卡应保留字面 mention:{live}");
        assert!(!live.contains("<at "), "流式卡不得产生真实 mention:{live}");

        let terminal = frontend.render_mentions_in_card(&tag, &card);
        assert!(
            terminal.contains("<at user_id=\\\"ou_bot_with_a_long_open_id\\\">星光</at>"),
            "终态卡必须产生真实 mention:{terminal}"
        );
    }

    /// 锁住真实 FeishuFrontend 出站边界，而不只测渲染 helper：文本流与交织卡都必须在
    /// begin 时保留字面 @，到 finish 的权威终态 PATCH 才生成会触发对方的 `<at>`。
    #[test]
    fn stream_http_payload_expands_mentions_only_on_finish() {
        use std::time::Duration;

        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let base = format!("http://{}", server.server_addr().to_ip().unwrap());
        let server_thread = std::thread::spawn(move || {
            for request_index in 0..7 {
                let mut request = server
                    .recv_timeout(Duration::from_secs(2))
                    .unwrap()
                    .expect("应收到 token + 两组 begin/live-patch/finish 请求");
                let response = match request_index {
                    0 => tiny_http::Response::from_string(
                        r#"{"code":0,"tenant_access_token":"token","expire":7200}"#,
                    ),
                    1 | 4 => {
                        assert_eq!(request.method(), &tiny_http::Method::Post);
                        let mut body = String::new();
                        request.as_reader().read_to_string(&mut body).unwrap();
                        let outer: serde_json::Value = serde_json::from_str(&body).unwrap();
                        let card = outer["content"].as_str().unwrap();
                        assert!(card.contains("@bot"), "live POST 应保留字面 @:{card}");
                        assert!(!card.contains("<at "), "live POST 不得产生真实 at:{card}");
                        tiny_http::Response::from_string(format!(
                            r#"{{"code":0,"data":{{"message_id":"om_{request_index}"}}}}"#
                        ))
                    }
                    2 | 5 => {
                        assert_eq!(request.method(), &tiny_http::Method::Patch);
                        let mut body = String::new();
                        request.as_reader().read_to_string(&mut body).unwrap();
                        let outer: serde_json::Value = serde_json::from_str(&body).unwrap();
                        let card = outer["content"].as_str().unwrap();
                        assert!(card.contains("@bot"), "live PATCH 应保留字面 @:{card}");
                        assert!(!card.contains("<at "), "live PATCH 不得产生真实 at:{card}");
                        tiny_http::Response::from_string(r#"{"code":0}"#)
                    }
                    3 | 6 => {
                        assert_eq!(request.method(), &tiny_http::Method::Patch);
                        let mut body = String::new();
                        request.as_reader().read_to_string(&mut body).unwrap();
                        let outer: serde_json::Value = serde_json::from_str(&body).unwrap();
                        let card = outer["content"].as_str().unwrap();
                        assert!(
                            card.contains(
                                "<at user_id=\\\"ou_bot_with_a_long_open_id\\\">星光</at>"
                            ),
                            "finish PATCH 必须产生真实 at:{card}"
                        );
                        tiny_http::Response::from_string(r#"{"code":0}"#)
                    }
                    _ => unreachable!(),
                };
                request.respond(response).unwrap();
            }
        });

        let dir = tempfile::tempdir().unwrap();
        let fc = FeishuClient::with_test_base_connection_close(base);
        let frontend = test_frontend(&fc, dir.path());
        let chat = ChatRef {
            thread_anchor: None,
            chat_id: "oc_split",
            chat_type: "group",
            agent_dir: dir.path(),
            agent_name: "tester",
            delivery_scope: "test-scope",
        };

        let text_stream = frontend
            .stream_begin(&chat, "请 @bot review", true)
            .unwrap();
        frontend
            .stream_map
            .borrow_mut()
            .get_mut(&text_stream.0)
            .unwrap()
            .throttle = PatchThrottle::new(STREAM_PATCH_MIN_INTERVAL_MS);
        frontend.stream_patch(text_stream, "草稿正文 @bot").unwrap();
        frontend
            .stream_finish(text_stream, "请 @bot review")
            .unwrap();

        let live_card = summon_core::feishu::card_markdown_v2("请 @bot review");
        let card_stream = frontend.stream_begin_card(&chat, &live_card).unwrap();
        frontend
            .stream_map
            .borrow_mut()
            .get_mut(&card_stream.0)
            .unwrap()
            .throttle = PatchThrottle::new(STREAM_PATCH_MIN_INTERVAL_MS);
        let patched_card = summon_core::feishu::card_markdown_v2("草稿正文 @bot");
        frontend
            .stream_patch_card(card_stream, &patched_card)
            .unwrap();
        frontend
            .stream_finish_card(card_stream, &live_card)
            .unwrap();

        server_thread.join().unwrap();
    }

    /// 暂停/打断/分卡只是把未完成快照冻结，不是 agent 的最终派单意图；这两类必达 PATCH
    /// 即使结束了当前卡片生命周期，也必须和 live begin/patch 一样保留字面 @。
    #[test]
    fn stream_freeze_http_payload_never_expands_mentions() {
        use std::time::Duration;

        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let base = format!("http://{}", server.server_addr().to_ip().unwrap());
        let server_thread = std::thread::spawn(move || {
            for request_index in 0..5 {
                let mut request = server
                    .recv_timeout(Duration::from_secs(2))
                    .unwrap()
                    .expect("应收到 token + 两组 begin/freeze 请求");
                let response = match request_index {
                    0 => tiny_http::Response::from_string(
                        r#"{"code":0,"tenant_access_token":"token","expire":7200}"#,
                    ),
                    1 | 3 => tiny_http::Response::from_string(format!(
                        r#"{{"code":0,"data":{{"message_id":"om_freeze_{request_index}"}}}}"#
                    )),
                    2 | 4 => {
                        assert_eq!(request.method(), &tiny_http::Method::Patch);
                        let mut body = String::new();
                        request.as_reader().read_to_string(&mut body).unwrap();
                        let outer: serde_json::Value = serde_json::from_str(&body).unwrap();
                        let card = outer["content"].as_str().unwrap();
                        assert!(card.contains("@bot"), "freeze 应保留字面 @:{card}");
                        assert!(!card.contains("<at "), "freeze 不得产生真实 at:{card}");
                        tiny_http::Response::from_string(r#"{"code":0}"#)
                    }
                    _ => unreachable!(),
                };
                request.respond(response).unwrap();
            }
        });

        let dir = tempfile::tempdir().unwrap();
        let fc = FeishuClient::with_test_base_connection_close(base);
        let frontend = test_frontend(&fc, dir.path());
        let chat = ChatRef {
            thread_anchor: None,
            chat_id: "oc_split",
            chat_type: "group",
            agent_dir: dir.path(),
            agent_name: "tester",
            delivery_scope: "test-scope",
        };

        let text_stream = frontend.stream_begin(&chat, "未完成 @bot", true).unwrap();
        frontend.stream_freeze(text_stream, "未完成 @bot").unwrap();

        let card = summon_core::feishu::card_markdown_v2("未完成 @bot");
        let card_stream = frontend.stream_begin_card(&chat, &card).unwrap();
        frontend.stream_freeze_card(card_stream, &card).unwrap();

        server_thread.join().unwrap();
    }

    #[test]
    fn feishu_split_final_reply_uses_rendered_payload_budget() {
        let dir = tempfile::tempdir().unwrap();
        let fc = FeishuClient::new("feishu.cn", String::new(), String::new(), true);
        let frontend = test_frontend(&fc, dir.path());
        let chat = ChatRef {
            thread_anchor: None,
            chat_id: "oc_split",
            chat_type: "group",
            agent_dir: dir.path(),
            agent_name: "tester",
            delivery_scope: "test-scope",
        };
        // `<a` 对消毒后插零宽空格膨胀(10KB → 25KB);`&` 不再转义,保留一段验证它照常分段。
        let body = format!(
            "{}{}{}",
            "<a".repeat(5_000),
            "&".repeat(5_000),
            " @bot".repeat(1_000)
        );

        let segments = frontend.split_final_reply(&chat, &body);

        assert!(segments.len() > 1, "原文较短但渲染后已超单卡预算");
        assert_eq!(segments.concat(), body);
        let tag = chat_tag(chat.chat_type, chat.chat_id);
        assert!(segments.iter().all(|segment| {
            let (rendered, _) = frontend.render_reply(&tag, segment);
            summon_core::feishu::rendered_final_reply_fits(&rendered)
        }));
        let mention_count = segments
            .iter()
            .map(|segment| frontend.render_reply(&tag, segment).0)
            .map(|rendered| rendered.as_str().matches("<at ").count())
            .sum::<usize>();
        assert_eq!(mention_count, 1_000, "分段不能切坏 mention token");
    }

    #[test]
    fn stream_finish_rejects_oversized_rendered_payload_instead_of_clipping() {
        let dir = tempfile::tempdir().unwrap();
        let fc = FeishuClient::new("feishu.cn", String::new(), String::new(), true);
        let frontend = test_frontend(&fc, dir.path());
        frontend.stream_map.borrow_mut().insert(
            1,
            StreamState {
                mid: "om_test".into(),
                chat_tag: chat_tag("group", "oc_split"),
                throttle: PatchThrottle::new(STREAM_PATCH_MIN_INTERVAL_MS),
                with_interrupt: false,
                transport: StreamTransport::Text,
            },
        );

        // 10KB 原文消毒后 25KB,超单卡文本预算 → 必须报错而不是内部裁剪。
        let err = frontend
            .stream_finish(StreamId(1), &"<a".repeat(5_000))
            .unwrap_err();

        assert!(err.to_string().contains("split_final_reply"));
    }

    #[test]
    fn observed_card_patch_distinguishes_applied_throttled_and_missing() {
        let dir = tempfile::tempdir().unwrap();
        let fc = FeishuClient::new("feishu.cn", String::new(), String::new(), true);
        let frontend = test_frontend(&fc, dir.path());
        let tag = chat_tag("group", "oc_split");
        frontend.stream_map.borrow_mut().insert(
            1,
            StreamState {
                mid: "om_applied".into(),
                chat_tag: tag.clone(),
                throttle: PatchThrottle::new(STREAM_PATCH_MIN_INTERVAL_MS),
                with_interrupt: false,
                transport: StreamTransport::Text,
            },
        );
        frontend.stream_map.borrow_mut().insert(
            2,
            StreamState {
                mid: "om_throttled".into(),
                chat_tag: tag,
                throttle: PatchThrottle::primed(STREAM_PATCH_MIN_INTERVAL_MS, frontend.now_ms()),
                with_interrupt: false,
                transport: StreamTransport::Text,
            },
        );
        let card = summon_core::feishu::card_markdown_v2("短正文");

        assert_eq!(
            frontend
                .stream_patch_card_observed(StreamId(1), &card)
                .unwrap(),
            crate::frontend::StreamPatchOutcome::Applied
        );
        assert_eq!(
            frontend
                .stream_patch_card_observed(StreamId(2), &card)
                .unwrap(),
            crate::frontend::StreamPatchOutcome::Throttled
        );
        assert_eq!(
            frontend
                .stream_patch_card_observed(StreamId(999), &card)
                .unwrap(),
            crate::frontend::StreamPatchOutcome::MissingStream
        );
    }

    #[test]
    fn send_message_rejects_oversized_rendered_payload() {
        let dir = tempfile::tempdir().unwrap();
        let fc = FeishuClient::new("feishu.cn", String::new(), String::new(), true);
        let frontend = test_frontend(&fc, dir.path());
        let chat = ChatRef {
            thread_anchor: None,
            chat_id: "oc_split",
            chat_type: "group",
            agent_dir: dir.path(),
            agent_name: "tester",
            delivery_scope: "test-scope",
        };

        let err = frontend
            .send_message(&chat, &"<a".repeat(5_000))
            .unwrap_err();

        assert!(err.to_string().contains("split_final_reply"));
        let err = frontend
            .stream_begin(&chat, &"<a".repeat(5_000), false)
            .unwrap_err();
        assert!(err.to_string().contains("split_final_reply"));
    }

    #[test]
    fn pre_rendered_card_rechecks_budget_after_mentions_expand() {
        let dir = tempfile::tempdir().unwrap();
        let fc = FeishuClient::new("feishu.cn", String::new(), String::new(), true);
        let frontend = test_frontend(&fc, dir.path());
        let chat = ChatRef {
            thread_anchor: None,
            chat_id: "oc_split",
            chat_type: "group",
            agent_dir: dir.path(),
            agent_name: "tester",
            delivery_scope: "test-scope",
        };
        let card = serde_json::json!({
            "schema": "2.0",
            "body": {"elements": [{"tag": "markdown", "content": "@bot ".repeat(1_000)}]},
        })
        .to_string();
        assert!(card.len() < summon_core::feishu::CARD_TOTAL_SAFE_LIMIT);

        let err = frontend.send_card(&chat, &card).unwrap_err();

        assert!(err.to_string().contains("send_card 超出飞书卡片预算"));
    }

    /// M1 连续 steer：多次同进程 handoff（同一 id 多次 patch）后终态 settle，映射归零。
    #[test]
    fn consecutive_handoffs_settle_to_zero_mappings() {
        let dir = tempfile::tempdir().unwrap();
        let fc = FeishuClient::new("feishu.cn", String::new(), String::new(), true);
        let frontend = test_frontend(&fc, dir.path());
        let chat = ChatRef {
            thread_anchor: None,
            chat_id: "oc_handoff_multi",
            chat_type: "group",
            agent_dir: dir.path(),
            agent_name: "tester",
            delivery_scope: "test-scope",
        };

        let card = summon_core::feishu::card_markdown_v2("第一轮内容");
        let id = frontend.stream_begin_card(&chat, &card).unwrap();
        assert_eq!(frontend.stream_map.borrow().len(), 1);

        // 模拟连续 3 次 steer：每次都是同一 id 续 patch
        for i in 0..3 {
            frontend
                .stream_map
                .borrow_mut()
                .get_mut(&id.0)
                .unwrap()
                .throttle = PatchThrottle::new(STREAM_PATCH_MIN_INTERVAL_MS);
            frontend
                .stream_patch_card(
                    id,
                    &summon_core::feishu::card_markdown_v2(&format!("续跑 {i}")),
                )
                .unwrap();
            assert_eq!(
                frontend.stream_map.borrow().len(),
                1,
                "第 {i} 次 steer 后仍只有一条映射"
            );
        }

        frontend.stream_finish(id, "完成").unwrap();
        assert_eq!(frontend.stream_map.borrow().len(), 0, "终态后映射归零");
    }

    /// 挂了预览服务后,终态文本里提到的本地文件变成回复末尾的按钮,正文里的文字链接只留文字;
    /// 流式快照里的路径是普通文字。不挂时逐字节不变(既有测试已覆盖,这里不重复)。
    #[test]
    fn preview_files_become_reply_buttons_in_text() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("report.md");
        std::fs::write(&file, "# 报告\n").unwrap();
        let fc = FeishuClient::new("feishu.cn", String::new(), String::new(), true);
        let frontend = test_frontend(&fc, dir.path()).with_preview_server(Some(
            crate::runtime::preview_server::PreviewServer::for_test(
                dir.path().join("tokens.json"),
                "http://10.0.0.1:7765",
                10 * 1024 * 1024,
            ),
        ));
        let tag = chat_tag("group", "oc_split");

        let body = format!("看 [报告]({}) 吧", file.display());
        let (rendered, buttons) = frontend.render_reply(&tag, &body);
        assert!(
            rendered.as_str().contains("看 报告 吧"),
            "{}",
            rendered.as_str()
        );
        assert!(
            !rendered.as_str().contains("http://"),
            "{}",
            rendered.as_str()
        );
        assert_eq!(buttons.len(), 1);
        assert_eq!(buttons[0].token.len(), 32);
        assert_eq!(buttons[0].label, "report.md");

        let live = frontend.render_streaming_text(&tag, &body);
        assert!(
            !live.as_str().contains("http://"),
            "流式文本不生成链接:{}",
            live.as_str()
        );

        // 不存在的路径原样保留、没有按钮(fail-closed)
        let (untouched, none) = frontend.render_reply(&tag, "看 [无](/tmp/不存在.md)");
        assert!(none.is_empty());
        assert!(
            untouched.as_str().contains("[无](/tmp/不存在.md)"),
            "不存在的路径原样保留:{}",
            untouched.as_str()
        );
    }

    /// 卡片 JSON 路径:markdown 元素(含折叠面板里的)提到的本地文件,在终态卡末尾合成一组按钮,
    /// 按钮只回传 `preview_open`;流式卡逐字节透传。
    #[test]
    fn preview_files_become_buttons_on_terminal_cards() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("data.json");
        std::fs::write(&file, "{}").unwrap();
        let fc = FeishuClient::new("feishu.cn", String::new(), String::new(), true);
        let frontend = test_frontend(&fc, dir.path()).with_preview_server(Some(
            crate::runtime::preview_server::PreviewServer::for_test(
                dir.path().join("tokens.json"),
                "http://10.0.0.1:7765",
                10 * 1024 * 1024,
            ),
        ));
        let tag = chat_tag("group", "oc_split");
        let link = format!("[数据]({})", file.display());
        let card = serde_json::json!({
            "schema": "2.0",
            "body": {"elements": [
                {"tag": "markdown", "content": format!("正文 {link}")},
                {"tag": "collapsible_panel", "header": {"title": {"tag": "markdown", "content": "思考"}},
                 "elements": [{"tag": "markdown", "content": format!("过程 `{}`", file.display())}]}
            ]},
        })
        .to_string();

        assert_eq!(frontend.render_streaming_card(&tag, &card), card);

        let terminal: serde_json::Value =
            serde_json::from_str(&frontend.render_mentions_in_card(&tag, &card)).unwrap();
        let elements = terminal["body"]["elements"].as_array().unwrap();
        assert_eq!(elements[0]["content"], "正文 数据", "文字链接只留文字");
        let row = elements.last().unwrap();
        assert_eq!(row["element_id"], "preview_files");
        let buttons = row["columns"].as_array().unwrap();
        assert_eq!(buttons.len(), 1, "正文与折叠面板里的同一文件只出一个按钮");
        let behaviors = buttons[0]["elements"][0]["behaviors"].as_array().unwrap();
        assert_eq!(behaviors.len(), 1, "不带打开链接:{behaviors:?}");
        assert_eq!(behaviors[0]["type"], "callback");
        assert_eq!(behaviors[0]["value"]["action"], "preview_open");

        let v1 = serde_json::json!({
            "config": {"wide_screen_mode": true},
            "elements": [{"tag": "markdown", "content": format!("正文 {link}")}],
        })
        .to_string();
        let rendered: serde_json::Value =
            serde_json::from_str(&frontend.render_mentions_in_card(&tag, &v1)).unwrap();
        assert_eq!(
            rendered["elements"][0]["content"],
            format!("正文 {link}"),
            "1.0 卡挂不了按钮,链接原样保留"
        );
        assert!(!rendered.to_string().contains("preview_files"));
    }

    // ---- CoT live 卡传输层 ----

    /// Cursor CLI steer 续跑:下一代的 segments 从 0 重新编号,同一条 CoT 上打边界后,
    /// 新一代的第一段思考必须完整推出,不能被上一代的段游标当成已推过。
    #[test]
    fn cot_restart_boundary_projects_the_next_generation_from_its_first_segment() {
        use summon_core::segment::TurnSegment;
        for visible in [true, false] {
            let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
            let base = format!("http://{}", server.server_addr().to_ip().unwrap());
            let seen: std::sync::Arc<std::sync::Mutex<Vec<String>>> = Default::default();
            let recorded = seen.clone();
            // 并行全量测试中客户端初始化可能超过 500ms;空闲不代表场景结束。
            // 由测试作用域持有服务生命周期,正常结束或 panic 都会断开通道让线程退出。
            let (server_lifetime, lifetime_rx) = std::sync::mpsc::channel::<()>();
            let server_thread = std::thread::spawn(move || {
                while matches!(
                    lifetime_rx.try_recv(),
                    Err(std::sync::mpsc::TryRecvError::Empty)
                ) {
                    let Some(mut request) = server
                        .recv_timeout(std::time::Duration::from_millis(100))
                        .unwrap()
                    else {
                        continue;
                    };
                    let url = request.url().to_string();
                    let mut body = String::new();
                    request.as_reader().read_to_string(&mut body).unwrap();
                    recorded.lock().unwrap().push(body);
                    let response = if url.contains("auth/v3/tenant_access_token") {
                        r#"{"tenant_access_token":"token","expire":7200}"#
                    } else if url.contains("/message_cot?receive_id_type=chat_id") {
                        r#"{"code":0,"data":{"cot_id":"cot_1","message_id":"om_cot"}}"#
                    } else {
                        r#"{"code":0,"data":{}}"#
                    };
                    request
                        .respond(tiny_http::Response::from_string(response))
                        .unwrap();
                }
            });

            let dir = tempfile::tempdir().unwrap();
            let fc = FeishuClient::with_test_base_connection_close(base);
            let frontend = test_frontend(&fc, dir.path()).with_cot_content_visible(visible);
            let chat = chat_ref(dir.path());
            let first = [
                TurnSegment::Thinking("第一代思考".into()),
                TurnSegment::Reply("第一代说明".into()),
                TurnSegment::Thinking("第一代收尾".into()),
            ];
            let id = frontend
                .live_begin(&chat, &summon_core::cot::CotView::new(&first, None))
                .unwrap();
            frontend
                .live_update(id, &summon_core::cot::CotView::new(&first, None))
                .unwrap();
            frontend
                .stream_restart_boundary(id, "—— 收到追加消息 ——\n\n")
                .unwrap();
            let next = [TurnSegment::Thinking("续跑思考".into())];
            frontend
                .live_update(id, &summon_core::cot::CotView::new(&next, None))
                .unwrap();
            frontend.stream_finish(id, "").unwrap();
            drop(server_lifetime);
            server_thread.join().unwrap();

            // 过程内容外不外显由上游投影决定;这里交来的段两种情况都要按新一代编号推出,
            // 只有分隔文案随外显开关走。
            let pushed = seen.lock().unwrap().join("\n");
            assert!(pushed.contains("续跑思考"), "visible={visible}");
            assert_eq!(
                pushed.contains("收到追加消息"),
                visible,
                "visible={visible}"
            );
        }
    }

    #[test]
    fn cot_and_final_reply_target_the_same_main_feed_message_without_creating_a_thread() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let base = format!("http://{}", server.server_addr().to_ip().unwrap());
        let seen: std::sync::Arc<std::sync::Mutex<Vec<(String, String)>>> = Default::default();
        let recorded = seen.clone();
        let server_thread = std::thread::spawn(move || loop {
            let mut request = server
                .recv_timeout(std::time::Duration::from_secs(2))
                .unwrap()
                .expect("CoT 与终态回复请求都应到达");
            let url = request.url().to_string();
            let mut body = String::new();
            request.as_reader().read_to_string(&mut body).unwrap();
            recorded.lock().unwrap().push((url.clone(), body));
            let (response, done) = if url.contains("auth/v3/tenant_access_token") {
                (r#"{"tenant_access_token":"token","expire":7200}"#, false)
            } else if url.contains("/message_cot?receive_id_type=chat_id") {
                (
                    r#"{"code":0,"data":{"cot_id":"cot_1","message_id":"om_cot"}}"#,
                    false,
                )
            } else if url.contains("/messages/om_origin/reply") {
                (r#"{"code":0,"data":{"message_id":"om_final"}}"#, true)
            } else {
                (r#"{"code":0,"data":{}}"#, false)
            };
            request
                .respond(tiny_http::Response::from_string(response))
                .unwrap();
            if done {
                break;
            }
        });

        let dir = tempfile::tempdir().unwrap();
        let fc = FeishuClient::with_test_base_connection_close(base);
        let frontend = test_frontend(&fc, dir.path()).with_reply_target("om_origin", false);
        let chat = chat_ref(dir.path());
        let view = summon_core::cot::CotView::new(&[], None);
        let id = frontend.live_begin(&chat, &view).unwrap();
        frontend.stream_finish(id, "最终正文").unwrap();
        frontend.send_message(&chat, "最终正文").unwrap();
        server_thread.join().unwrap();

        let calls = seen.lock().unwrap();
        let (_, cot_body) = calls
            .iter()
            .find(|(url, _)| url.contains("/message_cot?receive_id_type=chat_id"))
            .expect("应创建 CoT");
        let cot: serde_json::Value = serde_json::from_str(cot_body).unwrap();
        assert_eq!(cot["origin_message_id"], "om_origin");
        assert_eq!(cot["reply_in_thread"], false);
        let (_, reply_body) = calls
            .iter()
            .find(|(url, _)| url.contains("/messages/om_origin/reply"))
            .expect("终态应回复同一条消息");
        let reply: serde_json::Value = serde_json::from_str(reply_body).unwrap();
        assert_eq!(reply["reply_in_thread"], false);
    }

    #[test]
    fn hidden_cot_create_failure_never_falls_back_to_a_process_card() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let base = format!("http://{}", server.server_addr().to_ip().unwrap());
        let server_thread = std::thread::spawn(move || {
            for index in 0..2 {
                let request = server
                    .recv_timeout(std::time::Duration::from_secs(2))
                    .unwrap()
                    .expect("token 与 CoT create 应各到一次");
                let body = if index == 0 {
                    r#"{"tenant_access_token":"token","expire":7200}"#
                } else {
                    r#"{"code":99991672,"msg":"no permission"}"#
                };
                request
                    .respond(tiny_http::Response::from_string(body))
                    .unwrap();
            }
            assert!(
                server
                    .recv_timeout(std::time::Duration::from_millis(100))
                    .unwrap()
                    .is_none(),
                "关闭过程内容时不得创建 legacy 过程载体"
            );
        });

        let dir = tempfile::tempdir().unwrap();
        let fc = FeishuClient::with_test_base(base);
        let frontend = test_frontend(&fc, dir.path()).with_cot_content_visible(false);
        let chat = chat_ref(dir.path());
        let progress = live_progress();
        let view = summon_core::cot::CotView::new(&[], Some(&progress));
        assert!(frontend.live_begin(&chat, &view).is_err());
        server_thread.join().unwrap();
        assert!(frontend.stream_map.borrow().is_empty());
    }

    #[test]
    fn cot_initial_event_failure_never_creates_a_second_process_carrier() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let base = format!("http://{}", server.server_addr().to_ip().unwrap());
        let server_thread = std::thread::spawn(move || {
            for index in 0..4 {
                let request = server
                    .recv_timeout(std::time::Duration::from_secs(2))
                    .unwrap()
                    .expect("token、create、首次事件、complete 应各到一次");
                let body = match index {
                    0 => r#"{"tenant_access_token":"token","expire":7200}"#,
                    1 => r#"{"code":0,"data":{"cot_id":"cot_1","message_id":"om_cot"}}"#,
                    2 => r#"{"code":230001,"msg":"event rejected"}"#,
                    _ => r#"{"code":0,"data":{}}"#,
                };
                request
                    .respond(tiny_http::Response::from_string(body))
                    .unwrap();
            }
            assert!(
                server
                    .recv_timeout(std::time::Duration::from_millis(100))
                    .unwrap()
                    .is_none(),
                "归属已提交后不得再创建 legacy 过程载体"
            );
        });

        let dir = tempfile::tempdir().unwrap();
        let fc = FeishuClient::with_test_base(base);
        let frontend = test_frontend(&fc, dir.path());
        let chat = chat_ref(dir.path());
        let progress = live_progress();
        let view = summon_core::cot::CotView::new(&[], Some(&progress));
        let id = frontend.live_begin(&chat, &view).unwrap();
        {
            let map = frontend.stream_map.borrow();
            let StreamTransport::Cot(state) = &map[&id.0].transport else {
                panic!("create 成功后必须保留原 CoT 所有权")
            };
            assert!(state.append_failed);
            assert!(state.carrier.page.closed);
        }
        frontend.stream_finish(id, "").unwrap();
        server_thread.join().unwrap();
        assert!(frontend.stream_map.borrow().is_empty());
    }

    /// 原生 CoT 下终态回复是新发的消息:投递完后登记进「最近定格卡」槽,轮末之后才能在它
    /// 末尾原地追加说明。没发过消息就不登记,槽里原来的不动。
    #[test]
    fn the_sent_final_reply_is_registered_as_the_reply_card() {
        let server = std::sync::Arc::new(tiny_http::Server::http("127.0.0.1:0").unwrap());
        let base = format!("http://{}", server.server_addr().to_ip().unwrap());
        let serving = std::sync::Arc::clone(&server);
        let server_thread = std::thread::spawn(move || {
            // 客户端调用全是同步的:主线程做完就 unblock 收工,5s 只是失败时的兜底上限。
            while let Ok(Some(request)) = serving.recv_timeout(std::time::Duration::from_secs(5)) {
                let response = if request.url().contains("tenant_access_token") {
                    r#"{"code":0,"tenant_access_token":"token","expire":7200}"#
                } else {
                    r#"{"code":0,"data":{"message_id":"om_final"}}"#
                };
                request
                    .respond(tiny_http::Response::from_string(response))
                    .unwrap();
            }
        });
        let dir = tempfile::tempdir().unwrap();
        let fc = FeishuClient::with_test_base_connection_close(base);
        let slot: crate::frontend::SharedSettledCard = Default::default();
        let frontend = test_frontend(&fc, dir.path()).with_settled_card(slot.clone());
        let chat = chat_ref(dir.path());

        frontend.settle_last_sent_as_reply();
        assert!(slot.lock().unwrap().is_none(), "没发过消息就不登记");

        frontend.send_message(&chat, "这轮的结论").unwrap();
        frontend.settle_last_sent_as_reply();
        server.unblock();
        server_thread.join().unwrap();

        let settled = slot.lock().unwrap().clone().expect("已登记");
        assert_eq!(settled.message_id, "om_final");
        assert_eq!(settled.kind, crate::frontend::SettledCardKind::Reply);
        assert!(settled.card_json.contains("这轮的结论"));
    }

    fn chat_ref<'a>(dir: &'a std::path::Path) -> ChatRef<'a> {
        ChatRef {
            chat_id: "oc_split",
            chat_type: "group",
            agent_dir: dir,
            agent_name: "test",
            delivery_scope: "test-scope",
            thread_anchor: None,
        }
    }

    fn live_progress() -> summon_core::progress::TurnProgress {
        let mut p = summon_core::progress::TurnProgress::new().with_turn_id("turn-7");
        p.note_thinking("", 8);
        p.tool_started("cargo test");
        p.set_updated_label("12:09:33");
        p
    }

    #[test]
    fn terminal_card_without_peers_still_gets_feed_summary() {
        let dir = tempfile::tempdir().unwrap();
        let fc = FeishuClient::new("feishu.cn", String::new(), String::new(), true);
        let frontend = test_frontend(&fc, dir.path());
        let card =
            r#"{"schema":"2.0","body":{"elements":[{"tag":"markdown","content":"最终正文"}]}}"#;
        let rendered = frontend.render_mentions_in_card("g-no-peers", card);
        let value: serde_json::Value = serde_json::from_str(&rendered).unwrap();
        assert_eq!(value["config"]["summary"]["content"], "最终正文");
    }
    /// 假飞书记下的一跳:`(method, url, body)`。
    type RecordedCall = (String, String, String);

    /// 只记账的假飞书:第一跳发 token,其余一律回 `code=0` 并把 `(method, url, body)` 记下来。
    /// 收满 `expect` 跳就退出 —— 少收一跳会在 join 时超时失败,多发一跳会留在下一个 recv 上暴露。
    fn recording_feishu(expect: usize) -> (String, std::thread::JoinHandle<Vec<RecordedCall>>) {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let base = format!("http://{}", server.server_addr().to_ip().unwrap());
        let handle = std::thread::spawn(move || {
            let mut calls = Vec::new();
            for index in 0..expect {
                let mut request = server
                    .recv_timeout(std::time::Duration::from_secs(2))
                    .unwrap()
                    .expect("假飞书应收到预期跳数");
                if index == 0 {
                    request
                        .respond(tiny_http::Response::from_string(
                            r#"{"tenant_access_token":"token","expire":7200}"#,
                        ))
                        .unwrap();
                    continue;
                }
                let method = request.method().as_str().to_string();
                let url = request.url().to_string();
                let mut body = String::new();
                request.as_reader().read_to_string(&mut body).unwrap();
                calls.push((method, url, body));
                request
                    .respond(tiny_http::Response::from_string(
                        r#"{"code":0,"data":{"cot_id":"cot_new","message_id":"om_new"}}"#,
                    ))
                    .unwrap();
            }
            calls
        });
        (base, handle)
    }

    fn counted_page(cot: &str, mid: &str, text: &str) -> CotPage {
        let mut projector = summon_core::cot::CotProjector::new("oc_reattach", "turn-1", "任务");
        let events = projector.project(
            &[summon_core::segment::TurnSegment::Thinking(text.into())],
            None,
            "1000",
        );
        let (_, consumed, wire) = summon_core::cot::paging::CotWireState::default().batch(&events);
        assert_eq!(consumed, events.len());
        CotPage {
            cot_id: cot.into(),
            message_id: mid.into(),
            closed: false,
            creation_pending: false,
            wire,
        }
    }

    fn paging_feishu(
        failure: Option<&'static str>,
    ) -> (
        String,
        std::sync::Arc<tiny_http::Server>,
        std::thread::JoinHandle<Vec<RecordedCall>>,
    ) {
        let server = std::sync::Arc::new(tiny_http::Server::http("127.0.0.1:0").unwrap());
        let base = format!("http://{}", server.server_addr().to_ip().unwrap());
        let serving = server.clone();
        let handle = std::thread::spawn(move || {
            let mut calls = Vec::new();
            let mut page = 0;
            while let Ok(Some(mut request)) =
                serving.recv_timeout(std::time::Duration::from_secs(3))
            {
                let url = request.url().to_string();
                if url.contains("tenant_access_token") {
                    request
                        .respond(tiny_http::Response::from_string(
                            r#"{"tenant_access_token":"token","expire":7200}"#,
                        ))
                        .unwrap();
                    continue;
                }
                let method = request.method().as_str().to_string();
                let mut body = String::new();
                request.as_reader().read_to_string(&mut body).unwrap();
                let create = method == "POST" && !url.contains("complete");
                if create {
                    page += 1;
                }
                let fail = match failure {
                    Some("create") => create,
                    Some("close") => url.contains("complete"),
                    Some("prelude") => method == "PUT" && body.contains("cot_page1"),
                    _ => false,
                };
                calls.push((method, url, body));
                let response = if fail {
                    String::new()
                } else if create {
                    format!(
                        r#"{{"code":0,"data":{{"cot_id":"cot_page{page}","message_id":"om_page{page}"}}}}"#
                    )
                } else {
                    r#"{"code":0}"#.into()
                };
                request
                    .respond(tiny_http::Response::from_string(response))
                    .unwrap();
            }
            calls
        });
        (base, server, handle)
    }

    fn full_cot_wire() -> summon_core::cot::paging::CotWireState {
        let mut wire = summon_core::cot::paging::CotWireState::default();
        let events = (0..900)
            .map(|_| cot_event("REASONING_MESSAGE_CONTENT"))
            .collect::<Vec<_>>();
        for batch in events.chunks(50) {
            let (_, consumed, next) = wire.batch(batch);
            assert_eq!(consumed, batch.len());
            wire = next;
        }
        wire
    }

    #[test]
    fn cot_paging_delivers_late_content_and_checkpoints_the_active_carrier() {
        let (base, server, handle) = paging_feishu(None);
        let fc = FeishuClient::with_test_base_connection_close(base);
        let dir = tempfile::tempdir().unwrap();
        let record = follow_record();
        let path = FeishuFrontend::publish_cot_recovery(dir.path(), &record).unwrap();
        let mut carrier = test_carrier("cot_follow", "om_follow", "g-follow", Some(path.clone()));
        carrier.route = Some(CotRoute {
            chat_id: "oc_route".into(),
            reply_to: Some(("om_origin".into(), true)),
        });
        let mut projector = summon_core::cot::CotProjector::new("thread", "run", "query");
        let mut events = projector.project(
            &[summon_core::segment::TurnSegment::Thinking("first".into())],
            None,
            "1000",
        );
        let reasoning: serde_json::Value = serde_json::from_str(
            &events
                .iter()
                .find(|e| e.event_type == "REASONING_MESSAGE_START")
                .unwrap()
                .content,
        )
        .unwrap();
        for i in 0..1800 {
            events.push(summon_core::cot::CotEvent { event_type: "REASONING_MESSAGE_CONTENT".into(),
                content: serde_json::json!({"messageId":reasoning["messageId"],"delta":format!("part-{i}")}).to_string(),
                timestamp: "1000".into(), reply_delivery: None });
        }
        carrier.push(&fc, &events).unwrap();
        carrier.snapshot(&projector);
        let saved = FeishuFrontend::read_cot_recovery(&path).unwrap();
        let active = saved.page.as_ref().unwrap();
        assert_eq!(saved.cot_id, "cot_follow");
        assert_eq!(active.cot_id, "cot_page2");
        assert!(active.wire.events() < 900);
        assert!(!saved.append_failed);
        assert_eq!(
            std::os::unix::fs::PermissionsExt::mode(
                &std::fs::metadata(&path).unwrap().permissions()
            ) & 0o777,
            0o600
        );
        carrier.release(false);

        // Restart uses the current page and its capacity, while the durable root anchor stays stable.
        let frontend = test_frontend(&fc, dir.path());
        let segments = [summon_core::segment::TurnSegment::Thinking(
            "first and new".into(),
        )];
        let anchor = summon_core::TurnAnchorCot {
            cot_id: "cot_follow".into(),
            message_id: "om_follow".into(),
        };
        let id = frontend
            .stream_begin_reattached(
                &reattach_chat(dir.path()),
                &summon_core::cot::CotView::new(&segments, None),
                &anchor,
            )
            .unwrap();
        assert_eq!(
            frontend.stream_cot_handle(id),
            Some(("cot_follow".into(), "om_follow".into()))
        );
        let restored = FeishuFrontend::read_cot_recovery(&path).unwrap();
        assert!(restored.page.as_ref().unwrap().wire.events() > active.wire.events());
        assert_eq!(restored.page.unwrap().cot_id, "cot_page2");
        server.unblock();
        let calls = handle.join().unwrap();
        let mut counts = std::collections::BTreeMap::<String, usize>::new();
        let mut deltas = Vec::new();
        for (method, url, body) in &calls {
            if method == "POST" && !url.contains("complete") {
                let body: serde_json::Value = serde_json::from_str(body).unwrap();
                assert_eq!(body["receive_id"], "oc_route");
                assert_eq!(body["origin_message_id"], "om_origin");
                assert_eq!(body["reply_in_thread"], true);
            }
            if method != "PUT" {
                continue;
            }
            let body: serde_json::Value = serde_json::from_str(body).unwrap();
            let batch = body["events"].as_array().unwrap();
            assert!(batch.len() <= 50);
            *counts
                .entry(body["cot_id"].as_str().unwrap().into())
                .or_default() += batch.len();
            for event in batch {
                let content: serde_json::Value =
                    serde_json::from_str(event["content"].as_str().unwrap()).unwrap();
                if let Some(delta) = content["delta"].as_str().filter(|s| s.starts_with("part-")) {
                    deltas.push(delta.to_owned());
                }
            }
        }
        assert!(counts.values().all(|count| *count <= 900));
        assert_eq!(counts.len(), 3);
        assert_eq!(
            deltas,
            (0..1800).map(|i| format!("part-{i}")).collect::<Vec<_>>()
        );
        assert_eq!(
            calls
                .iter()
                .filter(|(m, u, _)| m == "POST" && !u.contains("complete"))
                .count(),
            2
        );
        assert!(calls.last().unwrap().2.contains("cot_page2"));
    }

    #[test]
    fn cot_paging_latches_close_create_and_prelude_failures_without_retry() {
        for failure in ["close", "create", "prelude"] {
            let (base, server, handle) = paging_feishu(Some(failure));
            let fc = FeishuClient::with_test_base_connection_close(base);
            let dir = tempfile::tempdir().unwrap();
            let path = FeishuFrontend::publish_cot_recovery(dir.path(), &follow_record()).unwrap();
            let frontend = test_frontend(&fc, dir.path());
            let id = install_detachable_cot(
                &frontend,
                Some(path.clone()),
                vec![cot_event("REASONING_MESSAGE_CONTENT")],
                false,
            );
            {
                let mut map = frontend.stream_map.borrow_mut();
                let StreamTransport::Cot(state) = &mut map.get_mut(&id.0).unwrap().transport else {
                    panic!("cot")
                };
                state.carrier.page.wire = full_cot_wire();
                state.carrier.route = Some(CotRoute {
                    chat_id: "oc_route".into(),
                    reply_to: None,
                });
            }
            assert!(frontend.flush_cot(id).is_err());
            assert!(frontend.flush_cot(id).is_err());
            let saved = FeishuFrontend::read_cot_recovery(&path).unwrap();
            assert!(saved.append_failed);
            assert_eq!(
                saved.page.as_ref().unwrap().creation_pending,
                failure == "create"
            );
            let anchor = summon_core::TurnAnchorCot {
                cot_id: saved.cot_id.clone(),
                message_id: saved.message_id.clone(),
            };
            assert!(frontend
                .stream_begin_reattached(
                    &reattach_chat(dir.path()),
                    &summon_core::cot::CotView::new(&[], None),
                    &anchor
                )
                .is_err());
            assert!(path.exists());
            server.unblock();
            let calls = handle.join().unwrap();
            let creates = calls
                .iter()
                .filter(|(m, u, _)| m == "POST" && !u.contains("complete"))
                .count();
            assert_eq!(creates, usize::from(failure != "close"));
            assert_eq!(
                calls.iter().filter(|(m, _, _)| m == "PUT").count(),
                usize::from(failure == "prelude")
            );
            assert_eq!(
                calls
                    .iter()
                    .filter(|(_, u, _)| u.contains("complete"))
                    .count(),
                1
            );
            assert!(calls
                .iter()
                .all(|(_, _, body)| !body.contains("private-args")));
        }
    }

    #[test]
    fn cot_paging_recovery_closes_the_current_page_and_preserves_unknown_creation() {
        let (base, server, handle) = paging_feishu(None);
        let fc = FeishuClient::with_test_base_connection_close(base);
        let dir = tempfile::tempdir().unwrap();
        let mut record = follow_record();
        record.generation = "old-generation".into();
        let mut page = counted_page("cot_current", "om_current", "body");
        page.creation_pending = true;
        record.page = Some(page);
        record.append_failed = true;
        let path = FeishuFrontend::persist_cot_recovery(dir.path(), &record).unwrap();
        finalize_orphan_cot_messages(&fc, dir.path(), &|_, _| false);
        assert!(path.exists());
        record.page.as_mut().unwrap().creation_pending = false;
        FeishuFrontend::persist_cot_recovery(dir.path(), &record).unwrap();
        finalize_orphan_cot_messages(&fc, dir.path(), &|_, _| false);
        assert!(!path.exists());
        server.unblock();
        let calls = handle.join().unwrap();
        assert_eq!(calls.len(), 1);
        assert!(calls[0]
            .1
            .contains("/complete/cot_current?message_id=om_current"));
        assert!(!calls[0].1.contains("cot_follow"));
    }

    #[test]
    fn cot_paging_requires_a_durable_checkpoint_before_creating_a_segment() {
        let (base, server, handle) = paging_feishu(None);
        let fc = FeishuClient::with_test_base_connection_close(base);
        let dir = tempfile::tempdir().unwrap();
        let mut carrier = test_carrier(
            "cot_root",
            "om_root",
            "g-test",
            Some(dir.path().join("missing.json")),
        );
        carrier.page.wire = full_cot_wire();
        carrier.route = Some(CotRoute {
            chat_id: "oc_route".into(),
            reply_to: None,
        });
        assert!(carrier
            .push(&fc, &[cot_event("REASONING_MESSAGE_CONTENT")])
            .is_err());
        carrier.recovery_path = None;
        assert!(carrier
            .push(&fc, &[cot_event("REASONING_MESSAGE_CONTENT")])
            .is_err());
        server.unblock();
        assert!(handle.join().unwrap().is_empty());
    }

    fn projector_after(text: &str) -> summon_core::cot::CotProjector {
        projector_after_segments(&[text])
    }

    fn projector_after_segments(texts: &[&str]) -> summon_core::cot::CotProjector {
        let segments = texts
            .iter()
            .map(|text| summon_core::segment::TurnSegment::Thinking((*text).into()))
            .collect::<Vec<_>>();
        let mut projector = summon_core::cot::CotProjector::new("oc_reattach", "turn-1", "任务");
        projector.project(&segments, None, "1000");
        projector
    }

    fn reattach_chat<'a>(dir: &'a std::path::Path) -> ChatRef<'a> {
        ChatRef {
            thread_anchor: None,
            chat_id: "oc_reattach",
            chat_type: "group",
            agent_dir: dir,
            agent_name: "tester",
            delivery_scope: "test-scope",
        }
    }

    /// 快照必须跟着每一次成功的 push 走。停在开卡那一刻的快照会让接管轮把中间那段思考重推一遍。
    #[test]
    fn flush_cot_rewrites_recovery_record_with_projector_snapshot() {
        let (base, server) = recording_feishu(2);
        let dir = tempfile::tempdir().unwrap();
        let record = CotRecoveryRecord {
            generation: cot_process_generation().to_string(),
            cot_id: "cot_1".into(),
            message_id: "om_cot".into(),
            legacy_control_mid: None,
            cot_closed: false,
            append_failed: false,
            chat_tag: Some("g-test".into()),
            projector: None,
            page: None,
        };
        let path = FeishuFrontend::publish_cot_recovery(dir.path(), &record).unwrap();
        let fc = FeishuClient::with_test_base_connection_close(base);
        let frontend = test_frontend(&fc, dir.path());
        let id = StreamId(91);
        frontend.stream_map.borrow_mut().insert(
            id.0,
            StreamState {
                mid: "om_cot".into(),
                chat_tag: "g-test".into(),
                throttle: PatchThrottle::primed(COT_UPDATE_MIN_INTERVAL_MS, 0),
                with_interrupt: true,
                transport: StreamTransport::Cot(Box::new(CotState {
                    carrier: test_carrier("cot_1", "om_cot", "g-test", Some(path.clone())),
                    projector: projector_after("想了一段"),
                    pending: vec![cot_event("REASONING_MESSAGE_CONTENT")],
                    confirmed_replies: Vec::new(),
                    next_batch: 0,
                    append_failed: false,
                })),
            },
        );

        frontend.flush_cot(id).unwrap();
        server.join().unwrap();

        let saved: CotRecoveryRecord =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(saved.chat_tag.as_deref(), Some("g-test"));
        assert!(
            saved.projector.is_some(),
            "push 成功后必须把投影器游标写回记录"
        );
        FeishuFrontend::retire_cot_recovery(&path, true);
    }

    fn install_detachable_cot(
        frontend: &FeishuFrontend<'_>,
        recovery_path: Option<std::path::PathBuf>,
        pending: Vec<summon_core::cot::CotEvent>,
        append_failed: bool,
    ) -> StreamId {
        let id = StreamId(93);
        frontend.stream_map.borrow_mut().insert(
            id.0,
            StreamState {
                mid: "om_follow".into(),
                chat_tag: "g-follow".into(),
                throttle: PatchThrottle::primed(COT_UPDATE_MIN_INTERVAL_MS, 0),
                with_interrupt: true,
                transport: StreamTransport::Cot(Box::new(CotState {
                    carrier: test_carrier("cot_follow", "om_follow", "g-follow", recovery_path),
                    projector: projector_after("主 agent 想完了"),
                    pending,
                    confirmed_replies: Vec::new(),
                    next_batch: 0,
                    append_failed,
                })),
            },
        );
        id
    }

    fn follow_record() -> CotRecoveryRecord {
        CotRecoveryRecord {
            generation: cot_process_generation().to_string(),
            cot_id: "cot_follow".into(),
            message_id: "om_follow".into(),
            legacy_control_mid: None,
            cot_closed: false,
            append_failed: false,
            chat_tag: Some("g-follow".into()),
            projector: None,
            page: None,
        }
    }

    /// 假飞书:这段时间里一个请求都不该来。
    fn silent_feishu() -> (String, std::thread::JoinHandle<()>) {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let base = format!("http://{}", server.server_addr().to_ip().unwrap());
        let handle = std::thread::spawn(move || {
            assert!(
                server
                    .recv_timeout(std::time::Duration::from_millis(150))
                    .unwrap()
                    .is_none(),
                "跟随中的 CoT 不该被收口"
            );
        });
        (base, handle)
    }

    /// 带预览的那一条发成 2.0 卡:会话列表只认 2.0 卡的 `config.summary`。
    #[test]
    fn message_with_feed_is_sent_as_a_v2_card_carrying_the_preview() {
        let (base, server) = recording_feishu(2);
        let dir = tempfile::tempdir().unwrap();
        let fc = FeishuClient::with_test_base_connection_close(base);
        let frontend = test_frontend(&fc, dir.path());
        let chat = ChatRef {
            thread_anchor: None,
            chat_id: "oc_split",
            chat_type: "group",
            agent_dir: dir.path(),
            agent_name: "tester",
            delivery_scope: "test-scope",
        };

        frontend
            .send_message_with_feed(&chat, "末段正文", "长报告已发出")
            .unwrap();

        let calls = server.join().unwrap();
        assert_eq!(calls.len(), 1, "{calls:?}");
        let body: serde_json::Value = serde_json::from_str(&calls[0].2).unwrap();
        let card: serde_json::Value =
            serde_json::from_str(body["content"].as_str().unwrap()).unwrap();
        assert_eq!(card["schema"], "2.0");
        assert_eq!(card["config"]["summary"]["content"], "长报告已发出");
        assert!(card.to_string().contains("末段正文"));
        assert!(frontend
            .last_sent
            .borrow()
            .as_ref()
            .is_some_and(|(_, sent)| sent.contains("长报告已发出")));
    }

    /// 摘出:待发事件先推出去,这条流从本轮前端消失;恢复记录仍归本进程所有 —— 本轮前端析构、
    /// 下一轮开新卡前的反熵都不收它。
    #[test]
    fn detached_cot_flushes_pending_and_stays_owned_by_this_process() {
        let (base, server) = recording_feishu(2);
        let dir = tempfile::tempdir().unwrap();
        let path = FeishuFrontend::publish_cot_recovery(dir.path(), &follow_record()).unwrap();
        let fc = FeishuClient::with_test_base_connection_close(base);
        let frontend = test_frontend(&fc, dir.path());
        let id = install_detachable_cot(
            &frontend,
            Some(path.clone()),
            vec![cot_event("REASONING_MESSAGE_CONTENT")],
            false,
        );

        let detached = frontend.stream_detach_cot(id).expect("正常的 CoT 可以摘出");
        let calls = server.join().unwrap();
        assert_eq!(calls.len(), 1, "待发事件先推完:{calls:?}");
        assert!(calls[0].2.contains("cot_follow"), "{calls:?}");
        assert!(!frontend.stream_map.borrow().contains_key(&id.0));
        assert!(!frontend.stream_is_process_only(id));
        assert_eq!(detached.carrier.cot_id, "cot_follow");
        assert_eq!(detached.carrier.message_id, "om_follow");
        let saved: CotRecoveryRecord =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert!(saved.projector.is_some(), "摘出前的推送也要落快照");

        drop(frontend);
        assert!(
            FeishuFrontend::cot_recovery_path_is_active(&path, cot_process_generation()),
            "本轮前端析构不得释放跟随中的记录"
        );
        let (base, server) = silent_feishu();
        let next_fc = FeishuClient::with_test_base_connection_close(base);
        test_frontend(&next_fc, dir.path()).drain_stale_cot_recoveries(dir.path());
        server.join().unwrap();
        assert!(path.exists());

        detached.carrier.release(true);
        assert!(!path.exists(), "收口后记录删掉");
        assert!(!FeishuFrontend::cot_recovery_path_is_active(
            &path,
            cot_process_generation()
        ));
    }

    /// append 已失败的载体处在不确定终态:不摘,照常由本轮收口。
    #[test]
    fn a_cot_whose_append_failed_is_not_detached() {
        let dir = tempfile::tempdir().unwrap();
        let fc = FeishuClient::new("feishu.cn", String::new(), String::new(), true);
        let frontend = test_frontend(&fc, dir.path());
        let id = install_detachable_cot(&frontend, None, Vec::new(), true);
        assert!(frontend.stream_detach_cot(id).is_none());
        assert!(frontend.stream_is_process_only(id), "留给本轮收口");

        let text = StreamId(94);
        frontend.stream_map.borrow_mut().insert(
            text.0,
            StreamState {
                mid: "om_text".into(),
                chat_tag: "g-test".into(),
                throttle: PatchThrottle::primed(COT_UPDATE_MIN_INTERVAL_MS, 0),
                with_interrupt: true,
                transport: StreamTransport::Text,
            },
        );
        assert!(frontend.stream_detach_cot(text).is_none(), "文本流没有跟随");
    }

    /// 摘出前那次推送失败:载体进入不确定终态,不摘。
    #[test]
    fn a_failed_flush_keeps_the_cot_for_the_normal_settle() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let base = format!("http://{}", server.server_addr().to_ip().unwrap());
        let server_thread = std::thread::spawn(move || {
            for index in 0..2 {
                let request = server
                    .recv_timeout(std::time::Duration::from_secs(2))
                    .unwrap()
                    .expect("token + push");
                let body = if index == 0 {
                    r#"{"tenant_access_token":"token","expire":7200}"#
                } else {
                    r#"{"code":230001,"msg":"retry"}"#
                };
                request
                    .respond(tiny_http::Response::from_string(body))
                    .unwrap();
            }
        });
        let dir = tempfile::tempdir().unwrap();
        let fc = FeishuClient::with_test_base_connection_close(base);
        let frontend = test_frontend(&fc, dir.path());
        let id = install_detachable_cot(
            &frontend,
            None,
            vec![cot_event("REASONING_MESSAGE_CONTENT")],
            false,
        );
        assert!(frontend.stream_detach_cot(id).is_none());
        server_thread.join().unwrap();
        let map = frontend.stream_map.borrow();
        let StreamTransport::Cot(state) = &map[&id.0].transport else {
            panic!("CoT 状态应留在本轮")
        };
        assert!(state.append_failed);
    }

    /// 跟随线程用的载体:分批推、推完落快照、收口后释放记录。
    #[test]
    fn detached_carrier_pushes_in_batches_and_snapshots_the_projector() {
        let (base, server) = recording_feishu(3);
        let dir = tempfile::tempdir().unwrap();
        let path = FeishuFrontend::publish_cot_recovery(dir.path(), &follow_record()).unwrap();
        let mut carrier = test_carrier("cot_follow", "om_follow", "g-follow", Some(path.clone()));
        let fc = FeishuClient::with_test_base_connection_close(base);
        let events = (0..51)
            .map(|_| cot_event("REASONING_MESSAGE_CONTENT"))
            .collect::<Vec<_>>();
        carrier.push(&fc, &events).unwrap();
        let calls = server.join().unwrap();
        assert_eq!(calls.len(), 2, "51 条分两批");

        carrier.snapshot(&projector_after("子 agent 在跑"));
        let saved: CotRecoveryRecord =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(saved.chat_tag.as_deref(), Some("g-follow"));
        assert!(saved.projector.is_some());
        assert!(!saved.cot_closed);

        carrier.release(false);
        assert!(path.exists(), "没收口成功的记录留给反熵重试");
        assert!(!FeishuFrontend::cot_recovery_path_is_active(
            &path,
            cot_process_generation()
        ));
    }

    /// 启动收口对「正在被接管」的那条记录一个字节都不许发:关掉就等于把接管轮的过程账本
    /// 提前封了口。其余的照收,但收口理由是 `done` —— 停机不是错误。
    #[test]
    fn finalize_orphan_cot_skips_records_kept_by_predicate_and_closes_others_as_done() {
        let (base, server) = recording_feishu(3);
        let dir = tempfile::tempdir().unwrap();
        let kept = CotRecoveryRecord {
            generation: "old-generation".into(),
            cot_id: "cot_keep".into(),
            message_id: "om_keep".into(),
            legacy_control_mid: None,
            cot_closed: false,
            append_failed: false,
            chat_tag: Some("g-keep".into()),
            projector: Some(projector_after("接管轮要接着写")),
            page: None,
        };
        let closed = CotRecoveryRecord {
            generation: "old-generation".into(),
            cot_id: "cot_close".into(),
            message_id: "om_close".into(),
            legacy_control_mid: None,
            cot_closed: false,
            append_failed: false,
            chat_tag: Some("g-close".into()),
            projector: Some(projector_after("停机时想到一半")),
            page: None,
        };
        let kept_path = FeishuFrontend::persist_cot_recovery(dir.path(), &kept).unwrap();
        let closed_path = FeishuFrontend::persist_cot_recovery(dir.path(), &closed).unwrap();
        let fc = FeishuClient::with_test_base_connection_close(base);

        finalize_orphan_cot_messages(&fc, dir.path(), &|tag, cot_id| {
            tag == "g-keep" && cot_id == "cot_keep"
        });
        let calls = server.join().unwrap();

        assert!(kept_path.exists(), "接管中的记录不得被删");
        assert!(!closed_path.exists());
        assert!(
            calls
                .iter()
                .all(|(_, url, body)| !url.contains("cot_keep") && !body.contains("cot_keep")),
            "接管中的 CoT 一个请求都不该发:{calls:?}"
        );
        let (_, complete_url, _) = calls
            .iter()
            .find(|(_, url, _)| url.contains("complete"))
            .expect("被跳过之外的记录必须收口");
        assert!(complete_url.contains("cot_close"));
        assert!(
            complete_url.contains("reason=done"),
            "停机不是错误:{complete_url}"
        );
        let (_, _, note_body) = calls
            .iter()
            .find(|(method, _, _)| method == "PUT")
            .expect("收口前先说明一句为什么停在这儿");
        assert!(note_body.contains("停机中断"), "{note_body}");
    }

    /// 跟随中的 CoT 随 daemon 停机:它的记录属于上一代、没有接管轮认领(轮末已清锚点),
    /// 启动收口照常收掉 —— 在播的子 agent 块先收起,再写停机说明、`done` 收口。
    #[test]
    fn a_follower_record_left_by_a_stopped_daemon_is_closed_at_boot() {
        let (base, server) = recording_feishu(3);
        let dir = tempfile::tempdir().unwrap();
        let mut progress = summon_core::progress::TurnProgress::new().with_tool_details(true);
        progress.set_parent_waiting(true);
        progress.subagent_started("toolu_async", "Agent Explore", None);
        progress.subagent_tool_started(
            "toolu_async",
            "toolu_sub",
            summon_core::progress::ToolCallInput::new("git status", "Bash").command("git status"),
        );
        let mut projector = summon_core::cot::CotProjector::new("oc_follow", "turn-1", "任务");
        projector.project(&[], Some(&progress), "1000");
        let record = CotRecoveryRecord {
            generation: "previous-daemon".into(),
            cot_id: "cot_follow".into(),
            message_id: "om_follow".into(),
            legacy_control_mid: None,
            cot_closed: false,
            append_failed: false,
            chat_tag: Some("g-follow".into()),
            projector: Some(projector),
            page: None,
        };
        let path = FeishuFrontend::persist_cot_recovery(dir.path(), &record).unwrap();
        let fc = FeishuClient::with_test_base_connection_close(base);

        finalize_orphan_cot_messages(&fc, dir.path(), &|_, _| false);
        let calls = server.join().unwrap();

        assert!(!path.exists());
        let (_, _, note) = calls
            .iter()
            .find(|(method, _, _)| method == "PUT")
            .expect("先写停机说明");
        assert!(note.contains("停机中断"), "{note}");
        assert!(
            note.contains("REASONING_MESSAGE_END"),
            "在播的块先收起:{note}"
        );
        let (_, complete, _) = calls
            .iter()
            .find(|(_, url, _)| url.contains("complete"))
            .expect("收口");
        assert!(complete.contains("cot_follow") && complete.contains("reason=done"));
    }

    /// 接管轮在**同一条** CoT 上续:不新建载体,首批事件带段边界文案。
    #[test]
    fn stream_begin_reattached_pushes_resume_boundary_then_continues_same_cot() {
        let (base, server) = recording_feishu(2);
        let dir = tempfile::tempdir().unwrap();
        let record = CotRecoveryRecord {
            generation: "old-generation".into(),
            cot_id: "cot_old".into(),
            message_id: "om_old".into(),
            legacy_control_mid: None,
            cot_closed: false,
            append_failed: false,
            chat_tag: Some(chat_tag("group", "oc_reattach")),
            projector: Some(projector_after("停机前想到一半")),
            page: Some(counted_page("cot_old", "om_old", "停机前想到一半")),
        };
        let path = FeishuFrontend::persist_cot_recovery(dir.path(), &record).unwrap();
        let fc = FeishuClient::with_test_base_connection_close(base);
        let frontend = test_frontend(&fc, dir.path());
        let chat = reattach_chat(dir.path());
        let segments = [summon_core::segment::TurnSegment::Thinking(
            "停机前想到一半,现在接着".into(),
        )];
        let view = summon_core::cot::CotView::new(&segments, None);
        let anchor = summon_core::TurnAnchorCot {
            cot_id: "cot_old".into(),
            message_id: "om_old".into(),
        };

        let id = frontend
            .stream_begin_reattached(&chat, &view, &anchor)
            .unwrap();
        let calls = server.join().unwrap();

        assert!(
            calls.iter().all(|(method, _, _)| method == "PUT"),
            "接管不得新建 CoT 载体:{calls:?}"
        );
        assert!(
            calls.iter().any(|(_, _, body)| body.contains("曾中断")),
            "首批必须带段边界文案:{calls:?}"
        );
        assert_eq!(frontend.stream_cot_handle(id).unwrap().0, "cot_old");
        assert!(path.exists(), "接管后记录仍归本代所有");
    }

    /// 思考内容不外显时,段边界连同思考一起不渲 —— 与 steer 一致:用户看不到过程,给他一条
    /// 孤零零的分隔线只是噪音。载体本身照续,不因此另开一条。
    #[test]
    fn hidden_cot_content_reattaches_in_place_without_a_resume_boundary() {
        let (base, server) = recording_feishu(2);
        let dir = tempfile::tempdir().unwrap();
        let record = CotRecoveryRecord {
            generation: "old-generation".into(),
            cot_id: "cot_old".into(),
            message_id: "om_old".into(),
            legacy_control_mid: None,
            cot_closed: false,
            append_failed: false,
            chat_tag: Some(chat_tag("group", "oc_reattach")),
            projector: Some(projector_after("停机前想到一半")),
            page: Some(counted_page("cot_old", "om_old", "停机前想到一半")),
        };
        FeishuFrontend::persist_cot_recovery(dir.path(), &record).unwrap();
        let fc = FeishuClient::with_test_base_connection_close(base);
        let frontend = test_frontend(&fc, dir.path()).with_cot_content_visible(false);
        let chat = reattach_chat(dir.path());
        let segments = [summon_core::segment::TurnSegment::Thinking(
            "停机前想到一半,现在接着".into(),
        )];
        let view = summon_core::cot::CotView::new(&segments, None);
        let anchor = summon_core::TurnAnchorCot {
            cot_id: "cot_old".into(),
            message_id: "om_old".into(),
        };

        let id = frontend
            .stream_begin_reattached(&chat, &view, &anchor)
            .unwrap();
        let calls = server.join().unwrap();

        assert!(
            calls.iter().all(|(method, _, _)| method == "PUT"),
            "仍在同一条载体上续,不得新建:{calls:?}"
        );
        assert!(
            calls.iter().all(|(_, _, body)| !body.contains("曾中断")),
            "过程不外显时不渲段边界:{calls:?}"
        );
        assert_eq!(frontend.stream_cot_handle(id).unwrap().0, "cot_old");
    }

    /// 段落对齐不变量:接管轮从同一个 `consumed_boundary` 起重解析转录,产出的应是同一份
    /// segments 前缀。重建出来的比快照游标还少 = 转录被截断 / 解析规则变了,续投会把上一代的
    /// 段接到对不上的游标上 —— 收口旧的、开新的。
    #[test]
    fn stream_begin_reattached_falls_back_when_rebuilt_segments_lag_the_cursor() {
        let (base, server) = recording_feishu(5);
        let dir = tempfile::tempdir().unwrap();
        let record = CotRecoveryRecord {
            generation: "old-generation".into(),
            cot_id: "cot_old".into(),
            message_id: "om_old".into(),
            legacy_control_mid: None,
            cot_closed: false,
            append_failed: false,
            chat_tag: Some(chat_tag("group", "oc_reattach")),
            // 快照游标停在第 2 段(投影把最后一段留作 open,游标 = 段数 - 1)。
            projector: Some(projector_after_segments(&[
                "想了第一段",
                "想了第二段",
                "想了第三段",
            ])),
            page: None,
        };
        let path = FeishuFrontend::persist_cot_recovery(dir.path(), &record).unwrap();
        let fc = FeishuClient::with_test_base_connection_close(base);
        let frontend = test_frontend(&fc, dir.path());
        let chat = reattach_chat(dir.path());
        // 重解析只产出 1 段:比游标少。
        let segments = [summon_core::segment::TurnSegment::Thinking(
            "想了第一段".into(),
        )];
        let view = summon_core::cot::CotView::new(&segments, None);
        let anchor = summon_core::TurnAnchorCot {
            cot_id: "cot_old".into(),
            message_id: "om_old".into(),
        };

        let id = frontend
            .stream_begin_reattached(&chat, &view, &anchor)
            .unwrap();
        let calls = server.join().unwrap();

        let (_, complete_url, _) = calls
            .iter()
            .find(|(_, url, _)| url.contains("complete"))
            .expect("对不齐就必须收口旧载体");
        assert!(complete_url.contains("cot_old") && complete_url.contains("reason=done"));
        assert_eq!(
            calls
                .iter()
                .filter(|(method, url, _)| method == "POST" && !url.contains("complete"))
                .count(),
            1,
            "退回新 CoT:{calls:?}"
        );
        assert!(
            calls.iter().all(|(_, _, body)| !body.contains("曾中断")),
            "对不齐的段一个字节都不许续进旧载体:{calls:?}"
        );
        assert_eq!(frontend.stream_cot_handle(id).unwrap().0, "cot_new");
        assert!(!path.exists(), "旧记录已收口,不留给下一次扫描");
    }

    /// 没有快照就证不了「推到哪了」,续投等于重推:收口旧的、开新的 —— 一条新 CoT 而已。
    #[test]
    fn stream_begin_reattached_without_snapshot_falls_back_to_new_cot() {
        let (base, server) = recording_feishu(4);
        let dir = tempfile::tempdir().unwrap();
        let record = CotRecoveryRecord {
            generation: "old-generation".into(),
            cot_id: "cot_old".into(),
            message_id: "om_old".into(),
            legacy_control_mid: None,
            cot_closed: false,
            append_failed: false,
            chat_tag: Some(chat_tag("group", "oc_reattach")),
            projector: None,
            page: None,
        };
        let path = FeishuFrontend::persist_cot_recovery(dir.path(), &record).unwrap();
        let fc = FeishuClient::with_test_base_connection_close(base);
        let frontend = test_frontend(&fc, dir.path());
        let chat = reattach_chat(dir.path());
        let segments = [summon_core::segment::TurnSegment::Thinking("接着跑".into())];
        let view = summon_core::cot::CotView::new(&segments, None);
        let anchor = summon_core::TurnAnchorCot {
            cot_id: "cot_old".into(),
            message_id: "om_old".into(),
        };

        let id = frontend
            .stream_begin_reattached(&chat, &view, &anchor)
            .unwrap();
        let calls = server.join().unwrap();

        let (_, complete_url, _) = calls
            .iter()
            .find(|(_, url, _)| url.contains("complete"))
            .expect("旧 CoT 必须收口");
        assert!(complete_url.contains("cot_old") && complete_url.contains("reason=done"));
        assert_eq!(
            calls
                .iter()
                .filter(|(method, url, _)| method == "POST" && !url.contains("complete"))
                .count(),
            1,
            "退回新 CoT:{calls:?}"
        );
        assert_eq!(frontend.stream_cot_handle(id).unwrap().0, "cot_new");
        assert!(!path.exists(), "旧记录已收口,不留给下一次扫描");
    }
}
