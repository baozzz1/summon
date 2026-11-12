//! chat Agent 当前正在处理的飞书消息上下文。
//!
//! `summon ask` 是 Agent 在自己的 shell 子进程里调用的；对持久 tmux 会话而言，
//! requester/message_id 不能靠启动期环境变量表达，所以 daemon 在一轮开始前原子落一个
//! 短生命周期文件，CLI 据稳定 conversation tag 读取。文件只是跨进程接缝，不是恢复 journal。

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::runtime::fsio::atomic_write;

/// 这一轮从哪里来。它不是展示字段，而是 requester 语义的一部分：
/// 飞书入站必须带真实发起人；owner 预授权的定时轮没有发信人，但仍需要一枚短租约 capability
/// 给本轮允许的 summon CLI 使用；仅结果定时轮只开放只读定位能力。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CurrentTurnOrigin {
    #[default]
    Feishu,
    Schedule,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CurrentTurnContext {
    /// 每个 chat turn 随机生成、随动态 prompt 交给 Agent 的一次性代际凭证。
    pub(crate) turn_id: String,
    pub(crate) agent_alias: String,
    pub(crate) delivery_scope: String,
    pub(crate) chat_id: String,
    pub(crate) chat_type: String,
    pub(crate) conversation_tag: String,
    pub(crate) message_id: String,
    /// 升级前的文件没有该字段；默认 Feishu 保持「真人入站必须有 requester」的保守语义。
    #[serde(default)]
    pub(crate) origin: CurrentTurnOrigin,
    /// 仅定时轮消费；真人入站与旧租约默认按 progress。
    #[serde(default)]
    pub(crate) schedule_output: summon_core::ScheduleOutput,
    pub(crate) requester_open_id: String,
    /// 发起人 union_id(同开发者下跨 app 稳定)。**必须与 open_id 一起落盘** —— 执行闸判定走
    /// `can_operate`,而名单里可能只配了 union_id(flare-5 的 owner 就是),缺了它 owner 会被
    /// 判成外部人、被闸只读,配了 open_id 的真外部人反而全权(H1)。
    ///
    /// 允许为空:入站事件不带、联系人 API 也解析不到时就是空串,此时退回按 open_id 比对。
    /// `#[serde(default)]` 兼容升级前写下的旧文件(读出空串 = 按现状降级,不会解析失败)。
    #[serde(default)]
    pub(crate) requester_union_id: String,
    /// 发起人是不是本 chat 已 `/introduce` 的成员(bot 或同事)。peer 经 operate-gate 显式认下,
    /// 执行闸视其为内部(否则认识关系被整体降只读)。缺省 false = 按普通人判定,保守。
    #[serde(default)]
    pub(crate) requester_is_peer: bool,
    /// 来源证明来自入站事件或 sentinel；缺失时不能用作真人委派凭据。
    #[serde(default = "unproven_sender_is_bot")]
    pub(crate) requester_is_bot: bool,
    #[serde(default)]
    pub(crate) thread_id: Option<String>,
    #[serde(default)]
    pub(crate) root_id: Option<String>,
    pub(crate) started_at_ms: i64,
    /// 本轮来自可信接管或其未送达重投；退出还需 worker 的本轮消费证明。
    #[serde(default)]
    pub(crate) reattached: bool,
}

fn unproven_sender_is_bot() -> bool {
    true
}

pub(crate) const INACTIVE_TURN_MESSAGE: &str = "r8692ea3289";

/// PreToolUse hook 侧读到的「本轮发起人」快照(不带 turn_id,不构成任何凭证)。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TurnActor {
    pub(crate) open_id: String,
    pub(crate) union_id: String,
    pub(crate) is_peer: bool,
    pub(crate) chat_type: String,
    pub(crate) origin: CurrentTurnOrigin,
}

#[derive(Clone)]
pub(crate) struct CurrentTurnStore {
    root: PathBuf,
}

impl CurrentTurnStore {
    #[cfg(test)]
    pub(crate) fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub(crate) fn for_agent(agent_dir: &Path) -> Self {
        Self {
            root: agent_dir.join("runtime").join("current-turns"),
        }
    }

