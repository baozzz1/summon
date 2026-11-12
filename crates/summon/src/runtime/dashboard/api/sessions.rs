//! `sessions.*` 与 `turns.list`:会话一栏、会话详情、四个行内动作,以及逐轮台账的分页读取。
//!
//! 会话事实与 Menu Bar 同一份(fleet 快照);动作走与 Menu Bar、卡片同一条受控通道(见 daemon 端口),
//! 按控制台对这个 chat 的判权(`Target::Chat`)。看得到的人不一定动得了:动不了的行只显示状态,
//! 台账里的触发消息头与 feed 总结也只给动得了的人看。

use std::collections::{HashMap, HashSet};

use serde::Deserialize;
use serde_json::Value;
use summon_core::dashboard::authz::{Decision, Target};
use summon_core::fleet::{ChatStatus, FleetSnapshot, VisibleState};
use summon_core::registry::{chat_tag, Agent};
use summon_core::turn_ledger::{TurnRecord, KEEP_DAYS};
use summon_core::waiting::WaitingStatus;

use super::chats::{chat_name_of, chat_type_of};
use super::types::{
    ActOutcome, ActResult, OutsideView, SessionActions, SessionDetail, SessionRow, SessionState,
    SessionsView, TurnView, TurnsPage,
};
use super::{internal, invalid, outcome, params, reply, ApiEnv, DashInterrupt};
use crate::runtime::dashboard::server::{ApiCtx, ApiError, ErrorCode};
use crate::runtime::menubar_session::EndSessionOutcome;
use crate::runtime::waiting_store::{self, HandledOutcome};

/// 详情页一次带出多少轮;更早的经 `turns.list` 往前翻。
const DETAIL_TURNS: usize = 30;
const MAX_TURNS: usize = 100;
const DAY_MS: i64 = 24 * 3600 * 1000;

fn iso_ago(now: chrono::DateTime<chrono::Local>, secs: u64) -> String {
    (now - chrono::Duration::seconds(i64::try_from(secs).unwrap_or(i64::MAX / 1000))).to_rfc3339()
}

pub(crate) fn iso_ms(ms: i64) -> Option<String> {
    use chrono::TimeZone;
    chrono::Local
        .timestamp_millis_opt(ms)
        .single()
        .map(|t| t.to_rfc3339())
}

/// 给人看的状态:泄漏 → 卡住 → 等人回复 → 在跑 → 空闲。在跑一轮的 waiting 归在跑(它此刻在干活)。
fn classify(chat: &ChatStatus, waiting: bool, blocked: bool) -> SessionState {
    if chat.state == VisibleState::Leaked {
        SessionState::Leaked
    } else if blocked {
        SessionState::Blocked
    } else if chat.busy {
        SessionState::Working
    } else if waiting {
        SessionState::Waiting
    } else if chat.state == VisibleState::Working {
        SessionState::Working
    } else {
        SessionState::Idle
    }
}

fn rank(state: SessionState) -> u8 {
    match state {
        SessionState::Leaked => 0,
        SessionState::Blocked => 1,
        SessionState::Waiting => 2,
        SessionState::Working => 3,
        SessionState::Idle => 4,
    }
}

fn can_operate(
    env: &ApiEnv,
    ctx: &ApiCtx,
    agent: &str,
    chat_id: Option<&str>,
) -> Result<(), String> {
    let Some(chat_id) = chat_id else {
        return Err(summon_i18n::text!("r093d4013f1").into());
    };
    match env.decide(ctx, &Target::Chat { agent, chat_id }) {
        Decision::Allow => Ok(()),
        Decision::Deny(why) => Err(why),
    }
}

struct RowInput<'a> {
    alias: &'a str,
    loaded: Option<&'a Agent>,
    chat: Option<&'a ChatStatus>,
    waiting: Option<&'a WaitingStatus>,
    /// 会话已经结束时,群的 id 取自盘上的 waiting 标记(快照里没有)。
    waiting_chat_id: Option<String>,
}

