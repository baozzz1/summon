//! sentinel 动作链——对外副作用的唯一收口。
//!
//! 「进来回答」:owner 用户身份拉 bot 进群 → user token 拉群历史 → durable 注入跑一轮。
//! 「建群」:先发确认卡(动真人必须前置确认,红线③),owner 点确认后才由 bot 建双人群
//! (owner 为群主)、owner 身份拉对方真人、注入原单聊上下文。
//!
//! 每步 fail-closed:任何事实拿不到/校验不过 → 不动作,失败尽量私聊告知 owner。

use std::sync::Arc;

use anyhow::{bail, Context, Result};

use super::super::agent_env::{
    agent_owner, feishu_delivery_scope, owner_id_matches, resolve_creds,
};
use super::super::outbox_drain::DrainAgents;
use super::super::{log, CardActionMsg, DispatchMsg};
use super::lark::{LarkUser, MsgBrief};
use super::proposals::{Proposal, ProposalState, ProposalStore, PROPOSAL_TTL_MS};
use crate::frontend::client::FeishuClient;
use crate::frontend::outbound;
use crate::runtime::core::Core;
use crate::runtime::inbox_wal::{InboxWal, ScheduledAppend, SentinelInbox};
use summon_core::registry::Agent;
use summon_core::sentinel::trigger::{Trigger, TriggerAction};

/// 通过全部闸门、待执行的触发。
pub(crate) struct PendingTrigger {
    pub(crate) trigger: Trigger,
    pub(crate) chat_id: String,
    /// `p2p` / `group` / `topic`。**是查来的真实值**:搜索通道不带会话模式时 poller 会按 chat_id
    /// 补查(`lark.chat()`),补查也失败就丢弃这条候选,不推定——建群/拉人猜错撤不回来。
    pub(crate) chat_mode: String,
    pub(crate) chat_name: String,
    pub(crate) message_id: String,
    pub(crate) create_time_ms: i64,
    pub(crate) sender_name: String,
}

/// chat_mode → summon 的 chat_type 词表。
fn chat_type_of(mode: &str) -> &'static str {
    match mode {
        "p2p" => "p2p",
        "topic" => "topic_group",
        _ => "group",
    }
}

fn now_ms() -> i64 {
    chrono::Local::now().timestamp_millis()
}

/// 召唤词之外没附问题 → `fallback`;否则用 **原话**(含动作词)。
///
/// **判空看 `body`、取值取 `verbatim`**,两个来源刻意不同:`body` 是剥掉动作词的部分,只有它
/// 能回答「owner 除了喊人还说了别的没有」;但拿它当 prompt 会改语义 ——「@X 来帮我看下这个 CR」
/// 的 body 是「我看下这个 CR」,主语从 bot 变成了 owner 自己。
fn ask_text<'a>(verbatim: &'a str, body: &str, fallback: &'a str) -> &'a str {
    if body.trim().is_empty() {
        fallback
    } else {
        verbatim
    }
}

fn display_name(agent: &Agent) -> String {
    if agent.display.trim().is_empty() {
        agent.name.clone()
    } else {
        agent.display.clone()
    }
}

/// 按触发 alias 找已上线 agent(name 或 display 命中)。锁中毒/找不到 → None(fail-closed)。
/// 已告警过的 scope 组合(进程内)。**同一组 scope 只发一次**:sentinel 每 60s 失败一次,
/// 每次都发卡就是刷屏 —— 那比不发更糟。授权成功后 tick 自然恢复,不必清;daemon 重启会清空,
/// 那时若问题仍在,重发一次也合理。
fn notified_auth_gaps() -> &'static std::sync::Mutex<std::collections::HashSet<String>> {
    static S: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<String>>> =
        std::sync::OnceLock::new();
    S.get_or_init(|| std::sync::Mutex::new(std::collections::HashSet::new()))
}

/// 认得出的授权缺口 → (去重键, 说明卡);其余失败 None。错误可能被 `.context()` 包过,
/// 按内层类型认。
fn auth_gap_notice(err: &anyhow::Error) -> Option<(String, String)> {
    if let Some(auth) = err.downcast_ref::<super::lark::LarkAuthError>() {
        return Some((
            auth.missing_scopes.join(" "),
            summon_core::feishu::card_sentinel_auth_gap(&auth.missing_scopes, &auth.to_string()),
        ));
    }
    let login = err.downcast_ref::<super::lark::LarkUserLoginMissing>()?;
    Some((
        format!("login-missing:{}", login.profile.as_deref().unwrap_or("")),
        summon_core::feishu::card_sentinel_login_missing(login.profile.as_deref()),
    ))
}

