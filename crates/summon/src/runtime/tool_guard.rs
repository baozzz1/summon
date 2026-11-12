//! PreToolUse 执行闸:外部发起人的轮次只读。
//!
//! **判定本身不在这里** —— 它是纯件,住在 `summon_core::authz::{requester_is_external,
//! tool_guard_verdict}`,与 `can_operate` / 恢复卡鉴权同处一室。本文件只负责 IO:读 env、
//! 定位 current-turn、加载注册表、把裁决翻译成 hook 的 exit code。
//!
//! **为什么判定必须住在 core**:prompt 的「外部」标签(chat_thread)与这道执行闸是同一个问题的
//! 两次提问。判据分居两个 crate 的第一天就漂移了——闸这边给 `can_operate` 传空 union_id,于是
//! 「名单里只配 union_id」的 owner 被判成外部人、被降只读,而配了 open_id 的真外部人反而全权。
//!
//! **为什么闸在这里而不是 `--permission-mode`**:permission mode 是会话级启动参数,而 per-chat 是
//! 共用会话;更要命的是 agent 跑在无人值守 tmux 里,一旦降级成需要确认,弹窗没人点会整轮卡死。
//!
//! **为什么是工具白名单而不是危险命令黑名单**:管道、变量、脚本、先写文件再执行 —— 绕过方式是
//! 开放集合,黑名单拦不住有意绕过的人。受闸工具由 settings 的 matcher 枚举(见 `session.rs`)。
//!
//! **为什么 fail-open**:只在「能证明发起人是外部人」时拒绝。owner `summon agents attach` 手工
//! 打字时没有 current-turn,fail-closed 会把 owner 自己锁在门外;而攻击面只是「daemon 写
//! current-turn 失败的那一瞬」——两者代价不对等。
//!
//! **按调用归属判**:主 Agent 让出本轮后,原轮起的后台任务与下一轮在同一个原生会话里并行。
//! 后台子 agent 的调用带 `agent_id`,按起它的那一轮(后台持有者)判;主 Agent 的调用按当前轮
//! 判,没有当前轮时按持有者判。持有者在场时归属不明一律取最严,不落入上面的缺记录放行。

use std::io::Read;
use std::path::Path;

use summon_core::authz::{tool_guard_verdict, GuardVerdict, TurnRequester};
use summon_core::registry::Registry;

use crate::observe::log::log;
use crate::runtime::background_holder::{BackgroundHolder, BackgroundHolderStore};
use crate::runtime::current_turn_store::{CurrentTurnOrigin, CurrentTurnStore, TurnActor};

/// hook 的整条 IO 链(注册表加载 → current-turn 定位 → 裁决),路径可注入以便集成测试。
///
/// 每一个「读不到就放行」的分支都留一行日志:fail-open 是有意设计,但**沉默的 fail-open 是事故**
/// ——闸失效时盘上不留任何痕迹,排查时无从分辨「没有外部人来过」与「闸根本没生效」。
pub(crate) fn hook_verdict(
    agents_dir: &Path,
    alias: &str,
    chat_id: &str,
    agent_id: Option<&str>,
    now_ms: i64,
) -> GuardVerdict {
    // 用 fail-soft 加载:一份别的 agent.yaml 坏掉不该让本 agent 的闸整体失效(`load` 是 fail-fast
    // 的诊断入口,生产路径不用它——见 `Registry::load` 文档)。本 agent 自己的清单坏掉时它不在
    // 结果里,下面找不到 → 放行,与旧行为一致。
    let registry = match Registry::load_tolerant(agents_dir) {
        Ok((registry, warnings)) => {
            for warning in warnings {
                log(&summon_i18n::format!("r80f89aa387"));
            }
            registry
        }
        Err(error) => {
            log(&summon_i18n::format!("r6062dd3490", agents_dir.display()));
            return GuardVerdict::Allow;
        }
    };
    let Some(agent) = registry.agents.into_iter().find(|a| a.name == alias) else {
        log(&summon_i18n::format!("re69b1d686b"));
        return GuardVerdict::Allow;
    };
    // hook 进程看不到 daemon 内存里的应用所有者,从 daemon 落的缓存装进来,与 prompt 标记同判;
    // 缓存按当前凭据的 app_id 校验。
    let (app_id, _) = crate::runtime::daemon::resolve_creds(&agent);
    crate::runtime::app_owner::load_cached(&agent, app_id.as_deref());
    // chat_type 取自 current-turn 而不是从 chat_id 前缀猜:飞书私聊的 chat_id 同样是 `oc_` 开头
    // (实测 `oc_` 开头的 id 配 `chat_type=p2p`),猜会把私聊判成群,名单皆空的 agent 在私聊里
    // 就会误拒 owner。后台持有者同理,记录里带的是让轮时那一轮的 chat_type。
    let current =
        CurrentTurnStore::for_agent(&agent.state_dir()).peek_turn_actor_by_chat(chat_id, now_ms);
    let holders = BackgroundHolderStore::for_agent(&agent.state_dir()).for_chat(chat_id, now_ms);
    // 一个都没有是高频合法路径(owner 手工 attach 打字),放行且不记日志——每次工具调用刷一行
    // 会淹掉真信号。
    for actor in responsible_actors(current, &holders, agent_id) {
        let verdict = actor_verdict(&actor, &agent, chat_id);
        if matches!(verdict, GuardVerdict::Deny(_)) {
            return verdict;
        }
    }
    GuardVerdict::Allow
}

