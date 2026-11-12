//! 群内免 @ 配置：后台预检，dispatcher 在当前授权与配置代际下提交。

use super::{Agent, CommandAck, DispatchMsg, FeishuClient, Inbound};
use crate::runtime::dashboard::api::MentionlessDone;
use anyhow::Context;
use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, Sender, SyncSender};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};
use summon_core::config::registry::{AppOwnerState, OperateDenial, OperateScope};

const TTL: Duration = Duration::from_secs(10);
const SCOPES: &[&str] = &[
    "im:message.group_msg",
    "im:chat:read",
    "im:chat.members:read",
];
pub(super) const HELP: &str = "r3e4c9e62c4";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Command {
    Help,
    On,
    Off,
    Permissions,
}

pub(super) fn command(msg: &Inbound) -> Option<Command> {
    let text = summon_core::ipc::strip_mentions_text(&msg.text, &msg.mentions);
    let mut args = text.split_whitespace();
    if !args.next()?.eq_ignore_ascii_case("/mentionless") {
        return None;
    }
    let action = args.next().unwrap_or("").to_ascii_lowercase();
    if args.next().is_some() {
        return Some(Command::Help);
    }
    Some(match action.as_str() {
        "on" => Command::On,
        "off" => Command::Off,
        "permissions" => Command::Permissions,
        _ => Command::Help,
    })
}

pub(super) fn authorized(agent: &Agent, msg: &Inbound) -> bool {
    denial(agent, msg).is_none()
}

/// 授权判定与拒绝说明:拒绝时说清缺哪项权限、改 agent.yaml 的哪个字段;放行返回 None。
pub(super) fn denial(agent: &Agent, msg: &Inbound) -> Option<String> {
    if agent.is_steward()
        || !agent.is_routable()
        || !matches!(msg.chat_type.as_deref(), Some("group" | "topic_group"))
        || msg.chat_id.is_empty()
    {
        return Some(summon_i18n::text!("r10fcda9512").into());
    }
    if msg.sender_type != "user" || msg.sender.is_empty() {
        return Some(summon_i18n::text!("rabb79ad7bc").into());
    }
    let name = &agent.name;
    if !agent.allows(&msg.sender, &msg.sender_union_id) {
        // 对话名单不通过的消息在入站就被静默丢弃，走到这里只可能是后台复核时名单变了。
        return Some(summon_i18n::format!("re6530eb597"));
    }
    let denial = agent.operate_denial(
        &msg.sender,
        &msg.sender_union_id,
        true,
        OperateScope::Chat(&msg.chat_id),
    )?;
    // union_id 跨 app 稳定，优先给它；取不到时 open_id 在本 app 下同样能命中名单。
    let (id, unresolved) = if msg.sender_union_id.is_empty() {
        (&msg.sender, summon_i18n::text!("r6e9f2a6a99"))
    } else {
        (&msg.sender_union_id, "")
    };
    let chat = &msg.chat_id;
    let (why, fix) = match denial {
        // 两份名单都空时私聊人人可操作；写 operators 会让它成为唯一名单、收走别人的私聊操作权，
        // 所以先给只放宽不收窄的本群名单。
        OperateDenial::NoListConfigured => (
            summon_i18n::format!(
                "r9384dc062e",
                match agent.app_owner.get() {
                    AppOwnerState::Known(Some(_)) => summon_i18n::text!("ra759c035be"),
                    AppOwnerState::Known(None) => summon_i18n::text!("r15b388b00e"),
                    AppOwnerState::Unknown => summon_i18n::text!("rf19998b05f"),
                }
            ),
            summon_i18n::format!("rad92f010d8"),
        ),
        OperateDenial::NotListed(list) => {
            let field = list.field();
            (
                summon_i18n::format!("r822c267417"),
                summon_i18n::format!("rd0bbd572b8"),
            )
        }
    };
    Some(summon_i18n::format!("r7c2f5425c9"))
}

/// 事件没带 union_id 时与其它 operate 闸点一样回退联系人 API(见 `operator_union_id`)。
/// 解析结果随命令进后台检查,写配置前的复核沿用同一份,不重复联网。
pub(super) fn with_operator_union_id(msg: &Inbound, resolve: impl FnOnce() -> String) -> Inbound {
    let mut msg = msg.clone();
    msg.sender_union_id = super::dispatcher::operator_union_id(&msg.sender_union_id, resolve);
    msg
}

struct Request {
    msg: Inbound,
    command: Command,
    manifest: String,
    mtime: SystemTime,
    started: Instant,
    canceled: Arc<AtomicBool>,
    /// 控制台发起的核验:结果交回页面,不往群里发。
    dashboard: Option<Sender<MentionlessDone>>,
}

/// 控制台的免 @ 开启:身份来自卡片点击人(签发 agent 的 open_id / union_id),与群里发
/// `/mentionless on` 进同一条队列、过同一段核验。
pub(crate) struct DashboardRequest {
    pub(crate) msg: Inbound,
    pub(crate) reply: Sender<MentionlessDone>,
}

/// 申请权限的链接(核验回复里 [`permission_reply`] 写的那一条)。
fn apply_url(reply: &str) -> Option<String> {
    let start = reply.find(::summon_core::i18n::label(APPLY_LINK))?
        + ::summon_core::i18n::label(APPLY_LINK).len();
    let end = reply[start..].find(')')?;
    Some(reply[start..start + end].to_string())
}

const APPLY_LINK: &str = "red6142bb71";

enum Outcome {
    Ready,
    Reply(String),
}
pub(crate) struct Checked {
    request: Request,
    outcome: Outcome,
}

impl Checked {
    pub(super) fn message(&self) -> &Inbound {
        &self.request.msg
    }
    /// 控制台发起的核验(结果交回页面,不往群里发)。
    pub(super) fn asked_by_dashboard(&self) -> bool {
        self.request.dashboard.is_some()
    }
    /// 群里的回执:开成了是纯确认,其余(没开成、权限查询结果)照常发文字。
    pub(super) fn finish(self, agent: &Agent, identity_ok: impl Fn(&Agent) -> bool) -> CommandAck {
        let dashboard = self.request.dashboard.clone();
        let (reply, enabled) = self.settle(agent, identity_ok);
        if let Some(tx) = dashboard {
            // 页面已关掉时没人收,丢掉即可。
            let _ = tx.send(MentionlessDone {
                ok: enabled,
                apply_url: apply_url(&reply),
                message: reply.clone(),
            });
        }
        if enabled {
            CommandAck::Done(reply)
        } else {
            CommandAck::Note(reply)
        }
    }
    /// 回复文案与是否真的开启了。
    fn settle(self, agent: &Agent, identity_ok: impl Fn(&Agent) -> bool) -> (String, bool) {
        let request = self.request;
        if request.canceled.load(Ordering::Acquire)
            || !identity_ok(agent)
            || !authorized(agent, &request.msg)
            || agent.manifest_mtime() != Some(request.mtime)
            || (matches!(&self.outcome, Outcome::Ready) && request.started.elapsed() > TTL)
        {
            return (summon_i18n::text!("r1f7ff37bac").into(), false);
        }
        match self.outcome {
            Outcome::Reply(reply) => (reply, false),
            Outcome::Ready => match write_config_checked(
                agent,
                &request.msg,
                true,
                &request.manifest,
                request.mtime,
                &|current| request.started.elapsed() <= TTL && identity_ok(current),
            ) {
                Ok(()) => (summon_i18n::text!("r2c03a72d38").into(), true),
                Err(error) => (summon_i18n::format!("r53eb2f56b9"), false),
            },
        }
    }
}