fn row(
    env: &ApiEnv,
    ctx: &ApiCtx,
    now: chrono::DateTime<chrono::Local>,
    input: RowInput<'_>,
) -> SessionRow {
    let RowInput {
        alias,
        loaded,
        chat,
        waiting,
        waiting_chat_id,
    } = input;
    let tag = chat
        .map(|c| c.tag.clone())
        .or_else(|| waiting.map(|w| w.tag.clone()))
        .unwrap_or_default();
    let chat_id = chat.and_then(|c| c.chat_id.clone()).or(waiting_chat_id);
    let running = chat
        .filter(|c| c.busy)
        .and_then(|_| env.port.running_turn(alias, &tag));
    let blocked = match (loaded, chat) {
        (Some(agent), Some(chat)) if chat.busy => env.port.blocked(agent, &chat.target),
        _ => None,
    };
    let state = match chat {
        Some(chat) => classify(chat, waiting.is_some(), blocked.is_some()),
        None => SessionState::Waiting,
    };
    let operate = can_operate(env, ctx, alias, chat_id.as_deref());
    let allowed = operate.is_ok();
    let actions = SessionActions {
        handled: allowed && waiting.is_some(),
        interrupt: allowed && running.as_ref().is_some_and(|r| r.turn_id.is_some()),
        exit: allowed && chat.is_some(),
        archive: allowed && chat.is_some(),
    };
    SessionRow {
        agent: alias.to_string(),
        agent_display: loaded
            .map(|a| a.display.clone())
            .filter(|d| !d.is_empty())
            .unwrap_or_else(|| alias.to_string()),
        cli: loaded.map(|a| a.cli_kind().to_string()),
        tag,
        chat_id,
        chat_type: chat
            .and_then(|c| c.chat_type.clone())
            .or_else(|| waiting.and_then(|w| w.chat_type.clone())),
        name: chat
            .and_then(|c| c.name.clone())
            .or_else(|| waiting.and_then(|w| w.name.clone())),
        state,
        listed: chat.is_some(),
        busy: chat.is_some_and(|c| c.busy),
        turn_started_at: running.as_ref().and_then(|r| iso_ms(r.started_at_ms)),
        turn_id: running.and_then(|r| r.turn_id),
        blocked,
        waiting_since: waiting.map(|w| iso_ago(now, w.since_secs)),
        waiting_reply: waiting.map(|w| w.reply_message_id.clone()),
        link: waiting.map(|w| w.link.clone()).filter(|l| !l.is_empty()),
        last_active: chat
            .and_then(|c| c.last_active_secs)
            .map(|secs| iso_ago(now, secs)),
        capture: chat.map(|c| c.capture.clone()).unwrap_or_default(),
        actions,
        deny_reason: operate.err(),
    }
}

fn rows(env: &ApiEnv, ctx: &ApiCtx, fleet: &FleetSnapshot) -> Vec<SessionRow> {
    let now = ctx.now;
    let loaded: Vec<Agent> = fleet
        .agents
        .iter()
        .filter_map(|a| env.agent(&a.alias).ok())
        .collect();
    let find = |alias: &str| loaded.iter().find(|a| a.name == alias);
    let waiting_of = |alias: &str, tag: &str| {
        fleet
            .waiting
            .iter()
            .find(|w| w.alias == alias && w.tag == tag)
    };
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for agent in &fleet.agents {
        for chat in agent
            .chats
            .iter()
            .filter(|c| c.state != VisibleState::Exited)
        {
            seen.insert((agent.alias.as_str(), chat.tag.as_str()));
            out.push(row(
                env,
                ctx,
                now,
                RowInput {
                    alias: &agent.alias,
                    loaded: find(&agent.alias),
                    chat: Some(chat),
                    waiting: waiting_of(&agent.alias, &chat.tag),
                    waiting_chat_id: None,
                },
            ));
        }
    }
    // 会话已经结束、回复还等着人看的:也占一行,只能标记已处理。
    for w in &fleet.waiting {
        if seen.contains(&(w.alias.as_str(), w.tag.as_str())) {
            continue;
        }
        let waiting_chat_id = find(&w.alias).and_then(|agent| {
            waiting_store::list(&agent.state_dir())
                .into_iter()
                .find(|(tag, _)| *tag == w.tag)
                .map(|(_, mark)| mark.chat_id)
        });
        out.push(row(
            env,
            ctx,
            now,
            RowInput {
                alias: &w.alias,
                loaded: find(&w.alias),
                chat: None,
                waiting: Some(w),
                waiting_chat_id,
            },
        ));
    }
    // 要处理的在前;等人回复的等得最久的在前,其余保持快照里的顺序(同群的会话挨着)。
    out.sort_by(|a, b| {
        rank(a.state)
            .cmp(&rank(b.state))
            .then_with(|| match a.state {
                SessionState::Waiting => a.waiting_since.cmp(&b.waiting_since),
                _ => std::cmp::Ordering::Equal,
            })
    });
    out
}