/// 若本轮 tick 失败是**授权缺口**,私聊 owner 一张说明卡(同一个缺口只发一次)。
///
/// 2026-08-07 现场:owner 的 lark-cli 用户身份掉线,sentinel 整体停摆两小时,飞书侧毫无表示 ——
/// owner 只觉得「@ 了没反应」,像功能坏了。而 lark-cli 早就把缺哪个 scope 写在响应里,这条信息
/// 一路被丢进日志没人看。**它自己知道自己坏了,就该说出来。**
///
/// 全程 best-effort:认不出是授权错误就什么都不做(这条路径会主动往外发消息,按红线默认
/// fail-closed,宁可静默也不误发);发不出去只 log,绝不影响下一轮轮询。
pub(super) fn notify_auth_gap_once(drain: &DrainAgents, err: &anyhow::Error) {
    let Some((key, card)) = auth_gap_notice(err) else {
        return; // 网络/参数错误只记日志 —— 给 owner 发一张无从下手的授权卡没有意义
    };
    {
        let Ok(mut seen) = notified_auth_gaps().lock() else {
            return;
        };
        if !seen.insert(key.clone()) {
            return;
        }
    }
    // 任取一个在线 agent 发卡。**owner 的 open_id 必须在该 agent 自己的 app 命名空间里解**
    // (open_id 跨 app 不通用,2026-08-07 真机踩过:union 当 open 发卡 → 99992361 cross app)。
    let picked = drain.read().ok().and_then(|m| {
        m.values()
            .next()
            .map(|v| (v.agent.clone(), v.client.clone()))
    });
    let Some((agent, client)) = picked else {
        log(summon_i18n::text!("r27380a4bc3"));
        return;
    };
    let owner_open = match owner_ids_for(&agent, &client) {
        Ok((open, _)) => open,
        Err(e) => {
            log(&summon_i18n::format!("r961df39ce6"));
            return;
        }
    };
    match outbound::send_card_to_open_id_archived_id(
        &client,
        &agent.state_dir(),
        &owner_open,
        "sentinel_auth_gap",
        &card,
    ) {
        Ok(_) => log(&summon_i18n::format!("rc7278a33e4")),
        Err(e) => log(&summon_i18n::format!("r2b1e4ac15f")),
    }
}

fn resolve_agent(drain: &DrainAgents, alias: &str) -> Option<(Agent, Arc<FeishuClient>)> {
    let map = drain.read().ok()?;
    map.values()
        .find(|v| v.agent.name == alias || v.agent.display == alias)
        .map(|v| (v.agent.clone(), v.client.clone()))
}

/// owner 在**本 agent app 命名空间**的 (open_id, union_id)。agent.yaml 的 allowed_users 首位
/// 是 owner,但通常存的是跨 app 稳定的 union_id(on_,owner 特意如此配)——而发卡/建群的
/// receive/owner 字段只吃本 app open_id,union 得经该 agent 的 contact API 反解一次
/// (2026-08-07 真机:union 当 open 发确认卡 → 99992361 cross app)。
fn owner_ids_for(agent: &Agent, client: &FeishuClient) -> Result<(String, String)> {
    let entry = agent_owner(agent).context(summon_i18n::text!("re350e01829"))?;
    if entry.starts_with("on_") {
        let open = client
            .resolve_open_id_by_union(&entry)
            .with_context(|| summon_i18n::format!("rdaf7e72379", agent.name))?;
        return Ok((open, entry));
    }
    Ok((entry, String::new()))
}

/// 执行一条已过闸触发;返回一句人读的结果描述(poller 记日志)。
pub(crate) fn execute(
    core: &Arc<Core>,
    drain: &DrainAgents,
    lark: &LarkUser,
    t: &PendingTrigger,
    dry_run: bool,
) -> Result<String> {
    let (agent, client) =
        resolve_agent(drain, &t.trigger.alias).context(summon_i18n::text!("rf5a1690bc7"))?;
    // 动作按会话类型归一:私聊里的「进来/帮」只能靠建群实现(p2p 加不了成员)。
    // 词表分的是 owner 的**说法**,这里落的才是**动作**,两者不是一回事。
    match summon_core::sentinel::trigger::effective_action(t.trigger.action, &t.chat_mode) {
        TriggerAction::JoinAnswer => join_answer(core, lark, &agent, &client, t, dry_run),
        TriggerAction::CreateGroup => {
            // `propose_create_group` 整套按 p2p 写:靠「成员表里不是 owner 的那个人」定对象。
            // 群里那条判据会挑到任意同事,所以在**进入它之前**就拒绝,别让它猜。
            if !summon_core::sentinel::trigger::create_group_has_unambiguous_counterpart(
                &t.chat_mode,
            ) {
                let notice = create_group_needs_p2p_notice(
                    &t.trigger.alias,
                    &display_name(&agent),
                    &t.chat_name,
                );
                if dry_run {
                    return Ok(format!("[dry-run] {notice}"));
                }
                let (owner_open, _) = owner_ids_for(&agent, &client)?;
                return Ok(notify_owner_soft(&agent, &client, &owner_open, &notice));
            }
            propose_create_group(lark, &agent, &client, t, dry_run)
        }
        TriggerAction::InviteOnly => invite_only(lark, &agent, &client, t, dry_run),
    }
}