pub(super) struct Queue {
    tx: SyncSender<Request>,
    stopped: Arc<AtomicBool>,
    pending: HashMap<String, Arc<AtomicBool>>,
}
impl Queue {
    pub(super) fn new(fc: Arc<FeishuClient>, dispatch: Sender<DispatchMsg>) -> Self {
        let (tx, rx) = sync_channel::<Request>(4);
        let stopped = Arc::new(AtomicBool::new(false));
        let stop = stopped.clone();
        std::thread::spawn(move || {
            while let Ok(request) = rx.recv() {
                if stop.load(Ordering::Acquire) {
                    break;
                }
                let outcome = if request.started.elapsed() > TTL {
                    Outcome::Reply(summon_i18n::text!("r6b7497e590").into())
                } else {
                    preflight(&fc, &request.msg, request.command)
                };
                if stop.load(Ordering::Acquire) {
                    break;
                }
                if dispatch
                    .send(DispatchMsg::MentionlessSetupChecked(Checked {
                        request,
                        outcome,
                    }))
                    .is_err()
                {
                    break;
                }
            }
        });
        Self {
            tx,
            stopped,
            pending: HashMap::new(),
        }
    }
    pub(super) fn submit(
        &mut self,
        agent: &Agent,
        msg: Inbound,
        command: Command,
        expected_mtime: Option<SystemTime>,
    ) -> anyhow::Result<()> {
        self.submit_with(agent, msg, command, expected_mtime, None)
    }
    /// 控制台发起的开启。进不了队列(授权、配置代际、队列繁忙、本代身份已变)就当场把原因交回页面。
    pub(super) fn submit_from_dashboard(
        &mut self,
        agent: &Agent,
        request: DashboardRequest,
        expected_mtime: Option<SystemTime>,
        identity_blocked: bool,
    ) {
        let DashboardRequest { msg, reply } = request;
        let refused = if identity_blocked {
            Err(anyhow::anyhow!(summon_i18n::format!("r21f28fd2dd")))
        } else {
            self.submit_with(agent, msg, Command::On, expected_mtime, Some(reply.clone()))
        };
        if let Err(error) = refused {
            let _ = reply.send(MentionlessDone {
                ok: false,
                message: summon_i18n::format!("rf9d8a3e71d"),
                apply_url: None,
            });
        }
    }
    fn submit_with(
        &mut self,
        agent: &Agent,
        msg: Inbound,
        command: Command,
        expected_mtime: Option<SystemTime>,
        dashboard: Option<Sender<MentionlessDone>>,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            matches!(command, Command::On | Command::Permissions),
            summon_i18n::format!("rcd37b6c59d")
        );
        if let Some(reason) = denial(agent, &msg) {
            anyhow::bail!(reason);
        }
        let mtime = agent
            .manifest_mtime()
            .context(summon_i18n::text!("rd2989a7b3f"))?;
        anyhow::ensure!(
            Some(mtime) == expected_mtime,
            summon_i18n::format!("r5f873e34a8")
        );
        let manifest = std::fs::read_to_string(agent.dir.join("agent.yaml"))?;
        anyhow::ensure!(
            agent.manifest_mtime() == Some(mtime),
            summon_i18n::format!("r5f873e34a8")
        );
        let chat_id = msg.chat_id.clone();
        let canceled = Arc::new(AtomicBool::new(false));
        self.tx
            .try_send(Request {
                msg,
                command,
                manifest,
                mtime,
                started: Instant::now(),
                canceled: canceled.clone(),
                dashboard,
            })
            .map_err(|_| anyhow::anyhow!(summon_i18n::format!("rca14110755")))?;
        if command == Command::On {
            self.cancel(&chat_id);
            self.pending.insert(chat_id, canceled);
        }
        Ok(())
    }
    pub(super) fn cancel(&mut self, chat_id: &str) {
        if let Some(pending) = self.pending.remove(chat_id) {
            pending.store(true, Ordering::Release);
        }
    }
    pub(super) fn complete(&mut self, checked: &Checked) {
        let request = &checked.request;
        if self
            .pending
            .get(&request.msg.chat_id)
            .is_some_and(|pending| Arc::ptr_eq(pending, &request.canceled))
        {
            self.pending.remove(&request.msg.chat_id);
        }
    }
}
impl Drop for Queue {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
    }
}

fn permission_reply(fc: &FeishuClient, scopes: &[&str], reason: &str) -> String {
    summon_i18n::format!(
        "r745aafe7a8",
        fc.mentionless_permission_url(scopes),
        APPLY_LINK = ::summon_core::i18n::label(APPLY_LINK)
    )
}

fn preflight(fc: &FeishuClient, msg: &Inbound, command: Command) -> Outcome {
    // 群信息读得到就先看人数:群本身不满足时,引导用户去申请权限、等管理员审批是白跑一趟。
    if command == Command::On {
        if let Ok(Err(facts)) = fc.mentionless_chat_topology(&msg.chat_id) {
            return Outcome::Reply(summon_i18n::format!("rfdb891f97f"));
        }
    }
    match fc.mentionless_missing_permissions() {
        Err(_) => {
            return Outcome::Reply(permission_reply(
                fc,
                SCOPES,
                summon_i18n::text!("r19a968c748"),
            ))
        }
        Ok(missing) if !missing.is_empty() => {
            return Outcome::Reply(permission_reply(
                fc,
                &missing,
                &summon_i18n::format!("r4aec757bdf", missing.join(", ")),
            ))
        }
        Ok(_) => {}
    }
    if command == Command::Permissions {
        return Outcome::Reply(summon_i18n::text!("rfe16d5f359").into());
    }
    match fc.prove_mentionless_chat(&msg.chat_id, &msg.sender) {
        Ok(()) => Outcome::Ready,
        Err(error) => Outcome::Reply(summon_i18n::format!("r2f44ed343e")),
    }
}

pub(super) fn turn_off_checked(
    agent: &Agent,
    msg: &Inbound,
    expected_mtime: Option<SystemTime>,
    identity_ok: impl Fn(&Agent) -> bool,
) -> anyhow::Result<()> {
    let mtime = agent
        .manifest_mtime()
        .context(summon_i18n::text!("rd2989a7b3f"))?;
    anyhow::ensure!(
        Some(mtime) == expected_mtime,
        summon_i18n::format!("r5f873e34a8")
    );
    let manifest = std::fs::read_to_string(agent.dir.join("agent.yaml"))?;
    write_config_checked(agent, msg, false, &manifest, mtime, &identity_ok)
}

