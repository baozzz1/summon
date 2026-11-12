//! lark-cli shell-out——sentinel 的 owner 用户身份飞书通道。
//!
//! why shell-out 而非自管 OAuth:复用 lark-cli 成熟的 device-flow/续期/scope 增量链路,
//! summon 绝不持有 owner 用户凭据(红线④;shell-out 只发生在 daemon 进程,agent 子进程
//! 不可见)。已知坑:device-flow 补授会覆盖 token scope(workflow playbook 2026-08-07)。
//!
//! 所有 JSON 解析拆成纯函数吃 canned fixture 单测;进程/网络失败一律 anyhow 上抛,
//! poller 收到错误就放弃本 tick(fail-closed:拿不到事实就不动作)。

use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::process::Command;

/// owner 用户身份的 lark-cli 调用器。`profile=None` 用 lark-cli 默认 profile；
/// `executable=None` 才从 daemon PATH 解析裸命令。
pub struct LarkUser {
    profile: Option<String>,
    executable: std::path::PathBuf,
    timeout: Option<std::time::Duration>,
}

/// 会话简项(chats 枚举)。
#[derive(Debug, Clone)]
pub struct ChatBrief {
    pub chat_id: String,
    pub name: String,
    /// `p2p` / `group` / `topic`。
    pub chat_mode: String,
}

/// 消息简项(tail / mget / history 共用)。
#[derive(Debug, Clone)]
pub struct MsgBrief {
    pub message_id: String,
    pub chat_id: String,
    /// sender.id 原样(用户为 open_id,bot 为 app_id)。
    pub sender_id: String,
    /// `user` / `app` / `bot`(飞书两种写法都见过,判 bot 时两个都认)。
    pub sender_type: String,
    /// 发送者显示名(lark-cli 富化;可能为空)。
    pub sender_name: String,
    /// 0 = 解析失败/未知 → gate::fresh 会拒绝,fail-closed。
    pub create_time_ms: i64,
    pub text: String,
    pub msg_type: String,
    pub chat_name: String,
}

/// lark-cli 报的**授权缺口**。单独成一个错误类型,是为了让调用方能把它和网络/参数错误区分开:
/// 前者有明确的、可执行的补救动作(让 owner 授这几个 scope),后者只能记日志。
///
/// 2026-08-07 现场教训:缺 scope 时 lark-cli 其实在响应里写清了 `missing_scopes`,但那段信息
/// 一路被降级(先是只读 stdout 丢掉、再是和普通失败混成一句话),最终 owner 只能靠人猜 scope 名。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LarkAuthError {
    pub missing_scopes: Vec<String>,
}

impl std::fmt::Display for LarkAuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}",
            summon_i18n::format!("rb18deda8ff", self.missing_scopes.join(" "))
        )
    }
}

impl std::error::Error for LarkAuthError {}

/// lark-cli 里登录的用户(owner 本人)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LarkOwner {
    pub open_id: String,
    pub name: Option<String>,
}

/// sentinel 用的 lark-cli profile 没有登录用户身份(只有 bot)。和缺 scope 一样是「有明确补救
/// 动作」的授权缺口:换一个有 owner 用户身份的 profile,或给这个 profile 登录。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LarkUserLoginMissing {
    /// `None` = 没有指定,用的是 lark-cli 当前默认 profile。
    pub profile: Option<String>,
}

impl std::fmt::Display for LarkUserLoginMissing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.profile {
            Some(profile) => write!(f, "{}", summon_i18n::format!("rcb99538b2c")),
            None => write!(f, "{}", summon_i18n::format!("r768fde83a1")),
        }
    }
}

impl std::error::Error for LarkUserLoginMissing {}

/// 从 lark-cli 错误 envelope 里取缺失的 scope 列表。认不出返回 None(交给通用失败路径)。
///
/// 两处都认:`error.missing_scopes` 数组是首选;只有 `subtype=="missing_scope"` 而没给数组时,
/// 退回空列表也仍然算授权错误 —— 「知道是权限问题但不知道缺哪个」比「当成未知失败」有用。
fn missing_scopes(v: &Value) -> Option<Vec<String>> {
    let err = v.get("error")?;
    let is_auth = err.get("subtype").and_then(Value::as_str) == Some("missing_scope");
    let list: Vec<String> = err
        .get("missing_scopes")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    if is_auth || !list.is_empty() {
        Some(list)
    } else {
        None
    }
}