/// 群里发建群词时给 owner 的说明。**必须同时说清「什么都没做」和「该怎么做」**——只说拒绝会让
/// owner 以为要去后台确认有没有半建的群;而两条替代路径都得是真的做得到的:
/// `/summon` 在群里恒落 `JoinAnswer`(拉进本群),建群则只有「owner 与真人同事的单聊」这一条路
/// ——**owner 与 bot 的单聊里不存在「对方真人」,在那儿说建群同样失败**,所以文案不能写成「私聊我」。
fn create_group_needs_p2p_notice(alias: &str, agent_display: &str, chat_name: &str) -> String {
    summon_i18n::format!("rbe4f586073")
}

/// 「进来回答」:拉 bot 进本群并注入上下文。p2p 里发这个动作词会在拉人 API 处天然失败
/// (chat.members 仅支持 group/topic),错误会私聊告知,无需上游预判。
/// `/invite`:**只把 bot 拉进群,到此为止**。
///
/// 与 [`join_answer`] 共用「已在群则跳过」「拉人失败告知 owner」这两段,但**不取历史、不发开场卡、
/// 不建任何会话** —— owner 的用例是拉进来之后改用 `/bridge` 桥接一个已有 CLI 会话,此时 summon
/// 自己起的会话不只是多余,还会先占住该 chat 的会话位、让 `/bridge` 无处可接。
///
/// 少做事就是这条路径的全部价值,所以它宁可短:任何"顺手帮个忙"的增补都会把它变回 join_answer。
fn invite_only(
    lark: &LarkUser,
    agent: &Agent,
    client: &Arc<FeishuClient>,
    t: &PendingTrigger,
    dry_run: bool,
) -> Result<String> {
    let app_id = resolve_creds(agent)
        .0
        .context(summon_i18n::text!("r56ede109be"))?;
    let bots = lark.bots_in_chat(&t.chat_id)?;
    if bots.contains(&app_id) {
        return Ok(summon_i18n::format!("r70133e70b6", t.chat_name));
    }
    if dry_run {
        return Ok(summon_i18n::format!(
            "rb4595a86a6",
            display_name(agent),
            t.chat_name
        ));
    }
    let (owner_open, _) = owner_ids_for(agent, client)?;
    lark.add_bot(&t.chat_id, &app_id).with_context(|| {
        notify_owner_soft(
            agent,
            client,
            &owner_open,
            &summon_i18n::format!("rb0cb4a3534", display_name(agent), t.chat_name),
        )
    })?;
    Ok(summon_i18n::format!(
        "re7939f468a",
        display_name(agent),
        t.chat_name
    ))
}

fn join_answer(
    core: &Arc<Core>,
    lark: &LarkUser,
    agent: &Agent,
    client: &Arc<FeishuClient>,
    t: &PendingTrigger,
    dry_run: bool,
) -> Result<String> {
    let app_id = resolve_creds(agent)
        .0
        .context(summon_i18n::text!("r56ede109be"))?;
    let bots = lark.bots_in_chat(&t.chat_id)?;
    if bots.contains(&app_id) {
        // bot 已在群:真 @ 走既有入站链路,sentinel 退出,杜绝双响应。
        return Ok(summon_i18n::format!("rc8535a33c2", t.chat_name));
    }
    if dry_run {
        return Ok(summon_i18n::format!(
            "r1f1ba441e9",
            display_name(agent),
            t.chat_name,
            t.trigger.verbatim
        ));
    }
    let (owner_open, owner_union) = owner_ids_for(agent, client)?;
    lark.add_bot(&t.chat_id, &app_id).with_context(|| {
        notify_owner_soft(
            agent,
            client,
            &owner_open,
            &summon_i18n::format!("rb0cb4a3534", display_name(agent), t.chat_name),
        )
    })?;
    let history = recent_history(lark, &t.chat_id);
    // 开场卡先行:bot 生成回答要几十秒,群里的人不该对着"邀请了 XX"的系统提示干等。
    // 失败可忽略——开场卡是体验增强,不该阻断召唤本身。
    let ask = ask_text(
        &t.trigger.verbatim,
        &t.trigger.body,
        summon_i18n::text!("rbef1056d82"),
    );
    let intro = summon_core::feishu::card_sentinel_intro(
        &t.sender_name,
        &display_name(agent),
        ask,
        history.len(),
    );
    let _ = outbound::send_card_archived(
        client,
        &agent.state_dir(),
        chat_type_of(&t.chat_mode),
        &t.chat_id,
        None,
        "sentinel_intro",
        &intro,
    );
    let images = fetch_context_images(
        lark,
        agent,
        &summon_core::registry::chat_tag(chat_type_of(&t.chat_mode), &t.chat_id),
        &history,
    );
    let prompt = compose_join_prompt(t, &history, &images);
    fire(
        core,
        agent,
        &t.chat_id,
        chat_type_of(&t.chat_mode),
        &prompt,
        &t.message_id,
        &t.sender_name,
        &owner_open,
        &owner_union,
    )?;
    Ok(summon_i18n::format!(
        "r1a307f96e6",
        display_name(agent),
        t.chat_name,
        history.len()
    ))
}