    /// current-turn 只是本代 daemon 与 CLI 的短租约接缝，不是恢复 journal。
    /// daemon 冷启必须清空上一代残留，避免持久 tmux 用旧 env 认领上一条消息。
    pub(crate) fn clear_on_daemon_boot(&self) -> Result<()> {
        if let Ok(mut active) = active_turns().lock() {
            active.retain(|key, _| key.root != self.root);
        }
        match std::fs::remove_dir_all(&self.root) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| summon_i18n::format!("r374d84f697", self.root));
            }
        }
        Ok(())
    }

    fn path(&self, conversation_tag: &str) -> PathBuf {
        self.root
            .join(format!("{}.json", safe_file_name(conversation_tag)))
    }

    /// 原子发布当前 turn，并返回精确 lease。lease drop 只删除自己发布的 message_id；
    /// 即使未来同 conversation 出现交叠收尾，旧 guard 也不会删掉新一轮上下文。
    pub(crate) fn activate(&self, context: &CurrentTurnContext) -> Result<CurrentTurnLease> {
        validate_context(context)?;
        std::fs::create_dir_all(&self.root)
            .with_context(|| summon_i18n::format!("re49d963805", self.root))?;
        let mut active = active_turns()
            .lock()
            .map_err(|_| anyhow::anyhow!(summon_i18n::format!("rbed2fc75ae")))?;
        let path = self.path(&context.conversation_tag);
        let json = serde_json::to_vec(context).context(summon_i18n::text!("r93cbd0e0c0"))?;
        atomic_write(&path, &json).with_context(|| summon_i18n::format!("r93d2b4d6ac"))?;
        let key = ActiveTurnKey {
            root: self.root.clone(),
            agent_alias: context.agent_alias.clone(),
            conversation_tag: context.conversation_tag.clone(),
        };
        let lease_id = uuid::Uuid::new_v4().to_string();
        active.insert(
            key.clone(),
            ActiveTurn {
                lease_id: lease_id.clone(),
                context: context.clone(),
            },
        );
        Ok(CurrentTurnLease {
            key,
            path,
            turn_id: context.turn_id.clone(),
            lease_id,
        })
    }

    pub(crate) fn active_for_conversation(
        &self,
        alias: &str,
        tag: &str,
    ) -> Option<CurrentTurnContext> {
        active_turns()
            .lock()
            .ok()?
            .get(&ActiveTurnKey {
                root: self.root.clone(),
                agent_alias: alias.to_string(),
                conversation_tag: tag.to_string(),
            })
            .map(|active| active.context.clone())
    }

    /// 这条 conversation 盘上有没有 current-turn 记录。
    ///
    /// 给「能不能安全拆掉这条会话」当闸用,所以**只看在不在、不看新鲜不新鲜**:租约过期可能是
    /// 一轮真的跑得久,也可能是上一代 daemon 崩溃的残留 —— 两者在这里都判「别动它」。代价是
    /// 崩溃残留会让那条 chat 一直不自动退出,但 `clear_on_daemon_boot` 每次冷启都会清空,
    /// 残留活不过一次重启;反方向的代价(拆掉一条正在跑的会话)是不可逆的。
    pub(crate) fn has_turn_record(&self, conversation_tag: &str) -> bool {
        self.path(conversation_tag).exists()
    }

    pub(crate) fn load_by_tag(
        &self,
        conversation_tag: &str,
        turn_id: &str,
        now_ms: i64,
    ) -> Result<CurrentTurnContext> {
        let path = self.path(conversation_tag);
        let raw =
            std::fs::read_to_string(&path).with_context(|| summon_i18n::format!("rc9bf59dbf8"))?;
        let context: CurrentTurnContext =
            serde_json::from_str(&raw).context(summon_i18n::text!("re4b532b5f3"))?;
        validate_fresh(&context, now_ms)?;
        if context.turn_id != turn_id {
            bail!(summon_i18n::format!("r548e25dcca"));
        }
        Ok(context)
    }

    /// 只按 **turn_id** 在本 agent 名下定位活跃轮 —— 不需要知道 tag / chat_id。
    ///
    /// why:RPC 会话会被 `rekey` 从一个 chat 迁到同 agent 的另一个 chat,而**进程环境是 spawn
    /// 时固定的、改不了**,于是子进程里的 `SUMMON_CONVERSATION_TAG` 仍是 source chat 的。若还按
    /// env 里的 tag 去定位,迁移之后这条命令要么找不到轮(体验:平白失败),要么定位到旧会话
    /// (更糟)。turn_id 是全局唯一的一次性凭证,拿它直接找,位置信息从查到的 context 里取。
    ///
    /// 注意这不放松身份:agent 维度仍由调用方用 env 的 alias 选定 store —— 跨 agent 的陈旧环境
    /// 会去错误 agent 的目录里找,自然找不到,照旧 fail-closed。这里只是不再要求**位置**也对。
    pub(crate) fn load_by_turn_id(&self, turn_id: &str, now_ms: i64) -> Result<CurrentTurnContext> {
        let entries = std::fs::read_dir(&self.root)
            .context(::summon_core::i18n::label(INACTIVE_TURN_MESSAGE))?;
        for entry in entries.flatten() {
            if entry.path().extension().and_then(|value| value.to_str()) != Some("json") {
                continue;
            }
            let Ok(raw) = std::fs::read_to_string(entry.path()) else {
                continue;
            };
            let Ok(context) = serde_json::from_str::<CurrentTurnContext>(&raw) else {
                continue;
            };
            // turn_id 全局唯一，命中后继续校验时间与完整身份。
            if context.turn_id == turn_id {
                validate_fresh(&context, now_ms)?;
                return Ok(context);
            }
        }
        bail!(::summon_core::i18n::label(INACTIVE_TURN_MESSAGE))
    }

    /// 只读取「这个 chat 当前活跃 turn 的发起人 + 会话类型」,**不校验 turn_id、不取任何权**。
    ///
    /// why 单独一条路:`load_by_tag`/`load_unique_by_chat` 都要 turn_id(那是发给 agent 的一次性
    /// 凭证,防 `summon ask` 伪造),`active_context` 走进程内内存表 —— 而 PreToolUse hook 是独立
    /// 进程,两者都用不上。它问的问题只有一个:本轮是不是外部人发起的。读不到就 None,由调用方
    /// fail-open(owner 手工 attach 打字时本来就没有活跃 turn)。
    ///
    /// why 连 `chat_type` 一起返回:`can_operate` 的 fail-closed 分支按群/私聊分叉,而**飞书私聊的
    /// chat_id 同样是 `oc_` 开头**(实测 `oc_` 开头的 id 配 `chat_type=p2p`)—— 靠 id 前缀猜会把私聊
    /// 判成群,名单皆空的 agent 在私聊里就会误拒 owner。context 里本来就有权威的 chat_type,不用猜。
    ///
    /// why 连 `union_id`/`is_peer` 一起返回:见 `CurrentTurnContext` 两个字段的注释 —— 少任何一条
    /// 都会让执行闸的判定与 dispatcher 的鉴权语义漂移。
    ///
    /// **已知窄窗**:若某轮 `activate` 写盘失败(chat_thread 会记日志)而上一轮文件仍在,这里会读到
    /// 上一轮的发起人。`validate_fresh` 无 TTL,挡不住这种残留;不自加窗口是因为长工具轮可能合法
    /// 跑数小时,加 TTL 会误伤正常轮次。
    pub(crate) fn peek_turn_actor_by_chat(&self, chat_id: &str, now_ms: i64) -> Option<TurnActor> {
        if chat_id.is_empty() {
            return None;
        }
        let entries = std::fs::read_dir(&self.root).ok()?;
        for entry in entries.flatten() {
            if entry.path().extension().and_then(|value| value.to_str()) != Some("json") {
                continue;
            }
            let Ok(raw) = std::fs::read_to_string(entry.path()) else {
                continue;
            };
            let Ok(context) = serde_json::from_str::<CurrentTurnContext>(&raw) else {
                continue;
            };
            if context.chat_id == chat_id && validate_fresh(&context, now_ms).is_ok() {
                return Some(TurnActor {
                    open_id: context.requester_open_id,
                    union_id: context.requester_union_id,
                    is_peer: context.requester_is_peer,
                    chat_type: context.chat_type,
                    origin: context.origin,
                });
            }
        }
        None
    }

    /// Core 侧的第二次校验：文件只是跨进程传参，权威活跃性来自本代 daemon 内存。
    /// Drop 即使删文件失败也会先撤销这条记录，因此已结束 turn 永远不能靠残留文件升格。
    ///
    /// **只读、不取权**:`summon ask` 要的就是「这一轮还活着且 turn_id 对得上」。
    /// 读当前活跃 turn 的 id(**只读、不校验**)。
    ///
    /// 给渲染侧用:live 卡要把 fencing token 印进打断按钮的 payload,而渲染侧本来就不知道 token,
    /// 是它要去取。**校验永远走 [`load_by_tag`](Self::load_by_tag)** —— 那条同时比对 token 与
    /// 新鲜度,是动作点的唯一权威。读不到(无活跃 turn / 文件损坏)→ None,按钮 payload 留空,
    /// 点击侧因 token 对不上而拒(fail-closed)。
    pub(crate) fn current_turn_id(&self, conversation_tag: &str) -> Option<String> {
        let raw = std::fs::read_to_string(self.path(conversation_tag)).ok()?;
        serde_json::from_str::<CurrentTurnContext>(&raw)
            .ok()
            .map(|c| c.turn_id)
    }

    pub(crate) fn active_context(
        &self,
        agent_alias: &str,
        conversation_tag: &str,
        turn_id: &str,
    ) -> Option<CurrentTurnContext> {
        let key = ActiveTurnKey {
            root: self.root.clone(),
            agent_alias: agent_alias.to_string(),
            conversation_tag: conversation_tag.to_string(),
        };
        active_turns()
            .lock()
            .ok()?
            .get(&key)
            .filter(|active| active.context.turn_id == turn_id)
            .map(|active| active.context.clone())
    }
}

