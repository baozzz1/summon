//! 录制 cassette(A1 拆分):入站 tee/timeline/turn cassette/出站录制装饰,debug-only 为主。

use super::*;
use crate::runtime::peers_store::PeersStore;

/// 入站 cassette 记录(RR2.2):`handle_conn` 解码后 tee 一条进 `inbound.ndjson`。带 `app_id`(回放
/// 注入器据它路由到 agent)+ 入站本体(消息 / 卡片回调)。serde 标签区分两类,回放按序读回注入。
///
/// **整 type `#[cfg(debug_assertions)]` gate** —— release 不编译,生产二进制零录制代码。
#[cfg(debug_assertions)]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub(crate) enum InboundRecord {
    Message { app_id: String, inbound: Inbound },
    Card { app_id: String, card: CardActionMsg },
}

/// 入站 tee:录制开关开(`record_scenario()` 非空)时把一条入站记录 append 到当前场景的
/// `inbound.ndjson`。**off 时此函数直接返回、`handle_conn` 逐字节不变**(调用点在路由成功后,
/// 旁路录制失败只 log 忽略,绝不影响派发)。release 整体不编译。
#[cfg(debug_assertions)]
pub(super) fn record_inbound(rec: &InboundRecord) {
    let Some(scenario) = record_scenario() else {
        return;
    };
    let store = crate::runtime::record_store::RecordStore::new(summon_home().join("raw"));
    if let Err(e) = store.append_ndjson(&scenario, "inbound.ndjson", rec) {
        log(&summon_i18n::format!("r3769c6f93d"));
    }
    // 同步记一条 timeline atom(chat_id + 入站下标),供 promote correlate 串 e2e 流。
    let (chat_id, idx) = match rec {
        InboundRecord::Message { inbound, .. } => (
            inbound.chat_id.as_str(),
            RECORD_INBOUND_IDX.fetch_add(1, std::sync::atomic::Ordering::SeqCst),
        ),
        InboundRecord::Card { card, .. } => (
            card.chat_id.as_str(),
            RECORD_INBOUND_IDX.fetch_add(1, std::sync::atomic::Ordering::SeqCst),
        ),
    };
    record_timeline_atom(
        chat_id,
        summon_core::record::FlowStep::Inbound { index: idx },
    );
}

/// `summon dev record on` 写的运行期标志文件:内容即场景名。`off` 删之。env `SUMMON_RECORD` 优先
/// (联调/测试直接 export),其次读此文件——故 daemon 已在跑时也能经 CLI 开关录制(无需重启 daemon)。
#[cfg(debug_assertions)]
pub(crate) fn record_flag_file() -> PathBuf {
    run_dir().join("record")
}

/// 录制开关 + 场景名:debug 构建里先读 `SUMMON_RECORD` 环境变量,再回退 `~/.summon/run/record` 标志
/// 文件(`dev record on` 写)。空/未设/文件不存在 = 不录制(返回 `None`,默认零行为变化)。非空值即
/// 场景名(`SUMMON_RECORD=1` 退化成场景名 `default`)。release 不编译。
#[cfg(debug_assertions)]
pub(crate) fn record_scenario() -> Option<String> {
    let env_val = std::env::var("SUMMON_RECORD").ok();
    // 回退运行期标志文件(dev record on 写;daemon 在跑时也能开)。
    let flag_file = std::fs::read_to_string(record_flag_file()).ok();
    scenario_from(env_val.as_deref(), flag_file.as_deref())
}

/// 纯归一:据 `SUMMON_RECORD` 值与标志文件内容定录制场景名。env 产出 Some 即用,否则回退文件——
/// 两端语义略有差异:env 把 `1` 退化成 `default`,文件只取非空原文。无副作用,单测直接覆盖(测试
/// 绝不 `set_var`:并行套件里改 environ 会与别的测试 fork/execve 抢,毒化无关 spawn 测试,见 T1)。
#[cfg(debug_assertions)]
pub(super) fn scenario_from(env_val: Option<&str>, flag_file: Option<&str>) -> Option<String> {
    let from_env = match env_val {
        Some(v) if v.trim().is_empty() => None,
        Some(v) if v.trim() == "1" => Some("default".to_string()),
        Some(v) => Some(v.trim().to_string()),
        None => None,
    };
    if from_env.is_some() {
        return from_env;
    }
    match flag_file {
        Some(s) if !s.trim().is_empty() => Some(s.trim().to_string()),
        _ => None,
    }
}