/// 这次工具调用该按谁判。返回多位时取最严。
fn responsible_actors(
    current: Option<TurnActor>,
    holders: &[BackgroundHolder],
    agent_id: Option<&str>,
) -> Vec<TurnActor> {
    if let Some(agent_id) = agent_id {
        if let Some(holder) = holders.iter().find(|holder| holder.holds_agent(agent_id)) {
            return vec![holder.actor()];
        }
        // workflow 内部子 agent 的 agent_id 对不上 task-id,分不清是谁起的。
        if holders.iter().any(BackgroundHolder::holds_workflow) {
            return current
                .into_iter()
                .chain(holders.iter().map(BackgroundHolder::actor))
                .collect();
        }
    }
    match current {
        Some(actor) => vec![actor],
        // 没有当前轮时主 Agent 还在动工具,是在处理让轮后到达的后台完成通知。
        None => holders.iter().map(BackgroundHolder::actor).collect(),
    }
}

fn actor_verdict(
    actor: &TurnActor,
    agent: &summon_core::registry::Agent,
    chat_id: &str,
) -> GuardVerdict {
    // Schedule 由一轮本身有写权限的发起人建时授权(operate 名单含本群名单、peer、无身份的
    // 手工 attach),再由 daemon 自己触发。它没有飞书发信人；显式 origin 让它在发布 current-turn
    // capability 后仍保持既有全权语义，而不是伪装成 peer/user。
    if actor.origin == CurrentTurnOrigin::Schedule {
        return GuardVerdict::Allow;
    }
    tool_guard_verdict(
        Some(TurnRequester::new(
            &actor.open_id,
            &actor.union_id,
            actor.is_peer,
        )),
        agent,
        summon_core::authz::is_group_chat(&actor.chat_type),
        chat_id,
    )
}

/// hook 进程入口。拒绝走 `exit 2` + stderr(实测契约:工具不执行,stderr 原文交给模型)。
/// 任何解析/定位失败都放行——闸的作用域判定失败不等于该拦。
pub fn run_tool_guard_hook(agents_dir: &Path) {
    // stdin 必须读干净:不读会让 claude 侧写管道阻塞。受闸工具已由 settings 的 matcher 过滤,
    // 这里只取子 agent 内的调用带的 `agent_id`;读不出就按主 Agent 的调用判。
    let mut buffer = String::new();
    let _ = std::io::stdin().lock().read_to_string(&mut buffer);
    let agent_id = hook_agent_id(&buffer);

    let (Ok(alias), Ok(chat_id)) = (
        std::env::var("SUMMON_AGENT"),
        std::env::var("SUMMON_CHAT_ID"),
    ) else {
        // 不是 summon 管的会话(用户自己开的 claude):放行,且不记日志(这条路径可能极高频)。
        return;
    };
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or_default();
    if let GuardVerdict::Deny(reason) =
        hook_verdict(agents_dir, &alias, &chat_id, agent_id.as_deref(), now_ms)
    {
        eprintln!("{reason}");
        std::process::exit(2);
    }
}