/// 此刻在跑(含卡住)与等人回复的会话数;取不到会话状态时没有。
pub(super) fn live_counts(env: &ApiEnv, ctx: &ApiCtx) -> Option<(u32, u32)> {
    let rows = rows(env, ctx, &fleet(env).ok()?);
    let count = |states: &[SessionState]| {
        u32::try_from(rows.iter().filter(|r| states.contains(&r.state)).count()).unwrap_or(u32::MAX)
    };
    Some((
        count(&[SessionState::Working, SessionState::Blocked]),
        count(&[SessionState::Waiting]),
    ))
}

fn fleet(env: &ApiEnv) -> Result<FleetSnapshot, ApiError> {
    env.port
        .fleet()
        .ok_or_else(|| ApiError::new(ErrorCode::Unavailable, summon_i18n::text!("r492517aefc")))
}

pub(crate) fn list(ctx: &ApiCtx, _p: Value) -> Result<Value, ApiError> {
    let env = ctx.env()?;
    let fleet = fleet(env)?;
    let authorized: HashSet<_> = fleet
        .comment_tasks
        .iter()
        .map(|t| t.agent.as_str())
        .collect::<HashSet<_>>()
        .into_iter()
        .filter(|agent| env.decide(ctx, &Target::Agent(agent)).is_allowed())
        .collect();
    reply(&SessionsView {
        comment_tasks: fleet
            .comment_tasks
            .iter()
            .filter(|task| authorized.contains(task.agent.as_str()))
            .cloned()
            .collect(),
        comment_tasks_error: fleet.comment_tasks_error.clone(),
        sessions: rows(env, ctx, &fleet),
    })
}

#[derive(Deserialize)]
struct GetParams {
    agent: String,
    chat_id: String,
}

pub(crate) fn get(ctx: &ApiCtx, p: Value) -> Result<Value, ApiError> {
    let env = ctx.env()?;
    let GetParams { agent, chat_id } = params(p)?;
    let loaded = env.agent(&agent)?;
    let tag = chat_tag(&chat_type_of(&loaded, &chat_id), &chat_id);
    let session = rows(env, ctx, &fleet(env)?)
        .into_iter()
        .find(|r| r.agent == agent && r.tag == tag);
    let page = turns_page(env, ctx, &agent, Some(&tag), None, None, DETAIL_TURNS);
    reply(&SessionDetail {
        name: session
            .as_ref()
            .and_then(|s| s.name.clone())
            .or_else(|| chat_name_of(&loaded, &chat_id)),
        agent,
        chat_id,
        tag,
        session,
        turns: page.turns,
        more: page.more,
        outside: page.outside,
    })
}

