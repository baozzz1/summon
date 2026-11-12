//! daemon 启动/单例/控制面(A1 拆分):run() 主循环、pidfile/flock、sidecar 配置、
//! events.sock/control.sock 连接处理、submit_command/inject。

use super::*;

/// 长连接事件握手：普通消息、持久卡片回调与文档评论通知使用同一协议代际。
pub(crate) const SIDECAR_PROTOCOL_VERSION: u32 = 3;
static CARD_ACTION_INGRESS_LOCK: std::sync::OnceLock<std::sync::Mutex<()>> =
    std::sync::OnceLock::new();

#[derive(Serialize)]
pub(super) struct AppCfg {
    app_id: String,
    app_secret: String,
    domain: String,
}

#[derive(Serialize)]
pub(super) struct SidecarConfig {
    language: summon_core::i18n::Language,
    language_file: String,
    events_sock: String,
    apps: Vec<AppCfg>,
}

#[derive(Debug, Deserialize, Serialize)]
pub(super) struct DaemonVersionIdentity {
    pub(super) pid: u32,
    pub(super) version: String,
}

/// 还没有任何 agent(新机器装完、onboard 之前)时 daemon 没有可服务的对象,不启动。返回给人看的
/// 说明;agents 目录在就返回 `None`。onboard 建好 agent 后会自己启动 daemon。
/// 只认「确实不存在」:读不了(权限、挂载还没就绪)照旧交给 registry 报错退出,自启单元会重试。
pub(crate) fn agents_missing_notice(agents_dir: &std::path::Path) -> Option<String> {
    let missing = matches!(
        std::fs::metadata(agents_dir),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound
    );
    missing.then(|| summon_i18n::format!("r5e3d8a06b3", agents_dir.display()))
}

/// 读 pidfile 并检活(kill -0),返回存活的 daemon pid。用于单例保护 / stop / status。
pub fn running_pid() -> Option<u32> {
    let pid = read_pid_at(&pid_file())?;
    daemon_identity_matches(pid).then_some(pid)
}

fn read_pid_at(path: &std::path::Path) -> Option<u32> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// ready 只在本代 daemon 全部入口就绪后出现；start 必须同时匹配自己刚 spawn 的 PID。
pub(crate) fn ready_pid() -> Option<u32> {
    read_pid_at(&ready_file())
}

/// 单例锁当前是否被别的进程持有。IO 错误向上传递，调用方不得把“看不清”当“锁空闲”。
pub(crate) fn daemon_lock_held() -> std::io::Result<bool> {
    lock_held_at(&lock_file())
}

fn lock_held_at(path: &std::path::Path) -> std::io::Result<bool> {
    if !path.parent().is_some_and(std::path::Path::is_dir) {
        return Ok(false);
    }
    Ok(match try_exclusive_lock(path)? {
        Some(lock) => {
            drop(lock);
            false
        }
        None => true,
    })
}

/// PID 身份闭环：pidfile、被持有的 flock 文件内 owner PID、活进程三者必须一致。
/// 陈旧 pidfile 即使恰好命中一个复用 PID，只要没有当前 daemon 持锁并声明同一 owner，就绝不发信号。
pub(crate) fn daemon_identity_matches(pid: u32) -> bool {
    identity_matches_at(pid, &pid_file(), &lock_file(), pid_is_alive)
}

/// stop 发信号后 daemon 会先删 pidfile、再随 `run()` 返回释放 flock。等待这一代真正
/// 交出单例锁，避免把“身份文件已删”误当成“旧 daemon 已完全退出”。锁探测异常时保守等待。
pub(crate) fn daemon_lock_still_owned_by(pid: u32) -> bool {
    let path = lock_file();
    lock_still_owned_by_at(pid, &path)
}

pub(super) fn lock_still_owned_by_at(pid: u32, path: &std::path::Path) -> bool {
    read_pid_at(path) == Some(pid) && !matches!(lock_held_at(path), Ok(false))
}

pub(super) fn identity_matches_at(
    pid: u32,
    pid_path: &std::path::Path,
    lock_path: &std::path::Path,
    is_alive: impl FnOnce(u32) -> bool,
) -> bool {
    is_alive(pid)
        && read_pid_at(pid_path) == Some(pid)
        && read_pid_at(lock_path) == Some(pid)
        && matches!(lock_held_at(lock_path), Ok(true))
}

/// 只删除仍属于 `expected` 这一代的身份文件，避免自启单元已拉起新 daemon 后，
/// 旧 stop 把新 PID/ready 哨兵删掉。
pub(crate) fn remove_identity_file_if_matches(path: &std::path::Path, expected: u32) -> bool {
    if read_pid_at(path) != Some(expected) {
        return false;
    }
    std::fs::remove_file(path).is_ok()
}

fn write_identity_file(path: &std::path::Path, pid: u32) -> anyhow::Result<()> {
    crate::runtime::fsio::atomic_write(path, pid.to_string().as_bytes())
}

pub(super) fn read_daemon_version_identity_at(
    path: &std::path::Path,
) -> Option<DaemonVersionIdentity> {
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

pub(super) fn write_daemon_version_identity(
    path: &std::path::Path,
    pid: u32,
    version: &str,
) -> anyhow::Result<()> {
    let bytes = serde_json::to_vec(&DaemonVersionIdentity {
        pid,
        version: version.to_string(),
    })?;
    crate::runtime::fsio::atomic_write(path, &bytes)
}

/// 只返回与当前 daemon PID 同代的构建版本；损坏/旧代文件一律视为未知。
pub(crate) fn daemon_version(pid: u32) -> Option<String> {
    daemon_version_at(&daemon_version_file(), pid)
}

pub(super) fn daemon_version_at(path: &std::path::Path, pid: u32) -> Option<String> {
    read_daemon_version_identity_at(path)
        .filter(|identity| identity.pid == pid)
        .map(|identity| identity.version)
}

/// compare-and-delete 版本身份，避免旧 daemon 收尾擦掉 KeepAlive 已发布的新代版本。
pub(crate) fn remove_daemon_version_if_matches(expected: u32) -> bool {
    let path = daemon_version_file();
    remove_daemon_version_if_matches_at(&path, expected)
}

pub(super) fn remove_daemon_version_if_matches_at(path: &std::path::Path, expected: u32) -> bool {
    if read_daemon_version_identity_at(path).map(|identity| identity.pid) != Some(expected) {
        return false;
    }
    std::fs::remove_file(path).is_ok()
}

/// 无副作用探测 pid 是否存活。`EPERM` 表示进程存在但无权发信号,仍算存活;
/// 0/超出 i32 的损坏 pidfile fail-closed 为不存活,避免 `kill(0, 0)` 误指向 CLI 进程组。
pub(crate) fn pid_is_alive(pid: u32) -> bool {
    use nix::errno::Errno;
    let Ok(raw) = i32::try_from(pid) else {
        return false;
    };
    if raw <= 0 {
        return false;
    }
    match nix::sys::signal::kill(nix::unistd::Pid::from_raw(raw), None) {
        Ok(()) | Err(Errno::EPERM) => true,
        Err(_) => false,
    }
}

/// 尝试对 `path` 取**独占非阻塞** flock(单例硬保证)。拿到 → `Some(持锁文件)`,**必须持有到
/// 进程退出**(drop 即释放,死亡自动释放、无残留锁);已被别的进程占 → `None`。IO 错误冒泡。
pub(super) fn try_exclusive_lock(path: &std::path::Path) -> std::io::Result<Option<std::fs::File>> {
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(path)?;
    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(std::fs::TryLockError::Error(e)) => Err(e),
    }
}

/// events.sock 的 accept 循环:**阻塞 accept**(与 control.sock 同范式),每条连接前复查停机 flag。
/// **why 不再非阻塞轮询**:轮询版每 50ms 醒一次 = 20 次/秒、172.8 万次/天的空唤醒,常驻进程上纯烧电;
/// 阻塞 accept 在无连接时零唤醒。停机语义保持等价——flag 置位后到达的连接一律丢弃、循环退出,
/// 不放新入站进来。停机时若仍阻塞在 accept,随进程退出被回收(同 control.sock,不 join)。
/// `on_conn`/`sock_path` 参数化只为可测:不必造 Core 就能验上面那条不变量。
fn events_accept_loop(
    listener: UnixListener,
    shutdown: &std::sync::atomic::AtomicBool,
    sock_path: &std::path::Path,
    on_conn: impl Fn(UnixStream),
) {
    for conn in listener.incoming() {
        if shutdown.load(std::sync::atomic::Ordering::SeqCst) {
            break;
        }
        match conn {
            Ok(c) => on_conn(c),
            Err(e) => {
                // unlink 后 listener 仍可能短暂存活;socket 文件已消失才认定入口没了。
                log(&summon_i18n::format!("rbcb872c170"));
                if !sock_path.exists() {
                    break;
                }
            }
        }
    }
}

/// 入站消息「什么内容都没有」时的 warn 文案(纯,便于单测)。返回 `None` = 正常,不打日志。
///
/// **为什么只在正文与附件都空时才 warn**:图片/语音这类消息本就没有正文、只有附件,是正常形态,
/// warn 它们只会把 daemon.log 刷成噪音;两者皆空才意味着这条消息在 agent 眼里等于不存在。
///
/// **为什么要有这条**:2026-08-04 bot 之间互发的 interactive 卡片在 sidecar 侧解析不出正文,
/// daemon 只留下一行 `textlen=0` 的常规摘要,得翻 sidecar 源码才知道是少了 msg_type 分支。
/// 一条自带「正文没解析出来」措辞的 warn,能把这类问题从「翻源码」降到「看一眼日志」。
fn empty_inbound_warning(
    text: &str,
    resources_len: usize,
    message_id: &str,
    chat_id: &str,
    sender: &str,
    chat_type: Option<&str>,
) -> Option<String> {
    if !text.trim().is_empty() || resources_len > 0 {
        return None;
    }
    Some(summon_i18n::format!(
        "reb50ed4ae0",
        chat_type.unwrap_or("p2p")
    ))
}

/// 一条 events.sock 连接:逐行读事件,经 **Core Port**(`crate::runtime::core::Core`)委派到对应 agent 的
/// dispatcher。入站普通消息翻成 `Command{ChatMessage}` 走 `dispatch_inbound`;卡片回调本步仍原样
/// 委派(`dispatch_card`),其 Intent 化(PlanResolve 等)留后续 Slice。行为与接缝前等价。
pub(super) fn handle_conn(conn: UnixStream, core: Arc<crate::runtime::core::Core>) {
    handle_conn_with_wal(
        conn,
        core,
        crate::runtime::card_action_wal::CardActionWal::at_run_dir(),
    );
}