impl LarkUser {
    pub fn new(profile: Option<String>, executable: Option<String>) -> Self {
        Self {
            profile,
            timeout: None,
            executable: executable
                .filter(|p| !p.trim().is_empty())
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| std::path::PathBuf::from("lark-cli")),
        }
    }

    pub(crate) fn with_timeout(mut self, timeout: std::time::Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// 跑一条 lark-cli 命令并取回 envelope 校验过的 `data`。
    fn run(&self, args: &[&str]) -> Result<Value> {
        self.run_in(None, args)
    }

    /// 同 [`Self::run`],但可指定子进程工作目录。**只有下载类命令需要**:lark-cli 的
    /// `--output` 只收相对路径(拒绝 `..`),落到哪儿完全由 cwd 决定。
    fn run_in(&self, dir: Option<&std::path::Path>, args: &[&str]) -> Result<Value> {
        self.run_envelope_in(dir, args).map(|v| v["data"].clone())
    }

    fn run_envelope_in(&self, dir: Option<&std::path::Path>, args: &[&str]) -> Result<Value> {
        let mut cmd = Command::new(&self.executable);
        if let Some(d) = dir {
            cmd.current_dir(d);
        }
        if let Some(p) = &self.profile {
            cmd.arg("--profile").arg(p);
        }
        cmd.args(args);
        let out = match self.timeout {
            Some(timeout) => bounded_output(&mut cmd, timeout),
            None => cmd.output().map_err(Into::into),
        }
        .context(summon_i18n::text!("r16b31736c4"))?;
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        // **stdout 空时必须回落 stderr**。2026-08-07 现场:缺 `im:chat:read` 时 lark-cli 把结构化
        // 错误(含 `missing_scopes`)写到了 stderr、stdout 全空,而这里只读 stdout —— 于是一个信息
        // 完整的授权错误被降级成「输出无 JSON」,owner 只能靠猜(我口头猜了个不存在的 scope 名,
        // 害他白授权一次)。诊断信息在传出去之前就被自己丢掉,是最没必要的一类失败。
        let json = extract_json(&stdout)
            .or_else(|| extract_json(&stderr))
            .with_context(|| summon_i18n::format!("re5962c4377"))?;
        let v: Value =
            serde_json::from_str(json).with_context(|| summon_i18n::format!("r4c2030ec99"))?;
        if v["ok"].as_bool() != Some(true) {
            // 授权类错误单独成型:调用方据此发授权卡片,而不是把它和网络/参数错误混成一句话。
            if let Some(missing) = missing_scopes(&v) {
                return Err(anyhow::Error::new(LarkAuthError {
                    missing_scopes: missing,
                }));
            }
            bail!(summon_i18n::format!(
                "r162ad12c15",
                args,
                serde_json::to_string(&v["error"]).unwrap_or_default()
            ));
        }
        anyhow::ensure!(out.status.success(), summon_i18n::format!("rf9cb0eab21"));
        Ok(v)
    }

    pub(crate) fn verified_requester(&self, union_id: &str) -> Result<String> {
        anyhow::ensure!(!union_id.is_empty(), summon_i18n::format!("r507bbc62ad"));
        let data = self.run(&[
            "api",
            "GET",
            "/open-apis/authen/v1/user_info",
            "--as",
            "user",
        ])?;
        anyhow::ensure!(
            data["union_id"].as_str() == Some(union_id),
            summon_i18n::format!("reb20cf06a4")
        );
        data["open_id"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .context(summon_i18n::text!("rf03b13d86e"))
    }

    pub(crate) fn search_exact_groups(
        &self,
        name: &str,
    ) -> Result<Vec<summon_core::session::group_target::GroupCandidate>> {
        let envelope = self.run_envelope_in(
            None,
            &[
                "im",
                "+chat-search",
                "--as",
                "user",
                "--query",
                name,
                "--disable-search-by-user",
                "--search-types",
                "private,public_joined,external",
                "--page-all",
                "--page-limit",
                "10",
                "--page-size",
                "100",
            ],
        )?;
        parse_group_search(&envelope, name)
    }

    pub(crate) fn prove_user_member(&self, chat: &str, open_id: &str) -> Result<()> {
        let data = self.run(&[
            "im",
            "+chat-members-list",
            "--as",
            "user",
            "--chat-id",
            chat,
            "--page-all",
        ])?;
        anyhow::ensure!(
            parse_user_members(&data)
                .iter()
                .any(|(id, _)| id == open_id),
            summon_i18n::format!("r71e3d19e5b")
        );
        Ok(())
    }

    /// 按活跃度取 top-K 会话(群 + 单聊)。单页上限 100,top_k 超过也只取一页——
    /// 长尾由搜索通道兜底,不值得为它翻页。
    pub fn active_chats(&self, top_k: usize, exclude_muted: bool) -> Result<Vec<ChatBrief>> {
        let args = active_chats_args(top_k, exclude_muted);
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let data = self.run(&refs)?;
        Ok(parse_chats(&data))
    }

    /// 取某会话最新一页消息(desc),供调用方按水位过滤。
    pub fn latest_messages(&self, chat_id: &str, page_size: usize) -> Result<Vec<MsgBrief>> {
        let n = page_size.clamp(1, 50).to_string();
        let data = self.run(&[
            "im",
            "+chat-messages-list",
            "--as",
            "user",
            "--chat-id",
            chat_id,
            "--order",
            "desc",
            "--page-size",
            &n,
            "--no-reactions",
        ])?;
        Ok(parse_msgs(&data, chat_id))
    }

    /// 按 chat_id 取真实会话类型。搜索结果不带 `chat_mode`，而 `/summon` 在 p2p/group 的
    /// 物理动作不同；只有真实命中且 tail 未见该会话时才多付这一次调用，绝不靠动作词猜。
    pub fn chat(&self, chat_id: &str) -> Result<ChatBrief> {
        let data = self.run(&["im", "chats", "get", "--as", "user", "--chat-id", chat_id])?;
        parse_chat(&data, chat_id).context(summon_i18n::text!("r972a2ef816"))
    }

    /// 会话最近 limit 条历史,升序(喂 primer 用)。
    pub fn history_asc(&self, chat_id: &str, limit: usize) -> Result<Vec<MsgBrief>> {
        let mut msgs = self.latest_messages(chat_id, limit)?;
        msgs.reverse();
        Ok(msgs)
    }

    /// 把一条消息里的图片下到 `dir/file_name`,返回落盘绝对路径。
    ///
    /// **必须 owner user 身份**:sentinel 的全部前提就是「bot 还没进那个会话」,拿 bot token
    /// 去下这张图必然拿不到 —— 这条链路上只有 owner 看得见它。
    ///
    /// `--output` 只吃相对路径(lark-cli 拒绝 `..`),所以落点靠子进程 cwd 决定;
    /// 真正的落盘位置以响应里的 `saved_path` 为准(扩展名可能被 Content-Type 改写)。
    pub fn download_image(
        &self,
        message_id: &str,
        file_key: &str,
        dir: &std::path::Path,
        file_name: &str,
    ) -> Result<std::path::PathBuf> {
        let data = self.run_in(
            Some(dir),
            &[
                "im",
                "+messages-resources-download",
                "--as",
                "user",
                "--message-id",
                message_id,
                "--file-key",
                file_key,
                "--type",
                "image",
                "--output",
                file_name,
            ],
        )?;
        let saved = data["saved_path"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| dir.join(file_name));
        Ok(saved)
    }

    /// 套件搜索(真全文)按动作词召回,返回 message_id 列表。
    /// 不传 start_time:时间窗参数格式未真机验证过,时效判定统一交给本地闸
    /// (mget 后 create_time + gate::fresh),宁多召回不冒静默漏召的险。
    pub fn search_ids(&self, query: &str, from_open_id: &str) -> Result<Vec<String>> {
        let body = serde_json::json!({
            "query": query,
            "from_ids": [from_open_id],
            "page_size": 20,
        })
        .to_string();
        let data = self.run(&[
            "api",
            "POST",
            "/open-apis/search/v2/message",
            "--as",
            "user",
            "--data",
            &body,
        ])?;
        Ok(parse_search_ids(&data))
    }

    /// 批量取消息详情(≤50/批,超出调用方分批)。
    pub fn mget(&self, ids: &[String]) -> Result<Vec<MsgBrief>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let joined = ids.join(",");
        let data = self.run(&[
            "im",
            "+messages-mget",
            "--as",
            "user",
            "--message-ids",
            &joined,
            "--no-reactions",
        ])?;
        Ok(parse_msgs(&data, ""))
    }

    /// 会话里的 bot app_id 列表(「bot 已在群则跳过」判定)。
    pub fn bots_in_chat(&self, chat_id: &str) -> Result<Vec<String>> {
        let data = self.run(&[
            "im",
            "+chat-members-list",
            "--as",
            "user",
            "--chat-id",
            chat_id,
            "--member-types",
            "bot",
            "--page-all",
            "--page-limit",
            "0",
        ])?;
        complete_bot_members(&data)
    }

    /// 以 owner 身份把 bot 拉进群。受群设置「仅群主/管理员可拉人」限制,失败上抛由动作链告知 owner。
    pub fn add_bot(&self, chat_id: &str, bot_app_id: &str) -> Result<()> {
        let body = serde_json::json!({ "id_list": [bot_app_id] }).to_string();
        let data = self.run(&[
            "im",
            "chat.members",
            "create",
            "--as",
            "user",
            "--chat-id",
            chat_id,
            "--member-id-type",
            "app_id",
            "--data",
            &body,
        ])?;
        if let Some(reason) = member_add_failure(&data) {
            bail!(summon_i18n::format!("r9f045d81e1"));
        }
        Ok(())
    }

    /// 以 owner 身份把真人拉进群(open_id 为 **lark-cli 命名空间**)。三方建群第二步用。
    pub fn add_user(&self, chat_id: &str, user_open_id: &str) -> Result<()> {
        let body = serde_json::json!({ "id_list": [user_open_id] }).to_string();
        let data = self.run(&[
            "im",
            "chat.members",
            "create",
            "--as",
            "user",
            "--chat-id",
            chat_id,
            "--member-id-type",
            "open_id",
            "--data",
            &body,
        ])?;
        if let Some(reason) = member_add_failure(&data) {
            bail!(summon_i18n::format!("rf98e371054"));
        }
        Ok(())
    }

    /// 会话里的真人成员 (open_id, name) 列表(lark-cli 命名空间;p2p 找对方用)。
    pub fn users_in_chat(&self, chat_id: &str) -> Result<Vec<(String, String)>> {
        let data = self.run(&[
            "im",
            "+chat-members-list",
            "--as",
            "user",
            "--chat-id",
            chat_id,
        ])?;
        Ok(parse_user_members(&data))
    }

    /// owner 本人在 lark-cli app 命名空间下的 open_id(触发 sender 过滤用)。
    /// 取不到 → Err(fail-closed,poller 本 tick 不扫)。
    pub fn owner_open_id(&self) -> Result<String> {
        self.owner().map(|owner| owner.open_id)
    }

    /// owner 本人在 lark-cli app 命名空间下的身份。从 `auth status` 取,零配置;没登录用户身份
    /// → [`LarkUserLoginMissing`]。
    pub fn owner(&self) -> Result<LarkOwner> {
        let out = self
            .auth_status()
            .output()
            .context(summon_i18n::text!("rd4866e3b1d"))?;
        self.parse_owner(&out.stdout)
    }

    /// 同 [`Self::owner`],但最多等 `timeout`:控制台在 HTTP 处理线程里调它,lark-cli 卡住不能
    /// 一直占着连接。超时杀掉子进程。
    pub fn owner_within(&self, timeout: std::time::Duration) -> Result<LarkOwner> {
        use std::io::Read as _;
        let mut child = self
            .auth_status()
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .context(summon_i18n::text!("rd4866e3b1d"))?;
        let mut stdout = child
            .stdout
            .take()
            .context(summon_i18n::text!("rf4a449f036"))?;
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = stdout.read_to_end(&mut buf);
            // 接收端超时后已放弃,发送失败可忽略。
            let _ = tx.send(buf);
        });
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if child.try_wait()?.is_some() {
                break;
            }
            if std::time::Instant::now() >= deadline {
                // 子进程已自己退出时 kill 报错,正是想要的结果,可忽略。
                let _ = child.kill();
                let _ = child.wait();
                bail!(summon_i18n::format!("rc223571fe0", timeout.as_secs()));
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        // 孙进程握着管道时子进程退出不等于输出读完,读也受同一个时限约束;留一点余量给刚好
        // 卡着时限退出的那一次把输出读完。
        let wait = deadline
            .saturating_duration_since(std::time::Instant::now())
            .max(std::time::Duration::from_millis(200));
        let out = rx
            .recv_timeout(wait)
            .map_err(|_| anyhow::anyhow!(summon_i18n::format!("rc223571fe0", timeout.as_secs())))?;
        self.parse_owner(&out)
    }

    fn auth_status(&self) -> Command {
        let mut cmd = Command::new(&self.executable);
        if let Some(p) = &self.profile {
            cmd.arg("--profile").arg(p);
        }
        cmd.args(["auth", "status"]);
        cmd
    }

    fn parse_owner(&self, stdout: &[u8]) -> Result<LarkOwner> {
        let stdout = String::from_utf8_lossy(stdout);
        let json = extract_json(&stdout).context(summon_i18n::text!("rf46aee30a1"))?;
        let v: Value = serde_json::from_str(json).context(summon_i18n::text!("r5a86200a58"))?;
        let user = &v["identities"]["user"];
        let open_id = user["openId"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                anyhow::Error::new(LarkUserLoginMissing {
                    profile: self.profile.clone(),
                })
            })?;
        Ok(LarkOwner {
            open_id: open_id.to_string(),
            name: user["userName"]
                .as_str()
                .filter(|s| !s.is_empty())
                .map(str::to_string),
        })
    }
}