/// 台账里这个 agent 的轮,按结束时间从新到旧;`before` 是上一页最后一轮的结束时刻(毫秒)。
/// 按会话查(给了 `tag`、没给 `schedule`)时一并带上这一页覆盖的时间段里的轮外用量。
fn turns_page(
    env: &ApiEnv,
    ctx: &ApiCtx,
    agent: &str,
    tag: Option<&str>,
    schedule: Option<&str>,
    before: Option<i64>,
    limit: usize,
) -> TurnsPage {
    let now_ms = ctx.now.timestamp_millis();
    let until = before.unwrap_or(now_ms + 1);
    let since = now_ms - KEEP_DAYS * DAY_MS;
    let dir = crate::runtime::turn_ledger::dir_in(&env.run_dir());
    let mut turns: Vec<TurnRecord> =
        crate::runtime::turn_ledger::read_range(&dir, Some(agent), since, until)
            .into_iter()
            .filter(|r| tag.is_none_or(|t| r.tag == t))
            .filter(|r| schedule.is_none_or(|s| r.schedule.as_deref() == Some(s)))
            .take(limit + 1)
            .collect();
    let more = turns.len() > limit;
    turns.truncate(limit);
    // 这一页往前翻到哪儿:还有更早的轮时到这一页最早那一轮为止,否则到保留期开头。
    let floor = if more {
        turns.last().map_or(since, |t| t.ended_at_ms)
    } else {
        since
    };
    let outside_rows = match (tag, schedule) {
        (Some(tag), None) => crate::runtime::turn_ledger::read_outside(
            &crate::runtime::turn_ledger::outside_dir_in(&env.run_dir()),
            Some(agent),
            floor,
            until,
        )
        .into_iter()
        .filter(|o| o.tag == tag)
        .collect(),
        _ => Vec::new(),
    };
    // 动不了这个 chat 的人看得到有这一轮,看不到是谁说了什么。
    let mut allowed: HashMap<String, bool> = HashMap::new();
    for t in &mut turns {
        let ok = *allowed
            .entry(t.chat_id.clone())
            .or_insert_with(|| can_operate(env, ctx, agent, Some(&t.chat_id)).is_ok());
        if !ok {
            t.prompt_head.clear();
            t.feed = None;
        }
    }
    let prices = crate::runtime::prices::load(&env.run_dir());
    let turns = turns
        .into_iter()
        .map(|record| TurnView {
            spend: summon_core::pricing::spend(prices.as_deref(), &record.usage),
            record,
        })
        .collect();
    let outside = outside_rows
        .into_iter()
        .map(|o| OutsideView {
            spend: summon_core::pricing::spend(prices.as_deref(), &o.usage),
            from_ms: o.from_ms,
            to_ms: o.to_ms,
            usage: o.usage,
        })
        .collect();
    TurnsPage {
        turns,
        more,
        outside,
    }
}

#[derive(Deserialize)]
struct TurnsParams {
    agent: String,
    #[serde(default)]
    chat_id: Option<String>,
    #[serde(default)]
    schedule: Option<String>,
    #[serde(default)]
    before: Option<i64>,
    #[serde(default)]
    limit: Option<usize>,
}

pub(crate) fn turns(ctx: &ApiCtx, p: Value) -> Result<Value, ApiError> {
    let env = ctx.env()?;
    let req: TurnsParams = params(p)?;
    let loaded = env.agent(&req.agent)?;
    let tag = req
        .chat_id
        .as_deref()
        .map(|id| chat_tag(&chat_type_of(&loaded, id), id));
    let limit = req.limit.unwrap_or(DETAIL_TURNS).clamp(1, MAX_TURNS);
    reply(&turns_page(
        env,
        ctx,
        &req.agent,
        tag.as_deref(),
        req.schedule.as_deref(),
        req.before,
        limit,
    ))
}

#[derive(Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
enum SessionAction {
    Handled,
    Interrupt,
    Exit,
    Archive,
}

impl SessionAction {
    fn name(self) -> &'static str {
        match self {
            Self::Handled => "handled",
            Self::Interrupt => "interrupt",
            Self::Exit => "exit",
            Self::Archive => "archive",
        }
    }
}

#[derive(Deserialize)]
struct ActParams {
    agent: String,
    chat_id: String,
    action: SessionAction,
    /// 退出 / 归档时在跑的那一轮打断也行(人在页面上确认过)。
    #[serde(default)]
    interrupt: bool,
    /// 打断:页面看到的那一轮的凭证。
    #[serde(default)]
    turn_id: Option<String>,
    /// 已处理:页面看到的那条回复。
    #[serde(default)]
    reply: Option<String>,
}

pub(crate) fn act(ctx: &ApiCtx, p: Value) -> Result<Value, ApiError> {
    let env = ctx.env()?;
    let req: ActParams = params(p)?;
    let result = do_act(env, ctx, &req);
    env.audit(
        ctx,
        &format!("session:{}:{}", req.agent, req.chat_id),
        &[req.action.name().to_string()],
        &outcome(&result),
    );
    reply(&result?)
}