/// events.sock 生产处理器的可注入 WAL 接缝。生产固定使用 runtime WAL；跨语言 socket 测试
/// 注入 tempdir，确保能跑真实协议路径而不读写用户的 `~/.summon/run`。
pub(super) fn handle_conn_with_wal(
    conn: UnixStream,
    core: Arc<crate::runtime::core::Core>,
    card_action_wal: crate::runtime::card_action_wal::CardActionWal,
) {
    use summon_core::core_port::{Actor, Command, Intent};
    use summon_core::ids::ChatId;
    let mut ack_conn = conn.try_clone().ok();
    let reader = BufReader::new(conn);
    let mut protocol_ok = false;
    for line in reader.lines().map_while(Result::ok) {
        if line.trim().is_empty() {
            continue;
        }
        let _writer = crate::runtime::agent_retirement::EVENT_WRITERS
            .read()
            .unwrap_or_else(|e| e.into_inner());
        match Event::from_line(&line) {
            Ok(Event::Message {
                app_id,
                chat_id,
                chat_type,
                thread_id,
                root_id,
                sender,
                sender_union_id,
                sender_type,
                text,
                message_id,
                parent_id,
                mentions,
                resources,
                create_time_ms,
                ..
            }) => {
                // 日志摘要在移动 Inbound 字段前先算好(只在成功路由时打)。
                //
                // 投递延迟 = 本机收到时刻 − 服务端 create_time,用来区分「没收到」与「收到得晚」
                // (飞书在长连接不可用时推送失败、之后按退避重投)。负数照打:那是本机时钟落后于
                // 服务端的直接证据,入站闸的时效判断在那种时钟下会偏宽。
                let delivery_lag = if create_time_ms > 0 {
                    summon_i18n::format!(
                        "rac429f9299",
                        (now_unix_ms() as i64 - create_time_ms) / 1000
                    )
                } else {
                    String::new()
                };
                let summary = summon_i18n::format!(
                    "rdb7ecd6719",
                    chat_type.as_deref().unwrap_or("p2p"),
                    text.chars().count(),
                    mentions.len(),
                    resources.len()
                );
                // 同上,也要在字段被移走前算好。
                let empty_warning = empty_inbound_warning(
                    &text,
                    resources.len(),
                    &message_id,
                    &chat_id,
                    &sender,
                    chat_type.as_deref(),
                );
                // Port 形态:谁(Feishu open_id)+ ChatMessage{chat,text}。富载元数据
                //(mentions/resources/parent_id/...)随 Inbound 走 dispatch_inbound,不丢。
                let cmd = Command {
                    actor: Actor::Feishu(sender.clone()),
                    intent: Intent::ChatMessage {
                        chat: ChatId::new(chat_id.clone()),
                        text: text.clone(),
                    },
                };
                let msg = DispatchMsg::Inbound(Inbound {
                    // 重注入的是真实入站消息,`message_id` 本身就是飞书那条。
                    anchor_message_id: None,
                    chat_id,
                    chat_type,
                    text,
                    sender,
                    sender_union_id,
                    sender_type,
                    message_id,
                    parent_id,
                    thread_id,
                    root_id,
                    mentions,
                    resources,
                    create_time_ms,
                    replayed: false,
                    reattach: false,
                });
                // 录制 tee(debug-gate;off 时编译进的也只是一次空查询直接返回 → 行为不变):
                // 在派发前把入站本体录进 inbound.ndjson。clone 仅在录制开关开时发生。
                #[cfg(debug_assertions)]
                if record_scenario().is_some() {
                    if let DispatchMsg::Inbound(ref inb) = msg {
                        record_inbound(&InboundRecord::Message {
                            app_id: app_id.clone(),
                            inbound: inb.clone(),
                        });
                    }
                }
                if core.dispatch_inbound(&app_id, &cmd, msg) {
                    log(&summary);
                    // 路由成功但内容全空 → 单独一条 warn。放在成功分支:未注册 app 已有「无路由」
                    // 兜底,再叠一条空正文 warn 只是噪音。
                    if let Some(warning) = empty_warning {
                        log(&warning);
                    }
                } else {
                    log(&summon_i18n::format!("re95e7b706d"));
                }
            }
            Ok(Event::DocumentComment {
                app_id,
                event_id,
                file_token,
                file_type,
                comment_id,
                reply_id,
                sender,
                recipient,
            }) => {
                if !protocol_ok {
                    write_document_comment_ack(ack_conn.as_mut(), &event_id, false);
                    return;
                }
                if !core.has_app_route(&app_id) {
                    write_document_comment_ack(ack_conn.as_mut(), &event_id, true);
                    continue;
                }
                let notice = crate::runtime::document_comments::CommentNotice {
                    app_id,
                    event_id: event_id.clone(),
                    file_token,
                    file_type,
                    comment_id,
                    reply_id,
                    sender_open_id: sender,
                    recipient_open_id: recipient,
                };
                let result = document_comments::receive_notice(
                    &crate::runtime::document_comments::Store::at_run_dir(),
                    &core,
                    notice,
                    now_unix_ms() as i64,
                );
                // 拒收与落盘成功一样答复已处理:sidecar 收到失败 ACK 会整体退出重连。
                let acked = result
                    .as_ref()
                    .map_or_else(crate::runtime::document_comments::is_rejected, |_| true);
                write_document_comment_ack(ack_conn.as_mut(), &event_id, acked);
                match result {
                    Ok(_) => {}
                    Err(error) if acked => log(&summon_i18n::format!("rc72680d545")),
                    Err(error) => log(&summon_i18n::format!("r6387a4b423")),
                }
            }
            Ok(Event::CardAction {
                app_id,
                chat_id,
                message_id,
                operator_open_id,
                event_id,
                value,
            }) => {
                if !core.has_app_route(&app_id) {
                    continue;
                }
                // 卡片 value 可能携带表单答案、任务评论或 gate 文本；production log 只记有限
                // 路由元数据，绝不把完整 callback payload 持久化到 daemon.log。
                let action = value
                    .get("action")
                    .and_then(|value| value.as_str())
                    .filter(|action| {
                        action.len() <= 64
                            && action.bytes().all(|byte| {
                                byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-')
                            })
                    })
                    .unwrap_or("<invalid>");
                let field_count = value.as_object().map_or(0, serde_json::Map::len);
                let summary = summon_i18n::format!("ra43e582263");
                let card = CardActionMsg {
                    chat_id,
                    message_id,
                    operator_open_id,
                    event_id,
                    value,
                    wal_id: None,
                    wal_replayed: false,
                };
                if !crate::runtime::card_action_wal::is_durable_card(&card) {
                    // 非 Bridge 卡片保持既有 socket 语义；其 handler 尚未统一 event_id 幂等，
                    // 纳入 WAL 重放会在“副作用完成→tombstone”崩溃窗口重复执行。
                    dispatch_card_msg(&core, &app_id, card, &summary);
                    continue;
                }
                if !protocol_ok {
                    log(summon_i18n::text!("rac995ad15c"));
                    write_card_callback_ack(
                        ack_conn.as_mut(),
                        &card.event_id,
                        false,
                        "protocol_required",
                    );
                    return;
                }
                // 多 events.sock 连接也必须按 durable receive sequence 串行“落盘→ACK→入队”；
                // 否则 seq=2 的线程可能先于 seq=1 进入 dispatcher，运行时与冷启重放顺序不一致。
                let _ingress = CARD_ACTION_INGRESS_LOCK
                    .get_or_init(|| std::sync::Mutex::new(()))
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                let persisted = match card_action_wal.persist(&app_id, &card) {
                    Ok(persisted) => {
                        write_card_callback_ack(ack_conn.as_mut(), &card.event_id, true, "durable");
                        persisted
                    }
                    Err(error) => {
                        log(&summon_i18n::format!("rdd5b22ada4"));
                        write_card_callback_ack(
                            ack_conn.as_mut(),
                            &card.event_id,
                            false,
                            "persist_failed",
                        );
                        continue;
                    }
                };
                let record = match persisted {
                    crate::runtime::card_action_wal::PersistOutcome::Pending {
                        record,
                        created: true,
                    } => *record,
                    crate::runtime::card_action_wal::PersistOutcome::Pending {
                        created: false,
                        ..
                    }
                    | crate::runtime::card_action_wal::PersistOutcome::Consumed => {
                        // 已在队列/重放中或已有脱敏终态，只 ACK，不重复执行非幂等动作。
                        continue;
                    }
                };
                dispatch_card_msg(&core, &app_id, record.card, &summary);
            }
            Ok(Event::ChatDisbanded {
                app_id,
                chat_id,
                operator_open_id,
            }) => {
                let summary = summon_i18n::format!("refeb497786");
                let msg = DispatchMsg::ChatGone {
                    chat_id,
                    reason: summon_core::exit::ExitReason::GroupDisbanded,
                    operator_open_id,
                };
                if core.dispatch_card(&app_id, msg) {
                    log(&summary);
                } else {
                    log(&summon_i18n::format!("re1ef855391"));
                }
            }
            Ok(Event::BotRemoved {
                app_id,
                chat_id,
                operator_open_id,
            }) => {
                let summary = summon_i18n::format!("rdb84d1005b");
                let msg = DispatchMsg::ChatGone {
                    chat_id,
                    reason: summon_core::exit::ExitReason::BotRemoved,
                    operator_open_id,
                };
                if core.dispatch_card(&app_id, msg) {
                    log(&summary);
                } else {
                    log(&summon_i18n::format!("r1ee22df3b2"));
                }
            }
            Ok(Event::MessageRecalled {
                app_id,
                message_id,
                chat_id,
            }) => {
                let summary = summon_i18n::format!("r2ab0fbbbe2");
                let msg = DispatchMsg::RecallMessage { message_id };
                if core.dispatch_card(&app_id, msg) {
                    log(&summary);
                } else {
                    log(&summon_i18n::format!("red33dd5f8b"));
                }
            }
            Ok(Event::Hello { app_id, protocol }) => {
                let accepted = protocol == SIDECAR_PROTOCOL_VERSION;
                let wrote = write_sidecar_protocol_ack(
                    ack_conn.as_mut(),
                    accepted,
                    if accepted {
                        "ready"
                    } else {
                        "version_mismatch"
                    },
                );
                if !accepted || !wrote {
                    log(&summon_i18n::format!("r9ca0c1abf0"));
                    return;
                }
                protocol_ok = true;
                log(&summon_i18n::format!("recd1e2dbac"));
            }
            Ok(Event::Error { app_id, detail }) => log(&summon_i18n::format!("rf98ef9db48")),
            Err(e) => log(&summon_i18n::format!("r3a2c4fde20")),
        }
    }
}

fn dispatch_card_msg(
    core: &crate::runtime::core::Core,
    app_id: &str,
    card: CardActionMsg,
    summary: &str,
) {
    let msg = DispatchMsg::Card(card);
    #[cfg(debug_assertions)]
    if record_scenario().is_some() {
        if let DispatchMsg::Card(ref card) = msg {
            record_inbound(&InboundRecord::Card {
                app_id: app_id.to_string(),
                card: card.clone(),
            });
        }
    }
    if core.dispatch_card(app_id, msg) {
        log(summary);
    } else {
        log(&summon_i18n::format!("r53d64546bc"));
    }
}

fn write_document_comment_ack(conn: Option<&mut UnixStream>, event_id: &str, ok: bool) {
    let Some(conn) = conn else {
        return;
    };
    let response = serde_json::json!({"type":"document_comment_ack", "event_id":event_id, "ok":ok});
    let _ = writeln!(conn, "{response}").and_then(|_| conn.flush());
}

fn write_card_callback_ack(conn: Option<&mut UnixStream>, event_id: &str, ok: bool, status: &str) {
    let Some(conn) = conn else {
        return;
    };
    let response = serde_json::json!({
        "type": "card_action_ack",
        "event_id": event_id,
        "ok": ok,
        "status": status,
    });
    if writeln!(conn, "{response}")
        .and_then(|_| conn.flush())
        .is_err()
    {
        log(summon_i18n::text!("r0c135389ce"));
    }
}

fn write_sidecar_protocol_ack(conn: Option<&mut UnixStream>, ok: bool, status: &str) -> bool {
    let Some(conn) = conn else {
        return false;
    };
    let response = serde_json::json!({
        "type": "hello_ack",
        "ok": ok,
        "status": status,
        "protocol": SIDECAR_PROTOCOL_VERSION,
    });
    writeln!(conn, "{response}")
        .and_then(|_| conn.flush())
        .is_ok()
}

fn replay_card_action_records(
    core: &crate::runtime::core::Core,
    records: Vec<crate::runtime::card_action_wal::CardActionRecord>,
) {
    for mut record in records {
        record.card.wal_replayed = true;
        if core.dispatch_card(&record.app_id, DispatchMsg::Card(record.card)) {
            log(&summon_i18n::format!("rb9c9fb5c5c", record.id));
        } else {
            log(&summon_i18n::format!("rd7000bf6e4", record.id));
        }
    }
}

/// 一条 control.sock 连接:读一行 JSON `Command`(actor=Owner,本机命令)→ `Core::submit` →
/// 回写一行 JSON `CommandOutcome`。每连接一条命令(请求-应答),处理完即关。
/// 解析失败回 `Denied`,不崩。`summon ask` / `agents kill` 等本机命令是它的客户端。
pub(super) fn handle_control_conn(mut conn: UnixStream, core: Arc<crate::runtime::core::Core>) {
    use summon_core::core_port::{Command, CommandOutcome, CommandSink};
    let mut line = String::new();
    let mut reader = BufReader::new(match conn.try_clone() {
        Ok(c) => c,
        Err(e) => {
            log(&summon_i18n::format!("r0867ac660c"));
            return;
        }
    });
    if reader.read_line(&mut line).is_err() || line.trim().is_empty() {
        return;
    }
    let outcome = match serde_json::from_str::<Command>(line.trim()) {
        Ok(cmd) => core.submit(cmd),
        Err(e) => CommandOutcome::Denied(summon_i18n::format!("r2822cce2b1")),
    };
    // 阻塞闸门(plan/question):Core 返回 `Blocked(gate_id)` = 已登记挂起表、闸门卡片已渲染。
    // **此处不关连接**，但必须同时观察 peer HUP：发起 `summon ask` 的子进程被 SIGINT/kill 后，
    // control 客户端会先消失；若 daemon 仍无条件 wait，gate 会永久留在内存且旧卡还能被点。
    // 每连接一线程，等待不阻塞 Core 其它命令(挂起表 + channel 解耦)。
    let outcome = match outcome {
        CommandOutcome::Blocked(gate_id) => {
            wait_gate_while_peer_connected(core.as_ref(), &gate_id, reader)
        }
        other => other,
    };
    let resp = serde_json::to_string(&outcome).unwrap_or_else(|_| {
        // CommandOutcome 一定可序列化;兜底也给个合法 JSON,不让客户端卡死。
        summon_i18n::text!("rf9bfdfa9af").to_string()
    });
    let _ = conn.write_all(resp.as_bytes());
    let _ = conn.write_all(b"\n");
    let _ = conn.flush();
}

fn wait_gate_while_peer_connected(
    core: &crate::runtime::core::Core,
    gate_id: &summon_core::ids::GateId,
    mut reader: BufReader<UnixStream>,
) -> summon_core::core_port::CommandOutcome {
    use std::io::{ErrorKind, Read as _};
    use std::sync::atomic::{AtomicBool, Ordering};

    const PEER_POLL: Duration = Duration::from_millis(100);
    if let Err(error) = reader.get_mut().set_read_timeout(Some(PEER_POLL)) {
        // UnixStream 正常都支持 SO_RCVTIMEO；失败时不能把“监控建不起来”误当成 peer 已断。
        log(&summon_i18n::format!("re2b06bdff1"));
        return core.wait_gate(gate_id);
    }
    let cancelled = Arc::new(AtomicBool::new(false));
    let finished = Arc::new(AtomicBool::new(false));
    let cancelled_w = Arc::clone(&cancelled);
    let finished_w = Arc::clone(&finished);
    let monitor = thread::spawn(move || {
        let mut byte = [0_u8; 1];
        loop {
            if finished_w.load(Ordering::Acquire) {
                return;
            }
            match reader.read(&mut byte) {
                // 一条连接只允许一条 Command；EOF 或额外字节都意味着本 waiter 不再有合法 peer。
                Ok(0) | Ok(_) => {
                    cancelled_w.store(true, Ordering::Release);
                    return;
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted
                    ) => {}
                Err(_) => {
                    cancelled_w.store(true, Ordering::Release);
                    return;
                }
            }
        }
    });
    let outcome = core.wait_gate_cancellable(gate_id, cancelled.as_ref(), None);
    finished.store(true, Ordering::Release);
    // read 最多再等一个 PEER_POLL；join 后不留每问一个的后台线程。
    let _ = monitor.join();
    outcome
}

/// 前台运行 daemon。
/// PF2:清扫 `<agent.dir>/inbound/` 里超过 `ttl` 的条目(mtime 判龄):`inbound/<tag>/<message_id>/`
/// 附件目录与 `inbound/quotes/<id>.md` 引用 spill。它们是「为某条消息落的临时上下文」,过期即弃;
/// **只动 inbound/,绝不碰 threads/**(观测归档另有轮转)。返回清掉的条目数,失败静默跳过。
pub(super) fn cleanup_inbound_spill(
    agent_dir: &std::path::Path,
    ttl: Duration,
    now: SystemTime,
) -> usize {
    let mut removed = 0;
    let Ok(tags) = std::fs::read_dir(agent_dir.join("inbound")) else {
        return 0;
    };
    for tag in tags.flatten() {
        let Ok(entries) = std::fs::read_dir(tag.path()) else {
            continue;
        };
        for e in entries.flatten() {
            let expired = e
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|mt| now.duration_since(mt).ok())
                .map(|age| age > ttl)
                .unwrap_or(false);
            if !expired {
                continue;
            }
            let p = e.path();
            let ok = if p.is_dir() {
                std::fs::remove_dir_all(&p).is_ok()
            } else {
                std::fs::remove_file(&p).is_ok()
            };
            if ok {
                removed += 1;
            }
        }
    }
    removed
}

/// S2:Unix socket 对端 uid 校验(fail-closed)——两条 socket 都只信**同 uid** 进程:
/// events.sock 只该被 daemon 亲手 spawn 的 sidecar(同 uid 子进程)连,control.sock 只该被
/// 本机同用户的 summon CLI 连。校验失败(跨 uid / 取不到 peer)直接丢弃连接。
pub(super) fn peer_uid_is_self(conn: &UnixStream) -> bool {
    #[cfg(target_os = "linux")]
    {
        nix::sys::socket::getsockopt(conn, nix::sys::socket::sockopt::PeerCredentials)
            .map(|credentials| credentials.uid() == nix::unistd::getuid().as_raw())
            .unwrap_or(false)
    }
    #[cfg(not(target_os = "linux"))]
    {
        nix::unistd::getpeereid(conn)
            .map(|(uid, _gid)| uid == nix::unistd::getuid())
            .unwrap_or(false)
    }
}