pub(crate) struct CurrentTurnLease {
    key: ActiveTurnKey,
    path: PathBuf,
    turn_id: String,
    lease_id: String,
}

impl CurrentTurnLease {}

impl Drop for CurrentTurnLease {
    fn drop(&mut self) {
        let Ok(mut active) = active_turns().lock() else {
            return;
        };
        // capability 可跨重启沿用，lease 标识和锁保证迟到 Drop 不会撤销新代持有者。
        if !active
            .get(&self.key)
            .is_some_and(|entry| entry.lease_id == self.lease_id)
        {
            return;
        }
        active.remove(&self.key);
        let belongs_to_me = std::fs::read_to_string(&self.path)
            .ok()
            .and_then(|raw| serde_json::from_str::<CurrentTurnContext>(&raw).ok())
            .is_some_and(|context| context.turn_id == self.turn_id);
        if belongs_to_me {
            // 这是短生命周期提示文件；删除失败不能改变已完成 turn 的业务结果。
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

fn validate_context(context: &CurrentTurnContext) -> Result<()> {
    for (name, value) in [
        ("turn_id", context.turn_id.as_str()),
        ("agent_alias", context.agent_alias.as_str()),
        ("delivery_scope", context.delivery_scope.as_str()),
        ("chat_id", context.chat_id.as_str()),
        ("chat_type", context.chat_type.as_str()),
        ("conversation_tag", context.conversation_tag.as_str()),
        ("message_id", context.message_id.as_str()),
    ] {
        if value.trim().is_empty() {
            bail!(summon_i18n::format!("rb5fc6e21f0"));
        }
    }
    if context.origin == CurrentTurnOrigin::Feishu && context.requester_open_id.trim().is_empty() {
        bail!(summon_i18n::format!("rd6e28b73c2"));
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct ActiveTurnKey {
    root: PathBuf,
    agent_alias: String,
    conversation_tag: String,
}

struct ActiveTurn {
    lease_id: String,
    context: CurrentTurnContext,
}

fn active_turns() -> &'static Mutex<std::collections::HashMap<ActiveTurnKey, ActiveTurn>> {
    static ACTIVE: OnceLock<Mutex<std::collections::HashMap<ActiveTurnKey, ActiveTurn>>> =
        OnceLock::new();
    ACTIVE.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

fn validate_fresh(context: &CurrentTurnContext, now_ms: i64) -> Result<()> {
    validate_context(context)?;
    let age = now_ms.saturating_sub(context.started_at_ms);
    // 活跃性由本代 daemon 内存 lease 证明，不再用固定 TTL 猜。长工具轮可能合法持续数小时；
    // 这里只拒绝来自未来的异常时间，上一代残留会在 boot 清理并被 Core 的空活跃表再次挡住。
    if age < 0 {
        bail!(summon_i18n::format!("rb65b2b6139", context.message_id));
    }
    Ok(())
}

fn safe_file_name(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-') {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context(tag: &str, chat: &str, message: &str, started_at_ms: i64) -> CurrentTurnContext {
        CurrentTurnContext {
            turn_id: format!("turn-{message}"),
            agent_alias: "arcadia".into(),
            delivery_scope: "arcadia-app-scope".into(),
            chat_id: chat.into(),
            chat_type: "group".into(),
            conversation_tag: tag.into(),
            message_id: message.into(),
            origin: CurrentTurnOrigin::Feishu,
            schedule_output: summon_core::ScheduleOutput::Progress,
            requester_open_id: "ou_requester".into(),
            requester_union_id: "on_requester".into(),
            requester_is_bot: false,
            requester_is_peer: false,
            thread_id: None,
            root_id: None,
            started_at_ms,
            reattached: false,
        }
    }

    #[test]
    fn a_restored_capability_survives_the_old_leases_drop() {
        let dir = tempfile::tempdir().unwrap();
        let store = CurrentTurnStore::new(dir.path());
        let original = context("g_tag", "oc_chat", "om_1", 100);
        let old = store.activate(&original).unwrap();
        store.clear_on_daemon_boot().unwrap();
        let current = store.activate(&original).unwrap();
        drop(old);
        assert!(store
            .active_context(&original.agent_alias, "g_tag", &original.turn_id)
            .is_some());
        assert!(store.load_by_turn_id(&original.turn_id, 101).is_ok());
        drop(current);
        assert!(store.load_by_turn_id(&original.turn_id, 101).is_err());
    }

    #[test]
    fn inactive_credentials_explain_recovery_without_disclosing_tokens() {
        let dir = tempfile::tempdir().unwrap();
        let store = CurrentTurnStore::new(dir.path());
        let current = context("g_tag", "oc_chat", "om_1", 100);
        let _lease = store.activate(&current).unwrap();
        let error = store
            .load_by_turn_id("unknown-secret", 101)
            .unwrap_err()
            .to_string();
        assert!(error.contains("接管期间可稍后"));
        assert!(error.contains("已结束或转人工恢复"));
        let error = store
            .load_by_tag("g_tag", "unknown-secret", 101)
            .unwrap_err()
            .to_string();
        assert!(!error.contains("unknown-secret"));
        assert!(!error.contains(&current.turn_id));
    }

    #[test]
    fn peek_requester_reads_active_turn_without_turn_id() {
        let dir = tempfile::tempdir().unwrap();
        let store = CurrentTurnStore::new(dir.path());
        let now = 1_700_000_000_000_i64;
        let _lease = store
            .activate(&context("g_tag", "oc_chat", "om_1", now))
            .unwrap();
        // hook 是独立进程,没有发给 agent 的一次性 turn_id,也必须问得到「这一轮是谁发起的」
        assert_eq!(
            store.peek_turn_actor_by_chat("oc_chat", now),
            Some(TurnActor {
                open_id: "ou_requester".to_string(),
                union_id: "on_requester".to_string(),
                is_peer: false,
                chat_type: "group".to_string(),
                origin: CurrentTurnOrigin::Feishu,
            }),
            "union_id/chat_type 必须一起返回:少 union_id 会让只配 union_id 的 owner 被判外部;\
             私聊 chat_id 同样是 oc_ 开头,靠前缀猜会把私聊判成群"
        );
        assert_eq!(store.peek_turn_actor_by_chat("oc_other", now), None);
        assert_eq!(store.peek_turn_actor_by_chat("", now), None);
    }

    /// peer 标记要能穿过盘上这一跳,否则执行闸看不到「这是本群已认下的成员」。
    #[test]
    fn peek_requester_carries_peer_flag() {
        let dir = tempfile::tempdir().unwrap();
        let store = CurrentTurnStore::new(dir.path());
        let now = 1_700_000_000_000_i64;
        let mut ctx = context("g_tag", "oc_chat", "om_1", now);
        ctx.requester_is_peer = true;
        let _lease = store.activate(&ctx).unwrap();
        assert!(
            store
                .peek_turn_actor_by_chat("oc_chat", now)
                .unwrap()
                .is_peer
        );
    }

    /// 升级前写下的 current-turn 文件没有 `requester_union_id` / `requester_is_peer` 两个字段。
    /// 必须读得出来(空串 / false = 按现状降级),不能整条解析失败——那会让 hook 静默 fail-open。
    #[test]
    fn legacy_context_without_new_identity_fields_still_parses() {
        let legacy = serde_json::json!({
            "turn_id": "turn-1",
            "agent_alias": "arcadia",
            "delivery_scope": "scope",
            "chat_id": "oc_chat",
            "chat_type": "group",
            "conversation_tag": "g_tag",
            "message_id": "om_1",
            "requester_open_id": "ou_requester",
            "started_at_ms": 1_700_000_000_000_i64,
        });
        let parsed: CurrentTurnContext = serde_json::from_value(legacy).unwrap();
        assert_eq!(parsed.requester_union_id, "");
        assert!(!parsed.requester_is_peer);
        assert_eq!(parsed.origin, CurrentTurnOrigin::Feishu);
        assert_eq!(
            parsed.schedule_output,
            summon_core::ScheduleOutput::Progress
        );
    }

    /// 定时轮没有真实发信人，但它仍是 owner 在创建 schedule 时预授权的执行轮。
    /// 允许空 requester 只限显式 Schedule origin；普通飞书轮继续 fail-closed。
    #[test]
    fn scheduled_context_allows_no_requester_but_feishu_context_does_not() {
        let dir = tempfile::tempdir().unwrap();
        let store = CurrentTurnStore::new(dir.path());
        let mut scheduled = context("g_sched", "oc_chat", "sched-1", 1_000);
        scheduled.origin = CurrentTurnOrigin::Schedule;
        scheduled.requester_open_id.clear();
        scheduled.requester_union_id.clear();
        let _lease = store
            .activate(&scheduled)
            .expect("owner 预授权的定时轮应取得短租约 capability");

        let mut invalid = context("g_user", "oc_chat", "om_1", 1_000);
        invalid.requester_open_id.clear();
        assert!(
            store.activate(&invalid).is_err(),
            "普通飞书轮缺真实 requester 必须继续拒绝"
        );
    }

    #[test]
    fn peek_requester_rejects_context_from_the_future() {
        let dir = tempfile::tempdir().unwrap();
        let store = CurrentTurnStore::new(dir.path());
        let started = 1_700_000_000_000_i64;
        let _lease = store
            .activate(&context("g_tag", "oc_chat", "om_1", started))
            .unwrap();
        // validate_fresh 无 TTL(长工具轮可能合法跑数小时),只拒来自未来的异常时间。
        // peek 必须继承这条语义,不自己另加窗口。
        assert_eq!(store.peek_turn_actor_by_chat("oc_chat", started - 1), None);
    }

    #[test]
    fn peek_requester_returns_none_after_lease_drop() {
        let dir = tempfile::tempdir().unwrap();
        let store = CurrentTurnStore::new(dir.path());
        let now = 1_700_000_000_000_i64;
        {
            let _lease = store
                .activate(&context("g_tag", "oc_chat", "om_1", now))
                .unwrap();
        }
        // 轮次结束后 hook 读不到发起人 → tool_guard 放行(owner 手工 attach 打字的合法路径)
        assert_eq!(store.peek_turn_actor_by_chat("oc_chat", now), None);
    }

    /// rekey 之后:进程环境里的 tag 还指着 source chat,但按 turn_id 一定能找回**当前**那一轮。
    ///
    /// 这条是「位置消费必须凭证化」的底座:只要定位不看 env 的 tag,rekey 后读历史 / 列会话
    /// 就不会落到 source chat 上。
    #[test]
    fn lookup_by_turn_id_finds_the_current_chat_after_a_rekey() {
        let dir = tempfile::tempdir().unwrap();
        let store = CurrentTurnStore::new(dir.path());
        // source chat 的旧轮(进程环境当初就是按它注入的),以及迁移后 target chat 的当前轮。
        let stale = context("g_source", "oc_source", "om_old", 1_000);
        let current = context("g_target", "oc_target", "om_new", 1_500);
        let _a = store.activate(&stale).unwrap();
        let _b = store.activate(&current).unwrap();

        // 按当前轮的凭证找 —— 拿到的必须是 target,而不是 env 里那个 source。
        let found = store.load_by_turn_id(&current.turn_id, 2_000).unwrap();
        assert_eq!(found.chat_id, "oc_target");
        assert_eq!(found.conversation_tag, "g_target");
        // 反过来也成立:旧轮的凭证只会找回旧轮,不会串到当前轮。
        let old = store.load_by_turn_id(&stale.turn_id, 2_000).unwrap();
        assert_eq!(old.chat_id, "oc_source");
    }

    /// **盘上残留 ≠ 还活着**:lease 文件删除失败时,盘上那条仍能被 `load_by_turn_id` 读出来,
    /// 但 `active_context`(本代内存表)已经撤销 —— 主动 CLI 的第二道闸正是拦这一格。
    ///
    /// 没有这条区分,一个早已结束的轮就能靠残留文件继续往群里发东西。
    #[test]
    fn a_stale_lease_on_disk_is_still_rejected_by_the_in_memory_table() {
        let dir = tempfile::tempdir().unwrap();
        let store = CurrentTurnStore::new(dir.path());
        let ctx = context("g123", "oc_room", "om_1", 1_000);
        let lease = store.activate(&ctx).unwrap();
        // 轮内:两边都认。
        assert!(store.load_by_turn_id(&ctx.turn_id, 2_000).is_ok());
        assert!(store
            .active_context("arcadia", "g123", &ctx.turn_id)
            .is_some());

        // 轮结束:lease drop 会同时撤内存表并删盘上文件。
        drop(lease);
        assert!(
            store
                .active_context("arcadia", "g123", &ctx.turn_id)
                .is_none(),
            "轮结束后内存表必须立刻不认"
        );
        // **模拟"盘上文件删除失败"**:把那份 context 原样写回去(不经 activate,不进内存表)。
        std::fs::write(
            dir.path().join("g123.json"),
            serde_json::to_string(&ctx).unwrap(),
        )
        .unwrap();

        // 盘上又能读到了 —— 这正是危险所在:只验盘上就会放行一个早已结束的轮。
        assert!(
            store.load_by_turn_id(&ctx.turn_id, 2_000).is_ok(),
            "残留文件确实还能被盘上校验读出来"
        );
        // 但内存表始终不认 —— 主动 CLI 的第二道闸据此拒绝。
        assert!(
            store
                .active_context("arcadia", "g123", &ctx.turn_id)
                .is_none(),
            "残留文件不能让内存表重新认这一轮"
        );
    }

    #[test]
    fn active_context_roundtrips_and_lease_removes_it() {
        let dir = tempfile::tempdir().unwrap();
        let store = CurrentTurnStore::new(dir.path());
        let context = context("g123", "oc_room", "om_1", 1_000);
        let lease = store.activate(&context).unwrap();
        assert_eq!(
            store
                .active_context("arcadia", "g123", &context.turn_id)
                .unwrap(),
            context
        );
        assert_eq!(
            store.load_by_tag("g123", &context.turn_id, 2_000).unwrap(),
            context
        );
        drop(lease);
        assert!(store
            .active_context("arcadia", "g123", "turn-om_1")
            .is_none());
        assert!(store.load_by_tag("g123", "turn-om_1", 2_000).is_err());
    }

    /// **同一个会话位置上、拿别的 turn_id 来问,必须查不到。**
    ///
    /// 这一条是 `preview` / `ChatAsk` / 主动 CLI 全部凭证复验的地基:它们各自的"轮已结束"
    /// 判据最终都落到这里。查询若只按 (agent, tag) 定位而不校验 turn_id,上一轮的旧凭证在
    /// 下一轮开始后依然"有效"——旧轮的 agent 能拿它继续往群里发东西。
    #[test]
    fn active_context_rejects_a_stale_turn_id_at_the_same_conversation() {
        let dir = tempfile::tempdir().unwrap();
        let store = CurrentTurnStore::new(dir.path());
        let current = context("g123", "oc_room", "om_2", 1_000);
        let _lease = store.activate(&current).unwrap();

        assert!(
            store
                .active_context("arcadia", "g123", "turn-om_1")
                .is_none(),
            "上一轮的 turn_id 不得在本轮位置上被认成有效凭证"
        );
        assert_eq!(
            store
                .active_context("arcadia", "g123", &current.turn_id)
                .unwrap(),
            current,
            "本轮自己的凭证仍必须通过"
        );
    }

    #[test]
    fn long_active_context_is_valid_but_future_timestamp_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let store = CurrentTurnStore::new(dir.path());
        let _lease = store
            .activate(&context("g123", "oc_room", "om_1", 1_000))
            .unwrap();
        assert!(store
            .load_by_tag("g123", "turn-om_1", 1_000 + 24 * 60 * 60 * 1000)
            .is_ok());
        assert!(store.load_by_tag("g123", "turn-om_1", 999).is_err());
    }

    #[test]
    fn old_lease_never_deletes_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let store = CurrentTurnStore::new(dir.path());
        let old = store
            .activate(&context("g123", "oc_room", "om_old", 1_000))
            .unwrap();
        let new = store
            .activate(&context("g123", "oc_room", "om_new", 2_000))
            .unwrap();
        drop(old);
        assert_eq!(
            store
                .load_by_tag("g123", "turn-om_new", 3_000)
                .unwrap()
                .message_id,
            "om_new"
        );
        drop(new);
    }

    #[test]
    fn lease_id_prevents_same_message_aba() {
        let dir = tempfile::tempdir().unwrap();
        let store = CurrentTurnStore::new(dir.path());
        let old_context = context("g123", "oc_room", "om_same", 1_000);
        let old = store.activate(&old_context).unwrap();
        let mut new_context = context("g123", "oc_room", "om_same", 2_000);
        new_context.turn_id = "turn-new-generation".into();
        let new = store.activate(&new_context).unwrap();

        drop(old);
        assert_eq!(
            store
                .load_by_tag("g123", "turn-new-generation", 3_000)
                .unwrap(),
            new_context
        );
        drop(new);
    }

    #[test]
    fn chat_fallback_uses_explicit_turn_id_without_guessing() {
        let dir = tempfile::tempdir().unwrap();
        let store = CurrentTurnStore::new(dir.path());
        let _one = store
            .activate(&context("t1", "oc_room", "om_1", 1_000))
            .unwrap();
        assert_eq!(
            store
                .load_by_turn_id("turn-om_1", 2_000)
                .unwrap()
                .message_id,
            "om_1"
        );
        let _two = store
            .activate(&context("t2", "oc_room", "om_2", 1_500))
            .unwrap();
        assert_eq!(
            store
                .load_by_turn_id("turn-om_2", 2_000)
                .unwrap()
                .message_id,
            "om_2"
        );
        assert!(store.load_by_turn_id("unknown-turn", 2_000).is_err());
    }

    #[test]
    fn daemon_boot_invalidates_previous_generation_context() {
        let dir = tempfile::tempdir().unwrap();
        let store = CurrentTurnStore::new(dir.path());
        let _lease = store
            .activate(&context("g123", "oc_room", "om_old", 1_000))
            .unwrap();

        store.clear_on_daemon_boot().unwrap();

        assert!(store.load_by_tag("g123", "turn-om_old", 2_000).is_err());
        assert!(store.load_by_turn_id("turn-om_old", 2_000).is_err());
    }
}