fn hook_agent_id(stdin: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(stdin)
        .ok()?
        .get("agent_id")?
        .as_str()
        .filter(|id| !id.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::{HeldBackgroundTask, HeldTaskKind};
    use crate::runtime::current_turn_store::{CurrentTurnContext, CurrentTurnStore};

    const NOW: i64 = 1_700_000_000_000;

    /// 在 tempdir 里布置一个真 agents 目录:`<root>/<alias>/agent.yaml`。
    /// agent 落在 `tmp/agents/<alias>/`,**不能直接放 `tmp/<alias>/`** —— `state_dir()` 由
    /// `agents_dir.parent()` 推导 state_root,agents_dir 用 tempdir 根会让 state 落到系统临时
    /// 目录的共享路径上,并发测试互相串。调用方相应传 `dir.path().join("agents")` 作 agents_dir。
    fn agents_dir_with(alias: &str, feishu_yaml: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let agent_dir = dir.path().join("agents").join(alias);
        std::fs::create_dir_all(&agent_dir).unwrap();
        std::fs::write(
            agent_dir.join("agent.yaml"),
            format!("name: {alias}\n{feishu_yaml}"),
        )
        .unwrap();
        dir
    }

    /// 给某 agent 的某 chat 发布一条活跃 turn,返回 lease(必须持有到断言之后)。
    fn activate(
        agents_dir: &Path,
        alias: &str,
        chat_id: &str,
        open_id: &str,
        union_id: &str,
        is_peer: bool,
    ) -> crate::runtime::current_turn_store::CurrentTurnLease {
        let store = CurrentTurnStore::for_agent(&agents_dir.join(alias));
        store
            .activate(&CurrentTurnContext {
                turn_id: "turn-1".into(),
                agent_alias: alias.into(),
                delivery_scope: "scope".into(),
                chat_id: chat_id.into(),
                chat_type: "group".into(),
                conversation_tag: "g_tag".into(),
                message_id: "om_1".into(),
                origin: CurrentTurnOrigin::Feishu,
                schedule_output: summon_core::ScheduleOutput::Progress,
                requester_open_id: open_id.into(),
                requester_union_id: union_id.into(),
                requester_is_bot: false,
                requester_is_peer: is_peer,
                thread_id: None,
                root_id: None,
                started_at_ms: NOW,
                reattached: false,
            })
            .unwrap()
    }

    /// L10:hook 的 IO 链此前零测试。三态各钉一条——放行 / 拒绝 / 定位失败 fail-open。
    #[test]
    fn hook_denies_outsider_turn_end_to_end() {
        let dir = agents_dir_with(
            "arcadia",
            "feishu:\n  app_id_env: A\n  operators:\n    - ou_owner\n",
        );
        let _lease = activate(
            &dir.path().join("agents"),
            "arcadia",
            "oc_chat",
            "ou_outsider",
            "",
            false,
        );
        assert!(matches!(
            hook_verdict(&dir.path().join("agents"), "arcadia", "oc_chat", None, NOW),
            GuardVerdict::Deny(_)
        ));
    }

    #[test]
    fn hook_allows_owner_turn_end_to_end() {
        let dir = agents_dir_with(
            "arcadia",
            "feishu:\n  app_id_env: A\n  operators:\n    - ou_owner\n",
        );
        let _lease = activate(
            &dir.path().join("agents"),
            "arcadia",
            "oc_chat",
            "ou_owner",
            "",
            false,
        );
        assert_eq!(
            hook_verdict(&dir.path().join("agents"), "arcadia", "oc_chat", None, NOW),
            GuardVerdict::Allow
        );
    }

    /// 名单皆空的群里,hook 进程从 daemon 落的缓存认出应用所有者,与 prompt 的外部标记同判;
    /// 凭据换成别的应用后,上一个应用的所有者不算数。
    #[test]
    fn hook_allows_app_owner_from_daemon_cache_when_lists_are_empty() {
        let dir = agents_dir_with("aodi", "feishu:\n  app_id_env: AODI_HOOK_TEST_APP_ID\n");
        let agents = dir.path().join("agents");
        let verdict = |open_id: &str| {
            let _lease = activate(&agents, "aodi", "oc_chat", open_id, "", false);
            hook_verdict(&agents, "aodi", "oc_chat", None, NOW)
        };
        let env = agents.join("aodi/.env");
        std::fs::write(&env, "AODI_HOOK_TEST_APP_ID=cli_x\n").unwrap();
        assert!(matches!(verdict("ou_app_owner"), GuardVerdict::Deny(_)));
        std::fs::write(
            agents.join("aodi/runtime/app-owner.json"),
            r#"{"app_id":"cli_x","owner":{"open_id":"ou_app_owner","union_id":"on_app_owner"}}"#,
        )
        .unwrap();
        assert_eq!(verdict("ou_app_owner"), GuardVerdict::Allow);
        assert!(matches!(verdict("ou_outsider"), GuardVerdict::Deny(_)));
        std::fs::write(&env, "AODI_HOOK_TEST_APP_ID=cli_y\n").unwrap();
        assert!(matches!(verdict("ou_app_owner"), GuardVerdict::Deny(_)));
    }

    /// H1 端到端:名单里只有 owner 的 union_id。open_id 对不上、union_id 对得上,
    /// 整条 IO 链必须放行 —— 这是修复前 owner 被自己的闸锁在门外的那条路径。
    #[test]
    fn hook_allows_owner_listed_only_by_union_id() {
        let dir = agents_dir_with(
            "flare-5",
            "feishu:\n  app_id_env: A\n  allowed_users:\n    - on_owner_union\n",
        );
        let _lease = activate(
            &dir.path().join("agents"),
            "flare-5",
            "oc_chat",
            "ou_owner_in_this_app",
            "on_owner_union",
            false,
        );
        assert_eq!(
            hook_verdict(&dir.path().join("agents"), "flare-5", "oc_chat", None, NOW),
            GuardVerdict::Allow,
            "只按 union_id 配名单的 owner 必须全权,否则策略整体反转"
        );
    }

    /// M6 端到端:peer bot 发起的轮不该被闸。
    #[test]
    fn hook_allows_introduced_peer_bot_turn() {
        let dir = agents_dir_with(
            "arcadia",
            "feishu:\n  app_id_env: A\n  operators:\n    - ou_owner\n",
        );
        let _lease = activate(
            &dir.path().join("agents"),
            "arcadia",
            "oc_chat",
            "ou_peer_bot",
            "",
            true,
        );
        assert_eq!(
            hook_verdict(&dir.path().join("agents"), "arcadia", "oc_chat", None, NOW),
            GuardVerdict::Allow
        );
    }

    #[test]
    fn hook_allows_owner_scheduled_turn_with_no_requester() {
        let dir = agents_dir_with(
            "arcadia",
            "feishu:\n  app_id_env: A\n  operators:\n    - ou_owner\n",
        );
        let store = CurrentTurnStore::for_agent(&dir.path().join("agents").join("arcadia"));
        let _lease = store
            .activate(&CurrentTurnContext {
                turn_id: "turn-scheduled".into(),
                agent_alias: "arcadia".into(),
                delivery_scope: "scope".into(),
                chat_id: "oc_chat".into(),
                chat_type: "group".into(),
                conversation_tag: "g_tag".into(),
                message_id: "sched-1".into(),
                origin: CurrentTurnOrigin::Schedule,
                schedule_output: summon_core::ScheduleOutput::Progress,
                requester_open_id: String::new(),
                requester_union_id: String::new(),
                requester_is_bot: false,
                requester_is_peer: false,
                thread_id: None,
                root_id: None,
                started_at_ms: NOW,
                reattached: false,
            })
            .unwrap();
        assert_eq!(
            hook_verdict(&dir.path().join("agents"), "arcadia", "oc_chat", None, NOW),
            GuardVerdict::Allow,
            "发布 capability 不能把既有定时任务误判成外部用户轮"
        );
    }

    #[test]
    fn hook_fails_open_when_turn_or_agent_cannot_be_located() {
        let dir = agents_dir_with(
            "arcadia",
            "feishu:\n  app_id_env: A\n  operators:\n    - ou_owner\n",
        );
        // 没有活跃 turn:owner 手工 attach 打字的合法路径。
        assert_eq!(
            hook_verdict(&dir.path().join("agents"), "arcadia", "oc_chat", None, NOW),
            GuardVerdict::Allow
        );
        // 注册表里没有这个 alias(清单损坏 / 已下线)。
        let _lease = activate(
            &dir.path().join("agents"),
            "arcadia",
            "oc_chat",
            "ou_outsider",
            "",
            false,
        );
        assert_eq!(
            hook_verdict(&dir.path().join("agents"), "ghost", "oc_chat", None, NOW),
            GuardVerdict::Allow
        );
        // agents 目录整个不存在。
        assert_eq!(
            hook_verdict(&dir.path().join("nope"), "arcadia", "oc_chat", None, NOW),
            GuardVerdict::Allow
        );
    }

    /// 一份坏清单不该连坐掉别的 agent 的闸(`load_tolerant` 而非 `load` 的意义)。
    #[test]
    fn broken_sibling_manifest_does_not_disable_the_gate() {
        let dir = agents_dir_with(
            "arcadia",
            "feishu:\n  app_id_env: A\n  operators:\n    - ou_owner\n",
        );
        let broken = dir.path().join("broken");
        std::fs::create_dir_all(&broken).unwrap();
        std::fs::write(broken.join("agent.yaml"), "name: [不是合法清单\n").unwrap();
        let _lease = activate(
            &dir.path().join("agents"),
            "arcadia",
            "oc_chat",
            "ou_outsider",
            "",
            false,
        );
        assert!(
            matches!(
                hook_verdict(&dir.path().join("agents"), "arcadia", "oc_chat", None, NOW),
                GuardVerdict::Deny(_)
            ),
            "邻居清单坏掉时闸必须照常生效,不能整体 fail-open"
        );
    }

    /// 让出本轮时原轮发起人留下的后台持有者。
    fn hold(agents_dir: &Path, open_id: &str, tasks: &[(&str, HeldTaskKind)]) {
        BackgroundHolderStore::for_agent(&agents_dir.join("arcadia"))
            .add(
                "g_tag",
                BackgroundHolder {
                    turn_id: "turn-held".into(),
                    message_id: "om_held".into(),
                    chat_id: "oc_chat".into(),
                    chat_type: "group".into(),
                    origin: CurrentTurnOrigin::Feishu,
                    requester_open_id: open_id.into(),
                    requester_union_id: String::new(),
                    requester_is_peer: false,
                    tmux_session: "soul-arcadia".into(),
                    session_id: "sid".into(),
                    transcript: agents_dir.join("t.jsonl"),
                    tasks: tasks
                        .iter()
                        .map(|(id, kind)| HeldBackgroundTask {
                            task_id: (*id).into(),
                            kind: *kind,
                        })
                        .collect(),
                    created_at_ms: NOW,
                    expires_at_ms: NOW + 60_000,
                },
            )
            .unwrap();
    }

    fn owner_only() -> tempfile::TempDir {
        agents_dir_with(
            "arcadia",
            "feishu:\n  app_id_env: A\n  operators:\n    - ou_owner\n",
        )
    }

    fn denied(verdict: GuardVerdict) -> bool {
        matches!(verdict, GuardVerdict::Deny(_))
    }

    /// 外部发起人让出本轮后,它的后台子 agent 不因 owner 开了新一轮而获得写权限;owner 这一轮
    /// 的主 Agent 与它自己起的子 agent 照常可写。
    #[test]
    fn outsider_background_agent_stays_read_only_under_an_owner_turn() {
        let dir = owner_only();
        let agents = dir.path().join("agents");
        hold(&agents, "ou_outsider", &[("a_bg", HeldTaskKind::Agent)]);
        let _lease = activate(&agents, "arcadia", "oc_chat", "ou_owner", "", false);
        assert!(denied(hook_verdict(
            &agents,
            "arcadia",
            "oc_chat",
            Some("a_bg"),
            NOW
        )));
        assert_eq!(
            hook_verdict(&agents, "arcadia", "oc_chat", None, NOW),
            GuardVerdict::Allow
        );
        assert_eq!(
            hook_verdict(&agents, "arcadia", "oc_chat", Some("a_owner_sub"), NOW),
            GuardVerdict::Allow
        );
    }

    /// 反向:owner 的后台子 agent 在外部发起人的轮里仍按 owner 判,外部发起人这一轮照旧只读。
    #[test]
    fn owner_background_agent_keeps_write_access_under_an_outsider_turn() {
        let dir = owner_only();
        let agents = dir.path().join("agents");
        hold(&agents, "ou_owner", &[("a_bg", HeldTaskKind::Agent)]);
        let _lease = activate(&agents, "arcadia", "oc_chat", "ou_outsider", "", false);
        assert_eq!(
            hook_verdict(&agents, "arcadia", "oc_chat", Some("a_bg"), NOW),
            GuardVerdict::Allow
        );
        assert!(denied(hook_verdict(
            &agents, "arcadia", "oc_chat", None, NOW
        )));
        assert!(denied(hook_verdict(
            &agents,
            "arcadia",
            "oc_chat",
            Some("a_out_sub"),
            NOW
        )));
    }

    /// 没有当前轮(让轮到下一轮发布之间、或主 Agent 空闲时处理后台完成通知):按持有者判,
    /// 不落入缺记录放行;持有者过期后回到原有放行。
    #[test]
    fn without_a_current_turn_holders_decide_instead_of_failing_open() {
        let dir = owner_only();
        let agents = dir.path().join("agents");
        hold(&agents, "ou_outsider", &[("a_bg", HeldTaskKind::Agent)]);
        assert!(denied(hook_verdict(
            &agents, "arcadia", "oc_chat", None, NOW
        )));
        assert!(denied(hook_verdict(
            &agents,
            "arcadia",
            "oc_chat",
            Some("a_bg"),
            NOW
        )));
        assert_eq!(
            hook_verdict(&agents, "arcadia", "oc_chat", None, NOW + 60_000),
            GuardVerdict::Allow
        );
        assert_eq!(
            hook_verdict(&agents, "arcadia", "oc_other", None, NOW),
            GuardVerdict::Allow,
            "别的 chat 不受影响"
        );
    }

    /// 多个持有者、没有当前轮时取最严。
    #[test]
    fn several_holders_without_a_current_turn_take_the_strictest() {
        let dir = owner_only();
        let agents = dir.path().join("agents");
        hold(&agents, "ou_owner", &[("a_owner", HeldTaskKind::Agent)]);
        hold(&agents, "ou_outsider", &[("a_out", HeldTaskKind::Shell)]);
        assert!(denied(hook_verdict(
            &agents, "arcadia", "oc_chat", None, NOW
        )));
        assert_eq!(
            hook_verdict(&agents, "arcadia", "oc_chat", Some("a_owner"), NOW),
            GuardVerdict::Allow
        );
    }

    /// workflow 内部子 agent 的 agent_id 认不出归属:当前轮与持有者取最严。
    #[test]
    fn unknown_agent_under_a_held_workflow_takes_the_strictest() {
        let dir = owner_only();
        let agents = dir.path().join("agents");
        hold(&agents, "ou_outsider", &[("wf_1", HeldTaskKind::Workflow)]);
        let _lease = activate(&agents, "arcadia", "oc_chat", "ou_owner", "", false);
        assert!(denied(hook_verdict(
            &agents,
            "arcadia",
            "oc_chat",
            Some("a_inner"),
            NOW
        )));
        assert_eq!(
            hook_verdict(&agents, "arcadia", "oc_chat", None, NOW),
            GuardVerdict::Allow
        );
    }

    /// claude 2.1.309 实测形态:子 agent 内的调用带 `agent_id`,主 Agent 的不带。
    #[test]
    fn hook_reads_agent_id_from_pre_tool_use_input() {
        let sub = r#"{"session_id":"s","prompt_id":"p","transcript_path":"/tmp/t.jsonl","cwd":"/tmp","permission_mode":"bypassPermissions","agent_id":"ad88667921425caf8","agent_type":"general-purpose","hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"echo probe"},"tool_use_id":"toolu_1"}"#;
        let main = r#"{"session_id":"s","prompt_id":"p","transcript_path":"/tmp/t.jsonl","cwd":"/tmp","permission_mode":"bypassPermissions","hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"echo probe"},"tool_use_id":"toolu_2"}"#;
        assert_eq!(hook_agent_id(sub).as_deref(), Some("ad88667921425caf8"));
        assert_eq!(hook_agent_id(main), None);
        assert_eq!(hook_agent_id(r#"{"agent_id":""}"#), None);
        assert_eq!(hook_agent_id("not json"), None);
    }

    /// 完成通知已入账、主 Agent 还在处理它时,整理不能清掉持有者:否则没有当前轮的那段处理
    /// 会落回缺记录放行。主 Agent 对通知收尾(end_turn)之后才清。
    #[test]
    fn holder_outlives_the_notification_until_the_main_agent_answers_it() {
        let dir = owner_only();
        let agents = dir.path().join("agents");
        let transcript = agents.join("t.jsonl");
        let launched = r#"{"type":"user","isSidechain":false,"message":{"role":"user","content":[{"tool_use_id":"tu_agent","type":"tool_result","content":[{"type":"text","text":"Async agent launched successfully."}]}]},"toolUseResult":{"isAsync":true,"status":"async_launched","agentId":"a_bg","description":"d","prompt":"p"}}"#;
        let notified = serde_json::json!({
            "type": "queue-operation",
            "operation": "enqueue",
            "content": "<task-notification>\n<task-id>a_bg</task-id>\n<tool-use-id>tu_agent</tool-use-id>\n<status>completed</status>\n<summary>done</summary>\n</task-notification>",
        });
        std::fs::write(&transcript, format!("{launched}\n{notified}\n")).unwrap();
        hold(&agents, "ou_outsider", &[("a_bg", HeldTaskKind::Agent)]);
        let store = BackgroundHolderStore::for_agent(&agents.join("arcadia"));
        let prune = || {
            store
                .prune("g_tag", |holder| {
                    crate::runtime::background_holder::still_running(holder, Some("sid"), true, NOW)
                })
                .unwrap()
        };
        prune();
        assert!(denied(hook_verdict(
            &agents, "arcadia", "oc_chat", None, NOW
        )));

        let answered = r#"{"type":"assistant","isSidechain":false,"message":{"role":"assistant","content":[{"type":"text","text":"done"}],"stop_reason":"end_turn"}}"#;
        let consumed = serde_json::json!({
            "type": "user",
            "isSidechain": false,
            "message": {"role": "user", "content": notified["content"]},
        });
        std::fs::write(&transcript, format!("{launched}\n{notified}\n{answered}\n")).unwrap();
        prune();
        assert!(
            denied(hook_verdict(&agents, "arcadia", "oc_chat", None, NOW)),
            "通知还排在队列里时的 end_turn 属于别的轮"
        );
        std::fs::write(
            &transcript,
            format!("{launched}\n{notified}\n{answered}\n{consumed}\n"),
        )
        .unwrap();
        prune();
        assert!(denied(hook_verdict(
            &agents, "arcadia", "oc_chat", None, NOW
        )));
        std::fs::write(
            &transcript,
            format!("{launched}\n{notified}\n{answered}\n{consumed}\n{answered}\n"),
        )
        .unwrap();
        prune();
        assert!(!store.has_any("g_tag"));
        assert_eq!(
            hook_verdict(&agents, "arcadia", "oc_chat", None, NOW),
            GuardVerdict::Allow
        );
    }
}