/// S2:收紧 run_dir 与 socket 文件权限(0700/0600)。home 已 0700(S3),这里是纵深防御
/// (run_dir 可能被旧版本以 0755 建出)。失败只 log——权限收紧是 hardening,不该挡启动。
fn tighten_socket_perms(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Err(e) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)) {
        log(&summon_i18n::format!("r43338af451", path.display()));
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct InboxWalBootHealth {
    pending_records: usize,
    started_records: usize,
    scheduled_records: usize,
    active_recovery_batches: usize,
    awaiting_recovery_items: usize,
    claimed_recovery_items: usize,
    repaired_recovery_claims: usize,
}

/// daemon 对外暴露任何入口前的 Inbox WAL 完整性屏障。先做纯校验，再修复 recovery claim 的
/// 双 fsync 窗口，最后复验并生成不含正文/身份值的健康摘要；任一步失败都向上传播并拒绝启动。
fn preflight_inbox_wal(root: &std::path::Path) -> anyhow::Result<InboxWalBootHealth> {
    let wal = crate::runtime::inbox_wal::InboxWal::new(root.to_path_buf());
    wal.scan_pending_strict()
        .context(summon_i18n::text!("r17ba394865"))?;
    let repaired_recovery_claims = wal
        .repair_pending_recovery_claims()
        .context(summon_i18n::text!("r0c199b489a"))?;
    let records = wal
        .scan_pending_strict()
        .context(summon_i18n::text!("rd595d11e45"))?;
    let batches = wal
        .active_recovery_batches()
        .context(summon_i18n::text!("r8f0688f613"))?;
    let awaiting_recovery_items = batches
        .iter()
        .flat_map(|batch| &batch.items)
        .filter(|item| {
            matches!(
                item.state,
                crate::runtime::inbox_wal::RecoveryItemState::Awaiting
            )
        })
        .count();
    let claimed_recovery_items = batches
        .iter()
        .flat_map(|batch| &batch.items)
        .filter(|item| {
            matches!(
                item.state,
                crate::runtime::inbox_wal::RecoveryItemState::Claimed { .. }
            )
        })
        .count();
    Ok(InboxWalBootHealth {
        pending_records: records.len(),
        started_records: records.iter().filter(|record| record.started).count(),
        scheduled_records: records
            .iter()
            .filter(|record| record.scheduled.is_some())
            .count(),
        active_recovery_batches: batches.len(),
        awaiting_recovery_items,
        claimed_recovery_items,
        repaired_recovery_claims,
    })
}

pub fn run() -> anyhow::Result<()> {
    // **第一件事:剥掉自己身上的 turn/session 身份键**。
    //
    // daemon 常常是从某个 agent 的会话里被拉起来的(owner 在 tmux 里敲 `summon daemon start`、
    // 或 agent 自己重启 daemon),于是它继承了那一刻的 `SUMMON_AGENT` 等等。daemon 是所有子
    // 进程的父亲:任何忘了 `env_clear` 的 spawn 边界都会把这份陈旧身份原样传下去,子进程里的
    // CLI 据此认领**别的 agent**——跨群错发、跨 agent 读会话。
    // (2026-08-14 现场:rpc-procs 记着 PID 属于 agent A,`ps eww` 里却是
    //  agent B 的 `SUMMON_AGENT`,且该进程是全新 spawn、没经过任何 takeover。)
    //
    // 只删**身份**键;运维/功能键(`SUMMON_RECORD`、`SUMMON_CONTROL_SOCK` 等)daemon 自己要用,
    // 误删会把功能一起关掉。判据与 `essential_system_env` 共用 `is_runtime_identity_env`,
    // 不各写一份清单 —— 漏一个就是一条泄漏路径。
    let identity_keys: Vec<String> = std::env::vars()
        .map(|(k, _)| k)
        .filter(|k| super::agent_env::is_runtime_identity_env(k))
        .collect();
    for key in identity_keys {
        {
            crate::observe::log::log(&summon_i18n::format!("r9ced2f5cda"));
            std::env::remove_var(key);
        }
    }
    // uptime 打点(要在任何可能失败的启动步骤之前):uptime 只在 daemon 进程内算得出,
    // CLI 是另一个进程读不到,故 `agents status` 必须经 control.sock 问它。
    crate::observe::runtime_status::mark_daemon_started();
    // PATH 自愈:`main` 已经做过一次(daemon 也经 `summon daemon run` 进来),这里是幂等的第二道
    // 保险 —— daemon 是唯一会长期 spawn tmux/claude 的进程,PATH 缺项对它是致命的,
    // 不值得依赖「上游一定调过」这个假设。
    crate::setup::env::heal_tool_path();
    crate::setup::env::apply_saved_user_path();
    crate::setup::env::append_summon_bin_dir();
    fd_limit::raise();
    // PF6:daemon 进程内启用 daemon.log 运行期轮转(stderr 已被 daemon_start 重定向到它;
    // CLI 进程不启用——stderr 是用户终端)。
    crate::observe::log::enable_stderr_rotation();
    // 确保 ~/.summon 是 git 仓库且 .gitignore 挡住密钥/运行时(幂等、best-effort,失败不致命)。
    crate::setup::init::ensure();
    std::fs::create_dir_all(run_dir())?;
    crate::runtime::localization::enable_daemon_display_cache(run_dir().join("display-language"));
    {
        // S2:run_dir 收紧 0700(socket/pidfile/outbox 都在这);失败只 log。
        use std::os::unix::fs::PermissionsExt;
        if let Err(e) = std::fs::set_permissions(run_dir(), std::fs::Permissions::from_mode(0o700))
        {
            log(&summon_i18n::format!("rca81022de4"));
        }
    }
    // 单例**硬保证**:flock。拿不到锁 = 已有 daemon 在跑 → 直接退,绝不起第二个。这兜住所有
    // 绕过 pidfile 检查的路径(直接 `daemon run`、两个 `start` 竞态、socket 被抢)。锁随进程死自动释放。
    let mut singleton_lock = match try_exclusive_lock(&lock_file())? {
        Some(lock) => lock,
        None => {
            log(summon_i18n::text!("r404dc0aa85"));
            return Ok(());
        }
    };
    // 早于启动记录与停机哨兵:以 0 退出,自启单元不重试,下一代也不会把这次当成崩溃。
    if let Some(notice) = agents_missing_notice(&Config::load().agents_dir) {
        log(&notice);
        return Ok(());
    }
    let daemon_pid = std::process::id();
    // flock 文件同时记录**当前持锁者** PID。CLI 只有在「锁确实被占 + lock owner + pidfile +
    // 活进程」四者一致时才会发信号，陈旧 pidfile/PID 复用不再可能误杀无关进程。
    singleton_lock.set_len(0)?;
    singleton_lock.write_all(daemon_pid.to_string().as_bytes())?;
    singleton_lock.sync_data()?;
    // 自动拉起的熔断必须早于一切会补发积压的步骤(outbox、恢复卡、错过的定时轮),也早于启动期
    // 可能出错退出的预检 —— 启动记录从这里起就在,预检失败的一代同样算「没跑稳」。退出码 0:
    // 自启单元(launchd `SuccessfulExit=false` / systemd `Restart=on-failure`)据此不再重试。
    let had_boot_record = crash_guard::previous_boot_recorded();
    if let crash_guard::BootVerdict::Refuse(reason) = crash_guard::check_and_record(daemon_pid) {
        log(&reason);
        return Ok(());
    }
    let inbox_health =
        preflight_inbox_wal(&run_dir().join("inbox")).context(summon_i18n::text!("r719554b856"))?;
    log(&summon_i18n::format!(
        "r215924f2e3",
        inbox_health.pending_records,
        inbox_health.started_records,
        inbox_health.scheduled_records,
        inbox_health.active_recovery_batches,
        inbox_health.awaiting_recovery_items,
        inbox_health.claimed_recovery_items,
        inbox_health.repaired_recovery_claims,
    ));
    let card_action_wal = crate::runtime::card_action_wal::CardActionWal::at_run_dir();
    let card_health = card_action_wal
        .preflight()
        .context(summon_i18n::text!("r5c76be0508"))?;
    // 在起 dispatcher 前冻结严格校验过的 replay 快照；之后只把这批记录入队，不再打开一个
    // “线程已启动、二次读盘失败却继续 ready”的窗口。
    let replay_card_actions = card_action_wal
        .pending()
        .context(summon_i18n::text!("r11092a1312"))?;
    log(&summon_i18n::format!(
        "r2f7bf60044",
        card_health.pending,
        card_health.consumed
    ));
    // 上一代 ready/version 不能泄漏给本代 start；本代仅在所有入口都就绪后重写。
    let _ = std::fs::remove_file(ready_file());
    let _ = std::fs::remove_file(daemon_version_file());

    // 信号屏障必须早于 pidfile：一旦 CLI 能发现本进程，就保证 SIGTERM/SIGINT 只置 flag，
    // 绝不会落入默认动作绕过 InFlight/dispatcher 收尾。注册失败属于硬不变量破坏，启动失败。
    let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
    for sig in [signal_hook::consts::SIGTERM, signal_hook::consts::SIGINT] {
        signal_hook::flag::register(sig, Arc::clone(&shutdown))
            .with_context(|| summon_i18n::format!("r74512fb2b1"))?;
    }
    // 主线程的停机等待器,与上面的 flag 并存(同一信号可挂多个 action,全部执行):flag 供后台
    // producer 轮询,等待器让主线程零唤醒地阻塞等信号。**必须在此刻就注册**——注册后到达的信号
    // 会写进它的自管道,即便发生在漫长的启动期也不会丢,主线程稍后一 wait 就立刻返回。
    let mut shutdown_waiter =
        shutdown_wait::ShutdownWaiter::register().context(summon_i18n::text!("rbad827efcf"))?;
    log(&summon_i18n::format!(
        "rcd956c76ba",
        shutdown_waiter.read_fd()
    ));
    // 崩溃 vs 优雅停机判定:上次优雅停机会在 run() 末尾写 clean_shutdown 哨兵;启动时它在 = 上次正常退,
    // 不在 = 上次崩溃(SIGKILL/panic 跑不到末尾)。读后立即删(本次运行末尾会重新写)。was_crash 交给
    // 崩溃恢复扫描(下一提交)决定是否对残留 pending.json / 入站 WAL 推恢复卡片(只在崩溃时推,避免与
    // 优雅停机已入队的 ShutdownInterrupt 重复)。
    let clean_shutdown = read_clean_shutdown_marker_at(&clean_shutdown_marker())?;
    let was_crash = clean_shutdown.is_none();
    let clean_record = clean_shutdown
        .as_deref()
        .and_then(parse_clean_shutdown_record);
    let _ = std::fs::remove_file(clean_shutdown_marker());
    if let Some(line) = crash_guard::previous_exit_log(was_crash, had_boot_record) {
        log(line);
    }
    // flock 已拿到 → 我们是唯一 daemon。此刻任何 summon-sidecar 都是上个 daemon 没清干净的孤儿
    // (会重复连 events.sock、重复投递事件)。起自己的 sidecar 前先清掉,保证全局只有一个 sidecar。
    // 按**进程名**(comm=summon-sidecar)匹配,不用 -f —— 否则会误杀命令行里含该串的无关进程
    // (比如带 grep summon-sidecar 的诊断命令、本进程自己)。
    let _ = Command::new(summon_core::tools::resolve(summon_core::tools::Tool::Pkill))
        .arg("summon-sidecar")
        .status();
    // 上一代 daemon 遗留的 RPC 常驻会话进程(codex app-server / cursor acp)同样要清。
    // **但绝不能照抄上面那条 pkill**:`codex` / `cursor-agent` 是用户自己也在用的 CLI,按名字扫
    // 等于随时可能打死用户手上正跑的会话。只按 spawn 时写下的登记点名,且验明 pid 存活 + 启动
    // 时刻一致(防 pid 复用),见 `capture::rpc_procs`。
    let reaped = crate::capture::rpc_procs::sweep_orphans(was_crash)
        .context(summon_i18n::text!("r658b06101c"))?;
    if reaped > 0 {
        log(&summon_i18n::format!("r499dcee3c6"));
    }
    let (invalidated, preserved) = invalidate_all_recall_target_lifecycles(&replay_card_actions)
        .context(summon_i18n::text!("rc82f99400c"))?;
    if invalidated > 0 || preserved > 0 {
        log(&summon_i18n::format!("ra84ab6f41a"));
    }
    let cfg = Config::load();
    // state 目录拆分:把 agents/<alias>/ 下的机器状态挪到 state/<alias>/。
    // 失败不致命(旧布局照旧工作,`agent_state_dir` 回退读旧),只记日志。
    if let Err(e) = summon_core::registry::migrate_agent_state(&cfg.agents_dir) {
        log(&summon_i18n::format!("rb9c877b352"));
    }
    // Claude statusLine 是额度卡片的数据源。安装器会复核 ownership；statusLine 缺失时按安装状态
    // 重建，被改成别的值时 fail-closed，不覆盖用户设置。
    let claude_quota_target = match crate::setup::claude_quota::current_exe_install_target() {
        Ok(target) => Some(target),
        Err(error) => {
            log(&summon_i18n::format!("r71f9c82e69"));
            None
        }
    };
    if let Some(target) = &claude_quota_target {
        match crate::setup::claude_quota::ensure_installed(&target.paths, &target.spec) {
            Ok(crate::setup::claude_quota::IntegrationStatus::Installed) => {
                log(summon_i18n::text!("r3d5f769a3a"));
            }
            Ok(status) => {
                log(&summon_i18n::format!("ra8d17aa566"));
            }
            Err(error) => {
                log(&summon_i18n::format!("r1850a8a7a0"));
            }
        }
    }
    let (reg, registry_warnings) =
        Registry::load_tolerant(&cfg.agents_dir).context(summon_i18n::text!("r54daa52bee"))?;
    let reg = reg.with_global_config_path(&summon_core::config::config_path());
    for warning in registry_warnings {
        log(&summon_i18n::format!("r566dd403f4"));
    }
    // current-turn 不是恢复数据；上一代 daemon 崩溃留下的租约若继续可读，会让持久 tmux
    // 把上一条消息误升格为当前 Task。ready 前逐 agent 清代际残留，失败则拒绝启动。
    for agent in &reg.agents {
        crate::runtime::current_turn_store::CurrentTurnStore::for_agent(&agent.state_dir())
            .clear_on_daemon_boot()
            .with_context(|| summon_i18n::format!("rd2f77b2c79", agent.name))?;
        // 后台持有者跨代保留(tmux 里的后台任务没停),只清已经不在跑的。
        if let Err(error) = crate::runtime::background_holder::prune_on_boot(agent) {
            log(&summon_i18n::format!("r1d7389e0ca", agent.name));
        }
    }
    // 写 pidfile(stop / status 的身份入口；真正单例靠上面的 flock)。ready 仍未写，start 不会假成功。
    write_identity_file(&pid_file(), daemon_pid)?;

    // 起前把「常驻要求」文件铺好(全局 + 各 agent),让用户随时可编辑,不必等首条消息冷启才出现。
    let _ = ensure_instruction_file(&summon_home().join("CLAUDE.md"));

    // 每个可路由 agent:建通道 + 注册路由键(真实 app_id 若有 + stub:<name>),dispatcher 延后到 Core
    // 建好再 spawn —— 这样 dispatcher 能持 `Arc<Core>` 处理飞书卡片动作(闸门 resolve 等)。
    let mut routes: HashMap<String, Sender<DispatchMsg>> = HashMap::new();
    // agent 名 → dispatcher 发送端:Core 的后台线程按 agent 名回投消息给原 dispatcher。
    let mut agent_routes: HashMap<String, Sender<DispatchMsg>> = HashMap::new();
    struct Pending {
        agent: Agent,
        fc: Arc<FeishuClient>,
        epoch: FeishuEpochIdentity,
        rx: Receiver<DispatchMsg>,
        self_tx: Sender<DispatchMsg>,
    }
    let mut pending: Vec<Pending> = Vec::new();
    for agent in reg.routable() {
        let _ = ensure_instruction_file(&agent.dir.join("CLAUDE.md"));
        let (tx, rx) = channel::<DispatchMsg>();
        let (app_id, app_secret) = resolve_creds(agent);
        let epoch =
            FeishuEpochIdentity::from_resolved(agent, app_id.as_deref(), app_secret.as_deref());
        let dry = app_id.is_none() || app_secret.is_none();
        let fc = Arc::new(
            FeishuClient::new(
                &cfg.default_domain,
                app_id.clone().unwrap_or_default(),
                app_secret.unwrap_or_default(),
                dry,
            )
            .with_email_mask(crate::runtime::daemon::agent_env::email_mask_switch(agent)),
        );
        // 先装上一代查到的应用所有者(按本代凭据的 app_id 校验);所有判权副本都 clone 自 `reg`,
        // 共享同一个槽。
        crate::runtime::app_owner::load_cached(agent, app_id.as_deref());
        crate::runtime::app_owner::spawn_refresher(
            agent.clone(),
            Arc::clone(&fc),
            app_id.clone(),
            dry,
        );
        let self_tx = tx.clone();
        if let Some(id) = app_id {
            routes.insert(id, tx.clone());
        }
        agent_routes.insert(agent.name.clone(), tx.clone());
        routes.insert(format!("stub:{}", agent.name), tx);
        log(&summon_i18n::format!("r6ab995bd25", agent.name));
        pending.push(Pending {
            agent: agent.clone(),
            fc,
            epoch,
            rx,
            self_tx,
        });
    }
    // 主动补发批准必须与真正创建 FeishuClient 的同一凭据快照比对，不能先单独重读 `.env`
    // 再建 client：两次读取之间 A→B 会把 A fence 错批给 B。规划仍严格早于任何
    // dispatcher/sidecar/ready；crash、旧 marker、缺身份或任一盘上事实漂移都得到空集。
    let frozen_boot_epochs = pending
        .iter()
        .map(|p| (p.agent.name.clone(), p.epoch.fingerprint()))
        .collect::<HashMap<_, _>>();
    // 接管判据要的投递身份也取自同一份快照:它决定「这个 chat 归主路径还是归镜像」,
    // 与 fingerprint 分两次读凭据就可能落在两个身份上。
    let frozen_boot_scopes = pending
        .iter()
        .map(|p| (p.agent.name.clone(), p.epoch.storage_scope().key.clone()))
        .collect::<HashMap<_, _>>();
    let approved_mirror_catchup = clean_record
        .as_ref()
        .map(|record| plan_mirror_catchup(&reg, record, &frozen_boot_epochs, &frozen_boot_scopes))
        .unwrap_or_default();
    record_approved_mirror_catchup(approved_mirror_catchup);
    // outbox drain 线程需要的 per-agent 句柄(agent 快照 + FeishuClient)快照——在 dispatcher 循环
    // 消费 `pending` 前先 clone 出来(Agent Clone、fc 是 Arc),供消费者按 ev.agent re-resolve 投递。
    // 两张表都做成共享可变:运行期新上线的 agent 要往里插(见 `admit` 模块的「不接会怎样」)。
    let drain_agents = shared_drain_agents(
        pending
            .iter()
            .map(|p| {
                (
                    p.agent.name.clone(),
                    OutboxDrainAgent {
                        agent: p.agent.clone(),
                        client: Arc::clone(&p.fc),
                        epoch: p.epoch.clone(),
                    },
                )
            })
            .collect(),
    );
    let scheduler_scopes = shared_delivery_scopes(
        drain_agents
            .read()
            .map(|d| {
                d.iter()
                    .map(|(agent, drain)| (agent.clone(), drain.epoch.storage_scope().key.clone()))
                    .collect()
            })
            .unwrap_or_default(),
    );
    log(&summon_i18n::format!(
        "r7c89654ac8",
        std::process::id(),
        routes.len()
    ));
    // 在跑无头一轮的进程级注册表(Item A):chat 线程登记本轮 kill 句柄,优雅停机时据此打断所有
    // 在跑的无头子进程(整组 SIGINT,绝不碰 tmux)+ 落 pending.json 供重启续跑。
    // task run / worker / 蒸馏轮也通过 Core 持有的引用登记进来,停机时统一打断。
    let inflight = crate::runtime::inflight::InFlight::new();

    // Core Port 接缝:路由表 + operate-gate 的 agent 快照交给 Core 托管。
    // chat 入站经 Core 委派到 dispatcher(行为等价);闸门 resolve 在 Core 里过 operate-gate。
    let gate_agents = crate::runtime::core::gate_agents_from_registry(&reg);
    let core = Arc::new(crate::runtime::core::Core::new(
        routes,
        agent_routes,
        gate_agents,
    ));

    // 文件预览服务器(config.yaml 可关):起 HTTP 服务并挂进 Core,chat 线程构造 frontend 时
    // 据此给回复里的本地文件出按钮。绑定失败不拖垮 daemon——消息主链路不依赖它,
    // doctor 会把「开了但没起来」报出来。
    if cfg.preview_server.enabled {
        match crate::runtime::preview_server::PreviewServer::spawn(
            &cfg.preview_server,
            Arc::clone(&shutdown),
        ) {
            Ok(server) => core.attach_preview_server(server),
            Err(e) => log(&summon_i18n::format!("rba10a9f17f")),
        }
    }

    // 网页终端(默认开):独占 web_terminal.port,绑定地址沿用 preview_server.bind。绑定失败不拖垮
    // daemon,出卡时回「终端服务没有启动」并附原因,doctor 也会报出来。必须先于 dispatcher 与
    // chat 线程定下来:它们据此决定卡片上显不显示「>_」。
    let web_terminal = if cfg.web_terminal.enabled {
        match crate::runtime::web_terminal::WebTerminal::spawn(
            &cfg.web_terminal,
            &cfg.preview_server.bind,
            Arc::clone(&shutdown),
        ) {
            Ok(server) => crate::runtime::web_terminal::Service::Running(server),
            Err(e) => {
                log(&summon_i18n::format!("r4f5d1a85d7"));
                crate::runtime::web_terminal::Service::Absent(format!("{e:#}"))
            }
        }
    } else {
        crate::runtime::web_terminal::Service::Absent(summon_i18n::text!("rf76621f623").to_string())
    };
    crate::runtime::web_terminal::install(web_terminal);

    // 配置控制台(默认开):独占 dashboard.port,绑定地址沿用 preview_server.bind。绑定失败不拖垮
    // daemon,签发入口回「控制台没有启动」并附原因,doctor 也会报出来。会话只在内存,停机即失效;
    // accept 线程看停机标志自己退出。
    let dashboard = if cfg.dashboard.enabled {
        crate::runtime::dashboard::Dashboard::spawn(
            &cfg.dashboard,
            &cfg.preview_server.bind,
            &cfg.preview_server,
            Arc::clone(&shutdown),
        )
        .map_err(|e| {
            log(&summon_i18n::format!("rb882bddd21"));
            format!("{e:#}")
        })
    } else {
        Err(summon_i18n::text!("rd71351014f").to_string())
    };
    // 接口实现的环境:启动存档按这一刻实际用的配置登记(判断哪些改动要重启才生效),daemon 侧的
    // 事实与动作经 Core 取。它同时是每个请求重判进入权限的那道闸。
    if let Ok(dash) = &dashboard {
        let port: Arc<dyn crate::runtime::dashboard::api::Port> =
            Arc::new(super::dashboard_port::DaemonPort::new(Arc::clone(&core)));
        dash.attach_env(Arc::new(
            crate::runtime::dashboard::api::ApiEnv::new(
                summon_home(),
                cfg.agents_dir.clone(),
                crate::runtime::dashboard::boot_snapshot::BootSnapshot::capture(&cfg, &reg.agents),
                port,
            )
            .with_analysis_stop(Arc::clone(&shutdown)),
        ));
    }
    crate::runtime::dashboard::install(dashboard);

    // cmux 遥控:身份复验通过的绑定起 worker 与回传线程,对不上的只记为已结束,不发群消息。
    // 必须先于入站恢复、dispatcher 与卡片回放:它们都可能拉起遥控 worker,而只有这里做过
    // 启动复验、登记了停机位。回传入队的回复留在 outbox pending,等 drain 线程起来再投递。
    crate::remote::runtime::boot(&drain_agents, &shutdown);

    // 启动恢复必须严格先于 dispatcher:崩溃路径会先清 pending.json 并生成
    // 确认恢复卡;若 dispatcher 已启动,它会并发扫到 pending 而自动续跑,与恢复卡形成
    // 双执行。此时 receiver 仍保存在 `pending`,优雅路径的 reinject 可先入通道等 dispatcher
    // 启动后消费;函数返回即建立 clear/classify → scan pending 的 happens-before。
    boot_recover_inbox(
        &reg,
        &crate::runtime::outbox_store::OutboxStore::at_run_dir(),
        was_crash,
        &core,
        &crate::bridge::store::BridgeStore::runtime_default(),
        clean_record.as_ref(),
    )
    .context(summon_i18n::text!("rcffc0aab5b"))?;
    // Core 建好,起各 agent dispatcher(持 `Arc<Core>`:处理 feishu 卡片动作)。
    let (poll, deb) = (cfg.poll_ms, cfg.debounce_ms);
    for p in pending {
        let Pending {
            agent,
            fc,
            epoch,
            rx,
            self_tx,
        } = p;
        let core_d = Arc::clone(&core);
        let inflight_d = inflight.clone();
        thread::spawn(move || {
            agent_dispatcher(agent, poll, deb, fc, epoch, rx, self_tx, core_d, inflight_d)
        });
    }
    replay_card_action_records(&core, replay_card_actions);

    // control.sock(本机命令面:CLI → Core::submit)。绑在独立线程 accept,不阻塞 events 循环。
    let _ = std::fs::remove_file(control_sock());
    let ctrl = UnixListener::bind(control_sock())
        .with_context(|| summon_i18n::format!("r2da3744177", control_sock().display()))?;
    tighten_socket_perms(&control_sock());
    log(&summon_i18n::format!(
        "rc8f791cb7b",
        control_sock().display()
    ));
    let core_ctrl = Arc::clone(&core);
    thread::spawn(move || {
        for conn in ctrl.incoming() {
            match conn {
                Ok(c) => {
                    // S2:control 把 actor 当 Owner(gate 一律放行),必须先验对端 uid。
                    if !peer_uid_is_self(&c) {
                        log(summon_i18n::text!("r14c50d7e2b"));
                        continue;
                    }
                    let core = Arc::clone(&core_ctrl);
                    thread::spawn(move || handle_control_conn(c, core));
                }
                Err(e) => {
                    // unlink 后 listener 仍可能短暂存活；进程停机时日志即可，不在 accept 线程 panic。
                    log(&summon_i18n::format!("rb90d221e8f"));
                    if !control_sock().exists() {
                        break;
                    }
                }
            }
        }
    });

    // events.sock(先 bind,sidecar才能连)。listener 保持**阻塞**:accept 在独立线程里阻塞等,
    // 停机由 flag 复查负责,不需要非阻塞轮询(见 `events_accept_loop`)。
    let _ = std::fs::remove_file(events_sock());
    let listener = UnixListener::bind(events_sock())?;
    tighten_socket_perms(&events_sock());
    log(&summon_i18n::format!(
        "rb07bd24510",
        events_sock().display()
    ));

    // 外部 Claude Hook backlog 在宣告 ready 前先同步归约一次，随后由纯本地线程持续消费。
    // worker 不持有 frontend/tmux 句柄，停机只观察共享 flag 并 join。
    let bridge_drain = crate::bridge::runtime::spawn_bridge_drain(Arc::clone(&shutdown));

    // 先准备 sidecar 启动参数；真正 spawn 延后到 events accept loop 前，确保新 sidecar 的 v2
    // hello 不会在 daemon 做启动清扫时超时。尤其 selftest 只跑一次，不能靠 supervisor 重试。
    let selftest = std::env::var("SUMMON_SIDECAR_SELFTEST").as_deref() == Ok("1");
    // 配置改成**每轮 spawn 前重算**的闭包:sidecar 重启即等于「带上当下 registry 的 app 列表
    // 重连」,这是新 agent 免 daemon 重启上线的最后一环(app 列表只能在 sidecar 启动时下发)。
    let sidecar_control = Arc::new(SidecarControl::new());
    let sidecar_launch = if let Some(bin) = cfg.sidecar_bin.clone().or_else(default_sidecar_bin) {
        let agents_dir = cfg.agents_dir.clone();
        let domain = cfg.default_domain.clone();
        let rebuild = move || -> Option<String> {
            let (reg, _) = Registry::load_tolerant(&agents_dir).ok()?;
            let apps = build_sidecar_apps(&reg, &domain, selftest);
            if apps.is_empty() {
                return None;
            }
            serde_json::to_string(&SidecarConfig {
                language: summon_core::i18n::language(),
                language_file: run_dir().join("display-language").display().to_string(),
                events_sock: events_sock().display().to_string(),
                apps,
            })
            .ok()
        };
        // 起前先算一次:算不出来(没有任何带凭据的 agent)就别起 supervisor,与旧行为一致。
        if rebuild().is_none() {
            log(summon_i18n::text!("rcee9616341"));
            None
        } else {
            Some((bin, rebuild, selftest))
        }
    } else {
        log(summon_i18n::text!("r687b7c623e"));
        None
    };

    // outbox 消费者:**独立 detached 线程**(dispatcher 都起好了),每 tick 投递「停机/离线拆除」时
    // 入队的退出通知 → ACK 归档,失败退避重试、封顶死信。**绝不内联进 accept 循环**(否则会阻塞
    // 启动);停机时随 shutdown flag 退出。投递 NotifyChat 复用 deliver_notify(发飞书)。
    spawn_outbox_drain(Arc::clone(&drain_agents), Arc::clone(&shutdown));

    // P2 定时器:daemon 内单条 scheduler 线程,~30s tick → due_plan → ScheduledFire(随 shutdown 退)。
    let scheduler = scheduler::spawn_scheduler(
        core.clone(),
        Arc::clone(&shutdown),
        Arc::clone(&scheduler_scopes),
    );
    // owner sentinel:任意会话召唤 agent 的轮询器。无条件 spawn、tick 内看 config 热开关
    // (enabled 默认 false 时纯空转);随 shutdown 位退出,join 时机与 scheduler 一致。
    let sentinel = sentinel::poller::spawn_sentinel(
        core.clone(),
        Arc::clone(&drain_agents),
        Arc::clone(&shutdown),
    );
    let document_comments =
        document_comments::spawn(Arc::clone(&drain_agents), Arc::clone(&shutdown));

    // 跨机 fleet 聚合:后台拉远端 summon 的 `agents status --json` 进缓存。无条件 spawn、
    // tick 内读配置(`remotes` 为空时纯空转),渲染侧只读缓存故永不阻塞 dispatcher。
    let remote_fleet = crate::observe::remote_fleet::spawn_refresher(Arc::clone(&shutdown));

    // CLI 额度观测是 chat/task 热路径之外的独立 producer，始终运行以维护账号最新快照，
    // 供菜单栏角标与额度卡片读取。
    let quota_observe = crate::codex::coordinator::spawn(Arc::clone(&shutdown))
        .context(summon_i18n::text!("rbffe63d8e6"))?;
    let claude_status_line_guard = claude_quota_target
        .map(|target| spawn_claude_status_line_guard(target, Arc::clone(&shutdown)));
    // claude 的用量从会话转录里记账(运行记录与轮外用量);旁路,起不来只记日志。
    let claude_usage =
        crate::runtime::claude_usage::spawn_scanner(cfg.agents_dir.clone(), Arc::clone(&shutdown))
            .inspect_err(|error| log(&summon_i18n::format!("r1fcf4898e2")))
            .ok();

    // 本代 ready 的唯一写点：signal handler 已安装、dispatcher 已 spawn、events/control 两条
    // socket 已成功 bind、后台 producer 已启动。版本身份先原子落盘，ready 再作为发布屏障；
    // 父 CLI 必须读到**同一 PID**才回报 start 成功。
    if shutdown.load(std::sync::atomic::Ordering::SeqCst) {
        let _ = quota_observe.join();
        if let Some(guard) = claude_status_line_guard {
            let _ = guard.join();
        }
        let _ = remote_fleet.join();
        let _ = scheduler.join();
        let _ = bridge_drain.join();
        if let Some(scanner) = claude_usage {
            let _ = scanner.join();
        }
        // 停机是被要求的,不是失败:退出码 0,自启单元不会把它拉回来;启动记录一并清掉,下一次
        // 启动不把这一代算成「没跑稳就死了」。
        crash_guard::clear_boot_record(daemon_pid);
        log(summon_i18n::text!("r4ff72dfdb8"));
        return Ok(());
    }
    write_daemon_version_identity(&daemon_version_file(), daemon_pid, crate::version::VERSION)?;
    write_identity_file(&ready_file(), daemon_pid)?;
    crash_guard::mark_ready(daemon_pid);

    // 清理入站 WAL 的 consumed/ 与 recalled/ 旧记录(7 天保留),防无限增长。
    let inbox_wal = crate::runtime::inbox_wal::InboxWal::new(run_dir().join("inbox"));
    let cleanup_now = now_unix_ms();
    inbox_wal.cleanup_consumed(cleanup_now);
    inbox_wal.cleanup_recalled(cleanup_now);
    crate::runtime::turn_ledger::prune(&crate::runtime::turn_ledger::dir(), cleanup_now as i64);
    crate::runtime::turn_ledger::prune(
        &crate::runtime::turn_ledger::outside_dir(),
        cleanup_now as i64,
    );
    // 控制台的费用按这份价格表算:后台拉取,失败只记日志,不影响启动。
    crate::runtime::prices::spawn_refresher(run_dir());
    // npm 安装的自动升级:后台检查、装包、等整机空闲后换代;不往飞书发任何东西。
    #[cfg(npm_dist)]
    crate::setup::npm_dist::auto_upgrade::spawn(Arc::clone(&shutdown));
    // PF2:启动期清扫各 agent 的 inbound 附件/长引用 spill(30 天 TTL)——它们是「为某条
    // 消息落的临时上下文」,此前无任何轮转/清理,重度使用下无界增长。失败 log-ignore。
    for a in reg.routable() {
        let n = cleanup_inbound_spill(
            &a.state_dir(),
            Duration::from_secs(30 * 24 * 3600),
            SystemTime::now(),
        );
        if n > 0 {
            log(&summon_i18n::format!("r3ff0d6a5aa", a.name));
        }
    }

    if let Some((bin, rebuild, selftest)) = sidecar_launch {
        log(&format!(
            "spawn sidecar: {} (selftest={selftest})",
            bin.display()
        ));
        let control = Arc::clone(&sidecar_control);
        thread::spawn(move || supervise_sidecar_with(bin, rebuild, selftest, control));
    }

    // 装配「运行期接新 agent 上线」的注册器,回填进 Core —— 这样 `Intent::ReloadAgents` 在 Core
    // 内就能自足处理。必须在 Core 建好之后(要 Weak 引用)、且在 sidecar supervisor 起来之后
    // (它要能 SIGTERM 到当前 child)。
    core.attach_agent_identities(Arc::clone(&drain_agents));
    core.attach_admitter(Arc::new(crate::runtime::daemon::AgentAdmitter::new(
        Arc::downgrade(&core),
        cfg.agents_dir.clone(),
        cfg.default_domain.clone(),
        cfg.poll_ms,
        cfg.debounce_ms,
        inflight.clone(),
        Arc::clone(&drain_agents),
        Arc::clone(&scheduler_scopes),
        Arc::clone(&sidecar_control),
    )));

    // events accept 挪到独立线程 + 阻塞 accept(与上面 control.sock 同范式),detached 不 join:
    // 停机收尾全在主线程,与本线程无耦合;它最多阻塞在 accept 上,随进程退出被回收。
    let core_ev = Arc::clone(&core);
    let shutdown_ev = Arc::clone(&shutdown);
    thread::spawn(move || {
        events_accept_loop(listener, &shutdown_ev, &events_sock(), move |c| {
            // accept 出的连接在 macOS 上**继承 listener 的 O_NONBLOCK**。listener 现在是阻塞的,
            // 但这里仍显式钉死(回归 fbee83c):`handle_conn` 用 `lines().map_while(Result::ok)`
            // 逐行读,连接一旦非阻塞,首次无数据的 `WouldBlock`(Err)会被误判成流结束 → 立刻关
            // 连接;而 sidecar 正是「先连上、几秒后(WS connected)才发首条事件」的持久连接,
            // 会在发数据前就被关掉 → 入站全丢。
            let _ = c.set_nonblocking(false);
            // S2:events 事件里的 operator_open_id 被 can_operate 直接信任,先验对端 uid
            // (只信 daemon 亲手 spawn 的同 uid sidecar / 同用户 dev 注入)。
            if !peer_uid_is_self(&c) {
                log(summon_i18n::text!("r976a26a6e0"));
                return;
            }
            let core = Arc::clone(&core_ev);
            thread::spawn(move || handle_conn(c, core));
        });
    });

    // 主线程只剩「等停机」:阻塞在信号上,零空唤醒。返回后照旧走下面的停机收尾序列。
    shutdown_waiter.wait(&shutdown);

    // 停机先撤 ready/新连接入口，再 join 所有后台 producer。stop flag 已由 signal handler 置 true；
    // scheduler 必须完全退出后才能往 dispatcher 排 Shutdown，否则它可能把 ScheduledFire 排在
    // Shutdown 后面、send 成功并 checkpoint，随后被 dispatcher 丢弃，造成永久漏触发。
    let _ = remove_identity_file_if_matches(&ready_file(), daemon_pid);
    let _ = remove_daemon_version_if_matches(daemon_pid);
    let _ = std::fs::remove_file(events_sock());
    let _ = std::fs::remove_file(control_sock());
    if scheduler.join().is_err() {
        log(summon_i18n::text!("rab20d60b2e"));
    }
    if sentinel.join().is_err() {
        log(summon_i18n::text!("r8ccb163a72"));
    }
    if document_comments.join().is_err() {
        log(summon_i18n::text!("r398bcf6d7a"));
    }
    // 洞察分析看停机标志自己打断进程组(SIGINT、宽限后 SIGKILL),这里等它收手。
    crate::runtime::insights_analysis::wait_stopped(Duration::from_secs(5));
    if bridge_drain.join().is_err() {
        log(summon_i18n::text!("r786fc210e1"));
    }
    if quota_observe.join().is_err() {
        log(summon_i18n::text!("rd5d1bb9fce"));
    }
    // 复查线程会写 `~/.claude/settings.json`，等它收手再退，不在用户目录里留半截临时文件。
    if let Some(guard) = claude_status_line_guard {
        if guard.join().is_err() {
            log(summon_i18n::text!("r24c44a2913"));
        }
    }
    // 扫描线程收手之后,把没有在跑的轮的会话的轮外用量写掉、游标存盘。
    if let Some(scanner) = claude_usage {
        if scanner.join().is_err() {
            log(summon_i18n::text!("rbec8b4a6c9"));
        }
    }
    crate::runtime::claude_usage::flush_on_shutdown();
    // 网页终端的连接线程不被 join,进程可能先于它们退出:按钉住表把窗口恢复原取值,有上限地等。
    // 只写窗口级 `window-size`,不碰会话。
    crate::runtime::web_terminal::restore_pins_on_shutdown();
    // scheduler 已静止后再冻结 InFlight 注册(与 insert 同锁)、停 dispatcher，最后快照/drain。
    // 屏障后竞态 spawn 的无头轮会在 insert 处当场被终止。这里不调用任何 tmux API。
    inflight.freeze();
    // 取自 Core 的路由表而非启动时拍下的快照:运行期上线的 agent 也必须收到 Shutdown,
    // 否则它的 dispatcher 线程会漏掉停机序列。
    for tx in core.all_dispatcher_senders() {
        let _ = tx.send(DispatchMsg::Shutdown);
    }

    // 优雅停机:打断所有在跑的无头子进程(整组 SIGINT → 2s 宽限 → SIGKILL 兜底)+ 为每条在跑轮
    // 落 pending.json(重启续跑)。**绝不碰 tmux 会话**(claude 持久层;注册表里就没有 tmux 句柄)。
    log(summon_i18n::text!("r1c8e913847"));
    // 停机 producer 钩子:为每条在跑轮入队一条「会话被打断」通知(reason=ShutdownInterrupt),
    // 纯本地 tmp+rename 不联网、失败 log-ignore,绝不占用 2s 宽限预算。重启后 drain 线程投递。
    let shutdown_outbox = crate::runtime::outbox_store::OutboxStore::at_run_dir();
    let n = inflight.shutdown_interrupt_all(
        Duration::from_secs(2),
        &crate::runtime::inflight::group_alive,
        &|agent_name, p| enqueue_shutdown_notify(&shutdown_outbox, &reg, agent_name, p),
    );
    log(&summon_i18n::format!("redc1b3c72e"));
    // 干净退出哨兵:走到这里 = 优雅停机(收到 SIGTERM/SIGINT 后正常收尾)。写下它,下次启动据其存在
    // 判定「上次是正常退」,不触发崩溃恢复扫描(优雅停机的中断通知已由 shutdown_interrupt_all 入队)。
    // SIGKILL/panic 跑不到这行 → 哨兵缺失 → 下次启动判为崩溃。
    // stop fence 必须冻结 daemon 本代实际持有的飞书身份，不能在停机末尾重读 `.env`：
    // 凭据可能已在本代运行期间漂移，重算会把旧连接产生的内容错误签给下一代新身份。
    let frozen_mirror_epochs = drain_agents
        .read()
        .map(|agents| {
            agents
                .iter()
                .map(|(name, drain)| (name.clone(), drain.epoch.fingerprint()))
                .collect::<HashMap<_, _>>()
        })
        .unwrap_or_default();
    // 停机时刻在扫 fence **之前**取:逐 agent 扫转录、算 checkpoint 是有耗时的,算进去等于把
    // 「停机后过了多久」记短一截,接管的新鲜度闸会因此偏松。
    let stopped_at_secs = crate::runtime::daemon::dispatcher::unix_now_secs();
    let clean_record = CleanShutdownRecordV2 {
        version: 2,
        mirror_fences: capture_mirror_stop_fences(&reg, &frozen_mirror_epochs),
        stopped_at_secs,
    };
    if let Ok(clean_marker) = serde_json::to_vec(&clean_record) {
        let _ = crate::runtime::fsio::atomic_write(&clean_shutdown_marker(), &clean_marker);
    }
    crash_guard::clear_boot_record(daemon_pid);
    // sidecar 随本进程退出而 stdin-EOF 死随父,无需显式杀。
    let _ = remove_identity_file_if_matches(&pid_file(), daemon_pid);
    let _ = remove_identity_file_if_matches(&ready_file(), daemon_pid);
    let _ = remove_daemon_version_if_matches(daemon_pid);
    Ok(())
}

/// Claude statusLine 运行期复查的间隔。
const CLAUDE_STATUS_LINE_CHECK_INTERVAL: Duration = Duration::from_secs(60);

/// statusLine 是 claude 卡片上下文用量与额度的唯一数据源，而 `~/.claude/settings.json` 会在
/// daemon 与托管会话都还跑着的时候被整份重置(重新登录、清理目录)。只在启动时检查，就会一直
/// 缺到下次重启，所以运行期按分钟复查，缺了就按安装状态重建；Claude Code 会热加载 settings，
/// 在跑的会话随之恢复。日志只在结论变化时写，避免每分钟重复同一条失败。
fn spawn_claude_status_line_guard(
    target: crate::setup::claude_quota::InstallTarget,
    shutdown: Arc<std::sync::atomic::AtomicBool>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let tick = Duration::from_millis(250);
        let mut next_run = Instant::now() + CLAUDE_STATUS_LINE_CHECK_INTERVAL;
        let mut last_error: Option<String> = None;
        while !shutdown.load(std::sync::atomic::Ordering::SeqCst) {
            if Instant::now() >= next_run {
                match crate::setup::claude_quota::restore_if_missing(&target) {
                    Ok(restored) => {
                        if restored {
                            log(summon_i18n::text!("r2a216708c0"));
                        }
                        last_error = None;
                    }
                    Err(error) => {
                        let message = format!("{error:#}");
                        if last_error.as_deref() != Some(message.as_str()) {
                            log(&summon_i18n::format!("r67fca35547"));
                        }
                        last_error = Some(message);
                    }
                }
                next_run = Instant::now() + CLAUDE_STATUS_LINE_CHECK_INTERVAL;
            }
            thread::sleep(tick);
        }
    })
}