fn do_act(env: &ApiEnv, ctx: &ApiCtx, req: &ActParams) -> Result<ActResult, ApiError> {
    env.require(
        ctx,
        &Target::Chat {
            agent: &req.agent,
            chat_id: &req.chat_id,
        },
    )?;
    let agent = env.agent(&req.agent)?;
    let tag = chat_tag(&chat_type_of(&agent, &req.chat_id), &req.chat_id);
    let unavailable = |why: String| ApiError::new(ErrorCode::Unavailable, why);
    let outcome = match req.action {
        SessionAction::Handled => {
            let reply = req
                .reply
                .as_deref()
                .filter(|r| !r.is_empty())
                .ok_or_else(|| invalid(summon_i18n::text!("r203a67a2ee")))?;
            match waiting_store::clear_if_reply(&agent.state_dir(), &tag, reply)
                .map_err(internal)?
            {
                HandledOutcome::Cleared => ActOutcome::Done,
                HandledOutcome::Superseded => ActOutcome::Superseded,
                HandledOutcome::Absent => ActOutcome::Absent,
            }
        }
        SessionAction::Interrupt => {
            let turn = req
                .turn_id
                .as_deref()
                .filter(|t| !t.is_empty())
                .ok_or_else(|| invalid(summon_i18n::text!("recb830aae7")))?;
            match env
                .port
                .interrupt(&agent.name, &req.chat_id, turn)
                .map_err(unavailable)?
            {
                DashInterrupt::Sent => ActOutcome::Done,
                DashInterrupt::Stale => ActOutcome::Stale,
                DashInterrupt::NotRunning => ActOutcome::NotRunning,
            }
        }
        SessionAction::Exit | SessionAction::Archive => {
            let archive = req.action == SessionAction::Archive;
            match env
                .port
                .end_session(&agent, &tag, archive, req.interrupt)
                .map_err(unavailable)?
            {
                EndSessionOutcome::Ended => ActOutcome::Done,
                EndSessionOutcome::Running => ActOutcome::Running,
                EndSessionOutcome::Pending => ActOutcome::Pending,
                EndSessionOutcome::Absent => ActOutcome::Absent,
            }
        }
    };
    Ok(ActResult { outcome })
}

#[cfg(test)]
mod tests {
    use super::super::testkit::*;
    use super::*;
    use crate::runtime::dashboard::sessions::{ChatKind, Issuer};
    use serde_json::json;
    use summon_core::turn_ledger::{TurnEnd, TurnOrigin};

    fn kit() -> (Kit, Agent) {
        let kit = Kit::new();
        let agent = kit.add_agent("starlight", &agent_yaml("starlight", "星光", "claude", ""));
        kit.add_chat(&agent, "group", GROUP, "示例群");
        (kit, agent)
    }

    fn stranger(kit: &Kit) -> ApiCtx {
        kit.feishu(Issuer {
            open_id: "ou_00000000000000000000000000000004".into(),
            union_id: PEER_UNION.into(),
            ..issuer("starlight", GROUP, ChatKind::Group)
        })
    }

    #[test]
    fn comment_stop_requires_agent_authorization_and_preserves_other_tasks() {
        let (kit, agent) = kit();
        let store =
            crate::runtime::document_comments::Store::new(kit.home().join("run/document-comments"));
        let notice = crate::runtime::document_comments::CommentNotice {
            app_id: crate::runtime::daemon::resolve_creds(&agent).0.unwrap(),
            event_id: "evt".into(),
            file_token: "doc_test".into(),
            file_type: "docx".into(),
            comment_id: "100".into(),
            reply_id: "200".into(),
            sender_open_id: "ou_reader".into(),
            recipient_open_id: "ou_bot".into(),
        };
        let id = store.receive(notice.clone(), 100, true).unwrap();
        let mut other = notice;
        other.comment_id = "101".into();
        let other = store.receive(other, 100, true).unwrap();
        let p = json!({"agent":"starlight","id":id});
        assert!(kit.call(&stranger(&kit), stop_comment, p.clone()).is_err());
        assert_eq!(
            store.get(&id).unwrap().stage,
            crate::runtime::document_comments::Stage::Received
        );
        assert_eq!(
            kit.call(&kit.owner(), stop_comment, p).unwrap(),
            json!({"stopped":true})
        );
        assert_eq!(
            store.get(&id).unwrap().stage,
            crate::runtime::document_comments::Stage::Closed
        );
        assert_eq!(
            store.get(&other).unwrap().stage,
            crate::runtime::document_comments::Stage::Received
        );
    }

