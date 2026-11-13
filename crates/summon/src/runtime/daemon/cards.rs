//! 卡片动作 handler(A1 拆分):会话面板生命周期/model/effort/workdir、恢复卡、Task 卡、
//! 闸门卡、TUI 注入;operate-gate 鉴权在各 handler 内。

use super::*;
use summon_core::feishu::ScheduleParamsCard;

/// 单个 chat 线程的控制指令(空闲/面板卡片回调映射成它)。
/// Clean = 保留 `.session`(下次 resume 续聊);Reset = 清 `.session`(下次冷启=真重置)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Control {
    Compact,
    Clean,
    Reset,
    Archive,
    Resume,
    KeepAlive,
}

/// 卡片 value.action → Control;未知动作保守当保活(绝不误清理/重置)。
pub(super) fn control_for_action(action: &str) -> Control {
    match action {
        "clean" => Control::Clean,
        "reset" => Control::Archive,
        "compact" => Control::Compact,
        _ => Control::KeepAlive,
    }
}

/// 已结束会话的归档直接提交盘上入口，终态回复只更新退出说明行。
pub(super) fn handle_archive_ended(
    chats: &HashMap<String, ChatHandle>,
    agent: &Agent,
    fc: &FeishuClient,
    inflight: &crate::runtime::inflight::InFlight,
    card: &CardActionMsg,
) {
    let chat_type = persisted_chat_type(chats, agent, &card.chat_id);
    let tag = chat_tag(&chat_type, &card.chat_id);
    super::waiting::clear_waiting(
        agent,
        &chat_type,
        &card.chat_id,
        summon_i18n::text!("r8ef5104786"),
    );
    let outcome = (|| -> anyhow::Result<()> {
        anyhow::ensure!(
            !chats.contains_key(&card.chat_id)
                && !inflight.contains(&crate::runtime::inflight::TurnKey::new(&agent.name, &tag)),
            summon_i18n::format!("r53503680e2")
        );
        let token = card
            .value
            .get("token")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let ticket = crate::session::ended_archive::archive_checked(
            agent,
            &chat_type,
            &card.chat_id,
            &card.message_id,
            token,
        )?;
        let updated = summon_core::feishu::card_with_exit_archive(
            &ticket.card_json,
            summon_i18n::text!("r7cec94a721"),
            &ticket.token,
            true,
        )
        .ok_or_else(|| anyhow::anyhow!(summon_i18n::format!("r6e0d7dfd9f")))
        .and_then(|content| {
            update_card_archived(
                fc,
                &agent.state_dir(),
                &chat_type,
                &card.chat_id,
                &card.message_id,
                &content,
            )
        });
        if let Err(error) = updated {
            log(&summon_i18n::format!("readad38b4e", agent.name));
            if let Err(error) = send_markdown_archived(
                fc,
                &agent.state_dir(),
                &chat_type,
                &card.chat_id,
                summon_i18n::text!("r89e5990c71"),
            ) {
                log(&summon_i18n::format!("rf3df6ade0f", agent.name));
            }
        }
        Ok(())
    })();
    if let Err(error) = outcome {
        log(&summon_i18n::format!("r1380c2df80", agent.name));
        let _ = send_markdown_archived(
            fc,
            &agent.state_dir(),
            &chat_type,
            &card.chat_id,
            &summon_i18n::format!("rc1e0968cf8"),
        );
    }
}

/// 卡片来源:空闲提醒卡(`card_idle_prompt`,按钮值带 `origin:idle`+`idle_secs`)vs `/session` 面板卡
/// (`card_session_panel`,无 origin)。两种卡的 clean/reset/compact 动作值完全一样,**靠 origin 区分**。
/// 收尾时据此选对应布局的终态渲染:Idle 走 `card_idle_closed`(保空闲布局)、Panel 走
/// `card_session_closed`(面板布局),避免空闲卡被错画成面板(布局漂移 bug)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct IdleCardState {
    pub(crate) idle_secs: u64,
    pub(crate) context: Option<summon_core::context_budget::ContextBudget>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CardOrigin {
    Idle(IdleCardState),
    Panel,
    /// 上下文提醒卡的 Compact:跑完把这张卡定格成「已压缩 / 没压缩成」。
    ContextWarn(summon_core::feishu::ContextWarnCard),
}

impl CardOrigin {
    /// 从卡片回调的 `value` 解析来源。仅当 `origin=="idle"` 视作空闲卡(缺 `idle_secs` 退 0);
    /// 其余(含旧卡 / 面板卡 / 缺字段)一律 Panel —— 向后兼容、fail-safe 到现有面板行为。
    pub(super) fn from_value(value: &serde_json::Value) -> Self {
        if value.get("origin").and_then(|v| v.as_str()) == Some("idle") {
            let idle_secs = value.get("idle_secs").and_then(|v| v.as_u64()).unwrap_or(0);
            let context = summon_core::context_budget::ContextBudget {
                used_tokens: value.get("ctx_used_tokens").and_then(|v| v.as_u64()),
                window_tokens: value.get("ctx_window_tokens").and_then(|v| v.as_u64()),
                reported_pct: value
                    .get("ctx_reported_pct")
                    .and_then(|v| v.as_u64())
                    .map(|pct| pct.min(100) as u8),
            };
            CardOrigin::Idle(IdleCardState {
                idle_secs,
                context: (context.used_tokens.is_some()
                    || context.window_tokens.is_some()
                    || context.reported_pct.is_some())
                .then_some(context),
            })
        } else {
            CardOrigin::Panel
        }
    }
}

/// 卡片动作的 operate-gate 输入:本 chat 是不是群。**三个调用点全是鉴权入口**,判错方向就是
/// fail-open——`can_operate` 在 operators/allowed_users 皆空时返回 `!is_group`,群误判成私聊
/// 就等于对任何人放行破坏性按钮。
///
/// 因此这里不能像展示类兜底那样「取不到就当私聊」:内存 handle 会被「清理/重置」摘除,daemon
/// 重启后 `chats` 也暂时为空,而旧面板卡始终可点。取不到就回退盘上 `.meta`;**盘上也无记录时
/// 按群处理**(fail-closed),宁可多拦一次,不可少拦一次。
pub(super) fn card_chat_is_group(
    chats: &HashMap<String, ChatHandle>,
    agent: &Agent,
    chat_id: &str,
) -> bool {
    let is_group = |t: &str| matches!(t, "group" | "topic_group");
    chats
        .get(chat_id)
        .map(|h| is_group(&h.chat_type))
        .or_else(|| {
            chat_session_on_disk(agent, chat_id).map(|(_, chat_type, _)| is_group(&chat_type))
        })
        .unwrap_or(true)
}

pub(super) fn card_chat_type(chats: &HashMap<String, ChatHandle>, chat_id: &str) -> String {
    chats
        .get(chat_id)
        .map(|h| h.chat_type.clone())
        .unwrap_or_else(|| "p2p".to_string())
}

/// **要落盘的** chat_type。与 `card_chat_type`(纯展示,取不到就当私聊)不同:这个值会被写进
/// `schedules.yaml`,并在**将来某次触发时**决定 `is_group`——判错方向等于让群定时器绕过 Bridge
/// 期间的 fail-closed 跳过闸,还会按私聊语义 derive 出错误的 tmux 会话名。
///
/// 内存 `chats` 不可靠:daemon 重启后为空、`/session clean` 也会摘除 handle,而卡片始终点得动。
/// 所以取不到就回退盘上 `.meta`;**盘上也没有就按群**——与 `card_chat_is_group` 同一条 fail-closed
/// 方向,宁可多拦一次,不可少拦一次。
pub(super) fn persisted_chat_type(
    chats: &HashMap<String, ChatHandle>,
    agent: &Agent,
    chat_id: &str,
) -> String {
    chats
        .get(chat_id)
        .map(|h| h.chat_type.clone())
        .or_else(|| chat_session_on_disk(agent, chat_id).map(|(_, chat_type, _)| chat_type))
        .unwrap_or_else(|| "group".to_string())
}

/// 解析本 chat 的会话键(= tmux 会话名)。优先内存 handle;handle 已被「清理/重置」
/// (`settle_chat_exit` 的 `chats.remove`)摘除时回退盘上 `.meta`。
///
/// 为什么必须回退:会话键是 (agent, chat) 的盘上事实,而写 `.cwd` 是纯盘操作——不需要活着的
/// chat 线程,也不需要活着的 tmux。旧实现只认内存 handle,于是「先清理会话、再改工作目录」
/// 这条最自然的操作序列会被整个拒绝、`.cwd` 不落盘,下次冷启静默回落 agent 级目录。
/// 盘上也没记录(从没来过消息的陌生 chat)才返回 None,由调用方明确拒绝——不猜会话名。
pub(super) fn resolve_chat_session(
    chats: &HashMap<String, ChatHandle>,
    agent: &Agent,
    chat_id: &str,
) -> Option<String> {
    chats
        .get(chat_id)
        .map(|h| h.tmux_name.clone())
        .or_else(|| chat_session_on_disk(agent, chat_id).map(|(session, _, _)| session))
}

/// 同上,但解析 chat_type。`card_chat_type` 在 handle 缺失时兜底成 `"p2p"`,群会话会因此把
/// tag 从 `g…` 算成 `p…`,回执归档与面板刷新都落到幽灵 tag 上。需要 tag 正确的路径走这里。
pub(super) fn resolve_chat_type(
    chats: &HashMap<String, ChatHandle>,
    agent: &Agent,
    chat_id: &str,
) -> String {
    chats
        .get(chat_id)
        .map(|h| h.chat_type.clone())
        .or_else(|| chat_session_on_disk(agent, chat_id).map(|(_, chat_type, _)| chat_type))
        .unwrap_or_else(|| "p2p".to_string())
}

/// U4:把被点过的交互卡片**原地替换**成终态 markdown(按钮消失,不可再点)——闸门/恢复/TUI
/// 卡此前点完原卡不变、按钮永远可点,再点得到误导性「被拒」。原地替换失败(卡片过期等)退回
/// 发一条普通回执,保证用户总能看到结果。
pub(super) fn finalize_card(
    agent: &Agent,
    fc: &FeishuClient,
    chats: &HashMap<String, ChatHandle>,
    card: &CardActionMsg,
    text: &str,
) {
    let chat_type = card_chat_type(chats, &card.chat_id);
    // 飞书**拒绝跨 schema 替换**:拿 V1 内容去覆盖一张 schema 2.0 的卡会报 230099
    // 「schemaV2 card can not change schemaV1」,于是定格失败、按钮一直留在卡上可点。
    // 会走到这里的交互卡(提问、定时器、TUI 菜单、恢复卡)都是 2.0,终态也必须是 2.0。
    let content = summon_core::feishu::card_markdown_v2(text);
    if let Err(e) = update_card_archived(
        fc,
        &agent.state_dir(),
        &chat_type,
        &card.chat_id,
        &card.message_id,
        &content,
    ) {
        log(&summon_i18n::format!("r14ddcf4314", agent.name));
        let _ = send_markdown_archived(fc, &agent.state_dir(), &chat_type, &card.chat_id, text);
    }
}

/// 定时器卡片的 `(agent, chat)` 归属校验:回调只带 id,`schedules.yaml` 又是全局单文件,
/// **没有这道校验,A 群的 operator 能读改 B agent 私聊里的定时器**。读盘失败一律拒(fail-closed),
/// 绝不「读不出来当没这条、照做不误」。
///
/// 返回归属本会话的那条(`Ok(Some)`),不存在或不归本会话 → `Ok(None)`,读盘失败 → `Err(可读原因)`。
fn owned_schedule(
    agent: &Agent,
    chat_id: &str,
    id: &str,
) -> Result<Option<summon_core::Schedule>, String> {
    let st = crate::schedule::schedule_store::ScheduleStore::at_summon_home();
    let all = st.load().map_err(|e| summon_i18n::format!("r502c2cc52b"))?;
    Ok(all
        .into_iter()
        .find(|s| s.id == id && summon_core::authz::schedule_belongs_to(s, &agent.name, chat_id)))
}

/// 从卡片回调里取 `value.id`(编辑态才有)。
fn card_schedule_id(card: &CardActionMsg) -> Option<&str> {
    card.value.get("id").and_then(|v| v.as_str())
}

/// 「＋ 新建定时」/「‹ 换个频率」:原地把本卡换成起点卡。纯渲染,不碰盘。
/// 正文从 `form_value` 带回来(换频率路径),新建路径为空。
pub(super) fn handle_schedule_new(
    agent: &Agent,
    fc: &FeishuClient,
    chats: &HashMap<String, ChatHandle>,
    card: &CardActionMsg,
) {
    let form = summon_core::schedule::form::form_of(&card.value);
    let prompt = summon_core::schedule::form::prompt(&form);
    let content = summon_core::feishu::card_schedule_start(&prompt, card_schedule_id(card));
    swap_card(
        agent,
        fc,
        chats,
        card,
        &content,
        summon_i18n::text!("r5c15e6b4f3"),
    );
}

/// 起点卡的频率键:原地换成该频率的参数卡,正文随表单带过来。纯渲染,不碰盘。
pub(super) fn handle_schedule_pick(
    agent: &Agent,
    fc: &FeishuClient,
    chats: &HashMap<String, ChatHandle>,
    card: &CardActionMsg,
) {
    use summon_core::schedule::form::{self, CardFreq, Prefill};
    let Some(freq) = card
        .value
        .get("kind")
        .and_then(|v| v.as_str())
        .and_then(CardFreq::from_kind)
    else {
        finalize_card(agent, fc, chats, card, summon_i18n::text!("rda3d54c031"));
        return;
    };
    let f = form::form_of(&card.value);
    let edit_id = card_schedule_id(card);
    // 换频率时保留正文;编辑态另附「当前:…」一行,让用户始终看得见原值。
    let now = chrono::Local::now();
    let slot = default_time_slot(now);
    // 时长默认按频率给:「过多久」最常见是几十分钟内,「每隔」最常见是按小时。
    let (every_n, every_unit) = match freq {
        CardFreq::After => ("20", "m"),
        _ => ("1", "h"),
    };
    let pre = Prefill {
        prompt: form::prompt(&f),
        output: form::output(&f).unwrap_or_default(),
        time: slot.format("%H:%M").to_string(),
        date: slot.format("%Y-%m-%d").to_string(),
        every_n: every_n.to_string(),
        every_unit: every_unit.to_string(),
        ..Default::default()
    };
    let current = edit_id
        .and_then(|id| owned_schedule(agent, &card.chat_id, id).ok().flatten())
        .map(|s| current_line(&s));
    let content = summon_core::feishu::card_schedule_params(&ScheduleParamsCard {
        freq,
        pre: &pre,
        edit_id,
        current: current.as_deref(),
        error: None,
    });
    swap_card(
        agent,
        fc,
        chats,
        card,
        &content,
        summon_i18n::text!("r1bec3d5e56"),
    );
}

/// 新建时时刻选择器的默认值:**下一个整点**(分钟恒为 00,跨天则日期跟着走)。
///
/// why 不是「此刻」:`picker_time` 是小时、分钟两列**独立滚动**的。默认给 17:48 时,用户滚小时列
/// 到 12、分钟列没动,提交上来就是 12:48——他以为选的是 12:00。真机上第一次就中了这个
/// (SCH-6 落成 `48 12 * * 1-5`,那个 48 正是点频率键那一刻的分钟数),而且体感是「选完滑回当前
/// 时刻」,因为没动的那一列看着就像被拉了回去。
///
/// 分钟恒为 00 之后,「只改小时」这个最常见的操作不会再悄悄带上一个随机分钟。取**下一个**整点
/// 而非当前整点,是为了让一次性定时器的默认值天然落在将来(`to_spec` 会拒绝过去的时刻)。
fn default_time_slot(now: chrono::DateTime<chrono::Local>) -> chrono::DateTime<chrono::Local> {
    use chrono::Timelike;
    now.checked_add_signed(chrono::Duration::hours(1))
        .and_then(|t| t.with_minute(0))
        .and_then(|t| t.with_second(0))
        .and_then(|t| t.with_nanosecond(0))
        .unwrap_or(now)
}

/// 编辑态那行「当前:…」。已停用的必须带上标记——否则用户改完拿到一句「已更新」,
/// 却永远等不到它触发(`due_plan` 第一行就 `if !s.enabled { return none }`)。
fn current_line(s: &summon_core::Schedule) -> String {
    let base = format!("{} · {}", summon_core::human_readable(s), s.output.label());
    if s.enabled {
        base
    } else {
        summon_i18n::format!("r7ef89982f8")
    }
}

/// 列表卡「改」:按 id 取本会话名下那条 → 反推频率与预填 → 原地换成参数卡。
///
/// 纯渲染但**读盘**,所以归属校验一样不能省:否则卡上会画出别的会话的定时器正文。
/// 识别不了的自定义 cron(`*/15 9-18 * * *`)不硬画卡——退回起点卡让用户重选频率,
/// 避免拿一张画错的卡去覆盖人家亲手写的表达式。
pub(super) fn handle_schedule_edit(
    agent: &Agent,
    fc: &FeishuClient,
    chats: &HashMap<String, ChatHandle>,
    card: &CardActionMsg,
) {
    use summon_core::schedule::form::Prefill;
    let Some(id) = card_schedule_id(card) else {
        finalize_card(agent, fc, chats, card, summon_i18n::text!("r33e85a76cd"));
        return;
    };
    let found = match owned_schedule(agent, &card.chat_id, id) {
        Ok(found) => found,
        Err(e) => {
            finalize_card(agent, fc, chats, card, &format!("⏰ {e}"));
            return;
        }
    };
    let Some(s) = found else {
        log(&summon_i18n::format!(
            "ra1130e2c95",
            agent.name,
            card.chat_id
        ));
        finalize_card(agent, fc, chats, card, summon_i18n::text!("re64eb53e91"));
        return;
    };
    let current = current_line(&s);
    let content = match Prefill::of_schedule(&s) {
        Some((freq, pre)) => summon_core::feishu::card_schedule_params(&ScheduleParamsCard {
            freq,
            pre: &pre,
            edit_id: Some(id),
            current: Some(&current),
            error: None,
        }),
        None => {
            log(&summon_i18n::format!("r0488156c83", agent.name, s.spec));
            summon_core::feishu::card_schedule_start(&s.prompt, Some(id))
        }
    };
    swap_card(
        agent,
        fc,
        chats,
        card,
        &content,
        summon_i18n::text!("rca854c757c"),
    );
}

/// 参数卡「创建」:表单 → `(Kind, spec)` → 落盘 → 卡原地变确认回执。
/// operate-gate 已在 dispatcher 集中鉴权(schedule_create = operate 档)。
pub(super) fn handle_schedule_create(
    agent: &Agent,
    fc: &FeishuClient,
    chats: &HashMap<String, ChatHandle>,
    card: &CardActionMsg,
) {
    let (freq, form, prompt, output) = match parse_params_card(agent, fc, chats, card) {
        Some(parsed) => parsed,
        None => return,
    };
    let now = chrono::Local::now();
    let (kind, spec) = match summon_core::schedule::form::to_spec(freq, &form, now) {
        Ok(v) => v,
        Err(e) => {
            redraw_params_card(agent, fc, chats, card, freq, &form, None, &e);
            return;
        }
    };
    let st = crate::schedule::schedule_store::ScheduleStore::at_summon_home();
    let id = match st.next_id() {
        Ok(id) => id,
        Err(e) => {
            finalize_card(agent, fc, chats, card, &summon_i18n::format!("r0dda72a946"));
            return;
        }
    };
    // 落盘的 chat_type 必须走盘上兜底:它决定将来触发时按群还是按私聊处理。
    let chat_type = persisted_chat_type(chats, agent, &card.chat_id);
    let s = summon_core::Schedule {
        id,
        kind,
        spec,
        agent: agent.name.clone(),
        chat_id: card.chat_id.clone(),
        chat_type,
        prompt,
        created_by: card.operator_open_id.clone(),
        created_at: now.format("%Y-%m-%d %H:%M:%S").to_string(),
        output,
        context: summon_core::ScheduleContext::Resume,
        enabled: true,
        last_fired: None,
        revision: 0,
    };
    match st.add(&s) {
        Ok(()) => {
            log(&summon_i18n::format!(
                "rf03ace9003",
                agent.name,
                s.id,
                card.chat_id
            ));
            settle_params_card(
                agent,
                fc,
                chats,
                card,
                freq,
                &form,
                &summon_core::feishu::schedule_confirm_markdown(&s),
            );
        }
        Err(e) => finalize_card(agent, fc, chats, card, &summon_i18n::format!("re3d1f72ee1")),
    }
}

/// 参数卡「保存」:归属校验 → 覆盖节律与正文 → 卡原地变确认回执。
/// 改了节律时 `update_rhythm` 会把 `last_fired` 置为 now,回执明说「从下一个周期开始生效」。
pub(super) fn handle_schedule_save(
    agent: &Agent,
    fc: &FeishuClient,
    chats: &HashMap<String, ChatHandle>,
    card: &CardActionMsg,
) {
    let Some(id) = card_schedule_id(card).map(str::to_string) else {
        finalize_card(agent, fc, chats, card, summon_i18n::text!("r38d1bcc74c"));
        return;
    };
    // 写之前必须再过归属:operate-gate 只答「这个人能不能操作」,答不了「这条归不归他管」。
    // 顺带把原条目留下当回执的渲染底稿——只读这一次盘。
    let mut shown = match owned_schedule(agent, &card.chat_id, &id) {
        Ok(Some(s)) => s,
        Ok(None) => {
            log(&summon_i18n::format!(
                "rb89d1d60eb",
                agent.name,
                card.chat_id
            ));
            finalize_card(agent, fc, chats, card, summon_i18n::text!("re64eb53e91"));
            return;
        }
        Err(e) => {
            finalize_card(agent, fc, chats, card, &format!("⏰ {e}"));
            return;
        }
    };
    let (freq, form, prompt, output) = match parse_params_card(agent, fc, chats, card) {
        Some(parsed) => parsed,
        None => return,
    };
    let now = chrono::Local::now();
    let (kind, spec) = match summon_core::schedule::form::to_spec(freq, &form, now) {
        Ok(v) => v,
        Err(e) => {
            let current = current_line(&shown);
            redraw_params_card(agent, fc, chats, card, freq, &form, Some(&current), &e);
            return;
        }
    };
    let st = crate::schedule::schedule_store::ScheduleStore::at_summon_home();
    match st.update_rhythm(&id, kind, &spec, &prompt, output, &now.to_rfc3339()) {
        Ok(rescheduled) => {
            log(&summon_i18n::format!(
                "rc71176822c",
                agent.name,
                card.chat_id
            ));
            // 回执用刚写进去的值渲染(底稿是校验时读到的那条),不再回盘读一次。
            shown.kind = kind;
            shown.spec = spec;
            shown.prompt = prompt;
            shown.output = output;
            let text = summon_core::feishu::schedule_saved_markdown(&shown, rescheduled);
            settle_params_card(agent, fc, chats, card, freq, &form, &text);
        }
        Err(e) => finalize_card(agent, fc, chats, card, &summon_i18n::format!("rd23af352cb")),
    }
}

/// 建成 / 存好之后把参数卡**原地钉死**:保留用户填的内容,所有控件置灰,底部追加回执。
///
/// why 不留一张能点的卡:聊天记录会一直保存它,滚上去再点一次「创建」就会重复建一条定时器,
/// 或者拿一组过期的值覆盖掉已经改好的那条。置灰是唯一能同时保住「看得见填了什么」和
/// 「点不动」的形态。
#[allow(clippy::too_many_arguments)] // 全是渲染所需上下文,拆结构体反而割裂调用点
fn settle_params_card(
    agent: &Agent,
    fc: &FeishuClient,
    chats: &HashMap<String, ChatHandle>,
    card: &CardActionMsg,
    freq: summon_core::schedule::form::CardFreq,
    form: &serde_json::Value,
    receipt: &str,
) {
    let pre = summon_core::schedule::form::Prefill::from_form(form);
    let content = summon_core::feishu::card_schedule_settled(
        &ScheduleParamsCard {
            freq,
            pre: &pre,
            edit_id: card_schedule_id(card),
            current: None,
            error: None,
        },
        receipt,
    );
    let chat_type = card_chat_type(chats, &card.chat_id);
    if let Err(e) = update_card_archived(
        fc,
        &agent.state_dir(),
        &chat_type,
        &card.chat_id,
        &card.message_id,
        &content,
    ) {
        // 卡片钉不死时退回纯文本回执:宁可丢掉「看得见填了什么」,也不能留一张还能点的卡。
        log(&summon_i18n::format!("rb383b415a5", agent.name));
        finalize_card(agent, fc, chats, card, receipt);
    }
}

/// 校验没过时把参数卡**按用户刚填的内容重画**,顶部挂一行原因。
///
/// why 不用 `finalize_card`:那会把整张卡换成一句纯文本,用户已填的正文 / 星期 / 时间全部蒸发,
/// 只能从 `/remind` 重来。填错时间是最常见的一次失误,代价不该是重填全部。
#[allow(clippy::too_many_arguments)] // 全是渲染所需的上下文,拆成结构体反而更绕
fn redraw_params_card(
    agent: &Agent,
    fc: &FeishuClient,
    chats: &HashMap<String, ChatHandle>,
    card: &CardActionMsg,
    freq: summon_core::schedule::form::CardFreq,
    form: &serde_json::Value,
    current: Option<&str>,
    error: &str,
) {
    let pre = summon_core::schedule::form::Prefill::from_form(form);
    let content = summon_core::feishu::card_schedule_params(&ScheduleParamsCard {
        freq,
        pre: &pre,
        edit_id: card_schedule_id(card),
        current,
        error: Some(error),
    });
    swap_card(
        agent,
        fc,
        chats,
        card,
        &content,
        summon_i18n::text!("r480eceffbd"),
    );
}

/// 参数卡提交的共通前半段:解析频率 + 取表单 + 校验正文非空。失败时已就地回执,返回 `None`。
fn parse_params_card(
    agent: &Agent,
    fc: &FeishuClient,
    chats: &HashMap<String, ChatHandle>,
    card: &CardActionMsg,
) -> Option<(
    summon_core::schedule::form::CardFreq,
    serde_json::Value,
    String,
    summon_core::ScheduleOutput,
)> {
    use summon_core::schedule::form::{self, CardFreq};
    let Some(freq) = card
        .value
        .get("kind")
        .and_then(|v| v.as_str())
        .and_then(CardFreq::from_kind)
    else {
        finalize_card(agent, fc, chats, card, summon_i18n::text!("rda3d54c031"));
        return None;
    };
    let f = form::form_of(&card.value);
    let prompt = form::prompt(&f);
    if prompt.is_empty() {
        // 控件 required 不可信:空正文的定时器到点只会往群里发一句「⏰ 定时任务:」,毫无意义。
        // 重画卡而不是换成纯文本——用户填过的频率参数得留着。
        redraw_params_card(
            agent,
            fc,
            chats,
            card,
            freq,
            &f,
            None,
            summon_i18n::text!("r87e34e31d3"),
        );
        return None;
    }
    let output = match form::output(&f) {
        Ok(output) => output,
        Err(error) => {
            redraw_params_card(agent, fc, chats, card, freq, &f, None, &error);
            return None;
        }
    };
    Some((freq, f, prompt, output))
}

/// 原地把本卡换成另一张卡(纯渲染类动作共用)。
///
/// **失败必须让用户看见。** 这类动作的全部反馈就是「卡片变了」——换卡失败又只写日志的话,用户看到
/// 的是一张纹丝不动的卡,只能靠反复点来猜(2026-08-03 真机上就这么连点了 5 次)。所以失败时补发
/// 一条普通消息说清楚,而不是让人对着哑掉的卡片较劲。
fn swap_card(
    agent: &Agent,
    fc: &FeishuClient,
    chats: &HashMap<String, ChatHandle>,
    card: &CardActionMsg,
    content: &str,
    what: &str,
) {
    let chat_type = card_chat_type(chats, &card.chat_id);
    if let Err(e) = update_card_archived(
        fc,
        &agent.state_dir(),
        &chat_type,
        &card.chat_id,
        &card.message_id,
        content,
    ) {
        log(&summon_i18n::format!("r22d3145d3b", agent.name));
        // 回执本身走新消息而不是再 PATCH 一次——卡片刚刚就是 PATCH 不动才走到这里的。
        let _ = send_markdown_archived(
            fc,
            &agent.state_dir(),
            &chat_type,
            &card.chat_id,
            &summon_i18n::format!("rf3bed0d0a0"),
        );
    }
}

/// 定时器「取消」按钮回流:归属校验 → 停用该 id(`set_enabled(false)`)→ 用最新列表原地刷新本卡
///(被停那条变「(已停)」、按钮消失)。id 缺失 / 不归本会话 / 停用失败 → 把卡原地置成终态错误回执。
/// operate-gate 已在 dispatcher 集中查表完成(schedule_cancel = operate 档),但那只答「这个人能不能
/// 操作」;**答不了「这条定时器归不归他管」**——`schedules.yaml` 是全局单文件、回调只带 id,故这里
/// 必须再过一次 (agent, chat) 归属,否则 A 群的 operator 能停掉 B agent 私聊里的定时器。
pub(super) fn handle_schedule_cancel(
    agent: &Agent,
    fc: &FeishuClient,
    chats: &HashMap<String, ChatHandle>,
    card: &CardActionMsg,
) {
    let Some(id) = card.value.get("id").and_then(|v| v.as_str()) else {
        finalize_card(agent, fc, chats, card, summon_i18n::text!("rfab06b874a"));
        return;
    };
    let st = crate::schedule::schedule_store::ScheduleStore::at_summon_home();
    // 归属校验要先于任何写:读不出来就拒(fail-closed),绝不「读失败当没这条、照停不误」。
    let all = match st.load() {
        Ok(all) => all,
        Err(e) => {
            finalize_card(agent, fc, chats, card, &summon_i18n::format!("r706a2650ed"));
            return;
        }
    };
    let owned = all.iter().any(|s| {
        s.id == id && summon_core::authz::schedule_belongs_to(s, &agent.name, &card.chat_id)
    });
    if !owned {
        log(&summon_i18n::format!(
            "r4aae2a675d",
            agent.name,
            card.chat_id
        ));
        finalize_card(agent, fc, chats, card, summon_i18n::text!("r60029fe3eb"));
        return;
    }
    if let Err(e) = st.set_enabled(id, false) {
        finalize_card(agent, fc, chats, card, &summon_i18n::format!("r31f3f72405"));
        return;
    }
    log(&summon_i18n::format!(
        "r1f31f1e62d",
        agent.name,
        card.chat_id
    ));
    // 原地刷新:重渲染最新列表卡。状态已落盘生效,卡片只是观感——刷新失败仅 log,不退回兜底
    // 回执(那会用一行错误覆盖掉整张列表)。
    let chat_type = card_chat_type(chats, &card.chat_id);
    let all = match st.load() {
        Ok(all) => all,
        Err(e) => {
            log(&summon_i18n::format!("r0fe9a78c08", agent.name));
            return;
        }
    };
    // 刷新只渲染本 (agent, chat) 名下的定时器:看得见的才点得动,别把别的会话的条目连按钮一起画出来。
    let visible = summon_core::authz::schedules_owned_by(&all, &agent.name, &card.chat_id);
    let timer_reset = resolve_settings(
        agent,
        &sidecar_session_key(
            chats.get(&card.chat_id).map(|h| h.tmux_name.as_str()),
            agent,
            &chat_type,
            &card.chat_id,
        ),
        &resolve_messaging(agent),
    )
    .timer_idle_retire_reset();
    let content = summon_core::feishu::card_schedules_panel(&visible, timer_reset);
    if let Err(e) = update_card_archived(
        fc,
        &agent.state_dir(),
        &chat_type,
        &card.chat_id,
        &card.message_id,
        &content,
    ) {
        log(&summon_i18n::format!("r10443da402", agent.name));
    }
}

/// schedules 面板「定时归档」开关:翻转本 chat 的 per-chat 重置覆盖并原地刷新面板。
/// 开关显示的是当前**生效值**(per-chat 覆盖优先于 agent 维度配置),点击翻成对面的显式值。
pub(super) fn handle_schedule_timer_reset_toggle(
    agent: &Agent,
    fc: &FeishuClient,
    chats: &HashMap<String, ChatHandle>,
    card: &CardActionMsg,
) {
    // chat_type 回退用 persisted_chat_type(fail-closed 到 group):card_chat_type 回退 p2p,
    // 群聊在无 handle 时会 derive 出错键。
    let chat_type = persisted_chat_type(chats, agent, &card.chat_id);
    let session = sidecar_session_key(
        chats.get(&card.chat_id).map(|h| h.tmux_name.as_str()),
        agent,
        &chat_type,
        &card.chat_id,
    );
    let messaging = resolve_messaging(agent);
    let current = resolve_settings(agent, &session, &messaging).timer_idle_retire_reset();
    // 落盘失败必须放弃翻转:接着往下渲染会画出一个「已翻转」的开关,而盘上还是旧值 ——
    // 用户看到的状态与实际生效的不一致,比点了没反应更糟。
    if let Err(error) = set_chat_setting(
        agent,
        &session,
        summon_core::settings::SettingKey::TimerIdleRetireReset,
        Some(!current),
    ) {
        log(&summon_i18n::format!("r2d42c8f31d", agent.name));
        return;
    }
    log(&summon_i18n::format!(
        "r025e32d808",
        agent.name,
        if current {
            summon_i18n::text!("re9f19158b4")
        } else {
            summon_i18n::text!("rcb5d682bac")
        },
        card.chat_id
    ));
    // 原地刷新:重渲染最新列表卡(带翻转后的开关态)。
    let st = crate::schedule::schedule_store::ScheduleStore::at_summon_home();
    let all = match st.load() {
        Ok(all) => all,
        Err(e) => {
            log(&summon_i18n::format!("r0fe9a78c08", agent.name));
            return;
        }
    };
    let visible = summon_core::authz::schedules_owned_by(&all, &agent.name, &card.chat_id);
    let content = summon_core::feishu::card_schedules_panel(&visible, !current);
    if let Err(e) = update_card_archived(
        fc,
        &agent.state_dir(),
        &chat_type,
        &card.chat_id,
        &card.message_id,
        &content,
    ) {
        log(&summon_i18n::format!("r10443da402", agent.name));
    }
}

/// 单选卡的这次点击是否仍落在发卡时那一张菜单上:按钮 value 里的签名与此刻屏上菜单的签名一致。
/// 没带签名的卡一律不认 —— 证明不了是同一张,就不替用户按 Enter。
fn choice_card_matches_screen(value: &serde_json::Value, pane: &str) -> bool {
    let Some(expected) = value.get("signature").and_then(|v| v.as_str()) else {
        return false;
    };
    summon_core::interactive::parse_interactive(pane)
        .is_some_and(|menu| summon_core::interactive::signature_token(menu.signature()) == expected)
}

/// 这次点击(注入已经成功)答掉的是哪张菜单:只有带签名的单选卡算。自由文本与 Esc 没绑定菜单身份。
fn answered_menu_signature(action: &str, value: &serde_json::Value) -> Option<u64> {
    (action == "tui_choice")
        .then(|| value.get("signature").and_then(|v| v.as_str()))
        .flatten()
        .and_then(summon_core::interactive::parse_signature_token)
}

/// 交互菜单中介(Q1)回流:把用户在飞书卡片上的选择/文本注入该 chat 的 tmux pane(claude 菜单在那
/// 等输入)。chat 线程此刻正阻塞在 run_turn tail,注入由 dispatcher 线程并发 `send-keys` 安全完成——
/// claude 收到按键后推进,tail 随即捕获后续输出并正常收尾。chat 不在线 / 注入失败均 log 忽略,不中断。
pub(super) fn handle_tui_inject(
    chats: &HashMap<String, ChatHandle>,
    agent: &Agent,
    fc: &FeishuClient,
    card: &CardActionMsg,
    action: &str,
) {
    let chat_id = &card.chat_id;
    // 鉴权在 dispatcher 集中查表完成(T4:card_gate_tier → operate 档,自由文本即任意指令注入)。
    let Some(h) = chats.get(chat_id) else {
        log(&summon_i18n::format!("ra8c022f808", agent.name));
        return;
    };
    let tmux = crate::session::tmux::Tmux::new(h.tmux_name.clone());
    // 陈旧点击防护:三个交互动作注入前都重抓屏,确认菜单**仍在**(parse_interactive 命中)。
    //
    // 三张交互卡(单选卡 / 降级自由文本卡)都由 `parse_interactive` 命中之后才发出去,所以
    // 「此刻还能解析出菜单」就是它们共同的、**正向**的新鲜度判据 —— 卡片躺在飞书里几分钟,期间
    // 菜单可能已被别的点击解决、attach 用户接管、或下一条消息起了新一轮。此时:
    // - `tui_esc` 的 Escape 会落在**运行中的轮**上 = 打断键(正是废弃分支 fix/prompt-arrival-liveness
    //   被否决的那个错误,只是入口换了一个);落在带草稿的空闲输入框上 = 清掉用户打了一半的文字。
    //   「Esc 空闲态无害」这句旧注释只在「空闲且输入框是空的」时成立,而那恰恰是无法假定的前提。
    // - `tui_text` 的自由文本会被此刻屏上的全屏面板整条吞掉(`/cost` 面板吃掉紧跟输入是实测),
    //   零证明、零外显 —— spec 第二节点名的第 ① 条盲注路径。
    //
    // 这里**不能**改用通用注入闸(`input_gate`):那道闸对 `Modal(Menu)` 恒拒,而菜单正是这三个
    // 动作的目标状态,压上去等于把整条交互中介链路拒死。判据必须是「目标状态本身仍在」。
    if matches!(action, "tui_choice" | "tui_text" | "tui_esc") {
        let pane = tmux.capture().unwrap_or_default();
        if summon_core::interactive::parse_interactive(&pane).is_none() {
            log(&summon_i18n::format!("rc1b4021924", agent.name));
            // U4:过期菜单卡原地置成终态,按钮不再可点。
            finalize_card(agent, fc, chats, card, summon_i18n::text!("r5587aa8b3c"));
            return;
        }
        // 单选卡的按钮只对发卡时那一张菜单有效。屏上换成了另一张(子代理的下一条授权、终端里有人
        // 答掉后又弹出新的),这一下 Enter 批准的就是用户在卡上从没见过的内容。
        if action == "tui_choice" && !choice_card_matches_screen(&card.value, &pane) {
            log(&summon_i18n::format!("rbcd4fe71f2", agent.name));
            let notice = if card.value.get("signature").is_some() {
                summon_i18n::text!("radf665902b")
            } else {
                summon_i18n::text!("rdd8d5d3d66")
            };
            finalize_card(agent, fc, chats, card, notice);
            return;
        }
    }
    // 立即重试的陈旧点击防护。判据必须是**重试态本身仍在**:claude 一旦自己恢复,注入的
    // 「继续」就落到空闲 prompt 上、凭空多跑一整轮 —— 那恰恰是这个按钮要替用户避免的事。
    // 不 `finalize_card`:这个按钮长在 live 卡上,由 chat 线程持续 patch,外部覆盖会和它打架;
    // 重试态一结束,下一个心跳就把按钮和倒计时一起抹掉。
    if action == "retry_now" {
        let pane = tmux.capture().unwrap_or_default();
        if summon_core::retry_wait::parse_retry_wait(&pane).is_none() {
            log(&summon_i18n::format!("r3350e76a1a", agent.name));
            return;
        }
    }
    // copy-mode(滚屏)会吞 send-keys,注入前先退出一次(幂等),与正规 prompt 注入路径一致(review #6)。
    tmux.exit_copy_mode();
    let res = match action {
        // 单选:ordinal=0-based 位置 → 从首项按 Down×ordinal 再 Enter(菜单光标默认在首项)。
        // clamp 上界(review #5):ordinal 取自不可信卡片 value,封顶防巨值注入循环。
        "tui_choice" => {
            let ord = card
                .value
                .get("ordinal")
                .and_then(|v| v.as_u64())
                .unwrap_or(0)
                .min(64);
            log(&summon_i18n::format!("rac2ece7ef4", agent.name));
            (|| -> std::io::Result<()> {
                for _ in 0..ord {
                    tmux.send_key("Down")?;
                }
                tmux.send_enter()
            })()
        }
        // 文本/降级:表单文本逐字键入 + Enter。
        "tui_text" => {
            let text = card
                .value
                .get("form_value")
                .and_then(|f| f.get("tui_text_input"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            if text.is_empty() {
                log(&summon_i18n::format!("r9ff038fb85", agent.name));
                return;
            }
            log(&summon_i18n::format!("rbf18c294bd", agent.name));
            tmux.send_line(&text)
        }
        // 取消菜单:送 Escape。授权凭据是上面那道「菜单此刻仍在」的正向校验 —— 屏上确实停着一个
        // 被识别出来的菜单,Esc 在它上面的语义就是取消这个菜单,不会打断别人的轮、不会清用户草稿。
        "tui_esc" => {
            log(&summon_i18n::format!("r8fdc522d18", agent.name));
            tmux.send_key("Escape")
        }
        // 立即重试:先把「继续」提交进 claude 自己的队列,再打断当前那个在退避等待的请求。
        // **顺序不能反**。先入队后打断,两种可能的 Esc 语义都安全:若 Esc 只掐掉退避、claude 自行
        // 重发,队列里的「继续」最多是句无害跟进;若 Esc 结束了这一轮,「继续」正好被消费、接着往下做。
        // 反过来先 Esc 则会留下一个无人接续的空档。
        // 用「继续」而不是重发原 prompt:本轮已做过的工具调用都还在上下文里,重发会让它重复执行一段。
        "retry_now" => {
            log(&summon_i18n::format!("r71374dd735", agent.name));
            (|| -> std::io::Result<()> {
                tmux.send_line(summon_i18n::text!("r7c9691192f"))?;
                // 等一拍再打断。两次 send-keys 只是把按键塞进 pane,claude 何时读到不由我们决定;
                // Escape 若抢在 Enter 前被处理,会被当成「清空输入行」而不是「中断请求」——
                // 那样「继续」当场消失,只剩一次纯打断。
                std::thread::sleep(std::time::Duration::from_millis(300));
                tmux.send_key("Escape")
            })()
        }
        _ => return,
    };
    if let Err(e) = res {
        log(&summon_i18n::format!("red9d080caf", agent.name));
        return;
    }
    // 卡片上的单选已经进了会话:这张菜单答掉了。同一个问题紧接着再弹出(子代理排队申请同一条命令)
    // 是新问题,账本要让轮外守望知道(见 `MenuLedger::note_answered`)。
    if let (Some(ledger), Some(signature)) = (
        h.menu_ledger.as_ref(),
        answered_menu_signature(action, &card.value),
    ) {
        ledger.note_answered(signature);
    }
    // U4:注入成功 → 原卡置终态。tui_choice 用置灰按钮 + 确认行(用户要求:按钮不消失、置灰、
    // 确认行像流式卡「已打断」那样单独留一行);tui_text/tui_esc 用简单确认。
    let chat_type = card_chat_type(chats, chat_id);
    match action {
        "tui_choice" => {
            let ord = card
                .value
                .get("ordinal")
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as usize;
            // 从 card.value 取回 title + options 快照(由 card_interactive_select 注入)。
            let title = card
                .value
                .get("title")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let options: Vec<(usize, String)> = card
                .value
                .get("options")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|item| {
                            let pair = item.as_array()?;
                            let num = pair.first()?.as_u64()? as usize;
                            let label = pair.get(1)?.as_str()?.to_string();
                            Some((num, label))
                        })
                        .collect()
                })
                .unwrap_or_default();
            let content = summon_core::feishu::card_interactive_select_result(title, &options, ord);
            if let Err(e) = update_card_archived(
                fc,
                &agent.state_dir(),
                &chat_type,
                chat_id,
                &card.message_id,
                &content,
            ) {
                log(&summon_i18n::format!("r5ff11257de", agent.name));
                let fallback = summon_i18n::format!("r2108af7b35", ord + 1);
                let _ =
                    send_markdown_archived(fc, &agent.state_dir(), &chat_type, chat_id, &fallback);
            }
        }
        "tui_text" => {
            let text = card
                .value
                .get("form_value")
                .and_then(|f| f.get("tui_text_input"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            // 降级态无按钮可置灰,用简单确认行。
            let done = summon_i18n::format!("r3bd8d49982", text);
            finalize_card(agent, fc, chats, card, &done);
        }
        _ => {
            finalize_card(agent, fc, chats, card, summon_i18n::text!("r9868d6b169"));
        }
    }
}

/// 路由生命周期类卡片动作到 chat 线程:KeepAlive/Compact 不鉴权;Clean/Reset 破坏性,经 operate-gate
///(operators 优先、空回退 allowed_users、群里 fail-closed),非授权挡下并回执。chat 不在线则记日志。
pub(super) fn route_lifecycle(
    chats: &mut HashMap<String, ChatHandle>,
    agent: &Agent,
    fc: &FeishuClient,
    card: &CardActionMsg,
    ctrl: Control,
    messaging: &Messaging,
    origin: CardOrigin,
) {
    let chat_id = &card.chat_id;
    match ctrl {
        Control::KeepAlive => {
            super::waiting::clear_on_keepalive(
                agent,
                fc,
                // chat 不在线时卡片仍代表点击人的意思,类型从盘上取。
                &persisted_chat_type(chats, agent, chat_id),
                super::waiting::IdleCardClick {
                    // 未登记动作也分派到这里,判据要分得出真正的保活按钮。
                    action: summon_core::feishu::CardAction::from_action(
                        card.value
                            .get("action")
                            .and_then(|v| v.as_str())
                            .unwrap_or(""),
                    ),
                    from_idle_card: matches!(origin, CardOrigin::Idle(_)),
                    operator_open_id: &card.operator_open_id,
                    is_group: card_chat_is_group(chats, agent, chat_id),
                    chat_id: &card.chat_id,
                },
            );
            if let Some(h) = chats.get_mut(chat_id) {
                // 保活是人点的按钮 = 人类活动,会话回到普通空闲规则:时钟/空闲卡去重(含
                // durable marker,否则重启后仍判「已提醒」漏下段提醒)/来源代际统一收口,
                // 在途的定时 admission 一并作废(人已表态跟进,不得再判定时尾轮)。
                h.note_activity(agent, chat_id, Some(&card.message_id));
                log(&summon_i18n::format!("re2614a2c60", agent.name));
            }
            // 空闲卡「保活」:定格卡片(置灰 + 「你已交互/已保活」footer),按钮不再可点。
            // /session 面板无保活按钮,origin=Panel 不进这条;上面的可变借用已结束。
            if let CardOrigin::Idle(state) = origin {
                patch_idle_closed_or_reply(
                    fc,
                    agent,
                    &card_chat_type(chats, chat_id),
                    chat_id,
                    &card.message_id,
                    state,
                    summon_core::feishu::IdleClosedReason::KeepAlive,
                );
            }
        }
        Control::Compact => match chats.get(chat_id) {
            Some(h) => {
                let _ = h.tx.send(ThreadMsg::Compact {
                    message_id: card.message_id.clone(),
                    messaging: Box::new(messaging.clone()),
                    origin,
                });
                log(&summon_i18n::format!("ra5ce103bc8", agent.name));
            }
            None => log(&summon_i18n::format!("ra258c711f5", agent.name)),
        },
        Control::Clean | Control::Reset | Control::Archive | Control::Resume => {
            // 文字命令是消息,准入时已清;卡片按钮单独在这里清。chat 不在线时卡片仍代表 owner 的
            // 意思,类型从盘上取,在线表里没有它。
            super::waiting::clear_on_card(
                agent,
                &persisted_chat_type(chats, agent, chat_id),
                chat_id,
                ctrl,
            );
            // 鉴权在 dispatcher 集中查表完成(T4:clean/reset → operate 档)。
            let label = format!("{ctrl:?}");
            match chats.get(chat_id) {
                Some(h) => {
                    // 带上被点面板卡片的 message_id + 当时模型候选(per-CLI 桶,与面板渲染同源,
                    // 别再传扁平 messaging.models——那是各 CLI 命名空间混在一起的回退表):
                    // 退出收尾后把面板 PATCH 成置灰终态。
                    let _ = h.tx.send(ThreadMsg::Control {
                        ctrl,
                        // 用户点的按钮,不是空闲扫描。
                        idle_retired: false,
                        card_message_id: Some(card.message_id.clone()),
                        models: model_candidates(
                            agent,
                            &chat_tag(&card_chat_type(chats, chat_id), chat_id),
                            summon_core::model_effort::options_for(messaging, agent.cli_kind())
                                .models,
                        ),
                        origin,
                        // 卡片按钮点出来的拆除就是用户主动退出。
                        notify_exit: true,
                        source_task: None,
                        source_started: false,
                    });
                    log(&summon_i18n::format!("red8d9043bc", agent.name));
                }
                None => log(&summon_i18n::format!("ra258c711f5", agent.name)),
            }
        }
    }
}

/// 上下文提醒卡上的三个按钮。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ContextWarnAction {
    Compact,
    Handoff,
    Ignore,
}

impl ContextWarnAction {
    pub(super) fn of(action: summon_core::feishu::CardAction) -> Option<Self> {
        use summon_core::feishu::CardAction;
        match action {
            CardAction::ContextCompact => Some(Self::Compact),
            CardAction::ContextHandoff => Some(Self::Handoff),
            CardAction::ContextIgnore => Some(Self::Ignore),
            _ => None,
        }
    }
}

/// 处理上下文提醒卡点击时要用到的 dispatcher 侧状态。
pub(super) struct ContextWarnClick<'a> {
    pub(super) inbox: &'a crate::runtime::inbox_wal::InboxWal,
    pub(super) bridge: &'a crate::bridge::store::BridgeStore,
    pub(super) delivery_scope: &'a str,
    pub(super) self_tx: &'a Sender<DispatchMsg>,
    /// 本代 daemon 里已经被点过的提醒卡(message_id)。
    pub(super) claimed: &'a mut std::collections::HashSet<String>,
}

/// 上下文提醒卡(owner 2026-10-15)的点击:Compact 交给会话线程压缩;Handoff 等同在本群发
/// `/handoff <输入框内容>`(不带目标);Ignore 只定格卡片。鉴权已在 dispatcher 集中做完。
///
/// - **一张卡只认第一次点击**:PATCH 生效之前按钮还点得到,连点、或换个按钮再点都不能再做一遍。
///   认领表只活在本代 daemon;换代之后卡已定格,Handoff 另有 WAL 键兜底。没执行成的点击
///   (会话没接上、群在 Bridge / 遥控中、落盘失败)放开认领,卡还能再点。
/// - **会话换过就不作用**:卡上带发卡时的原生会话 id,与盘上当前记录比。Handoff 会归档或压缩
///   会话,证不了是同一个会话就不执行(证得换过与对不上分开说) —— 旧会话的交接写在新会话上只会
///   是一份空交接,再把新会话也归档或压掉。证不了时卡上还有 Compact 的(claude 关了续聊时盘上
///   没有记录)不定格整张卡,只回一句原因,Compact 照样能点。
///   Compact 只作用于此刻的会话、不拆任何东西,只在证得换过时才拒。Ignore 只动卡片,不比。
pub(super) fn handle_context_warn_card(
    chats: &HashMap<String, ChatHandle>,
    agent: &Agent,
    fc: &FeishuClient,
    card: &CardActionMsg,
    action: ContextWarnAction,
    messaging: &Messaging,
    click: ContextWarnClick<'_>,
) {
    use summon_core::feishu::ContextWarnOutcome;
    let Some(warn) = summon_core::feishu::ContextWarnCard::from_value(&card.value) else {
        log(&summon_i18n::format!(
            "rf0aad12417",
            agent.name,
            card.chat_id,
            card.message_id
        ));
        return;
    };
    if !click.claimed.insert(card.message_id.clone()) {
        log(&summon_i18n::format!(
            "r5172ac8880",
            agent.name,
            card.chat_id,
            card.message_id
        ));
        return;
    }
    let chat_type = persisted_chat_type(chats, agent, &card.chat_id);
    let close = |outcome: &ContextWarnOutcome| {
        patch_context_warn(
            fc,
            agent,
            &chat_type,
            &card.chat_id,
            &card.message_id,
            &warn,
            outcome,
        )
    };
    // 没执行:放开认领、卡保持可点,用一条消息说明原因。
    let decline = |claimed: &mut std::collections::HashSet<String>, reason: &str| {
        claimed.remove(&card.message_id);
        let _ = send_markdown_archived(fc, &agent.state_dir(), &chat_type, &card.chat_id, reason);
    };
    let card_sid = summon_core::feishu::context_warn_session(&card.value);
    let current_sid = || {
        resolve_chat_session(chats, agent, &card.chat_id).and_then(|session| {
            crate::session::resume_store::read_id(agent, &session, &agent.resume_cli_kind())
        })
    };
    let fenced = match action {
        ContextWarnAction::Ignore => None,
        ContextWarnAction::Compact => card_sid
            .and_then(|sid| current_sid().filter(|current| current != sid))
            .map(|_| ContextWarnOutcome::Stale),
        ContextWarnAction::Handoff => match (card_sid, current_sid()) {
            (Some(sid), Some(current)) if current == sid => None,
            (Some(_), Some(_)) => Some(ContextWarnOutcome::Stale),
            _ => Some(ContextWarnOutcome::Unverified),
        },
    };
    if let Some(outcome) = fenced {
        log(&summon_i18n::format!(
            "rb37bd96870",
            agent.name,
            card.chat_id
        ));
        if outcome == ContextWarnOutcome::Unverified
            && warn.remedy == summon_core::context_budget::ContextRemedy::Compact
        {
            decline(click.claimed, summon_i18n::text!("r6decc7939c"));
        } else {
            close(&outcome);
        }
        return;
    }
    match action {
        ContextWarnAction::Ignore => close(&ContextWarnOutcome::Ignored),
        ContextWarnAction::Compact => {
            const NOT_ATTACHED: &str = "rdcb16987c9";
            let Some(handle) = chats.get(&card.chat_id) else {
                decline(click.claimed, ::summon_core::i18n::label(NOT_ATTACHED));
                return;
            };
            // 先定格成「正在压缩」再交给会话线程:线程那边失败得快时,它写的终态不能被这一步盖掉。
            close(&ContextWarnOutcome::Compacting);
            let sent = handle.tx.send(ThreadMsg::Compact {
                message_id: card.message_id.clone(),
                messaging: Box::new(messaging.clone()),
                origin: CardOrigin::ContextWarn(warn),
            });
            match sent {
                Ok(()) => log(&summon_i18n::format!(
                    "r1348846355",
                    agent.name,
                    card.chat_id
                )),
                Err(_) => {
                    // 线程恰在这一刻退出:卡改回可点的原样。
                    let reopened = summon_core::feishu::card_context_warning(&warn, card_sid);
                    let _ = update_card_archived(
                        fc,
                        &agent.state_dir(),
                        &chat_type,
                        &card.chat_id,
                        &card.message_id,
                        &reopened,
                    );
                    decline(click.claimed, ::summon_core::i18n::label(NOT_ATTACHED));
                }
            }
        }
        ContextWarnAction::Handoff => {
            if let Some(reason) = handoff_blocked(agent, &chat_type, &card.chat_id, &click) {
                log(&summon_i18n::format!(
                    "r581cbb48ec",
                    agent.name,
                    card.chat_id
                ));
                decline(click.claimed, &reason);
                return;
            }
            start_context_handoff(agent, fc, card, &chat_type, click, decline, &close)
        }
    }
}

/// 群在 Bridge / 遥控中时,本地不起会话,代投的写交接轮会被 dispatcher 跳过;点的时候就拒,卡上不留
/// 「已发起交接」。状态读不出同样按占用处理。
fn handoff_blocked(
    agent: &Agent,
    chat_type: &str,
    chat_id: &str,
    click: &ContextWarnClick<'_>,
) -> Option<String> {
    if !matches!(chat_type, "group" | "topic_group") {
        return None;
    }
    let target =
        summon_core::bridge::ScopedChatKey::new(click.delivery_scope, &agent.name, chat_id);
    match bridge_slot_message(click.bridge, &target) {
        Ok(None) => {}
        Ok(Some(slot)) => return Some(summon_i18n::format!("ra9876ebef7")),
        Err(error) => return Some(summon_i18n::format!("r3488885675")),
    }
    crate::remote::inbound::remote_bound(agent, chat_id)
        .then(|| summon_i18n::text!("r3a98212f3a").to_string())
}

/// 提醒卡的 Handoff:把写交接那一轮落进本 agent 的入站 WAL(`fire_id` = `ctx-handoff-<卡片 id>`,
/// 同一张卡只落一次),再经代投通道交给 dispatcher。之后与 `/handoff <备注>` 同一条路:chat 线程
/// 拿到带 `handoff` 的 task,轮末校验交接文件、归档、新会话接着做。
///
/// 键已被占就是这张卡已经发起过(后一代 daemon 里重新提交,载荷可能不同),不当失败,只按盘上
/// 那一次补定格:还在待跑写「已发起交接」和那次的接手重点,已销账写「已处理过」。真落盘失败才
/// 放开认领。
fn start_context_handoff(
    agent: &Agent,
    fc: &FeishuClient,
    card: &CardActionMsg,
    chat_type: &str,
    click: ContextWarnClick<'_>,
    decline: impl FnOnce(&mut std::collections::HashSet<String>, &str),
    close: &dyn Fn(&summon_core::feishu::ContextWarnOutcome),
) {
    use summon_core::feishu::ContextWarnOutcome;
    let fire_id = format!("ctx-handoff-{}", card.message_id);
    let failed = |error: anyhow::Error| {
        log(&summon_i18n::format!(
            "r8277f2ae3c",
            agent.name,
            card.chat_id
        ));
    };
    match click.inbox.recorded_sentinel(&agent.name, &fire_id) {
        Ok(Some(recorded)) => {
            log(&summon_i18n::format!(
                "re9c7b66501",
                agent.name,
                recorded.consumed,
                card.chat_id,
                card.message_id
            ));
            close(&if recorded.consumed {
                ContextWarnOutcome::HandoffDone
            } else {
                ContextWarnOutcome::HandoffStarted {
                    note: recorded
                        .payload
                        .and_then(|payload| payload.handoff_note)
                        .unwrap_or_default(),
                }
            });
            return;
        }
        Ok(None) => {}
        Err(error) => {
            failed(error);
            decline(
                click.claimed,
                ::summon_core::i18n::label(HANDOFF_NOT_WRITTEN),
            );
            return;
        }
    }
    let note = card
        .value
        .get("form_value")
        .and_then(|form| form.get(summon_core::feishu::CTX_HANDOFF_NOTE_FIELD))
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .trim()
        .to_string();
    let who_id = card.operator_open_id.clone();
    let payload = crate::runtime::inbox_wal::SentinelInbox {
        chat_id: card.chat_id.clone(),
        chat_type: chat_type.to_string(),
        prompt: summon_core::handoff::instruction(
            if summon_core::handoff::agent_compacts_in_place(agent) {
                summon_core::handoff::HandoffAfter::CompactInPlace
            } else {
                summon_core::handoff::HandoffAfter::NewSession
            },
            &note,
        ),
        fire_id,
        who: fc
            .resolve_name_cached(&who_id)
            .unwrap_or_else(|| super::dispatcher::fallback_display_name(&who_id, false)),
        who_union_id: fc.resolve_union_id(&who_id),
        who_id,
        handoff_note: Some(note.clone()),
    };
    match click.inbox.append_sentinel(
        &agent.name,
        click.delivery_scope,
        &payload,
        now_unix_ms() as i64,
    ) {
        Ok(crate::runtime::inbox_wal::ScheduledAppend::Inserted) => {
            close(&ContextWarnOutcome::HandoffStarted { note });
            if click.self_tx.send(payload.fire()).is_err() {
                // 停机中:WAL 已落盘,启动恢复会自动重投。
                log(&summon_i18n::format!(
                    "raee6f4690f",
                    agent.name,
                    card.chat_id
                ));
            } else {
                log(&summon_i18n::format!(
                    "r2dcd7fbb43",
                    agent.name,
                    card.chat_id
                ));
            }
        }
        // 上面查过键之后才落盘,命中只可能是并发的同一次提交(载荷已校验相同),由它负责投递。
        Ok(crate::runtime::inbox_wal::ScheduledAppend::Pending) => {
            close(&ContextWarnOutcome::HandoffStarted { note })
        }
        Ok(crate::runtime::inbox_wal::ScheduledAppend::Consumed) => {
            close(&ContextWarnOutcome::HandoffDone)
        }
        Err(error) => {
            failed(error);
            decline(
                click.claimed,
                ::summon_core::i18n::label(HANDOFF_NOT_WRITTEN),
            );
        }
    }
}

const HANDOFF_NOT_WRITTEN: &str = "r17cd89b4ee";

/// 把上下文提醒卡原地定格成 `outcome`。PATCH 失败(卡片过期、网络)就把同一句结果发成普通消息,
/// 用户总能看到这次点击的结果。
pub(super) fn patch_context_warn(
    fc: &FeishuClient,
    agent: &Agent,
    chat_type: &str,
    chat_id: &str,
    message_id: &str,
    warn: &summon_core::feishu::ContextWarnCard,
    outcome: &summon_core::feishu::ContextWarnOutcome,
) {
    let card = summon_core::feishu::card_context_warning_closed(warn, outcome);
    let Err(error) = update_card_archived(
        fc,
        &agent.state_dir(),
        chat_type,
        chat_id,
        message_id,
        &card,
    ) else {
        return;
    };
    log(&summon_i18n::format!("re2fcd75a60", agent.name));
    let _ = send_markdown_archived(
        fc,
        &agent.state_dir(),
        chat_type,
        chat_id,
        &outcome.text(warn),
    );
}

/// 配置变更(model/work_dir)后的快重置:不归档、清 `.session`、kill,让下次冷启用新配置。
/// chat 不在线时配置已写盘,下次冷启自然生效。
///
/// 在线路投 `Control::Reset` 给 chat 线程,常驻 RPC 会话由那条臂一并收掉(见 `chat_thread`);
/// 不在线路则**直接摘池**:chat 线程退了不代表池里那条会话没了(键是 (agent, tag),与线程无关),
/// 留着它下次 `checkout` 就会捞回同一个进程,新配置照样不生效。
pub(super) fn reset_for_config_change(
    chats: &HashMap<String, ChatHandle>,
    agent: &Agent,
    chat_type: &str,
    chat_id: &str,
) {
    match chats.get(chat_id) {
        Some(h) => {
            let _ =
                // 配置变更快重置:不是「退出」动作,面板由 set_model/set_workdir 各自回执,不 PATCH 成退出态。
            h.tx.send(ThreadMsg::Control {
                ctrl: Control::Reset,
                idle_retired: false,
                card_message_id: None,
                models: Vec::new(),
                // 配置变更快重置:非卡片收尾(card_message_id=None 已让 mark_session_card_closed 早退),
                // origin 走 Panel 占位即可(不会触发任何空闲卡渲染)。
                origin: CardOrigin::Panel,
                // 改模型/工作目录顺带的重启,不是「退出」,不该报退出回执。
                notify_exit: false,
                source_task: None,
                source_started: false,
            });
        }
        None => {
            let evicted =
                crate::capture::rpc_pool::evict_shared(&agent.name, &chat_tag(chat_type, chat_id));
            log(&summon_i18n::format!("r60457b2612", agent.name));
        }
    }
}

/// (纯件,可单测)切 model/effort 写盘后的执行计划:按 driver 热路径 spec × chat 是否在线,
/// 决定「投线程就地生效 / 直接回执 / 快重置」。`set_note` 形如「模型已设为」(含自身空格风格),
/// `value` 是新值。目标:**能不丢上下文就不丢**,只有 Restart(无热切通道)才重置。
#[derive(Debug, PartialEq, Eq)]
pub(super) enum SwitchPlan {
    /// 投 `ThreadMsg::SwitchApply` 给在线 chat 线程(slash=Some 先注入 tmux);回执由线程
    /// 在切换真正完成后发。
    Apply {
        slash: Option<String>,
        receipt: CommandAck,
    },
    /// 无在线线程,写盘已够(下条消息 spawn 新线程即用新值):直接回执。
    ReceiptOnly(CommandAck),
    /// 无热切换通道:退回快重置 + 回执(唯一丢上下文的路,fail-safe)。
    ResetThenReceipt(CommandAck),
}

impl SwitchPlan {
    /// 回执后面再接一句(改默认时说其他群什么时候换上)。这句话用户从指令里推不出,回执随之
    /// 要照常发文字。
    pub(super) fn append_note(&mut self, note: &str) {
        let receipt = self.receipt_mut();
        *receipt = CommandAck::Note(format!("{}{note}", receipt.text()));
    }

    /// 回执里有用户从指令推不出的值(回到的默认值、片段解析出的完整 id),照常发文字。
    pub(super) fn keep_text(&mut self) {
        let receipt = self.receipt_mut();
        *receipt = CommandAck::Note(receipt.text().to_string());
    }

    fn receipt_mut(&mut self) -> &mut CommandAck {
        match self {
            SwitchPlan::Apply { receipt, .. }
            | SwitchPlan::ReceiptOnly(receipt)
            | SwitchPlan::ResetThenReceipt(receipt) => receipt,
        }
    }
}

pub(super) fn plan_config_switch(
    spec: ModelSwitchSpec,
    online: bool,
    set_note: &str,
    value: &str,
) -> SwitchPlan {
    match spec {
        ModelSwitchSpec::TmuxSlash { prefix } if online => SwitchPlan::Apply {
            slash: Some(format!("{prefix} {value}")),
            receipt: CommandAck::Done(summon_i18n::format!("re7b2ef46bc")),
        },
        ModelSwitchSpec::NextTurn if online => SwitchPlan::Apply {
            slash: None,
            receipt: CommandAck::Done(summon_i18n::format!("rb321468639")),
        },
        ModelSwitchSpec::TmuxSlash { .. } | ModelSwitchSpec::NextTurn => {
            SwitchPlan::ReceiptOnly(CommandAck::Done(summon_i18n::format!("rd01eb4fa36")))
        }
        // 上下文没了,用户得知道。
        ModelSwitchSpec::Restart => {
            SwitchPlan::ResetThenReceipt(CommandAck::Note(summon_i18n::format!("r5052057502")))
        }
    }
}

/// 执行 [`SwitchPlan`](IO 收口):Apply→投线程(线程恰好已退则据实回退离线文案);
/// ReceiptOnly→直发;ResetThenReceipt→快重置 + 直发。
pub(super) fn dispatch_config_switch(
    chats: &mut HashMap<String, ChatHandle>,
    agent: &Agent,
    fc: &FeishuClient,
    chat_id: &str,
    chat_type: &str,
    plan: SwitchPlan,
    anchor: Option<&str>,
) {
    let reply = |ack: &CommandAck| {
        let _ = send_command_ack(fc, agent, chat_type, chat_id, anchor, ack);
    };
    match plan {
        SwitchPlan::Apply { slash, receipt } => {
            let sent = chats
                .get(chat_id)
                .map(|h| {
                    h.tx.send(ThreadMsg::SwitchApply {
                        slash,
                        receipt: receipt.clone(),
                        anchor: anchor.map(str::to_string),
                        confirm_model: None,
                    })
                    .is_ok()
                })
                .unwrap_or(false);
            if !sent {
                reply(&receipt.with_text(summon_i18n::text!("rd78fab8473").to_string()));
            }
        }
        SwitchPlan::ReceiptOnly(ack) => reply(&ack),
        SwitchPlan::ResetThenReceipt(ack) => {
            reset_for_config_change(chats, agent, chat_type, chat_id);
            reply(&ack);
        }
    }
}

/// 面板「切模型」:operate-gate → 写 agent.yaml 顶层 `model` → 按 `model_switch_spec` 热路径生效
/// (claude 注入 `/model` 就地切 / 无头下一轮自带 / 兜底快重置)→ 回执。
/// 改 model/effort 成功后,用最新配置**重渲会话面板卡并原地刷新**(update_card)。不刷则卡片停在
/// 点击前状态:被选中的 model/effort 按钮不会更新高亮(观感是"按钮点了没变化/不变灰")——高亮是
/// `session_panel_elements` 按当前值现算的,卡片不重渲就不更新。刷新目标就是被点的那张面板卡自身
/// (`card.message_id`);`model`/`effort` 传**刚写入的新值**,另一维用 agent 当前值。
/// 刷卡失败 log 忽略,绝不中断已生效的配置切换。
/// 刷新面板卡:重渲染摘要(生效值+来源)+ 按钮高亮。`model_override`/`effort_override` 是**刚改的**
/// 那个字段的新值(内存 `agent` 尚未 reload,直接 resolve 会读到旧值)——只覆盖被改的一格,其余走
/// [`panel_summary_inputs`] 正常解析链(cursor 的模型改从 rpc_model/会话实时解析,不再显示假「未指定」)。
///
/// **subagent 区块**:这里渲的骨架传空(dispatcher 线程摸不到 chat 线程的共享状态),随后投
/// `RenderSubagentPanelSnapshot` 让 chat 线程补渲**重渲那一刻**的快照。
/// 补渲之后 chat 线程会把这张卡登记为「群里最新那张」,交给刷新 tick 接管
/// (owner 2026-08-14:判据是**哪张卡最新**,不是卡的类型)。少了这一投,改一次 model 就会把
/// 卡上的 subagent 区块抹平到下一个 tick 才补回来。
fn refresh_config_panel(
    chats: &HashMap<String, ChatHandle>,
    agent: &Agent,
    fc: &FeishuClient,
    chat_type: &str,
    card: &CardActionMsg,
) {
    // dispatcher 手里的 agent 快照可能还没跟上刚写的 agent.yaml(改默认):按盘上现状重渲。
    let fresh = agent.reread().unwrap_or_else(|| agent.clone());
    let agent = &fresh;
    let chat_id = &card.chat_id;
    let tag = chat_tag(chat_type, chat_id);
    let msg = resolve_messaging(agent);
    let mut opts = summon_core::model_effort::options_for(&msg, agent.cli_kind());
    opts.models = model_candidates(agent, &tag, opts.models);
    let inputs = panel_summary_inputs(agent, &tag, &msg);
    // A(隐私):面板不再传工作目录路径(路径不进群);查看/切换走「私信候选目录」。
    let panel = summon_core::feishu::card_session_panel(
        &tag,
        agent.cli_kind(),
        inputs.channel,
        &inputs.model,
        &inputs.effort,
        &opts.models,
        &opts.efforts,
        &summon_core::subagent::SubagentDisplay::default(),
        session_panel_controls(
            agent,
            &tag,
            &super::chat_meta::chat_session_name(agent, chat_type, chat_id),
            &inputs.model,
        ),
    );
    if let Err(e) = update_card_archived(
        fc,
        &agent.state_dir(),
        chat_type,
        chat_id,
        &card.message_id,
        &panel,
    ) {
        log(&summon_i18n::format!("r27e126956a", agent.name));
        return; // 卡没重画成功,补渲快照没有意义(卡上还是旧内容,含旧 subagent 区块)
    }
    if let Some(handle) = chats.get(chat_id) {
        let _ = handle.tx.send(ThreadMsg::RenderSubagentPanelSnapshot {
            card_id: card.message_id.clone(),
        });
    }
}

/// 提问卡定格:替换成「问题 + 你的回答」的只读卡(schema 2.0,与原卡同 schema —— 飞书拒绝
/// 跨 schema 替换)。新卡题面来自 Core gate；按钮 value 的 `qs` 只兼容升级前已发出的旧卡。
fn finalize_ask_card(
    agent: &Agent,
    fc: &FeishuClient,
    chats: &HashMap<String, ChatHandle>,
    card: &CardActionMsg,
    skipped: bool,
    authoritative: Option<&crate::runtime::core::ChatAskGateContext>,
) {
    // 新卡只认 Core gate 保存的权威题面；`value.qs` 是客户端可改的 echo，仅给升级前已发出的
    // 旧卡做定格兼容，绝不参与答案校验。
    let questions: Vec<summon_core::core_port::AskQuestion> = authoritative
        .map(|context| context.questions.clone())
        .or_else(|| {
            card.value
                .get("qs")
                .and_then(|v| serde_json::from_value(v.clone()).ok())
        })
        .unwrap_or_default();
    if questions.is_empty() {
        let text = if skipped {
            summon_i18n::text!("re5e846da42")
        } else {
            summon_i18n::text!("rd1722e01f0")
        };
        finalize_card(agent, fc, chats, card, text);
        return;
    }
    let answers: Vec<Option<String>> = if skipped {
        vec![None; questions.len()]
    } else {
        serde_json::from_str::<Vec<Option<String>>>(&collect_chat_ask_answers(
            &card.value,
            questions.len(),
        ))
        .unwrap_or_default()
    };
    // gate_id 留空:终态卡的控件全被置灰、按钮不再回调,没有闸门可指。
    let content = summon_core::feishu::card_chat_ask_frozen(
        &summon_core::feishu::ChatAskCard {
            gate_id: "",
            requester_open_id: authoritative
                .map(|context| context.origin.requester_open_id.as_str())
                .unwrap_or(&card.operator_open_id),
            questions: &questions,
        },
        &answers,
        skipped,
    );
    let chat_type = card_chat_type(chats, &card.chat_id);
    if let Err(e) = update_card_archived(
        fc,
        &agent.state_dir(),
        &chat_type,
        &card.chat_id,
        &card.message_id,
        &content,
    ) {
        log(&summon_i18n::format!("r064e115ea4", agent.name));
        let _ = send_markdown_archived(
            fc,
            &agent.state_dir(),
            &chat_type,
            &card.chat_id,
            summon_i18n::text!("r966104505e"),
        );
    }
}

/// 授权卡定格:保留请求原文,按钮置灰、标出选了哪个,底部写明谁点的。闸门上下文缺失(理论上
/// 不会:resolve 成功说明点击时闸门还在)时退回一行字的终态。
fn finalize_permission_card(
    agent: &Agent,
    fc: &FeishuClient,
    chats: &HashMap<String, ChatHandle>,
    card: &CardActionMsg,
    context: Option<&crate::runtime::core::ClaudePermissionGateContext>,
    action: &str,
) {
    use summon_core::feishu::ClaudePermissionEnd;
    let operator_open_id = card.operator_open_id.as_str();
    let (end, text) = match action {
        "gate_perm_allow" => (
            ClaudePermissionEnd::Allowed { operator_open_id },
            summon_i18n::text!("r78bb34a65d"),
        ),
        "gate_perm_terminal" => (
            ClaudePermissionEnd::Terminal { operator_open_id },
            summon_i18n::text!("r24d416310c"),
        ),
        _ => (
            ClaudePermissionEnd::Denied { operator_open_id },
            summon_i18n::text!("r96bf1b3f05"),
        ),
    };
    let Some(context) = context else {
        finalize_card(agent, fc, chats, card, text);
        return;
    };
    let content = summon_core::feishu::card_claude_permission_settled(
        &summon_core::feishu::ClaudePermissionCard {
            gate_id: "",
            requester_open_id: &context.origin.requester_open_id,
            request: &context.request,
        },
        end,
    );
    let chat_type = card_chat_type(chats, &card.chat_id);
    if let Err(e) = update_card_archived(
        fc,
        &agent.state_dir(),
        &chat_type,
        &card.chat_id,
        &card.message_id,
        &content,
    ) {
        log(&summon_i18n::format!("rb9bb39c1f9", agent.name));
        let _ = send_markdown_archived(fc, &agent.state_dir(), &chat_type, &card.chat_id, text);
    }
}

/// 从提问卡的 `form_value` 收答案:控件名是 `q0`/`q1`/…,自定义输入名是
/// `q0_other`/`q1_other`/…；自定义答案非空时优先。最终仍按题序回成 JSON 数组。
/// 多选控件回的是数组,按 Claude AskUserQuestion 的约定用 `", "` 连成一个答案。
/// **未答的位置回 `null`**(下拉没选/输入留空),让 agent 知道「这题他没表态」而不是收到空串。
/// 长度只认 Core gate 的权威题数；callback 是不可信输入，绝不能让伪造的 `q999999999`
/// 驱动大数组分配。
fn collect_chat_ask_answers(value: &serde_json::Value, question_count: usize) -> String {
    let form = value.get("form_value").and_then(|f| f.as_object());
    let answers: Vec<serde_json::Value> = (0..question_count)
        .map(|i| {
            let nonempty = |name: &str| {
                let text = match form.and_then(|values| values.get(name))? {
                    serde_json::Value::String(text) => text.trim().to_string(),
                    serde_json::Value::Array(items) => items
                        .iter()
                        .filter_map(|item| item.as_str().map(str::trim))
                        .filter(|item| !item.is_empty())
                        .collect::<Vec<_>>()
                        .join(", "),
                    _ => return None,
                };
                (!text.is_empty()).then_some(text)
            };
            nonempty(&format!("q{i}_other"))
                .or_else(|| nonempty(&format!("q{i}")))
                .map(serde_json::Value::String)
                .unwrap_or(serde_json::Value::Null)
        })
        .collect();
    serde_json::Value::Array(answers).to_string()
}

#[cfg(test)]
mod chat_ask_tests {
    fn is_close_button(value: &serde_json::Value) -> bool {
        value["tag"] == "button"
            && value["behaviors"]
                .as_array()
                .is_some_and(|items| items.len() == 1)
            && value["behaviors"][0]["value"]["action"] == "card_close"
    }

    use super::collect_chat_ask_answers;
    use serde_json::json;

    #[test]
    fn unanswered_questions_become_null_not_empty_string() {
        // 用户只答了第 2 题:前后都必须是 null,让 agent 知道「他没表态」而不是「他答了空」。
        let v = json!({"form_value": {"q0": "", "q1": "  develop  ", "q2": "   "}});
        assert_eq!(collect_chat_ask_answers(&v, 3), r#"[null,"develop",null]"#);
    }

    #[test]
    fn answer_array_spans_authoritative_question_count() {
        // 只有 q2 有值时,数组仍按权威题数保留 3 项 —— 否则 CLI 按下标配对会错位。
        let v = json!({"form_value": {"q2": "x"}});
        assert_eq!(collect_chat_ask_answers(&v, 3), r#"[null,null,"x"]"#);
    }

    #[test]
    fn nonempty_other_answer_overrides_select_and_keeps_question_positions() {
        let v = json!({"form_value": {
            "q0": "灰度",
            "q0_other": "  仅内部员工  ",
            "q1": "全量",
            "q1_other": "   ",
            "q2_other": "自定义第三项"
        }});
        assert_eq!(
            collect_chat_ask_answers(&v, 3),
            r#"["仅内部员工","全量","自定义第三项"]"#
        );
    }

    #[test]
    fn multi_select_answers_join_with_comma() {
        let v = json!({"form_value": {"q0": ["A", " B ", ""], "q1": []}});
        assert_eq!(collect_chat_ask_answers(&v, 2), r#"["A, B",null]"#);
    }

    #[test]
    fn forged_large_question_index_cannot_control_allocation_or_answer_count() {
        let value = json!({"form_value": {"q0": "A", "q999999999": "伪造"}});
        assert_eq!(collect_chat_ask_answers(&value, 2), r#"["A",null]"#);
        assert_eq!(collect_chat_ask_answers(&value, 0), "[]");
    }

    /// 回归钉:提问卡的 action 名必须以 `gate_` 开头。
    /// 真机翻车过 —— 起名 `chat_ask_submit` 时 `CardAction::from_action` 只认 `task_`/`gate_`
    /// 两个前缀,它落进 `Unknown`,dispatcher 压根不把回调路由到闸门 handler:卡片发出去了、
    /// 用户点了、回调也到了 daemon,但闸门永不 resolve,agent 一直阻塞、卡片也不定格
    /// (定格是 resolve 成功后 `finalize_card` 做的,同一条路)。
    #[test]
    fn ask_card_actions_must_be_routed_as_gate_family() {
        use summon_core::feishu::{CardAction, ChatAskCard};
        for action in ["gate_ask_submit", "gate_ask_skip"] {
            assert!(
                matches!(CardAction::from_action(action), CardAction::GateFamily),
                "{action} 必须被识别成 GateFamily,否则回调进不了闸门 handler"
            );
        }
        // 卡片里真正发出去的 action 名也必须是这两个 —— 渲染与路由不能各说各话。
        let card = summon_core::feishu::card_chat_ask(&ChatAskCard {
            gate_id: "g1",
            requester_open_id: "ou_x",
            questions: &[summon_core::core_port::AskQuestion {
                question: "选哪个?".into(),
                options: vec!["A".into(), "B".into()],
                ..Default::default()
            }],
        });
        assert!(card.contains("gate_ask_submit"), "提交按钮 action 名不对");
        assert!(card.contains("gate_ask_skip"), "跳过按钮 action 名不对");
        assert!(!card.contains("chat_ask_"), "残留旧的不可路由 action 名");
    }

    /// 定格卡必须**保留原貌**、只置灰 —— 真机反馈:重渲染成纯文本列表等于换了张卡,
    /// 用户既认不出是原来那张,也看不出控件当时长什么样(与 card_session_closed 同一套做法)。
    #[test]
    fn frozen_card_keeps_shape_and_disables_controls() {
        use summon_core::core_port::AskQuestion;
        use summon_core::feishu::ChatAskCard;
        let questions = vec![
            AskQuestion {
                question: "从哪里触发导出?".into(),
                options: vec!["CLI".into(), "Desktop".into()],
                ..Default::default()
            },
            AskQuestion {
                question: "导出范围?".into(),
                options: vec![],
                ..Default::default()
            },
        ];
        let spec = ChatAskCard {
            gate_id: "",
            requester_open_id: "ou_x",
            questions: &questions,
        };
        let frozen =
            summon_core::feishu::card_chat_ask_frozen(&spec, &[Some("CLI".into()), None], false);
        let v: serde_json::Value = serde_json::from_str(&frozen).unwrap();
        // 原貌还在:题目、下拉、输入框、按钮一个不少。
        assert!(frozen.contains("从哪里触发导出?"));
        assert!(frozen.contains("select_static"), "下拉控件应保留(只是置灰)");
        assert!(frozen.contains("\"tag\":\"input\"") || frozen.contains("\"input\""));
        // 提问控件置灰，关闭入口保留。
        fn all_interactive_disabled(v: &serde_json::Value) -> bool {
            match v {
                serde_json::Value::Object(m) => {
                    let tag = m.get("tag").and_then(|t| t.as_str()).unwrap_or("");
                    let interactive = matches!(
                        tag,
                        "button" | "input" | "select_static" | "multi_select_static"
                    );
                    if interactive
                        && !is_close_button(v)
                        && m.get("disabled").and_then(|d| d.as_bool()) != Some(true)
                    {
                        return false;
                    }
                    m.values().all(all_interactive_disabled)
                }
                serde_json::Value::Array(a) => a.iter().all(all_interactive_disabled),
                _ => true,
            }
        }
        assert!(
            all_interactive_disabled(&v),
            "提问控件必须 disabled:\n{frozen}"
        );
        // 选了什么要看得出来;未答的题显式标出(与「答了空」不是一回事)。
        assert!(frozen.contains("→ **CLI**"), "应显示用户的选择");
        assert!(frozen.contains("(未答)"), "未答的题必须标出来");
        assert!(frozen.contains("已提交"));

        // 全部跳过:每题都标未答,措辞不同。
        let skipped = summon_core::feishu::card_chat_ask_frozen(&spec, &[], true);
        assert!(skipped.contains("已全部跳过"));
        assert_eq!(skipped.matches("(未答)").count(), 2);
    }

    #[test]
    fn invalidated_card_is_disabled_without_pretending_user_skipped() {
        use summon_core::core_port::AskQuestion;
        use summon_core::feishu::ChatAskCard;

        let questions = [AskQuestion {
            question: "继续吗?".into(),
            options: vec!["继续".into(), "停止".into()],
            ..Default::default()
        }];
        let spec = ChatAskCard {
            gate_id: "",
            requester_open_id: "ou_asker",
            questions: &questions,
        };
        let timeout = summon_core::feishu::card_chat_ask_invalidated(
            &spec,
            summon_core::feishu::AskCardEnd::TimedOut,
        );
        assert!(timeout.contains("等待窗口已结束"));
        assert!(!timeout.contains("已全部跳过"));
        let ended = summon_core::feishu::card_chat_ask_invalidated(
            &spec,
            summon_core::feishu::AskCardEnd::TurnEnded,
        );
        assert!(ended.contains("本轮已结束"));
        assert!(!ended.contains("已全部跳过"));

        fn interactive_controls_are_disabled(value: &serde_json::Value) -> bool {
            match value {
                serde_json::Value::Object(map) => {
                    let interactive = map
                        .get("tag")
                        .and_then(serde_json::Value::as_str)
                        .is_some_and(|tag| matches!(tag, "button" | "input" | "select_static"));
                    (!interactive
                        || is_close_button(value)
                        || map.get("disabled").and_then(serde_json::Value::as_bool) == Some(true))
                        && map.values().all(interactive_controls_are_disabled)
                }
                serde_json::Value::Array(values) => {
                    values.iter().all(interactive_controls_are_disabled)
                }
                _ => true,
            }
        }
        assert!(interactive_controls_are_disabled(
            &serde_json::from_str(&ended).unwrap()
        ));
    }

    /// 题干必须随按钮 value 回传,否则定格时拿不到题面(回调里只有答案)。
    #[test]
    fn ask_card_echoes_question_text_in_button_value() {
        use summon_core::feishu::ChatAskCard;
        let card = summon_core::feishu::card_chat_ask(&ChatAskCard {
            gate_id: "g1",
            requester_open_id: "ou_x",
            questions: &[summon_core::core_port::AskQuestion {
                question: "从哪里触发导出?".into(),
                options: vec!["CLI".into()],
                ..Default::default()
            }],
        });
        assert!(card.contains("\"qs\""), "按钮 value 必须带题干:\n{card}");
        assert!(card.contains("从哪里触发导出?"));
    }

    #[test]
    fn rich_ask_card_renders_safe_metadata_and_echoes_it_for_freezing() {
        use summon_core::core_port::AskQuestion;
        use summon_core::feishu::ChatAskCard;

        let question = AskQuestion {
            question: "选择 <at id=all></at> 的发布方式".into(),
            header: Some("发布 <header>".into()),
            options: vec!["<灰度>".into(), "全量".into()],
            descriptions: vec!["先覆盖 <5%> 用户".into(), "覆盖全部 & 立即生效".into()],
            allow_other: true,
            multi_select: false,
        };
        let card = summon_core::feishu::card_chat_ask(&ChatAskCard {
            gate_id: "g1",
            requester_open_id: "ou_x",
            questions: &[question],
        });
        let parsed: serde_json::Value = serde_json::from_str(&card).unwrap();

        fn markdown_text(value: &serde_json::Value, out: &mut String) {
            match value {
                serde_json::Value::Object(map) => {
                    if map.get("tag").and_then(|v| v.as_str()) == Some("markdown") {
                        if let Some(content) = map.get("content").and_then(|v| v.as_str()) {
                            out.push_str(content);
                            out.push('\n');
                        }
                    }
                    map.values().for_each(|value| markdown_text(value, out));
                }
                serde_json::Value::Array(values) => {
                    values.iter().for_each(|value| markdown_text(value, out));
                }
                _ => {}
            }
        }
        let mut rendered_markdown = String::new();
        markdown_text(&parsed, &mut rendered_markdown);

        assert!(
            rendered_markdown.contains("发布 <\u{200B}header>"),
            "header 必须展示并拆散标签形态"
        );
        assert!(
            rendered_markdown.contains("选择 <\u{200B}at id=all><\u{200B}/at> 的发布方式"),
            "题干不能把模型文本解释成飞书标签"
        );
        // `<灰度>`/`<5%>` 非字母开头,不是标签形态,原样保留;`&` 不再转成飞书不解码的实体。
        assert!(
            rendered_markdown.contains("• <灰度> — 先覆盖 <5%> 用户")
                && rendered_markdown.contains("覆盖全部 & 立即生效"),
            "选项说明必须原样可读"
        );
        assert!(!rendered_markdown.contains("&amp;") && !rendered_markdown.contains("&lt;"));
        assert!(!rendered_markdown.contains("<at id=all>"));

        let form = parsed["body"]["elements"]
            .as_array()
            .unwrap()
            .iter()
            .find(|el| el["tag"] == "form")
            .unwrap();
        let controls = form["elements"].as_array().unwrap();
        assert!(controls
            .iter()
            .any(|el| { el["tag"] == "select_static" && el["name"].as_str() == Some("q0") }));
        assert!(controls
            .iter()
            .any(|el| el["tag"] == "input" && el["name"].as_str() == Some("q0_other")));

        let submit = controls
            .iter()
            .find(|el| el["name"] == "gate_ask_submit_btn")
            .unwrap();
        let echoed = &submit["behaviors"][0]["value"]["qs"][0];
        assert_eq!(echoed["header"], "发布 <header>");
        assert_eq!(echoed["options"][0], "<灰度>");
        assert_eq!(echoed["descriptions"][0], "先覆盖 <5%> 用户");
        assert_eq!(echoed["allow_other"], true);

        let frozen = summon_core::feishu::card_chat_ask_frozen(
            &ChatAskCard {
                gate_id: "",
                requester_open_id: "ou_x",
                questions: &[serde_json::from_value(echoed.clone()).unwrap()],
            },
            &[Some("<at id=all></at> & 完成".into())],
            false,
        );
        let frozen: serde_json::Value = serde_json::from_str(&frozen).unwrap();
        let mut frozen_markdown = String::new();
        markdown_text(&frozen, &mut frozen_markdown);
        assert!(frozen_markdown.contains("发布 <\u{200B}header>"));
        assert!(frozen_markdown.contains("先覆盖 <5%> 用户"));
        assert!(frozen_markdown.contains("→ **<\u{200B}at id=all><\u{200B}/at> & 完成**"));
        assert!(!frozen_markdown.contains("<at id=all>"));
    }

    #[test]
    fn callback_question_metadata_is_bounded_by_char_count() {
        use summon_core::core_port::AskQuestion;
        use summon_core::feishu::ChatAskCard;

        let card = summon_core::feishu::card_chat_ask(&ChatAskCard {
            gate_id: "g1",
            requester_open_id: "ou_x",
            questions: &[AskQuestion {
                question: "问".repeat(400),
                header: Some("头".repeat(100)),
                options: vec!["选".repeat(200)],
                descriptions: vec!["释".repeat(300)],
                allow_other: true,
                multi_select: false,
            }],
        });
        let parsed: serde_json::Value = serde_json::from_str(&card).unwrap();
        let echoed = &parsed["body"]["elements"][1]["elements"]
            .as_array()
            .unwrap()
            .iter()
            .find(|el| el["name"] == "gate_ask_submit_btn")
            .unwrap()["behaviors"][0]["value"]["qs"][0];

        assert_eq!(echoed["q"].as_str().unwrap().chars().count(), 241);
        assert_eq!(echoed["header"].as_str().unwrap().chars().count(), 49);
        assert_eq!(echoed["options"][0].as_str().unwrap().chars().count(), 81);
        assert_eq!(
            echoed["descriptions"][0].as_str().unwrap().chars().count(),
            161
        );
        for value in [
            &echoed["q"],
            &echoed["header"],
            &echoed["options"][0],
            &echoed["descriptions"][0],
        ] {
            assert!(
                value.as_str().unwrap().ends_with('…'),
                "被裁剪字段应明确显示省略号"
            );
        }
        let select = parsed["body"]["elements"][1]["elements"]
            .as_array()
            .unwrap()
            .iter()
            .find(|element| element["name"] == "q0")
            .unwrap();
        assert_eq!(
            select["options"][0]["value"]
                .as_str()
                .unwrap()
                .chars()
                .count(),
            200,
            "候选只截展示文案，协议回传值必须保持原 label"
        );
    }

    #[test]
    fn missing_form_value_preserves_unanswered_slots() {
        assert_eq!(collect_chat_ask_answers(&json!({}), 2), "[null,null]");
        assert_eq!(
            collect_chat_ask_answers(&json!({"form_value": {}}), 2),
            "[null,null]"
        );
    }
}

pub(super) fn handle_card_set_model(
    chats: &mut HashMap<String, ChatHandle>,
    agent: &Agent,
    fc: &FeishuClient,
    card: &CardActionMsg,
    default_layer: bool,
) {
    let chat_id = &card.chat_id;
    // 写本群覆盖要用真实的 chat tag:handle 不在内存(daemon 重启、会话已退)时回退盘上 `.meta`,
    // 不能当私聊算出 p<hash>,否则写进一个没有会话的目录、回执却说成功。
    let chat_type = resolve_chat_type(chats, agent, chat_id);
    // 鉴权在 dispatcher 集中查表完成:本群按钮按本群范围、设为默认按 agent 范围。
    let inherit = !default_layer
        && card
            .value
            .get("inherit")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
    let model = card
        .value
        .get("model")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .unwrap_or_default()
        .to_string();
    if model.is_empty() && !inherit {
        let _ = send_markdown_archived(
            fc,
            &agent.state_dir(),
            &chat_type,
            chat_id,
            summon_i18n::text!("r5f0503bf98"),
        );
        return;
    }
    let target = match (default_layer, inherit) {
        (true, _) => LayerTarget::Default,
        (false, true) => LayerTarget::ChatInherit,
        (false, false) => LayerTarget::Chat,
    };
    // 写进配置了才刷新面板卡，让新选中的 model 按钮当场高亮;还在等 claude 确认时不刷，
    // 免得 claude 不认时卡上高亮一个没切成的模型。
    if set_model_for_chat(
        chats, agent, fc, chat_id, &chat_type, &model, None, target, true,
    ) == ModelSet::Saved
    {
        refresh_config_panel(chats, agent, fc, &chat_type, card);
    }
}

/// 这个 agent 的模型是不是走 Cursor RPC 参数化 bundle。Codex 即便走 RPC 也仍用顶层
/// `model`；只有 cursor+ACP 的 base model 与参数进入 per-agent 目标 bundle，并由每个 ACP
/// 进程的独占配置根承载。
///
/// 2026-08-20 之后 cursor 恒 Rpc,`capture_mode()` 那一维实际恒真;判定保留是防御性的
/// (将来若再给 cursor 加第二种通道,这里不必回头改)。
pub(super) fn uses_rpc_model(agent: &Agent) -> bool {
    CliDriver::from_kind(agent.cli_kind()) == CliDriver::Cursor
        && agent.capture_mode() == summon_core::registry::CaptureMode::Rpc
}

/// `/model` 面板的候选模型:一律以 backend 自报的目录为准,配置桶只剩冷启兜底。
///
/// backend 报目录的方式不同,所以取数分两路 ——
/// 走 ACP 的 cursor 目录随会话下发(`configOptions` 的 `model` 轴),只对**这条会话**有效;
/// codex/traex 与走 CLI 的 cursor 的目录按账号作用域共享，由后台按需刷新
/// (后者跑 `cursor-agent models`)。
/// claude 没有列模型的接口,只能留在配置桶里。
///
/// cursor 的目录上百项且服务端顺序把各档位、新旧版本混在一起,两条通道都按
/// [`summon_core::model_order::order_cursor_models`] 重排(`auto`、各家族推荐项在前),
/// 面板只列得下开头一截时也能先看到常用的;其余 CLI 保持 backend 原序。
pub(crate) fn model_candidates(agent: &Agent, tag: &str, from_config: Vec<String>) -> Vec<String> {
    let candidates = if uses_rpc_model(agent) {
        prefer_dynamic_catalog(
            crate::capture::rpc_pool::available_models_shared(&agent.name, tag),
            from_config,
        )
    } else {
        prefer_dynamic_catalog(
            crate::codex::model_catalog::models_for_agent(agent),
            from_config,
        )
    };
    if CliDriver::from_kind(agent.cli_kind()) == CliDriver::Cursor {
        summon_core::model_order::order_cursor_models(&candidates)
    } else {
        candidates
    }
}

/// RPC 面的候选:动态目录优先,**空则回退配置桶**。
///
/// 动态目录是「这个 ACP 会话此刻真能选的」,比配置桶权威,所以非空时压过它。但它只有在会话
/// 已建立**且**服务端下发过目录时才有值——冷启前、懒重连未触发、服务端没给,拿到的都是空。
/// 那时候若返回空,面板一个模型按钮都不渲染、`/model <id>` 也无从对照,用户既看不到能选什么、
/// 也没法切——比给几个可能过期的候选糟得多:回退的代价上限只是「点了某个按钮起不来」。
fn prefer_dynamic_catalog(dynamic: Vec<String>, from_config: Vec<String>) -> Vec<String> {
    if dynamic.is_empty() {
        from_config
    } else {
        dynamic
    }
}

/// 面板/角标「当前跑的模型」值 —— 与候选同一个命名空间,否则高亮永远对不上。
/// cursor-RPC 优先读**会话实时**(`current_model_shared`)，取不到回退 per-agent bundle
/// （bundle 尚未建立时读取 legacy `rpc_model` seed）；其余通道用顶层 `model`。**不含默认回退**——
/// 角标要如实反映「此刻在跑什么」,填一个没真在跑的默认反而误导(默认回退只属于面板摘要,见
/// [`resolve_panel_model`])。
pub(super) fn panel_model(agent: &Agent, tag: &str) -> Option<String> {
    if uses_rpc_model(agent) {
        crate::capture::rpc_pool::current_model_shared(&agent.name, tag).or_else(|| {
            let fresh = fresh_cursor_agent(agent);
            crate::capture::cursor_config::load_target(&fresh, tag)
                .ok()
                .and_then(|target| target.model().map(str::to_string))
        })
    } else {
        crate::runtime::chat_model::effective(agent, Some(tag)).model
    }
}

/// 角标「本轮在跑的思考档位」值。链与 [`effort_for`] 一致(本会话覆盖 > agent.yaml),外面再套一层
/// [`resolve_effort`] 的合法性判定 —— **没有独立 effort 轴的 CLI(cursor/other)恒 None**:
/// 它们的档位烤在模型名里,把 agent.yaml 里那个不会被下发的值渲上角标就是假信息。
///
/// 与 [`panel_model`] 同一条原则:**不含默认回退**。没配 effort 时 CLI 用自己的默认档,
/// summon 不知道那是什么,缺项好过猜一个。claude 另有 statusLine 实时值,收尾时覆盖这里的配置值。
///
/// [`resolve_effort`]: summon_core::model_effort::resolve_effort
pub(super) fn badge_effort(agent: &Agent, tag: &str) -> Option<String> {
    summon_core::model_effort::resolve_effort(agent.cli_kind(), effort_for(agent, tag).as_deref())
        .value
}

/// 面板摘要区的「生效值 + 来源」原料(owned,调用方按需 collapse/借用)。
/// **不含 work_dir**:A(隐私)后面板不再展示工作目录路径(路径不进群),故摘要原料里也不带它——
/// 路径只经私信选择器(`pick_workdir`)现算现发。
pub(super) struct PanelSummaryInputs {
    pub channel: &'static str,
    pub model: summon_core::model_effort::ResolvedModel,
    pub effort: summon_core::model_effort::ResolvedEffort,
}

/// 解析面板摘要区的**生效模型 + 来源**(带 summon 默认桶兜底)。链:
/// 会话实时(cursor-RPC)→ per-agent bundle / agent 配置→默认桶→CLI 自决。
/// 与 [`panel_model`] 的区别:这里多一层默认兜底并标来源,给摘要展示;`panel_model` 不兜底,给角标。
pub(super) fn resolve_panel_model(
    agent: &Agent,
    tag: &str,
    msg: &summon_core::config::config::Messaging,
) -> summon_core::model_effort::ResolvedModel {
    use summon_core::model_effort::{ModelSource, ResolvedModel};
    let rpc = uses_rpc_model(agent);
    let chat_model = if rpc {
        let fresh = fresh_cursor_agent(agent);
        crate::capture::cursor_config::load_layer(
            &fresh,
            crate::capture::cursor_config::CursorConfigLayer::Chat(tag),
        )
        .ok()
        .and_then(|view| view.selected_model().map(str::to_string))
    } else {
        crate::runtime::chat_model::chat_model(agent, tag)
    };
    if let Some(value) = chat_model {
        return ResolvedModel {
            value: Some(value),
            source: ModelSource::ChatOverride,
        };
    }
    let live = if rpc {
        crate::capture::rpc_pool::current_model_shared(&agent.name, tag)
    } else {
        None
    };
    match live {
        Some(value) => ResolvedModel {
            value: Some(value),
            source: ModelSource::SessionLive,
        },
        None => resolve_default_model(agent, msg),
    }
}

/// bot 默认模型 + 来源:per-agent bundle / agent 配置 → 默认桶 → CLI 自决。面板「默认 Y」与没有
/// 本群覆盖时的摘要共用。
pub(super) fn resolve_default_model(
    agent: &Agent,
    msg: &summon_core::config::config::Messaging,
) -> summon_core::model_effort::ResolvedModel {
    let rpc = uses_rpc_model(agent);
    let configured = rpc.then(|| {
        let fresh = fresh_cursor_agent(agent);
        crate::capture::cursor_config::load(&fresh)
            .ok()
            .and_then(|view| view.selected_model().map(str::to_string))
    });
    let default = crate::runtime::chat_model::effective(agent, None);
    let agent_model = if rpc {
        configured.as_ref().and_then(|model| model.as_deref())
    } else {
        default.model()
    };
    // daemon 里只读缓存,不起进程探测:卡片渲染跑在 chat 线程上,起一个 app-server 问目录
    // 会把面板挂在那里。缓存由会话建立时填,没填过就退回配置桶。
    let upstream =
        crate::codex::model_catalog::cached_for_agent(agent).and_then(|c| c.default_model);
    let fallback =
        summon_core::model_effort::default_model_for(msg, agent.cli_kind(), upstream.as_deref());
    summon_core::model_effort::resolve_model(None, agent_model, &fallback)
}

/// 聚一处算面板摘要三行(通道/模型/effort),供各面板渲染点共用,不在多处重复解析链。
/// **不再解析工作目录**:A(隐私)后面板不展示路径(路径不进群),工作目录一律经私信选择器。
pub(super) fn panel_summary_inputs(
    agent: &Agent,
    tag: &str,
    msg: &summon_core::config::config::Messaging,
) -> PanelSummaryInputs {
    let channel = summon_core::model_effort::channel_label(agent.cli_kind(), agent.capture_mode());
    let model = resolve_panel_model(agent, tag, msg);
    // cursor 参数化 ACP 没有独立 effort 轴,摘要指向 Cursor 参数里的思考档位轴;其余本群有覆盖
    // 就显示覆盖值(来源标「本群覆盖」),否则走 agent.yaml 解析。
    let effort = if uses_rpc_model(agent) {
        cursor_effort_summary(agent, tag)
    } else {
        match crate::runtime::chat_model::chat_effort(agent, tag) {
            Some(value) => summon_core::model_effort::ResolvedEffort {
                value: Some(value),
                source: summon_core::model_effort::EffortSource::ChatOverride,
            },
            None => summon_core::model_effort::resolve_effort(
                agent.cli_kind(),
                crate::runtime::chat_model::effective(agent, None).effort(),
            ),
        }
    };
    PanelSummaryInputs {
        channel,
        model,
        effort,
    }
}

/// Cursor 参数化 ACP 的思考档位摘要:reasoning / effort / thinking 里第一个出现的轴此刻的值。
fn cursor_effort_summary(agent: &Agent, tag: &str) -> summon_core::model_effort::ResolvedEffort {
    use summon_core::model_effort::{EffortSource, ResolvedEffort};
    let live = crate::capture::rpc_pool::cursor_config_options_shared(&agent.name, tag);
    if live.is_empty() {
        return ResolvedEffort {
            value: None,
            source: EffortSource::CursorParameterUnknown,
        };
    }
    let axis = ["reasoning", "effort", "thinking"]
        .iter()
        .find_map(|id| live.iter().find(|option| option.id == *id));
    ResolvedEffort {
        value: axis.and_then(|axis| axis.current_value.clone()),
        source: EffortSource::CursorParameter,
    }
}

fn codex_fast_support(model: Option<&str>) -> Option<bool> {
    let model = model?.trim();
    if model.is_empty() {
        return None;
    }
    let codex_home = std::env::var_os("CODEX_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| std::path::PathBuf::from(home).join(".codex"))
        })?;
    let catalog = std::fs::read_to_string(codex_home.join("models_cache.json")).ok()?;
    summon_core::driver::codex_model_supports_fast(&catalog, model)
}

/// 卡片回调发生在 dispatcher 持有的 agent 快照上；模型按钮写盘后，那个快照可能仍是旧模型。
/// Fast 落盘前必须重读 manifest 再判断能力，成功后也用**同一份**快照刷新卡片，否则会先按
/// 新模型放行、再把卡片画回旧模型。重读失败由 Fast 分支 fail-closed；Standard 仍可安全写入。
fn reread_for_codex_speed_action(agent: &Agent) -> Option<Agent> {
    agent.reread()
}

/// Fast 偏好在模型能力未知/不支持时必须显式降为 Standard，而不是省略：RPC 的 tier 是 sticky，
/// 省略会让此前已开启的 Fast 留在线程上。偏好文件仍保留 Fast，换回支持模型后可自动恢复。
fn effective_codex_service_tier(
    selected: Option<summon_core::driver::CodexServiceTier>,
    supports_fast: Option<bool>,
) -> Option<summon_core::driver::CodexServiceTier> {
    match selected {
        Some(summon_core::driver::CodexServiceTier::Fast) if supports_fast != Some(true) => {
            Some(summon_core::driver::CodexServiceTier::Standard)
        }
        other => other,
    }
}

/// 下一轮实际要发给 Codex 的档位。能力判断只看本轮真正会传入 TurnCtx 的显式模型。
pub(super) fn codex_service_tier_for_turn(
    agent: &Agent,
    tag: &str,
) -> Option<summon_core::driver::CodexServiceTier> {
    codex_service_tier_for_turn_with(agent, tag, codex_fast_support)
}

fn codex_service_tier_for_turn_with(
    agent: &Agent,
    tag: &str,
    fast_support: impl Fn(Option<&str>) -> Option<bool>,
) -> Option<summon_core::driver::CodexServiceTier> {
    let selected = codex_service_tier(agent, tag);
    // 与 TurnCtx 同一个模型:本群生效值。
    let model = crate::runtime::chat_model::effective(agent, Some(tag)).model;
    let support = match selected {
        Some(summon_core::driver::CodexServiceTier::Fast) => fast_support(model.as_deref()),
        _ => None,
    };
    let effective = effective_codex_service_tier(selected, support);
    if selected == Some(summon_core::driver::CodexServiceTier::Fast)
        && effective == Some(summon_core::driver::CodexServiceTier::Standard)
    {
        log(&summon_i18n::format!("rad595d2325", agent.name,));
    }
    effective
}

/// Claude statusLine 的百分比是上游权威口径；窗口存在时只反算一个近似 token 数用于展示。
fn context_budget_from_claude_snapshot(
    snapshot: &crate::setup::claude_quota::ClaudeQuotaSnapshot,
) -> summon_core::context_budget::ContextBudget {
    snapshot
        .context_used_percentage
        .map(|pct| {
            summon_core::context_budget::ContextBudget::from_reported_pct(
                pct.round().clamp(0.0, 100.0) as u8,
                snapshot.context_window_size,
            )
        })
        .unwrap_or_default()
}

pub(super) fn read_claude_session_snapshot(
    session_id: &str,
) -> Option<crate::setup::claude_quota::ClaudeQuotaSnapshot> {
    let paths = crate::setup::claude_quota::IntegrationPaths::for_current_user().ok()?;
    crate::setup::claude_quota::read_snapshot(&paths.snapshot, session_id).ok()
}

fn claude_session_context_budget_at(
    agent: &Agent,
    session: &str,
    snapshot_root: &std::path::Path,
) -> summon_core::context_budget::ContextBudget {
    crate::session::resume_store::read_id(agent, session, agent.cli_kind())
        .and_then(|id| crate::setup::claude_quota::read_snapshot(snapshot_root, &id).ok())
        .as_ref()
        .map(context_budget_from_claude_snapshot)
        .unwrap_or_default()
}

pub(super) fn codex_context_budget(
    agent_name: &str,
    tag: &str,
) -> summon_core::context_budget::ContextBudget {
    rpc_context_budget_from_snapshot(crate::capture::rpc_pool::current_ctx_budget_shared(
        agent_name, tag,
    ))
}

pub(super) fn cursor_context_budget(
    agent_name: &str,
    tag: &str,
) -> summon_core::context_budget::ContextBudget {
    rpc_context_budget_from_snapshot(crate::capture::rpc_pool::current_ctx_budget_shared(
        agent_name, tag,
    ))
}

fn rpc_context_budget_from_snapshot(
    snapshot: Option<(u64, u64)>,
) -> summon_core::context_budget::ContextBudget {
    snapshot
        .map(|(used, window)| {
            summon_core::context_budget::ContextBudget::from_tokens(used, Some(window))
        })
        .unwrap_or_default()
}

#[cfg(test)]
pub(crate) fn cursor_context_budget_from_pool(
    pool: &crate::capture::rpc_pool::RpcPool,
    agent_name: &str,
    tag: &str,
) -> summon_core::context_budget::ContextBudget {
    rpc_context_budget_from_snapshot(pool.current_ctx_budget(agent_name, tag))
}

pub(super) fn rpc_context_budget(
    agent_name: &str,
    tag: &str,
    driver: summon_core::driver::CliDriver,
) -> summon_core::context_budget::ContextBudget {
    match driver {
        summon_core::driver::CliDriver::Codex | summon_core::driver::CliDriver::Traex => {
            codex_context_budget(agent_name, tag)
        }
        summon_core::driver::CliDriver::Cursor => cursor_context_budget(agent_name, tag),
        _ => Default::default(),
    }
}

pub(super) fn current_session_context_budget(
    agent: &Agent,
    tag: &str,
    session: &str,
) -> summon_core::context_budget::ContextBudget {
    if is_cursor_cli_session(agent) {
        let Some(sid) =
            crate::session::resume_store::read_id(agent, session, &agent.resume_cli_kind())
        else {
            return Default::default();
        };
        return crate::capture::cursor_context::registered_budget(&agent.name, tag, &sid)
            .unwrap_or_else(|| {
                crate::capture::cursor_context::budget_for_env(
                    &super::resolve_extra_env(agent),
                    &sid,
                )
            });
    }
    let driver = summon_core::driver::CliDriver::from_kind(agent.cli_kind());
    match driver {
        summon_core::driver::CliDriver::Claude => {
            let Ok(paths) = crate::setup::claude_quota::IntegrationPaths::for_current_user() else {
                return Default::default();
            };
            claude_session_context_budget_at(agent, session, &paths.snapshot)
        }
        summon_core::driver::CliDriver::Cursor => {
            let live = rpc_context_budget(&agent.name, tag, driver);
            if live.used_tokens.is_some() {
                return live;
            }
            let Some(sid) =
                crate::session::resume_store::read_id(agent, session, &agent.resume_cli_kind())
            else {
                return live;
            };
            crate::capture::cursor_context::registered_budget(&agent.name, tag, &sid)
                .filter(|budget| budget.used_tokens.is_some())
                .unwrap_or_else(|| {
                    crate::capture::cursor_context::acp_budget_at(
                        &agent
                            .state_dir()
                            .join(crate::capture::cursor_config::CURSOR_ACP_SESSIONS_DIR_NAME),
                        &sid,
                    )
                })
        }
        _ => rpc_context_budget(&agent.name, tag, driver),
    }
}

pub(super) fn is_cursor_cli_session(agent: &Agent) -> bool {
    summon_core::driver::CliDriver::from_kind(agent.cli_kind())
        == summon_core::driver::CliDriver::Cursor
        && agent.capture_mode() == summon_core::registry::CaptureMode::CliSession
}

pub(super) fn quota_left_from_used(used_pct: f64) -> u8 {
    (100.0 - used_pct).round().clamp(0.0, 100.0) as u8
}

/// 把 statusLine 快照里的周额度与 5 小时额度记进账号级缓存,连同快照落盘时刻 —— 缓存靠它在
/// 同一 reset 窗口内分辨快照新旧。快照缺哪个窗口(订阅用户首个 API 响应前)就不记哪个。
pub(super) fn record_claude_snapshot_quota(
    agent_name: &str,
    tag: &str,
    snapshot: &crate::setup::claude_quota::ClaudeQuotaSnapshot,
) {
    if let Some(window) = &snapshot.five_hour {
        crate::capture::rpc_pool::record_five_hour_quota_observation_shared(
            agent_name,
            quota_left_from_used(window.used_percentage),
            Some(window.resets_at),
            snapshot.observed_at,
        );
    }
    if let Some(window) = &snapshot.seven_day {
        crate::capture::rpc_pool::record_quota_observation_shared(
            agent_name,
            tag,
            quota_left_from_used(window.used_percentage),
            Some(window.resets_at),
            snapshot.observed_at,
        );
    }
}

/// agent 账号的 5 小时窗口剩余额度。claude 的快照由周额度那条读取路径一并记入缓存,
/// 这里只读缓存;默认 Codex 账号的判据同 [`current_session_quota_at`]。
pub(super) fn five_hour_quota_for(agent: &Agent) -> Option<(u8, Option<i64>)> {
    five_hour_quota_shared(
        summon_core::driver::CliDriver::from_kind(agent.cli_kind()),
        crate::capture::rpc_pool::agent_uses_default_codex_account(agent),
        &agent.name,
    )
}

/// 5 小时额度的唯一读取口:面板、轮初角标与收尾角标都从这里取。返回 (剩余百分比, 重置时刻)。`default_codex_account` 只在
/// 调用方已证明该 Codex agent 用默认账号时为 true(rollout 回放的观测只记在默认账号名下)。
pub(super) fn five_hour_quota_shared(
    driver: summon_core::driver::CliDriver,
    default_codex_account: bool,
    agent_name: &str,
) -> Option<(u8, Option<i64>)> {
    if !driver.has_five_hour_and_weekly_quota() {
        return None;
    }
    if driver == summon_core::driver::CliDriver::Codex && default_codex_account {
        crate::capture::rpc_pool::current_default_codex_five_hour_quota_shared(agent_name)
    } else {
        crate::capture::rpc_pool::current_five_hour_quota_shared(agent_name)
    }
}

fn current_session_quota(agent: &Agent, tag: &str, session: &str) -> Option<u8> {
    let claude_snapshot_root = crate::setup::claude_quota::IntegrationPaths::for_current_user()
        .ok()
        .map(|paths| paths.snapshot);
    current_session_quota_at(agent, tag, session, claude_snapshot_root.as_deref())
}

fn current_session_quota_at(
    agent: &Agent,
    tag: &str,
    session: &str,
    claude_snapshot_root: Option<&std::path::Path>,
) -> Option<u8> {
    let driver = summon_core::driver::CliDriver::from_kind(agent.cli_kind());
    if driver == summon_core::driver::CliDriver::Claude {
        let snapshot = crate::session::resume_store::read_id(agent, session, agent.cli_kind())
            .and_then(|id| {
                claude_snapshot_root
                    .and_then(|root| crate::setup::claude_quota::read_snapshot(root, &id).ok())
            });
        if let Some(snapshot) = snapshot {
            record_claude_snapshot_quota(&agent.name, tag, &snapshot);
        }
    }
    match driver {
        summon_core::driver::CliDriver::Claude => {
            crate::capture::rpc_pool::current_quota_shared(&agent.name, tag)
        }
        summon_core::driver::CliDriver::Codex => {
            if crate::capture::rpc_pool::agent_uses_default_codex_account(agent) {
                crate::capture::rpc_pool::current_default_codex_quota_shared(&agent.name, tag)
            } else {
                crate::capture::rpc_pool::current_quota_shared(&agent.name, tag)
            }
        }
        summon_core::driver::CliDriver::Traex => {
            crate::capture::rpc_pool::current_quota_shared(&agent.name, tag)
        }
        _ => None,
    }
}

/// 面板控制项的唯一装配点：上下文、额度、Codex Fast 与 Cursor mode 都是 per-chat 状态，所有
/// 初始/刷新/终态卡共用。Fast 支持性来自 Codex 目录；Cursor mode 优先取 live ACP 目录。
pub(super) fn session_panel_controls(
    agent: &Agent,
    tag: &str,
    session: &str,
    model: &summon_core::model_effort::ResolvedModel,
) -> summon_core::feishu::SessionPanelControls {
    let messaging = resolve_messaging(agent);
    let settings = super::chat_meta::resolve_settings(agent, session, &messaging);
    let cot = summon_core::feishu::SessionCotControls {
        visible: settings.cot_visible(),
        // 面板按「显示」呈现,而配置里存的是「隐藏」——取反只发生在 `ResolvedSettings` 一处。
        thinking: !settings.hide_thinking(),
        tools: settings.cot_tool_details(),
    };
    let codex_speed =
        (agent.cli_kind() == "codex").then(|| summon_core::feishu::CodexSpeedControl {
            selected: codex_service_tier(agent, tag),
            supported: match model.source {
                summon_core::model_effort::ModelSource::ChatOverride
                | summon_core::model_effort::ModelSource::AgentConfig
                | summon_core::model_effort::ModelSource::SessionLive => {
                    codex_fast_support(model.value.as_deref())
                }
                // 这三层都意味着 agent.yaml 没写 model,实际下发时不带 `-m` —— summon 不知道
                // CLI 会挑哪个,判不了它支不支持 Fast。
                summon_core::model_effort::ModelSource::SummonDefault
                | summon_core::model_effort::ModelSource::BackendDefault
                | summon_core::model_effort::ModelSource::CliDefault => None,
            },
        });
    let cursor_mode = uses_rpc_model(agent).then(|| {
        let live = crate::capture::rpc_pool::available_modes_shared(&agent.name, tag);
        let catalog_live = !live.is_empty();
        let effective =
            crate::capture::rpc_pool::current_mode_shared(&agent.name, tag).filter(|mode| {
                mode.parse::<summon_core::driver::CursorSessionMode>()
                    .is_ok()
            });
        let (desired, preference_error) = match cursor_session_mode(agent, tag) {
            Ok(desired) => (desired, false),
            Err(_) => (None, true),
        };
        let pending = desired
            .map(|mode| mode.to_string())
            .filter(|desired| effective.as_deref() != Some(desired.as_str()));
        let options = if catalog_live {
            live.into_iter()
                .filter(|mode| {
                    mode.id
                        .parse::<summon_core::driver::CursorSessionMode>()
                        .is_ok()
                })
                .map(|mode| summon_core::feishu::CursorModeOption {
                    name: if mode.name.trim().is_empty() {
                        mode.id.clone()
                    } else {
                        mode.name
                    },
                    id: mode.id,
                })
                .collect()
        } else {
            [("agent", "Agent"), ("plan", "Plan"), ("ask", "Ask")]
                .into_iter()
                .map(|(id, name)| summon_core::feishu::CursorModeOption {
                    id: id.to_string(),
                    name: name.to_string(),
                })
                .collect()
        };
        summon_core::feishu::CursorModeControl {
            effective,
            pending,
            preference_error,
            options,
            catalog_live,
        }
    });
    let cursor_config = uses_rpc_model(agent).then(|| {
        let live = crate::capture::rpc_pool::cursor_config_options_shared(&agent.name, tag);
        let catalog_live = !live.is_empty();
        let live_model = live
            .iter()
            .find(|option| option.id == "model")
            .and_then(|option| option.current_value.clone())
            .or_else(|| crate::capture::rpc_pool::current_model_shared(&agent.name, tag));
        let fresh = agent.reread().unwrap_or_else(|| agent.clone());
        let loaded = crate::capture::cursor_config::load_target(&fresh, tag);
        if let Err(error) = &loaded {
            log(&summon_i18n::format!(
                "r7160663eb6",
                agent.name,
                error.diagnostic()
            ));
        }
        let desired_model = loaded
            .as_ref()
            .ok()
            .and_then(|target| target.model().map(str::to_string));
        let model_pending = desired_model.is_some() && desired_model != live_model;
        let desired_options = loaded.as_ref().ok().map(|target| target.options());
        // 只开放本轮需求里的模型参数轴。未来 server 即使新增权限/网络类 config，也不能因为
        // “动态目录”四个字就自动出现在 operate 卡片上。
        let controls = live
            .into_iter()
            .filter(|option| {
                matches!(
                    option.id.as_str(),
                    "context" | "reasoning" | "effort" | "thinking" | "fast"
                ) && option
                    .option_type
                    .as_deref()
                    .is_none_or(|kind| kind == "select")
            })
            .map(|option| {
                let pending = desired_options
                    .as_ref()
                    .and_then(|options| options.get(&option.id))
                    .filter(|wanted| option.current_value.as_ref() != Some(*wanted))
                    .cloned();
                let overridden = loaded
                    .as_ref()
                    .is_ok_and(|target| target.overridden_option(&option.id));
                let default_value = loaded
                    .as_ref()
                    .ok()
                    .and_then(|target| target.default_option(&option.id).map(str::to_string));
                summon_core::feishu::CursorConfigControl {
                    overridden,
                    default_value,
                    id: option.id,
                    name: option.name,
                    description: option.description,
                    current: option.current_value,
                    pending,
                    options: option
                        .options
                        .into_iter()
                        .map(|value| summon_core::feishu::CursorConfigOption {
                            value: value.value,
                            name: value.name,
                            description: value.description,
                        })
                        .collect(),
                }
            })
            .collect();
        let model_pending_note = model_pending.then(|| {
            cursor_model_pending_note(
                desired_model.as_deref().unwrap_or_default(),
                live_model.as_deref(),
                &crate::capture::rpc_pool::available_models_shared(&agent.name, tag),
            )
        });
        summon_core::feishu::CursorConfigPanel {
            controls,
            preference_error: loaded.is_err(),
            catalog_live,
            model_pending,
            model_pending_note,
        }
    });
    // 周额度那条读取路径会把 claude 快照记进账号缓存,5 小时额度要排在它之后读。
    let quota_left_pct = current_session_quota(agent, tag, session);
    summon_core::feishu::SessionPanelControls {
        quota_badge: quota_badge_enabled(agent, tag),
        badge_effort: settings.badge_effort(),
        badge_cli: settings.badge_cli(),
        badge_model: settings.badge_model(),
        badge_ctx: settings.badge_ctx(),
        quota_left_pct,
        quota_5h_left_pct: five_hour_quota_for(agent).map(|(left_pct, _)| left_pct),
        context: current_session_context_budget(agent, tag, session),
        cot,
        codex_speed,
        cursor_mode,
        cursor_config,
        terminal: panel_offers_terminal(agent, crate::runtime::web_terminal::serving_links()),
        model_default: (model.source == summon_core::model_effort::ModelSource::ChatOverride)
            .then(|| resolve_default_model(agent, &messaging)),
        effort_default: crate::runtime::chat_model::chat_effort(agent, tag).map(|_| {
            summon_core::model_effort::resolve_effort(
                agent.cli_kind(),
                crate::runtime::chat_model::effective(agent, None).effort(),
            )
        }),
    }
}

/// 模型待收敛时的说明:目标模型不在这条会话公布的目录里就收敛不了,得在目录里另选一个 base
/// model;在目录里则只是还没到下一条消息。
fn cursor_model_pending_note(desired: &str, live: Option<&str>, available: &[String]) -> String {
    let quoted = |model: &str| format!("`{}`", summon_core::feishu::escape_lark_md(model));
    let live = live
        .map(quoted)
        .unwrap_or_else(|| summon_i18n::text!("r0a9156766e").to_string());
    if !available.is_empty() && !available.iter().any(|model| model == desired) {
        summon_i18n::format!("recf4124a6e", quoted(desired))
    } else {
        summon_i18n::format!("rd3ce849db9", quoted(desired))
    }
}

/// 面板上的「>_ 终端」:会话跑在 tmux 里,且终端服务出得了链接。
fn panel_offers_terminal(agent: &Agent, serving_links: bool) -> bool {
    crate::runtime::web_terminal::offers(agent.capture_mode(), serving_links)
}

/// `/model` 与面板按钮改模型的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ModelSet {
    /// 没改，原因已回给当前 chat。
    Refused,
    /// 已写进配置，回执已发或已交给 chat 线程。
    Saved,
    /// 已交给在线的 claude 会话就地切换:claude 认了才写盘，回执由 chat 线程发。
    AwaitingConfirm,
}

/// 改模型 / effort 改到哪一层:本群覆盖、清掉本群覆盖回到默认、bot 默认。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum LayerTarget {
    Chat,
    ChatInherit,
    Default,
}

impl LayerTarget {
    /// 照做了的回执是不是纯确认:只有改本群、且值就是用户打的那个。回到默认时回执报的是默认值,
    /// 改默认时报的是对其他群的影响,都是用户从指令推不出的。
    pub(super) fn receipt_only_confirms(self, echoes_input: bool) -> bool {
        self == LayerTarget::Chat && echoes_input
    }

    /// 回执开头那半句。
    fn set_note(self, what: &str) -> String {
        match self {
            LayerTarget::Chat => summon_i18n::format!("r26e7831e35"),
            LayerTarget::ChatInherit => summon_i18n::format!("r6cf2cd366a"),
            LayerTarget::Default => summon_i18n::format!("r775b56b891"),
        }
    }
}

/// 清掉本群覆盖、而默认层也没设值(交给 CLI 自定)时的回执。summon 此时不再下发这个字段:claude
/// 新起的会话由 CLI 决定;codex 的 `turn/start` 省略字段时沿用线程上持久化的值(连 resume 也带着),
/// 之前切过的值要到新会话才回到 CLI 默认;cursor CLI Session 每轮起进程时不再带 `--model`,用
/// cursor-agent 自己配置里的模型。cursor 参数化 ACP 的模型不走这里(见 `set_model_for_chat`)。
fn inherit_to_cli_default_note(agent: &Agent, what: &str) -> String {
    match CliDriver::from_kind(agent.cli_kind()) {
        CliDriver::Codex | CliDriver::Traex => summon_i18n::format!("r766089f8d9"),
        CliDriver::Cursor => summon_i18n::format!("rbfd3250023"),
        _ => summon_i18n::format!("r0026cc7753"),
    }
}

/// 改默认之后,其他没有本群覆盖的群什么时候换上;再说一句有几个群设了本群覆盖、不受影响。
/// `model` 为 true 讲模型,false 讲 effort(claude 的 effort 不就地跟随,下次启动生效)。
fn default_scope_note(agent: &Agent, tag: &str, model: bool) -> String {
    let when = match (
        CliDriver::from_kind(agent.cli_kind()),
        agent.capture_mode(),
        model,
    ) {
        (CliDriver::Claude, _, true) => summon_i18n::text!("rcb2854eb65"),
        (CliDriver::Claude, _, false) => summon_i18n::text!("r6f70c5322a"),
        (CliDriver::Cursor, summon_core::registry::CaptureMode::Rpc, _) => {
            summon_i18n::text!("rd0eda3d2f9")
        }
        _ => summon_i18n::text!("r4d1c33533e"),
    };
    let what = if model {
        summon_i18n::text!("rc98e118e0a")
    } else {
        "effort"
    };
    let overridden = if model {
        crate::runtime::chat_model::chats_with_model_override(agent, Some(tag))
    } else {
        crate::runtime::chat_model::chats_with_effort_override(agent, Some(tag))
    };
    let mut note = summon_i18n::format!("r6f512c916d");
    if overridden > 0 {
        note.push_str(&summon_i18n::format!("r3c87455848"));
    }
    note
}

/// `/model` 与面板按钮共用的持久模型切换：校验、按 CLI 热切策略投递并写盘、发送回执。
///
/// claude 会话在线时先注入 `/model`、等 claude 认了再写盘(见 `ThreadMsg::SwitchApply` 的
/// `confirm_model`):claude 不认的模型若先写进配置,下次冷启都会带着它。改本群写
/// `threads/<tag>/model`(cursor 参数化 ACP 写本群稀疏覆盖),改默认写 agent.yaml / per-agent bundle。
/// `ChatInherit` 时 `model` 不用,切回的是默认模型。`echoes_input`:`model` 就是用户打的那个
/// (文本命令从片段解析出完整 id 时为 false),只有这时改本群的回执才是纯确认。
#[allow(clippy::too_many_arguments)] // 群、锚点、层次各自独立
pub(super) fn set_model_for_chat(
    chats: &mut HashMap<String, ChatHandle>,
    agent: &Agent,
    fc: &FeishuClient,
    chat_id: &str,
    chat_type: &str,
    model: &str,
    anchor: Option<&str>,
    target: LayerTarget,
    echoes_input: bool,
) -> ModelSet {
    let reply = |text: &str| {
        let _ = send_markdown_archived_anchored(
            fc,
            &agent.state_dir(),
            chat_type,
            chat_id,
            anchor,
            text,
        );
    };
    if target != LayerTarget::ChatInherit {
        if let Err(e) = summon_core::model_effort::check_model_id(model) {
            reply(&summon_i18n::format!(
                "r9e9032f73f",
                summon_core::feishu::escape_lark_md(&e.to_string())
            ));
            return ModelSet::Refused;
        }
    }
    let tag = chat_tag(chat_type, chat_id);
    // cursor 参数化 ACP 的 model 是 base id,和 context/reasoning/fast 同属参数 bundle:本群写稀疏
    // 覆盖、默认写 per-agent bundle,每轮 prompt 前收敛,不发只刷新当前 worker 的 SwitchApply。
    if uses_rpc_model(agent) {
        let requested = (target != LayerTarget::ChatInherit).then_some(model);
        if let Err(error) = apply_cursor_selected_model(agent, &tag, requested, target) {
            log(&summon_i18n::format!("r5929e23cfa", agent.name));
            reply(&error.to_string());
            return ModelSet::Refused;
        }
        log(&summon_i18n::format!("rde55e04dc4", agent.name));
        let ack = match target {
            LayerTarget::Chat => {
                let text = summon_i18n::format!("r6f297450ac");
                if echoes_input {
                    CommandAck::Done(text)
                } else {
                    CommandAck::Note(text)
                }
            }
            LayerTarget::ChatInherit => {
                CommandAck::Done(summon_i18n::text!("r4e80a70bcf").to_string())
            }
            LayerTarget::Default => CommandAck::Note(summon_i18n::format!(
                "r40402b2af3",
                default_scope_note(agent, &tag, true)
            )),
        };
        let _ = send_command_ack(fc, agent, chat_type, chat_id, anchor, &ack);
        return ModelSet::Saved;
    }
    let effective = crate::runtime::chat_model::effective(agent, Some(&tag));
    let (new_model, write) = match target {
        LayerTarget::Chat => (
            Some(model.to_string()),
            super::ModelWrite::Chat(tag.clone()),
        ),
        LayerTarget::ChatInherit => (
            crate::runtime::chat_model::effective(agent, None).model,
            super::ModelWrite::ChatInherit(tag.clone()),
        ),
        LayerTarget::Default => (Some(model.to_string()), super::ModelWrite::Default),
    };
    if let Some(new_model) = &new_model {
        if let Err(e) = summon_core::model_effort::validate(
            agent.cli_kind(),
            Some(new_model),
            effective.effort(),
        ) {
            reply(&summon_i18n::format!("rf1cbb97ec4"));
            return ModelSet::Refused;
        }
    }
    let scope_note =
        (target == LayerTarget::Default).then(|| default_scope_note(agent, &tag, true));
    // 改默认时本群设了自己的模型:本群生效值不变,只写配置。
    if target == LayerTarget::Default
        && effective.model_layer == crate::runtime::chat_model::Layer::Chat
    {
        let confirm = super::ModelConfirm {
            model: model.to_string(),
            write,
        };
        if let Err(e) = confirm.write(agent) {
            reply(&summon_i18n::format!("rf1cbb97ec4"));
            return ModelSet::Refused;
        }
        log(&summon_i18n::format!("r810951611f", agent.name));
        reply(&summon_i18n::format!(
            "r1ce01f460f",
            effective.model().unwrap_or_default(),
            scope_note.unwrap_or_default()
        ));
        return ModelSet::Saved;
    }
    let Some(new_model) = new_model else {
        // 回到默认而默认交给 CLI 自定:没有具体模型可切,删掉本群覆盖,下次启动由 CLI 决定。
        if let Err(e) = crate::runtime::chat_model::set_chat_model(agent, &tag, None) {
            reply(&summon_i18n::format!("rf1cbb97ec4"));
            return ModelSet::Refused;
        }
        reply(&inherit_to_cli_default_note(
            agent,
            summon_i18n::text!("rc98e118e0a"),
        ));
        return ModelSet::Saved;
    };
    let mut plan = plan_config_switch(
        CliDriver::from_kind(agent.cli_kind()).model_switch_spec(agent.capture_mode()),
        chats.contains_key(chat_id),
        &target.set_note(summon_i18n::text!("rc98e118e0a")),
        &new_model,
    );
    if let Some(note) = &scope_note {
        plan.append_note(note);
    }
    if !target.receipt_only_confirms(echoes_input) {
        plan.keep_text();
    }
    if let SwitchPlan::Apply {
        slash: Some(slash),
        receipt,
    } = &plan
    {
        let handed = chats.get(chat_id).is_some_and(|h| {
            h.tx.send(ThreadMsg::SwitchApply {
                slash: Some(slash.clone()),
                receipt: receipt.clone(),
                anchor: anchor.map(str::to_string),
                confirm_model: Some(super::ModelConfirm {
                    model: new_model.clone(),
                    write: write.clone(),
                }),
            })
            .is_ok()
        });
        if handed {
            log(&summon_i18n::format!("ra966fab953", agent.name));
            return ModelSet::AwaitingConfirm;
        }
    }
    let confirm = super::ModelConfirm {
        model: new_model.clone(),
        write,
    };
    if let Err(e) = confirm.write(agent) {
        log(&summon_i18n::format!("r50de9d8286", agent.name));
        reply(&summon_i18n::format!("rf1cbb97ec4"));
        return ModelSet::Refused;
    }
    log(&summon_i18n::format!("r91516b1f16", agent.name));
    dispatch_config_switch(chats, agent, fc, chat_id, chat_type, plan, anchor);
    ModelSet::Saved
}

/// 飞书 `/model <id>` 没有切换时的回执：说明原因，有候选就附上模型按钮。
#[allow(clippy::too_many_arguments)] // 群、输入、判定、锚点、层次各自独立
pub(super) fn reply_model_not_switched(
    agent: &Agent,
    fc: &FeishuClient,
    chat_id: &str,
    chat_type: &str,
    input: &str,
    decision: &summon_core::model_effort::ModelTextCommand,
    anchor: Option<&str>,
    default_layer: bool,
) {
    use summon_core::model_effort::ModelTextCommand;
    let (note, suggestions): (String, &[String]) = match decision {
        ModelTextCommand::Switch(_) => return,
        ModelTextCommand::NotAModelId { error, suggestions } => (
            summon_i18n::format!(
                "rb6a14095ce",
                summon_core::feishu::escape_lark_md(&error.to_string())
            ),
            suggestions,
        ),
        ModelTextCommand::NotInCatalog { suggestions } => {
            (summon_i18n::format!("rf38feb3a92"), suggestions)
        }
        ModelTextCommand::NoCatalog => (summon_i18n::format!("rc2e7c1bba8", agent.name), &[]),
    };
    send_model_note(
        agent,
        fc,
        chat_id,
        chat_type,
        &note,
        suggestions,
        anchor,
        default_layer,
    );
}

/// 只发了 `/model`:说明用法，附上候选目录开头的几个模型按钮。
pub(super) fn reply_model_usage(
    agent: &Agent,
    fc: &FeishuClient,
    chat_id: &str,
    chat_type: &str,
    candidates: &[String],
    anchor: Option<&str>,
) {
    let shown: Vec<String> = candidates
        .iter()
        .filter(|m| !m.trim().is_empty())
        .take(summon_core::model_effort::MODEL_SUGGESTION_CAP)
        .cloned()
        .collect();
    send_model_note(
        agent,
        fc,
        chat_id,
        chat_type,
        summon_i18n::text!("rf146a45577"),
        &shown,
        anchor,
        false,
    );
}

/// cursor 的无参 `/model`(`default_layer` 为 true 时是 `/model default`):发一张只有一个下拉框的
/// 选择卡,列出完整候选目录。选中走与会话面板下拉框相同的 `model` / `model_default` 回调,
/// 鉴权与校验都沿用卡片回调既有的那一套。没有候选时退回文字用法说明。
pub(super) fn reply_cursor_model_picker(
    agent: &Agent,
    fc: &FeishuClient,
    chat_id: &str,
    chat_type: &str,
    candidates: &[String],
    anchor: Option<&str>,
    default_layer: bool,
) {
    if candidates.iter().all(|m| m.trim().is_empty()) {
        let _ = send_markdown_archived_anchored(
            fc,
            &agent.state_dir(),
            chat_type,
            chat_id,
            anchor,
            summon_i18n::text!("rf146a45577"),
        );
        return;
    }
    let tag = chat_tag(chat_type, chat_id);
    let messaging = resolve_messaging(agent);
    // 高亮口径与会话面板相同:本群层是摘要里的生效值,默认层在有本群覆盖时取 bot 默认。
    let chat = resolve_panel_model(agent, &tag, &messaging);
    let selected = if !default_layer {
        chat.value
    } else if chat.source == summon_core::model_effort::ModelSource::ChatOverride {
        resolve_default_model(agent, &messaging).value
    } else {
        chat.value
    };
    let card = summon_core::feishu::card_cursor_model_picker(
        candidates,
        selected.as_deref(),
        &tag,
        default_layer,
    );
    let _ = send_card_archived(
        fc,
        &agent.state_dir(),
        chat_type,
        chat_id,
        anchor,
        "model_picker",
        &card,
    );
}

/// `/model` 的说明回执：有候选就发带模型按钮的卡(与会话面板的模型按钮同一个动作),没有就发文字。
/// `default_layer` 为 true 时按钮改 bot 默认。
#[allow(clippy::too_many_arguments)] // 群、说明、候选、锚点、层次各自独立
fn send_model_note(
    agent: &Agent,
    fc: &FeishuClient,
    chat_id: &str,
    chat_type: &str,
    note: &str,
    suggestions: &[String],
    anchor: Option<&str>,
    default_layer: bool,
) {
    if suggestions.is_empty() {
        let _ = send_markdown_archived_anchored(
            fc,
            &agent.state_dir(),
            chat_type,
            chat_id,
            anchor,
            note,
        );
        return;
    }
    let card = summon_core::feishu::card_model_suggestions(
        &summon_i18n::format!("ra0d92cdec3"),
        suggestions,
        &chat_tag(chat_type, chat_id),
        default_layer,
    );
    let _ = send_card_archived(
        fc,
        &agent.state_dir(),
        chat_type,
        chat_id,
        anchor,
        "model_suggestions",
        &card,
    );
}

/// 面板「改 effort」:operate-gate → 取 `value.effort` → fail-closed 校验 (model, effort) → 写 agent.yaml
/// 顶层 `effort:` → 快重置 → 回执。只对有独立 effort 轴的 cli(claude/codex)渲染了按钮、才会触发。
/// per-chat「额度显示」偏好:`threads/<tag>/badge_quota` 标记文件,**存在即开、默认关**
/// (owner 2026-08-10:额度是敏感面,群卡角标默认不露,面板开关按会话粒度控制)。
/// 用标记文件而不是 json:单布尔值,存在性即语义,读写都无解析失败路径。
pub(super) fn quota_badge_enabled(agent: &Agent, tag: &str) -> bool {
    quota_badge_enabled_at(&agent.state_dir(), tag)
}

pub(super) fn quota_badge_enabled_at(state_dir: &std::path::Path, tag: &str) -> bool {
    state_dir
        .join("threads")
        .join(tag)
        .join("badge_quota")
        .exists()
}

/// 写「额度显示」偏好:开 = 建标记文件,关 = 删(不存在视为已关,幂等)。
pub(super) fn set_quota_badge(agent: &Agent, tag: &str, on: bool) -> std::io::Result<()> {
    let dir = agent.state_dir().join("threads").join(tag);
    let path = dir.join("badge_quota");
    if on {
        std::fs::create_dir_all(&dir)?;
        std::fs::write(&path, b"on\n")
    } else {
        match std::fs::remove_file(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            r => r,
        }
    }
}

fn codex_service_tier_path(agent: &Agent, tag: &str) -> std::path::PathBuf {
    agent
        .state_dir()
        .join("threads")
        .join(tag)
        .join("codex_service_tier")
}

/// 本 chat 的上下文提醒已发到哪一档(`threads/<tag>/ctx_warned`)。没发过 → None。
///
/// **靠内容表达状态(存的是档位数字),不是靠文件在不在** —— 与 `stream_state.json` 那类
/// 「存在即语义」的 checkpoint 不同,后者被版本库恢复出来会造成误动作。这个文件即使被恢复,
/// 最坏也只是少提醒一次。即便如此它仍是运行时产物,已在 .gitignore 里挡掉。
pub(super) fn ctx_warned_step(agent_dir: &std::path::Path, tag: &str) -> Option<u8> {
    let raw =
        std::fs::read_to_string(agent_dir.join("threads").join(tag).join("ctx_warned")).ok()?;
    raw.trim().parse::<u8>().ok()
}

/// 写已提醒档位;`None` = 清除(上下文回落后重新计档)。
pub(super) fn set_ctx_warned_step(
    agent_dir: &std::path::Path,
    tag: &str,
    step: Option<u8>,
) -> std::io::Result<()> {
    let dir = agent_dir.join("threads").join(tag);
    let path = dir.join("ctx_warned");
    match step {
        Some(s) => {
            std::fs::create_dir_all(&dir)?;
            std::fs::write(&path, format!("{s}\n"))
        }
        None => match std::fs::remove_file(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            r => r,
        },
    }
}

/// 本轮该用的 effort:本群覆盖 > agent.yaml(见 [`crate::runtime::chat_model::effective`])。
pub(super) fn effort_for(agent: &Agent, tag: &str) -> Option<String> {
    crate::runtime::chat_model::effective(agent, Some(tag)).effort
}

fn cursor_session_mode_path(agent: &Agent, tag: &str) -> std::path::PathBuf {
    agent
        .state_dir()
        .join("threads")
        .join(tag)
        .join("cursor_mode")
}

const CURSOR_MODE_READ_PUBLIC_ERROR: &str = "re2a22c4d64";

/// 本 chat 显式选择的 Cursor ACP session mode。只有文件缺失表示未托管；IO/内容错误必须
/// fail-closed，否则已选 plan/ask 的 chat 可能悄悄沿用权限更宽的旧 agent mode。
pub(super) fn cursor_session_mode(
    agent: &Agent,
    tag: &str,
) -> anyhow::Result<Option<summon_core::driver::CursorSessionMode>> {
    let path = cursor_session_mode_path(agent, tag);
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            log(&summon_i18n::format!(
                "r6d3ca6007f",
                agent.name,
                path.display()
            ));
            anyhow::bail!(::summon_core::i18n::label(CURSOR_MODE_READ_PUBLIC_ERROR));
        }
    };
    match raw.parse() {
        Ok(mode) => Ok(Some(mode)),
        Err(error) => {
            log(&summon_i18n::format!(
                "r1304505d9c",
                agent.name,
                path.display()
            ));
            anyhow::bail!(::summon_core::i18n::label(CURSOR_MODE_READ_PUBLIC_ERROR));
        }
    }
}

/// 原子覆盖本 chat 的 Cursor ACP session mode。
fn set_cursor_session_mode(
    agent: &Agent,
    tag: &str,
    mode: &summon_core::driver::CursorSessionMode,
) -> anyhow::Result<()> {
    crate::runtime::fsio::atomic_write(
        &cursor_session_mode_path(agent, tag),
        mode.as_str().as_bytes(),
    )
}

const CURSOR_MODE_ERROR_ID_CAP: usize = 6;
const CURSOR_MODE_ERROR_ID_CHAR_CAP: usize = 32;
/// 包含省略提示；按上面两层 cap 推导出的硬上限，测试钉死避免后续文案扩成无界目录。
const CURSOR_MODE_ERROR_CHOICES_CHAR_CAP: usize = 256;
const CURSOR_MODE_PERSISTENCE_PUBLIC_ERROR: &str = "r71a7a93b9b";

fn bounded_cursor_mode_error_id(value: &str) -> String {
    let mut chars = value.chars();
    let mut bounded: String = chars.by_ref().take(CURSOR_MODE_ERROR_ID_CHAR_CAP).collect();
    if chars.next().is_some() {
        bounded.pop();
        bounded.push('…');
    }
    bounded
}

fn cursor_mode_choices_for_error(
    available: &[crate::capture::cursor_acp::CursorAcpMode],
) -> Option<String> {
    let mut safe_count = 0usize;
    let mut shown = Vec::with_capacity(CURSOR_MODE_ERROR_ID_CAP);
    for mode in available {
        if mode
            .id
            .parse::<summon_core::driver::CursorSessionMode>()
            .is_err()
        {
            continue;
        }
        safe_count = safe_count.saturating_add(1);
        if shown.len() < CURSOR_MODE_ERROR_ID_CAP {
            shown.push(bounded_cursor_mode_error_id(&mode.id));
        }
    }
    if safe_count == 0 {
        return None;
    }

    let mut choices = shown.join(" / ");
    let omitted = safe_count.saturating_sub(shown.len());
    if omitted > 0 {
        choices.push_str(&summon_i18n::format!("r881e5b46cf"));
    }
    debug_assert!(choices.chars().count() <= CURSOR_MODE_ERROR_CHOICES_CHAR_CAP);
    Some(choices)
}

/// `/mode` 与卡片共用的校验/提交接缝。live catalog 存在时精确以它为准；尚未建立会话时只允许
/// ACP 标准 agent/plan/ask，避免拼错值永久落盘并在每轮重复失败。
pub(super) fn apply_cursor_session_mode(
    agent: &Agent,
    tag: &str,
    requested: &summon_core::driver::CursorSessionMode,
) -> anyhow::Result<()> {
    if !uses_rpc_model(agent) {
        anyhow::bail!(summon_i18n::format!("ref613171ca"));
    }
    let available = crate::capture::rpc_pool::available_modes_shared(&agent.name, tag);
    if available.is_empty() {
        if !requested.is_standard() {
            anyhow::bail!(summon_i18n::format!("r0878e42c74"));
        }
    } else if !available.iter().any(|mode| mode.id == requested.as_str()) {
        let Some(ids) = cursor_mode_choices_for_error(&available) else {
            anyhow::bail!(summon_i18n::format!("rb8f41a9f93"));
        };
        anyhow::bail!(summon_i18n::format!("r0a266d65d5"));
    }
    if let Err(error) = set_cursor_session_mode(agent, tag, requested) {
        log(&summon_i18n::format!(
            "rdb26d94328",
            agent.name,
            cursor_session_mode_path(agent, tag).display()
        ));
        anyhow::bail!(
            "{CURSOR_MODE_PERSISTENCE_PUBLIC_ERROR}",
            CURSOR_MODE_PERSISTENCE_PUBLIC_ERROR =
                ::summon_core::i18n::label(CURSOR_MODE_PERSISTENCE_PUBLIC_ERROR)
        );
    }
    Ok(())
}

const CURSOR_PARAMETER_ID_CHAR_CAP: usize = 64;
const CURSOR_PARAMETER_VALUE_CHAR_CAP: usize = 256;
const CURSOR_PARAMETER_PERSISTENCE_PUBLIC_ERROR: &str = "r303686a87b";

fn cursor_parameter_axis_is_open(id: &str) -> bool {
    matches!(id, "context" | "reasoning" | "effort" | "thinking" | "fast")
}

fn fresh_cursor_agent(agent: &Agent) -> Agent {
    agent.reread().unwrap_or_else(|| agent.clone())
}

/// 改 Cursor 参数化模型。`requested` 为 None 只用于 `ChatInherit`(清掉本群模型覆盖)。
/// 模型必须在这条 ACP 会话报的 live 目录里,写进哪一层由 `target` 定;不写 legacy
/// `runtime.rpc_model`。下一轮 checkout 在原 SID 上 exact 收敛，失败则 prompt 前 fail-stop。
fn apply_cursor_selected_model(
    agent: &Agent,
    tag: &str,
    requested: Option<&str>,
    target: LayerTarget,
) -> anyhow::Result<()> {
    if !uses_rpc_model(agent) {
        anyhow::bail!(summon_i18n::format!("rbcd2ea0c5c"));
    }
    if let Some(requested) = requested {
        if requested.trim() != requested
            || requested.is_empty()
            || requested.chars().count() > CURSOR_PARAMETER_VALUE_CHAR_CAP
        {
            anyhow::bail!(summon_i18n::format!("rf41742a14e"));
        }
        let available = crate::capture::rpc_pool::available_models_shared(&agent.name, tag);
        if available.is_empty() {
            anyhow::bail!(summon_i18n::format!("r0d46fb91ea"));
        }
        if !available.iter().any(|model| model == requested) {
            anyhow::bail!(summon_i18n::format!("r143e7d0274"));
        }
    }
    let fresh = fresh_cursor_agent(agent);
    let read_error = |error: crate::capture::cursor_config::CursorConfigError| {
        log(&summon_i18n::format!(
            "rff172b41fb",
            agent.name,
            error.diagnostic()
        ));
        anyhow::anyhow!(error.to_string())
    };
    let write_error = |error: crate::capture::cursor_config::CursorConfigError| {
        log(&summon_i18n::format!(
            "r8618791743",
            agent.name,
            error.diagnostic()
        ));
        anyhow::anyhow!(::summon_core::i18n::label(
            CURSOR_PARAMETER_PERSISTENCE_PUBLIC_ERROR
        ))
    };
    match (target, requested) {
        (LayerTarget::Default, Some(requested)) => {
            let view = crate::capture::cursor_config::load(&fresh).map_err(read_error)?;
            crate::capture::cursor_config::set_selected_model(
                &fresh,
                view.bundle.generation,
                requested,
            )
            .map_err(write_error)?;
        }
        (LayerTarget::Chat, Some(_)) | (LayerTarget::ChatInherit, None) => {
            let view = crate::capture::cursor_config::load_layer(
                &fresh,
                crate::capture::cursor_config::CursorConfigLayer::Chat(tag),
            )
            .map_err(read_error)?;
            crate::capture::cursor_config::set_chat_selected_model(
                &fresh,
                tag,
                view.bundle.generation,
                requested,
            )
            .map_err(write_error)?;
        }
        _ => anyhow::bail!(summon_i18n::format!("rf41742a14e")),
    }
    Ok(())
}

/// 卡片与 `/fast` 共用的 Cursor 参数提交接缝:写本群覆盖,`value` 为 None 清掉这个轴的本群覆盖。
/// id/value 必须来自 exact live catalog;当前模型尚未与本群目标收敛时拒绝写,避免把旧模型目录
/// 里的值记到新模型 profile。
pub(super) fn apply_cursor_config_option(
    agent: &Agent,
    tag: &str,
    config_id: &str,
    value: Option<&str>,
) -> anyhow::Result<()> {
    if !uses_rpc_model(agent) {
        anyhow::bail!(summon_i18n::format!("r1c73433aa3"));
    }
    if config_id.trim() != config_id
        || !cursor_parameter_axis_is_open(config_id)
        || config_id.chars().count() > CURSOR_PARAMETER_ID_CHAR_CAP
        || value.is_some_and(|value| {
            value.trim() != value
                || value.is_empty()
                || value.chars().count() > CURSOR_PARAMETER_VALUE_CHAR_CAP
        })
    {
        anyhow::bail!(summon_i18n::format!("rbe732c4663"));
    }
    let live = crate::capture::rpc_pool::cursor_config_options_shared(&agent.name, tag);
    let live_model = live
        .iter()
        .find(|option| option.id == "model")
        .and_then(|option| option.current_value.clone())
        // 旧 server 可能只在 models 块报 currentModelId；仍要求 live exact 证据。
        .or_else(|| crate::capture::rpc_pool::current_model_shared(&agent.name, tag))
        .ok_or_else(|| anyhow::anyhow!(summon_i18n::format!("rbb61f604f2")))?;
    if let Some(value) = value {
        let axis = live
            .iter()
            .find(|option| option.id == config_id)
            .ok_or_else(|| anyhow::anyhow!(summon_i18n::format!("r6f8b51b7ee")))?;
        if !axis.options.iter().any(|option| option.value == value) {
            anyhow::bail!(summon_i18n::format!("r1a441074ff"));
        }
    }

    let fresh = fresh_cursor_agent(agent);
    let target = crate::capture::cursor_config::load_target(&fresh, tag).map_err(|error| {
        log(&summon_i18n::format!(
            "rff172b41fb",
            agent.name,
            error.diagnostic()
        ));
        anyhow::anyhow!(error.to_string())
    })?;
    if target.model().is_some_and(|model| model != live_model) {
        anyhow::bail!(summon_i18n::format!("r523153e00a"));
    }
    crate::capture::cursor_config::set_chat_model_option(
        &fresh,
        tag,
        target.chat.bundle.generation,
        &live_model,
        config_id,
        value,
    )
    .map_err(|error| {
        log(&summon_i18n::format!(
            "rcd9498ecb6",
            agent.name,
            error.diagnostic()
        ));
        anyhow::anyhow!(::summon_core::i18n::label(
            CURSOR_PARAMETER_PERSISTENCE_PUBLIC_ERROR
        ))
    })?;
    Ok(())
}

/// 「本群这套设为默认」:把本群当前的 Cursor 模型与参数写成 per-agent 默认 bundle。
fn promote_cursor_chat_to_default(agent: &Agent, tag: &str) -> anyhow::Result<()> {
    if !uses_rpc_model(agent) {
        anyhow::bail!(summon_i18n::format!("r1c73433aa3"));
    }
    let fresh = fresh_cursor_agent(agent);
    crate::capture::cursor_config::promote_chat_target_to_default(&fresh, tag).map_err(
        |error| {
            log(&summon_i18n::format!(
                "reb525e5dc8",
                agent.name,
                error.diagnostic()
            ));
            match error.kind() {
                crate::capture::cursor_config::CursorConfigErrorKind::InvalidInput => {
                    anyhow::anyhow!(summon_i18n::format!("rb10bb5c31a"))
                }
                _ => anyhow::anyhow!(::summon_core::i18n::label(
                    CURSOR_PARAMETER_PERSISTENCE_PUBLIC_ERROR
                )),
            }
        },
    )?;
    Ok(())
}

/// 本 chat 显式托管的 Codex 速度档位。缺文件代表 legacy unmanaged：继续继承 Codex 自身配置。
///
/// 文件属于跨轮协议，必须严格接受 `default` / `fast` 两个 canonical 值；坏值或读失败均降级
/// unmanaged，避免把未知内容继续下发给 app-server。降级需要留日志，不能静默吞掉盘上损坏。
pub(super) fn codex_service_tier(
    agent: &Agent,
    tag: &str,
) -> Option<summon_core::driver::CodexServiceTier> {
    let path = codex_service_tier_path(agent, tag);
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            log(&summon_i18n::format!("r02755e050d", agent.name));
            return None;
        }
    };
    match raw.parse() {
        Ok(tier) => Some(tier),
        Err(e) => {
            log(&summon_i18n::format!(
                "r6e06d9c16a",
                agent.name,
                path.display()
            ));
            None
        }
    }
}

/// 原子覆盖本 chat 的 Codex 速度档位。写入 canonical 裸值，不带换行；reader 对内容严格解析。
pub(super) fn set_codex_service_tier(
    agent: &Agent,
    tag: &str,
    tier: summon_core::driver::CodexServiceTier,
) -> anyhow::Result<()> {
    crate::runtime::fsio::atomic_write(
        &codex_service_tier_path(agent, tag),
        tier.as_persisted_str().as_bytes(),
    )
}

/// 已取得模型能力结论后提交档位。拆出这一层是为了把「校验成功才写」做成可独立回归的原子接缝；
/// `supports_fast=None` 只允许 Standard，不能把未知误当支持。
fn persist_checked_codex_service_tier(
    agent: &Agent,
    tag: &str,
    requested: summon_core::driver::CodexServiceTier,
    supports_fast: Option<bool>,
) -> anyhow::Result<()> {
    if agent.cli_kind() != "codex" {
        anyhow::bail!(summon_i18n::format!("r17e9a8afb8"));
    }
    if requested == summon_core::driver::CodexServiceTier::Fast {
        match supports_fast {
            Some(true) => {}
            Some(false) => anyhow::bail!(summon_i18n::format!("racf041cd38")),
            None => anyhow::bail!(summon_i18n::format!("r6ec5b2e3de")),
        }
    }
    set_codex_service_tier(agent, tag, requested)
        .map_err(|e| anyhow::anyhow!(summon_i18n::format!("r7aac416250")))?;
    Ok(())
}

/// 提交本 chat 的 Codex 速度档位，供卡片与 `/fast on|off` 共用。
///
/// Fast 必须基于刚重读的 manifest 显式模型和 Codex 本机目录得到确定支持；任何一步未知都
/// fail-closed。Standard 是关闭 sticky Fast 的安全方向，即使 manifest 暂时不可重读也必须允许写。
/// 返回重读后的 agent，供卡片刷新继续使用同一份模型快照。
pub(super) fn apply_codex_service_tier(
    agent: &Agent,
    tag: &str,
    requested: summon_core::driver::CodexServiceTier,
) -> anyhow::Result<Option<Agent>> {
    apply_codex_service_tier_with(agent, tag, requested, codex_fast_support)
}

fn apply_codex_service_tier_with(
    agent: &Agent,
    tag: &str,
    requested: summon_core::driver::CodexServiceTier,
    fast_support: impl Fn(Option<&str>) -> Option<bool>,
) -> anyhow::Result<Option<Agent>> {
    let fresh_agent = if agent.cli_kind() == "codex" {
        reread_for_codex_speed_action(agent)
    } else {
        None
    };
    let supports_fast = if requested == summon_core::driver::CodexServiceTier::Fast {
        // 只认本群生效的显式模型(本群覆盖或 manifest):TurnCtx 对缺省模型同样传 None 给 Codex
        // 自决，不能拿面板展示用的 summon 默认桶冒充实际模型。
        let current_model = fresh_agent
            .as_ref()
            .and_then(|fresh| crate::runtime::chat_model::effective(fresh, Some(tag)).model);
        fast_support(current_model.as_deref())
    } else {
        None
    };
    persist_checked_codex_service_tier(agent, tag, requested, supports_fast)?;
    Ok(fresh_agent)
}

fn card_codex_service_tier(
    value: &serde_json::Value,
) -> Option<summon_core::driver::CodexServiceTier> {
    match value.get("mode").and_then(|v| v.as_str()) {
        Some("default") => Some(summon_core::driver::CodexServiceTier::Standard),
        Some("fast") => Some(summon_core::driver::CodexServiceTier::Fast),
        _ => None,
    }
}

/// 面板「Codex 速度」回调：严格解析 `value.mode` → 原子写 per-chat 偏好 → 原地刷新。
///
/// 鉴权在 dispatcher 集中查表完成；此处不投递 `SwitchApply`，常驻 app-server 下一轮的
/// `turn/start` 会显式携带新档位。非法 mode fail-closed，不得创建或改写偏好文件。
pub(super) fn handle_card_codex_service_tier(
    chats: &mut HashMap<String, ChatHandle>,
    agent: &Agent,
    fc: &FeishuClient,
    card: &CardActionMsg,
) {
    let chat_id = &card.chat_id;
    // 偏好路径依赖 tag；handle 不在内存时必须回退盘上 chat_type，避免群误写进 p2p 幽灵目录。
    let chat_type = resolve_chat_type(chats, agent, chat_id);
    let tag = chat_tag(&chat_type, chat_id);
    let Some(requested) = card_codex_service_tier(&card.value) else {
        let mode = card.value.get("mode");
        log(&summon_i18n::format!("rd65a3f321c", agent.name));
        let _ = send_markdown_archived(
            fc,
            &agent.state_dir(),
            &chat_type,
            chat_id,
            summon_i18n::text!("r1328f08dad"),
        );
        return;
    };
    let fresh_agent = match apply_codex_service_tier(agent, &tag, requested) {
        Ok(fresh_agent) => fresh_agent,
        Err(e) => {
            log(&summon_i18n::format!("r1d5eb61239", agent.name));
            let _ =
                send_markdown_archived(fc, &agent.state_dir(), &chat_type, chat_id, &e.to_string());
            return;
        }
    };
    log(&summon_i18n::format!(
        "r9cd9e3d7df",
        agent.name,
        requested.as_persisted_str()
    ));
    refresh_config_panel(
        chats,
        fresh_agent.as_ref().unwrap_or(agent),
        fc,
        &chat_type,
        card,
    );
}

/// 面板「Cursor mode」回调：严格解析动态 id → 以 live ACP catalog 复核 → 原子写 per-chat
/// 偏好 → 原地刷新。下一轮 checkout 走 `session/set_mode`，不打断当前轮、不重开进程。
pub(super) fn handle_card_cursor_mode(
    chats: &mut HashMap<String, ChatHandle>,
    agent: &Agent,
    fc: &FeishuClient,
    card: &CardActionMsg,
) {
    let chat_id = &card.chat_id;
    let chat_type = resolve_chat_type(chats, agent, chat_id);
    let tag = chat_tag(&chat_type, chat_id);
    let requested = card
        .value
        .get("mode")
        .and_then(|value| value.as_str())
        .and_then(|value| value.parse::<summon_core::driver::CursorSessionMode>().ok());
    let Some(requested) = requested else {
        log(&summon_i18n::format!(
            "r9d159fd073",
            agent.name,
            card.value.get("mode")
        ));
        let _ = send_markdown_archived(
            fc,
            &agent.state_dir(),
            &chat_type,
            chat_id,
            summon_i18n::text!("r9f7a1dad51"),
        );
        return;
    };
    if let Err(error) = apply_cursor_session_mode(agent, &tag, &requested) {
        log(&summon_i18n::format!("r340f7d677a", agent.name));
        let _ = send_markdown_archived(
            fc,
            &agent.state_dir(),
            &chat_type,
            chat_id,
            &error.to_string(),
        );
        return;
    }
    log(&summon_i18n::format!("r71a88e6dae", agent.name));
    refresh_config_panel(chats, agent, fc, &chat_type, card);
}

/// 面板「Cursor 参数」回调：exact live catalog 校验 → per-agent bundle 原子提交 → 原地刷新。
/// 只保存 pending，不打断当前轮；下一轮 checkout 在同一 SID 上收敛成功后才发 prompt。
pub(super) fn handle_card_cursor_config(
    chats: &mut HashMap<String, ChatHandle>,
    agent: &Agent,
    fc: &FeishuClient,
    card: &CardActionMsg,
) {
    let chat_id = &card.chat_id;
    let chat_type = resolve_chat_type(chats, agent, chat_id);
    let tag = chat_tag(&chat_type, chat_id);
    let config_id = card.value.get("config_id").and_then(|value| value.as_str());
    let inherit = card
        .value
        .get("inherit")
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    let value = card.value.get("value").and_then(|value| value.as_str());
    let (Some(config_id), true) = (config_id, inherit || value.is_some()) else {
        log(&summon_i18n::format!("r0a6e5f4e7d", agent.name));
        let _ = send_markdown_archived(
            fc,
            &agent.state_dir(),
            &chat_type,
            chat_id,
            summon_i18n::text!("rbe732c4663"),
        );
        return;
    };
    let value = if inherit { None } else { value };
    if let Err(error) = apply_cursor_config_option(agent, &tag, config_id, value) {
        log(&summon_i18n::format!("r5a502d7f91", agent.name));
        let _ = send_markdown_archived(
            fc,
            &agent.state_dir(),
            &chat_type,
            chat_id,
            &error.to_string(),
        );
        return;
    }
    log(&summon_i18n::format!("r62b760b862", agent.name));
    refresh_config_panel(chats, agent, fc, &chat_type, card);
}

/// 面板「本群这套设为默认」:把本群当前的 Cursor 模型与参数写成 bot 默认。鉴权在 dispatcher
/// 按 agent 范围查表完成。
pub(super) fn handle_card_cursor_config_default(
    chats: &mut HashMap<String, ChatHandle>,
    agent: &Agent,
    fc: &FeishuClient,
    card: &CardActionMsg,
) {
    let chat_id = &card.chat_id;
    let chat_type = resolve_chat_type(chats, agent, chat_id);
    let tag = chat_tag(&chat_type, chat_id);
    let text = match promote_cursor_chat_to_default(agent, &tag) {
        Ok(()) => {
            log(&summon_i18n::format!("r0b84da1b25", agent.name));
            summon_i18n::format!("r40c0ce315a", default_scope_note(agent, &tag, true))
        }
        Err(error) => error.to_string(),
    };
    let _ = send_markdown_archived(fc, &agent.state_dir(), &chat_type, chat_id, &text);
    refresh_config_panel(chats, agent, fc, &chat_type, card);
}

/// 面板「额度显示」开关回调:写 per-chat 标记 → 原地刷新面板卡(按钮标签/高亮翻转即回执)。
/// 鉴权在 dispatcher 集中查表完成(quota_badge → operate 档)。
pub(super) fn handle_card_quota_badge(
    chats: &mut HashMap<String, ChatHandle>,
    agent: &Agent,
    fc: &FeishuClient,
    card: &CardActionMsg,
) {
    let chat_id = &card.chat_id;
    let chat_type = card_chat_type(chats, chat_id);
    let on = card
        .value
        .get("on")
        .and_then(|v| v.as_bool())
        .unwrap_or(false); // 缺字段按「关」处理:失败方向与默认一致,绝不误开敏感面
    let tag = chat_tag(&chat_type, chat_id);
    if let Err(e) = set_quota_badge(agent, &tag, on) {
        log(&summon_i18n::format!("r9121fdbea8", agent.name));
        let _ = send_markdown_archived(
            fc,
            &agent.state_dir(),
            &chat_type,
            chat_id,
            &summon_i18n::format!("r8c765b78ac"),
        );
        return;
    }
    log(&summon_i18n::format!(
        "r3548620931",
        agent.name,
        if on {
            summon_i18n::text!("r39eae64cfc")
        } else {
            summon_i18n::text!("r5d0ae622f6")
        }
    ));
    refresh_config_panel(chats, agent, fc, &chat_type, card);
}

/// 面板角标那一行的开关(CLI / 模型 / 档位 / 上下文用量)→ 写 per-chat 分级开关
/// `badge.cli` / `badge.model` / `badge.effort` / `badge.ctx`,控制回复卡角标是否渲染那一项。
///
/// 鉴权在 dispatcher 集中查表完成(badge_* → operate 档)。走分级开关的一轮一快照契约:
/// 写完只刷新面板,**不通知在跑的那一轮** —— 轮中途改了下一轮才生效,当前这一轮的角标已经定了。
pub(super) fn handle_card_badge_item(
    chats: &mut HashMap<String, ChatHandle>,
    agent: &Agent,
    fc: &FeishuClient,
    card: &CardActionMsg,
    item: summon_core::settings::SettingKey,
) {
    let setting = summon_core::settings::decl_by_id(item).key;
    let chat_id = &card.chat_id;
    let chat_type = card_chat_type(chats, chat_id);
    let session = chats
        .get(chat_id)
        .map(|handle| handle.tmux_name.clone())
        .unwrap_or_else(|| super::chat_meta::chat_session_name(agent, &chat_type, chat_id));
    let Some(on) = card.value.get("on").and_then(|value| value.as_bool()) else {
        return;
    };
    if let Err(error) = super::chat_meta::set_chat_setting(agent, &session, item, Some(on)) {
        log(&summon_i18n::format!("r0756d70c7d", agent.name));
        let _ = send_markdown_archived(
            fc,
            &agent.state_dir(),
            &chat_type,
            chat_id,
            summon_i18n::text!("race443cfe7"),
        );
        return;
    }
    log(&summon_i18n::format!(
        "r230183d157",
        agent.name,
        if on {
            summon_i18n::text!("r39eae64cfc")
        } else {
            summon_i18n::text!("r5d0ae622f6")
        }
    ));
    refresh_config_panel(chats, agent, fc, &chat_type, card);
}

pub(super) fn handle_card_session_cot(
    chats: &mut HashMap<String, ChatHandle>,
    agent: &Agent,
    fc: &FeishuClient,
    card: &CardActionMsg,
) {
    let chat_id = &card.chat_id;
    let chat_type = card_chat_type(chats, chat_id);
    let session = chats
        .get(chat_id)
        .map(|handle| handle.tmux_name.clone())
        .unwrap_or_else(|| super::chat_meta::chat_session_name(agent, &chat_type, chat_id));
    let Some(axis) = card.value.get("axis").and_then(|value| value.as_str()) else {
        return;
    };
    let Some(on) = card.value.get("on").and_then(|value| value.as_bool()) else {
        return;
    };
    let id = match axis {
        "visible" => summon_core::settings::SettingKey::CotVisible,
        // 面板给的是「显示」语义,与声明表一致 —— 不在这里取反。
        "thinking" => summon_core::settings::SettingKey::CotThinking,
        "tools" => summon_core::settings::SettingKey::CotToolDetails,
        _ => return,
    };
    let settings = match super::chat_meta::set_chat_setting(agent, &session, id, Some(on)) {
        Ok(settings) => settings,
        Err(error) => {
            log(&summon_i18n::format!("rb2504db987", agent.name));
            let _ = send_markdown_archived(
                fc,
                &agent.state_dir(),
                &chat_type,
                chat_id,
                summon_i18n::text!("r3fa3c66a5e"),
            );
            return;
        }
    };
    if let Some(handle) = chats.get(chat_id) {
        let _ = handle.tx.send(ThreadMsg::CotApply {
            visible: settings.get(summon_core::settings::SettingKey::CotVisible),
            thinking: settings.get(summon_core::settings::SettingKey::CotThinking),
            tools: settings.get(summon_core::settings::SettingKey::CotToolDetails),
        });
    }
    refresh_config_panel(chats, agent, fc, &chat_type, card);
}

pub(super) fn handle_card_session_refresh(
    chats: &mut HashMap<String, ChatHandle>,
    agent: &Agent,
    fc: &FeishuClient,
    card: &CardActionMsg,
) {
    let chat_type = card_chat_type(chats, &card.chat_id);
    refresh_config_panel(chats, agent, fc, &chat_type, card);
}

pub(super) fn handle_card_session_close(
    chats: &HashMap<String, ChatHandle>,
    agent: &Agent,
    fc: &FeishuClient,
    card: &CardActionMsg,
) {
    super::waiting::clear_on_idle_close(
        agent,
        fc,
        &persisted_chat_type(chats, agent, &card.chat_id),
        super::waiting::IdleCardClick {
            action: summon_core::feishu::CardAction::from_action(
                card.value
                    .get("action")
                    .and_then(|v| v.as_str())
                    .unwrap_or(""),
            ),
            from_idle_card: matches!(CardOrigin::from_value(&card.value), CardOrigin::Idle(_)),
            operator_open_id: &card.operator_open_id,
            is_group: card_chat_is_group(chats, agent, &card.chat_id),
            chat_id: &card.chat_id,
        },
    );
    match fc.recall_message(&card.message_id) {
        Ok(()) => {
            if let Some(handle) = chats.get(&card.chat_id) {
                let _ = handle.tx.send(ThreadMsg::ForgetSessionCard {
                    card_id: card.message_id.clone(),
                });
            }
        }
        Err(error) => log(&summon_i18n::format!(
            "r18a7c679fd",
            agent.name,
            card.message_id
        )),
    }
}

pub(super) fn handle_card_set_effort(
    chats: &mut HashMap<String, ChatHandle>,
    agent: &Agent,
    fc: &FeishuClient,
    card: &CardActionMsg,
    default_layer: bool,
) {
    let chat_id = &card.chat_id;
    // 写本群覆盖要用真实的 chat tag:handle 不在内存(daemon 重启、会话已退)时回退盘上 `.meta`,
    // 不能当私聊算出 p<hash>,否则写进一个没有会话的目录、回执却说成功。
    let chat_type = resolve_chat_type(chats, agent, chat_id);
    // 鉴权在 dispatcher 集中查表完成:本群按钮按本群范围、设为默认按 agent 范围。
    let inherit = !default_layer
        && card
            .value
            .get("inherit")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
    let effort = card
        .value
        .get("effort")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .unwrap_or_default()
        .to_string();
    if effort.is_empty() && !inherit {
        let _ = send_markdown_archived(
            fc,
            &agent.state_dir(),
            &chat_type,
            chat_id,
            summon_i18n::text!("r3e92000abf"),
        );
        return;
    }
    let target = match (default_layer, inherit) {
        (true, _) => LayerTarget::Default,
        (false, true) => LayerTarget::ChatInherit,
        (false, false) => LayerTarget::Chat,
    };
    if set_effort_for_chat(
        chats, agent, fc, chat_id, &chat_type, &effort, None, target, true,
    ) {
        refresh_config_panel(chats, agent, fc, &chat_type, card);
    }
}

/// `/effort` 与面板按钮共用的思考档位切换:校验 → 写本群覆盖 / 清掉覆盖 / 写 agent.yaml →
/// 本群生效值变了才按 CLI 热切策略投递 → 回执。返回是否写进了配置。`ChatInherit` 时
/// `effort` 不用,回到的是默认档位。`echoes_input` 同 [`set_model_for_chat`]。
#[allow(clippy::too_many_arguments)] // 群、锚点、层次各自独立
pub(super) fn set_effort_for_chat(
    chats: &mut HashMap<String, ChatHandle>,
    agent: &Agent,
    fc: &FeishuClient,
    chat_id: &str,
    chat_type: &str,
    effort: &str,
    anchor: Option<&str>,
    target: LayerTarget,
    echoes_input: bool,
) -> bool {
    let reply = |text: &str| {
        let _ = send_markdown_archived_anchored(
            fc,
            &agent.state_dir(),
            chat_type,
            chat_id,
            anchor,
            text,
        );
    };
    let tag = chat_tag(chat_type, chat_id);
    let effective = crate::runtime::chat_model::effective(agent, Some(&tag));
    // 写前 fail-closed 校验 (model, effort) 在该 cli 上合法(与 model 同一 core 接缝)。
    if target != LayerTarget::ChatInherit {
        if let Err(e) =
            summon_core::model_effort::validate(agent.cli_kind(), effective.model(), Some(effort))
        {
            reply(&summon_i18n::format!("rfacbc4ff5a"));
            return false;
        }
    }
    let written = match target {
        LayerTarget::Chat => crate::runtime::chat_model::set_chat_effort(agent, &tag, Some(effort)),
        LayerTarget::ChatInherit => crate::runtime::chat_model::set_chat_effort(agent, &tag, None),
        LayerTarget::Default => crate::setup::bind::set_effort(&agent.manifest_path(), effort),
    };
    if let Err(e) = written {
        log(&summon_i18n::format!("r59ce107749", agent.name));
        reply(&summon_i18n::format!("rfacbc4ff5a"));
        return false;
    }
    log(&summon_i18n::format!("r2acd36b13a", agent.name));
    let scope_note =
        (target == LayerTarget::Default).then(|| default_scope_note(agent, &tag, false));
    // 本群生效值:改默认而本群有自己的档位时不变,不注入。
    let new_effort = match target {
        LayerTarget::Chat => Some(effort.to_string()),
        LayerTarget::ChatInherit => crate::runtime::chat_model::effective(agent, None).effort,
        LayerTarget::Default
            if effective.effort_layer == crate::runtime::chat_model::Layer::Chat =>
        {
            reply(&summon_i18n::format!(
                "r09d627d7df",
                effective.effort().unwrap_or_default(),
                scope_note.unwrap_or_default()
            ));
            return true;
        }
        LayerTarget::Default => Some(effort.to_string()),
    };
    let Some(new_effort) = new_effort else {
        reply(&inherit_to_cli_default_note(agent, " effort "));
        return true;
    };
    let mut plan = plan_config_switch(
        CliDriver::from_kind(agent.cli_kind()).effort_switch_spec(agent.capture_mode()),
        chats.contains_key(chat_id),
        &target.set_note(" effort "),
        &new_effort,
    );
    if let Some(note) = &scope_note {
        plan.append_note(note);
    }
    if !target.receipt_only_confirms(echoes_input) {
        plan.keep_text();
    }
    dispatch_config_switch(chats, agent, fc, chat_id, chat_type, plan, anchor);
    true
}

/// 面板「改工作目录」手打表单入口。**A(隐私)后群面板已移除该手打表单**——改工作目录一律经私信
/// 选择器(`pick_workdir` → `set_workdir_to`),路径只进私信。此 handler 仅为**在途遗留卡**(部署前
/// 已发出、仍带手打表单的旧面板卡)兜底,不再由新卡触发。逻辑:operate-gate → 取表单
/// `form_value.workdir` → 写**本会话** cwd 覆盖(`runtime/chats/<session>.cwd`)。
/// **成功回执不带路径**(C:群里只说「已更新」,路径不进群),与私信选择器 `set_workdir_to` 一致。
pub(super) fn handle_card_set_workdir(
    chats: &mut HashMap<String, ChatHandle>,
    agent: &Agent,
    fc: &FeishuClient,
    card: &CardActionMsg,
) {
    let chat_id = &card.chat_id;
    // chat_type 走盘上事实兜底:handle 被「清理/重置」摘除后仍要认出这是群,否则 tag 从 g… 变
    // p…,回执归档与面板刷新都落到幽灵 tag 上。
    let chat_type = resolve_chat_type(chats, agent, chat_id);
    // 鉴权在 dispatcher 集中查表完成(T4:set_workdir → operate 档)。
    let dir = card
        .value
        .get("form_value")
        .and_then(|f| f.get("workdir"))
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let dir = match dir {
        Some(d) => d.to_string(),
        None => {
            let _ = send_markdown_archived(
                fc,
                &agent.state_dir(),
                &chat_type,
                chat_id,
                summon_i18n::text!("r4820edcc9c"),
            );
            return;
        }
    };
    // 会话不在线**不是**拒绝理由:写 `.cwd` 是纯盘操作,下次冷启 `effective_work_dir` 自会读到。
    // 只有盘上也无记录(从没来过消息的陌生 chat)才拒绝。
    let Some(session) = resolve_chat_session(chats, agent, chat_id) else {
        let _ = send_markdown_archived(
            fc,
            &agent.state_dir(),
            &chat_type,
            chat_id,
            summon_i18n::text!("rc12607cd4b"),
        );
        return;
    };
    // 校验/写盘 apply_chat_workdir(A5:与 /cd 同一实现);成功返回展示路径(仅用于日志,不进群回执)。
    if let Some(shown) = apply_chat_workdir(agent, fc, &chat_type, chat_id, &session, &dir) {
        // 注入 /cd 到运行中的 claude tmux 会话(Claude 原生支持),热切换不杀进程。
        // 注意:注入要等当前轮结束才被 claude 消费(与 model/effort 切换同行为),
        // 故回执文案说「当前回复完成后生效」,避免用户误以为没生效。
        let expanded = summon_core::registry::expand_tilde(&dir);
        match chats.get(chat_id) {
            Some(h) => {
                let slash = format!("/cd {expanded}");
                log(&summon_i18n::format!("r987d18876a", agent.name));
                // C:群里回执**只说「已更新」不带路径**(与私信选择器 set_workdir_to 一致)。
                if let Err(e) = h.tx.send(ThreadMsg::SwitchApply {
                    slash: Some(slash),
                    receipt: CommandAck::Done(summon_i18n::text!("r4d898b8b3b").to_string()),
                    anchor: None,
                    confirm_model: None,
                }) {
                    log(&summon_i18n::format!("r50adb55429", agent.name));
                }
            }
            None => log(&summon_i18n::format!("r584662976b", agent.name)),
        }
        // 刷新面板卡:工作目录改后表单回填当场更新(model/effort 不动,由 refresh 正常解析)。
        refresh_config_panel(chats, agent, fc, &chat_type, card);
    }
}

/// 读一个 sibling agent 目录的默认 work_dir(`agent.yaml` 的 `runtime.working_dir`,已 expand_tilde)。
/// 解析失败/缺字段 → None(坏 manifest 不阻塞聚合)。
fn read_sibling_default_work_dir(agent_dir: &std::path::Path) -> Option<String> {
    let content = std::fs::read_to_string(agent_dir.join("agent.yaml")).ok()?;
    serde_yaml::from_str::<Agent>(&content).ok()?.working_dir()
}

/// 读一个 `.cwd` 文件(单行已展开路径)。缺 / 空 → None。
fn read_cwd_file(path: &std::path::Path) -> Option<String> {
    let s = std::fs::read_to_string(path).ok()?;
    let v = s.trim();
    (!v.is_empty()).then(|| v.to_string())
}

/// 扫盘聚合 work_dir 候选素材(**单 daemon 可读全 fleet 盘上事实,无网络、无文件系统全树扫描**):
/// 遍历 `~/.summon/agents/*/`,取每个 agent 的默认 work_dir + `runtime/chats/*.cwd`,按「同群其他
/// agent / 本 agent / fleet」分档喂给 `rank_work_dir_candidates` 排序去重。
///
/// **同群判定**:同一 chat_id 在各 agent 下的 tmux 会话名都以 `-<tag>` 结尾(base 含 agent 名、
/// tag 只由 chat_id 派生),故 `.cwd` 文件名以 `-<current_tag>` 结尾 = 同群其他 agent 正用的目录。
/// `current_session` 是本会话自己的 `.cwd`(= 当前目录),跳过不作候选;`current_dir` 亦排除
/// (不推荐「切到已在的地方」)。**只列 fleet 内部盘上目录,绝不扫 home / 其他系统用户路径。**
pub(super) fn scan_fleet_work_dir_options(
    self_agent: &Agent,
    current_tag: &str,
    current_session: &str,
    current_dir: Option<&str>,
) -> Vec<summon_core::work_dir::WorkDirOption> {
    use summon_core::work_dir::{WorkDirBucket, WorkDirOption};
    let mut options = Vec::new();
    let suffix = format!("-{current_tag}");
    let current_norm = current_dir.map(|d| d.trim_end_matches('/').to_string());
    let mut push = |path: String, bucket: WorkDirBucket| {
        // 排除当前生效目录本身(切到已在的地方无意义)。
        if let Some(cur) = &current_norm {
            if path.trim_end_matches('/') == cur.as_str() {
                return;
            }
        }
        let exists = std::path::Path::new(&path).is_dir();
        options.push(WorkDirOption::new(path, bucket, exists));
    };
    let Some(agents_dir) = self_agent.dir.parent() else {
        return options;
    };
    let Ok(entries) = std::fs::read_dir(agents_dir) else {
        return options;
    };
    for entry in entries.flatten() {
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        let Some(agent_name) = dir.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        let is_self = agent_name == self_agent.name;
        // 该 agent 的默认 work_dir(本 agent → SelfAgent,其余 → Fleet 垫底)。
        if let Some(def) = read_sibling_default_work_dir(&dir) {
            let bucket = if is_self {
                WorkDirBucket::SelfAgent
            } else {
                WorkDirBucket::Fleet
            };
            push(def, bucket);
        }
        // 该 agent 的 per-chat `.cwd`。**状态目录拆分后 runtime/ 归 state_dir 管**,不能再拼
        // `dir.join("runtime")` —— 那是 authored 目录。这里没有现成的 `Agent` 可用(扫盘只拿到
        // 目录名),所以直接问 resolver 要路径,它自带新旧布局回退。
        let state = summon_core::registry::agent_state_dir(agents_dir, agent_name);
        let Ok(cwds) = std::fs::read_dir(state.join("runtime").join("chats")) else {
            continue;
        };
        for c in cwds.flatten() {
            let p = c.path();
            let Some(session) = p
                .file_name()
                .and_then(|s| s.to_str())
                .and_then(|s| s.strip_suffix(".cwd"))
            else {
                continue;
            };
            // 本会话自己的 .cwd = 当前目录,跳过。
            if is_self && session == current_session {
                continue;
            }
            let Some(path) = read_cwd_file(&p) else {
                continue;
            };
            let same_chat = session.ends_with(&suffix);
            let bucket = if is_self {
                WorkDirBucket::SelfAgent
            } else if same_chat {
                WorkDirBucket::SameChatOtherAgent
            } else {
                WorkDirBucket::Fleet
            };
            push(path, bucket);
        }
    }
    options
}

/// 群面板「私信候选目录」按钮:聚合 fleet 候选 → 私信给操作者一张选择器卡。**路径不进群**——
/// 候选只出现在私信里；空集或私信失败时，原会话给出「/cd 手打」提示。
/// operate-gate 已在 dispatcher 集中判(pick_workdir → operate,群 chat 语义)。
pub(super) fn handle_card_pick_workdir(
    chats: &HashMap<String, ChatHandle>,
    agent: &Agent,
    fc: &FeishuClient,
    card: &CardActionMsg,
) {
    let chat_id = &card.chat_id; // 群 chat(pick 按钮在群面板上)
    let chat_type = resolve_chat_type(chats, agent, chat_id);
    let tag = chat_tag(&chat_type, chat_id);
    let session = resolve_chat_session(chats, agent, chat_id);
    let session_name = session.clone().unwrap_or_default();
    let current_dir = session
        .as_deref()
        .and_then(|s| effective_work_dir(agent, s, Some(chat_id)));
    let options = scan_fleet_work_dir_options(agent, &tag, &session_name, current_dir.as_deref());
    let ranked = summon_core::work_dir::rank_work_dir_candidates(options);
    // 折叠 home:候选路径即便只进私信也不露 /Users/<人>。
    let displays: Vec<(String, &'static str)> = ranked
        .iter()
        .map(|r| {
            (
                summon_core::registry::collapse_tilde(&r.path),
                r.bucket.hint(),
            )
        })
        .collect();
    if displays.is_empty() {
        // 空集降级:群里只提示手打,不列任何路径。
        let _ = send_markdown_archived(
            fc,
            &agent.state_dir(),
            &chat_type,
            chat_id,
            summon_i18n::text!("r21423e4c80"),
        );
        return;
    }
    let items: Vec<summon_core::feishu::WorkDirPickItem> = displays
        .iter()
        .map(|(p, h)| summon_core::feishu::WorkDirPickItem { path: p, hint: h })
        .collect();
    let current_display = current_dir
        .as_deref()
        .map(summon_core::registry::collapse_tilde);
    let dm = summon_core::feishu::card_workdir_picker(
        chat_id,
        &chat_type,
        current_display.as_deref(),
        &items,
    );
    // D:主动私聊卡标注来源群(owner 并发收多群私信时认得出是哪个群)。群名解析不到退 chat_id 尾段。
    let note = summon_core::feishu::source_group_note(
        fc.resolve_source_group(chat_id).as_deref(),
        chat_id,
    );
    let dm = summon_core::feishu::card_with_source_note(&dm, &note);
    // 主动私聊操作者。飞书不保证主动 p2p 可达(见 send_card_to_open_id 注释)→ 失败降级群里提示手打。
    match send_card_to_open_id_archived_id(
        fc,
        &agent.state_dir(),
        &card.operator_open_id,
        "workdir_picker",
        &dm,
    ) {
        Ok(_) => {
            log(&summon_i18n::format!(
                "r2473b3d3e3",
                agent.name,
                items.len(),
                card.operator_open_id
            ));
        }
        Err(e) => {
            log(&summon_i18n::format!("r61d1b4f8a5", agent.name));
            let _ = send_markdown_archived(
                fc,
                &agent.state_dir(),
                &chat_type,
                chat_id,
                summon_i18n::text!("r8b8dc7a16a"),
            );
        }
    }
}

/// 私信候选卡的 operate 闸,也是这张卡唯一的一道:卡在私聊里,改的却是目标会话 → 按**目标
/// chat** 的名单与群语义判(与召回卡对目标群硬编码 is_group 同一防线)。
fn set_workdir_to_allowed(
    agent: &Agent,
    operator_open_id: &str,
    operator_union_id: &str,
    target_chat_id: &str,
    target_chat_type: &str,
) -> bool {
    agent.can_operate(
        operator_open_id,
        operator_union_id,
        matches!(target_chat_type, "group" | "topic_group"),
        OperateScope::Chat(target_chat_id),
    )
}

/// 私信选择器候选按钮:写**目标 chat**(value 带 `target_chat_id`/`target_chat_type`,tag 解不回)
/// 的 `.cwd`。破坏性 → 集中闸豁免,由 `set_workdir_to_allowed` 按目标 chat 判。所有回执发到本私信,
/// 注入目标会话的 /cd receipt **不含路径**——群里只见「已更新」,路径不进群。
pub(super) fn handle_card_set_workdir_to(
    chats: &HashMap<String, ChatHandle>,
    agent: &Agent,
    fc: &FeishuClient,
    card: &CardActionMsg,
) {
    let dm_id = &card.chat_id; // 私信 p2p(候选卡所在)
    let dm_type = "p2p";
    let target_chat_id = match card.value.get("target_chat_id").and_then(|v| v.as_str()) {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => {
            let _ = send_markdown_archived(
                fc,
                &agent.state_dir(),
                dm_type,
                dm_id,
                summon_i18n::text!("r1fe31457de"),
            );
            return;
        }
    };
    let target_chat_type = card
        .value
        .get("target_chat_type")
        .and_then(|v| v.as_str())
        .unwrap_or("group")
        .to_string();
    let path = match card
        .value
        .get("path")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(p) => p.to_string(),
        None => {
            let _ = send_markdown_archived(
                fc,
                &agent.state_dir(),
                dm_type,
                dm_id,
                summon_i18n::text!("rbf3ce117dd"),
            );
            return;
        }
    };
    let op_union = fc.resolve_union_id(&card.operator_open_id);
    if !set_workdir_to_allowed(
        agent,
        &card.operator_open_id,
        &op_union,
        &target_chat_id,
        &target_chat_type,
    ) {
        let _ = send_markdown_archived(
            fc,
            &agent.state_dir(),
            dm_type,
            dm_id,
            summon_i18n::text!("r9cf64c9319"),
        );
        return;
    }
    // 解析目标会话(纯盘操作,不需活着的 chat/tmux)。
    let Some(session) = resolve_chat_session(chats, agent, &target_chat_id) else {
        let _ = send_markdown_archived(
            fc,
            &agent.state_dir(),
            dm_type,
            dm_id,
            summon_i18n::text!("r6e39b87018"),
        );
        return;
    };
    // 写 .cwd + 存在性校验:错误回执发到**本私信**(路径不进群);session=目标会话名。
    if let Some(shown) = apply_chat_workdir(agent, fc, dm_type, dm_id, &session, &path) {
        // 注入 /cd 到目标运行会话:receipt **不含路径**——群里只见「已更新」。
        let expanded = summon_core::registry::expand_tilde(&path);
        if let Some(h) = chats.get(&target_chat_id) {
            let slash = format!("/cd {expanded}");
            if let Err(e) = h.tx.send(ThreadMsg::SwitchApply {
                slash: Some(slash),
                receipt: CommandAck::Done(summon_i18n::text!("r4d898b8b3b").to_string()),
                anchor: None,
                confirm_model: None,
            }) {
                log(&summon_i18n::format!("r798123b776", agent.name));
            }
        }
        // 私信确认:路径只在私信里显示。D:标注来源群(改的是哪个群的会话),owner 并发时不混。
        let confirm = summon_i18n::format!("r214586112e");
        let note = summon_core::feishu::source_group_note(
            fc.resolve_source_group(&target_chat_id).as_deref(),
            &target_chat_id,
        );
        let confirm = summon_core::feishu::card_markdown_with_note(&confirm, &note);
        let _ = send_card_archived(
            fc,
            &agent.state_dir(),
            dm_type,
            dm_id,
            None,
            "workdir_set_receipt",
            &confirm,
        );
        log(&format!(
            "[{}] set_workdir_to → {expanded} target_chat={target_chat_id}",
            agent.name
        ));
    }
}

/// 恢复卡片「恢复」：callback 必须精确命中 durable batch 的 scope/agent/chat/card/message，且
/// 原发送者或群 operate 用户才可 claim。claim 先落 pending replay，再进易失 dispatcher。
#[allow(clippy::too_many_arguments)]
pub(super) fn handle_card_recover(
    chats: &HashMap<String, ChatHandle>,
    inbox_wal: &crate::runtime::inbox_wal::InboxWal,
    self_tx: &Sender<DispatchMsg>,
    agent: &Agent,
    fc: &FeishuClient,
    storage_scope: &FeishuStorageScope,
    bridge_store: &crate::bridge::store::BridgeStore,
    card: &CardActionMsg,
) -> bool {
    let Some(batch_id) = card.value.get("batch").and_then(|value| value.as_str()) else {
        return true;
    };
    let mid = match card.value.get("mid").and_then(|v| v.as_str()) {
        Some(m) if !m.trim().is_empty() => m.trim().to_string(),
        _ => return true,
    };
    let batch = match inbox_wal.load_recovery_batch(batch_id) {
        Ok(Some(batch)) => batch,
        Ok(None) => {
            finalize_card(agent, fc, chats, card, summon_i18n::text!("r1fecf2135c"));
            return true;
        }
        Err(error) => {
            log(&summon_i18n::format!("rfcc76dcefd", agent.name));
            let _ = send_markdown_archived(
                fc,
                &agent.state_dir(),
                "group",
                &card.chat_id,
                summon_i18n::text!("r1714c92e76"),
            );
            return false;
        }
    };
    let exact = batch.scope == storage_scope.key
        && batch.agent == agent.name
        && batch.chat_id == card.chat_id
        && matches!(batch.chat_type.as_str(), "p2p" | "group" | "topic_group")
        && batch.card_message_id.as_deref() == Some(card.message_id.as_str());
    let Some(item) = batch
        .items
        .iter()
        .find(|item| item.record.inbound.message_id == mid)
    else {
        let _ = send_markdown_archived(
            fc,
            &agent.state_dir(),
            &batch.chat_type,
            &card.chat_id,
            summon_i18n::text!("r078e29bb07"),
        );
        return true;
    };
    let op_union = fc.resolve_union_id(&card.operator_open_id);
    // 鉴权判定下沉 core(`summon_core::authz`):这里只做 IO(查 union_id)+ 一行调用,矩阵由
    // core 的表驱动测试钉死。**dispatcher 中央闸门对 recover 是 CardGate::None**,这行就是
    // 这条动作仅有的鉴权。
    let authorized = summon_core::authz::recovery_card_authorized(
        agent,
        &batch.chat_type,
        &batch.chat_id,
        summon_core::authz::FeishuUser::new(&card.operator_open_id, &op_union),
        &[summon_core::authz::FeishuUser::new(
            &item.record.inbound.sender,
            &item.record.inbound.sender_union_id,
        )],
        summon_core::authz::RecoveryAction::Recover,
    );
    if !exact || !authorized {
        let _ = send_markdown_archived(
            fc,
            &agent.state_dir(),
            &batch.chat_type,
            &card.chat_id,
            summon_i18n::text!("r5076b2dc84"),
        );
        return true;
    }
    let target =
        summon_core::bridge::ScopedChatKey::new(&batch.scope, &batch.agent, &batch.chat_id);
    match bridge_slot_message(bridge_store, &target) {
        Ok(None) => {}
        Ok(Some(_)) => {
            let _ = send_markdown_archived(
                fc,
                &agent.state_dir(),
                &batch.chat_type,
                &batch.chat_id,
                summon_i18n::text!("r6ae7d83669"),
            );
            return true;
        }
        Err(error) => {
            log(&summon_i18n::format!("rb77d40f97a", agent.name));
            return false;
        }
    }
    match inbox_wal.claim_recovery(
        batch_id,
        &storage_scope.key,
        &agent.name,
        &card.chat_id,
        &card.message_id,
        &mid,
    ) {
        Ok(crate::runtime::inbox_wal::RecoveryClaimOutcome::Claimed(record)) => {
            let queued = self_tx.send(DispatchMsg::Inbound(record.inbound)).is_ok();
            let receipt = if queued {
                summon_i18n::text!("rf61596a61e")
            } else {
                summon_i18n::text!("rc176e36812")
            };
            let _ = send_markdown_archived(
                fc,
                &agent.state_dir(),
                &batch.chat_type,
                &batch.chat_id,
                receipt,
            );
        }
        Ok(crate::runtime::inbox_wal::RecoveryClaimOutcome::AlreadyClaimed) => {
            let _ = send_markdown_archived(
                fc,
                &agent.state_dir(),
                &batch.chat_type,
                &batch.chat_id,
                summon_i18n::text!("r45c449a5fe"),
            );
        }
        Ok(crate::runtime::inbox_wal::RecoveryClaimOutcome::NotAwaiting) => {}
        Err(error) => {
            log(&summon_i18n::format!("r39b1f28cee", agent.name));
            return false;
        }
    }
    refresh_recovery_card(inbox_wal, agent, fc, chats, card, batch_id);
    true
}

/// U4:恢复卡原地刷新——本 chat 还剩待恢复项则重渲染恢复卡(余项按钮仍可点),清空则替换成
/// 终态文案。`just_recovered`=刚被用户点了「恢复」的那条(mid, preview),渲染成置灰「已恢复 ✓」
/// 给即时反馈;**绝不扫 consumed/ 全量**(那里面有 7 天历史消息,会把不相关的旧对话也列出来)。
/// 原卡 message_id 来自 CardActionMsg;失败 log-ignore(finalize_card 内有兜底回执)。
fn refresh_recovery_card(
    inbox_wal: &crate::runtime::inbox_wal::InboxWal,
    agent: &Agent,
    fc: &FeishuClient,
    chats: &HashMap<String, ChatHandle>,
    card: &CardActionMsg,
    batch_id: &str,
) {
    let Ok(Some(batch)) = inbox_wal.load_recovery_batch(batch_id) else {
        finalize_card(agent, fc, chats, card, summon_i18n::text!("r11af601836"));
        return;
    };
    let items: Vec<(String, String)> = batch
        .items
        .iter()
        .filter(|item| {
            !matches!(
                item.state,
                crate::runtime::inbox_wal::RecoveryItemState::Dismissed
            )
        })
        .map(|item| (item.record.inbound.message_id.clone(), item.preview.clone()))
        .collect();
    let recovered_ids = batch.claimed_ids();
    if items.is_empty() {
        finalize_card(agent, fc, chats, card, summon_i18n::text!("r11af601836"));
        return;
    }
    let tag = card
        .value
        .get("chat")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    let refs: Vec<(&str, &str)> = items
        .iter()
        .map(|(m, p)| (m.as_str(), p.as_str()))
        .collect();
    let content = summon_core::feishu::card_crash_recovery(
        batch_id,
        tag,
        batch.stage.as_deref(),
        &refs,
        &recovered_ids,
    );
    let chat_type = batch.chat_type;
    if let Err(e) = update_card_redacted_archived(
        fc,
        &agent.state_dir(),
        &chat_type,
        &card.chat_id,
        &card.message_id,
        &content,
    ) {
        log(&summon_i18n::format!("rdd87be58af", agent.name));
    }
}

/// 恢复卡片「全部忽略」:把本 chat 下入站 WAL 里所有 pending 标记已消费(不再恢复)。
pub(super) fn handle_card_dismiss_recovery(
    chats: &HashMap<String, ChatHandle>,
    inbox_wal: &crate::runtime::inbox_wal::InboxWal,
    agent: &Agent,
    fc: &FeishuClient,
    storage_scope: &FeishuStorageScope,
    bridge_store: &crate::bridge::store::BridgeStore,
    card: &CardActionMsg,
) -> bool {
    let Some(batch_id) = card.value.get("batch").and_then(|value| value.as_str()) else {
        return true;
    };
    let batch = match inbox_wal.load_recovery_batch(batch_id) {
        Ok(Some(batch)) => batch,
        Ok(None) => {
            finalize_card(agent, fc, chats, card, summon_i18n::text!("r1fecf2135c"));
            return true;
        }
        Err(error) => {
            log(&summon_i18n::format!("rfcc76dcefd", agent.name));
            return false;
        }
    };
    let op_union = fc.resolve_union_id(&card.operator_open_id);
    // 仍待恢复的项的发送者;鉴权判定下沉 core(dismiss 在群里只认 operate 名单,私聊要求整批
    // 都是点击者本人发的——不对称是刻意的,见 `RecoveryAction` 文档)。
    let pending_senders: Vec<summon_core::authz::FeishuUser<'_>> = batch
        .items
        .iter()
        .filter(|item| item.state == crate::runtime::inbox_wal::RecoveryItemState::Awaiting)
        .map(|item| {
            summon_core::authz::FeishuUser::new(
                &item.record.inbound.sender,
                &item.record.inbound.sender_union_id,
            )
        })
        .collect();
    let authorized = summon_core::authz::recovery_card_authorized(
        agent,
        &batch.chat_type,
        &batch.chat_id,
        summon_core::authz::FeishuUser::new(&card.operator_open_id, &op_union),
        &pending_senders,
        summon_core::authz::RecoveryAction::Dismiss,
    );
    if batch.scope != storage_scope.key
        || batch.agent != agent.name
        || batch.chat_id != card.chat_id
        || !matches!(batch.chat_type.as_str(), "p2p" | "group" | "topic_group")
        || batch.card_message_id.as_deref() != Some(card.message_id.as_str())
        || !authorized
    {
        let _ = send_markdown_archived(
            fc,
            &agent.state_dir(),
            &batch.chat_type,
            &card.chat_id,
            summon_i18n::text!("r670c030c0f"),
        );
        return true;
    }
    let target =
        summon_core::bridge::ScopedChatKey::new(&batch.scope, &batch.agent, &batch.chat_id);
    match bridge_slot_message(bridge_store, &target) {
        Ok(None) => {}
        Ok(Some(_)) => {
            let _ = send_markdown_archived(
                fc,
                &agent.state_dir(),
                &batch.chat_type,
                &card.chat_id,
                summon_i18n::text!("r0683a26839"),
            );
            return true;
        }
        Err(error) => {
            log(&summon_i18n::format!("r5d6b568f86", agent.name));
            return false;
        }
    }
    let n = match inbox_wal.dismiss_recovery_batch(
        batch_id,
        &storage_scope.key,
        &agent.name,
        &card.chat_id,
        &card.message_id,
    ) {
        Ok(n) => n,
        Err(error) => {
            log(&summon_i18n::format!("r40e09e42cc", agent.name));
            return false;
        }
    };
    // U4:原卡原地置终态(替代另发一条回执;替换失败时 finalize_card 内退回普通回执)。
    finalize_card(agent, fc, chats, card, &summon_i18n::format!("r4168bed5cd"));
    true
}

/// 处理提问卡片动作(提交答案 / 全部跳过 / 取消)→ Core resolve 闸门(operate-gate)。
/// value 带 `gate_id`(+ 表单的 form_value)。resolve 后给点按人回执。
pub(super) fn handle_gate_card_action(
    core: &crate::runtime::core::Core,
    agent: &Agent,
    fc: &FeishuClient,
    chats: &HashMap<String, ChatHandle>,
    card: &CardActionMsg,
    action: &str,
) {
    use summon_core::core_port::{Actor, Command, CommandOutcome, Intent};
    use summon_core::ids::GateId;
    let chat_id = &card.chat_id;
    let chat_type = card_chat_type(chats, chat_id);
    let is_group = card_chat_is_group(chats, agent, chat_id);
    let op_union = fc.resolve_union_id(&card.operator_open_id);
    let Some(gate_id) = card.value.get("gate_id").and_then(|v| v.as_str()) else {
        // 提问卡的 gate_id 缺失同样走这里:没有闸门就没人可唤醒,只能记日志。
        log(&summon_i18n::format!("r188302c34e", agent.name));
        return;
    };
    let gid = GateId::new(gate_id);
    // resolve 会把 gate 清掉，故必须在提交前保存一份只读权威上下文；Core 自己仍会在同一次
    // submit 内复验 exact chat/message，避免这里的快照被当成授权依据。
    let authoritative = action
        .starts_with("gate_ask_")
        .then(|| core.chat_ask_gate_context(&gid))
        .flatten();
    let authoritative_question_count = authoritative
        .as_ref()
        .map(|context| context.questions.len())
        .unwrap_or(0);
    let permission = action
        .starts_with("gate_perm_")
        .then(|| core.claude_permission_gate_context(&gid))
        .flatten();
    let intent = match action {
        // chat 提问卡:多题一次提交。答案按题序回成 JSON 数组,**未答的位置是 null** ——
        // 允许只答一部分(设计不变量 7),CLI 侧再与它自己问的题配对成 {q,a}。
        "gate_ask_submit" => Intent::QuestionResolve(
            gid,
            collect_chat_ask_answers(&card.value, authoritative_question_count),
        ),
        // 「全部跳过」= 一题没答。空数组让 CLI 把所有题的 a 都填 null,agent 拿到明确的
        // 「用户跳过」而不是无声挂死 —— 这是「不超时」的逃生阀。
        "gate_ask_skip" => Intent::QuestionResolve(gid, "[]".to_string()),
        // Claude 授权卡:答案只有这两种,Core 按闸门种类再校验一次。
        "gate_perm_allow" => Intent::QuestionResolve(gid, "allow".to_string()),
        "gate_perm_deny" => Intent::QuestionResolve(gid, "deny".to_string()),
        // 卡上看不全的请求:取消闸门,hook 不作答,这一步回到终端里的 Claude 对话框。
        "gate_perm_terminal" => Intent::CancelGate(gid),
        "gate_cancel" => Intent::CancelGate(gid),
        other => {
            log(&summon_i18n::format!("rf59b8a7c11", agent.name));
            return;
        }
    };
    let cmd = Command {
        actor: Actor::Feishu(card.operator_open_id.clone()),
        intent,
    };
    let ctx = crate::runtime::core::FeishuGateCtx {
        union_id: op_union,
        is_group,
        chat_id: card.chat_id.clone(),
        message_id: card.message_id.clone(),
    };
    let outcome = core.submit_feishu(cmd, ctx);
    // 授权卡的结局就写在卡上,不再往群里补一句回执。
    if action.starts_with("gate_perm_") {
        match &outcome {
            CommandOutcome::Done => {
                finalize_permission_card(agent, fc, chats, card, permission.as_ref(), action)
            }
            CommandOutcome::GateClosed(_) => {
                finalize_card(agent, fc, chats, card, summon_i18n::text!("r89882c532b"))
            }
            CommandOutcome::Denied(msg) => {
                let _ = send_markdown_archived(
                    fc,
                    &agent.state_dir(),
                    &chat_type,
                    chat_id,
                    &summon_i18n::format!("r46232103d3"),
                );
            }
            other => log(&summon_i18n::format!("r80bf085f87", agent.name)),
        }
        return;
    }
    let reply = match outcome {
        CommandOutcome::Done => {
            // U4:resolve 成功 → 原闸门卡原地置终态(按钮消失),再发继续回执。
            // 提问卡走专门的定格:通用 finalize_card 会把整张卡换成一行字,用户回头就看不到
            // 自己选了什么了 —— 交互记录不该在提交的瞬间蒸发。
            if action.starts_with("gate_ask_") {
                finalize_ask_card(
                    agent,
                    fc,
                    chats,
                    card,
                    action == "gate_ask_skip",
                    authoritative.as_ref(),
                );
            } else {
                finalize_card(agent, fc, chats, card, summon_i18n::text!("r5449a2381f"));
            }
            summon_i18n::text!("ra188e0dd1e").to_string()
        }
        // U4:双击已 resolve 的闸门不是权限问题——中性文案,别用 🔒「被拒」误导成权限错误。
        CommandOutcome::GateClosed(_) => summon_i18n::text!("r0f4f349876").to_string(),
        CommandOutcome::Denied(msg) => summon_i18n::format!("r46232103d3"),
        other => {
            log(&summon_i18n::format!("r443ffb83a3", agent.name));
            summon_i18n::text!("r711743c366").to_string()
        }
    };
    let _ = send_markdown_archived(fc, &agent.state_dir(), &chat_type, chat_id, &reply);
}

#[cfg(test)]
mod model_candidates_tests {
    use super::*;

    /// 动态目录是「这个 ACP 会话此刻真能选的」,比配置桶权威——非空时压过它。
    #[test]
    fn 动态目录非空时压过配置桶() {
        let got = prefer_dynamic_catalog(
            vec!["gpt-5.6-sol-xhigh".into(), "composer-2.5".into()],
            vec!["claude-opus-5-thinking-xhigh".into()],
        );
        assert_eq!(got, vec!["gpt-5.6-sol-xhigh", "composer-2.5"]);
    }

    /// 会话没建立 / 服务端没下发目录时动态目录为空。此时必须回退配置桶——
    /// 返回空会让面板一个模型按钮都不渲染,用户完全没得点(2026-08-12 owner 实测)。
    #[test]
    fn 动态目录为空时回退配置桶() {
        let got = prefer_dynamic_catalog(
            Vec::new(),
            vec![
                "gpt-5.6-sol-xhigh".into(),
                "claude-opus-5-thinking-xhigh".into(),
                "cursor-grok-4.5-high".into(),
            ],
        );
        assert_eq!(got.len(), 3, "空动态目录必须回退到配置桶,而不是返回空");
        assert!(got.contains(&"cursor-grok-4.5-high".to_string()));
    }

    /// 两边都空只能是空——但那是「配置桶也没配」,不是本函数的锅。
    #[test]
    fn 两边都空时才允许为空() {
        assert!(prefer_dynamic_catalog(Vec::new(), Vec::new()).is_empty());
    }

    /// 目录按 agent 分桶,所以每条用例给自己起一个独有名字,互不干扰。
    fn codex_agent(name: &str) -> Agent {
        serde_yaml::from_str(&format!("name: {name}\nruntime:\n  cli: codex\n")).unwrap()
    }

    fn record(agent: &Agent, models: &[&str], default_model: &str) {
        crate::codex::model_catalog::record_for_agent(
            agent,
            crate::codex::codex_app_server::CodexModelCatalog {
                models: models.iter().map(|m| m.to_string()).collect(),
                default_model: Some(default_model.to_string()),
            },
        );
    }

    /// 抓到过目录就以目录为准:上游今天新加的模型,不改配置也能在面板上点到。
    #[test]
    fn 抓到的目录压过配置桶() {
        let agent = codex_agent("panel-probe-fresh");
        record(&agent, &["gpt-6-astra", "gpt-5.5"], "gpt-6-astra");
        let got = model_candidates(&agent, "g-a", vec!["gpt-5.4-mini".into()]);
        assert_eq!(got, vec!["gpt-6-astra", "gpt-5.5"]);
    }

    /// cursor(CLI 通道)的候选按展示顺序重排;codex 保持 backend 原序。
    #[test]
    fn cursor候选按展示顺序重排而codex保持原序() {
        let cursor: Agent = serde_yaml::from_str(
            "name: panel-order-cursor\nruntime:\n  cli: cursor\n  capture: cli\n",
        )
        .unwrap();
        let got = model_candidates(
            &cursor,
            "g-a",
            vec![
                "gpt-5.2".into(),
                "composer-2.5-fast".into(),
                "auto".into(),
                "gpt-5.6-sol-xhigh".into(),
                "composer-2.5".into(),
            ],
        );
        assert_eq!(
            got,
            vec![
                "auto",
                "gpt-5.6-sol-xhigh",
                "composer-2.5",
                "gpt-5.2",
                "composer-2.5-fast"
            ]
        );
        let got = model_candidates(
            &codex_agent("panel-order-codex"),
            "g-a",
            vec!["gpt-5.2".into(), "gpt-5.5".into()],
        );
        assert_eq!(got, vec!["gpt-5.2", "gpt-5.5"]);
    }

    /// daemon 刚起、还没开过任何会话时目录是空的。此时回退配置桶,别让面板一个按钮都没有。
    #[test]
    fn 没抓到目录时回退配置桶() {
        let got = model_candidates(
            &codex_agent("panel-probe-cold"),
            "g-a",
            vec!["gpt-5.5".into()],
        );
        assert_eq!(got, vec!["gpt-5.5"]);
    }

    /// 面板摘要那一行的模型来源:抓到目录后要显示 backend 报的默认,而不是配置桶首项。
    /// 这条链此前零覆盖 —— 整层删掉全量测试也不会红。
    #[test]
    fn 面板摘要的默认模型取自目录并标backend来源() {
        let agent = codex_agent("panel-summary-default");
        record(&agent, &["gpt-6-astra", "gpt-5.5"], "gpt-6-astra");
        let mut msg = summon_core::config::builtin_messaging();
        msg.models_by_cli
            .insert("codex".into(), vec!["gpt-5.4-mini".into()]);

        let resolved = resolve_panel_model(&agent, "g-a", &msg);
        assert_eq!(resolved.value.as_deref(), Some("gpt-6-astra"));
        assert_eq!(
            resolved.source,
            summon_core::model_effort::ModelSource::BackendDefault
        );
    }

    /// 没抓到目录就落回配置桶,来源也要如实标成 summon 这边的默认。
    #[test]
    fn 没有目录时面板摘要落回配置桶并标summon来源() {
        let mut msg = summon_core::config::builtin_messaging();
        msg.models_by_cli
            .insert("codex".into(), vec!["gpt-5.4-mini".into()]);

        let resolved = resolve_panel_model(&codex_agent("panel-summary-cold"), "g-a", &msg);
        assert_eq!(resolved.value.as_deref(), Some("gpt-5.4-mini"));
        assert_eq!(
            resolved.source,
            summon_core::model_effort::ModelSource::SummonDefault
        );
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn ended_archive_delivery_failures_report_committed_archive_state() {
        let (agent, _tmp) = crate::test_support::agent_dir("codex", "");
        let chat_id = format!("oc_delivery_{}", uuid::Uuid::new_v4().simple());
        let session = agent.tmux_name_for_chat("group", &chat_id, false);
        super::super::chat_meta::write_chat_meta(
            &agent, &session, &chat_id, "group", None, "ou_owner", None,
        );
        crate::session::resume_store::write(&agent, &session, "codex", "thread-ended", None, None);
        crate::session::lifecycle::mark_closed(&agent, &chat_tag("group", &chat_id));
        let mut ticket =
            crate::session::ended_archive::prepare(&agent, &session, "group", &chat_id).unwrap();
        ticket.message_id = "om_reply".into();
        ticket.card_json = summon_core::feishu::card_with_exit_archive(
            &summon_core::feishu::card_markdown_v2("结论"),
            "会话已退出",
            &ticket.token,
            false,
        )
        .unwrap();
        crate::session::ended_archive::save(&agent, &ticket).unwrap();
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let base = format!("http://{}", server.server_addr());
        // 并行冷跑时客户端初始化可超过空闲超时；服务寿命必须覆盖整个回执场景。
        let (server_lifetime, lifetime_rx) = std::sync::mpsc::channel::<()>();
        let worker = std::thread::spawn(move || {
            let mut messages = Vec::new();
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
                let mut body = String::new();
                request.as_reader().read_to_string(&mut body).unwrap();
                let status = if request.url().contains("tenant_access_token") {
                    200
                } else {
                    500
                };
                let response = if request.url().contains("tenant_access_token") {
                    r#"{"code":0,"tenant_access_token":"test-token","expire":7200}"#
                } else {
                    messages.push((request.method().as_str().to_string(), body));
                    r#"{"code":999999,"msg":"delivery unavailable"}"#
                };
                request
                    .respond(tiny_http::Response::from_string(response).with_status_code(status))
                    .unwrap();
            }
            messages
        });
        let fc = FeishuClient::with_test_base_connection_close(base);
        let card = CardActionMsg {
            chat_id: chat_id.clone(),
            message_id: ticket.message_id.clone(),
            operator_open_id: "ou_owner".into(),
            event_id: "evt_delivery".into(),
            value: serde_json::json!({"action": "archive_ended", "token": ticket.token}),
            wal_id: None,
            wal_replayed: false,
        };
        handle_archive_ended(
            &HashMap::new(),
            &agent,
            &fc,
            &crate::runtime::inflight::InFlight::new(),
            &card,
        );
        drop(server_lifetime);
        let messages = worker.join().unwrap();
        assert!(
            crate::session::resume_store::read_record_checked(&agent, &session)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            crate::session::resume_store::previous_checked(&agent, &session)
                .unwrap()
                .id,
            "thread-ended"
        );
        assert!(messages
            .iter()
            .any(|(method, body)| method == "PATCH" && body.contains("会话已归档")));
        assert!(
            messages
                .iter()
                .any(|(method, body)| method == "POST" && body.contains("会话已归档")),
            "{messages:?}"
        );
        assert!(
            !messages.iter().any(|(_, body)| body.contains("归档未完成")),
            "{messages:?}"
        );
    }

    #[test]
    fn ended_archive_callback_works_offline_and_refuses_a_current_worker() {
        for online in [false, true] {
            let (agent, _tmp) = crate::test_support::agent_dir("codex", "");
            let chat_id = format!("oc_callback_{}", uuid::Uuid::new_v4().simple());
            let session = agent.tmux_name_for_chat("group", &chat_id, false);
            super::super::chat_meta::write_chat_meta(
                &agent, &session, &chat_id, "group", None, "ou_owner", None,
            );
            crate::session::resume_store::write(
                &agent,
                &session,
                "codex",
                "thread-ended",
                None,
                None,
            );
            crate::session::lifecycle::mark_closed(&agent, &chat_tag("group", &chat_id));
            let mut ticket =
                crate::session::ended_archive::prepare(&agent, &session, "group", &chat_id)
                    .unwrap();
            ticket.message_id = "om_reply".into();
            ticket.card_json = summon_core::feishu::card_with_exit_archive(
                &summon_core::feishu::card_turn_summary("完整结论", Some("角标"), None),
                "会话已退出",
                &ticket.token,
                false,
            )
            .unwrap();
            crate::session::ended_archive::save(&agent, &ticket).unwrap();
            let (tx, rx) = std::sync::mpsc::channel();
            let mut chats = HashMap::new();
            if online {
                chats.insert(
                    chat_id.clone(),
                    ChatHandle {
                        tx,
                        last_activity: std::time::SystemTime::now(),
                        idle_prompted: false,
                        chat_type: "group".into(),
                        tmux_name: session.clone(),
                        tmux_seen_live: false,
                        local_claimed: false,
                        last_inbound_scheduled: false,
                        last_inbound_id: String::new(),
                        turn_was_active: false,
                        subagent_state: None,
                        menu_ledger: None,
                    },
                );
            }
            if !online {
                assert_eq!(
                    super::super::dispatcher::redeliver_orphaned_tasks(
                        &mut chats,
                        &agent,
                        &chat_id,
                        Vec::new(),
                        || panic!("已退出会话的空交接不得建立 worker"),
                    ),
                    Some(0)
                );
            }
            let card = CardActionMsg {
                chat_id: chat_id.clone(),
                message_id: ticket.message_id.clone(),
                operator_open_id: "ou_owner".into(),
                event_id: "evt_archive".into(),
                value: serde_json::json!({"action": "archive_ended", "token": ticket.token}),
                wal_id: None,
                wal_replayed: false,
            };
            let fc = FeishuClient::new("feishu.cn", String::new(), String::new(), true);
            handle_archive_ended(
                &chats,
                &agent,
                &fc,
                &crate::runtime::inflight::InFlight::new(),
                &card,
            );
            assert!(
                rx.try_recv().is_err(),
                "归档回调不提交 provider 轮或 worker 控制消息"
            );
            assert_eq!(chats.len(), usize::from(online));
            assert_eq!(
                crate::session::resume_store::read_record_checked(&agent, &session)
                    .unwrap()
                    .is_some(),
                online
            );
            let audit = std::fs::read_to_string(
                agent
                    .state_dir()
                    .join("threads")
                    .join(chat_tag("group", &chat_id))
                    .join("outbound.ndjson"),
            )
            .unwrap();
            if online {
                assert!(audit.contains("归档未完成"));
            } else {
                assert_eq!(
                    crate::session::resume_store::previous_checked(&agent, &session)
                        .unwrap()
                        .id,
                    "thread-ended"
                );
                assert!(audit.contains("完整结论") && audit.contains("会话已归档"));
                assert!(audit.contains("disabled"));
                handle_archive_ended(
                    &chats,
                    &agent,
                    &fc,
                    &crate::runtime::inflight::InFlight::new(),
                    &card,
                );
                assert_eq!(
                    crate::session::resume_store::previous_checked(&agent, &session)
                        .unwrap()
                        .id,
                    "thread-ended"
                );
            }
        }
    }

    use super::*;
    use serde_json::json;

    /// 私信候选卡按目标会话判:本群 operator 能改自己那个群的目录,改不了别的群的。
    #[test]
    fn set_workdir_to_checks_the_target_chat_not_the_private_chat() {
        let agent: Agent = serde_yaml::from_str(
            "name: a\nfeishu:\n  operators: [ou_owner]\n  chat_operators:\n    oc_shared: [ou_colleague]\n    oc_dm: [ou_other]\n",
        )
        .unwrap();
        assert!(set_workdir_to_allowed(
            &agent,
            "ou_colleague",
            "",
            "oc_shared",
            "group"
        ));
        assert!(!set_workdir_to_allowed(
            &agent,
            "ou_colleague",
            "",
            "oc_elsewhere",
            "group"
        ));
        assert!(
            !set_workdir_to_allowed(&agent, "ou_other", "", "oc_shared", "group"),
            "卡所在私聊的本群名单管不到目标会话"
        );
        assert!(set_workdir_to_allowed(
            &agent,
            "ou_owner",
            "",
            "oc_elsewhere",
            "group"
        ));
    }
    /// 面板的「>_ 终端」跟着捕获方式走:tmux 会话有,无头会话没有;终端服务出不了链接(没起,
    /// 或只听回环)时一律没有。
    #[test]
    fn the_panel_terminal_button_follows_capture_mode_and_the_service() {
        let claude: Agent = serde_yaml::from_str("name: c\nruntime:\n  cli: claude\n").unwrap();
        let codex: Agent = serde_yaml::from_str("name: x\nruntime:\n  cli: codex\n").unwrap();
        assert!(panel_offers_terminal(&claude, true));
        assert!(!panel_offers_terminal(&codex, true));
        assert!(!panel_offers_terminal(&claude, false));
        let model = summon_core::model_effort::ResolvedModel {
            value: None,
            source: summon_core::model_effort::ModelSource::CliDefault,
        };
        let tmp = tempfile::tempdir().unwrap();
        let mut claude = claude;
        claude.dir = tmp.path().join("agents").join(&claude.name);
        std::fs::create_dir_all(claude.state_dir()).unwrap();
        assert!(
            !session_panel_controls(&claude, "p2p-term", "", &model).terminal,
            "测试进程里没有终端服务"
        );
    }

    /// per-chat effort:没设 → 回退 agent.yaml;设了 → 只影响本会话;空串清除覆盖。
    #[test]
    fn effort_pref_is_per_chat_with_agent_yaml_fallback() {
        let tmp = tempfile::tempdir().unwrap();
        let mut agent: Agent =
            serde_yaml::from_str("name: t\neffort: xhigh\nruntime:\n  cli: claude\n").unwrap();
        agent.dir = tmp.path().join("agents").join(&agent.name);
        std::fs::create_dir_all(&agent.dir).unwrap();
        std::fs::create_dir_all(agent.state_dir()).unwrap();

        assert_eq!(
            crate::runtime::chat_model::chat_effort(&agent, "g-a"),
            None,
            "默认无覆盖"
        );
        assert_eq!(
            effort_for(&agent, "g-a").as_deref(),
            Some("xhigh"),
            "无覆盖时回退 agent.yaml"
        );

        crate::runtime::chat_model::set_chat_effort(&agent, "g-a", Some("low")).unwrap();
        assert_eq!(effort_for(&agent, "g-a").as_deref(), Some("low"));
        assert_eq!(
            effort_for(&agent, "g-b").as_deref(),
            Some("xhigh"),
            "per-chat 粒度:别的会话仍用全局值"
        );

        // 空串 = 清除覆盖,回落全局;再清一次幂等。
        crate::runtime::chat_model::set_chat_effort(&agent, "g-a", None).unwrap();
        assert_eq!(effort_for(&agent, "g-a").as_deref(), Some("xhigh"));
        crate::runtime::chat_model::set_chat_effort(&agent, "g-a", None).unwrap();
    }

    /// effort 读不到**回退全局、不 fail-closed** —— 与 `cursor_session_mode` 相反。
    /// 那边读不到必须拒绝启动(plan/ask 关的是权限,悄悄放宽是安全问题);
    /// effort 只是思考档位,退回全局值无害且可预期。
    #[test]
    fn chat_effort_falls_back_instead_of_failing_closed() {
        let tmp = tempfile::tempdir().unwrap();
        let mut agent: Agent =
            serde_yaml::from_str("name: t\neffort: high\nruntime:\n  cli: claude\n").unwrap();
        agent.dir = tmp.path().join("agents").join(&agent.name);
        std::fs::create_dir_all(&agent.dir).unwrap();
        std::fs::create_dir_all(agent.state_dir()).unwrap();
        // 把 effort 这个位置占成目录:读必然失败。
        std::fs::create_dir_all(agent.state_dir().join("threads/g-a/effort")).unwrap();
        assert_eq!(crate::runtime::chat_model::chat_effort(&agent, "g-a"), None);
        assert_eq!(
            effort_for(&agent, "g-a").as_deref(),
            Some("high"),
            "读失败也要回退到全局值,不能整轮拒绝"
        );
    }

    /// 「额度显示」偏好:默认关;开→关幂等往返;per-chat 粒度互不影响。
    #[test]
    fn quota_badge_pref_roundtrip_and_default_off() {
        let tmp = tempfile::tempdir().unwrap();
        let mut agent: Agent = serde_yaml::from_str("name: t\nruntime:\n  cli: claude\n").unwrap();
        agent.dir = tmp.path().join("agents").join(&agent.name);
        std::fs::create_dir_all(&agent.dir).unwrap();
        std::fs::create_dir_all(agent.state_dir()).unwrap();
        assert!(!quota_badge_enabled(&agent, "p2p-a"), "默认必须是关");
        set_quota_badge(&agent, "p2p-a", true).unwrap();
        assert!(quota_badge_enabled(&agent, "p2p-a"));
        assert!(
            !quota_badge_enabled(&agent, "p2p-b"),
            "per-chat 粒度,别的会话不受影响"
        );
        set_quota_badge(&agent, "p2p-a", false).unwrap();
        assert!(!quota_badge_enabled(&agent, "p2p-a"));
        set_quota_badge(&agent, "p2p-a", false).unwrap(); // 再关一次:幂等,不报错
    }

    #[test]
    fn session_panel_controls_use_latest_account_quota_from_another_chat() {
        let _quota_guard = crate::capture::rpc_pool::default_codex_quota_test_guard();
        let tmp = tempfile::tempdir().unwrap();
        let mut agent: Agent = serde_yaml::from_str(
            "name: quota-codex-panel\nmodel: gpt-5.6-sol\nruntime:\n  cli: codex\n",
        )
        .unwrap();
        agent.dir = tmp.path().join("agents").join(&agent.name);
        std::fs::create_dir_all(agent.state_dir()).unwrap();
        set_quota_badge(&agent, "p2p-target", true).unwrap();
        crate::capture::rpc_pool::record_quota_observation_shared(
            &agent.name,
            "g-source",
            44,
            None,
            None,
        );
        let model = summon_core::model_effort::ResolvedModel {
            value: Some("gpt-5.6-sol".into()),
            source: summon_core::model_effort::ModelSource::AgentConfig,
        };

        let controls = session_panel_controls(&agent, "p2p-target", "session-target", &model);
        assert!(controls.quota_badge);
        assert_eq!(controls.quota_left_pct, Some(44));
    }

    /// 默认 Codex 账号的 5 小时观测只记在默认账号名下(rollout 回放没有 chat 身份),面板要读得到。
    #[test]
    fn session_panel_reads_the_default_codex_account_five_hour_window() {
        use summon_core::codex_quota::{CodexRateLimitSnapshot, CodexRateLimitWindow};

        let _quota_guard = crate::capture::rpc_pool::default_codex_quota_test_guard();
        let tmp = tempfile::tempdir().unwrap();
        let mut agent: Agent = serde_yaml::from_str(
            "name: quota-codex-default-five-hour\nmodel: gpt-5.6-sol\nruntime:\n  cli: codex\n",
        )
        .unwrap();
        agent.dir = tmp.path().join("agents").join(&agent.name);
        std::fs::create_dir_all(agent.state_dir()).unwrap();
        assert!(crate::capture::rpc_pool::agent_uses_default_codex_account(
            &agent
        ));
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        crate::capture::rpc_pool::record_codex_quota_snapshot_at(
            None,
            &CodexRateLimitSnapshot {
                limit_id: "codex".into(),
                limit_name: None,
                primary: Some(CodexRateLimitWindow {
                    used_percent: 25.0,
                    window_minutes: Some(300),
                    resets_at: Some(now + 3_600),
                }),
                secondary: Some(CodexRateLimitWindow {
                    used_percent: 10.0,
                    window_minutes: Some(10_080),
                    resets_at: Some(now + 86_400),
                }),
                rate_limit_reached_type: None,
            },
            Some(now),
        );
        let model = summon_core::model_effort::ResolvedModel {
            value: Some("gpt-5.6-sol".into()),
            source: summon_core::model_effort::ModelSource::AgentConfig,
        };

        let controls = session_panel_controls(&agent, "p2p-five", "session-five", &model);
        assert_eq!(controls.quota_5h_left_pct, Some(75));
        assert_eq!(controls.quota_left_pct, Some(90));
    }

    #[test]
    fn custom_codex_account_does_not_read_the_default_rollout_quota() {
        use summon_core::codex_quota::{CodexRateLimitSnapshot, CodexRateLimitWindow};

        let _quota_guard = crate::capture::rpc_pool::default_codex_quota_test_guard();
        let tmp = tempfile::tempdir().unwrap();
        let mut agent: Agent = serde_yaml::from_str(
            "name: isolated-codex-panel\nmodel: gpt-5.6-sol\nruntime:\n  cli: codex\n  env:\n    CODEX_HOME: /tmp/isolated-codex\n",
        )
        .unwrap();
        agent.dir = tmp.path().join("agents").join(&agent.name);
        std::fs::create_dir_all(agent.state_dir()).unwrap();
        crate::capture::rpc_pool::record_codex_quota_snapshot_shared(
            None,
            &CodexRateLimitSnapshot {
                limit_id: "codex".into(),
                limit_name: None,
                primary: Some(CodexRateLimitWindow {
                    used_percent: 12.0,
                    window_minutes: Some(10_080),
                    resets_at: Some(i64::MAX - 3),
                }),
                secondary: None,
                rate_limit_reached_type: None,
            },
        );

        assert_eq!(
            current_session_quota_at(&agent, "g-isolated", "session", None),
            None,
            "自定义 Codex 账号不得读取默认 rollout watcher 的额度"
        );
    }

    #[test]
    fn claude_session_context_joins_resume_id_with_statusline_snapshot() {
        let tmp = tempfile::tempdir().unwrap();
        let mut agent: Agent =
            serde_yaml::from_str("name: context-claude\nruntime:\n  cli: claude\n").unwrap();
        agent.dir = tmp.path().join("agents").join(&agent.name);
        std::fs::create_dir_all(agent.state_dir()).unwrap();
        let session = "soul-context-claude-p2p";
        crate::session::resume_store::write(&agent, session, "claude", "sid-context", None, None);
        let snapshot_root = tmp.path().join("snapshots");
        crate::setup::claude_quota::write_snapshot(
            &snapshot_root,
            &crate::setup::claude_quota::ClaudeQuotaSnapshot {
                session_id: "sid-context".into(),
                transcript_path: None,
                cwd: None,
                five_hour: None,
                seven_day: None,
                model_display_name: None,
                effort_level: None,
                context_used_percentage: Some(42.4),
                context_window_size: Some(1_000_000),
                observed_at: None,
            },
        )
        .unwrap();

        let context = claude_session_context_budget_at(&agent, session, &snapshot_root);
        assert_eq!(context.used_pct(), Some(42));
        assert_eq!(context.used_tokens, Some(420_000));
        assert_eq!(context.window_tokens, Some(1_000_000));
        assert_eq!(
            claude_session_context_budget_at(&agent, "unknown-session", &snapshot_root),
            Default::default(),
            "没有 resume 映射时不得串读别的 Claude 快照"
        );
    }

    #[test]
    fn claude_session_quota_records_both_windows_from_the_statusline_snapshot() {
        let tmp = tempfile::tempdir().unwrap();
        let mut agent: Agent =
            serde_yaml::from_str("name: quota-claude-two-windows\nruntime:\n  cli: claude\n")
                .unwrap();
        agent.dir = tmp.path().join("agents").join(&agent.name);
        std::fs::create_dir_all(agent.state_dir()).unwrap();
        let session = "soul-quota-claude-two-windows";
        crate::session::resume_store::write(&agent, session, "claude", "sid-two", None, None);
        let snapshot_root = tmp.path().join("snapshots");
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        crate::setup::claude_quota::write_snapshot(
            &snapshot_root,
            &crate::setup::claude_quota::ClaudeQuotaSnapshot {
                session_id: "sid-two".into(),
                transcript_path: None,
                cwd: None,
                five_hour: Some(crate::setup::claude_quota::ClaudeQuotaWindow {
                    used_percentage: 30.0,
                    resets_at: now + 3_600,
                }),
                seven_day: Some(crate::setup::claude_quota::ClaudeQuotaWindow {
                    used_percentage: 20.0,
                    resets_at: now + 86_400,
                }),
                model_display_name: None,
                effort_level: None,
                context_used_percentage: None,
                context_window_size: None,
                observed_at: Some(now),
            },
        )
        .unwrap();

        assert_eq!(
            current_session_quota_at(&agent, "g-two", session, Some(&snapshot_root)),
            Some(80)
        );
        assert_eq!(
            five_hour_quota_for(&agent).map(|(left_pct, _)| left_pct),
            Some(70)
        );
    }

    /// 同一 reset 窗口内,面板取 observed_at 更晚的那份快照:额度被刷新后剩余量回升也照样跟上,
    /// 迟到的旧快照不能把值倒回去。
    #[test]
    fn claude_session_quota_follows_the_latest_snapshot_in_the_same_window() {
        let tmp = tempfile::tempdir().unwrap();
        let mut agent: Agent =
            serde_yaml::from_str("name: quota-claude-refill\nruntime:\n  cli: claude\n").unwrap();
        agent.dir = tmp.path().join("agents").join(&agent.name);
        std::fs::create_dir_all(agent.state_dir()).unwrap();
        let session = "soul-quota-claude-refill";
        crate::session::resume_store::write(&agent, session, "claude", "sid-refill", None, None);
        let snapshot_root = tmp.path().join("snapshots");
        let write = |used: f64, observed_at: i64| {
            crate::setup::claude_quota::write_snapshot(
                &snapshot_root,
                &crate::setup::claude_quota::ClaudeQuotaSnapshot {
                    session_id: "sid-refill".into(),
                    transcript_path: None,
                    cwd: None,
                    five_hour: None,
                    seven_day: Some(crate::setup::claude_quota::ClaudeQuotaWindow {
                        used_percentage: used,
                        resets_at: 1_756_882_800,
                    }),
                    model_display_name: None,
                    effort_level: None,
                    context_used_percentage: None,
                    context_window_size: None,
                    observed_at: Some(observed_at),
                },
            )
            .unwrap()
        };
        let quota =
            || current_session_quota_at(&agent, "p2p-refill", session, Some(&snapshot_root));

        write(69.0, 1_000);
        assert_eq!(quota(), Some(31));
        write(1.0, 2_000);
        assert_eq!(quota(), Some(99));
        write(69.0, 1_000);
        assert_eq!(quota(), Some(99), "迟到的旧快照不能把账号值倒回去");
    }

    #[test]
    fn claude_session_quota_promotes_snapshot_into_account_memory() {
        let tmp = tempfile::tempdir().unwrap();
        let mut agent: Agent =
            serde_yaml::from_str("name: quota-claude-panel\nruntime:\n  cli: claude\n").unwrap();
        agent.dir = tmp.path().join("agents").join(&agent.name);
        std::fs::create_dir_all(agent.state_dir()).unwrap();
        let session = "soul-quota-claude-p2p";
        crate::session::resume_store::write(&agent, session, "claude", "sid-quota", None, None);
        let snapshot_root = tmp.path().join("snapshots");
        crate::setup::claude_quota::write_snapshot(
            &snapshot_root,
            &crate::setup::claude_quota::ClaudeQuotaSnapshot {
                session_id: "sid-quota".into(),
                transcript_path: None,
                cwd: None,
                five_hour: None,
                seven_day: Some(crate::setup::claude_quota::ClaudeQuotaWindow {
                    used_percentage: 14.0,
                    resets_at: 123,
                }),
                model_display_name: None,
                effort_level: None,
                context_used_percentage: None,
                context_window_size: None,
                observed_at: None,
            },
        )
        .unwrap();

        assert_eq!(
            current_session_quota_at(&agent, "p2p-source", session, Some(&snapshot_root),),
            Some(86)
        );
        assert_eq!(
            crate::capture::rpc_pool::current_quota_shared(&agent.name, "g-other"),
            Some(86),
            "session 卡读到的 Claude 快照应进入账号级内存"
        );
    }

    #[test]
    fn session_panel_cot_uses_per_chat_overrides_over_agent_defaults() {
        let tmp = tempfile::tempdir().unwrap();
        let mut agent: Agent = serde_yaml::from_str(
            "name: cot-panel\nruntime:\n  cli: claude\nmessaging:\n  hide_thinking: true\n  cot_visible: true\n  cot_tool_details: false\n",
        )
        .unwrap();
        agent.dir = tmp.path().join("agents").join(&agent.name);
        std::fs::create_dir_all(&agent.dir).unwrap();
        std::fs::create_dir_all(agent.state_dir()).unwrap();
        std::fs::write(
            agent.dir.join("agent.yaml"),
            "name: cot-panel\nruntime:\n  cli: claude\nmessaging:\n  hide_thinking: true\n  cot_visible: true\n  cot_tool_details: false\n",
        )
        .unwrap();
        let session = "soul-cot-panel";
        for (id, value) in [
            (summon_core::settings::SettingKey::CotVisible, false),
            (summon_core::settings::SettingKey::CotThinking, true),
        ] {
            crate::runtime::daemon::chat_meta::set_chat_setting(&agent, session, id, Some(value))
                .unwrap();
        }
        let model = summon_core::model_effort::ResolvedModel {
            value: None,
            source: summon_core::model_effort::ModelSource::CliDefault,
        };

        let controls = session_panel_controls(&agent, "p2p-cot", session, &model);
        assert!(!controls.cot.visible);
        assert!(controls.cot.thinking);
        assert!(!controls.cot.tools, "未覆盖 tools 应继承 agent 默认 false");
    }

    /// 角标档位取的是**会被下发的那个值**:本会话覆盖 > agent.yaml;没配就缺项(不猜 CLI 默认档),
    /// 没有 effort 轴的 CLI 恒缺项(cursor 的档位烤在模型名里,agent.yaml 里写了也不会被下发)。
    #[test]
    fn badge_effort_takes_the_value_that_actually_ships() {
        let tmp = tempfile::tempdir().unwrap();
        let make = |name: &str, yaml: &str| {
            let mut agent: Agent = serde_yaml::from_str(yaml).unwrap();
            agent.dir = tmp.path().join("agents").join(name);
            std::fs::create_dir_all(&agent.dir).unwrap();
            std::fs::create_dir_all(agent.state_dir()).unwrap();
            std::fs::write(agent.dir.join("agent.yaml"), yaml).unwrap();
            agent
        };

        let codex = make(
            "badge-effort-codex",
            "name: badge-effort-codex\nruntime:\n  cli: codex\neffort: high\n",
        );
        assert_eq!(badge_effort(&codex, "g-a").as_deref(), Some("high"));
        crate::runtime::chat_model::set_chat_effort(&codex, "g-a", Some("xhigh")).unwrap();
        assert_eq!(
            badge_effort(&codex, "g-a").as_deref(),
            Some("xhigh"),
            "本会话覆盖优先"
        );
        assert_eq!(
            badge_effort(&codex, "g-b").as_deref(),
            Some("high"),
            "覆盖只作用于设它的那个会话"
        );

        let bare = make(
            "badge-effort-bare",
            "name: badge-effort-bare\nruntime:\n  cli: codex\n",
        );
        assert_eq!(
            badge_effort(&bare, "g-a"),
            None,
            "没配 effort 时 CLI 用自己的默认档,summon 不知道那是什么"
        );

        let cursor = make(
            "badge-effort-cursor",
            "name: badge-effort-cursor\nruntime:\n  cli: cursor\neffort: high\n",
        );
        assert_eq!(
            badge_effort(&cursor, "g-a"),
            None,
            "cursor 无 effort 轴:配了也不会下发,角标不得渲这个值"
        );
    }

    /// 面板角标行的档位开关态是四层合并后的**生效值**:出厂开、agent.yaml 能关、per-chat 再翻回来。
    /// 面板画错等于用户按着一个与实际相反的按钮。
    #[test]
    fn session_panel_badge_effort_shows_the_effective_value() {
        let yaml = "name: badge-effort-panel\nruntime:\n  cli: claude\nmessaging:\n  badge_effort: false\n";
        let tmp = tempfile::tempdir().unwrap();
        let mut agent: Agent = serde_yaml::from_str(yaml).unwrap();
        agent.dir = tmp.path().join("agents").join(&agent.name);
        std::fs::create_dir_all(&agent.dir).unwrap();
        std::fs::create_dir_all(agent.state_dir()).unwrap();
        std::fs::write(agent.dir.join("agent.yaml"), yaml).unwrap();
        let session = "soul-badge-effort-panel";
        let model = summon_core::model_effort::ResolvedModel {
            value: None,
            source: summon_core::model_effort::ModelSource::CliDefault,
        };

        let controls = session_panel_controls(&agent, "p2p-badge", session, &model);
        assert!(!controls.badge_effort, "agent.yaml 关掉时面板应显示关");

        crate::runtime::daemon::chat_meta::set_chat_setting(
            &agent,
            session,
            summon_core::settings::SettingKey::BadgeEffort,
            Some(true),
        )
        .unwrap();
        let controls = session_panel_controls(&agent, "p2p-badge", session, &model);
        assert!(controls.badge_effort, "per-chat 覆盖优先于 agent 默认");
    }

    /// 角标另外三项同理:出厂开、agent.yaml 关掉哪项面板就显示哪项关、per-chat 再翻回来,
    /// 互不牵连。
    #[test]
    fn session_panel_badge_items_show_the_effective_values() {
        use summon_core::settings::SettingKey;
        let yaml =
            "name: badge-items-panel\nruntime:\n  cli: claude\nmessaging:\n  badge_model: false\n";
        let tmp = tempfile::tempdir().unwrap();
        let mut agent: Agent = serde_yaml::from_str(yaml).unwrap();
        agent.dir = tmp.path().join("agents").join(&agent.name);
        std::fs::create_dir_all(&agent.dir).unwrap();
        std::fs::create_dir_all(agent.state_dir()).unwrap();
        std::fs::write(agent.dir.join("agent.yaml"), yaml).unwrap();
        let session = "soul-badge-items-panel";
        let model = summon_core::model_effort::ResolvedModel {
            value: None,
            source: summon_core::model_effort::ModelSource::CliDefault,
        };
        let state = |c: &summon_core::feishu::SessionPanelControls| {
            [c.badge_cli, c.badge_model, c.badge_effort, c.badge_ctx]
        };

        let controls = session_panel_controls(&agent, "p2p-badge", session, &model);
        assert_eq!(state(&controls), [true, false, true, true]);

        for (key, value) in [
            (SettingKey::BadgeModel, true),
            (SettingKey::BadgeCli, false),
            (SettingKey::BadgeCtx, false),
        ] {
            crate::runtime::daemon::chat_meta::set_chat_setting(&agent, session, key, Some(value))
                .unwrap();
        }
        let controls = session_panel_controls(&agent, "p2p-badge", session, &model);
        assert_eq!(state(&controls), [false, true, true, false]);
    }

    #[test]
    fn cursor_context_snapshot_is_wired_into_session_and_completion_assemblers() {
        let source = include_str!("cards.rs");
        let session_start = source
            .find("fn current_session_context_budget(")
            .expect("Session 上下文装配器");
        let session_end = source[session_start..]
            .find("\n}\n\n/// 面板控制项")
            .expect("Session 上下文装配器收口");
        assert!(source[session_start..session_start + session_end].contains("rpc_context_budget("));

        let rpc_start = source
            .find("pub(super) fn rpc_context_budget(")
            .expect("RPC 上下文装配器");
        let rpc_end = source[rpc_start..]
            .find("\n}\n\npub(super) fn current_session_context_budget")
            .expect("RPC 上下文装配器收口");
        let rpc_body = &source[rpc_start..rpc_start + rpc_end];
        assert!(rpc_body.contains("CliDriver::Cursor"));
        assert!(rpc_body.contains("cursor_context_budget(agent_name, tag)"));

        let chat_source = include_str!("chat_thread.rs");
        let completion = chat_source
            .find("fn completion_badge(")
            .expect("完成角标装配器");
        let completion_end = chat_source[completion..]
            .find("\n}\n\n/// 一轮正常结束后")
            .expect("完成角标装配器收口");
        assert!(chat_source[completion..completion + completion_end]
            .contains("apply_rpc_context_budget("));
    }

    #[test]
    fn current_session_context_reads_persistent_acp_store_when_pool_is_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let mut agent: Agent = serde_yaml::from_str(
            "name: cursor-acp-panel-test\nruntime:\n  cli: cursor\n  capture: rpc\n",
        )
        .unwrap();
        agent.dir = tmp.path().join("agents").join(&agent.name);
        std::fs::create_dir_all(agent.state_dir()).unwrap();
        let session = "soul-cursor-acp-panel-test";
        let tag = "g-panel";
        crate::session::resume_store::write(
            &agent,
            session,
            &agent.resume_cli_kind(),
            "sid-acp-panel",
            None,
            None,
        );
        crate::capture::cursor_context::write_test_acp_snapshot(
            &agent
                .state_dir()
                .join(crate::capture::cursor_config::CURSOR_ACP_SESSIONS_DIR_NAME),
            "sid-acp-panel",
            64_000,
            256_000,
        );
        assert_eq!(
            current_session_context_budget(&agent, tag, session),
            summon_core::context_budget::ContextBudget::from_tokens(64_000, Some(256_000))
        );
    }

    #[test]
    fn cursor_context_requires_an_authoritative_snapshot() {
        let budget = cursor_context_budget("cursor-context-snapshot-only", "g-test");
        assert_eq!(budget, Default::default());
    }

    #[test]
    fn session_cot_card_handler_delivers_exact_axes_to_live_worker() {
        let tmp = tempfile::tempdir().unwrap();
        let mut agent: Agent =
            serde_yaml::from_str("name: cot-handler\nruntime:\n  cli: claude\n").unwrap();
        agent.dir = tmp.path().join("agents").join(&agent.name);
        std::fs::create_dir_all(&agent.dir).unwrap();
        std::fs::write(
            agent.dir.join("agent.yaml"),
            "name: cot-handler\nruntime:\n  cli: claude\n",
        )
        .unwrap();
        let fc = FeishuClient::new("feishu.cn", String::new(), String::new(), true);
        let (tx, rx) = std::sync::mpsc::channel();
        let mut chats = HashMap::from([(
            "oc_cot".to_string(),
            ChatHandle {
                tx,
                last_activity: std::time::SystemTime::now(),
                idle_prompted: false,
                chat_type: "group".into(),
                tmux_name: "soul-cot-handler".into(),
                tmux_seen_live: false,
                local_claimed: false,
                last_inbound_scheduled: false,
                last_inbound_id: String::new(),
                turn_was_active: false,
                subagent_state: None,
                menu_ledger: None,
            },
        )]);
        let mut card = CardActionMsg {
            chat_id: "oc_cot".into(),
            message_id: "om_panel".into(),
            operator_open_id: "ou_owner".into(),
            event_id: "evt_cot".into(),
            value: serde_json::json!({"action":"session_cot","axis":"visible","on":false}),
            wal_id: None,
            wal_replayed: false,
        };

        handle_card_session_cot(&mut chats, &agent, &fc, &card);
        match rx.recv().unwrap() {
            ThreadMsg::CotApply {
                visible,
                thinking,
                tools,
            } => {
                assert_eq!(visible, Some(false));
                assert_eq!(thinking, None);
                assert_eq!(tools, None);
            }
            _ => panic!("第一条必须把 visible 变更交给 worker"),
        }
        assert!(matches!(
            rx.recv().unwrap(),
            ThreadMsg::RenderSubagentPanelSnapshot { .. }
        ));

        card.value = serde_json::json!({"action":"session_cot","axis":"thinking","on":true});
        handle_card_session_cot(&mut chats, &agent, &fc, &card);
        match rx.recv().unwrap() {
            ThreadMsg::CotApply {
                visible,
                thinking,
                tools,
            } => {
                assert_eq!(visible, Some(false), "连续切换必须保留上一轴");
                assert_eq!(thinking, Some(true));
                assert_eq!(tools, None);
            }
            _ => panic!("第二条必须把 thinking 变更交给 worker"),
        }
        let persisted =
            crate::runtime::daemon::chat_meta::read_chat_settings(&agent, "soul-cot-handler");
        assert_eq!(
            persisted.get(summon_core::settings::SettingKey::CotVisible),
            Some(false)
        );
        assert_eq!(
            persisted.get(summon_core::settings::SettingKey::CotThinking),
            Some(true)
        );
    }

    /// 面板角标行各开关(CLI / 模型 / 档位 / 上下文用量):各落各的 per-chat 分级开关,并且
    /// **不给在跑的那一轮发任何指令** —— 它们下一轮才生效,当轮的角标在轮初就定了
    /// (见 `badge_seed_for`)。
    #[test]
    fn badge_card_handler_persists_per_chat_without_touching_the_live_turn() {
        let tmp = tempfile::tempdir().unwrap();
        let mut agent: Agent =
            serde_yaml::from_str("name: badge-effort-handler\nruntime:\n  cli: claude\n").unwrap();
        agent.dir = tmp.path().join("agents").join(&agent.name);
        std::fs::create_dir_all(&agent.dir).unwrap();
        std::fs::write(
            agent.dir.join("agent.yaml"),
            "name: badge-effort-handler\nruntime:\n  cli: claude\n",
        )
        .unwrap();
        let fc = FeishuClient::new("feishu.cn", String::new(), String::new(), true);
        let (tx, rx) = std::sync::mpsc::channel();
        let mut chats = HashMap::from([(
            "oc_badge".to_string(),
            ChatHandle {
                tx,
                last_activity: std::time::SystemTime::now(),
                idle_prompted: false,
                chat_type: "group".into(),
                tmux_name: "soul-badge-effort-handler".into(),
                tmux_seen_live: false,
                local_claimed: false,
                last_inbound_scheduled: false,
                last_inbound_id: String::new(),
                turn_was_active: false,
                subagent_state: None,
                menu_ledger: None,
            },
        )]);
        let card = CardActionMsg {
            chat_id: "oc_badge".into(),
            message_id: "om_panel".into(),
            operator_open_id: "ou_owner".into(),
            event_id: "evt_badge".into(),
            value: serde_json::json!({"action":"badge_effort","on":false}),
            wal_id: None,
            wal_replayed: false,
        };

        handle_card_badge_item(
            &mut chats,
            &agent,
            &fc,
            &card,
            summon_core::settings::SettingKey::BadgeEffort,
        );
        let persisted = crate::runtime::daemon::chat_meta::read_chat_settings(
            &agent,
            "soul-badge-effort-handler",
        );
        assert_eq!(
            persisted.get(summon_core::settings::SettingKey::BadgeEffort),
            Some(false),
            "关态必须落到 per-chat 分级开关上"
        );
        // 唯一发给 worker 的消息是面板重绘;不得夹带任何改变当前轮的指令。
        assert!(matches!(
            rx.recv().unwrap(),
            ThreadMsg::RenderSubagentPanelSnapshot { .. }
        ));
        assert!(
            rx.try_recv().is_err(),
            "角标行的档位开关不得通知在跑的那一轮"
        );

        // 同一个处理函数接另外三项:各写各的键,不碰别的项,也不通知在跑的那一轮。
        for (action, key) in [
            ("badge_cli", summon_core::settings::SettingKey::BadgeCli),
            ("badge_model", summon_core::settings::SettingKey::BadgeModel),
            ("badge_ctx", summon_core::settings::SettingKey::BadgeCtx),
        ] {
            let card = CardActionMsg {
                value: serde_json::json!({"action": action, "on": false}),
                ..card.clone()
            };
            handle_card_badge_item(&mut chats, &agent, &fc, &card, key);
            let persisted = crate::runtime::daemon::chat_meta::read_chat_settings(
                &agent,
                "soul-badge-effort-handler",
            );
            assert_eq!(persisted.get(key), Some(false), "{action}");
            assert!(matches!(
                rx.recv().unwrap(),
                ThreadMsg::RenderSubagentPanelSnapshot { .. }
            ));
            assert!(rx.try_recv().is_err(), "{action} 不得通知在跑的那一轮");
        }
        let persisted = crate::runtime::daemon::chat_meta::read_chat_settings(
            &agent,
            "soul-badge-effort-handler",
        );
        assert_eq!(
            persisted.get(summon_core::settings::SettingKey::BadgeEffort),
            Some(false),
            "别的项保持原样"
        );
    }

    #[test]
    fn cursor_session_mode_pref_is_per_chat_strict_and_offline_safe() {
        let tmp = tempfile::tempdir().unwrap();
        let mut agent: Agent =
            serde_yaml::from_str("name: cursor-mode-pref-unit\nruntime:\n  cli: cursor\n").unwrap();
        agent.dir = tmp.path().join("agents").join(&agent.name);
        std::fs::create_dir_all(&agent.dir).unwrap();
        std::fs::create_dir_all(agent.state_dir()).unwrap();
        let plan: summon_core::driver::CursorSessionMode = "plan".parse().unwrap();

        assert_eq!(cursor_session_mode(&agent, "p2p-a").unwrap(), None);
        apply_cursor_session_mode(&agent, "p2p-a", &plan).unwrap();
        assert_eq!(
            cursor_session_mode(&agent, "p2p-a").unwrap(),
            Some(plan.clone())
        );
        assert_eq!(cursor_session_mode(&agent, "p2p-b").unwrap(), None);
        assert_eq!(
            std::fs::read_to_string(agent.state_dir().join("threads/p2p-a/cursor_mode")).unwrap(),
            "plan"
        );

        let dynamic: summon_core::driver::CursorSessionMode = "review-v2".parse().unwrap();
        assert!(
            apply_cursor_session_mode(&agent, "p2p-a", &dynamic)
                .unwrap_err()
                .to_string()
                .contains("尚未建立"),
            "没有 live catalog 时未知 id 必须 fail-closed"
        );
        assert_eq!(cursor_session_mode(&agent, "p2p-a").unwrap(), Some(plan));

        std::fs::write(
            agent.state_dir().join("threads/p2p-a/cursor_mode"),
            "two words",
        )
        .unwrap();
        let error = cursor_session_mode(&agent, "p2p-a")
            .unwrap_err()
            .to_string();
        assert_eq!(
            error,
            ::summon_core::i18n::label(CURSOR_MODE_READ_PUBLIC_ERROR)
        );
        assert!(!error.contains(&tmp.path().display().to_string()));
    }

    #[test]
    fn cursor_session_mode_rejects_non_cursor_rpc_without_writing() {
        // 用 codex(同样走 Rpc,但不是 cursor)验证「CLI 不对就拒」。
        // 原来这里用的是「cursor 但非 rpc」,而 2026-08-20 之后 cursor 恒 Rpc,那个组合不存在了。
        let tmp = tempfile::tempdir().unwrap();
        let mut agent: Agent =
            serde_yaml::from_str("name: cursor-mode-wrong-cli\nruntime:\n  cli: codex\n").unwrap();
        agent.dir = tmp.path().join("agents").join(&agent.name);
        std::fs::create_dir_all(&agent.dir).unwrap();
        std::fs::create_dir_all(agent.state_dir()).unwrap();
        let ask: summon_core::driver::CursorSessionMode = "ask".parse().unwrap();
        assert!(apply_cursor_session_mode(&agent, "p2p-a", &ask).is_err());
        assert!(!cursor_session_mode_path(&agent, "p2p-a").exists());
    }

    #[test]
    fn cursor_session_panel_separates_offline_pending_from_live_effective_mode() {
        let tmp = tempfile::tempdir().unwrap();
        let mut agent: Agent =
            serde_yaml::from_str("name: cursor-mode-panel-offline-unit\nruntime:\n  cli: cursor\n")
                .unwrap();
        agent.dir = tmp.path().join("agents").join(&agent.name);
        std::fs::create_dir_all(&agent.dir).unwrap();
        std::fs::create_dir_all(agent.state_dir()).unwrap();
        let plan: summon_core::driver::CursorSessionMode = "plan".parse().unwrap();
        apply_cursor_session_mode(&agent, "p2p-panel-offline", &plan).unwrap();
        let model = summon_core::model_effort::ResolvedModel {
            value: None,
            source: summon_core::model_effort::ModelSource::CliDefault,
        };

        let control = session_panel_controls(&agent, "p2p-panel-offline", "", &model)
            .cursor_mode
            .unwrap();
        assert_eq!(control.effective, None);
        assert_eq!(control.pending.as_deref(), Some("plan"));
        assert!(!control.preference_error);
        assert!(!control.catalog_live);
    }

    #[test]
    fn cursor_session_panel_marks_unreadable_preference_without_claiming_effective() {
        let tmp = tempfile::tempdir().unwrap();
        let mut agent: Agent =
            serde_yaml::from_str("name: cursor-mode-panel-error-unit\nruntime:\n  cli: cursor\n")
                .unwrap();
        agent.dir = tmp.path().join("agents").join(&agent.name);
        std::fs::create_dir_all(agent.state_dir().join("threads/p2p-panel-error")).unwrap();
        std::fs::write(
            agent
                .state_dir()
                .join("threads/p2p-panel-error/cursor_mode"),
            "bad mode",
        )
        .unwrap();
        let model = summon_core::model_effort::ResolvedModel {
            value: None,
            source: summon_core::model_effort::ModelSource::CliDefault,
        };

        let control = session_panel_controls(&agent, "p2p-panel-error", "", &model)
            .cursor_mode
            .unwrap();
        assert!(control.preference_error);
        assert_eq!(control.effective, None);
        assert_eq!(control.pending, None);
    }

    #[test]
    fn cursor_parameter_panel_keeps_offline_intent_honest_and_fails_closed_on_bad_bundle() {
        let tmp = tempfile::tempdir().unwrap();
        let mut agent: Agent = serde_yaml::from_str(
            "name: cursor-parameter-panel-offline-unit\nruntime:\n  cli: cursor\n  capture: rpc\n  rpc_model: \"gpt-5.6-sol[context=1m,reasoning=xhigh,fast=true]\"\n",
        )
        .unwrap();
        agent.dir = tmp.path().join("agents").join(&agent.name);
        std::fs::create_dir_all(&agent.dir).unwrap();
        std::fs::create_dir_all(agent.state_dir()).unwrap();
        let model = summon_core::model_effort::ResolvedModel {
            value: Some("gpt-5.6-sol".into()),
            source: summon_core::model_effort::ModelSource::AgentConfig,
        };

        let offline = session_panel_controls(&agent, "p2p-parameter-offline", "", &model)
            .cursor_config
            .unwrap();
        assert!(!offline.catalog_live);
        assert!(
            offline.model_pending,
            "只有意图、没有 live 会话时不能伪装已生效"
        );
        assert!(!offline.preference_error);
        assert!(offline.controls.is_empty(), "没有动态目录就不能猜参数按钮");

        std::fs::write(
            agent
                .state_dir()
                .join(crate::capture::cursor_config::CURSOR_CONFIG_BUNDLE_FILE),
            "{not-json",
        )
        .unwrap();
        let broken = session_panel_controls(&agent, "p2p-parameter-offline", "", &model)
            .cursor_config
            .unwrap();
        assert!(broken.preference_error);
        assert!(!broken.catalog_live);
        assert!(broken.controls.is_empty());
    }

    #[test]
    fn cursor_parameter_card_allowlist_never_auto_opens_future_server_axes() {
        for open in ["context", "reasoning", "effort", "thinking", "fast"] {
            assert!(cursor_parameter_axis_is_open(open));
        }
        for closed in ["model", "mode", "mcp", "approval", "network", "terminal"] {
            assert!(!cursor_parameter_axis_is_open(closed));
        }
    }

    #[test]
    fn cursor_mode_error_choices_are_bounded() {
        let available: Vec<crate::capture::cursor_acp::CursorAcpMode> = (0..100)
            .map(|index| crate::capture::cursor_acp::CursorAcpMode {
                id: format!("mode-{index}-{}", "x".repeat(48)),
                name: format!("Mode {index}"),
                description: None,
            })
            .collect();

        let choices = cursor_mode_choices_for_error(&available).unwrap();
        assert!(choices.chars().count() <= CURSOR_MODE_ERROR_CHOICES_CHAR_CAP);
        assert!(choices.contains("另有 94 个"));
        assert!(!choices.contains("mode-99-"), "尾部目录不能无界进入群消息");
    }

    #[test]
    fn cursor_mode_persistence_error_is_stable_and_redacted() {
        let tmp = tempfile::tempdir().unwrap();
        let mut agent: Agent = serde_yaml::from_str(
            "name: cursor-mode-io-redaction-unit\nruntime:\n  cli: cursor\n  capture: rpc\n",
        )
        .unwrap();
        agent.dir = tmp.path().join("agents").join(&agent.name);
        std::fs::create_dir_all(&agent.dir).unwrap();
        std::fs::create_dir_all(agent.state_dir()).unwrap();
        std::fs::write(agent.state_dir().join("threads"), "not-a-directory").unwrap();
        let plan: summon_core::driver::CursorSessionMode = "plan".parse().unwrap();

        let error = apply_cursor_session_mode(&agent, "p2p-io-error", &plan)
            .unwrap_err()
            .to_string();
        assert_eq!(
            error,
            ::summon_core::i18n::label(CURSOR_MODE_PERSISTENCE_PUBLIC_ERROR)
        );
        assert!(!error.contains(&tmp.path().display().to_string()));
        assert!(!error.contains("cursor_mode"));
    }

    /// Fast 是 per-chat 的显式选择；旧会话没有文件时保持 unmanaged，不替用户猜默认值。
    #[test]
    fn codex_service_tier_pref_isolated_and_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let mut agent: Agent = serde_yaml::from_str("name: t\nruntime:\n  cli: codex\n").unwrap();
        agent.dir = tmp.path().join("agents").join(&agent.name);
        std::fs::create_dir_all(&agent.dir).unwrap();
        std::fs::create_dir_all(agent.state_dir()).unwrap();

        assert_eq!(codex_service_tier(&agent, "p2p-a"), None);
        set_codex_service_tier(&agent, "p2p-a", summon_core::driver::CodexServiceTier::Fast)
            .unwrap();
        assert_eq!(
            codex_service_tier(&agent, "p2p-a"),
            Some(summon_core::driver::CodexServiceTier::Fast)
        );
        assert_eq!(
            codex_service_tier(&agent, "p2p-b"),
            None,
            "另一个 chat 不得继承 Fast"
        );

        // 同值重复写必须幂等；随后切回标准档也应覆盖完整文件。
        set_codex_service_tier(&agent, "p2p-a", summon_core::driver::CodexServiceTier::Fast)
            .unwrap();
        set_codex_service_tier(
            &agent,
            "p2p-a",
            summon_core::driver::CodexServiceTier::Standard,
        )
        .unwrap();
        assert_eq!(
            codex_service_tier(&agent, "p2p-a"),
            Some(summon_core::driver::CodexServiceTier::Standard)
        );
        assert_eq!(
            std::fs::read_to_string(agent.state_dir().join("threads/p2p-a/codex_service_tier"))
                .unwrap(),
            "default"
        );
    }

    /// 坏值不传播到 turn/start；严格解析也避免把 Codex 内部名 `priority` 当持久化契约。
    #[test]
    fn codex_service_tier_bad_value_degrades_to_unmanaged() {
        let tmp = tempfile::tempdir().unwrap();
        let mut agent: Agent = serde_yaml::from_str("name: t\nruntime:\n  cli: codex\n").unwrap();
        agent.dir = tmp.path().join("agents").join(&agent.name);
        std::fs::create_dir_all(&agent.dir).unwrap();
        std::fs::create_dir_all(agent.state_dir()).unwrap();
        let path = agent.state_dir().join("threads/p2p-a/codex_service_tier");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();

        for bad in ["priority", "fast\n", "FAST", ""] {
            std::fs::write(&path, bad).unwrap();
            assert_eq!(
                codex_service_tier(&agent, "p2p-a"),
                None,
                "坏值 {bad:?} 必须降级为 unmanaged"
            );
        }
    }

    /// 卡片入口与盘上格式共用同一 canonical 集合；非法 mode 在进入写盘分支前就被拒绝。
    #[test]
    fn codex_service_tier_card_mode_is_strict() {
        assert_eq!(
            card_codex_service_tier(&json!({"mode": "default"})),
            Some(summon_core::driver::CodexServiceTier::Standard)
        );
        assert_eq!(
            card_codex_service_tier(&json!({"mode": "fast"})),
            Some(summon_core::driver::CodexServiceTier::Fast)
        );
        for value in [
            json!({}),
            json!({"mode": null}),
            json!({"mode": true}),
            json!({"mode": "priority"}),
            json!({"mode": "fast\n"}),
            json!({"mode": "FAST"}),
        ] {
            assert_eq!(card_codex_service_tier(&value), None, "value={value}");
        }

        let tmp = tempfile::tempdir().unwrap();
        let mut agent: Agent = serde_yaml::from_str("name: t\nruntime:\n  cli: codex\n").unwrap();
        agent.dir = tmp.path().join("agents").join(&agent.name);
        std::fs::create_dir_all(&agent.dir).unwrap();
        std::fs::create_dir_all(agent.state_dir()).unwrap();
        set_codex_service_tier(&agent, "p2p-a", summon_core::driver::CodexServiceTier::Fast)
            .unwrap();
        assert_eq!(card_codex_service_tier(&json!({"mode": "priority"})), None);
        assert_eq!(
            codex_service_tier(&agent, "p2p-a"),
            Some(summon_core::driver::CodexServiceTier::Fast),
            "非法 mode 不得改写既有偏好"
        );
    }

    /// dispatcher 的 agent 是启动时快照；Fast 能力检查必须以刚写入 manifest 的模型为准。
    #[test]
    fn codex_fast_check_rereads_the_current_manifest_model() {
        let tmp = tempfile::tempdir().unwrap();
        let mut stale: Agent =
            serde_yaml::from_str("name: t\nmodel: gpt-5.6-sol\nruntime:\n  cli: codex\n").unwrap();
        stale.dir = tmp.path().join("agents").join(&stale.name);
        std::fs::create_dir_all(&stale.dir).unwrap();
        // agent.yaml 是 **authored** 文件,住在 agent.dir 里(不是 state_dir)——状态目录拆分后
        // 这两者分家了,写 manifest 必须跟着 dir 走。
        std::fs::write(
            stale.dir.join("agent.yaml"),
            "name: t\nmodel: gpt-5.4-mini\nruntime:\n  cli: codex\n",
        )
        .unwrap();

        let current = reread_for_codex_speed_action(&stale).unwrap();
        assert_eq!(current.model(), Some("gpt-5.4-mini"));

        std::fs::write(stale.dir.join("agent.yaml"), "not: [valid").unwrap();
        assert!(reread_for_codex_speed_action(&stale).is_none());
    }

    /// 文字命令与卡片必须共用同一条 apply 路径：关 Fast 即使 manifest 暂时不可重读也要能
    /// 显式写 Standard；开 Fast 在支持性未知时 fail-closed，且不得覆盖刚写好的 Standard。
    #[test]
    fn codex_service_tier_apply_keeps_off_safe_and_on_fail_closed() {
        let tmp = tempfile::tempdir().unwrap();
        let mut agent: Agent = serde_yaml::from_str("name: t\nruntime:\n  cli: codex\n").unwrap();
        agent.dir = tmp.path().join("agents").join(&agent.name);
        std::fs::create_dir_all(&agent.dir).unwrap();
        std::fs::create_dir_all(agent.state_dir()).unwrap();

        let refreshed = apply_codex_service_tier(
            &agent,
            "p2p-a",
            summon_core::driver::CodexServiceTier::Standard,
        )
        .expect("关闭 Fast 不依赖 manifest 重读成功");
        assert!(refreshed.is_none());
        assert_eq!(
            codex_service_tier(&agent, "p2p-a"),
            Some(summon_core::driver::CodexServiceTier::Standard)
        );

        let error =
            apply_codex_service_tier(&agent, "p2p-a", summon_core::driver::CodexServiceTier::Fast)
                .expect_err("缺 manifest/模型目录证据时不得猜成支持 Fast");
        assert!(error.to_string().contains("无法"));
        assert_eq!(
            codex_service_tier(&agent, "p2p-a"),
            Some(summon_core::driver::CodexServiceTier::Standard),
            "Fast 校验失败不得改写既有 Standard"
        );

        persist_checked_codex_service_tier(
            &agent,
            "p2p-a",
            summon_core::driver::CodexServiceTier::Fast,
            Some(true),
        )
        .expect("模型目录明确支持时应写入 Fast");
        assert_eq!(
            codex_service_tier(&agent, "p2p-a"),
            Some(summon_core::driver::CodexServiceTier::Fast)
        );
    }

    /// Fast 能力按本群生效模型判,与 TurnCtx 同一个模型:默认支持、本群模型不支持时降为 Standard、
    /// 开 Fast 被拒;反过来本群模型支持时照发 Fast、开得了。
    #[test]
    fn codex_fast_follows_the_chats_own_model() {
        use summon_core::driver::CodexServiceTier;
        let support = |model: Option<&str>| match model {
            Some("gpt-fast") => Some(true),
            Some("gpt-slow") => Some(false),
            _ => None,
        };
        for (default, chat, fast) in [
            ("gpt-fast", "gpt-slow", false),
            ("gpt-slow", "gpt-fast", true),
        ] {
            let (agent, _dir) =
                crate::test_support::agent_dir("codex", &format!("model: {default}\n"));
            crate::runtime::chat_model::set_chat_model(&agent, "g-a", Some(chat)).unwrap();
            set_codex_service_tier(&agent, "g-a", CodexServiceTier::Fast).unwrap();
            assert_eq!(
                codex_service_tier_for_turn_with(&agent, "g-a", support),
                Some(if fast {
                    CodexServiceTier::Fast
                } else {
                    CodexServiceTier::Standard
                }),
                "default={default} chat={chat}"
            );
            set_codex_service_tier(&agent, "g-a", CodexServiceTier::Standard).unwrap();
            let applied =
                apply_codex_service_tier_with(&agent, "g-a", CodexServiceTier::Fast, support);
            assert_eq!(applied.is_ok(), fast, "default={default} chat={chat}");
        }
    }

    /// 已持久 Fast 后模型能力漂移，必须显式 Standard 清 sticky；None 会继续沿用旧 Fast。
    #[test]
    fn unsupported_or_unknown_fast_preference_downgrades_to_explicit_standard() {
        use summon_core::driver::CodexServiceTier;

        assert_eq!(effective_codex_service_tier(None, None), None);
        assert_eq!(
            effective_codex_service_tier(Some(CodexServiceTier::Standard), None),
            Some(CodexServiceTier::Standard)
        );
        assert_eq!(
            effective_codex_service_tier(Some(CodexServiceTier::Fast), Some(true)),
            Some(CodexServiceTier::Fast)
        );
        for support in [Some(false), None] {
            assert_eq!(
                effective_codex_service_tier(Some(CodexServiceTier::Fast), support),
                Some(CodexServiceTier::Standard)
            );
        }
    }

    /// 时刻默认值的分钟**必须是 00**。
    ///
    /// `picker_time` 两列独立滚动:默认给「此刻」(如 17:48)时,用户滚完小时列就以为选好了,
    /// 分钟列还停在 48,于是 12:00 悄悄变成 12:48。真机第一次用就中了(SCH-6)。
    #[test]
    fn default_time_slot_always_lands_on_the_hour() {
        use chrono::{TimeZone, Timelike};
        let at = |h, m| {
            chrono::Local
                .with_ymd_and_hms(2026, 8, 3, h, m, 30)
                .unwrap()
        };
        for (h, m) in [(17, 48), (9, 1), (0, 0), (12, 59)] {
            let slot = default_time_slot(at(h, m));
            assert_eq!(slot.minute(), 0, "{h}:{m} 的默认值分钟必须是 00");
            assert_eq!(slot.second(), 0);
        }
        // 下一个整点(不是当前整点):一次性定时器的默认值得天然落在将来。
        assert_eq!(default_time_slot(at(17, 48)).hour(), 18);
        assert_eq!(default_time_slot(at(9, 0)).hour(), 10);
        // 跨天时日期要跟着走,否则默认值会落在今天的凌晨(已过去)。
        let cross = default_time_slot(at(23, 30));
        assert_eq!(cross.hour(), 0);
        assert_eq!(cross.format("%Y-%m-%d").to_string(), "2026-08-04");
    }

    /// 红线:**带 id 的定时器动作必须过 (agent, chat) 归属校验**。
    ///
    /// `schedules.yaml` 是全局单文件、卡片回调只带 id,operate-gate 只答「这个人能不能操作」,
    /// 答不了「这条归不归他管」——漏一处,A 群的 operator 就能读改 B agent 私聊里的定时器。
    ///
    /// 这三个 handler 内部自建 store、依赖真实 `~/.summon`,结构上难以单测,但「有没有调用校验」
    /// 可以静态断言(同 `scheduler.rs` 红线测试与 `frontend/endpoints.rs` 的既有做法)。
    /// 删掉任何一处 `owned_schedule` 调用,这个测试立刻红。注释行不参与判定。
    #[test]
    fn schedule_handlers_taking_an_id_all_check_ownership() {
        let code = include_str!("cards.rs")
            .split("#[cfg(test)]")
            .next()
            .unwrap_or_default();
        for name in [
            "handle_schedule_edit",
            "handle_schedule_save",
            "handle_schedule_cancel",
        ] {
            let start = code
                .find(&format!("fn {name}("))
                .unwrap_or_else(|| panic!("{name} 不见了(改名了?同步更新本测试)"));
            // 函数体到下一个顶层 `\n}` 为止,足够框住单个 handler。
            let body = &code[start..];
            let end = body.find("\n}\n").map(|i| i + 2).unwrap_or(body.len());
            let body: String = body[..end]
                .lines()
                .map(|line| line.split("//").next().unwrap_or_default())
                .collect::<Vec<_>>()
                .join("\n");
            assert!(
                body.contains("owned_schedule") || body.contains("schedule_belongs_to"),
                "{name} 必须过归属校验:卡片回调只带 id,没有这道校验就能跨会话读改定时器"
            );
        }
    }

    /// 红线:**交互卡的三个注入动作都必须先证明「菜单此刻仍在」**(spec 2026-08-14 第九节验收
    /// 不变量:任何注入路径在目标状态缺席时零 send-keys;Esc 仅指向被正向识别的已知 modal)。
    ///
    /// 这条路径不经 `send_line_for_driver`,所以通用注入闸管不到它 —— 也**不该**管:那道闸对
    /// `Modal(Menu)` 恒拒,而菜单正是这三个动作的目标状态。判据只能是「目标状态本身仍在」。
    /// 少任何一个动作的后果都很具体:`tui_esc` 的 Escape 落到运行中的轮上是打断键、落到带草稿的
    /// 输入框上会清掉用户的文字;`tui_text` 的自由文本会被此刻屏上的全屏面板整条吞掉。
    ///
    /// 这个 handler 依赖真实 tmux/飞书,结构上难以单测,但「有没有这道校验、盖没盖全」可以静态
    /// 断言(同上面的定时器归属红线测试)。注释行不参与判定。
    #[test]
    fn every_interactive_injection_proves_the_menu_is_still_on_screen() {
        let code = include_str!("cards.rs")
            .split("#[cfg(test)]")
            .next()
            .unwrap_or_default();
        let start = code
            .find("fn handle_tui_inject(")
            .expect("handle_tui_inject 不见了(改名了?同步更新本测试)");
        let body: String = code[start..]
            .lines()
            .map(|line| line.split("//").next().unwrap_or_default())
            .collect::<Vec<_>>()
            .join("\n");
        let guard = body
            .find("parse_interactive(&pane).is_none()")
            .expect("必须有「菜单此刻仍在」的正向校验");
        let cond_start = body[..guard]
            .rfind("if matches!(action,")
            .expect("校验必须由一个覆盖动作名的 matches! 守着");
        let cond = &body[cond_start..guard];
        for action in ["tui_choice", "tui_text", "tui_esc"] {
            assert!(cond.contains(action), "{action} 漏出了新鲜度校验:{cond}");
        }
        // 校验必须在**任何**按键之前。
        for key in ["send_key(", "send_line(", "send_enter(", "send_text("] {
            if let Some(at) = body.find(key) {
                assert!(at > guard, "{key} 出现在新鲜度校验之前 —— 那就是零证明注入");
            }
        }
        let choice_guard = body
            .find("choice_card_matches_screen(&card.value, &pane)")
            .expect("单选卡必须校验屏上还是发卡时那一张菜单");
        for key in ["send_key(", "send_line(", "send_enter(", "send_text("] {
            if let Some(at) = body.find(key) {
                assert!(at > choice_guard, "{key} 出现在同一张菜单的校验之前");
            }
        }
    }

    /// 注入成功之后算不算答掉了一张菜单:只有带签名的单选卡算。
    #[test]
    fn only_a_signed_choice_click_answers_a_menu() {
        let signed = json!({
            "action": "tui_choice",
            "signature": summon_core::interactive::signature_token(42),
        });
        assert_eq!(answered_menu_signature("tui_choice", &signed), Some(42));
        assert_eq!(answered_menu_signature("tui_text", &signed), None);
        assert_eq!(answered_menu_signature("tui_esc", &signed), None);
        assert_eq!(
            answered_menu_signature("tui_choice", &json!({"action": "tui_choice"})),
            None
        );
    }

    /// 单选卡只认发卡时那一张菜单:换了一张菜单、或卡上根本没带签名,都不算。
    #[test]
    fn choice_card_only_matches_the_menu_it_was_sent_for() {
        let pane = |command: &str| {
            format!("──────────────────────────────\n Bash command\n\n   {command}\n\n Do you want to proceed?\n ❯ 1. Yes\n   2. No\n\n Esc to cancel")
        };
        let sent = pane("rm -f $LOGS/a.bin");
        let menu = summon_core::interactive::parse_interactive(&sent).unwrap();
        let options: Vec<(usize, String)> = menu
            .options
            .iter()
            .map(|c| (c.index, c.label.clone()))
            .collect();
        let card: serde_json::Value = serde_json::from_str(
            &summon_core::feishu::card_interactive_select(&menu.title, &options),
        )
        .unwrap();
        let value = card["body"]["elements"][3]["columns"][0]["elements"][0]["behaviors"][0]
            ["value"]
            .clone();
        assert!(choice_card_matches_screen(&value, &sent));
        assert!(
            !choice_card_matches_screen(&value, &pane("rm -rf $HOME/work")),
            "屏上换了一张菜单"
        );
        let mut unsigned = value.clone();
        unsigned.as_object_mut().unwrap().remove("signature");
        assert!(
            !choice_card_matches_screen(&unsigned, &sent),
            "没带签名的卡不认"
        );
    }

    /// 发卡时倒计时是 1:55,点的时候屏上已走到 0:09:还是同一张菜单,点击要认。
    #[test]
    fn choice_card_still_matches_after_the_auto_deny_countdown_moves_on() {
        let pane = |reading: &str| {
            format!("──────────────────────────────\n Bash command\n\n   rm -f $S/*\n\n ⚠ Claude Code will automatically deny this request in {reading}, to avoid blocking\n progress on an unattended session\n\n Do you want to proceed?\n ❯ 1. Yes\n   2. No\n\n Esc to cancel")
        };
        let sent = summon_core::interactive::parse_interactive(&pane("1:55")).unwrap();
        let value = serde_json::json!({
            "signature": summon_core::interactive::signature_token(sent.signature()),
        });
        assert!(choice_card_matches_screen(&value, &pane("0:09")));
    }

    #[test]
    fn card_origin_idle_only_when_value_marks_it() {
        // 空闲卡按钮:origin=idle + idle_secs → Idle(收尾保空闲布局)。
        assert_eq!(
            CardOrigin::from_value(
                &json!({"action": "reset", "origin": "idle", "idle_secs": 86400})
            ),
            CardOrigin::Idle(IdleCardState {
                idle_secs: 86400,
                context: None,
            })
        );
        // 缺 idle_secs → 退 0(仍是 Idle,不崩)。
        assert_eq!(
            CardOrigin::from_value(&json!({"action": "compact", "origin": "idle"})),
            CardOrigin::Idle(IdleCardState {
                idle_secs: 0,
                context: None,
            })
        );
        assert_eq!(
            CardOrigin::from_value(&json!({
                "origin": "idle",
                "idle_secs": 3600,
                "ctx_used_tokens": 350000,
                "ctx_window_tokens": 1000000
            })),
            CardOrigin::Idle(IdleCardState {
                idle_secs: 3600,
                context: Some(summon_core::context_budget::ContextBudget::from_tokens(
                    350_000,
                    Some(1_000_000),
                )),
            })
        );
        // /session 面板卡(无 origin)与旧卡 → Panel(向后兼容,fail-safe 回现有面板行为)。
        assert_eq!(
            CardOrigin::from_value(&json!({"action": "reset", "chat": "g"})),
            CardOrigin::Panel
        );
        assert_eq!(CardOrigin::from_value(&json!({})), CardOrigin::Panel);
    }

    fn ctx_warn(
        remedy: summon_core::context_budget::ContextRemedy,
    ) -> summon_core::feishu::ContextWarnCard {
        summon_core::feishu::ContextWarnCard {
            step: 60,
            budget: summon_core::context_budget::ContextBudget::from_tokens(
                650_000,
                Some(1_000_000),
            ),
            remedy,
        }
    }

    /// 从真实渲染出来的提醒卡上取某个按钮的回调值,点击测试只用卡上真有的东西。
    fn ctx_click(
        warn: &summon_core::feishu::ContextWarnCard,
        sid: Option<&str>,
        action: &str,
        chat_id: &str,
    ) -> CardActionMsg {
        fn find(v: &serde_json::Value, action: &str) -> Option<serde_json::Value> {
            match v {
                serde_json::Value::Object(map) => {
                    if map.get("type").and_then(|t| t.as_str()) == Some("callback")
                        && map["value"]["action"] == action
                    {
                        return Some(map["value"].clone());
                    }
                    map.values().find_map(|child| find(child, action))
                }
                serde_json::Value::Array(items) => items.iter().find_map(|i| find(i, action)),
                _ => None,
            }
        }
        let card: serde_json::Value =
            serde_json::from_str(&summon_core::feishu::card_context_warning(warn, sid)).unwrap();
        CardActionMsg {
            chat_id: chat_id.into(),
            message_id: "om_ctx_card".into(),
            operator_open_id: "ou_owner".into(),
            event_id: format!("evt_{action}"),
            value: find(&card, action).unwrap_or_else(|| panic!("卡上没有 {action}")),
            wal_id: None,
            wal_replayed: false,
        }
    }

    #[test]
    fn idle_close_clears_waiting_without_changing_the_idle_period() {
        let (agent, _tmp) = crate::test_support::agent_dir(
            "codex",
            "feishu:\n  operators: [ou_owner]\n  chat_operators:\n    oc_idle: [ou_colleague]\n",
        );
        let fc = FeishuClient::new("feishu.cn", String::new(), String::new(), true);
        let chat_id = "oc_idle";
        let tag = chat_tag("group", chat_id);
        for (origin, who, online, clears) in [
            ("idle", "ou_owner", true, true),
            ("idle", "ou_colleague", true, true),
            ("idle", "ou_guest", true, false),
            ("panel", "ou_owner", true, false),
            ("", "ou_owner", true, false),
            ("idle", "ou_owner", false, true),
        ] {
            let mark = summon_core::waiting::WaitingMark {
                chat_id: chat_id.into(),
                reply_message_id: "om_reply".into(),
                link: "test".into(),
                since_unix: 1,
            };
            crate::runtime::waiting_store::set(&agent.state_dir(), &tag, &mark).unwrap();
            let (tx, rx) = std::sync::mpsc::channel();
            let mut handle = ctx_handle(tx, "soul-idle-close");
            handle.idle_prompted = true;
            let before = handle.last_activity;
            let chats = if online {
                HashMap::from([(chat_id.into(), handle)])
            } else {
                HashMap::new()
            };
            let card = CardActionMsg {
                chat_id: chat_id.into(),
                message_id: "om_idle".into(),
                operator_open_id: who.into(),
                event_id: "evt_close".into(),
                value: json!({"action":"card_close","origin":origin}),
                wal_id: None,
                wal_replayed: false,
            };
            handle_card_session_close(&chats, &agent, &fc, &card);
            assert_eq!(
                crate::runtime::waiting_store::list(&agent.state_dir()).is_empty(),
                clears,
                "{origin} {who} online={online}"
            );
            if online {
                assert_eq!(chats[chat_id].last_activity, before);
                assert!(chats[chat_id].idle_prompted);
                assert!(
                    matches!(rx.try_recv(), Ok(ThreadMsg::ForgetSessionCard { card_id }) if card_id == "om_idle")
                );
                assert!(rx.try_recv().is_err());
            }
        }
    }

    fn ctx_handle(tx: std::sync::mpsc::Sender<ThreadMsg>, tmux_name: &str) -> ChatHandle {
        ChatHandle {
            tx,
            last_activity: std::time::SystemTime::now(),
            idle_prompted: false,
            chat_type: "group".into(),
            tmux_name: tmux_name.into(),
            tmux_seen_live: false,
            local_claimed: false,
            last_inbound_scheduled: false,
            last_inbound_id: String::new(),
            turn_was_active: false,
            subagent_state: None,
            menu_ledger: None,
        }
    }

    /// 本 chat 的出站归档里所有 PATCH 过的卡片内容。
    fn patched_cards(agent: &Agent, chat_id: &str) -> Vec<String> {
        let path = agent
            .state_dir()
            .join("threads")
            .join(chat_tag("group", chat_id))
            .join("outbound.ndjson");
        std::fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .filter(|line| line.contains("\"card_update\""))
            .map(str::to_string)
            .collect()
    }

    /// 本 chat 的出站归档里除卡片 PATCH 之外发出的消息。
    fn sent_messages(agent: &Agent, chat_id: &str) -> Vec<String> {
        let path = agent
            .state_dir()
            .join("threads")
            .join(chat_tag("group", chat_id))
            .join("outbound.ndjson");
        std::fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .filter(|line| !line.contains("\"card_update\""))
            .map(str::to_string)
            .collect()
    }

    struct CtxHarness {
        inbox: crate::runtime::inbox_wal::InboxWal,
        bridge: crate::bridge::store::BridgeStore,
        self_tx: std::sync::mpsc::Sender<DispatchMsg>,
        self_rx: std::sync::mpsc::Receiver<DispatchMsg>,
        claimed: std::collections::HashSet<String>,
        _run: tempfile::TempDir,
    }

    impl CtxHarness {
        fn new() -> Self {
            let run = tempfile::tempdir().unwrap();
            let (self_tx, self_rx) = std::sync::mpsc::channel();
            Self {
                inbox: crate::runtime::inbox_wal::InboxWal::new(run.path().join("inbox")),
                bridge: crate::bridge::store::BridgeStore::new(run.path().join("bridge")),
                self_tx,
                self_rx,
                claimed: Default::default(),
                _run: run,
            }
        }

        fn click(
            &mut self,
            chats: &HashMap<String, ChatHandle>,
            agent: &Agent,
            card: &CardActionMsg,
            action: ContextWarnAction,
        ) {
            let fc = FeishuClient::new("feishu.cn", String::new(), String::new(), true);
            handle_context_warn_card(
                chats,
                agent,
                &fc,
                card,
                action,
                &summon_core::config::builtin_messaging(),
                ContextWarnClick {
                    inbox: &self.inbox,
                    bridge: &self.bridge,
                    delivery_scope: "scope",
                    self_tx: &self.self_tx,
                    claimed: &mut self.claimed,
                },
            );
        }
    }

    #[test]
    fn ignoring_a_context_warning_freezes_the_card_and_only_counts_once() {
        use summon_core::context_budget::ContextRemedy;
        let (agent, _tmp) = crate::test_support::agent_dir("codex", "");
        let chat_id = "oc_ctx_ignore";
        let warn = ctx_warn(ContextRemedy::Handoff);
        let card = ctx_click(&warn, None, "ctx_ignore", chat_id);
        let mut harness = CtxHarness::new();
        harness.click(&HashMap::new(), &agent, &card, ContextWarnAction::Ignore);
        let patched = patched_cards(&agent, chat_id);
        assert_eq!(patched.len(), 1, "{patched:?}");
        assert!(patched[0].contains("已忽略"));

        let handoff = ctx_click(&warn, None, "ctx_handoff", chat_id);
        harness.click(
            &HashMap::new(),
            &agent,
            &handoff,
            ContextWarnAction::Handoff,
        );
        assert_eq!(
            patched_cards(&agent, chat_id).len(),
            1,
            "同一张卡点过就不再认"
        );
        assert!(
            harness.self_rx.try_recv().is_err(),
            "忽略之后再点交接不能发起"
        );
        assert!(harness.inbox.scan_pending().is_empty());
    }

    #[test]
    fn compacting_from_a_context_warning_goes_to_the_live_session() {
        use summon_core::context_budget::ContextRemedy;
        let (agent, _tmp) = crate::test_support::agent_dir("claude", "");
        let chat_id = "oc_ctx_compact";
        let warn = ctx_warn(ContextRemedy::Compact);
        let card = ctx_click(&warn, None, "ctx_compact", chat_id);
        let (tx, rx) = std::sync::mpsc::channel();
        let chats = HashMap::from([(chat_id.to_string(), ctx_handle(tx, "soul-ctx"))]);
        let mut harness = CtxHarness::new();
        harness.click(&chats, &agent, &card, ContextWarnAction::Compact);
        match rx.try_recv() {
            Ok(ThreadMsg::Compact {
                message_id, origin, ..
            }) => {
                assert_eq!(message_id, "om_ctx_card");
                assert_eq!(origin, CardOrigin::ContextWarn(warn));
            }
            _ => panic!("压缩要交给本 chat 的会话线程"),
        }
        let patched = patched_cards(&agent, chat_id);
        assert!(patched[0].contains("正在压缩"), "{patched:?}");
        harness.click(&chats, &agent, &card, ContextWarnAction::Compact);
        assert!(rx.try_recv().is_err(), "连点不能压两次");
    }

    /// 会话线程不在(退出了,或 daemon 刚换代、tmux 还活着):不定格、不断言会话没在运行,说一句
    /// 怎么接回来;接回来之后同一张卡还能点。
    #[test]
    fn compacting_without_a_live_session_thread_keeps_the_card_clickable() {
        use summon_core::context_budget::ContextRemedy;
        let (agent, _tmp) = crate::test_support::agent_dir("claude", "");
        let chat_id = "oc_ctx_compact_offline";
        let card = ctx_click(
            &ctx_warn(ContextRemedy::Compact),
            None,
            "ctx_compact",
            chat_id,
        );
        let mut harness = CtxHarness::new();
        harness.click(&HashMap::new(), &agent, &card, ContextWarnAction::Compact);
        assert!(patched_cards(&agent, chat_id).is_empty());
        let sent = sent_messages(&agent, chat_id);
        assert!(sent.iter().any(|m| m.contains("说句话")), "{sent:?}");
        assert!(
            !sent.iter().any(|m| m.contains("没在运行")),
            "tmux 可能还活着,不能这么说:{sent:?}"
        );

        let (tx, rx) = std::sync::mpsc::channel();
        let chats = HashMap::from([(chat_id.to_string(), ctx_handle(tx, "soul-ctx"))]);
        harness.click(&chats, &agent, &card, ContextWarnAction::Compact);
        assert!(matches!(rx.try_recv(), Ok(ThreadMsg::Compact { .. })));
    }

    /// claude 关了续聊时盘上没有 resume 记录,证不了会话换过,压缩照做(只作用于此刻的会话,
    /// 不拆任何东西);交接是破坏性的,卡上没有会话 id 就不执行。
    #[test]
    fn the_session_fence_is_strict_only_for_handoff() {
        use summon_core::context_budget::ContextRemedy;
        let (claude, _tmp) = crate::test_support::agent_dir("claude", "  resume: false\n");
        let chat_id = "oc_ctx_no_record";
        let (tx, rx) = std::sync::mpsc::channel();
        let chats = HashMap::from([(chat_id.to_string(), ctx_handle(tx, "soul-ctx-no-record"))]);
        let card = ctx_click(
            &ctx_warn(ContextRemedy::Compact),
            Some("claude-sid"),
            "ctx_compact",
            chat_id,
        );
        let mut harness = CtxHarness::new();
        harness.click(&chats, &claude, &card, ContextWarnAction::Compact);
        assert!(matches!(rx.try_recv(), Ok(ThreadMsg::Compact { .. })));

        let (codex, _tmp) = crate::test_support::agent_dir("codex", "");
        let chat_id = "oc_ctx_no_sid";
        let card = ctx_click(
            &ctx_warn(ContextRemedy::Handoff),
            None,
            "ctx_handoff",
            chat_id,
        );
        let mut harness = CtxHarness::new();
        harness.click(&HashMap::new(), &codex, &card, ContextWarnAction::Handoff);
        assert!(
            harness.self_rx.try_recv().is_err(),
            "证不了是哪个会话就不交接"
        );
        assert!(harness.inbox.scan_pending().is_empty());
        let patched = patched_cards(&codex, chat_id);
        assert!(patched[0].contains("缺会话 id"), "{patched:?}");
        assert!(!patched[0].contains("已结束"), "会话没换过,不能这么说");

        // 卡上有 id,盘上却没有记录可对:同样证不了,原因同样如实说。
        let chat_id = "oc_ctx_no_record_handoff";
        let (tx, _rx) = std::sync::mpsc::channel();
        let chats = HashMap::from([(chat_id.to_string(), ctx_handle(tx, "soul-ctx-unrecorded"))]);
        let card = ctx_click(
            &ctx_warn(ContextRemedy::Handoff),
            Some("thread-1"),
            "ctx_handoff",
            chat_id,
        );
        let mut harness = CtxHarness::new();
        harness.click(&chats, &codex, &card, ContextWarnAction::Handoff);
        assert!(harness.self_rx.try_recv().is_err());
        assert!(patched_cards(&codex, chat_id)[0].contains("缺会话 id"));

        // claude 关了续聊:Handoff 证不了会话,但卡上还有 Compact —— 不定格,只回一句原因,
        // 认领放开,Compact 还能点。
        let chat_id = "oc_ctx_claude_unrecorded_handoff";
        let (tx, rx) = std::sync::mpsc::channel();
        let chats = HashMap::from([(chat_id.to_string(), ctx_handle(tx, "soul-ctx-claude-norec"))]);
        let warn = ctx_warn(ContextRemedy::Compact);
        let handoff = ctx_click(&warn, Some("claude-sid"), "ctx_handoff", chat_id);
        let mut harness = CtxHarness::new();
        harness.click(&chats, &claude, &handoff, ContextWarnAction::Handoff);
        assert!(harness.self_rx.try_recv().is_err(), "证不了就不交接");
        assert!(harness.inbox.scan_pending().is_empty());
        assert!(patched_cards(&claude, chat_id).is_empty(), "不定格整张卡");
        assert!(!harness.claimed.contains(&handoff.message_id), "认领放开");
        let outbound = std::fs::read_to_string(
            claude
                .state_dir()
                .join("threads")
                .join(chat_tag("group", chat_id))
                .join("outbound.ndjson"),
        )
        .unwrap();
        assert!(outbound.contains("Compact 仍可用"), "{outbound}");
        let compact = ctx_click(&warn, Some("claude-sid"), "ctx_compact", chat_id);
        harness.click(&chats, &claude, &compact, ContextWarnAction::Compact);
        assert!(matches!(rx.try_recv(), Ok(ThreadMsg::Compact { .. })));
    }

    /// 后一代 daemon 里同一张卡再提交一次,载荷可能与落盘那次不同(换了人、名字缓存冷了):
    /// 键已被占就是已经发起过,不是落盘失败。
    #[test]
    fn a_card_whose_handoff_is_already_on_disk_is_not_reported_as_a_failure() {
        use summon_core::context_budget::ContextRemedy;
        let (agent, _tmp) = crate::test_support::agent_dir("codex", "");
        let chat_id = "oc_ctx_handoff_twice";
        let session = "soul-ctx-twice";
        crate::session::resume_store::write(&agent, session, "codex", "thread-1", None, None);
        let (tx, _rx) = std::sync::mpsc::channel();
        let chats = HashMap::from([(chat_id.to_string(), ctx_handle(tx, session))]);
        let warn = ctx_warn(ContextRemedy::Handoff);
        let mut first = ctx_click(&warn, Some("thread-1"), "ctx_handoff", chat_id);
        first.value["form_value"] =
            json!({summon_core::feishu::CTX_HANDOFF_NOTE_FIELD: "先修登录"});
        let mut harness = CtxHarness::new();
        harness.click(&chats, &agent, &first, ContextWarnAction::Handoff);
        assert!(matches!(
            harness.self_rx.try_recv(),
            Ok(DispatchMsg::SentinelFire { .. })
        ));

        let frozen_once = patched_cards(&agent, chat_id).len();
        harness.claimed.clear();
        let mut again = first.clone();
        again.operator_open_id = "ou_other".into();
        again.value["form_value"] =
            json!({summon_core::feishu::CTX_HANDOFF_NOTE_FIELD: "别的重点"});
        harness.click(&chats, &agent, &again, ContextWarnAction::Handoff);
        assert!(harness.self_rx.try_recv().is_err());
        assert!(
            !sent_messages(&agent, chat_id)
                .iter()
                .any(|m| m.contains("落盘失败")),
            "已经发起过的交接不能报成失败"
        );
        assert!(harness.claimed.contains("om_ctx_card"), "认领不放开");
        let patched = patched_cards(&agent, chat_id);
        assert_eq!(
            patched.len(),
            frozen_once + 1,
            "落盘之后、定格之前崩过的卡,再点要补上定格"
        );
        let refrozen = patched.last().unwrap();
        assert!(refrozen.contains("已发起交接"));
        assert!(refrozen.contains("先修登录"), "接手重点取落盘那次的");
        assert!(!refrozen.contains("别的重点"));
    }

    /// Bridge 状态读不出与 Bridge 占着同样处理:不起本地会话,点 Handoff 当场拒绝。
    #[test]
    fn handoff_is_refused_while_the_bridge_state_is_unreadable() {
        use summon_core::context_budget::ContextRemedy;
        let (agent, _tmp) = crate::test_support::agent_dir("codex", "");
        let chat_id = "oc_ctx_bridge";
        let session = "soul-ctx-bridge";
        crate::session::resume_store::write(&agent, session, "codex", "thread-1", None, None);
        let (tx, _rx) = std::sync::mpsc::channel();
        let chats = HashMap::from([(chat_id.to_string(), ctx_handle(tx, session))]);
        let card = ctx_click(
            &ctx_warn(ContextRemedy::Handoff),
            Some("thread-1"),
            "ctx_handoff",
            chat_id,
        );
        let mut harness = CtxHarness::new();
        let bridge_root = harness._run.path().join("bridge");
        std::fs::create_dir_all(&bridge_root).unwrap();
        std::fs::write(bridge_root.join("state.json"), b"{broken").unwrap();
        harness.click(&chats, &agent, &card, ContextWarnAction::Handoff);
        assert!(harness.self_rx.try_recv().is_err());
        assert!(harness.inbox.scan_pending().is_empty());
        assert!(patched_cards(&agent, chat_id).is_empty());
        assert!(sent_messages(&agent, chat_id)
            .iter()
            .any(|m| m.contains("Bridge")));
    }

    /// Bridge 占着本群时不起本地会话,点 Handoff 当场拒绝,说的是 Bridge 自己的那句。
    #[test]
    fn handoff_is_refused_while_the_bridge_holds_the_group() {
        use summon_core::bridge::{
            BridgeTicket, ChatSlot, ClaudeProvider, ScopedChatKey, TicketStatus,
        };
        use summon_core::context_budget::ContextRemedy;
        let (agent, _tmp) = crate::test_support::agent_dir("codex", "");
        let chat_id = "oc_ctx_bridge_held";
        let session = "soul-ctx-bridge-held";
        crate::session::resume_store::write(&agent, session, "codex", "thread-1", None, None);
        let (tx, _rx) = std::sync::mpsc::channel();
        let chats = HashMap::from([(chat_id.to_string(), ctx_handle(tx, session))]);
        let card = ctx_click(
            &ctx_warn(ContextRemedy::Handoff),
            Some("thread-1"),
            "ctx_handoff",
            chat_id,
        );
        let mut harness = CtxHarness::new();
        let target = ScopedChatKey::new("scope", &agent.name, chat_id);
        harness
            .bridge
            .with_state(|state| {
                state.tickets.insert(
                    "ticket".into(),
                    BridgeTicket {
                        id: "ticket".into(),
                        short_code: None,
                        target: target.clone(),
                        target_chat_type: "group".into(),
                        target_provider: ClaudeProvider::Default,
                        requester_open_id: "owner".into(),
                        requester_union_id: "union".into(),
                        control_card_message_id: None,
                        private_chat_id: None,
                        slot_revision: 1,
                        choices: vec![],
                        issued_at_ms: 1,
                        expires_at_ms: u64::MAX / 2,
                        status: TicketStatus::Pending,
                    },
                );
                state.slots.insert(
                    target.storage_key(),
                    ChatSlot::Pending {
                        ticket_id: "ticket".into(),
                        revision: 1,
                        expires_at_ms: u64::MAX / 2,
                    },
                );
                Ok(())
            })
            .unwrap();
        harness.click(&chats, &agent, &card, ContextWarnAction::Handoff);
        assert!(harness.self_rx.try_recv().is_err());
        assert!(harness.inbox.scan_pending().is_empty());
        assert!(patched_cards(&agent, chat_id).is_empty());
        let sent = sent_messages(&agent, chat_id);
        assert!(sent.iter().any(|m| m.contains("私聊确认中")), "{sent:?}");
    }

    /// 交接那一轮已经销账(跑完,或被跳过),换代后再点这张还亮着的卡:说已处理过,而不是
    /// 「正在写交接」。
    #[test]
    fn a_card_whose_handoff_already_ran_says_so() {
        use summon_core::context_budget::ContextRemedy;
        let (agent, _tmp) = crate::test_support::agent_dir("codex", "");
        let chat_id = "oc_ctx_handoff_ran";
        let session = "soul-ctx-ran";
        crate::session::resume_store::write(&agent, session, "codex", "thread-1", None, None);
        let (tx, _rx) = std::sync::mpsc::channel();
        let chats = HashMap::from([(chat_id.to_string(), ctx_handle(tx, session))]);
        let card = ctx_click(
            &ctx_warn(ContextRemedy::Handoff),
            Some("thread-1"),
            "ctx_handoff",
            chat_id,
        );
        let mut harness = CtxHarness::new();
        harness.click(&chats, &agent, &card, ContextWarnAction::Handoff);
        assert!(harness.self_rx.try_recv().is_ok());
        harness
            .inbox
            .mark_consumed(&agent.name, "ctx-handoff-om_ctx_card");

        harness.claimed.clear();
        harness.click(&chats, &agent, &card, ContextWarnAction::Handoff);
        assert!(harness.self_rx.try_recv().is_err());
        let refrozen = patched_cards(&agent, chat_id).pop().unwrap();
        assert!(refrozen.contains("已处理过"), "{refrozen}");
        assert!(!refrozen.contains("正在写交接"));
    }

    /// 群在遥控 / Bridge 中时,代投的写交接轮会被跳过;点的时候就说清楚,卡上不留「已发起交接」。
    #[test]
    fn handoff_is_refused_while_the_group_is_remote_controlled() {
        use summon_core::context_budget::ContextRemedy;
        let (agent, _tmp) = crate::test_support::agent_dir("codex", "");
        let chat_id = "oc_ctx_remote";
        let session = "soul-ctx-remote";
        crate::session::resume_store::write(&agent, session, "codex", "thread-1", None, None);
        let remote = agent.state_dir().join("remote");
        std::fs::create_dir_all(&remote).unwrap();
        // 读不出的遥控记录按遥控中处理(fail-closed),不必拼一份完整绑定。
        std::fs::write(remote.join(format!("{chat_id}.json")), "{").unwrap();
        let (tx, _rx) = std::sync::mpsc::channel();
        let chats = HashMap::from([(chat_id.to_string(), ctx_handle(tx, session))]);
        let card = ctx_click(
            &ctx_warn(ContextRemedy::Handoff),
            Some("thread-1"),
            "ctx_handoff",
            chat_id,
        );
        let mut harness = CtxHarness::new();
        harness.click(&chats, &agent, &card, ContextWarnAction::Handoff);
        assert!(harness.self_rx.try_recv().is_err());
        assert!(harness.inbox.scan_pending().is_empty());
        assert!(
            patched_cards(&agent, chat_id).is_empty(),
            "卡上不能留已发起交接"
        );
        assert!(sent_messages(&agent, chat_id)
            .iter()
            .any(|m| m.contains("遥控")));
    }

    #[test]
    fn handing_off_from_a_context_warning_is_a_durable_handoff_turn() {
        use summon_core::context_budget::ContextRemedy;
        let (agent, _tmp) = crate::test_support::agent_dir("codex", "");
        let chat_id = "oc_ctx_handoff";
        let session = "soul-ctx-handoff";
        crate::session::resume_store::write(&agent, session, "codex", "thread-1", None, None);
        let (tx, _rx) = std::sync::mpsc::channel();
        let chats = HashMap::from([(chat_id.to_string(), ctx_handle(tx, session))]);
        let warn = ctx_warn(ContextRemedy::Handoff);
        let mut card = ctx_click(&warn, Some("thread-1"), "ctx_handoff", chat_id);
        card.value["form_value"] = json!({
            summon_core::feishu::CTX_HANDOFF_NOTE_FIELD: "  先修登录  "
        });
        let mut harness = CtxHarness::new();
        harness.click(&chats, &agent, &card, ContextWarnAction::Handoff);
        match harness.self_rx.try_recv() {
            Ok(DispatchMsg::SentinelFire {
                chat_id: fired_chat,
                prompt,
                fire_id,
                who_id,
                handoff_note,
                ..
            }) => {
                assert_eq!(fired_chat, chat_id);
                assert_eq!(fire_id, "ctx-handoff-om_ctx_card");
                assert_eq!(who_id, "ou_owner");
                assert_eq!(handoff_note.as_deref(), Some("先修登录"));
                assert_eq!(
                    prompt,
                    summon_core::handoff::instruction(
                        summon_core::handoff::HandoffAfter::NewSession,
                        "先修登录"
                    )
                );
            }
            _ => panic!("交接要经代投通道进 dispatcher"),
        }
        let pending = harness.inbox.scan_pending();
        assert_eq!(pending.len(), 1, "落盘才算发起,重启后由启动恢复重投");
        assert_eq!(
            pending[0]
                .sentinel
                .as_ref()
                .and_then(|s| s.handoff_note.as_deref()),
            Some("先修登录")
        );
        let patched = patched_cards(&agent, chat_id);
        assert!(patched[0].contains("已发起交接"), "{patched:?}");
        assert!(patched[0].contains("先修登录"));

        // 换一代 daemon(内存认领表清空),同一张卡再提交一次:WAL 键已占,不再投第二次。
        harness.claimed.clear();
        harness.click(&chats, &agent, &card, ContextWarnAction::Handoff);
        assert!(harness.self_rx.try_recv().is_err());
        assert_eq!(harness.inbox.scan_pending().len(), 1);
    }

    #[test]
    fn a_handoff_that_could_not_be_written_down_can_be_clicked_again() {
        use summon_core::context_budget::ContextRemedy;
        let (agent, _tmp) = crate::test_support::agent_dir("codex", "");
        let chat_id = "oc_ctx_handoff_retry";
        let session = "soul-ctx-retry";
        crate::session::resume_store::write(&agent, session, "codex", "thread-1", None, None);
        let (tx, _rx) = std::sync::mpsc::channel();
        let chats = HashMap::from([(chat_id.to_string(), ctx_handle(tx, session))]);
        let card = ctx_click(
            &ctx_warn(ContextRemedy::Handoff),
            Some("thread-1"),
            "ctx_handoff",
            chat_id,
        );
        let mut harness = CtxHarness::new();
        let blocked = harness._run.path().join("inbox");
        std::fs::write(&blocked, "占住入站 WAL 的目录").unwrap();
        harness.click(&chats, &agent, &card, ContextWarnAction::Handoff);
        assert!(harness.self_rx.try_recv().is_err());
        assert!(patched_cards(&agent, chat_id).is_empty(), "没发起就不定格");

        std::fs::remove_file(&blocked).unwrap();
        harness.click(&chats, &agent, &card, ContextWarnAction::Handoff);
        assert!(
            matches!(
                harness.self_rx.try_recv(),
                Ok(DispatchMsg::SentinelFire { .. })
            ),
            "落盘失败要放开认领,卡还能再点"
        );
    }

    #[test]
    fn a_context_warning_from_an_ended_session_does_nothing() {
        use summon_core::context_budget::ContextRemedy;
        let (agent, _tmp) = crate::test_support::agent_dir("codex", "");
        let chat_id = "oc_ctx_stale";
        let session = "soul-ctx-stale";
        crate::session::resume_store::write(&agent, session, "codex", "thread-new", None, None);
        let (tx, rx) = std::sync::mpsc::channel();
        let chats = HashMap::from([(chat_id.to_string(), ctx_handle(tx, session))]);
        let warn = ctx_warn(ContextRemedy::Handoff);
        let card = ctx_click(&warn, Some("thread-old"), "ctx_handoff", chat_id);
        let mut harness = CtxHarness::new();
        harness.click(&chats, &agent, &card, ContextWarnAction::Handoff);
        assert!(
            harness.self_rx.try_recv().is_err(),
            "旧会话的卡不能在新会话上交接"
        );
        assert!(harness.inbox.scan_pending().is_empty());
        assert!(rx.try_recv().is_err());
        let patched = patched_cards(&agent, chat_id);
        assert!(patched[0].contains("已结束或换过"), "{patched:?}");

        // Compact 只在证得换过时拒:盘上是新会话,旧卡上的 Compact 同样不作用。
        let compact = ctx_click(
            &ctx_warn(ContextRemedy::Compact),
            Some("thread-old"),
            "ctx_compact",
            chat_id,
        );
        let mut harness = CtxHarness::new();
        harness.click(&chats, &agent, &compact, ContextWarnAction::Compact);
        assert!(rx.try_recv().is_err(), "旧卡的 Compact 不压新会话");
        assert!(patched_cards(&agent, chat_id)
            .last()
            .unwrap()
            .contains("已结束或换过"));

        let current = ctx_click(&warn, Some("thread-new"), "ctx_handoff", chat_id);
        let mut harness = CtxHarness::new();
        harness.click(&chats, &agent, &current, ContextWarnAction::Handoff);
        assert!(
            matches!(
                harness.self_rx.try_recv(),
                Ok(DispatchMsg::SentinelFire { .. })
            ),
            "会话没换过就照常交接"
        );
    }

    fn claude_agent_with_model(root: &std::path::Path) -> Agent {
        let yaml = "name: arcadia\nmodel: opus\nruntime:\n  cli: claude\n";
        let mut agent: Agent = serde_yaml::from_str(yaml).unwrap();
        agent.dir = root.join("agents").join(&agent.name);
        std::fs::create_dir_all(&agent.dir).unwrap();
        std::fs::create_dir_all(agent.state_dir()).unwrap();
        std::fs::write(agent.manifest_path(), yaml).unwrap();
        agent
    }

    fn live_chat(tx: std::sync::mpsc::Sender<ThreadMsg>) -> ChatHandle {
        ChatHandle {
            tx,
            last_activity: std::time::SystemTime::now(),
            idle_prompted: false,
            chat_type: "group".into(),
            tmux_name: "soul-arcadia".into(),
            tmux_seen_live: false,
            local_claimed: false,
            last_inbound_scheduled: false,
            last_inbound_id: String::new(),
            turn_was_active: false,
            subagent_state: None,
            menu_ledger: None,
        }
    }

    /// claude 会话在线：模型先交给会话确认，配置原样，等 claude 认了再写进对应那一层。
    #[test]
    fn a_live_claude_session_confirms_the_model_before_it_is_saved() {
        for (target, write) in [
            (
                LayerTarget::Chat,
                super::super::ModelWrite::Chat(chat_tag("group", "oc_m")),
            ),
            (LayerTarget::Default, super::super::ModelWrite::Default),
        ] {
            let tmp = tempfile::tempdir().unwrap();
            let agent = claude_agent_with_model(tmp.path());
            let before = std::fs::read_to_string(agent.manifest_path()).unwrap();
            let fc = FeishuClient::new("feishu.cn", String::new(), String::new(), true);
            let (tx, rx) = std::sync::mpsc::channel();
            let mut chats = HashMap::from([("oc_m".to_string(), live_chat(tx))]);

            let outcome = set_model_for_chat(
                &mut chats,
                &agent,
                &fc,
                "oc_m",
                "group",
                "claude-sonnet-4-6",
                None,
                target,
                true,
            );

            assert_eq!(outcome, ModelSet::AwaitingConfirm);
            assert_eq!(
                std::fs::read_to_string(agent.manifest_path()).unwrap(),
                before
            );
            assert_eq!(
                crate::runtime::chat_model::chat_model(&agent, &chat_tag("group", "oc_m")),
                None
            );
            match rx.try_recv() {
                Ok(ThreadMsg::SwitchApply {
                    slash,
                    confirm_model,
                    ..
                }) => {
                    assert_eq!(slash.as_deref(), Some("/model claude-sonnet-4-6"));
                    assert_eq!(
                        confirm_model,
                        Some(super::super::ModelConfirm {
                            model: "claude-sonnet-4-6".into(),
                            write: write.clone(),
                        })
                    );
                }
                _ => panic!("在线会话要收到待确认的切换"),
            }
        }
    }

    /// 只有改本群、值就是用户打的那个时,切换回执才是纯确认;片段解析出的完整 id、回到的默认值、
    /// 改默认的影响范围都要照常发文字。
    #[test]
    fn only_a_chat_switch_to_the_typed_model_is_a_pure_confirmation() {
        for (target, echoes_input, done) in [
            (LayerTarget::Chat, true, true),
            (LayerTarget::Chat, false, false),
            (LayerTarget::ChatInherit, true, false),
            (LayerTarget::Default, true, false),
        ] {
            let tmp = tempfile::tempdir().unwrap();
            let agent = claude_agent_with_model(tmp.path());
            let fc = FeishuClient::new("feishu.cn", String::new(), String::new(), true);
            let (tx, rx) = std::sync::mpsc::channel();
            let mut chats = HashMap::from([("oc_m".to_string(), live_chat(tx))]);
            set_model_for_chat(
                &mut chats,
                &agent,
                &fc,
                "oc_m",
                "group",
                "claude-sonnet-4-6",
                Some("om_cmd"),
                target,
                echoes_input,
            );
            match rx.try_recv() {
                Ok(ThreadMsg::SwitchApply { receipt, .. }) => assert_eq!(
                    matches!(receipt, CommandAck::Done(_)),
                    done,
                    "{target:?} echoes_input={echoes_input}: {receipt:?}"
                ),
                _ => panic!("{target:?}:在线会话要收到切换"),
            }
        }
    }

    /// 会话不在线：没有 claude 可以确认，模型直接写进对应那一层、下次冷启带上。改本群不碰
    /// agent.yaml,改默认不写本群文件。
    #[test]
    fn an_offline_chat_saves_the_model_right_away() {
        let tag = chat_tag("group", "oc_m");
        let tmp = tempfile::tempdir().unwrap();
        let agent = claude_agent_with_model(tmp.path());
        let before = std::fs::read_to_string(agent.manifest_path()).unwrap();
        let fc = FeishuClient::new("feishu.cn", String::new(), String::new(), true);
        let mut chats = HashMap::new();

        let outcome = set_model_for_chat(
            &mut chats,
            &agent,
            &fc,
            "oc_m",
            "group",
            "claude-sonnet-4-6",
            None,
            LayerTarget::Chat,
            true,
        );
        assert_eq!(outcome, ModelSet::Saved);
        assert_eq!(
            crate::runtime::chat_model::chat_model(&agent, &tag).as_deref(),
            Some("claude-sonnet-4-6")
        );
        assert_eq!(
            std::fs::read_to_string(agent.manifest_path()).unwrap(),
            before
        );

        let outcome = set_model_for_chat(
            &mut chats,
            &agent,
            &fc,
            "oc_m",
            "group",
            "",
            None,
            LayerTarget::ChatInherit,
            true,
        );
        assert_eq!(outcome, ModelSet::Saved);
        assert_eq!(crate::runtime::chat_model::chat_model(&agent, &tag), None);

        let outcome = set_model_for_chat(
            &mut chats,
            &agent,
            &fc,
            "oc_other",
            "group",
            "claude-sonnet-4-6",
            None,
            LayerTarget::Default,
            true,
        );
        assert_eq!(outcome, ModelSet::Saved);
        assert!(std::fs::read_to_string(agent.manifest_path())
            .unwrap()
            .contains("model: claude-sonnet-4-6"));
        assert_eq!(crate::runtime::chat_model::chat_model(&agent, &tag), None);
    }

    /// effort 分层:改本群写本群文件,`inherit` 清掉;改默认写 agent.yaml,本群有自己的档位时
    /// 不注入、回执说不受影响;别的群跟随默认。
    #[test]
    fn effort_changes_land_on_their_own_layer() {
        let tag = chat_tag("group", "oc_m");
        let tmp = tempfile::tempdir().unwrap();
        let agent = claude_agent_with_model(tmp.path());
        let fc = FeishuClient::new("feishu.cn", String::new(), String::new(), true);
        let (tx, rx) = std::sync::mpsc::channel();
        let mut chats = HashMap::from([("oc_m".to_string(), live_chat(tx))]);

        assert!(set_effort_for_chat(
            &mut chats,
            &agent,
            &fc,
            "oc_m",
            "group",
            "low",
            None,
            LayerTarget::Chat,
            true
        ));
        assert_eq!(
            crate::runtime::chat_model::chat_effort(&agent, &tag).as_deref(),
            Some("low")
        );
        assert!(matches!(
            rx.try_recv(),
            Ok(ThreadMsg::SwitchApply { slash: Some(slash), .. }) if slash == "/effort low"
        ));

        assert!(set_effort_for_chat(
            &mut chats,
            &agent,
            &fc,
            "oc_m",
            "group",
            "high",
            None,
            LayerTarget::Default,
            true
        ));
        assert!(rx.try_recv().is_err(), "本群有自己的档位,改默认不注入");
        let fresh = agent.reread().unwrap();
        assert_eq!(fresh.effort(), Some("high"));
        assert_eq!(
            crate::runtime::chat_model::effective(&fresh, Some(&tag)).effort(),
            Some("low")
        );
        assert_eq!(
            crate::runtime::chat_model::effective(&fresh, Some("g-other")).effort(),
            Some("high")
        );

        assert!(set_effort_for_chat(
            &mut chats,
            &fresh,
            &fc,
            "oc_m",
            "group",
            "",
            None,
            LayerTarget::ChatInherit,
            true
        ));
        assert_eq!(crate::runtime::chat_model::chat_effort(&fresh, &tag), None);
        assert!(matches!(
            rx.try_recv(),
            Ok(ThreadMsg::SwitchApply { slash: Some(slash), .. }) if slash == "/effort high"
        ));
    }

    /// 目标模型不在会话公布的目录里(会话停在 auto-smart):说清收敛不了、怎么办;在目录里只是还没
    /// 到下一条消息。
    #[test]
    fn a_pending_cursor_model_says_why_and_what_to_do() {
        let available: Vec<String> = ["auto-smart", "gpt-5.6-sol"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let missing = cursor_model_pending_note("gpt-5.6", Some("auto-smart"), &available);
        assert!(
            missing.contains("不在 Cursor 这条会话公布的模型目录里"),
            "{missing}"
        );
        assert!(missing.contains("停在 `auto-smart`"), "{missing}");
        assert!(missing.contains("base model"), "{missing}");
        let later = cursor_model_pending_note("gpt-5.6-sol", Some("auto-smart"), &available);
        assert!(later.contains("下一条消息时才在会话里生效"), "{later}");
        assert!(!later.contains("base model"), "{later}");
    }

    /// 改默认时本群设了自己的模型:只写 agent.yaml,不往本群会话注入,回执说本群不受影响。
    #[test]
    fn a_default_change_leaves_a_chat_with_its_own_model_alone() {
        let tag = chat_tag("group", "oc_m");
        let tmp = tempfile::tempdir().unwrap();
        let agent = claude_agent_with_model(tmp.path());
        crate::runtime::chat_model::set_chat_model(&agent, &tag, Some("claude-opus-4-8")).unwrap();
        let fc = FeishuClient::new("feishu.cn", String::new(), String::new(), true);
        let (tx, rx) = std::sync::mpsc::channel();
        let mut chats = HashMap::from([("oc_m".to_string(), live_chat(tx))]);

        let outcome = set_model_for_chat(
            &mut chats,
            &agent,
            &fc,
            "oc_m",
            "group",
            "claude-sonnet-4-6",
            None,
            LayerTarget::Default,
            true,
        );
        assert_eq!(outcome, ModelSet::Saved);
        assert!(rx.try_recv().is_err(), "本群生效值没变,不注入");
        assert!(std::fs::read_to_string(agent.manifest_path())
            .unwrap()
            .contains("model: claude-sonnet-4-6"));
        assert_eq!(
            crate::runtime::chat_model::chat_model(&agent, &tag).as_deref(),
            Some("claude-opus-4-8")
        );
        let receipt = std::fs::read_to_string(
            agent
                .state_dir()
                .join("threads")
                .join(&tag)
                .join("outbound.ndjson"),
        )
        .unwrap();
        assert!(receipt.contains("不受影响"), "{receipt}");
    }

    /// 一段话当模型名：不写盘、不注入。
    #[test]
    fn a_sentence_is_never_saved_or_injected_as_a_model() {
        let sentence =
            ::summon_core::i18n::label(summon_core::model_effort::PROSE_AS_MODEL_FIXTURE);
        let tmp = tempfile::tempdir().unwrap();
        let agent = claude_agent_with_model(tmp.path());
        let before = std::fs::read_to_string(agent.manifest_path()).unwrap();
        let fc = FeishuClient::new("feishu.cn", String::new(), String::new(), true);
        let (tx, rx) = std::sync::mpsc::channel();
        let mut online = HashMap::from([("oc_m".to_string(), live_chat(tx))]);
        let mut offline = HashMap::new();

        for chats in [&mut online, &mut offline] {
            for target in [LayerTarget::Chat, LayerTarget::Default] {
                let outcome = set_model_for_chat(
                    chats, &agent, &fc, "oc_m", "group", sentence, None, target, true,
                );
                assert_eq!(outcome, ModelSet::Refused);
            }
        }
        assert_eq!(
            crate::runtime::chat_model::chat_model(&agent, &chat_tag("group", "oc_m")),
            None
        );
        assert!(rx.try_recv().is_err(), "不注入");
        assert_eq!(
            std::fs::read_to_string(agent.manifest_path()).unwrap(),
            before
        );
        let receipt = std::fs::read_to_string(
            agent
                .state_dir()
                .join("threads")
                .join(chat_tag("group", "oc_m"))
                .join("outbound.ndjson"),
        )
        .unwrap();
        assert!(receipt.contains("改模型失败"), "{receipt}");
        assert!(!receipt.contains("模型已设为"), "{receipt}");
    }

    /// `/model <id>` 没切换时的回执：说清原因，有候选就带模型按钮(与面板同一个动作)。
    #[test]
    fn a_model_that_was_not_switched_gets_its_reason_and_candidates() {
        use summon_core::model_effort::{model_text_command, ModelTextCommand};
        let tmp = tempfile::tempdir().unwrap();
        let agent = claude_agent_with_model(tmp.path());
        let fc = FeishuClient::new("feishu.cn", String::new(), String::new(), true);
        let catalog: Vec<String> = ["opus", "claude-opus-4-8", "claude-sonnet-4-6"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let outbound = || {
            std::fs::read_to_string(
                agent
                    .state_dir()
                    .join("threads")
                    .join(chat_tag("group", "oc_m"))
                    .join("outbound.ndjson"),
            )
            .unwrap_or_default()
        };

        let decision = model_text_command("claude-opus-4-9", &catalog);
        reply_model_not_switched(
            &agent,
            &fc,
            "oc_m",
            "group",
            "claude-opus-4-9",
            &decision,
            None,
            false,
        );
        let sent = outbound();
        assert!(sent.contains("候选目录里没有 `claude-opus-4-9`"), "{sent}");
        assert!(sent.contains(r#""action":"model""#), "{sent}");
        assert!(sent.contains(r#""model":"claude-opus-4-8""#), "{sent}");

        reply_model_not_switched(
            &agent,
            &fc,
            "oc_m",
            "group",
            "opus",
            &ModelTextCommand::NoCatalog,
            None,
            false,
        );
        assert!(outbound().contains("拿不到候选目录"));
    }

    /// 面板按钮切模型：写进配置了才刷新面板高亮;还在等 claude 确认时不刷，免得高亮一个没切成的模型。
    #[test]
    fn the_panel_is_refreshed_only_once_the_model_is_saved() {
        let panel_updates = |agent: &Agent, chat_type: &str| {
            std::fs::read_to_string(
                agent
                    .state_dir()
                    .join("threads")
                    .join(chat_tag(chat_type, "oc_m"))
                    .join("outbound.ndjson"),
            )
            .unwrap_or_default()
            .matches(r#""kind":"card_update""#)
            .count()
        };
        let click = CardActionMsg {
            chat_id: "oc_m".into(),
            message_id: "om_panel".into(),
            operator_open_id: "ou_owner".into(),
            event_id: "evt_model".into(),
            value: serde_json::json!({"action": "model", "model": "claude-sonnet-4-6"}),
            wal_id: None,
            wal_replayed: false,
        };
        let fc = FeishuClient::new("feishu.cn", String::new(), String::new(), true);

        let tmp = tempfile::tempdir().unwrap();
        let agent = claude_agent_with_model(tmp.path());
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut online = HashMap::from([("oc_m".to_string(), live_chat(tx))]);
        handle_card_set_model(&mut online, &agent, &fc, &click, false);
        assert_eq!(panel_updates(&agent, "group"), 0, "等确认时不刷新面板");

        let tmp = tempfile::tempdir().unwrap();
        let agent = claude_agent_with_model(tmp.path());
        let mut offline = HashMap::new();
        handle_card_set_model(&mut offline, &agent, &fc, &click, false);
        assert_eq!(panel_updates(&agent, "p2p"), 1, "写进配置后刷新面板");
    }

    /// handle 不在内存(daemon 重启、会话已退)时点群里的旧面板:本群覆盖仍落到群的 g tag,不当私聊。
    #[test]
    fn a_group_panel_click_without_a_live_handle_lands_on_the_group_tag() {
        let tmp = tempfile::tempdir().unwrap();
        let agent = claude_agent_with_model(tmp.path());
        let session = agent.tmux_name_for_chat("group", "oc_g", false);
        super::super::chat_meta::write_chat_meta(
            &agent, &session, "oc_g", "group", None, "ou_owner", None,
        );
        let fc = FeishuClient::new("feishu.cn", String::new(), String::new(), true);
        let click = |value: serde_json::Value| CardActionMsg {
            chat_id: "oc_g".into(),
            message_id: "om_panel".into(),
            operator_open_id: "ou_owner".into(),
            event_id: "evt".into(),
            value,
            wal_id: None,
            wal_replayed: false,
        };
        let mut chats = HashMap::new();
        handle_card_set_model(
            &mut chats,
            &agent,
            &fc,
            &click(serde_json::json!({"action": "model", "model": "claude-opus-4-8"})),
            false,
        );
        handle_card_set_effort(
            &mut chats,
            &agent,
            &fc,
            &click(serde_json::json!({"action": "effort", "effort": "low"})),
            false,
        );
        let group = chat_tag("group", "oc_g");
        let p2p = chat_tag("p2p", "oc_g");
        assert_eq!(
            crate::runtime::chat_model::chat_model(&agent, &group).as_deref(),
            Some("claude-opus-4-8")
        );
        assert_eq!(
            crate::runtime::chat_model::chat_effort(&agent, &group).as_deref(),
            Some("low")
        );
        assert_eq!(crate::runtime::chat_model::chat_model(&agent, &p2p), None);
        assert_eq!(crate::runtime::chat_model::chat_effort(&agent, &p2p), None);
    }

    /// 清掉本群覆盖而默认层没设值:codex 照实说会沿用线程上之前切过的值,其余说下次启动生效。
    #[test]
    fn inheriting_into_an_empty_default_says_what_codex_actually_does() {
        let (codex, _dir) = crate::test_support::agent_dir("codex", "");
        let note = inherit_to_cli_default_note(&codex, "模型");
        assert!(note.contains("沿用之前切过的值"), "{note}");
        assert!(note.contains("新会话"), "{note}");
        let (claude, _dir) = crate::test_support::agent_dir("claude", "");
        let note = inherit_to_cli_default_note(&claude, "模型");
        assert!(note.contains("下次会话启动生效"), "{note}");
        let (cursor, _dir) = crate::test_support::agent_dir("cursor", "");
        let note = inherit_to_cli_default_note(&cursor, "模型");
        assert!(note.contains("下一条消息起不再指定模型"), "{note}");
        assert!(!note.contains("下次会话启动生效"), "{note}");
    }

    /// 面板两组按钮各写各的层:本群按钮写本群文件、「跟随默认」清掉它,「设为默认」只写 agent.yaml。
    #[test]
    fn panel_model_buttons_write_their_own_layer() {
        let tmp = tempfile::tempdir().unwrap();
        let agent = claude_agent_with_model(tmp.path());
        let fc = FeishuClient::new("feishu.cn", String::new(), String::new(), true);
        let tag = chat_tag("p2p", "oc_m");
        let click = |value: serde_json::Value| CardActionMsg {
            chat_id: "oc_m".into(),
            message_id: "om_panel".into(),
            operator_open_id: "ou_owner".into(),
            event_id: "evt_model".into(),
            value,
            wal_id: None,
            wal_replayed: false,
        };
        let mut chats = HashMap::new();
        handle_card_set_model(
            &mut chats,
            &agent,
            &fc,
            &click(serde_json::json!({"action": "model_default", "model": "claude-sonnet-4-6"})),
            true,
        );
        assert!(std::fs::read_to_string(agent.manifest_path())
            .unwrap()
            .contains("model: claude-sonnet-4-6"));
        assert_eq!(crate::runtime::chat_model::chat_model(&agent, &tag), None);

        let fresh = agent.reread().unwrap();
        handle_card_set_model(
            &mut chats,
            &fresh,
            &fc,
            &click(serde_json::json!({"action": "model", "model": "claude-opus-4-8"})),
            false,
        );
        assert_eq!(
            crate::runtime::chat_model::chat_model(&fresh, &tag).as_deref(),
            Some("claude-opus-4-8")
        );
        assert!(std::fs::read_to_string(fresh.manifest_path())
            .unwrap()
            .contains("model: claude-sonnet-4-6"));

        handle_card_set_model(
            &mut chats,
            &fresh,
            &fc,
            &click(serde_json::json!({"action": "model", "inherit": true})),
            false,
        );
        assert_eq!(crate::runtime::chat_model::chat_model(&fresh, &tag), None);
    }

    #[test]
    fn cursor_model_selections_write_their_layer_and_reject_unknown_ids() {
        let _guard = crate::test_support::process_test_guard();
        let tmp = tempfile::tempdir().unwrap();
        let cases = r#"
    *'session/new'*) printf '%s\n' '{"jsonrpc":"2.0","id":'"$id"',"result":{"sessionId":"sess-dropdown","models":{"currentModelId":"model-a","availableModels":[{"modelId":"model-a","name":"A"},{"modelId":"model-b","name":"B"}]},"configOptions":[{"id":"model","name":"Model","type":"select","currentValue":"model-a","options":[{"value":"model-a","name":"A"},{"value":"model-b","name":"B"}]}]}}' ;;
    *'session/set_config_option'*) printf '%s\n' '{"jsonrpc":"2.0","id":'"$id"',"result":{"configOptions":[{"id":"model","name":"Model","type":"select","currentValue":"model-a","options":[{"value":"model-a","name":"A"},{"value":"model-b","name":"B"}]}]}}' ;;
"#;
        let script = crate::test_support::write_fake_acp_server(tmp.path(), cases);
        let yaml = format!("name: cursor-dropdown-test\nruntime:\n  cli: cursor\n  capture: rpc\n  cli_cmd: {}\n  working_dir: {}\n", script.display(), tmp.path().display());
        let mut agent: Agent = serde_yaml::from_str(&yaml).unwrap();
        agent.dir = tmp.path().join("agents").join(&agent.name);
        std::fs::create_dir_all(&agent.dir).unwrap();
        std::fs::write(agent.manifest_path(), yaml).unwrap();
        let chat_id = "oc_dropdown";
        let tag = chat_tag("p2p", chat_id);
        let pool = crate::capture::rpc_pool::shared_for_tests();
        let client = pool
            .checkout(&crate::capture::rpc_pool::RpcChat {
                agent: &agent,
                session: "soul-cursor-dropdown",
                tag: &tag,
                chat_id,
                cwd: None,
                primer: None,
                model: None,
                service_tier: None,
                cursor_mode: None,
                codex_policy: Default::default(),
                register_platform_tools: false,
            })
            .unwrap();
        drop(client);
        let fc = FeishuClient::new("feishu.cn", String::new(), String::new(), true);
        let mut chats = HashMap::new();
        let click = |model: &str, default: bool| CardActionMsg {
            chat_id: chat_id.into(),
            message_id: "om_dropdown".into(),
            operator_open_id: "ou_owner".into(),
            event_id: "evt_dropdown".into(),
            value: serde_json::json!({"action": if default {"model_default"} else {"model"}, "model": model}),
            wal_id: None,
            wal_replayed: false,
        };
        handle_card_set_model(&mut chats, &agent, &fc, &click("model-b", true), true);
        assert_eq!(
            crate::capture::cursor_config::load(&agent)
                .unwrap()
                .selected_model(),
            Some("model-b")
        );
        handle_card_set_model(&mut chats, &agent, &fc, &click("model-a", false), false);
        let layer = crate::capture::cursor_config::CursorConfigLayer::Chat(&tag);
        assert_eq!(
            crate::capture::cursor_config::load_layer(&agent, layer)
                .unwrap()
                .selected_model(),
            Some("model-a")
        );
        for default in [false, true] {
            handle_card_set_model(
                &mut chats,
                &agent,
                &fc,
                &click("outside-catalog", default),
                default,
            );
            assert_eq!(
                crate::capture::cursor_config::load(&agent)
                    .unwrap()
                    .selected_model(),
                Some("model-b")
            );
            assert_eq!(
                crate::capture::cursor_config::load_layer(&agent, layer)
                    .unwrap()
                    .selected_model(),
                Some("model-a")
            );
        }
        let mut inherit = click("", false);
        inherit.value = serde_json::json!({"action":"model", "inherit":true});
        handle_card_set_model(&mut chats, &agent, &fc, &inherit, false);
        assert_eq!(
            crate::capture::cursor_config::load_layer(&agent, layer)
                .unwrap()
                .selected_model(),
            None
        );
        assert_eq!(
            crate::capture::cursor_config::load(&agent)
                .unwrap()
                .selected_model(),
            Some("model-b")
        );
        pool.evict(&agent.name, &tag);
    }

    /// 只发 `/model`:回用法，附上候选目录开头的几个模型按钮;没有候选时只回文字。
    #[test]
    fn a_bare_model_command_gets_usage_and_candidates() {
        let tmp = tempfile::tempdir().unwrap();
        let agent = claude_agent_with_model(tmp.path());
        let fc = FeishuClient::new("feishu.cn", String::new(), String::new(), true);
        let outbound = || {
            std::fs::read_to_string(
                agent
                    .state_dir()
                    .join("threads")
                    .join(chat_tag("group", "oc_m"))
                    .join("outbound.ndjson"),
            )
            .unwrap_or_default()
        };
        let candidates: Vec<String> = (0..9).map(|i| format!("model-{i}")).collect();
        reply_model_usage(&agent, &fc, "oc_m", "group", &candidates, None);
        let sent = outbound();
        assert!(sent.contains("只认候选目录里的模型"), "{sent}");
        assert!(sent.contains(r#""model":"model-0""#), "{sent}");
        assert!(
            !sent.contains(r#""model":"model-8""#),
            "至多列 {} 个",
            summon_core::model_effort::MODEL_SUGGESTION_CAP
        );

        reply_model_usage(&agent, &fc, "oc_m", "group", &[], None);
        assert_eq!(outbound().matches("只认候选目录里的模型").count(), 2);
    }
}