fn parse_clean_shutdown_record(raw: &str) -> Option<CleanShutdownRecordV2> {
    let record: CleanShutdownRecordV2 = serde_json::from_str(raw).ok()?;
    (record.version == 2).then_some(record)
}

fn read_clean_shutdown_marker_at(path: &std::path::Path) -> anyhow::Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(raw) => Ok(Some(raw)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => {
            Err(error).with_context(|| summon_i18n::format!("r2bae364d1f", path.display()))
        }
    }
}

#[cfg(test)]
mod agents_missing_tests {
    use super::agents_missing_notice;

    #[test]
    fn missing_agents_dir_explains_to_onboard_first() {
        let dir = tempfile::tempdir().unwrap();
        let notice = agents_missing_notice(&dir.path().join("agents")).expect("目录不在要说明");
        assert!(notice.contains("summon onboard"), "{notice}");
        assert_eq!(agents_missing_notice(dir.path()), None);
        // 在但不是目录:交给 registry 报错,不当成「还没有 agent」悄悄退出。
        let file = dir.path().join("not-a-dir");
        std::fs::write(&file, "").unwrap();
        assert_eq!(agents_missing_notice(&file), None);
    }

    /// daemon 自己判在启动记录与停机哨兵之前:以 0 退出时不能留下「上一代没走完停机」的现场,
    /// `daemon start` 判在拉起子进程之前。
    #[test]
    fn both_entries_check_for_agents_before_leaving_any_trace() {
        let boot = include_str!("boot.rs");
        let run = &boot[boot.find("pub fn run()").unwrap()..];
        let check = run.find("agents_missing_notice(").unwrap();
        let record = run.find("crash_guard::check_and_record(").unwrap();
        assert!(check < record);
        assert!(check < run.find("read_clean_shutdown_marker_at(").unwrap());
        // 上一代有没有启动记录,要在本代写下自己的记录之前读。
        assert!(run.find("crash_guard::previous_boot_recorded()").unwrap() < record);

        let cli = include_str!("../../cli/cli.rs");
        let start = &cli[cli.find("fn daemon_start()").unwrap()..];
        let start = &start[..start.find("\nfn ").unwrap()];
        let check = start.find("agents_missing_notice(").unwrap();
        assert!(check < start.find("command.spawn()").unwrap());
        assert!(check < start.find("start_supervised(").unwrap());
    }
}