/// 「建群」:只发确认卡,不直接执行。真正建群在 owner 点确认后(handle_card)。
fn propose_create_group(
    lark: &LarkUser,
    agent: &Agent,
    client: &Arc<FeishuClient>,
    t: &PendingTrigger,
    dry_run: bool,
) -> Result<String> {
    let owner_lark = lark.owner_open_id()?;
    let users = lark.users_in_chat(&t.chat_id)?;
    let (cp_id, cp_name) = users
        .into_iter()
        .find(|(id, _)| id != &owner_lark)
        .context(summon_i18n::text!("rb77fe9d3c3"))?;
    if dry_run {
        return Ok(summon_i18n::format!("r63adf44f7f", display_name(agent)));
    }
    let (owner_open, owner_union) = owner_ids_for(agent, client)?;
    let proposal_id = format!("sp-{}", t.message_id);
    let store = ProposalStore::at_run_dir();
    // 同一触发消息只发一张卡(搜索通道迟到重复命中时靠这里兜底幂等)。
    if store.load()?.iter().any(|p| p.id == proposal_id) {
        return Ok(summon_i18n::text!("r1c16f084de").to_string());
    }
    let summary = summon_i18n::format!(
        "rf628aa0520",
        display_name(agent),
        ask_text(
            &t.trigger.verbatim,
            &t.trigger.body,
            summon_i18n::text!("r628e6d8783")
        )
    );
    let card = summon_core::feishu::card_sentinel_confirm(&proposal_id, &summary);
    let card_message_id = outbound::send_card_to_open_id_archived_id(
        client,
        &agent.state_dir(),
        &owner_open,
        "sentinel_proposal",
        &card,
    )?;
    store.insert(Proposal {
        id: proposal_id,
        agent_name: agent.name.clone(),
        p2p_chat_id: t.chat_id.clone(),
        counterpart_open_id: cp_id,
        counterpart_name: cp_name.clone(),
        body: t.trigger.body.clone(),
        verbatim: t.trigger.verbatim.clone(),
        trigger_message_id: t.message_id.clone(),
        who: t.sender_name.clone(),
        who_id: owner_open,
        who_union_id: owner_union,
        created_ms: now_ms(),
        state: ProposalState::Pending,
        card_message_id: Some(card_message_id),
    })?;
    Ok(summon_i18n::format!("r50046790f1"))
}