/// 改写 `feishu.chat_reply_without_mention.<chat_id>`(认得 `/mentionless on` 自己写出的行内映射)。
/// 控制台关闭免 @ 也用它,写法与群里的命令一致。
pub(crate) fn patch(manifest: &str, chat_id: &str, enabled: bool) -> anyhow::Result<String> {
    use serde_yaml::Value;
    let mut expected: Value = serde_yaml::from_str(manifest)?;
    let root = expected
        .as_mapping_mut()
        .context(summon_i18n::text!("r0ef404ac39"))?;
    let feishu = root
        .entry(Value::from("feishu"))
        .or_insert_with(|| Value::Mapping(Default::default()));
    let feishu = feishu
        .as_mapping_mut()
        .context(summon_i18n::text!("r1788199ba1"))?;
    let map = feishu
        .entry(Value::from("chat_reply_without_mention"))
        .or_insert_with(|| Value::Mapping(Default::default()));
    let map = map
        .as_mapping_mut()
        .context(summon_i18n::text!("rf588072b09"))?;
    if map.get(Value::from(chat_id)) == Some(&Value::from(enabled)) {
        return Ok(manifest.to_string());
    }
    map.insert(Value::from(chat_id), Value::from(enabled));
    let inline = serde_json::to_string(map)?;
    // 只折叠目标键的子块；其余文本由通用行编辑器保留。解析值等价校验守住 YAML 特殊写法。
    let newline = if manifest.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    };
    let mut lines: Vec<&str> = manifest.lines().collect();
    if let Some(start) = lines.iter().position(|line| line.starts_with("feishu:")) {
        let end = (start + 1..lines.len())
            .find(|&i| !lines[i].trim().is_empty() && !lines[i].starts_with([' ', '\t', '#']))
            .unwrap_or(lines.len());
        if let Some(target) = (start + 1..end).find(|&i| {
            lines[i]
                .trim_start()
                .starts_with("chat_reply_without_mention:")
        }) {
            let indent = lines[target].len() - lines[target].trim_start().len();
            let tail = (target + 1..end)
                .find(|&i| {
                    !lines[i].trim().is_empty()
                        && lines[i].len() - lines[i].trim_start().len() <= indent
                })
                .unwrap_or(end);
            lines.drain(target + 1..tail);
        }
    }
    let mut collapsed = lines.join(newline);
    if manifest.ends_with('\n') {
        collapsed.push_str(newline);
    }
    let out = crate::setup::config_edit::patch_section_key(
        &collapsed,
        "feishu",
        "chat_reply_without_mention",
        &inline,
    )?;
    let actual: Value = serde_yaml::from_str(&out)?;
    anyhow::ensure!(actual == expected, summon_i18n::format!("r49c3b466a6"));
    Ok(out)
}

fn write_config_checked(
    agent: &Agent,
    msg: &Inbound,
    enabled: bool,
    manifest: &str,
    mtime: SystemTime,
    identity_ok: &impl Fn(&Agent) -> bool,
) -> anyhow::Result<()> {
    anyhow::ensure!(authorized(agent, msg), summon_i18n::format!("rd180f32f90"));
    let path = agent.dir.join("agent.yaml");
    let _manifest_lock = crate::setup::bind::manifest_write_lock(&path)?;
    let current = std::fs::read_to_string(&path)?;
    anyhow::ensure!(
        current == manifest && agent.manifest_mtime() == Some(mtime),
        summon_i18n::format!("r1015af2c38")
    );
    let fresh = agent.reread().context(summon_i18n::text!("r91f415515e"))?;
    anyhow::ensure!(
        identity_ok(&fresh) && authorized(&fresh, msg),
        summon_i18n::format!("r234f32579f")
    );
    let out = patch(&current, &msg.chat_id, enabled)?;
    if current == out {
        return Ok(());
    }
    let mode = std::fs::metadata(&path)?.permissions().mode() & 0o777;
    anyhow::ensure!(
        std::fs::read_to_string(&path)? == current && agent.manifest_mtime() == Some(mtime),
        summon_i18n::format!("r5f873e34a8")
    );
    crate::runtime::fsio::atomic_write_mode_checked(&path, out.as_bytes(), mode, || {
        let fresh = agent.reread().context(summon_i18n::text!("r91f415515e"))?;
        anyhow::ensure!(
            identity_ok(&fresh) && authorized(&fresh, msg),
            summon_i18n::format!("r234f32579f")
        );
        let metadata = std::fs::metadata(&path)?;
        anyhow::ensure!(
            std::fs::read_to_string(&path)? == current
                && metadata.modified()? == mtime
                && metadata.permissions().mode() & 0o777 == mode,
            summon_i18n::format!("r1015af2c38")
        );
        Ok(())
    })
}

#[cfg(test)]
fn write_config(
    agent: &Agent,
    msg: &Inbound,
    enabled: bool,
    manifest: &str,
    mtime: SystemTime,
) -> anyhow::Result<()> {
    write_config_checked(agent, msg, enabled, manifest, mtime, &|_| true)
}