#[cfg(test)]
mod clean_shutdown_marker_tests {
    use super::{
        parse_clean_shutdown_record, read_clean_shutdown_marker_at, CleanShutdownRecordV2,
        MirrorStopFence,
    };

    #[test]
    fn only_clean_shutdown_v2_json_can_authorize_mirror_catchup() {
        let record = CleanShutdownRecordV2 {
            version: 2,
            mirror_fences: vec![MirrorStopFence {
                agent: "a".into(),
                feishu_epoch_fingerprint: "digest".into(),
                session: "s".into(),
                tag: "g1".into(),
                chat_id: "oc".into(),
                chat_type: "group".into(),
                // 外层 clean-shutdown schema 是 v2；内部 mirror offset 已是绑定 epoch/kind 的 v3。
                offset_record: "v3 epoch sid 1 00 terminal".into(),
                stop_eof: 42,
                stop_eof_checkpoint: "AA".into(),
                stopped_at_secs: 1_700_000_000,
            }],
            stopped_at_secs: 1_700_000_000,
        };
        let raw = serde_json::to_string(&record).unwrap();
        assert_eq!(parse_clean_shutdown_record(&raw), Some(record));
        for invalid in ["clean\n", "{}", r#"{"version":1,"mirror_fences":[]}"#] {
            assert_eq!(
                parse_clean_shutdown_record(invalid),
                None,
                "旧版/损坏 marker 不能授权主动群出站: {invalid:?}"
            );
        }
    }