/// 确认卡回调(dispatcher 的 SentinelFamily 臂调用;集中 operate-gate 已过)。
/// fencing:proposal 存在且 Pending + 卡片 message_id 精确匹配 + operator 是该 agent 的
/// owner + 未过期,任一不满足只置灰不执行。
pub(crate) fn handle_card(
    agent: &Agent,
    fc: &Arc<FeishuClient>,
    inbox_wal: &InboxWal,
    self_tx: &std::sync::mpsc::Sender<DispatchMsg>,
    card: &CardActionMsg,
) {
    let action = card.value["action"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let Some(proposal_id) = card.value["proposal"].as_str().map(|s| s.to_string()) else {
        log(&summon_i18n::format!("ra34eee0199", agent.name));
        return;
    };
    if let Err(e) = handle_card_inner(agent, fc, inbox_wal, self_tx, card, &action, &proposal_id) {
        log(&summon_i18n::format!("r87c6459770", agent.name));
    }
}

fn handle_card_inner(
    agent: &Agent,
    fc: &Arc<FeishuClient>,
    inbox_wal: &InboxWal,
    self_tx: &std::sync::mpsc::Sender<DispatchMsg>,
    card: &CardActionMsg,
    action: &str,
    proposal_id: &str,
) -> Result<()> {
    let store = ProposalStore::at_run_dir();
    let proposals = store.load()?;
    let Some(p) = proposals
        .iter()
        .find(|p| p.id == proposal_id && p.agent_name == agent.name)
    else {
        bail!(summon_i18n::format!("rc9351763ca"));
    };
    let summary = summon_i18n::format!("rfcfda64e13", p.counterpart_name, display_name(agent));
    let finish_card = |outcome: &str| {
        let done = summon_core::feishu::card_sentinel_confirm_done(&summary, outcome);
        let _ = outbound::update_card_archived(
            fc,
            &agent.state_dir(),
            "p2p",
            &card.chat_id,
            &card.message_id,
            &done,
        );
    };
    // fencing 逐条(任一不过 → 只置灰)。
    if p.state != ProposalState::Pending {
        finish_card(summon_i18n::text!("redb1abc768"));
        return Ok(());
    }
    if p.card_message_id.as_deref() != Some(card.message_id.as_str()) {
        bail!(summon_i18n::format!("r6bedb313b7"));
    }
    // owner 判定双 id:agent.yaml 的 owner 条目通常是 union_id,而事件只带 open_id,
    // 直接字符串比较必然拒真 owner——先反解 operator 的 union 再双判。
    let owner_entry = agent_owner(agent).unwrap_or_default();
    let op_union = fc.resolve_union_id(&card.operator_open_id);
    if !owner_id_matches(&owner_entry, &card.operator_open_id, &op_union) {
        finish_card(summon_i18n::text!("r9562b0fc8d"));
        return Ok(());
    }
    if now_ms() - p.created_ms > PROPOSAL_TTL_MS {
        store.update(proposal_id, |p| p.state = ProposalState::Dismissed)?;
        finish_card(summon_i18n::text!("rc25c3a9e76"));
        return Ok(());
    }
    if action == "sentinel_dismiss" {
        store.update(proposal_id, |p| p.state = ProposalState::Dismissed)?;
        finish_card(summon_i18n::text!("rd2f4d5deb0"));
        return Ok(());
    }
    // sentinel_confirm:执行建群。
    let root_cfg = summon_core::config::Config::load();
    let lark_cli = summon_core::tools::resolve_from(&root_cfg, summon_core::tools::Tool::LarkCli);
    let cfg = root_cfg.sentinel;
    let lark = LarkUser::new(
        cfg.lark_profile.clone(),
        Some(lark_cli.to_string_lossy().to_string()),
    );
    let app_id = resolve_creds(agent)
        .0
        .context(summon_i18n::text!("r021d264030"))?;
    let group_name = format!("{}×{}·{}", p.who, p.counterpart_name, display_name(agent));
    let new_chat = fc.create_chat(
        &group_name,
        &p.who_id,
        std::slice::from_ref(&p.who_id),
        std::slice::from_ref(&app_id),
    )?;
    // 建群成功即刻置 Done:哪怕后续拉人/注入失败,重复点击也绝不能再建第二个群。
    store.update(proposal_id, |p| {
        p.state = ProposalState::Done {
            chat_id: new_chat.clone(),
        }
    })?;
    let mut notes: Vec<String> = Vec::new();
    if let Err(e) = lark.add_user(&new_chat, &p.counterpart_open_id) {
        notes.push(summon_i18n::format!("rec54438dd2", p.counterpart_name));
    }
    let history = recent_history(&lark, &p.p2p_chat_id);
    // 开场卡先行(体验增强,失败可忽略):对方刚被拉进群,先交代谁召唤了谁、要答什么。
    let ask = ask_text(p.ask_verbatim(), &p.body, summon_i18n::text!("rca618bc15d"));
    let intro =
        summon_core::feishu::card_sentinel_intro(&p.who, &display_name(agent), ask, history.len());
    let _ = outbound::send_card_archived(
        fc,
        &agent.state_dir(),
        "group",
        &new_chat,
        None,
        "sentinel_intro",
        &intro,
    );
    // 图片按**原单聊**的 chat 归档:这些图是从那边取的,和新建的群没有关系。
    let images = fetch_context_images(
        &lark,
        agent,
        &summon_core::registry::chat_tag("p2p", &p.p2p_chat_id),
        &history,
    );
    let prompt = compose_group_prompt(p, &history, &images);
    let payload = SentinelInbox {
        chat_id: new_chat.clone(),
        chat_type: "group".to_string(),
        prompt,
        // 触发消息在原单聊里,bot 不在那个会话,合成一个衍生 id 当 fire_id。
        fire_id: format!("sentinel-{}", p.trigger_message_id),
        who: p.who.clone(),
        who_id: p.who_id.clone(),
        who_union_id: p.who_union_id.clone(),
        handoff_note: None,
    };
    let scope = feishu_delivery_scope(agent);
    match inbox_wal.append_sentinel(&agent.name, &scope, &payload, now_ms()) {
        Ok(ScheduledAppend::Inserted) => {
            let msg = payload.fire();
            // self_tx 即本 agent dispatcher 自己的 channel;send 失败说明 daemon 正在停机,
            // WAL 已落盘,冷启会自动重注入,不丢。
            if self_tx.send(msg).is_err() {
                notes.push(summon_i18n::text!("r0485b3d3ac").to_string());
            }
        }
        Ok(_) => {} // 幂等命中:已有持久事实。
        Err(e) => notes.push(summon_i18n::format!("r63c38a8294")),
    }
    let outcome = if notes.is_empty() {
        summon_i18n::format!("r6c2558f304")
    } else {
        summon_i18n::format!("rf3cd227ed0", notes.join(";"))
    };
    finish_card(&outcome);
    Ok(())
}

/// 最近 24h 内至多 50 条历史(升序)。拉不到就空——上下文缺失不该阻断召唤本身。
fn recent_history(lark: &LarkUser, chat_id: &str) -> Vec<MsgBrief> {
    const LIMIT: usize = 50;
    let cutoff = now_ms() - 24 * 3600 * 1000;
    match lark.history_asc(chat_id, LIMIT) {
        Ok(msgs) => msgs
            .into_iter()
            .filter(|m| m.create_time_ms >= cutoff && !m.text.trim().is_empty())
            .collect(),
        Err(e) => {
            log(&summon_i18n::format!("r96f004185a"));
            Vec::new()
        }
    }
}

/// 一轮召唤最多下几张图。上限的理由是**时间**不是磁盘:每张一次 lark-cli 子进程,
/// 而这段路跑在 owner 已经在等的召唤动作里。8 张够覆盖「发几张图问怎么装」这类真实用法。
const MAX_CONTEXT_IMAGES: usize = 8;

/// 把历史里的图片真下下来,返回 message_id → 本地绝对路径。
///
/// **为什么必须下**:lark-cli 把图片渲染成 `[Image: img_xxx]`,那串 key 对 agent 是死的 ——
/// 2026-08-08 owner 召唤 agent 看两张窗帘轨道安装图,agent 拿到的只有占位符,只能回
/// 「我看不到图」。图片是这类求助的**全部信息量**,丢了图等于没召唤。
///
/// 落点与正常入站附件同构(`<agent.dir>/inbound/<tag>/<message_id>/`),agent 用 Read 直接打开。
/// 逐张失败即跳过(记日志):少一张图比整个召唤失败好。
fn fetch_context_images(
    lark: &LarkUser,
    agent: &Agent,
    tag: &str,
    history: &[MsgBrief],
) -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
    let candidates = context_images_to_fetch(history);
    if history
        .iter()
        .filter(|m| super::lark::image_key_in(&m.text).is_some())
        .count()
        > candidates.len()
    {
        log(&summon_i18n::format!("r521b3fd9d0"));
    }
    // 从新到旧地取:图多时保留离提问最近的那几张,它们才是 owner 正在说的那几张。
    // 上限按**尝试数**而非成功数:失效图片也不能把一次召唤拖成几十个串行子进程。
    for (m, key) in candidates {
        let dir = agent
            .state_dir()
            .join("inbound")
            .join(tag)
            .join(&m.message_id);
        if let Err(e) = std::fs::create_dir_all(&dir) {
            log(&summon_i18n::format!("rbb651e2466"));
            continue;
        }
        match lark.download_image(&m.message_id, key, &dir, "image") {
            Ok(path) => {
                out.insert(m.message_id.clone(), path.to_string_lossy().into_owned());
            }
            Err(e) => log(&summon_i18n::format!("r7a8bdbc4d4", m.message_id)),
        }
    }
    out
}