    #[test]
    fn comment_task_details_require_agent_authorization() {
        let (kit, _) = kit();
        let mut fleet = sample_fleet();
        fleet.comment_tasks.push(summon_core::fleet::CommentTask {
            id: "task".into(),
            agent: "starlight".into(),
            title: "Private title".into(),
            stage: "drafting".into(),
            status: "正在起草".into(),
            status_en: "Drafting".into(),
            busy: true,
            received_ms: 100,
            updated_ms: 200,
            pid: Some(4242),
            error: None,
        });
        kit.port.set_fleet(fleet);
        let mine = kit.call(&kit.owner(), list, json!({})).unwrap();
        assert_eq!(mine["comment_tasks"][0]["title"], "Private title");
        assert_eq!(mine["comment_tasks"][0]["pid"], 4242);
        let theirs = kit.call(&stranger(&kit), list, json!({})).unwrap();
        assert_eq!(theirs["comment_tasks"], json!([]));
        assert!(!theirs.to_string().contains("Private title"));
    }

    fn tag() -> String {
        chat_tag("group", GROUP)
    }

    fn record(message_id: &str, ended_at_ms: i64) -> TurnRecord {
        TurnRecord {
            agent: "starlight".into(),
            chat_id: GROUP.into(),
            tag: tag(),
            turn: None,
            origin: TurnOrigin::Human,
            schedule: None,
            requester: None,
            message_id: message_id.into(),
            prompt_head: String::new(),
            started_at_ms: ended_at_ms - 1_000,
            ended_at_ms,
            end: TurnEnd::Done,
            error: None,
            reattached: false,
            tool_steps: 0,
            cli: None,
            model: None,
            effort: None,
            ctx: None,
            usage: Vec::new(),
            quota: None,
            feed: None,
        }
    }

    fn write_ledger(kit: &Kit, records: &[TurnRecord], now: i64) {
        let dir = kit.home().join("run/turns/starlight");
        std::fs::create_dir_all(&dir).unwrap();
        let lines: String = records
            .iter()
            .map(|r| serde_json::to_string(r).unwrap() + "\n")
            .collect();
        std::fs::write(dir.join(summon_core::turn_ledger::file_name(now)), lines).unwrap();
    }

    #[test]
    fn rows_follow_the_menubar_order_and_offer_only_what_the_viewer_can_do() {
        let (kit, agent) = kit();
        kit.port.set_fleet(sample_fleet());
        mark_sample_waiting(&agent);
        kit.port.running.lock().unwrap().insert(
            ("starlight".into(), tag()),
            super::super::RunningTurn {
                started_at_ms: 1_700_000_000_000,
                turn_id: Some("turn-1".into()),
            },
        );
        let me = kit.call(&kit.owner(), list, json!({})).unwrap();
        let sessions = me["sessions"].as_array().unwrap();
        let states: Vec<&str> = sessions
            .iter()
            .map(|s| s["state"].as_str().unwrap())
            .collect();
        assert_eq!(states, ["waiting", "working", "idle"]);
        let working = &sessions[1];
        assert_eq!(working["turn_id"], "turn-1");
        assert_eq!(
            working["actions"],
            json!({"handled": false, "interrupt": true, "exit": true, "archive": true})
        );
        // 会话已结束、只剩 waiting 的行:只能标记已处理
        assert_eq!(
            sessions[0]["actions"],
            json!({"handled": true, "interrupt": false, "exit": false, "archive": false})
        );
        let other = kit.call(&stranger(&kit), list, json!({})).unwrap();
        let row = &other["sessions"][1];
        assert_eq!(
            row["actions"],
            json!({"handled": false, "interrupt": false, "exit": false, "archive": false})
        );
        assert!(row["deny_reason"].is_string());
    }

    #[test]
    fn blocked_claude_sessions_rank_above_waiting() {
        let (kit, _agent) = kit();
        kit.port.set_fleet(sample_fleet());
        kit.port
            .blocked
            .lock()
            .unwrap()
            .insert(sample_target(), "permission".into());
        let out = kit.call(&kit.owner(), list, json!({})).unwrap();
        assert_eq!(out["sessions"][0]["state"], "blocked");
        assert_eq!(out["sessions"][0]["blocked"], "permission");
    }