    /// 上一代写的 marker 没有停机时刻这一行(旧格式),反序列化必须落到 0 —— 接管的新鲜度
    /// 判据据此判过期、不接管:证不了停了多久就不接,而不是猜。
    #[test]
    fn a_marker_written_before_stopped_at_existed_parses_as_expired() {
        let record = parse_clean_shutdown_record(r#"{"version":2,"mirror_fences":[]}"#)
            .expect("v2 应可解析");
        assert_eq!(record.stopped_at_secs, 0);
    }

    #[test]
    fn only_a_missing_clean_marker_counts_as_crash_evidence() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing-clean-marker");
        assert_eq!(read_clean_shutdown_marker_at(&missing).unwrap(), None);

        let marker = dir.path().join("clean-marker");
        std::fs::write(&marker, "clean").unwrap();
        assert_eq!(
            read_clean_shutdown_marker_at(&marker).unwrap(),
            Some("clean".to_string())
        );

        let error = read_clean_shutdown_marker_at(dir.path())
            .expect_err("目录/权限类读取错误不能伪装成 marker 不存在");
        assert!(error.to_string().contains("fail-closed"), "{error:#}");
    }
}

/// 优雅停机的 outbox producer:为一条被打断的在跑轮入队「会话被打断」回执
///(`reason=ShutdownInterrupt`)。**纯本地 tmp+rename 不联网**,失败只 log,绝不占用 2s 宽限预算;
/// 重启后由 drain 线程投递。
///
/// 两处提前 return 都是「拿不出可证明的身份就不发」,不是可有可无的防御:
/// - `delivery_scope` 空 = 旧/非 Chat checkpoint 没冻结过飞书身份,从当前 agent 配置猜一个
///   等于把回执发到换绑后的另一个 app;
/// - agent 已不在 registry(罕见)= 文案/凭据都 resolve 不出来。
///
/// 身份三项(scope / chat_id / chat_type)一律取轮创建时冻结在 `PendingTurn` 里的值,不重算。
fn enqueue_shutdown_notify(
    outbox: &crate::runtime::outbox_store::OutboxStore,
    reg: &Registry,
    agent_name: &str,
    p: &summon_core::pending::PendingTurn,
) {
    if p.delivery_scope.is_empty() {
        return;
    }
    let Some(a) = reg.get(agent_name) else {
        return;
    };
    exit_receipt::enqueue(
        outbox,
        &resolve_messaging(a),
        &ExitReceipt {
            agent: a,
            delivery_scope: &p.delivery_scope,
            chat_type: &p.chat_type,
            chat_id: &p.chat_id,
            chat_name: None,
            // 停机路径不查 .meta creator(best-effort,先发原 chat 兜底)。
            creator: None,
            event_operator: None,
            reason: summon_core::exit::ExitReason::ShutdownInterrupt,
            // ShutdownInterrupt 是临时打断、会话会续跑;created_at 留空(下次真实退出时再展示)。
            session_created_at: None,
        },
    )
    .log_if_failed(&a.name);
}

/// 为可路由 agent 构造sidecar app 列表。selftest 用 stub:<name> + 占位 secret;
/// 否则只收录有真实 env 凭据的 agent。
pub(super) fn build_sidecar_apps(
    reg: &Registry,
    default_domain: &str,
    selftest: bool,
) -> Vec<AppCfg> {
    let mut apps = Vec::new();
    for a in reg.routable() {
        if crate::runtime::agent_retirement::retired(&a.dir) {
            continue;
        }

        if selftest {
            apps.push(AppCfg {
                app_id: format!("stub:{}", a.name),
                app_secret: "selftest".into(),
                domain: default_domain.to_string(),
            });
        } else {
            let (id, sec) = resolve_creds(a);
            if let (Some(id), Some(sec)) = (id, sec) {
                apps.push(AppCfg {
                    app_id: id,
                    app_secret: sec,
                    domain: default_domain.to_string(),
                });
            }
        }
    }
    apps
}

/// 默认 sidecar 二进制:与 summon 同目录优先,回退 `~/.summon/bin/`(见 `summon_core::companion`)。
pub fn default_sidecar_bin() -> Option<PathBuf> {
    summon_core::companion::locate(summon_core::companion::SIDECAR)
}

/// sidecar 日志上限:与 daemon.log 同档(10MB,保留一代)。这里独立一份常量而不复用
/// `observe::log`——那边的轮转靠 dup2 改**本进程** fd,天生够不到子进程,两者只是规则相同。
const SIDECAR_LOG_MAX_BYTES: u64 = 10 * 1024 * 1024;

/// sidecar 独立日志:`~/.summon/run/sidecar.log`。
fn sidecar_log_path() -> PathBuf {
    run_dir().join("sidecar.log")
}

/// 超限就把 `path` 挪成 `path.1`(只保留一代)。best-effort:rename 失败(权限/跨设备)只意味着
/// 本轮不轮转、继续往原文件追加,绝不让日志问题挡住 sidecar 启动。返回是否真的轮转了。
fn rotate_if_oversized(path: &std::path::Path, max_bytes: u64) -> bool {
    let oversized = std::fs::metadata(path)
        .map(|m| m.len() > max_bytes)
        .unwrap_or(false);
    if !oversized {
        return false;
    }
    let mut rotated = path.as_os_str().to_owned();
    rotated.push(".1");
    std::fs::rename(path, PathBuf::from(rotated)).is_ok()
}

/// 打开 sidecar 日志(先按同一规则轮转一次)。
/// **why 不用 `Stdio::inherit()`**:sidecar 是 daemon spawn 的独立进程,inherit 会让它的 fd1/fd2
/// 与 daemon 指向同一个 inode;而 daemon 的轮转靠 dup2 只改自己进程的 fd,够不到 sidecar ——
/// 轮转后 sidecar 继续往已被 rename 的旧 inode 写,且它的输出全程绕过 daemon 的轮转闸
/// (实测占 daemon.log 15% 行数)。给它独立文件 + 同规则轮转,两条日志各自封顶。
fn open_sidecar_log_at(path: &std::path::Path, max_bytes: u64) -> std::io::Result<std::fs::File> {
    rotate_if_oversized(path, max_bytes);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
}

/// sidecar 子进程的重启闸。sidecar 的 app 列表是**启动时经 stdin 一次性下发**的,新 agent 的
/// 飞书长连接只能靠重开进程建立 —— supervisor 本就有退避重启循环,故「让新 app 上线」= 杀掉当前
/// child 让它自然重建,重建时从 registry 重算配置。
///
/// why 这样也值得:daemon 整体重启会 SIGTERM 掉所有在跑的无头轮、丢掉全部内存态;只重启 sidecar
/// 仅断几秒飞书长连接 —— 而 daemon 重启本来也会连带重启 sidecar(stdin EOF),所以这条路径在
/// 丢消息维度上不比现状差,省下的是打断与状态重建。
pub(crate) struct SidecarControl {
    /// 当前 child 的 pid(supervisor 每轮 spawn 后写入,退出时清空)。None = 此刻没在跑。
    pid: std::sync::Mutex<Option<u32>>,
    configuration: std::sync::Mutex<()>,
}

impl SidecarControl {
    pub(crate) fn new() -> Self {
        Self {
            pid: std::sync::Mutex::new(None),
            configuration: std::sync::Mutex::new(()),
        }
    }

    pub(crate) fn quiesce(&self) -> anyhow::Result<()> {
        let _configuration = self
            .configuration
            .lock()
            .map_err(|_| anyhow::anyhow!(summon_i18n::format!("ra21e7b47b0")))?;
        self.request_restart();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if self
                .pid
                .lock()
                .map_err(|_| anyhow::anyhow!(summon_i18n::format!("ra6b92ab5a6")))?
                .is_none()
            {
                return Ok(());
            }
            anyhow::ensure!(
                Instant::now() < deadline,
                summon_i18n::format!("r4d3fd2d08f")
            );
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn set_pid(&self, pid: Option<u32>) {
        if let Ok(mut slot) = self.pid.lock() {
            *slot = pid;
        }
    }

    /// 请求重启:给当前 child 发 SIGTERM,supervisor 的 `wait` 返回后按既有退避重开。
    /// 没在跑(pid None / 已退出)→ 返回 false,调用方据此提示「sidecar 未运行」。
    /// **只 kill sidecar 自己的 pid**,不碰进程组、更不碰 tmux。
    pub(crate) fn request_restart(&self) -> bool {
        let Ok(slot) = self.pid.lock() else {
            return false;
        };
        let Some(pid) = *slot else {
            return false;
        };
        // SIGTERM 到 **sidecar 自己这一个 pid**:Go 侧正常退出即可(与无头 CLI 用 SIGINT 保留
        // 被打断上下文不同)。绝不用 killpg —— 进程组里还有 daemon 自己和它的其它子进程。
        nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(pid as i32),
            nix::sys::signal::Signal::SIGTERM,
        )
        .is_ok()
    }
}