fn bounded_output(
    command: &mut Command,
    timeout: std::time::Duration,
) -> Result<std::process::Output> {
    use std::io::{Read, Seek};
    use std::process::Stdio;
    const LIMIT: u64 = 16 * 1024 * 1024;
    let mut stdout = tempfile::tempfile()?;
    let mut stderr = tempfile::tempfile()?;
    command
        .stdin(Stdio::null())
        .stdout(stdout.try_clone()?)
        .stderr(stderr.try_clone()?);
    let mut child = crate::runtime::process::retry_text_file_busy(|| command.spawn())?;
    let deadline = std::time::Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if std::time::Instant::now() >= deadline
            || stdout.metadata()?.len() > LIMIT
            || stderr.metadata()?.len() > LIMIT
        {
            let _ = child.kill();
            let _ = child.wait();
            bail!(summon_i18n::format!("rb86d56f68c"));
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    let read = |file: &mut std::fs::File| -> Result<Vec<u8>> {
        anyhow::ensure!(
            file.metadata()?.len() <= LIMIT,
            summon_i18n::format!("rde7813328b")
        );
        file.rewind()?;
        let mut bytes = Vec::new();
        file.take(LIMIT).read_to_end(&mut bytes)?;
        Ok(bytes)
    };
    Ok(std::process::Output {
        status,
        stdout: read(&mut stdout)?,
        stderr: read(&mut stderr)?,
    })
}

pub(crate) fn parse_group_search(
    envelope: &Value,
    name: &str,
) -> Result<Vec<summon_core::session::group_target::GroupCandidate>> {
    use summon_core::session::group_target::{exact_matches, GroupCandidate};
    let data = &envelope["data"];
    let complete = envelope["meta"]["pagination"]["complete"]
        .as_bool()
        .or_else(|| data["has_more"].as_bool().map(|more| !more));
    anyhow::ensure!(complete == Some(true), summon_i18n::format!("red3bb9242c"));
    let chats = match data.get("chats") {
        Some(Value::Array(chats)) => chats.as_slice(),
        Some(Value::Null) => &[],
        _ => anyhow::bail!(summon_i18n::format!("ra0e5959a9a")),
    };
    let mut groups = Vec::new();
    for c in chats {
        if c["chat_status"].as_str() != Some("normal") {
            continue;
        }
        let id = c["chat_id"]
            .as_str()
            .context(summon_i18n::text!("r7eaecccdcc"))?;
        let title = c["name"]
            .as_str()
            .context(summon_i18n::text!("r2321035511"))?;
        groups.push(GroupCandidate {
            chat_id: id.into(),
            name: title.into(),
            description: c["description"].as_str().unwrap_or_default().into(),
            member_count: c["member_count"]
                .as_u64()
                .or_else(|| c["member_count"].as_str().and_then(|n| n.parse().ok())),
        });
    }
    Ok(exact_matches(name, groups))
}

#[cfg(test)]
mod group_search_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn group_search_accepts_complete_null_and_filters_dissolved_chats() {
        let mut response =
            json!({"data":{"chats":null,"has_more":false},"meta":{"pagination":{"complete":true}}});
        assert!(parse_group_search(&response, "Target").unwrap().is_empty());
        response["data"].as_object_mut().unwrap().remove("chats");
        assert!(parse_group_search(&response, "Target").is_err());
        response["data"]["chats"] = json!({});
        assert!(parse_group_search(&response, "Target").is_err());
        response["data"]["chats"] = json!([
            {"chat_id":"oc_dead","name":"Target","chat_status":"dissolved"},
            {"chat_id":"oc_live","name":"Target","chat_status":"normal"}
        ]);
        let found = parse_group_search(&response, "Target").unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].chat_id, "oc_live");
    }

    #[test]
    fn bot_members_require_complete_untruncated_results() {
        let mut data = json!({"bots":[{"app_id":"app_target"}],"has_more":false});
        assert_eq!(complete_bot_members(&data).unwrap(), vec!["app_target"]);
        data["has_more"] = json!(true);
        assert!(complete_bot_members(&data).is_err());
        data["has_more"] = json!(false);
        data["truncations"] = json!([{"type":"bot"}]);
        assert!(complete_bot_members(&data).is_err());
        data["truncations"] = json!([]);
        assert!(complete_bot_members(&data).is_ok());
        data.as_object_mut().unwrap().remove("has_more");
        assert!(complete_bot_members(&data).is_err());
    }

    #[test]
    fn bot_members_cli_requests_all_bot_pages() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("lark");
        crate::test_support::write_test_executable(
            &exe,
            r#"#!/bin/sh
[ "$*" = 'im +chat-members-list --as user --chat-id oc_target --member-types bot --page-all --page-limit 0' ] || exit 1
printf '%s' '{"ok":true,"data":{"bots":[{"app_id":"app_later_page"}],"has_more":false}}'
"#,
        );
        let lark = LarkUser::new(None, Some(exe.to_string_lossy().into_owned()));
        assert_eq!(
            lark.bots_in_chat("oc_target").unwrap(),
            vec!["app_later_page"]
        );
    }

    #[test]
    fn group_cli_output_has_a_deadline_and_preserves_exit_status() {
        let mut slow = Command::new("/bin/sh");
        slow.args(["-c", "exec sleep 5"]);
        let start = std::time::Instant::now();
        assert!(bounded_output(&mut slow, std::time::Duration::from_millis(50)).is_err());
        assert!(start.elapsed() < std::time::Duration::from_secs(2));
        let mut failure = Command::new("/bin/sh");
        failure.args(["-c", "printf '{\"ok\":true}'; printf 'failed' >&2; exit 7"]);
        let output = bounded_output(&mut failure, std::time::Duration::from_secs(2)).unwrap();
        assert_eq!(output.status.code(), Some(7));
        assert_eq!(output.stdout, br#"{"ok":true}"#);
        assert_eq!(output.stderr, b"failed");
    }

    #[test]
    fn complete_group_search_preserves_ambiguity_and_rejects_partial_pages() {
        let mut response = json!({"data":{"chats":[
            {"chat_id":"oc_one","name":"Target","description":"A","chat_status":"normal"},
            {"chat_id":"oc_two","name":"Target","member_count":"5","chat_status":"normal"},
            {"chat_id":"oc_three","name":"Target Extended","chat_status":"normal"}
        ]},"meta":{"pagination":{"complete":true}}});
        let matches = parse_group_search(&response, "Target").unwrap();
        assert_eq!(matches.len(), 2);
        assert_eq!(matches[1].member_count, Some(5));
        response["meta"]["pagination"]["complete"] = json!(false);
        assert!(parse_group_search(&response, "Target").is_err());
        response.as_object_mut().unwrap().remove("meta");
        assert!(parse_group_search(&response, "Target").is_err());
        response["data"]["has_more"] = json!(false);
        assert_eq!(parse_group_search(&response, "Target").unwrap().len(), 2);
        response["data"]["chats"][0]
            .as_object_mut()
            .unwrap()
            .remove("chat_id");
        assert!(parse_group_search(&response, "Target").is_err());
    }
}