/// 录制场景的全局到达序计数器(debug-only):入站/出站/轮 record 各取一个递增 seq,供 correlation
/// 跨侧排序串 e2e 流(`scenario.json`)。与轮计数解耦(轮计数定目录名,seq 定时序)。
#[cfg(debug_assertions)]
pub(super) static RECORD_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// 取下一个全局到达序。
#[cfg(debug_assertions)]
pub(super) fn record_next_seq() -> u64 {
    RECORD_SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
}

/// 各侧原子记录的下标计数器(debug-only):inbound/outbound 各一个,turn 用 `RECORD_TURN` 的轮号。
/// 与 ndjson 的 append 顺序一一对应(第 n 次 append = index n),回放据 FlowStep 的 index 读回该条。
#[cfg(debug_assertions)]
pub(super) static RECORD_INBOUND_IDX: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);
#[cfg(debug_assertions)]
pub(super) static RECORD_OUTBOUND_IDX: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// 录一条 timeline atom 到 `<scenario>/timeline.ndjson`(debug-only):带 chat_id + 全局 seq +
/// FlowStep。promote 时读回喂 `correlate` 生成 `scenario.json`(按 chat 分组 + 时序串 e2e 流)。
/// off → no-op;失败只 log 忽略(timeline 是索引旁路,绝不中断会话)。
#[cfg(debug_assertions)]
pub(super) fn record_timeline_atom(chat_id: &str, step: summon_core::record::FlowStep) {
    let Some(scenario) = record_scenario() else {
        return;
    };
    let atom = summon_core::record::AtomRef {
        chat_id: chat_id.to_string(),
        seq: record_next_seq(),
        step,
    };
    let store = crate::runtime::record_store::RecordStore::new(summon_home().join("raw"));
    if let Err(e) = store.append_ndjson(&scenario, "timeline.ndjson", &atom) {
        log(&summon_i18n::format!("r588914eeeb"));
    }
}

/// 录一条出站 timeline atom(debug-only):`RecordingFrontend` 的 ndjson sink 每写一条 outbound 后调,
/// 据其在 outbound.ndjson 的下标(`RECORD_OUTBOUND_IDX` 自增)+ chat_id 串进时序。
#[cfg(debug_assertions)]
pub(crate) fn record_outbound_timeline(chat_id: &str) {
    let idx = RECORD_OUTBOUND_IDX.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    record_timeline_atom(
        chat_id,
        summon_core::record::FlowStep::Outbound { index: idx },
    );
}

/// 录制会话快照(debug-only):一轮 turn-end 拿到 sid 后,按 CLI 把该 sid 的会话文件快照进
/// `<scenario>/session/<cli>/`(claude/codex;cursor 暂不)。off / 无 sid → no-op。失败只 log 忽略。
#[cfg(debug_assertions)]
pub(super) fn record_session_snapshot(agent: &Agent, session_id: Option<&str>) {
    use crate::runtime::record_store::SessionCli;
    let Some(scenario) = record_scenario() else {
        return;
    };
    let Some(sid) = session_id.filter(|s| !s.is_empty()) else {
        return;
    };
    // cli_kind() 返回 &str;按 CliDriver 归一(与 capture_mode 一致),非四类则不快照。
    let cli = match CliDriver::from_kind(agent.cli_kind()) {
        CliDriver::Claude => SessionCli::Claude,
        CliDriver::Codex => SessionCli::Codex,
        CliDriver::Traex => SessionCli::Traex,
        CliDriver::Cursor => SessionCli::Cursor,
        _ => return, // Other:无会话文件可快照
    };
    let store = crate::runtime::record_store::RecordStore::new(summon_home().join("raw"));
    let home = dirs::home_dir().unwrap_or_default();
    if let Err(e) =
        store.snapshot_session(&scenario, cli, sid, agent.working_dir().as_deref(), &home)
    {
        log(&summon_i18n::format!("rfedc03c0f5"));
    }
}

/// 出站录制装饰器选择(debug-only):录制开 → 把 `FeishuFrontend` 包成 `RecordingFrontend`(Ndjson
/// sink,落 `outbound.ndjson`);关 → 裸 `FeishuFrontend`。返回 `Box<dyn Frontend>`,turn 流程对它
/// 无感(同 `Frontend` 签名)。off 时与裸 `FeishuFrontend` 逐字节等价。release 恒裸。
/// 统一入口:不需要「最近定格卡」记账的调用点走这里(行为与本能力引入前逐字一致)。
pub(super) fn record_frontend<'a>(agent: &Agent, fc: &'a FeishuClient) -> Box<dyn Frontend + 'a> {
    record_frontend_with_settled(agent, fc, None, None, None, None, None)
}