/// 监督 Go sidecar:spawn + stdin 下发配置(并**保持 stdin 打开**作为父进程存活信号)+ 退避重启。
/// sidecar侧监听 stdin EOF → daemon 一旦退出(stop/crash/kill)sidecar随之退出,杜绝孤儿。selftest 跑完不重启。
/// `rebuild_cfg` 每轮 spawn 前重算配置:重启即等于「带上当下 registry 的 app 列表重连」,
/// 这是新 agent 免 daemon 重启上线的最后一环。
pub(super) fn supervise_sidecar_with(
    bin: PathBuf,
    rebuild_cfg: impl Fn() -> Option<String>,
    selftest: bool,
    control: Arc<SidecarControl>,
) {
    match sidecar_protocol_version(&bin) {
        Ok(version) if version == SIDECAR_PROTOCOL_VERSION => {}
        Ok(version) => {
            log(&summon_i18n::format!("r599869c8f8", bin.display()));
            return;
        }
        Err(error) => {
            log(&summon_i18n::format!("r1fea28c345", bin.display()));
            return;
        }
    }
    let mut backoff = 1u64;
    loop {
        let configuration = control
            .configuration
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // 空配置时保留 supervisor，后续恢复或新增 agent 可重新建立连接。
        let Some(cfg_json) = rebuild_cfg() else {
            drop(configuration);
            if selftest {
                return;
            }
            thread::sleep(Duration::from_secs(1));
            continue;
        };
        // 每次 spawn 前查一次轮转:句柄一旦交给子进程就没法在它运行期换文件,而 sidecar 是长驻
        // 进程。按重启粒度轮转对当前量级(daemon.log 46 天共 6.1MB ≈ 0.13MB/天)绰绰有余。
        let (out, err) = match open_sidecar_log_at(&sidecar_log_path(), SIDECAR_LOG_MAX_BYTES)
            .and_then(|f| f.try_clone().map(|c| (f, c)))
        {
            Ok((f, c)) => (Stdio::from(f), Stdio::from(c)),
            Err(e) => {
                // 打不开独立日志属罕见磁盘/权限故障:宁可退回与 daemon.log 混写,也不静默丢掉
                // sidecar 的诊断输出(丢了就查不了飞书长连接问题)。
                log(&summon_i18n::format!("r6ddf836939"));
                (Stdio::inherit(), Stdio::inherit())
            }
        };
        let child = Command::new(&bin)
            .arg0("summon-sidecar") // 按功能命名:ps 显示 summon-sidecar,带 summon 关键字
            .env("SIDECAR_SELFTEST", if selftest { "1" } else { "0" })
            .stdin(Stdio::piped())
            .stdout(out)
            .stderr(err)
            .spawn();
        let mut child = match child {
            Ok(c) => c,
            Err(e) => {
                drop(configuration);
                log(&summon_i18n::format!("r5f7bc4a42d"));
                thread::sleep(Duration::from_secs(backoff));
                backoff = (backoff * 2).min(30);
                continue;
            }
        };
        // 写配置后**不丢弃 sin**:管道在 child 存活期间保持打开;daemon 进程一死,
        // OS 关闭该 fd → sidecar stdin 读到 EOF → sidecar自杀(死随父)。
        let mut keep_stdin = child.stdin.take();
        if let Some(sin) = keep_stdin.as_mut() {
            let _ = sin.write_all(cfg_json.as_bytes());
            let _ = sin.flush();
        }
        // 登记 pid,让 `SidecarControl::request_restart` 能精确 SIGTERM 到这一个进程。
        control.set_pid(Some(child.id()));
        drop(configuration);
        let status = child.wait();
        control.set_pid(None);
        drop(keep_stdin);
        log(&summon_i18n::format!("ra8eedd83d5"));
        if selftest {
            return; // selftest 一次性
        }
        thread::sleep(Duration::from_secs(backoff));
        backoff = (backoff * 2).min(30);
    }
}

pub(crate) fn sidecar_protocol_version(bin: &std::path::Path) -> anyhow::Result<u32> {
    let output = Command::new(bin)
        .arg("protocol-version")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .context(summon_i18n::text!("r2c31d65dbe"))?;
    anyhow::ensure!(output.status.success(), summon_i18n::format!("r066293f008"));
    let stdout = String::from_utf8(output.stdout).context(summon_i18n::text!("r5ef6501096"))?;
    stdout
        .trim()
        .parse::<u32>()
        .context(summon_i18n::text!("r52cba59d1c"))
}

/// 客户端:把一条 `Command` 经 control.sock 发给 daemon,读回 `CommandOutcome`。
/// 本机 CLI 命令用它。daemon 没起 / socket 不通 → 带可操作提示的错误。
pub fn submit_command(
    cmd: &summon_core::core_port::Command,
) -> anyhow::Result<summon_core::core_port::CommandOutcome> {
    let mut stream =
        UnixStream::connect(control_sock()).context(summon_i18n::text!("rbc312d5e04"))?;
    let line = serde_json::to_string(cmd).context(summon_i18n::text!("r5bdefa97fc"))?;
    stream.write_all(line.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()?;
    let mut resp = String::new();
    BufReader::new(stream)
        .read_line(&mut resp)
        .context(summon_i18n::text!("r25e5f5409c"))?;
    serde_json::from_str(resp.trim()).context(summon_i18n::text!("rf60a3908c0"))
}

/// 本地注入一条伪造消息事件(stub 测试,无飞书)。仅 debug 构建用(dev 命令)。
#[cfg(debug_assertions)]
pub fn inject(agent: &str, text: &str) -> anyhow::Result<()> {
    let ev = Event::Message {
        app_id: format!("stub:{agent}"),
        chat_id: "stub-chat".into(),
        chat_type: None,
        thread_id: None,
        root_id: None,
        sender: "owner".into(),
        sender_union_id: String::new(),
        sender_type: String::new(),
        sender_name: "owner".into(),
        text: text.into(),
        message_id: format!("stub-{}", text.len()),
        parent_id: None,
        mentions: vec![],
        resources: vec![],
        ts: 0,
        create_time_ms: 0,
    };
    let mut stream =
        UnixStream::connect(events_sock()).context(summon_i18n::text!("r4dc947e86c"))?;
    stream.write_all(ev.to_line().as_bytes())?;
    println!("{}", summon_i18n::format!("r014ca8a08c"));
    Ok(())
}

#[cfg(test)]
mod shutdown_notify_tests {
    use super::*;
    use summon_core::exit::ExitReason;
    use summon_core::pending::{PendingTurn, PendingTurnKind};

    fn pending(scope: &str) -> PendingTurn {
        PendingTurn {
            kind: PendingTurnKind::Chat,
            delivery_scope: scope.into(),
            agent_name: "test".into(),
            inbox_message_ids: vec![],
            resume_authorized: false,
            chat_id: "oc_shutdown".into(),
            chat_type: "group".into(),
            cli: "cursor".into(),
            prompt: "干活".into(),
            session_id: None,
            partial: String::new(),
        }
    }

    /// 停机回执按**盘上生效的** messaging resolve(热重载语义),本机 config 可能把这条文案配空。
    /// 故期望值由同一份配置推出,断言才不随开发机配置漂移。
    fn expects_event(agent: &Agent) -> bool {
        !resolve_messaging(agent)
            .render_session_exited_text(ExitReason::ShutdownInterrupt, None, "oc_shutdown")
            .trim()
            .is_empty()
    }

    /// 正常在跑轮:回执的飞书身份三项(scope / chat_id / chat_type)全取 `PendingTurn` 里
    /// **轮创建时冻结**的值,reason 恒 ShutdownInterrupt;停机路径不查 .meta,故无 creator。
    #[test]
    fn enqueues_shutdown_receipt_with_frozen_identity() {
        let (agent, _tmp) = crate::test_support::agent_dir("cursor", "");
        let outbox = crate::runtime::outbox_store::OutboxStore::new(agent.dir.join("outbox"));
        let reg = Registry {
            agents: vec![agent.clone()],
        };

        enqueue_shutdown_notify(&outbox, &reg, &agent.name, &pending("frozen-scope-1234"));

        let due = outbox.list_due(now_unix_ms() + 1);
        if !expects_event(&agent) {
            assert!(due.is_empty(), "文案被配空就不该入队");
            return;
        }
        assert_eq!(due.len(), 1);
        assert_eq!(
            due[0].delivery_scope, "frozen-scope-1234",
            "必须用轮上冻结的 scope,不能按当前配置重算"
        );
        match &due[0].kind {
            summon_core::outbox::OutboxKind::NotifyChat {
                chat_id,
                chat_type,
                creator,
                event_operator,
                reason,
                ..
            } => {
                assert_eq!(chat_id, "oc_shutdown");
                assert_eq!(chat_type, "group");
                assert!(creator.is_none(), "停机路径不查 .meta creator");
                assert!(event_operator.is_none());
                assert_eq!(*reason, ExitReason::ShutdownInterrupt);
            }
            other => panic!("应入队 NotifyChat,得到 {other:?}"),
        }
        assert!(
            !crate::session::exit_mark::exists(&agent, "group", "oc_shutdown"),
            "停机中断不是会话结束,写标记会吞掉之后一次真实 offline kill 的回执"
        );
    }

    /// 空 `delivery_scope` = 这轮没冻结过飞书身份(旧/非 Chat checkpoint)。
    /// 从当前 agent 配置猜一个 scope,等于把回执发到换绑后的另一个 app —— 宁可不发。
    #[test]
    fn skips_turn_without_frozen_delivery_scope() {
        let (agent, _tmp) = crate::test_support::agent_dir("cursor", "");
        let outbox = crate::runtime::outbox_store::OutboxStore::new(agent.dir.join("outbox"));
        let reg = Registry {
            agents: vec![agent.clone()],
        };

        enqueue_shutdown_notify(&outbox, &reg, &agent.name, &pending(""));

        assert!(
            outbox.list_due(now_unix_ms() + 1).is_empty(),
            "身份不可证明时必须放弃这条回执"
        );
    }

    /// agent 已不在 registry(停机时被删/改名):连文案与凭据都 resolve 不出来,放弃。
    #[test]
    fn skips_turn_whose_agent_left_registry() {
        let (agent, _tmp) = crate::test_support::agent_dir("cursor", "");
        let outbox = crate::runtime::outbox_store::OutboxStore::new(agent.dir.join("outbox"));
        let reg = Registry { agents: vec![] };

        enqueue_shutdown_notify(&outbox, &reg, &agent.name, &pending("frozen-scope-1234"));

        assert!(outbox.list_due(now_unix_ms() + 1).is_empty());
    }
}

#[cfg(test)]
mod events_accept_tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    /// 轮询到条件成立或超时;失败直接返回 false 让调用方给出可读断言。
    fn wait_until(deadline: Duration, mut cond: impl FnMut() -> bool) -> bool {
        let start = std::time::Instant::now();
        while start.elapsed() < deadline {
            if cond() {
                return true;
            }
            thread::sleep(Duration::from_millis(5));
        }
        cond()
    }

    /// 阻塞 accept 必须仍然逐条交付连接;停机 flag 置位后到达的连接一律丢弃(与旧轮询版
    /// 「break 后不再 accept」语义等价),且循环自身退出。
    #[test]
    fn events_accept_loop_serves_then_drops_connections_after_shutdown() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("events.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let shutdown = AtomicBool::new(false);
        let served = AtomicUsize::new(0);

        thread::scope(|s| {
            let handle = s.spawn(|| {
                events_accept_loop(listener, &shutdown, &path, |_c| {
                    served.fetch_add(1, Ordering::SeqCst);
                });
            });

            let live = UnixStream::connect(&path).unwrap();
            assert!(
                wait_until(Duration::from_secs(5), || served.load(Ordering::SeqCst)
                    == 1),
                "停机前的连接必须被交付"
            );
            drop(live);

            shutdown.store(true, Ordering::SeqCst);
            // 停机踢醒:自连一次让阻塞的 accept 返回,循环复查 flag 后退出。
            let poke = UnixStream::connect(&path).unwrap();
            assert!(
                wait_until(Duration::from_secs(5), || handle.is_finished()),
                "flag 置位后 accept 循环必须退出"
            );
            drop(poke);
            assert_eq!(
                served.load(Ordering::SeqCst),
                1,
                "停机后到达的连接不得被交付"
            );
        });
    }

    /// socket 文件已被 unlink + listener 失效时,循环不能在 accept 错误上无限空转。
    #[test]
    fn events_accept_loop_exits_when_socket_file_gone() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("events.sock");
        let listener = UnixListener::bind(&path).unwrap();
        listener.set_nonblocking(true).unwrap(); // 无连接时 accept 立刻 WouldBlock,模拟持续错误
        std::fs::remove_file(&path).unwrap();
        let shutdown = AtomicBool::new(false);

        events_accept_loop(listener, &shutdown, &path, |_c| {
            panic!("不应有连接");
        });
    }
}

#[cfg(test)]
mod sidecar_log_tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn rotate_keeps_file_when_under_limit() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("sidecar.log");
        std::fs::write(&path, b"0123456789").unwrap();

        assert!(!rotate_if_oversized(&path, 100), "未超限不该轮转");
        assert!(!temp.path().join("sidecar.log.1").exists());
        assert_eq!(std::fs::read(&path).unwrap(), b"0123456789");
    }

    #[test]
    fn rotate_moves_oversized_file_to_single_previous_generation() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("sidecar.log");
        let previous = temp.path().join("sidecar.log.1");
        std::fs::write(&previous, "更早一代").unwrap();
        std::fs::write(&path, b"0123456789").unwrap();

        assert!(rotate_if_oversized(&path, 5));
        assert!(!path.exists(), "轮转后原文件应已挪走");
        assert_eq!(
            std::fs::read(&previous).unwrap(),
            b"0123456789",
            "只保留一代:.1 被新一代覆盖"
        );
    }

    #[test]
    fn rotate_tolerates_missing_file() {
        let temp = tempfile::tempdir().unwrap();
        assert!(!rotate_if_oversized(
            &temp.path().join("never-written.log"),
            0
        ));
    }

    #[test]
    fn open_sidecar_log_rotates_then_appends_to_fresh_file() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("run").join("sidecar.log");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "上一代输出").unwrap();

        let mut f = open_sidecar_log_at(&path, 1).unwrap();
        f.write_all(b"new line\n").unwrap();
        f.flush().unwrap();

        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "new line\n",
            "轮转后新句柄必须写进全新文件"
        );
        assert_eq!(
            std::fs::read_to_string(temp.path().join("run").join("sidecar.log.1")).unwrap(),
            "上一代输出"
        );
    }

    #[test]
    fn open_sidecar_log_creates_missing_run_dir_and_appends() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("absent").join("sidecar.log");

        let mut first = open_sidecar_log_at(&path, SIDECAR_LOG_MAX_BYTES).unwrap();
        first.write_all(b"a\n").unwrap();
        drop(first);
        let mut second = open_sidecar_log_at(&path, SIDECAR_LOG_MAX_BYTES).unwrap();
        second.write_all(b"b\n").unwrap();
        drop(second);

        let mut got = String::new();
        std::fs::File::open(&path)
            .unwrap()
            .read_to_string(&mut got)
            .unwrap();
        assert_eq!(got, "a\nb\n", "未超限时必须追加而非截断");
    }

    /// 子进程真的把 stdout/stderr 写进独立文件(而非继承 daemon 的 fd)。用 /bin/sh 而不是
    /// 真 sidecar:这里验的是 `Stdio::from(File)` 的接线,与飞书/tmux 无关,可无条件跑。
    #[test]
    fn spawned_child_writes_into_dedicated_log_file() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("sidecar.log");
        let f = open_sidecar_log_at(&path, SIDECAR_LOG_MAX_BYTES).unwrap();
        let err = f.try_clone().unwrap();

        let status = Command::new("/bin/sh")
            .arg("-c")
            .arg("echo out; echo err 1>&2")
            .stdin(Stdio::null())
            .stdout(Stdio::from(f))
            .stderr(Stdio::from(err))
            .status()
            .unwrap();

        assert!(status.success());
        let got = std::fs::read_to_string(&path).unwrap();
        assert!(got.contains("out") && got.contains("err"), "实际内容:{got}");
    }
}