fn complete_bot_members(data: &Value) -> Result<Vec<String>> {
    anyhow::ensure!(
        data["has_more"].as_bool() == Some(false),
        summon_i18n::format!("rd62a0e5b6a")
    );
    anyhow::ensure!(
        data.get("truncations")
            .is_none_or(|v| v.is_null() || v.as_array().is_some_and(Vec::is_empty)),
        summon_i18n::format!("r9d036c5e06")
    );
    Ok(parse_bot_app_ids(data))
}

/// 单独构造参数，既让快/慢通道的勿扰策略可单测，也避免两份命令逐渐漂移。
fn active_chats_args(top_k: usize, exclude_muted: bool) -> Vec<String> {
    let mut args = vec![
        "im".to_string(),
        "+chat-list".to_string(),
        "--as".to_string(),
        "user".to_string(),
        "--types=p2p,group".to_string(),
        "--sort".to_string(),
        "active_time".to_string(),
        "--page-size".to_string(),
        top_k.clamp(1, 100).to_string(),
    ];
    if exclude_muted {
        args.push("--exclude-muted".to_string());
    }
    args
}

/// 从 lark-cli stdout 里定位 JSON 起点。有些 +shortcut 会在 JSON 前打印进度行
/// (实测 `+chat-members-list`:`[page 1] fetching...` / `Found 1 user(s)...`),
/// 所以不能整段直接 parse。
pub(crate) fn extract_json(stdout: &str) -> Option<&str> {
    stdout.find('{').map(|i| &stdout[i..])
}