/// 同上,但把跨轮共享的「最近定格卡」槽挂进去。chat 线程走这一版(它要在轮末之后继续刷子代理
/// 状态);outbox drain 等无此需求的调用点仍走无参版。
/// `preview`:文件预览改写器(chat 线程从 `Core` 取;outbox drain 传 `None`——它重发的是
/// 已渲染成品,原始发送时改过的链接已经是 http URL)。
#[cfg(debug_assertions)]
pub(super) fn record_frontend_with_settled<'a>(
    agent: &Agent,
    fc: &'a FeishuClient,
    settled: Option<crate::frontend::SharedSettledCard>,
    preview: Option<std::sync::Arc<crate::runtime::preview_server::PreviewServer>>,
    reply_target: Option<(&str, bool)>,
    cot_visible_override: Option<bool>,
    final_reply: Option<crate::frontend::SharedFinalReply>,
) -> Box<dyn Frontend + 'a> {
    let alias_map = super::agent_env::build_alias_name_map();
    let scope = super::agent_env::feishu_storage_scope(agent);
    let peers_store = Some(PeersStore::at_run_dir(
        &scope.key,
        scope.allow_legacy_import,
    ));
    let identity_cache = crate::runtime::identity_cache::IdentityCacheStore::at_run_dir(
        &scope.key,
        scope.allow_legacy_import,
    );
    let messaging = super::agent_env::resolve_messaging(agent);
    let show_cot = cot_visible_override.unwrap_or(messaging.cot_visible);
    let agent_name = agent.name.clone();
    let configure = |frontend: FeishuFrontend<'a>| {
        let name = agent_name.clone();
        let frontend = frontend
            .with_preview_server(preview.clone())
            .with_preview_base(agent.working_dir())
            .with_cot_content_visible(show_cot)
            .with_final_reply(final_reply.clone())
            .with_cot_keep(Box::new(move |tag: &str, cot_id: &str| {
                super::dispatcher::cot_claimed_by_reattach(&name, tag, cot_id)
            }));
        match reply_target {
            Some((message_id, in_thread)) => frontend.with_reply_target(message_id, in_thread),
            None => frontend,
        }
    };
    match record_scenario() {
        Some(scenario) => Box::new(crate::frontend::RecordingFrontend::new(
            with_slot(
                configure(FeishuFrontend::new(
                    fc,
                    alias_map,
                    peers_store.clone(),
                    identity_cache.clone(),
                )),
                settled,
            ),
            crate::frontend::RecordSink::ndjson(scenario),
        )),
        None => Box::new(with_slot(
            configure(FeishuFrontend::new(
                fc,
                alias_map,
                peers_store,
                identity_cache,
            )),
            settled,
        )),
    }
}

#[cfg(not(debug_assertions))]
pub(super) fn record_frontend_with_settled<'a>(
    agent: &Agent,
    fc: &'a FeishuClient,
    settled: Option<crate::frontend::SharedSettledCard>,
    preview: Option<std::sync::Arc<crate::runtime::preview_server::PreviewServer>>,
    reply_target: Option<(&str, bool)>,
    cot_visible_override: Option<bool>,
    final_reply: Option<crate::frontend::SharedFinalReply>,
) -> Box<dyn Frontend + 'a> {
    let alias_map = super::agent_env::build_alias_name_map();
    let scope = super::agent_env::feishu_storage_scope(agent);
    let peers_store = Some(PeersStore::at_run_dir(
        &scope.key,
        scope.allow_legacy_import,
    ));
    let identity_cache = crate::runtime::identity_cache::IdentityCacheStore::at_run_dir(
        &scope.key,
        scope.allow_legacy_import,
    );
    let messaging = super::agent_env::resolve_messaging(agent);
    let show_cot = cot_visible_override.unwrap_or(messaging.cot_visible);
    let agent_name = agent.name.clone();
    let frontend = FeishuFrontend::new(fc, alias_map, peers_store, identity_cache)
        .with_preview_server(preview)
        .with_preview_base(agent.working_dir())
        .with_cot_content_visible(show_cot)
        .with_final_reply(final_reply)
        .with_cot_keep(Box::new(move |tag: &str, cot_id: &str| {
            super::dispatcher::cot_claimed_by_reattach(&agent_name, tag, cot_id)
        }));
    let frontend = match reply_target {
        Some((message_id, in_thread)) => frontend.with_reply_target(message_id, in_thread),
        None => frontend,
    };
    Box::new(with_slot(frontend, settled))
}

/// 有槽就挂,没有就原样返回。抽出来只为让两个 cfg 分支共用同一句,不各写一份。
fn with_slot<'a>(
    frontend: FeishuFrontend<'a>,
    settled: Option<crate::frontend::SharedSettledCard>,
) -> FeishuFrontend<'a> {
    match settled {
        Some(slot) => frontend.with_settled_card(slot),
        None => frontend,
    }
}