#[cfg(test)]
mod inbox_boot_tests {
    use super::*;

    #[test]
    fn document_comment_callbacks_ack_after_persistence_and_replay_once() {
        use crate::runtime::card_action_wal::{CardActionWal, PersistOutcome};
        use std::io::{BufRead, BufReader, Write};
        use std::net::Shutdown;
        use std::time::Duration;

        let temp = tempfile::tempdir().unwrap();
        let wal_root = temp.path().join("card-wal");
        let wal = CardActionWal::new(&wal_root);
        let (dispatch_tx, dispatch_rx) = channel();
        let core = Arc::new(crate::runtime::core::Core::for_routes(HashMap::from([(
            "comment-app".to_string(),
            dispatch_tx,
        )])));
        let (mut client, server) = UnixStream::pair().unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut reader = BufReader::new(client.try_clone().unwrap());
        let worker = {
            let core = Arc::clone(&core);
            let wal = wal.clone();
            thread::spawn(move || handle_conn_with_wal(server, core, wal))
        };
        let read_ack = |reader: &mut BufReader<UnixStream>| {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            serde_json::from_str::<serde_json::Value>(&line).unwrap()
        };
        writeln!(
            client,
            "{}",
            serde_json::json!({
                "type":"hello", "app_id":"comment-app", "protocol":SIDECAR_PROTOCOL_VERSION
            })
        )
        .unwrap();
        assert_eq!(read_ack(&mut reader)["ok"], true);

        for action in ["doc_comment_approve", "doc_comment_dismiss"] {
            let event = serde_json::json!({
                "type":"card_action", "app_id":"comment-app", "chat_id":"oc_owner",
                "message_id":"om_approval", "operator_open_id":"ou_owner", "event_id":action,
                "value":{"action":action, "comment":"comment-task"}
            });
            writeln!(client, "{event}").unwrap();
            let ack = read_ack(&mut reader);
            assert_eq!(
                ack,
                serde_json::json!({
                    "type":"card_action_ack", "event_id":action, "ok":true, "status":"durable"
                })
            );
            let DispatchMsg::Card(card) = dispatch_rx.recv_timeout(Duration::from_secs(2)).unwrap()
            else {
                panic!("callback 必须路由为卡片动作")
            };
            assert!(card.wal_id.is_some());
            let cold = CardActionWal::new(&wal_root);
            assert!(cold
                .pending()
                .unwrap()
                .iter()
                .any(|record| record.card == card));
            writeln!(client, "{event}").unwrap();
            assert_eq!(read_ack(&mut reader), ack);
            assert!(dispatch_rx.try_recv().is_err(), "重复投递只能 ACK");
        }
        client.shutdown(Shutdown::Both).unwrap();
        worker.join().unwrap();
        let cold = CardActionWal::new(&wal_root);
        let pending = cold.pending().unwrap();
        assert_eq!(pending.len(), 2);
        assert_eq!(cold.preflight().unwrap().pending, 2);
        replay_card_action_records(&core, pending.clone());
        for record in pending {
            let DispatchMsg::Card(replayed) =
                dispatch_rx.recv_timeout(Duration::from_secs(2)).unwrap()
            else {
                panic!("冷启必须重放卡片动作")
            };
            assert!(replayed.wal_replayed);
            assert_eq!(replayed.wal_id.as_deref(), Some(record.id.as_str()));
            assert_eq!(replayed.event_id, record.card.event_id);
            cold.ack(&record.id).unwrap();
            let mut duplicate = record.card;
            duplicate.wal_id = None;
            assert!(matches!(
                cold.persist("comment-app", &duplicate).unwrap(),
                PersistOutcome::Consumed
            ));
        }
        assert!(cold.pending().unwrap().is_empty());
        assert_eq!(cold.inspect_health().unwrap().consumed, 2);
    }

    fn inbound(message_id: &str, text: &str) -> Inbound {
        Inbound {
            anchor_message_id: None,
            chat_id: "oc_boot".into(),
            chat_type: Some("p2p".into()),
            text: text.into(),
            sender: "ou_owner".into(),
            sender_union_id: "on_owner".into(),
            sender_type: String::new(),
            message_id: message_id.into(),
            parent_id: None,
            thread_id: None,
            root_id: None,
            mentions: Vec::new(),
            resources: Vec::new(),
            create_time_ms: 100,
            replayed: false,
            reattach: false,
        }
    }

    #[test]
    fn inbox_boot_preflight_reports_non_sensitive_health_counts() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("inbox");
        let wal = crate::runtime::inbox_wal::InboxWal::new(root.clone());
        wal.append("agent-a", "scope-a", &inbound("om-started", "正文"))
            .unwrap();
        assert_eq!(
            wal.mark_started("agent-a", "scope-a", "om-started")
                .unwrap(),
            crate::runtime::inbox_wal::MarkStartedOutcome::Marked
        );
        wal.append_scheduled(
            "agent-a",
            "scope-a",
            "oc_boot",
            "p2p",
            "定时正文",
            summon_core::ScheduleOutput::Progress,
            summon_core::ScheduleContext::Resume,
            "sched-boot-1",
            200,
        )
        .unwrap();

        let health = preflight_inbox_wal(&root).unwrap();
        assert_eq!(health.pending_records, 2);
        assert_eq!(health.started_records, 1);
        assert_eq!(health.scheduled_records, 1);
        assert_eq!(health.active_recovery_batches, 0);
        assert_eq!(health.awaiting_recovery_items, 0);
        assert_eq!(health.claimed_recovery_items, 0);
        assert_eq!(health.repaired_recovery_claims, 0);
    }

    #[test]
    fn inbox_boot_preflight_propagates_corrupt_wal_without_deleting_it() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("inbox");
        let corrupt = root.join("pending").join("om-corrupt.json");
        std::fs::create_dir_all(corrupt.parent().unwrap()).unwrap();
        std::fs::write(&corrupt, b"{").unwrap();

        let error = preflight_inbox_wal(&root).unwrap_err();
        assert!(
            format!("{error:#}").contains("首次严格扫描"),
            "冷启必须保留完整错误链:{error:#}"
        );
        assert!(corrupt.exists(), "预检失败不得擅自删除损坏 WAL");
    }

    #[test]
    fn inbox_boot_preflight_repairs_half_committed_claim_idempotently() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("inbox");
        let wal = crate::runtime::inbox_wal::InboxWal::new(root.clone());
        wal.append("agent-a", "scope-a", &inbound("om-original", "执行一次"))
            .unwrap();
        let source = wal.scan_pending_strict().unwrap().pop().unwrap();
        let batch = wal
            .create_recovery_batch(
                "scope-a",
                "agent-a",
                "oc_boot",
                "p2p",
                None,
                vec![source],
                100,
            )
            .unwrap();
        wal.finalize_recovery_sources(&batch).unwrap();
        wal.record_recovery_card_delivery(&batch.id, &batch.outbox_id, "om-card")
            .unwrap();
        assert!(matches!(
            wal.claim_recovery(
                &batch.id,
                "scope-a",
                "agent-a",
                "oc_boot",
                "om-card",
                "om-original",
            )
            .unwrap(),
            crate::runtime::inbox_wal::RecoveryClaimOutcome::Claimed(_)
        ));

        // 精确还原 crash window：replay pending 已 durable，batch manifest 尚停在 Awaiting。
        let mut half_committed = wal.load_recovery_batch(&batch.id).unwrap().unwrap();
        half_committed.items[0].state = crate::runtime::inbox_wal::RecoveryItemState::Awaiting;
        let active_path = root
            .join("recovery")
            .join("active")
            .join(format!("{}.json", batch.id));
        crate::runtime::fsio::atomic_write(
            &active_path,
            &serde_json::to_vec_pretty(&half_committed).unwrap(),
        )
        .unwrap();

        let health = preflight_inbox_wal(&root).unwrap();
        assert_eq!(health.repaired_recovery_claims, 1);
        assert_eq!(health.pending_records, 1);
        assert_eq!(health.active_recovery_batches, 1);
        assert_eq!(health.awaiting_recovery_items, 0);
        assert_eq!(health.claimed_recovery_items, 1);

        let second = preflight_inbox_wal(&root).unwrap();
        assert_eq!(second.repaired_recovery_claims, 0, "重复预检必须幂等");
        assert_eq!(second.claimed_recovery_items, 1);
    }

    #[test]
    fn inbox_boot_preflight_runs_before_publishing_or_mutating_daemon_state() {
        let source = include_str!("boot.rs");
        let preflight = source
            .find("    let inbox_health =")
            .expect("run() 必须接入 Inbox WAL 预检");
        for marker in [
            "    let _ = std::fs::remove_file(ready_file());",
            "    let _ = std::fs::remove_file(daemon_version_file());",
            "    let _ = std::fs::remove_file(clean_shutdown_marker());",
            "    let _ = Command::new(summon_core::tools::resolve(",
            "    write_identity_file(&pid_file(), daemon_pid)?;",
        ] {
            let mutation = source
                .find(marker)
                .unwrap_or_else(|| panic!("缺少启动标记:{marker}"));
            assert!(
                preflight < mutation,
                "Inbox WAL 预检必须早于 daemon 对外状态变化:{marker}"
            );
        }
    }

    #[test]
    fn card_action_boot_preflight_rejects_corrupt_durable_callback() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("card-inbox");
        let wal = crate::runtime::card_action_wal::CardActionWal::new(&root);
        wal.persist(
            "app",
            &CardActionMsg {
                chat_id: "oc_private".into(),
                message_id: "om_card".into(),
                operator_open_id: "ou_owner".into(),
                event_id: "event-corrupt".into(),
                value: serde_json::json!({"action":"bridge_stop"}),
                wal_id: None,
                wal_replayed: false,
            },
        )
        .unwrap();
        let pending = std::fs::read_dir(root.join("pending"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        std::fs::write(&pending, b"{").unwrap();

        assert!(wal.preflight().is_err());
        assert!(pending.exists(), "损坏 callback WAL 必须保留供修复");
    }

    #[test]
    fn remote_restore_precedes_inbox_recovery_dispatcher_and_card_replay() {
        let source = include_str!("boot.rs");
        let remote = source
            .find("\n    crate::remote::runtime::boot(&drain_agents, &shutdown);\n")
            .expect("run() 必须接入遥控启动恢复");
        let recover = source
            .find("\n    boot_recover_inbox(\n")
            .expect("run() 必须有入站恢复");
        let dispatcher = source
            .find("\n    for p in pending {\n")
            .expect("run() 必须有 dispatcher 启动点");
        let replay = source
            .find("\n    replay_card_action_records(&core, replay_card_actions);\n")
            .expect("run() 必须有卡片回放");
        assert!(
            remote < recover && recover < dispatcher && dispatcher < replay,
            "遥控启动复验必须先于一切可能拉起遥控 worker 的入口"
        );
    }

    #[test]
    fn card_action_preflight_and_snapshot_precede_dispatcher_start() {
        let source = include_str!("boot.rs");
        let preflight = source
            .find("    let card_health =")
            .expect("run() 必须接入 CardActionWal 预检");
        let snapshot = source
            .find("    let replay_card_actions = card_action_wal")
            .expect("run() 必须在起线程前冻结 callback replay");
        let dispatcher = source
            .find("    for p in pending {\n")
            .expect("run() 必须有 dispatcher 启动点");
        let ready = source
            .find("    write_identity_file(&ready_file(), daemon_pid)?;")
            .expect("run() 必须有 ready 写点");
        let version = source
            .find("    write_daemon_version_identity(&daemon_version_file(), daemon_pid, crate::version::VERSION)?;")
            .expect("run() 必须在 ready 前发布同代版本身份");
        assert!(
            preflight < snapshot
                && snapshot < dispatcher
                && dispatcher < version
                && version < ready
        );
    }
}

#[cfg(test)]
mod empty_inbound_tests {
    use super::*;

    #[test]
    fn warns_only_when_text_and_resources_are_both_empty() {
        // 事故形状:interactive 卡片解析不出正文,又没有附件 → 这条消息对 agent 等于不存在。
        let warning = empty_inbound_warning(
            "",
            0,
            "om_card_1",
            "oc_0000000000",
            "ou_devbox",
            Some("group"),
        )
        .expect("空正文 + 无附件必须 warn");
        for needle in ["om_card_1", "oc_0000000000", "ou_devbox"] {
            assert!(warning.contains(needle), "warn 必须带 {needle}: {warning}");
        }
        assert!(
            warning.contains("正文为空"),
            "warn 文案要一眼认得出是正文没解析出来: {warning}"
        );
    }

    #[test]
    fn stays_silent_for_normal_messages() {
        assert!(
            empty_inbound_warning("你好", 0, "om_1", "oc_1", "ou_1", None).is_none(),
            "有正文就不该 warn"
        );
        assert!(
            empty_inbound_warning("", 1, "om_2", "oc_1", "ou_1", None).is_none(),
            "纯图片/语音本就无正文,warn 它们只是噪音"
        );
    }

    #[test]
    fn treats_whitespace_only_text_as_empty() {
        assert!(
            empty_inbound_warning(" \n\t ", 0, "om_3", "oc_1", "ou_1", None).is_some(),
            "只有空白的正文等同于空"
        );
    }
}

#[cfg(test)]
mod retirement_tests {
    use super::*;

    #[test]
    fn sidecar_quiesce_waits_for_reap_and_excludes_configuration_spawn() {
        let control = Arc::new(SidecarControl::new());
        let mut child = Command::new("/bin/sleep").arg("30").spawn().unwrap();
        control.set_pid(Some(child.id()));
        let observer = control.clone();
        let waiter = thread::spawn(move || {
            let status = child.wait().unwrap();
            assert!(!status.success());
            // supervisor 可以先清 pid，不需要配置锁；下一次 spawn 才取该锁。
            observer.set_pid(None);
        });
        control.quiesce().unwrap();
        waiter.join().unwrap();
        assert!(control.pid.lock().unwrap().is_none());
    }

    #[test]
    fn retired_agent_is_excluded_from_sidecar_until_new_generation() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("retired");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("agent.yaml"),
            "name: retired\nfeishu:\n  app_id_env: RETIRED_APP_ID\n",
        )
        .unwrap();
        let (reg, warnings) = Registry::load_tolerant(root.path()).unwrap();
        assert!(warnings.is_empty());
        assert_eq!(build_sidecar_apps(&reg, "feishu", true).len(), 1);
        crate::runtime::agent_retirement::retire(&dir);
        assert!(build_sidecar_apps(&reg, "feishu", true).is_empty());
        crate::runtime::agent_retirement::activate(&dir);
        assert_eq!(build_sidecar_apps(&reg, "feishu", true).len(), 1);
    }
}