/// "2026-08-07 14:38" / "2026-08-07 14:49:17"(lark-cli 本地时区渲染) / 纯数字 epoch 毫秒
/// → epoch ms。解析失败给 0(gate::fresh 拒绝,fail-closed)。
pub(crate) fn parse_time_ms(s: &str) -> i64 {
    let t = s.trim();
    if t.is_empty() {
        return 0;
    }
    if t.chars().all(|c| c.is_ascii_digit()) {
        return t.parse().unwrap_or(0);
    }
    for fmt in ["%Y-%m-%d %H:%M:%S", "%Y-%m-%d %H:%M"] {
        if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(t, fmt) {
            if let chrono::LocalResult::Single(dt) =
                chrono::TimeZone::from_local_datetime(&chrono::Local, &naive)
            {
                return dt.timestamp_millis();
            }
        }
    }
    0
}

pub(crate) fn parse_chats(data: &Value) -> Vec<ChatBrief> {
    data["chats"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|c| {
            let chat_id = c["chat_id"].as_str()?.to_string();
            Some(ChatBrief {
                chat_id,
                name: c["name"].as_str().unwrap_or_default().to_string(),
                chat_mode: c["chat_mode"].as_str().unwrap_or_default().to_string(),
            })
        })
        .collect()
}

fn parse_chat(data: &Value, fallback_chat_id: &str) -> Option<ChatBrief> {
    let chat_id = data["chat_id"]
        .as_str()
        .filter(|s| !s.is_empty())
        .unwrap_or(fallback_chat_id);
    let chat_mode = data["chat_mode"].as_str().filter(|s| !s.is_empty())?;
    Some(ChatBrief {
        chat_id: chat_id.to_string(),
        name: data["name"].as_str().unwrap_or_default().to_string(),
        chat_mode: chat_mode.to_string(),
    })
}