fn context_images_to_fetch(history: &[MsgBrief]) -> Vec<(&MsgBrief, &str)> {
    history
        .iter()
        .rev()
        .filter_map(|m| super::lark::image_key_in(&m.text).map(|key| (m, key)))
        .take(MAX_CONTEXT_IMAGES)
        .collect()
}

/// 上下文块渲染:单条截断 + 总量封顶,防把超长群聊灌爆 prompt。
/// `images` 是已下好的图(message_id → 本地路径),命中的行把占位符换成可 Read 的真路径。
fn render_context(
    history: &[MsgBrief],
    images: &std::collections::HashMap<String, String>,
) -> String {
    const PER_MSG: usize = 400;
    const TOTAL: usize = 16_000;
    let mut out = String::new();
    for m in history {
        let who = if m.sender_name.is_empty() {
            m.sender_id.as_str()
        } else {
            m.sender_name.as_str()
        };
        // 下到了就给真路径。**不保留原占位符** —— 两个都摆着只会让 agent 以为有两样东西。
        let text: String = match images.get(&m.message_id) {
            Some(path) => {
                summon_i18n::format!("r3e1635b0f0")
            }
            None => m.text.chars().take(PER_MSG).collect(),
        };
        // 卡片消息(bot 的过程/总结卡)标注出来,agent 好分辨"这是人说的还是 bot 贴的"。
        let tag = if m.msg_type == "interactive" {
            summon_i18n::text!("r248e0b023f")
        } else {
            ""
        };
        let line = format!("{who}{tag}: {text}\n");
        if out.chars().count() + line.chars().count() > TOTAL {
            out.push_str(summon_i18n::text!("r377cc12de4"));
            break;
        }
        out.push_str(&line);
    }
    out
}

fn compose_join_prompt(
    t: &PendingTrigger,
    history: &[MsgBrief],
    images: &std::collections::HashMap<String, String>,
) -> String {
    let ctx = render_context(history, images);
    let ask = ask_text(
        &t.trigger.verbatim,
        &t.trigger.body,
        summon_i18n::text!("r345d0fc13f"),
    );
    summon_i18n::format!("rba738e4406", t.chat_name, history.len(), ctx, ask)
}

fn compose_group_prompt(
    p: &Proposal,
    history: &[MsgBrief],
    images: &std::collections::HashMap<String, String>,
) -> String {
    let ctx = render_context(history, images);
    let ask = ask_text(p.ask_verbatim(), &p.body, summon_i18n::text!("reecf3729f7"));
    summon_i18n::format!(
        "r0687f118f6",
        p.counterpart_name,
        p.counterpart_name,
        history.len(),
        ctx,
        ask
    )
}