    #[test]
    fn actions_need_the_chat_operate_gate_and_map_outcomes() {
        let (kit, _agent) = kit();
        let act_on = |ctx: &ApiCtx, body: Value| kit.call(ctx, act, body);
        let exit = json!({"agent": "starlight", "chat_id": GROUP, "action": "exit"});
        let err = act_on(&stranger(&kit), exit.clone()).unwrap_err();
        assert_eq!(err.code, ErrorCode::Forbidden);
        assert!(kit.port.ended.lock().unwrap().is_empty());

        *kit.port.end_outcome.lock().unwrap() = Some(EndSessionOutcome::Running);
        let out = act_on(&kit.owner(), exit).unwrap();
        assert_eq!(out["outcome"], "running");
        *kit.port.end_outcome.lock().unwrap() = Some(EndSessionOutcome::Ended);
        let out = act_on(
            &kit.owner(),
            json!({"agent": "starlight", "chat_id": GROUP, "action": "archive", "interrupt": true}),
        )
        .unwrap();
        assert_eq!(out["outcome"], "done");
        assert_eq!(
            *kit.port.ended.lock().unwrap(),
            [
                ("starlight".to_string(), tag(), false, false),
                ("starlight".to_string(), tag(), true, true)
            ]
        );

        let out = act_on(
            &kit.owner(),
            json!({"agent": "starlight", "chat_id": GROUP, "action": "interrupt", "turn_id": "turn-1"}),
        )
        .unwrap();
        assert_eq!(out["outcome"], "done");
        let missing = act_on(
            &kit.owner(),
            json!({"agent": "starlight", "chat_id": GROUP, "action": "interrupt"}),
        )
        .unwrap_err();
        assert_eq!(missing.code, ErrorCode::Invalid);
        assert!(kit
            .read("run/dashboard-audit.ndjson")
            .contains("session:starlight"));
    }

    #[test]
    fn handled_clears_only_the_reply_the_page_saw() {
        let (kit, agent) = kit();
        crate::runtime::waiting_store::set(
            &agent.state_dir(),
            &tag(),
            &summon_core::waiting::WaitingMark {
                chat_id: GROUP.into(),
                reply_message_id: "om_newer".into(),
                link: String::new(),
                since_unix: 1,
            },
        )
        .unwrap();
        let handled = |reply: &str| {
            kit.call(
                &kit.owner(),
                act,
                json!({"agent": "starlight", "chat_id": GROUP, "action": "handled", "reply": reply}),
            )
            .unwrap()["outcome"]
                .clone()
        };
        assert_eq!(handled("om_older"), "superseded");
        assert_eq!(handled("om_newer"), "done");
        assert_eq!(handled("om_newer"), "absent");
    }

    #[test]
    fn detail_pages_through_the_ledger_newest_first() {
        let (kit, _agent) = kit();
        kit.port.set_fleet(sample_fleet());
        let now = chrono::Local::now().timestamp_millis();
        let records: Vec<TurnRecord> = (0..35)
            .map(|i| record(&format!("om_{i}"), now - 99_000 + i * 1_000))
            .collect();
        write_ledger(&kit, &records, now);
        let detail = kit
            .call(
                &kit.owner(),
                get,
                json!({"agent": "starlight", "chat_id": GROUP}),
            )
            .unwrap();
        assert_eq!(detail["name"], "示例群");
        assert_eq!(detail["turns"].as_array().unwrap().len(), DETAIL_TURNS);
        assert_eq!(detail["turns"][0]["message_id"], "om_34");
        assert_eq!(detail["more"], true);
        let before = detail["turns"][DETAIL_TURNS - 1]["ended_at_ms"]
            .as_i64()
            .unwrap();
        let older = kit
            .call(
                &kit.owner(),
                turns,
                json!({"agent": "starlight", "chat_id": GROUP, "before": before}),
            )
            .unwrap();
        assert_eq!(older["turns"].as_array().unwrap().len(), 5);
        assert_eq!(older["more"], false);
    }