/// 从 lark-cli 渲染的正文里认出图片占位符,返回 `img_` 开头的资源 key。
///
/// lark-cli 把图片消息的 content 渲染成 `[Image: img_v3_xxx]` —— 对人可读,对 agent 是**死的**:
/// 2026-08-08 owner 召唤 agent 看两张窗帘轨道安装图,agent 只拿到占位符,只能回「我看不到图」。
/// 这个函数是把死占位符换回真图片的第一步。
///
/// 判据刻意窄:必须是 `[Image: ` 开头的整段占位、key 必须 `img_` 前缀。认不出返回 None —— 上游
/// 据此原样保留文本,不猜、不下载。
pub(crate) fn image_key_in(text: &str) -> Option<&str> {
    let rest = text.trim().strip_prefix("[Image:")?;
    let key = rest.strip_suffix(']')?.trim();
    (key.starts_with("img_") && !key.contains(char::is_whitespace)).then_some(key)
}

/// 消息数组解析。lark-cli 不同命令的 data 键不统一(`messages` / `items` 都见过),两个都认;
/// `fallback_chat_id` 供 chat-messages-list 场景(单会话查询,条目里不带 chat_id 时兜底)。
pub(crate) fn parse_msgs(data: &Value, fallback_chat_id: &str) -> Vec<MsgBrief> {
    let arr = data["messages"]
        .as_array()
        .or_else(|| data["items"].as_array());
    arr.into_iter()
        .flatten()
        .filter_map(|m| {
            let message_id = m["message_id"].as_str()?.to_string();
            Some(MsgBrief {
                message_id,
                chat_id: m["chat_id"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .unwrap_or(fallback_chat_id)
                    .to_string(),
                sender_id: m["sender"]["id"].as_str().unwrap_or_default().to_string(),
                sender_type: m["sender"]["sender_type"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
                sender_name: m["sender"]["name"].as_str().unwrap_or_default().to_string(),
                create_time_ms: parse_time_ms(m["create_time"].as_str().unwrap_or_default()),
                text: m["content"].as_str().unwrap_or_default().to_string(),
                msg_type: m["msg_type"].as_str().unwrap_or_default().to_string(),
                chat_name: m["chat_name"].as_str().unwrap_or_default().to_string(),
            })
        })
        .collect()
}

pub(crate) fn parse_search_ids(data: &Value) -> Vec<String> {
    data["items"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|v| v.as_str().map(|s| s.to_string()))
        .collect()
}

pub(crate) fn parse_bot_app_ids(data: &Value) -> Vec<String> {
    data["bots"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|b| b["app_id"].as_str().map(|s| s.to_string()))
        .collect()
}

/// 真人成员解析:users 桶的 (member_id, name)。member_id 即该 app 命名空间下的 open_id。
pub(crate) fn parse_user_members(data: &Value) -> Vec<(String, String)> {
    data["users"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|u| {
            let id = u["member_id"].as_str()?.to_string();
            Some((id, u["name"].as_str().unwrap_or_default().to_string()))
        })
        .collect()
}

/// 拉人响应的三张失败清单任一非空 → 返回原因。飞书对部分失败也回 200,必须显式检查。
pub(crate) fn member_add_failure(data: &Value) -> Option<String> {
    for key in [
        "invalid_id_list",
        "not_existed_id_list",
        "pending_approval_id_list",
    ] {
        if let Some(arr) = data[key].as_array() {
            if !arr.is_empty() {
                return Some(format!("{key}={arr:?}"));
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {

    /// profile 只有 bot 身份时,`auth status` 里没有 user openId:要报成能被认出来的授权缺口,
    /// 并带上是哪个 profile —— 补救动作(换 profile 或给它登录)全靠这一项。
    #[test]
    fn a_bot_only_profile_is_reported_as_missing_user_login() {
        let temp = tempfile::tempdir().unwrap();
        let fake = temp.path().join("lark-cli");
        crate::test_support::write_test_executable(
            &fake,
            "#!/bin/sh\nprintf '%s\\n' '{\"identities\":{\"bot\":{\"status\":\"ready\"},\"user\":{\"status\":\"missing\"}}}'\n",
        );

        let lark = super::LarkUser::new(
            Some("bot-only".to_string()),
            Some(fake.to_string_lossy().into_owned()),
        );
        let err = lark.owner_open_id().unwrap_err();
        assert_eq!(
            err.downcast_ref::<super::LarkUserLoginMissing>(),
            Some(&super::LarkUserLoginMissing {
                profile: Some("bot-only".to_string())
            })
        );
    }

    #[test]
    fn 显式lark_cli路径优先于path裸命令() {
        let lark = super::LarkUser::new(
            Some("owner".to_string()),
            Some("/home/me/.npm-global/bin/lark-cli".to_string()),
        );
        assert_eq!(
            lark.executable.to_string_lossy(),
            "/home/me/.npm-global/bin/lark-cli"
        );
        assert_eq!(lark.profile.as_deref(), Some("owner"));

        let fallback = super::LarkUser::new(None, None);
        assert_eq!(fallback.executable.to_string_lossy(), "lark-cli");
    }

    /// 2026-08-07 事故回归：daemon 若从 agent pane 重启，PATH 第一项会是身份 wrapper。
    /// 启动段净化后 resolver 必须选中真实 lark-cli，sentinel 的 owner_open_id 仍可成功。
    #[test]
    fn wrapper前插不再劫持sentinel_owner身份() {
        let temp = tempfile::tempdir().unwrap();
        let wrapper_dir = temp.path().join(".summon/runtime/lark-cli-wrappers/agent");
        let real_dir = temp.path().join("real-bin");
        std::fs::create_dir_all(&wrapper_dir).unwrap();
        std::fs::create_dir_all(&real_dir).unwrap();
        let wrapper = wrapper_dir.join("lark-cli");
        let real = real_dir.join("lark-cli");
        crate::test_support::write_test_executable(
            &wrapper,
            "#!/bin/sh\necho wrapper-was-used >&2\nexit 91\n",
        );
        crate::test_support::write_test_executable(
            &real,
            "#!/bin/sh\nprintf '%s\\n' '{\"identities\":{\"user\":{\"openId\":\"ou_owner\"}}}'\n",
        );

        let dirty = std::env::join_paths([wrapper_dir, real_dir]).unwrap();
        let clean = crate::setup::env::path_without_lark_cli_wrappers(&dirty);
        let selected =
            summon_core::tools::find_executable(std::path::Path::new("lark-cli"), &clean)
                .expect("净化后应找到真实 lark-cli");
        assert_eq!(selected, real);
        let lark = super::LarkUser::new(None, Some(selected.to_string_lossy().to_string()));
        assert_eq!(lark.owner_open_id().unwrap(), "ou_owner");
    }

    #[test]
    fn owner_within_gives_up_on_a_hung_lark_cli() {
        let temp = tempfile::tempdir().unwrap();
        let fake = temp.path().join("lark-cli");
        crate::test_support::write_test_executable(&fake, "#!/bin/sh\nexec sleep 30\n");
        let lark = super::LarkUser::new(None, Some(fake.to_string_lossy().into_owned()));
        let started = std::time::Instant::now();
        let err = lark
            .owner_within(std::time::Duration::from_millis(300))
            .unwrap_err();
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
        assert!(err.to_string().contains("没有返回"), "{err:#}");
    }

    #[test]
    fn owner_carries_the_logged_in_name_when_lark_cli_has_one() {
        let temp = tempfile::tempdir().unwrap();
        let fake = temp.path().join("lark-cli");
        crate::test_support::write_test_executable(
            &fake,
            "#!/bin/sh\nprintf '%s\\n' '{\"identities\":{\"user\":{\"openId\":\"ou_owner\",\"userName\":\"示例用户\"}}}'\n",
        );
        let lark = super::LarkUser::new(None, Some(fake.to_string_lossy().into_owned()));
        let want = super::LarkOwner {
            open_id: "ou_owner".into(),
            name: Some("示例用户".into()),
        };
        assert_eq!(lark.owner().unwrap(), want);
        assert_eq!(
            lark.owner_within(std::time::Duration::from_secs(10))
                .unwrap(),
            want
        );
    }

    /// 图片占位符识别。真机形态取自 2026-08-08 那条没被 agent 看见的窗帘安装图。
    #[test]
    fn 图片占位符识别出资源key() {
        assert_eq!(
            super::image_key_in("[Image: img_v3_0214c_8911779f-e245-4e96-b85f-4e581752cdhu]"),
            Some("img_v3_0214c_8911779f-e245-4e96-b85f-4e581752cdhu")
        );
        // 前后空白不影响。
        assert_eq!(super::image_key_in("  [Image: img_abc]  "), Some("img_abc"));
    }

    /// 认不出就返回 None —— 上游据此原样保留文本,不猜、不下载。
    #[test]
    fn 非图片占位符一律不认() {
        for t in [
            "这是一段普通正文",
            "[File: file_v3_xxx]",            // 文件不在本条路径的射程内
            "[Image: file_v3_xxx]",           // key 前缀不对
            "[Image: img_a img_b]",           // 含空白 = 不是单个 key
            "看这张 [Image: img_abc] 怎么样", // 占位符不是整段 → 不认(避免截错边界)
            "[Image: img_abc",                // 没闭合
            "",
        ] {
            assert_eq!(super::image_key_in(t), None, "不该认出:{t}");
        }
    }

    /// 2026-08-07 现场原样响应:缺 `im:chat:read` 时 lark-cli 给的是**结构化**错误,
    /// `missing_scopes` 写得清清楚楚。此前这段信息在传出去之前就被丢了(只读 stdout),
    /// 于是 owner 只能靠人猜 scope 名 —— 我猜了个不存在的 `im:chat:readonly`,白授权一次。
    #[test]
    fn missing_scope_is_recognized_with_the_exact_scope_names() {
        let raw = r#"{"ok":false,"identity":"user","error":{"type":"authorization",
            "subtype":"missing_scope","message":"missing required scope(s): im:chat:read",
            "missing_scopes":["im:chat:read"]}}"#;
        let v: Value = serde_json::from_str(raw).unwrap();
        assert_eq!(
            missing_scopes(&v),
            Some(vec!["im:chat:read".to_string()]),
            "必须原样取出 scope 名,不能靠猜"
        );
    }

    /// 只标了 subtype、没给数组时仍算授权错误:「知道是权限问题但不知道缺哪个」
    /// 比「当成未知失败」有用 —— 至少能告诉 owner 去看授权页。
    #[test]
    fn missing_scope_subtype_alone_still_counts_as_auth_error() {
        let v: Value =
            serde_json::from_str(r#"{"ok":false,"error":{"subtype":"missing_scope"}}"#).unwrap();
        assert_eq!(missing_scopes(&v), Some(vec![]));
    }

    /// 非授权失败不能被误判成授权问题 —— 否则会给 owner 发一张无从下手的授权卡。
    #[test]
    fn other_failures_are_not_auth_errors() {
        let v: Value = serde_json::from_str(
            r#"{"ok":false,"error":{"type":"network","subtype":"timeout","message":"timed out"}}"#,
        )
        .unwrap();
        assert_eq!(missing_scopes(&v), None);
        let v2: Value = serde_json::from_str(r#"{"ok":true,"data":{}}"#).unwrap();
        assert_eq!(missing_scopes(&v2), None);
    }

    /// 结构化错误若只出现在 stderr(现场就是这样),也必须被解析出来。
    #[test]
    fn json_is_extracted_from_stderr_when_stdout_is_empty() {
        let stderr = r#"some noise
{"ok":false,"error":{"subtype":"missing_scope","missing_scopes":["im:chat:read"]}}"#;
        assert!(extract_json("").is_none(), "空 stdout 本身取不出 JSON");
        let json = extract_json(stderr).expect("stderr 里的 JSON 必须取得出来");
        let v: Value = serde_json::from_str(json).unwrap();
        assert_eq!(missing_scopes(&v), Some(vec!["im:chat:read".to_string()]));
    }
    use super::*;

    #[test]
    fn 剥进度行前缀取json() {
        let out = "[page 1] fetching...\nFound 1 user(s) and 2 bot(s)\n{\"ok\":true}\n";
        assert_eq!(extract_json(out).unwrap(), "{\"ok\":true}\n");
        assert!(extract_json("no json here").is_none());
    }

    #[test]
    fn 时间解析_分钟精度与秒精度与epoch() {
        // 分钟精度(chat-messages-list / mget 实测格式)
        assert!(parse_time_ms("2026-08-07 14:38") > 0);
        // 秒精度(messages-send 响应实测格式)
        let s = parse_time_ms("2026-08-07 14:49:17");
        let m = parse_time_ms("2026-08-07 14:49");
        assert_eq!(s - m, 17_000);
        // 纯数字按 epoch ms 原样过
        assert_eq!(parse_time_ms("1775648961145"), 1_775_648_961_145);
        // 解析失败 → 0(fail-closed)
        assert_eq!(parse_time_ms(""), 0);
        assert_eq!(parse_time_ms("昨天"), 0);
    }

    #[test]
    fn 会话列表解析() {
        let data: Value = serde_json::from_str(
            r#"{"chats":[
                {"chat_id":"oc_1","chat_mode":"p2p","name":"日历助手","p2p_target_type":"bot"},
                {"chat_id":"oc_2","chat_mode":"group","name":"sample开发群"},
                {"no_chat_id":true}
            ]}"#,
        )
        .unwrap();
        let cs = parse_chats(&data);
        assert_eq!(cs.len(), 2);
        assert_eq!(cs[0].chat_id, "oc_1");
        assert_eq!(cs[1].chat_mode, "group");
    }

    #[test]
    fn 单会话详情解析保留真实类型() {
        let data: Value =
            serde_json::from_str(r#"{"chat_id":"oc_1","chat_mode":"p2p","name":"私聊"}"#).unwrap();
        let c = parse_chat(&data, "fallback").unwrap();
        assert_eq!(c.chat_id, "oc_1");
        assert_eq!(c.chat_mode, "p2p");

        let fallback: Value = serde_json::from_str(r#"{"chat_mode":"group"}"#).unwrap();
        assert_eq!(parse_chat(&fallback, "oc_f").unwrap().chat_id, "oc_f");
        assert!(parse_chat(&serde_json::json!({}), "oc_f").is_none());
    }

    #[test]
    fn 快速会话列表排除勿扰而慢速列表保留() {
        let fast = active_chats_args(5, true);
        assert!(fast.iter().any(|a| a == "--exclude-muted"));
        assert!(fast.windows(2).any(|w| w == ["--page-size", "5"]));

        let slow = active_chats_args(50, false);
        assert!(!slow.iter().any(|a| a == "--exclude-muted"));
        assert!(slow.windows(2).any(|w| w == ["--page-size", "50"]));
    }

    #[test]
    fn 消息解析_messages键与items键都认() {
        let m = r#"{"message_id":"om_1","chat_id":"oc_x","create_time":"2026-08-07 14:38",
            "content":"@阿卡迪亚 进来回答 xx","msg_type":"text",
            "sender":{"id":"ou_a","id_type":"open_id","sender_type":"user"}}"#;
        let a: Value = serde_json::from_str(&format!(r#"{{"messages":[{m}]}}"#)).unwrap();
        let b: Value = serde_json::from_str(&format!(r#"{{"items":[{m}]}}"#)).unwrap();
        for data in [a, b] {
            let ms = parse_msgs(&data, "");
            assert_eq!(ms.len(), 1);
            assert_eq!(ms[0].sender_id, "ou_a");
            assert!(ms[0].create_time_ms > 0);
        }
    }

    #[test]
    fn 消息解析_无chat_id用兜底() {
        let data: Value = serde_json::from_str(
            r#"{"items":[{"message_id":"om_1","create_time":"2026-08-07 14:38","content":"x",
                "msg_type":"text","sender":{"id":"ou_a","id_type":"open_id","sender_type":"user"}}]}"#,
        )
        .unwrap();
        let ms = parse_msgs(&data, "oc_fallback");
        assert_eq!(ms[0].chat_id, "oc_fallback");
    }

    #[test]
    fn 搜索id与bot成员解析() {
        let s: Value = serde_json::from_str(r#"{"items":["om_1","om_2"]}"#).unwrap();
        assert_eq!(parse_search_ids(&s), vec!["om_1", "om_2"]);
        let b: Value = serde_json::from_str(
            r#"{"bots":[{"app_id":"cli_a","name":"A"},{"app_id":"cli_b","name":"B"}]}"#,
        )
        .unwrap();
        assert_eq!(parse_bot_app_ids(&b), vec!["cli_a", "cli_b"]);
    }

    #[test]
    fn 拉人失败清单检查() {
        let ok: Value = serde_json::from_str(
            r#"{"invalid_id_list":[],"not_existed_id_list":[],"pending_approval_id_list":[]}"#,
        )
        .unwrap();
        assert!(member_add_failure(&ok).is_none());
        let bad: Value =
            serde_json::from_str(r#"{"invalid_id_list":["cli_x"],"not_existed_id_list":[]}"#)
                .unwrap();
        assert!(member_add_failure(&bad)
            .unwrap()
            .contains("invalid_id_list"));
    }
}