/// durable-before-route:先幂等落 WAL,再经 Core 按 alias 路由;无路由则撤销本次插入。
#[allow(clippy::too_many_arguments)] // 触发上下文与身份三元组职责各异,无合并意义
fn fire(
    core: &Arc<Core>,
    agent: &Agent,
    chat_id: &str,
    chat_type: &str,
    prompt: &str,
    fire_id: &str,
    who: &str,
    who_id: &str,
    who_union_id: &str,
) -> Result<()> {
    let wal = InboxWal::new(crate::runtime::paths::run_dir().join("inbox"));
    let scope = feishu_delivery_scope(agent);
    let payload = SentinelInbox {
        chat_id: chat_id.to_string(),
        chat_type: chat_type.to_string(),
        prompt: prompt.to_string(),
        fire_id: fire_id.to_string(),
        who: who.to_string(),
        who_id: who_id.to_string(),
        who_union_id: who_union_id.to_string(),
        handoff_note: None,
    };
    match wal.append_sentinel(&agent.name, &scope, &payload, now_ms())? {
        ScheduledAppend::Inserted => {
            let ok = core.dispatch_scheduled(&agent.name, payload.fire());
            if !ok {
                let _ = wal.discard_unrouted_sentinel(&agent.name, fire_id);
                bail!(summon_i18n::format!("r9773464945"));
            }
            Ok(())
        }
        // 幂等命中:该触发已有持久交付事实(搜索通道重复召回等),不再路由。
        ScheduledAppend::Pending | ScheduledAppend::Consumed => Ok(()),
    }
}