    #[test]
    fn outside_usage_shows_up_on_the_page_whose_time_span_covers_it() {
        let (kit, _agent) = kit();
        kit.port.set_fleet(sample_fleet());
        let now = chrono::Local::now().timestamp_millis();
        let records: Vec<TurnRecord> = (0..35)
            .map(|i| record(&format!("om_{i}"), now - 99_000 + i * 1_000))
            .collect();
        write_ledger(&kit, &records, now);
        let dir = crate::runtime::turn_ledger::outside_dir_in(&kit.home().join("run"));
        let row = |tag: String, from_ms: i64| summon_core::turn_ledger::OutsideUsage {
            agent: "starlight".into(),
            chat_id: GROUP.into(),
            tag,
            cli: "claude".into(),
            from_ms,
            to_ms: from_ms + 500,
            usage: vec![summon_core::turn_ledger::ModelUsage {
                model: "claude-test-1".into(),
                requests: 1,
                output: 9,
                ..Default::default()
            }],
        };
        for r in [
            row(tag(), now - 50_000),
            row(tag(), now - 97_000),
            row("g-other".into(), now - 50_000),
        ] {
            crate::runtime::turn_ledger::append_outside(&dir, &r).unwrap();
        }
        let body = json!({"agent": "starlight", "chat_id": GROUP});
        let detail = kit.call(&kit.owner(), get, body).unwrap();
        let first: Vec<i64> = detail["outside"]
            .as_array()
            .unwrap()
            .iter()
            .map(|o| o["from_ms"].as_i64().unwrap())
            .collect();
        assert_eq!(first, [now - 50_000]);
        assert_eq!(detail["outside"][0]["spend"]["tokens"]["output"], 9);
        let before = detail["turns"][DETAIL_TURNS - 1]["ended_at_ms"]
            .as_i64()
            .unwrap();
        let older = kit
            .call(
                &kit.owner(),
                turns,
                json!({"agent": "starlight", "chat_id": GROUP, "before": before}),
            )
            .unwrap();
        assert_eq!(older["outside"][0]["from_ms"], now - 97_000);
        // 按定时任务查不带轮外用量
        let by_schedule = kit
            .call(
                &kit.owner(),
                turns,
                json!({"agent": "starlight", "schedule": "SCH-1"}),
            )
            .unwrap();
        assert!(by_schedule.get("outside").is_none());
    }

    /// 触发消息头与 feed 总结只给动得了这个 chat 的人看;别人看得到这一轮,看不到内容。
    #[test]
    fn turn_content_is_only_shown_to_people_who_can_operate_the_chat() {
        let (kit, _agent) = kit();
        let now = chrono::Local::now().timestamp_millis();
        let mut said = record("om_said", now - 1_000);
        said.prompt_head = "帮我看看构建".into();
        said.feed = Some("修好构建".into());
        write_ledger(&kit, &[said], now);
        let body = json!({"agent": "starlight", "chat_id": GROUP});
        let mine = kit.call(&kit.owner(), turns, body.clone()).unwrap();
        assert_eq!(mine["turns"][0]["prompt_head"], "帮我看看构建");
        assert_eq!(mine["turns"][0]["feed"], "修好构建");
        let theirs = kit.call(&stranger(&kit), turns, body).unwrap();
        assert_eq!(theirs["turns"][0]["message_id"], "om_said");
        // 空的触发消息头不下发
        assert!(theirs["turns"][0]["prompt_head"].is_null());
        assert!(theirs["turns"][0]["feed"].is_null());
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StopCommentParams {
    agent: String,
    id: String,
}

pub(crate) fn stop_comment(ctx: &ApiCtx, p: Value) -> Result<Value, ApiError> {
    let env = ctx.env()?;
    let req: StopCommentParams = params(p)?;
    let agent = env.agent(&req.agent)?;
    env.require(ctx, &Target::Agent(&agent.name))?;
    let result =
        crate::runtime::document_comments::Store::new(env.run_dir().join("document-comments"))
            .stop_draft(&req.id, &agent)
            .map_err(|e| super::internal(format!("{e:#}")));
    env.audit(
        ctx,
        &format!("agent:{}", req.agent),
        &["comments.stop".into()],
        &super::outcome(&result),
    );
    result?;
    reply(&serde_json::json!({"stopped": true}))
}