#[cfg(test)]
fn turn_off(agent: &Agent, msg: &Inbound, mtime: Option<SystemTime>) -> anyhow::Result<()> {
    turn_off_checked(agent, msg, mtime, |_| true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::mpsc::channel;

    fn agent() -> (Agent, tempfile::TempDir) {
        crate::test_support::agent_dir("claude", "feishu:\n  app_id_env: MENTIONLESS_TEST_APP\n  allowed_users: [ou_owner]\n  operators: [ou_owner]\n  chat_reply_without_mention: {oc_other: true}\nmessaging:\n  session_mirror: false\n")
    }
    fn msg() -> Inbound {
        serde_json::from_value(json!({"chat_id":"oc_target","chat_type":"group","text":"@_user_1 /mentionless on","sender":"ou_owner","sender_union_id":"","sender_type":"user","message_id":"om_setup","parent_id":null,"thread_id":null,"root_id":null,"resources":[],"mentions":[{"key":"@_user_1","open_id":"ou_bot"}]})).unwrap()
    }
    fn grants(names: &[&str], kind: &str, status: i32) -> String {
        json!({"code":0,"data":{"scopes":names.iter().map(|name| json!({"scope_name":name,"scope_type":kind,"grant_status":status})).collect::<Vec<_>>()}}).to_string()
    }
    struct Http {
        base: String,
        stop: Sender<()>,
        thread: Option<std::thread::JoinHandle<Vec<String>>>,
    }
    impl Http {
        fn new(
            scopes: String,
            bots: &str,
            hold: Option<(Sender<()>, std::sync::mpsc::Receiver<()>)>,
            replies: Option<Sender<String>>,
        ) -> Self {
            let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
            let base = format!("http://{}", server.server_addr().to_ip().unwrap());
            let (stop, rx) = channel();
            let bots = bots.to_string();
            let thread = std::thread::spawn(move || {
                let mut urls = Vec::new();
                let mut hold = hold;
                while rx.try_recv().is_err() {
                    let Some(mut request) = server.recv_timeout(Duration::from_millis(10)).unwrap()
                    else {
                        continue;
                    };
                    let url = request.url().to_string();
                    let response = if url.contains("tenant_access_token") {
                        r#"{"code":0,"tenant_access_token":"t","expire":7200}"#.into()
                    } else if url.contains("bot/v3/info") {
                        r#"{"code":0,"bot":{"open_id":"ou_bot","app_name":"test"}}"#.into()
                    } else if url.contains("application/v6/scopes") {
                        if let Some((started, release)) = hold.take() {
                            started.send(()).unwrap();
                            let scopes = scopes.clone();
                            urls.push(url);
                            std::thread::spawn(move || {
                                release.recv_timeout(Duration::from_secs(8)).unwrap();
                                let _ = request.respond(tiny_http::Response::from_string(scopes));
                            });
                            continue;
                        }
                        scopes.clone()
                    } else if url.contains("/members?") {
                        r#"{"code":0,"data":{"has_more":false,"items":[{"member_id_type":"open_id","member_id":"ou_owner"}]}}"#.into()
                    } else if url.contains("/im/v1/chats/oc_target") && bots == "deny" {
                        r#"{"code":99991672,"msg":"access denied"}"#.into()
                    } else if url.contains("/im/v1/chats/oc_target") && bots == "missing" {
                        r#"{"code":0,"data":{}}"#.into()
                    } else if url.contains("/im/v1/chats/oc_target") {
                        json!({"code":0,"data":{"chat_mode":"group","user_count":"1","bot_count":bots}}).to_string()
                    } else if url.contains("/messages") {
                        let mut body = String::new();
                        request.as_reader().read_to_string(&mut body).unwrap();
                        if let Some(tx) = &replies {
                            tx.send(body).unwrap();
                        }
                        r#"{"code":0,"data":{"message_id":"om_reply"}}"#.into()
                    } else if url.contains("/contact/v3/users/ou_owner?") {
                        r#"{"code":0,"data":{"user":{"union_id":"on_owner"}}}"#.into()
                    } else {
                        panic!("unexpected HTTP: {url}");
                    };
                    urls.push(url);
                    let _ = request.respond(tiny_http::Response::from_string(response));
                }
                urls
            });
            Self {
                base,
                stop,
                thread: Some(thread),
            }
        }
        fn finish(mut self) -> Vec<String> {
            self.stop.send(()).unwrap();
            self.thread.take().unwrap().join().unwrap()
        }
    }
    impl Drop for Http {
        fn drop(&mut self) {
            let _ = self.stop.send(());
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    #[test]
    fn mentionless_setup_scopes_are_tenant_grants_and_link_is_targeted() {
        for (names, kind, status, missing) in [
            (SCOPES, "tenant", 1, vec![]),
            (
                &["im:message.group_msg:readonly", "im:chat:readonly"][..],
                "tenant",
                1,
                vec![],
            ),
            (SCOPES, "user", 1, SCOPES.to_vec()),
            (SCOPES, "tenant", 2, SCOPES.to_vec()),
            (&["im:chat:readonly"][..], "tenant", 1, vec![SCOPES[0]]),
        ] {
            let http = Http::new(grants(names, kind, status), "1", None, None);
            let fc = FeishuClient::with_test_base(http.base.clone());
            assert_eq!(fc.mentionless_missing_permissions().unwrap(), missing);
            let url = reqwest::Url::parse(&fc.mentionless_permission_url(&missing)).unwrap();
            assert_eq!(url.host_str(), Some("open.feishu.cn"));
            assert_eq!(url.path(), "/app/test-app/auth");
            assert_eq!(
                url.query_pairs().find(|(k, _)| k == "q").unwrap().1,
                missing.join(",")
            );
            assert_eq!(
                url.query_pairs()
                    .find(|(k, _)| k == "token_type")
                    .unwrap()
                    .1,
                "tenant"
            );
            assert!(http.finish().iter().all(|url| !url.contains("/chats")));
        }
        let fc = FeishuClient::new("larksuite.com", "cli_lark".into(), "secret".into(), false);
        assert!(fc
            .mentionless_permission_url(SCOPES)
            .starts_with("https://open.larksuite.com/app/cli_lark/auth?"));
    }

    /// 群信息读得到时先看人数:群不满足就不引导申请权限;读不到再按权限结论走。
    #[test]
    fn mentionless_setup_preflight_checks_readable_topology_before_permissions() {
        let missing_group_msg = || grants(&["im:chat:readonly"], "tenant", 1);
        for (response, bots, expected, link, scopes_queried) in [
            (grants(SCOPES, "tenant", 1), "1", "ready", false, true),
            (
                grants(SCOPES, "tenant", 1),
                "2",
                "bot_count=2",
                false,
                false,
            ),
            (
                missing_group_msg(),
                "2",
                "无法通过申请权限满足",
                false,
                false,
            ),
            (missing_group_msg(), "1", "im:message.group_msg", true, true),
            (grants(&[], "tenant", 1), "deny", "im:chat:read", true, true),
            (
                grants(SCOPES, "tenant", 1),
                "missing",
                "缺少计数",
                false,
                true,
            ),
            (r#"{"code":99991672}"#.into(), "1", "无法确认", true, true),
            (
                r#"{"code":0,"data":{}}"#.into(),
                "1",
                "无法确认",
                true,
                true,
            ),
            ("invalid".into(), "1", "无法确认", true, true),
        ] {
            let http = Http::new(response, bots, None, None);
            let fc = FeishuClient::with_test_base(http.base.clone());
            let outcome = preflight(&fc, &msg(), Command::On);
            match outcome {
                Outcome::Ready => assert_eq!(expected, "ready"),
                Outcome::Reply(reply) => {
                    assert!(reply.contains(expected), "{reply}");
                    assert_eq!(
                        reply.contains("https://open.feishu.cn/app/test-app/auth?"),
                        link,
                        "{reply}"
                    );
                    // 只有查到计数且不满足时才说「申请权限也没用」。
                    assert_eq!(
                        reply.contains("无法通过申请权限满足"),
                        bots == "2",
                        "{reply}"
                    );
                }
            }
            let urls = http.finish();
            assert!(urls.iter().any(|url| url.contains("/chats/oc_target")));
            assert_eq!(
                urls.iter().any(|url| url.contains("/scopes")),
                scopes_queried
            );
        }
        let http = Http::new(grants(SCOPES, "tenant", 1), "2", None, None);
        let fc = FeishuClient::with_test_base(http.base.clone());
        assert!(
            matches!(preflight(&fc, &msg(), Command::Permissions), Outcome::Reply(reply) if reply.contains("均已授权"))
        );
        assert!(!http.finish().iter().any(|url| url.contains("/chats")));
    }

    #[test]
    fn mentionless_setup_writer_is_local_preserves_fields_mode_and_rejects_stale() {
        let (agent, _root) = agent();
        let path = agent.manifest_path();
        let original = std::fs::read_to_string(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
        write_config(
            &agent,
            &msg(),
            true,
            &original,
            agent.manifest_mtime().unwrap(),
        )
        .unwrap();
        let fresh = agent.reread().unwrap();
        assert!(fresh.replies_without_mention("oc_target"));
        assert!(fresh.replies_without_mention("oc_other"));
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o640
        );
        let current = std::fs::read_to_string(&path).unwrap();
        write_config(
            &fresh,
            &msg(),
            true,
            &current,
            fresh.manifest_mtime().unwrap(),
        )
        .unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), current);
        assert!(write_config(
            &agent,
            &msg(),
            false,
            &original,
            agent.manifest_mtime().unwrap()
        )
        .is_err());
        turn_off(&fresh, &msg(), fresh.manifest_mtime()).unwrap();
        assert!(!agent.reread().unwrap().replies_without_mention("oc_target"));
        assert!(agent.reread().unwrap().replies_without_mention("oc_other"));
        let before = std::fs::read_to_string(&path).unwrap();
        let mut denied = msg();
        denied.sender = "ou_other".into();
        assert!(turn_off(&agent, &denied, agent.manifest_mtime()).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
    }

    #[test]
    fn mentionless_setup_patch_preserves_unrelated_yaml_and_rejects_unsupported_forms() {
        let yaml = "# top\nname: test\nfeishu: # app\n  allowed_users: [ou_owner] # owner\n  chat_reply_without_mention:\n    oc_other: true\n  operators: [ou_owner] # op\nmodel: x # model\n";
        let patched = patch(yaml, "oc_target", true).unwrap();
        for line in [
            "# top",
            "name: test",
            "feishu: # app",
            "  allowed_users: [ou_owner] # owner",
            "  operators: [ou_owner] # op",
            "model: x # model",
        ] {
            assert!(patched.lines().any(|actual| actual == line));
        }
        assert_eq!(patch(&patched, "oc_target", true).unwrap(), patched);
        let crlf = patch(&yaml.replace('\n', "\r\n"), "oc_target", true).unwrap();
        assert_eq!(crlf, patched.replace('\n', "\r\n"));
        for yaml in [
            "feishu: {allowed_users: [ou_owner]}\n",
            "feishu:\n  chat_reply_without_mention: []\n",
            "feishu: null\n",
            "feishu:\n  other:\n    chat_reply_without_mention:\n      oc_other: true\n",
        ] {
            assert!(patch(yaml, "oc_target", true).is_err(), "{yaml}");
        }
    }

    #[test]
    fn mentionless_setup_proofs_require_current_authorized_identity_and_timely_snapshot() {
        let (agent, _root) = agent();
        let original = std::fs::read_to_string(agent.manifest_path()).unwrap();
        for case in ["identity", "expired", "mtime", "sender", "bot", "p2p"] {
            let mut request = Request {
                msg: msg(),
                command: Command::On,
                manifest: original.clone(),
                mtime: agent.manifest_mtime().unwrap(),
                started: Instant::now(),
                canceled: Arc::new(AtomicBool::new(false)),
                dashboard: None,
            };
            match case {
                "expired" => request.started -= TTL + Duration::from_secs(1),
                "mtime" => request.mtime -= Duration::from_secs(1),
                "sender" => request.msg.sender = "ou_other".into(),
                "bot" => request.msg.sender_type = "app".into(),
                "p2p" => request.msg.chat_type = Some("p2p".into()),
                _ => {}
            }
            let reply = Checked {
                request,
                outcome: Outcome::Ready,
            }
            .finish(&agent, |_| case != "identity");
            assert!(matches!(reply, CommandAck::Note(_)), "{case}: {reply:?}");
            assert!(reply.text().contains("变化"), "{case}: {reply:?}");
            assert_eq!(
                std::fs::read_to_string(agent.manifest_path()).unwrap(),
                original
            );
        }
        let request = Request {
            msg: msg(),
            command: Command::On,
            manifest: original,
            mtime: agent.manifest_mtime().unwrap(),
            started: Instant::now(),
            canceled: Arc::new(AtomicBool::new(false)),
            dashboard: None,
        };
        let reply = Checked {
            request,
            outcome: Outcome::Ready,
        }
        .finish(&agent, |_| true);
        assert!(
            matches!(reply, CommandAck::Done(_)),
            "开成了是纯确认:{reply:?}"
        );
        assert!(reply.text().starts_with("已开启"), "{reply:?}");
    }

    #[test]
    fn mentionless_setup_queue_is_bounded_and_drops_results_after_shutdown() {
        let (agent, _root) = agent();
        let (started, started_rx) = channel();
        let (release, release_rx) = channel();
        let http = Http::new(
            grants(SCOPES, "tenant", 1),
            "1",
            Some((started, release_rx)),
            None,
        );
        let (tx, rx) = channel();
        let mut queue = Queue::new(
            Arc::new(FeishuClient::with_test_base(http.base.clone())),
            tx,
        );
        queue
            .submit(&agent, msg(), Command::Permissions, agent.manifest_mtime())
            .unwrap();
        started_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        for _ in 0..4 {
            queue
                .submit(&agent, msg(), Command::Permissions, agent.manifest_mtime())
                .unwrap();
        }
        assert!(queue
            .submit(&agent, msg(), Command::Permissions, agent.manifest_mtime())
            .is_err());
        drop(queue);
        release.send(()).unwrap();
        assert!(rx.recv_timeout(Duration::from_secs(3)).is_err());
        assert_eq!(
            http.finish()
                .iter()
                .filter(|url| url.contains("/scopes"))
                .count(),
            1
        );
        assert!(!agent.reread().unwrap().replies_without_mention("oc_target"));
    }

    #[test]
    fn mentionless_setup_command_parser_is_exact_and_consumes_invalid_arguments() {
        let mut message = msg();
        assert_eq!(command(&message), Some(Command::On));
        for text in [
            "/mentionless",
            "/mentionless on oc_other",
            "/mentionless invalid",
        ] {
            message.text = text.into();
            assert_eq!(command(&message), Some(Command::Help));
        }
        message.text = "/mentionless OFF".into();
        assert_eq!(command(&message), Some(Command::Off));
        message.text = "/mentionlessx on".into();
        assert_eq!(command(&message), None);
    }
    #[test]
    fn mentionless_setup_production_dispatcher_configures_without_model_and_keeps_commands_responsive(
    ) {
        use super::super::{dispatcher, FeishuEpochIdentity};
        use std::collections::HashMap;
        let (agent, root) = agent();
        let (started, started_rx) = channel();
        let (release, release_rx) = channel();
        let (reply_tx, replies) = channel();
        let http = Http::new(
            grants(SCOPES, "tenant", 1),
            "1",
            Some((started, release_rx)),
            Some(reply_tx),
        );
        let fc = Arc::new(FeishuClient::with_test_base(http.base.clone()));
        let stores = dispatcher::DispatcherStores {
            bridge: crate::bridge::store::BridgeStore::new(root.path().join("bridge")),
            outbox: crate::runtime::outbox_store::OutboxStore::new(root.path().join("outbox")),
            inbox: crate::runtime::inbox_wal::InboxWal::new(root.path().join("inbox")),
            card_actions: crate::runtime::card_action_wal::CardActionWal::new(
                root.path().join("card-wal"),
            ),
            support_root: Some(root.path().join("support")),
        };
        let (tx, rx) = channel();
        let worker = {
            let agent = agent.clone();
            let tx = tx.clone();
            std::thread::spawn(move || {
                let epoch = FeishuEpochIdentity::capture(&agent);
                dispatcher::agent_dispatcher_with_stores(
                    agent,
                    20,
                    20,
                    fc,
                    epoch,
                    rx,
                    tx,
                    Arc::new(crate::runtime::core::Core::for_routes(HashMap::new())),
                    crate::runtime::inflight::InFlight::new(),
                    stores,
                );
            })
        };
        tx.send(DispatchMsg::Inbound(msg())).unwrap();
        started_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        let mut help = msg();
        help.message_id = "om_help".into();
        help.text = "@_user_1 /mentionless".into();
        tx.send(DispatchMsg::Inbound(help)).unwrap();
        let help_reply = replies.recv_timeout(Duration::from_secs(3)).unwrap();
        assert!(help_reply.contains("/mentionless off"), "{help_reply}");
        assert!(!agent.reread().unwrap().replies_without_mention("oc_target"));
        release.send(()).unwrap();
        // 开成了、关掉了都是纯确认:群里只给命令贴完成表情,不发文字。
        let on_reply = replies.recv_timeout(Duration::from_secs(3)).unwrap();
        assert!(on_reply.contains("\"emoji_type\""), "{on_reply}");
        assert!(agent.reread().unwrap().replies_without_mention("oc_target"));
        tx.send(DispatchMsg::Inbound(msg())).unwrap(); // 同一飞书事件只配置一次。

        // 已开启的群里,斜杠命令经一人一 Bot 验证后不 @ 也生效。
        let unmentioned = |id: &str| {
            let mut unmentioned = msg();
            unmentioned.message_id = id.into();
            unmentioned.text = "/mentionless off".into();
            unmentioned.mentions.clear();
            unmentioned
        };
        tx.send(DispatchMsg::Inbound(unmentioned("om_no_at")))
            .unwrap();
        let off_reply = replies.recv_timeout(Duration::from_secs(3)).unwrap();
        assert!(off_reply.contains("\"emoji_type\""), "{off_reply}");
        assert!(!agent.reread().unwrap().replies_without_mention("oc_target"));
        // 关闭后不 @ 的命令不再是免 @ 候选,静默忽略;显式 @ 照常应答。
        tx.send(DispatchMsg::Inbound(unmentioned("om_no_at_after_off")))
            .unwrap();
        let mut off = msg();
        off.message_id = "om_off".into();
        off.text = "@_user_1 /mentionless off".into();
        tx.send(DispatchMsg::Inbound(off)).unwrap();
        let off_reply = replies.recv_timeout(Duration::from_secs(3)).unwrap();
        assert!(off_reply.contains("\"emoji_type\""), "{off_reply}");
        assert!(!agent.reread().unwrap().replies_without_mention("oc_target"));
        tx.send(DispatchMsg::Shutdown).unwrap();
        worker.join().unwrap();
        let urls = http.finish();
        assert_eq!(urls.iter().filter(|url| url.contains("/scopes")).count(), 1);
        assert_eq!(
            urls.iter().filter(|url| url.contains("/members?")).count(),
            2,
            "开启检查与免 @ 关闭命令各查一次成员"
        );
        let archive = agent
            .state_dir()
            .join("threads")
            .join(summon_core::registry::chat_tag("group", "oc_target"));
        let files: Vec<_> = std::fs::read_dir(&archive)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(
            files,
            [std::ffi::OsString::from("outbound.ndjson")],
            "命令通路只产生出站归档"
        );
        assert!(!agent.state_dir().join("sessions").exists());
        assert!(replies.try_recv().is_err());
    }
    #[test]
    fn mentionless_setup_production_dispatcher_explains_denial_and_resolves_union_id() {
        use super::super::{dispatcher, FeishuEpochIdentity};
        use std::collections::HashMap;
        let run = |lists: &str| {
            let (agent, root) = crate::test_support::agent_dir(
                "claude",
                &format!("feishu:\n  app_id_env: MENTIONLESS_TEST_APP\n{lists}messaging:\n  session_mirror: false\n"),
            );
            let (reply_tx, replies) = channel();
            let http = Http::new(grants(SCOPES, "tenant", 1), "1", None, Some(reply_tx));
            let fc = Arc::new(FeishuClient::with_test_base(http.base.clone()));
            let stores = dispatcher::DispatcherStores {
                bridge: crate::bridge::store::BridgeStore::new(root.path().join("bridge")),
                outbox: crate::runtime::outbox_store::OutboxStore::new(root.path().join("outbox")),
                inbox: crate::runtime::inbox_wal::InboxWal::new(root.path().join("inbox")),
                card_actions: crate::runtime::card_action_wal::CardActionWal::new(
                    root.path().join("card-wal"),
                ),
                support_root: Some(root.path().join("support")),
            };
            let (tx, rx) = channel();
            let worker = {
                let tx = tx.clone();
                std::thread::spawn(move || {
                    let epoch = FeishuEpochIdentity::capture(&agent);
                    dispatcher::agent_dispatcher_with_stores(
                        agent,
                        20,
                        20,
                        fc,
                        epoch,
                        rx,
                        tx,
                        Arc::new(crate::runtime::core::Core::for_routes(HashMap::new())),
                        crate::runtime::inflight::InFlight::new(),
                        stores,
                    );
                })
            };
            // 私聊与 bot 发的命令不判权，也不查 union_id:mock 只认 ou_owner 的联系人请求，
            // 为 ou_peer 去查会让它当场 panic。
            let mut p2p = msg();
            p2p.message_id = "om_p2p".into();
            p2p.chat_id = "oc_p2p".into();
            p2p.chat_type = Some("p2p".into());
            p2p.mentions.clear();
            p2p.text = "/mentionless permissions".into();
            let mut bot = msg();
            bot.message_id = "om_bot".into();
            bot.sender = "ou_peer".into();
            bot.sender_type = "app".into();
            bot.text = "@_user_1 /mentionless permissions".into();
            let mut permissions = msg();
            permissions.text = "@_user_1 /mentionless permissions".into();
            let mut got = Vec::new();
            for message in [p2p, bot, permissions] {
                tx.send(DispatchMsg::Inbound(message)).unwrap();
                got.push(replies.recv_timeout(Duration::from_secs(3)).unwrap());
            }
            tx.send(DispatchMsg::Shutdown).unwrap();
            worker.join().unwrap();
            let contact = http
                .finish()
                .iter()
                .filter(|u| u.contains("/contact/v3/users/"))
                .count();
            (got, contact)
        };

        // 事件没带 union_id、名单只配了 union_id:回退联系人 API 后放行，只查这一次。
        let (got, contact) = run("  operators: [on_owner]\n");
        assert!(got[0].contains("请在目标群"), "{}", got[0]);
        assert!(got[1].contains("只有真人"), "{}", got[1]);
        assert!(got[2].contains("所需权限均已授权"), "{}", got[2]);
        assert_eq!(contact, 1);

        let (got, _) = run("");
        assert!(got[2].contains("需要本群操作权限"), "{}", got[2]);
        assert!(got[2].contains("都为空"), "{}", got[2]);
        assert!(got[2].contains("`on_owner`"), "{}", got[2]);
    }
    #[test]
    fn mentionless_setup_authorization_requires_talk_and_current_chat_operation() {
        let (mut agent, _root) = agent();
        let message = msg();
        assert!(authorized(&agent, &message));
        let feishu = agent.feishu.as_mut().unwrap();
        feishu.operators = Some(vec!["ou_admin".into()]);
        assert!(!authorized(&agent, &message));
        agent
            .feishu
            .as_mut()
            .unwrap()
            .chat_operators
            .insert("oc_other".into(), vec!["ou_owner".into()]);
        assert!(!authorized(&agent, &message));
        agent
            .feishu
            .as_mut()
            .unwrap()
            .chat_operators
            .insert("oc_target".into(), vec!["ou_owner".into()]);
        assert!(authorized(&agent, &message));
        agent.feishu.as_mut().unwrap().allowed_users = vec!["ou_admin".into()];
        assert!(!authorized(&agent, &message));
    }
    #[test]
    fn mentionless_setup_denial_names_missing_permission_and_field() {
        let (mut agent, _root) = agent();
        let name = agent.name.clone();
        let mut message = msg();
        message.sender_union_id = "on_owner".into();
        assert_eq!(denial(&agent, &message), None);

        let feishu = agent.feishu.as_mut().unwrap();
        feishu.allowed_users.clear();
        feishu.operators = Some(Vec::new());
        let reply = denial(&agent, &message).unwrap();
        assert!(reply.contains(&format!("agent `{name}`")), "{reply}");
        assert!(
            reply.contains("`feishu.operators` 和 `feishu.allowed_users` 都为空"),
            "{reply}"
        );
        assert!(reply.contains("`on_owner`"), "{reply}");
        assert!(
            reply.contains("`feishu.chat_operators` 下 `oc_target` 的名单即可在本群操作"),
            "{reply}"
        );
        assert!(reply.contains("其他人在私聊里也将不能操作"), "{reply}");
        assert!(reply.contains("暂未查到所有者"), "{reply}");
        let owner = |union_id: &str| {
            AppOwnerState::Known(Some(summon_core::config::registry::AppOwner {
                open_id: "ou_app_owner".into(),
                union_id: union_id.into(),
            }))
        };
        agent.app_owner.set(owner("on_app_owner"));
        let reply = denial(&agent, &message).unwrap();
        assert!(reply.contains("群里只有飞书应用所有者能操作"), "{reply}");
        agent.app_owner.set(AppOwnerState::Known(None));
        let reply = denial(&agent, &message).unwrap();
        assert!(reply.contains("应用所有者不是企业成员"), "{reply}");
        // 发起人就是应用所有者:放行。
        agent.app_owner.set(owner("on_owner"));
        assert_eq!(denial(&agent, &message), None);
        agent.app_owner.set(AppOwnerState::Unknown);

        agent.feishu.as_mut().unwrap().operators = Some(vec!["on_admin".into()]);
        let reply = denial(&agent, &message).unwrap();
        assert!(reply.contains("不在 agent"), "{reply}");
        assert!(reply.contains("`feishu.operators` 里"), "{reply}");

        assert!(
            reply.contains("加进 `feishu.operators` 在所有会话"),
            "{reply}"
        );
        assert!(!reply.contains("union_id"), "{reply}");

        // 取不到 union_id 时说清根因并给 open_id:它在本 app 下同样能命中名单。
        message.sender_union_id.clear();
        let reply = denial(&agent, &message).unwrap();
        assert!(reply.contains("未能取得你的 union_id"), "{reply}");
        assert!(reply.contains("`ou_owner`"), "{reply}");

        agent.feishu.as_mut().unwrap().allowed_users = vec!["on_admin".into()];
        message.sender_union_id = "on_owner".into();
        assert!(!authorized(&agent, &message));
        let reply = denial(&agent, &message).unwrap();
        assert!(reply.contains("对话名单 `feishu.allowed_users`"), "{reply}");
        // 名单按 union_id 命中:open_id 对不上也放行对话。
        agent.feishu.as_mut().unwrap().allowed_users = vec!["on_owner".into()];
        assert!(!denial(&agent, &message).unwrap().contains("对话名单"));

        message.sender_type = "app".into();
        assert_eq!(
            denial(&agent, &message).as_deref(),
            Some("只有真人可以执行 `/mentionless`。")
        );
    }
    #[test]
    fn mentionless_setup_resolves_union_id_only_when_event_lacks_it() {
        let (mut agent, _root) = agent();
        let feishu = agent.feishu.as_mut().unwrap();
        feishu.allowed_users.clear();
        feishu.operators = Some(vec!["on_owner".into()]);
        let message = msg();
        assert!(!authorized(&agent, &message));
        let resolved = with_operator_union_id(&message, || "on_owner".into());
        assert_eq!(resolved.sender_union_id, "on_owner");
        assert!(authorized(&agent, &resolved));
        assert_eq!(denial(&agent, &resolved), None);

        let mut carried = msg();
        carried.sender_union_id = "on_event".into();
        let kept = with_operator_union_id(&carried, || panic!("事件带了 union_id 就不该查联系人"));
        assert_eq!(kept.sender_union_id, "on_event");
    }
    #[test]
    fn mentionless_setup_writer_reports_busy_without_blocking_dispatcher() {
        let (agent, _root) = agent();
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(agent.dir.join(".agent.yaml.lock"))
            .unwrap();
        lock.lock().unwrap();
        let (tx, rx) = channel();
        let thread = std::thread::spawn(move || {
            let result = turn_off(&agent, &msg(), agent.manifest_mtime());
            tx.send(result).unwrap();
        });
        let result = rx.recv_timeout(Duration::from_secs(1));
        drop(lock);
        thread.join().unwrap();
        assert!(result
            .unwrap()
            .unwrap_err()
            .to_string()
            .contains("配置正在修改"));
    }
    #[test]
    fn mentionless_setup_off_cancels_an_inflight_on_even_when_already_off() {
        let (agent, _root) = agent();
        turn_off(&agent, &msg(), agent.manifest_mtime()).unwrap();
        let (started, started_rx) = channel();
        let (release, release_rx) = channel();
        let http = Http::new(
            grants(SCOPES, "tenant", 1),
            "1",
            Some((started, release_rx)),
            None,
        );
        let (tx, rx) = channel();
        let mut queue = Queue::new(
            Arc::new(FeishuClient::with_test_base(http.base.clone())),
            tx,
        );
        queue
            .submit(&agent, msg(), Command::On, agent.manifest_mtime())
            .unwrap();
        started_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        let mtime = agent.manifest_mtime();
        queue.cancel("oc_target");
        turn_off(&agent, &msg(), mtime).unwrap();
        assert_eq!(agent.manifest_mtime(), mtime, "重复关闭保持幂等");
        release.send(()).unwrap();
        let DispatchMsg::MentionlessSetupChecked(checked) =
            rx.recv_timeout(Duration::from_secs(3)).unwrap()
        else {
            panic!("expected setup result")
        };
        queue.complete(&checked);
        assert!(checked.finish(&agent, |_| true).text().contains("变化"));
        assert!(!agent.reread().unwrap().replies_without_mention("oc_target"));
        assert!(queue.pending.is_empty());
        drop(queue);
        http.finish();
    }
    #[test]
    fn mentionless_setup_slow_permission_failure_still_provides_the_application_link() {
        let (agent, _root) = agent();
        let fc = FeishuClient::new("feishu.cn", "cli_current".into(), "secret".into(), false);
        let request = Request {
            msg: msg(),
            command: Command::On,
            manifest: std::fs::read_to_string(agent.manifest_path()).unwrap(),
            mtime: agent.manifest_mtime().unwrap(),
            started: Instant::now() - TTL - Duration::from_secs(1),
            canceled: Arc::new(AtomicBool::new(false)),
            dashboard: None,
        };
        let checked = Checked {
            request,
            outcome: Outcome::Reply(permission_reply(&fc, SCOPES, "无法确认当前应用权限")),
        };
        assert!(checked
            .finish(&agent, |_| true)
            .text()
            .contains("https://open.feishu.cn/app/cli_current/auth?"));
        assert!(!agent.reread().unwrap().replies_without_mention("oc_target"));
    }
    #[test]
    fn mentionless_setup_staging_preserves_concurrent_revocation_and_other_fields() {
        for revoke in [true, false] {
            let (agent, _root) = agent();
            let path = agent.manifest_path();
            let compact = std::fs::read_to_string(&path).unwrap();
            let original = format!("#{}\n{}", "x".repeat(32 * 1024 * 1024), compact);
            std::fs::write(&path, &original).unwrap();
            let edited = if revoke {
                compact.replace("allowed_users: [ou_owner]", "allowed_users: [ou_revoked]")
            } else {
                compact
            };
            let revoked = edited + "model: concurrent-model\n";
            let expected = revoked.clone();
            let dir = agent.dir.clone();
            let target = path.clone();
            let (stop, rx) = std::sync::mpsc::channel();
            let editor = std::thread::spawn(move || {
                let deadline = Instant::now() + Duration::from_secs(60);
                loop {
                    let staged = std::fs::read_dir(&dir).unwrap().any(|entry| {
                        let name = entry.unwrap().file_name().to_string_lossy().into_owned();
                        name.starts_with(".agent.yaml.") && name.ends_with(".tmp")
                    });
                    if staged {
                        let replacement = dir.join("revoked.yaml");
                        std::fs::write(&replacement, revoked).unwrap();
                        std::fs::rename(replacement, target).unwrap();
                        return true;
                    }
                    if rx.try_recv().is_ok() || Instant::now() > deadline {
                        return false;
                    }
                    std::thread::sleep(Duration::from_millis(1));
                }
            });
            let result = write_config(
                &agent,
                &msg(),
                true,
                &original,
                agent.manifest_mtime().unwrap(),
            );
            let _ = stop.send(());
            assert!(
                editor.join().unwrap(),
                "需要实际观察到 staging 期间的并发改写"
            );
            assert!(result.is_err(), "并发改写后不得提交旧快照");
            assert_eq!(std::fs::read_to_string(&path).unwrap(), expected);
            assert!(!std::fs::read_dir(agent.dir).unwrap().any(|entry| entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".tmp")));
        }
    }

    #[test]
    fn mentionless_setup_rechecks_application_identity_after_staging() {
        let (agent, _root) = agent();
        let original = std::fs::read_to_string(agent.manifest_path()).unwrap();
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let result = write_config_checked(
            &agent,
            &msg(),
            true,
            &original,
            agent.manifest_mtime().unwrap(),
            &|_| calls.fetch_add(1, Ordering::Relaxed) == 0,
        );
        assert!(result.is_err());
        assert_eq!(calls.load(Ordering::Relaxed), 2);
        assert_eq!(
            std::fs::read_to_string(agent.manifest_path()).unwrap(),
            original
        );
    }

    /// 控制台发起的核验:结果连同申请链接交回页面;没开成不写配置。
    #[test]
    fn dashboard_result_is_handed_back_with_the_application_link() {
        let (agent, _root) = agent();
        let fc = FeishuClient::new("feishu.cn", "cli_current".into(), "secret".into(), false);
        let (tx, rx) = channel();
        let checked = Checked {
            request: Request {
                msg: msg(),
                command: Command::On,
                manifest: std::fs::read_to_string(agent.manifest_path()).unwrap(),
                mtime: agent.manifest_mtime().unwrap(),
                started: Instant::now(),
                canceled: Arc::new(AtomicBool::new(false)),
                dashboard: Some(tx),
            },
            outcome: Outcome::Reply(permission_reply(&fc, SCOPES, "当前应用缺少权限")),
        };
        assert!(checked.asked_by_dashboard());
        let reply = checked.finish(&agent, |_| true);
        let done = rx.try_recv().unwrap();
        assert!(!done.ok);
        assert_eq!(done.message, reply.text());
        assert!(done
            .apply_url
            .unwrap()
            .starts_with("https://open.feishu.cn/app/cli_current/auth?"));
        assert!(!agent.reread().unwrap().replies_without_mention("oc_target"));
    }

    #[test]
    fn dashboard_ready_check_writes_config_and_reports_success() {
        let (agent, _root) = agent();
        let (tx, rx) = channel();
        let checked = Checked {
            request: Request {
                msg: msg(),
                command: Command::On,
                manifest: std::fs::read_to_string(agent.manifest_path()).unwrap(),
                mtime: agent.manifest_mtime().unwrap(),
                started: Instant::now(),
                canceled: Arc::new(AtomicBool::new(false)),
                dashboard: Some(tx),
            },
            outcome: Outcome::Ready,
        };
        assert!(checked
            .finish(&agent, |_| true)
            .text()
            .starts_with("已开启"));
        let done = rx.try_recv().unwrap();
        assert!(done.ok && done.apply_url.is_none());
        assert!(agent.reread().unwrap().replies_without_mention("oc_target"));
    }

    /// 进不了队列(不在名单、本代身份已变)当场交回原因,不联网。
    #[test]
    fn dashboard_submit_refusals_are_answered_at_once() {
        let (agent, _root) = agent();
        let (dispatch, _dispatch_rx) = channel();
        let mut queue = Queue::new(
            Arc::new(FeishuClient::with_test_base("http://127.0.0.1:9".into())),
            dispatch,
        );
        for (sender, blocked) in [("ou_stranger", false), ("ou_owner", true)] {
            let (tx, rx) = channel();
            let mut m = msg();
            m.sender = sender.into();
            queue.submit_from_dashboard(
                &agent,
                DashboardRequest { msg: m, reply: tx },
                agent.manifest_mtime(),
                blocked,
            );
            let done = rx.try_recv().expect("当场答复");
            assert!(!done.ok);
            assert!(done.message.starts_with("未开启"), "{}", done.message);
        }
        assert!(queue.pending.is_empty());
    }

    /// dispatcher 收到控制台发起的核验结果时只交回页面:控制台不往群里发消息。
    #[test]
    fn dispatcher_answers_dashboard_checks_without_posting_to_the_chat() {
        let source = include_str!("dispatcher.rs");
        let arm = &source[source
            .find("Ok(DispatchMsg::MentionlessSetupChecked(checked)) => {")
            .unwrap()..];
        let skip = arm
            .find("if from_dashboard {")
            .expect("控制台的结果要跳过群消息");
        let post = arm.find("send_command_ack(").unwrap();
        assert!(skip < post);
        assert!(source.contains("Ok(DispatchMsg::DashboardMentionless(request)) => {"));
    }
}