/// 失败时尽量私聊告知 owner(best-effort,失败本身只记日志);返回给 with_context 用的描述。
/// `owner_open` 必须是本 agent app 命名空间的 open_id(由 [`owner_ids_for`] 解析)。
fn notify_owner_soft(
    agent: &Agent,
    client: &Arc<FeishuClient>,
    owner_open: &str,
    text: &str,
) -> String {
    let _ = outbound::send_markdown_to_open_id_archived(
        client,
        &agent.state_dir(),
        owner_open,
        "sentinel_error",
        text,
    );
    text.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_profile_without_user_login_is_an_auth_gap_worth_telling_the_owner() {
        let err = anyhow::Error::new(super::super::lark::LarkUserLoginMissing {
            profile: Some("devbox-bot".into()),
        })
        .context("取 owner open_id 失败");
        let (key, card) = auth_gap_notice(&err).expect("用户身份缺失要告诉 owner");
        assert_eq!(key, "login-missing:devbox-bot");
        assert!(card.contains("lark-cli auth login --profile devbox-bot"));

        let unpinned =
            anyhow::Error::new(super::super::lark::LarkUserLoginMissing { profile: None });
        let (_, card) = auth_gap_notice(&unpinned).unwrap();
        assert!(
            card.contains("sentinel.lark_profile"),
            "没指定 profile 时要指出这一项"
        );
    }

    #[test]
    fn ordinary_failures_are_not_auth_gaps() {
        assert!(auth_gap_notice(&anyhow::anyhow!("网络超时")).is_none());
    }

    /// 判空与取值必须取自不同来源:`body` 为空才走兜底文案,不空则转发**原话**。
    /// 若两边都用 body,「@X 来帮我看下 CR」发给 agent 的就是「我看下 CR」。
    #[test]
    fn ask取原话而判空取剥过动作词的正文() {
        assert_eq!(
            ask_text("来帮我看下这个 CR", "我看下这个 CR", "兜底"),
            "来帮我看下这个 CR"
        );
        for empty in ["", "   ", "\n"] {
            assert_eq!(
                ask_text("进来回答", empty, "兜底"),
                "兜底",
                "body={empty:?}"
            );
        }
    }

    /// 拒绝文案有两个不可省的职责:说清「什么都没做」(否则 owner 会去翻有没有半建的群),
    /// 以及给出**真的走得通**的两条路。特别地不能写成「私聊我说建群」——bot 单聊里
    /// `users_in_chat` 找不到「不是 owner 的真人」,那条路在 `propose_create_group` 里必失败。
    #[test]
    fn 群里建群词的说明不得把owner引向bot单聊() {
        let notice = create_group_needs_p2p_notice("arcadia", "阿卡迪亚", "某某项目群");
        assert!(notice.contains("某某项目群"), "{notice}");
        assert!(notice.contains("没有建群"), "{notice}");
        assert!(notice.contains("/summon arcadia"), "{notice}");
        assert!(notice.contains("你和那位同事的单聊"), "{notice}");
        assert!(
            !notice.contains("私聊我"),
            "bot 单聊建群必失败,文案不能指向它:{notice}"
        );
    }

    fn msg(name: &str, text: &str, t: i64) -> MsgBrief {
        MsgBrief {
            message_id: "om".into(),
            chat_id: "oc".into(),
            sender_id: "ou".into(),
            sender_type: "user".into(),
            sender_name: name.into(),
            create_time_ms: t,
            text: text.into(),
            msg_type: "text".into(),
            chat_name: String::new(),
        }
    }

    fn no_images() -> std::collections::HashMap<String, String> {
        std::collections::HashMap::new()
    }

    #[test]
    fn 图片下载候选按尝试数封顶而非成功数() {
        let history: Vec<MsgBrief> = (0..20)
            .map(|i| {
                let mut m = msg("张三", &format!("[Image: img_{i}]"), i);
                m.message_id = format!("om_{i}");
                m
            })
            .collect();
        let got = context_images_to_fetch(&history);
        assert_eq!(got.len(), MAX_CONTEXT_IMAGES);
        assert_eq!(got[0].0.message_id, "om_19", "应从最新图片开始");
    }

    #[test]
    fn 上下文渲染_单条与总量截断() {
        let long = "长".repeat(1000);
        let ctx = render_context(&[msg("甲", &long, 1)], &no_images());
        assert!(ctx.chars().count() < 500);

        let many: Vec<MsgBrief> = (0..200).map(|i| msg("乙", &"字".repeat(400), i)).collect();
        let ctx = render_context(&many, &no_images());
        assert!(ctx.contains("已截断"));
        assert!(ctx.chars().count() <= 16_100);
    }

    /// 下到了的图渲成本地路径,**并且原占位符不再出现** —— 两个都摆着会让 agent
    /// 以为有两样东西。这正是 2026-08-08 那次「我只看到图片占位符」的反面。
    #[test]
    fn 下载到的图渲成可读路径而非占位符() {
        let mut m = msg("张三", "[Image: img_v3_abc]", 1);
        m.message_id = "om_pic".into();
        m.msg_type = "image".into();
        let mut images = std::collections::HashMap::new();
        images.insert("om_pic".to_string(), "/tmp/a/1-image.png".to_string());

        let ctx = render_context(&[m], &images);
        assert!(ctx.contains("/tmp/a/1-image.png"), "应给出本地路径:{ctx}");
        assert!(ctx.contains("Read"), "应告诉 agent 怎么打开:{ctx}");
        assert!(
            ctx.contains("owner 用户身份"),
            "应说明图片由哪种身份下载:{ctx}"
        );
        assert!(!ctx.contains("img_v3_abc"), "原占位符不该再出现:{ctx}");
    }

    /// 没下到的图保持原样 —— 宁可让 agent 看见「这里有张图但没拿到」,也不假装没有过这条消息。
    #[test]
    fn 没下到的图保留原文() {
        let mut m = msg("张三", "[Image: img_v3_abc]", 1);
        m.message_id = "om_pic".into();
        let ctx = render_context(&[m], &no_images());
        assert!(ctx.contains("img_v3_abc"));
    }

    #[test]
    fn join_prompt_空body给出主动接话指令() {
        let t = PendingTrigger {
            trigger: Trigger {
                alias: "阿卡迪亚".into(),
                action: TriggerAction::JoinAnswer,
                body: String::new(),
                // 只喊了召唤词、没附问题:原话就是那个动作词本身。
                verbatim: "进来回答".into(),
            },
            chat_id: "oc_1".into(),
            chat_mode: "group".into(),
            chat_name: "测试群".into(),
            message_id: "om_1".into(),
            create_time_ms: 1,
            sender_name: "张三".into(),
        };
        let p = compose_join_prompt(&t, &[], &no_images());
        assert!(p.contains("主动接话"));
        assert!(p.contains("测试群"));
    }

    #[test]
    fn owner_ids_直接open与union反解() {
        let fc = Arc::new(FeishuClient::new(
            "feishu.cn",
            "app".into(),
            "sec".into(),
            true, // dry_run:union 反解走短路,不打网络
        ));
        let direct: Agent = serde_yaml::from_str(
            "name: t
feishu:
  app_id_env: A
  app_secret_env: B
  allowed_users: [ou_direct]",
        )
        .unwrap();
        assert_eq!(
            owner_ids_for(&direct, &fc).unwrap(),
            ("ou_direct".to_string(), String::new())
        );
        let via_union: Agent = serde_yaml::from_str(
            "name: t
feishu:
  app_id_env: A
  app_secret_env: B
  allowed_users: [on_u1]",
        )
        .unwrap();
        assert_eq!(
            owner_ids_for(&via_union, &fc).unwrap(),
            ("dry-open-id".to_string(), "on_u1".to_string())
        );
    }

    #[test]
    fn chat_type_映射() {
        assert_eq!(chat_type_of("p2p"), "p2p");
        assert_eq!(chat_type_of("topic"), "topic_group");
        assert_eq!(chat_type_of("group"), "group");
        assert_eq!(chat_type_of(""), "group");
    }
}
