use super::endpoints;
mod document_comments;
use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// 群成员列表最多翻几页(每页 100)。1 万人足够覆盖任何真实协作群;设上限只为防服务端
/// `page_token` 不收敛时把 daemon 的取名线程无限拖住,不是业务约束。
const MAX_MEMBER_PAGES: usize = 100;
const MAX_BOT_CHAT_PAGES: usize = 100;

/// `fetch_message` 拉回的一条消息。普通消息只有一个 item；合并转发则保留全部 nested items，
/// 不能把外层固定占位当正文。
pub struct FetchedMessage {
    pub sender_open_id: String,
    content: FetchedMessageContent,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CotHandle {
    pub cot_id: String,
    pub message_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CotCompleteReason {
    Done,
    Error,
    Timeout,
}

impl CotCompleteReason {
    fn as_str(self) -> &'static str {
        match self {
            Self::Done => "done",
            Self::Error => "error",
            Self::Timeout => "timeout",
        }
    }
}

enum FetchedMessageContent {
    Single(FetchedMessageItem),
    MergeForward(Vec<FetchedMessageItem>),
}

struct FetchedMessageItem {
    sender_id: String,
    sender_type: String,
    msg_type: String,
    content: String,
    /// mget 响应的 `mentions` 数组摊平成 `(占位 key, 名字)`。文本消息正文里只有 `@_user_N`,
    /// 名字全在这;卡片的这个字段实测恒为 `null`,靠 open_id 回查,所以这里会是空。
    mentions: Vec<(String, String)>,
}

impl FetchedMessage {
    /// 合并转发逐条渲染发送人；同一 open_id 只解析一次，失败时保留原 id，避免不同人的消息
    /// 都退化成同一个“用户”而再次失去归属。普通引用不调用 resolver。
    pub fn quoted_plaintext_full_with_sender_names(
        &self,
        resolve_user_name: impl Fn(&str) -> Option<String>,
    ) -> String {
        match &self.content {
            FetchedMessageContent::Single(item) => summon_core::quote::quoted_plaintext_full(
                &item.msg_type,
                &item.content,
                summon_core::quote::Mentions::new(&item.mentions, &resolve_user_name),
            ),
            FetchedMessageContent::MergeForward(items) => {
                let mut sender_names = HashMap::<String, String>::new();
                let labels = items
                    .iter()
                    .map(|item| {
                        merge_forward_sender_label(item, &mut sender_names, &resolve_user_name)
                    })
                    .collect::<Vec<_>>();
                let parts = items
                    .iter()
                    .zip(labels.iter())
                    .map(|(item, sender)| summon_core::quote::MergeForwardItem {
                        sender,
                        msg_type: &item.msg_type,
                        content: &item.content,
                        mentions: summon_core::quote::Mentions::new(
                            &item.mentions,
                            &resolve_user_name,
                        ),
                    })
                    .collect::<Vec<_>>();
                summon_core::quote::merge_forward_plaintext(&parts)
            }
        }
    }

    /// 抽取被引用消息里的资源；合并转发可能同时含多张图片/多个文件。
    pub fn quoted_resources(&self) -> Vec<summon_core::ipc::Resource> {
        match &self.content {
            FetchedMessageContent::Single(item) => {
                summon_core::quote::extract_quoted_resource(&item.msg_type, &item.content)
                    .into_iter()
                    .collect()
            }
            FetchedMessageContent::MergeForward(items) => items
                .iter()
                .filter_map(|item| {
                    summon_core::quote::extract_quoted_resource(&item.msg_type, &item.content)
                })
                .collect(),
        }
    }
}

fn merge_forward_sender_label(
    item: &FetchedMessageItem,
    sender_names: &mut HashMap<String, String>,
    resolve_user_name: &impl Fn(&str) -> Option<String>,
) -> String {
    let sender_id = item.sender_id.trim();
    if sender_id.is_empty() {
        return if item.sender_type == "app" {
            summon_i18n::text!("r63c73c4730").to_string()
        } else {
            summon_i18n::text!("r1be0cad4e9").to_string()
        };
    }
    if item.sender_type == "app" {
        return summon_i18n::format!("r57ae7f2d54");
    }
    if let Some(name) = sender_names.get(sender_id) {
        return name.clone();
    }
    let label = resolve_user_name(sender_id)
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| sender_id.to_string());
    sender_names.insert(sender_id.to_string(), label.clone());
    label
}

const MENTIONLESS_CAPABILITIES: &[&[&str]] = &[
    &[
        "im:message.group_msg",
        "im:message.group_msg:readonly",
        "im:message.group_msg.include_bot:read",
    ],
    &["im:chat:read", "im:chat:readonly", "im:chat"],
    &[
        "im:chat.members:read",
        "im:chat:readonly",
        "im:chat",
        "im:chat.group_info:readonly",
    ],
];

/// 飞书出站:取 tenant_access_token(按实例缓存)+ 发 interactive 卡片 + 解析发送人名。
/// dry_run = true 时只打印不真发(M2-core / 无凭据)。
pub struct FeishuClient {
    base: String, // https://open.feishu.cn
    app_id: String,
    permission_console: String,
    app_secret: String,
    dry_run: bool,
    token: Mutex<Option<(String, Instant, Duration)>>,
    names: Mutex<HashMap<String, String>>, // open_id → 人名 缓存
    union_ids: Mutex<HashMap<String, String>>, // open_id → union_id 缓存(operate 鉴权用)
    chat_names: Mutex<HashMap<String, String>>, // chat_id → 群名 缓存
    bot_open_id: Mutex<Option<String>>,    // 本 app 的 bot open_id 缓存(群 @ 门控用)
    /// bot info 上次失败时刻(R4 负缓存):窗口内不重试,免得网络故障期每条群消息都在
    /// dispatcher 线程里同步重打 token+bot info 两跳 HTTP,饿死该 agent 全部 chat 的派发。
    bot_open_id_fail: Mutex<Option<Instant>>,
    http: reqwest::blocking::Client,
    /// 被审核以含邮箱拒发时要不要打码重发,每次被拒时问一次(见 [`Self::with_email_mask`])。
    email_mask: Option<EmailMaskSwitch>,
    recalled_cards: Mutex<HashSet<String>>,
}

/// 邮箱打码开关:被拒时调用,返回此刻是否打码重发。
pub type EmailMaskSwitch = std::sync::Arc<dyn Fn() -> bool + Send + Sync>;

/// bot info 失败负缓存窗口内?(纯判定,window 由调用方注入)
fn bot_neg_cache_active(last_fail: Option<Instant>, window: Duration) -> bool {
    last_fail.is_some_and(|t| t.elapsed() < window)
}

/// bot info 失败负缓存窗口:1 分钟内不重试(成功后清除)。
const BOT_INFO_NEG_TTL: Duration = Duration::from_secs(60);

#[cfg(not(test))]
const INVALID_MESSAGE_ID_RETRY_DELAYS: [Duration; 2] =
    [Duration::from_millis(500), Duration::from_millis(1500)];
#[cfg(test)]
const INVALID_MESSAGE_ID_RETRY_DELAYS: [Duration; 2] =
    [Duration::from_millis(1), Duration::from_millis(1)];

/// 卡片动作锁的退避表。锁只持续到那次回调 ACK 完成,量级是几百毫秒;给三档、总计约 1.4s,
/// 足够覆盖 sidecar→飞书的往返抖动,又不至于让用户感到「点完卡了很久」。
#[cfg(not(test))]
const CARD_LOCKED_RETRY_DELAYS: [Duration; 3] = [
    Duration::from_millis(200),
    Duration::from_millis(400),
    Duration::from_millis(800),
];
#[cfg(test)]
const CARD_LOCKED_RETRY_DELAYS: [Duration; 3] = [
    Duration::from_millis(1),
    Duration::from_millis(1),
    Duration::from_millis(1),
];

/// 飞书会在 token 尚处于本地 TTL 内时主动令它失效。此时 HTTP 状态不稳定(真机见过 400),
/// 唯一可靠判据是响应 JSON 的业务错误码。
const INVALID_TENANT_ACCESS_TOKEN_CODE: i64 = 99_991_663;

fn is_invalid_tenant_access_token(body: &[u8]) -> bool {
    serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|response| response["code"].as_i64())
        == Some(INVALID_TENANT_ACCESS_TOKEN_CODE)
}

/// 飞书内容审核拦下了这条消息(`code=230028`,例如正文里有邮箱地址)。同一正文原样重发多少次
/// 结果都一样,调用方据它改发说明。
#[derive(Debug)]
pub struct ContentRejected {
    /// 飞书给的拦截原因,取 msg 里 `ext=` 之后那段(如 `contain sensitive data: EMAIL_ADDRESS`)。
    pub reason: String,
    detail: String,
}

impl std::fmt::Display for ContentRejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.detail)
    }
}

impl std::error::Error for ContentRejected {}

#[cfg(test)]
impl ContentRejected {
    pub(crate) fn for_test(reason: &str) -> Self {
        Self {
            reason: reason.to_string(),
            detail: format!("send_card 失败 400 Bad Request: {reason}"),
        }
    }
}

const CONTENT_AUDIT_REJECTED_CODE: i64 = 230_028;

/// 发消息失败的错误:审核拦截成 [`ContentRejected`],其余是带原始响应的文本错误。
fn send_failure(operation: &str, status: reqwest::StatusCode, body: &str) -> anyhow::Error {
    let detail = summon_i18n::format!("rc32de152ef");
    let response = serde_json::from_str::<serde_json::Value>(body).ok();
    match response
        .as_ref()
        .filter(|response| response["code"].as_i64() == Some(CONTENT_AUDIT_REJECTED_CODE))
    {
        Some(response) => {
            let msg = response["msg"].as_str().unwrap_or_default();
            let reason = msg
                .split_once("ext=")
                .map_or(msg, |(_, reason)| reason)
                .trim()
                .to_string();
            anyhow::Error::new(ContentRejected { reason, detail })
        }
        None => anyhow::anyhow!(detail),
    }
}

fn is_transient_invalid_message_id(status: reqwest::StatusCode, body: &str) -> bool {
    if status != reqwest::StatusCode::BAD_REQUEST {
        return false;
    }
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .is_some_and(|response| {
            response["code"].as_i64() == Some(230001)
                && response["msg"]
                    .as_str()
                    .is_some_and(|message| message.contains("invalid message_id"))
        })
}

/// 「卡片动作锁」:飞书处理一次卡片回调期间会锁住该卡,此时 PATCH 一律 400
/// (`code=230099`,msg 里带 `card action is lock`)。
///
/// 这是 sidecar 与 daemon 之间的**天然竞态**——sidecar 收到 `card.action.trigger` 后先把事件写进
/// socket(异步投给 daemon),**再**返回响应完成 ACK;daemon 拿到事件立刻 PATCH 卡片,跑得比那次
/// ACK 快就撞上锁。handler 里 IO 越少越容易撞:纯渲染动作(只重画一张卡、不读盘)几乎必中,
/// 读写盘的动作反而因为够慢躲了过去。所以它表现得像「偶发」,其实是按 handler 快慢分布的。
///
/// 这里的退避重试是治标——锁只持续到那次 ACK 完成,几百毫秒内必然释放。治本是改用**回调响应
/// 直接返回新卡片**(飞书 `card.action.trigger` 支持),但那要求 sidecar 同步等 daemon 渲染完,
/// 而 events.sock 目前是单向异步,属于另一个量级的改动。
fn is_card_action_locked(status: reqwest::StatusCode, body: &str) -> bool {
    if status != reqwest::StatusCode::BAD_REQUEST {
        return false;
    }
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .is_some_and(|response| {
            response["code"].as_i64() == Some(230099)
                && response["msg"]
                    .as_str()
                    .is_some_and(|message| message.contains("card action is lock"))
        })
}

/// bot/v3/info 的解析结果(机器人能力探测 + 名字展示)。
#[derive(Debug)]
pub struct BotInfo {
    pub app_name: String,
    pub open_id: String,
}

impl FeishuClient {
    pub fn new(domain: &str, app_id: String, app_secret: String, dry_run: bool) -> Self {
        // API 域名是 open.feishu.cn / open.larksuite.com(不是裸 feishu.cn,后者是官网→404)
        let host = match domain {
            "larksuite.com" | "open.larksuite.com" => "open.larksuite.com",
            "feishu.cn" | "open.feishu.cn" => "open.feishu.cn",
            other => other,
        };
        FeishuClient {
            base: format!("https://{host}"),
            permission_console: if host == "open.larksuite.com" {
                "https://open.larksuite.com"
            } else {
                "https://open.feishu.cn"
            }
            .into(),
            app_id,
            app_secret,
            dry_run,
            token: Mutex::new(None),
            names: Mutex::new(HashMap::new()),
            union_ids: Mutex::new(HashMap::new()),
            chat_names: Mutex::new(HashMap::new()),
            bot_open_id: Mutex::new(None),
            bot_open_id_fail: Mutex::new(None),
            // R4:显式超时——reqwest 默认无总超时,网络黑洞时一跳可挂 30s+,dispatcher 线程
            // 内的同步调用会饿死该 agent 全部 chat 的派发。连接 10s / 整请求 30s 兜底
            // (30s 兼顾附件下载,不至于截断大文件常规耗时)。build 失败回退默认 client。
            http: reqwest::blocking::Client::builder()
                .connect_timeout(Duration::from_secs(10))
                .timeout(Duration::from_secs(30))
                .build()
                .unwrap_or_else(|_| reqwest::blocking::Client::new()),
            email_mask: None,
            recalled_cards: Mutex::new(HashSet::new()),
        }
    }

    /// 发卡、回复、改卡、CoT 推送被飞书审核以含邮箱整条拒发时,`switch` 返回真就把其中的邮箱
    /// 打码后重发一次(见 [`summon_core::email_mask`])。不设就不重发,照常报被拒。
    pub fn with_email_mask(mut self, switch: EmailMaskSwitch) -> Self {
        self.email_mask = Some(switch);
        self
    }

    /// 这次失败是审核以含邮箱拒发,且开关开着。
    fn masks_after(&self, error: &anyhow::Error) -> bool {
        error
            .downcast_ref::<ContentRejected>()
            .is_some_and(|rejected| rejected.reason.contains("EMAIL_ADDRESS"))
            && self.email_mask.as_ref().is_some_and(|switch| switch())
    }

    /// [`Self::masks_after`] 成立时给出打码后的卡片;没有可打的或不该打返回 `None`。
    fn masked_after_rejection(&self, error: &anyhow::Error, card: &str) -> Option<String> {
        if !self.masks_after(error) {
            return None;
        }
        let masked = summon_core::email_mask::mask_emails_in_card(card)?;
        crate::observe::log::log(summon_i18n::text!("re55c3275cb"));
        Some(masked)
    }

    #[cfg(test)]
    pub(crate) fn with_test_base(base: String) -> Self {
        let mut client = Self::new("feishu.cn", "test-app".into(), "test-secret".into(), false);
        client.base = base;
        client
    }

    #[cfg(test)]
    pub(crate) fn with_test_base_connection_close(base: String) -> Self {
        let mut client = Self::with_test_base(base);
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::CONNECTION,
            reqwest::header::HeaderValue::from_static("close"),
        );
        // 只带 `Connection: close` 不够：hyper 不看请求头里的 close，tiny_http 的回应又不带它，
        // 连接照样回池；服务端的 FIN 晚到一步，下一条请求就写进对端已不再读的连接，报
        // 「connection closed before message completed」。关掉连接池，每条请求都新建连接。
        client.http = reqwest::blocking::Client::builder()
            .default_headers(headers)
            .pool_max_idle_per_host(0)
            .connect_timeout(Duration::from_secs(2))
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();
        client
    }

    /// 校验 app 凭据能否换到 tenant_access_token(bind 绑定时验真用)。
    pub fn check_auth(&self) -> anyhow::Result<()> {
        self.token().map(|_| ())
    }

    /// 本 app 的 bot open_id(带缓存)。群消息门控用:只有 mentions 命中它才应答。
    /// dry_run / 拿不到时返回 Err,由调用方决定回退策略。失败带 1 分钟负缓存(R4):
    /// 窗口内直接 Err 快路径,不重打 HTTP。
    pub fn bot_open_id(&self) -> anyhow::Result<String> {
        if let Some(id) = self.bot_open_id.lock().unwrap().as_ref() {
            return Ok(id.clone());
        }
        if self.dry_run {
            anyhow::bail!("dry_run");
        }
        if bot_neg_cache_active(*self.bot_open_id_fail.lock().unwrap(), BOT_INFO_NEG_TTL) {
            anyhow::bail!(summon_i18n::format!("r7a4c4088ce"));
        }
        let fetch = || -> anyhow::Result<String> { Ok(self.fetch_bot_v3_info()?.open_id) };
        match fetch() {
            Ok(id) => {
                *self.bot_open_id.lock().unwrap() = Some(id.clone());
                *self.bot_open_id_fail.lock().unwrap() = None;
                Ok(id)
            }
            Err(e) => {
                *self.bot_open_id_fail.lock().unwrap() = Some(Instant::now());
                Err(e)
            }
        }
    }

    /// 机器人能力探测(onboard 配置体检用):`bot/v3/info` 端点(URL 见 endpoints.rs)。
    /// 成功 = 机器人能力已启用(顺带拿回 bot 名字);失败多半是能力未启用或应用未发布。
    /// 无缓存、无负缓存 —— 一次性体检调用,不碰 daemon 门控用的 [`Self::bot_open_id`] 缓存。
    pub fn bot_v3_info(&self) -> anyhow::Result<BotInfo> {
        if self.dry_run {
            anyhow::bail!("dry_run");
        }
        self.fetch_bot_v3_info()
    }

    /// bot/v3/info 原始请求 + 解析,bot_open_id(带缓存)与 bot_v3_info(体检)共用。
    fn fetch_bot_v3_info(&self) -> anyhow::Result<BotInfo> {
        let url = endpoints::bot_info(&self.base);
        let (status, body) = self.request_with_token_refresh(|token| {
            Ok(self.http.get(&url).bearer_auth(token).send()?)
        })?;
        let body = String::from_utf8_lossy(&body);
        if !status.is_success() {
            anyhow::bail!(summon_i18n::format!("r6769aba5ad"));
        }
        let v: serde_json::Value = serde_json::from_str(&body)
            .map_err(|e| anyhow::anyhow!(summon_i18n::format!("r663ff21630")))?;
        // code != 0 是飞书业务错误(机器人能力未启用等):把 code/msg 带出来,调用方据此分流。
        if let Some(code) = v["code"].as_i64() {
            if code != 0 {
                let msg = v["msg"].as_str().unwrap_or("");
                anyhow::bail!(summon_i18n::format!("r4c650d4857"));
            }
        }
        let bot = &v["bot"];
        let open_id = bot["open_id"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow::anyhow!(summon_i18n::format!("r6a618ee2fc")))?
            .to_string();
        let app_name = bot["app_name"].as_str().unwrap_or("").to_string();
        Ok(BotInfo { app_name, open_id })
    }

    fn token(&self) -> anyhow::Result<String> {
        // 用 API 返回的 expire 字段(秒)做缓存 TTL,留 300s 安全余量,避免用到即将过期的 token。
        // 飞书 tenant_access_token 默认 7200s 过期,硬编码 6000s 在 token 被续期时可能用到失效值。
        const TOKEN_SAFETY_MARGIN: Duration = Duration::from_secs(300);
        if let Some((t, at, ttl)) = self.token.lock().unwrap().as_ref() {
            if at.elapsed() < *ttl {
                return Ok(t.clone());
            }
        }
        let url = endpoints::tenant_access_token(&self.base);
        // 先读原始文本再解析,失败时把 status + body 一并报出来(别让 .json() 吞掉真实响应)
        let resp = self
            .http
            .post(url)
            .json(&serde_json::json!({"app_id": self.app_id, "app_secret": self.app_secret}))
            .send()?;
        let status = resp.status();
        let body = resp.text()?;
        let v: serde_json::Value = serde_json::from_str(&body)
            .map_err(|e| anyhow::anyhow!(summon_i18n::format!("rfe06284def")))?;
        let t = v["tenant_access_token"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!(summon_i18n::format!("rb56f939a81")))?
            .to_string();
        // expire 字段(秒):飞书 API 返回的 token 有效期;缺省时回退 7200s(飞书默认值)。
        let expire_secs = v["expire"].as_u64().unwrap_or(7200);
        let ttl = Duration::from_secs(expire_secs).saturating_sub(TOKEN_SAFETY_MARGIN);
        *self.token.lock().unwrap() = Some((t.clone(), Instant::now(), ttl));
        Ok(t)
    }

    /// 执行一次带 tenant token 的飞书请求。服务端若明确返回 99991663,只清掉**本次被拒的**
    /// 那个缓存 token,立即重取并原样重试一次。
    ///
    /// 条件清理很重要:多个 chat 可并发撞到同一个旧 token。A 已经换到新 token 后,B 的旧响应
    /// 才回来时不能把 A 的新值再清掉；否则会形成刷新风暴。第二次仍失败则把真实响应交给调用方,
    /// 不做无限重试。
    fn request_with_token_refresh(
        &self,
        mut request: impl FnMut(&str) -> anyhow::Result<reqwest::blocking::Response>,
    ) -> anyhow::Result<(reqwest::StatusCode, Vec<u8>)> {
        let mut token = self.token()?;
        for attempt in 0..=1 {
            let response = request(&token)?;
            let status = response.status();
            let body = response.bytes()?.to_vec();
            if attempt == 0 && is_invalid_tenant_access_token(&body) {
                let mut cached = self.token.lock().unwrap();
                if cached
                    .as_ref()
                    .is_some_and(|(current, _, _)| current == &token)
                {
                    *cached = None;
                }
                drop(cached);
                crate::observe::log::log(summon_i18n::text!("r273c15abd4"));
                token = self.token()?;
                continue;
            }
            return Ok((status, body));
        }
        unreachable!("token refresh retry loop has a fixed terminal attempt")
    }

    /// 加表情 reaction,返回 reaction_id(撤回时用)。dry_run 打印并回假 id。
    pub fn react(&self, message_id: &str, emoji_type: &str) -> anyhow::Result<String> {
        if self.dry_run {
            println!("[dry-run] react {emoji_type} on msg={message_id}");
            return Ok("dry-run".to_string());
        }
        let url = endpoints::reactions(&self.base, message_id);
        let (status, body) = self.request_with_token_refresh(|token| {
            Ok(self
                .http
                .post(&url)
                .bearer_auth(token)
                .json(&serde_json::json!({ "reaction_type": { "emoji_type": emoji_type } }))
                .send()?)
        })?;
        let body = String::from_utf8_lossy(&body);
        if !status.is_success() {
            anyhow::bail!(summon_i18n::format!("r4c0a49f6de"));
        }
        let v: serde_json::Value = serde_json::from_str(&body)
            .map_err(|e| anyhow::anyhow!(summon_i18n::format!("r307c670e38")))?;
        Ok(v["data"]["reaction_id"]
            .as_str()
            .unwrap_or_default()
            .to_string())
    }

    /// 撤回某个 reaction(by reaction_id)。dry_run 打印。
    pub fn unreact(&self, message_id: &str, reaction_id: &str) -> anyhow::Result<()> {
        if self.dry_run {
            println!("[dry-run] unreact {reaction_id} on msg={message_id}");
            return Ok(());
        }
        let url = endpoints::reaction_delete(&self.base, message_id, reaction_id);
        let (status, body) = self.request_with_token_refresh(|token| {
            Ok(self.http.delete(&url).bearer_auth(token).send()?)
        })?;
        let body = String::from_utf8_lossy(&body);
        if !status.is_success() {
            anyhow::bail!(summon_i18n::format!("r4735f29397"));
        }
        Ok(())
    }

    /// 仅查缓存,不发网络请求。empty open_id → None;未命中 → None;命中 → Some(name)。
    pub fn resolve_name_cached(&self, open_id: &str) -> Option<String> {
        if open_id.is_empty() {
            return None;
        }
        self.names.lock().unwrap().get(open_id).cloned()
    }

    /// 用 mention / 磁盘 identity-cache 里的 `{open_id, name}` 刷新内存名字表(**覆盖语义**)。
    ///
    /// why:identity-cache 记的是入站消息里 mention 自带的名字(零权限可得)。预热之后,
    /// `/introduce` 那一刻学到的名字在**下一条消息**就能用上,不必等群成员列表异步补齐。
    ///
    /// why 覆盖而不是 `or_insert`:调用点有两类,**两类都要求以新值为准** ——
    /// ① daemon 冷启用磁盘缓存预热(此时内存表本来就空,覆不覆盖无差别);
    /// ② 每条入站消息用本条 mention 刷新(dispatcher)。而人是会改名的:`or_insert` 让首次
    /// 学到的名字变成永久值,改名后整个 daemon 生命周期都叫不对,只能靠重启纠正。
    /// 两个来源同源同鲜度(都是飞书当条下发的显示名),没有「旧值更可信」的情形。
    pub fn prime_names(&self, entries: impl IntoIterator<Item = (String, String)>) {
        let mut cache = self.names.lock().unwrap();
        for (open_id, name) in entries {
            if open_id.is_empty() || name.is_empty() {
                continue;
            }
            cache.insert(open_id, name);
        }
    }

    /// 只查内存缓存。**取名不再有网络路径** —— 名字来源是群成员列表(入站时批量灌,见
    /// [`Self::fetch_and_cache_chat_member_names`])与 identity-cache 预热([`Self::prime_names`])。
    ///
    /// 私聊等拿不到群成员的场景 miss 就是 miss,由调用方回退占位,**不缓存占位**
    /// (缓存占位会让名字补齐后永不重试,老坑)。
    pub fn fetch_and_cache_name(&self, open_id: &str) -> Option<String> {
        if open_id.is_empty() {
            return None;
        }
        self.names.lock().unwrap().get(open_id).cloned()
    }

    /// 把 sender 的 open_id 解析成人名(带缓存)。失败/dry_run 回退为占位,绝不阻断。
    /// 委托给 fetch_and_cache_name;失败不再缓存占位(fix:缓存"用户"会令后续重试永远失败)。
    pub fn resolve_name(&self, open_id: &str) -> String {
        self.fetch_and_cache_name(open_id)
            .unwrap_or_else(|| summon_i18n::text!("r0d0e1a86b3").to_string())
    }

    /// 拉一次群成员列表,把**全部成员(人 + bot)**的名字批量灌进名字缓存,返回 `open_id` 对应的名字。
    ///
    /// **为什么不用 `contact/v3/users`**:那个接口只覆盖 app 的通讯录数据权限范围,范围外的人一律
    /// `41050 no user authority error` —— 每加一个协作者都要走审批,而群成员列表零额外权限就能拿到
    /// 同一份名字。2026-08-05 真机实测:同一个 app 查群成员拿得到「李四」,查 contact 拿不到。
    ///
    /// 一次调用缓存整群(不是逐个查):同群后续任何人发言都直接命中缓存,零额外往返。
    /// 失败/dry_run → None 且**不写缓存**,留给下次重试(缓存占位会让补齐后永不重试,老坑)。
    ///
    /// **跟到底翻页**:单页上限 100,不翻页就是「排在 100 名之后的人永远没有名字」,而飞书
    /// 对此不报错——静默故障。上限 `MAX_MEMBER_PAGES` 页(=1 万人)只为防服务端 token 不收敛
    /// 时把 daemon 线程拖死;真到上限就用已拿到的部分,不整体作废。
    pub fn fetch_and_cache_chat_member_names(
        &self,
        chat_id: &str,
        open_id: &str,
    ) -> Option<String> {
        if chat_id.is_empty() || self.dry_run {
            return None;
        }
        let mut collected = Vec::new();
        let mut page_token: Option<String> = None;
        for _ in 0..MAX_MEMBER_PAGES {
            let url = endpoints::chat_members(&self.base, chat_id, page_token.as_deref());
            let (status, text) = self
                .request_with_token_refresh(|token| {
                    Ok(self.http.get(&url).bearer_auth(token).send()?)
                })
                .ok()?;
            if !status.is_success() {
                // 首页就失败 → 整体放弃(不写缓存,留给下次重试);翻到一半失败 → 用已有的部分,
                // 半份名字也比整群退化成占位强。
                break;
            }
            let Ok(body) = serde_json::from_slice::<serde_json::Value>(&text) else {
                break;
            };
            collected.extend(summon_core::messaging::feishu::parse_chat_members(&body));
            let has_more = body["data"]["has_more"].as_bool().unwrap_or(false);
            let next = body["data"]["page_token"].as_str().unwrap_or_default();
            if !has_more || next.is_empty() {
                break;
            }
            page_token = Some(next.to_string());
        }
        if collected.is_empty() {
            return None;
        }
        let mut cache = self.names.lock().unwrap();
        for member in collected {
            cache.insert(member.open_id, member.name);
        }
        cache.get(open_id).cloned()
    }

    /// 当前应用的定向权限管理入口；应用权限需要管理员授权及版本发布后才生效。
    pub fn mentionless_permission_url(&self, scopes: &[&str]) -> String {
        let mut url = reqwest::Url::parse(&self.permission_console).expect("official console URL");
        url.path_segments_mut()
            .expect("console path")
            .extend(["app", &self.app_id, "auth"]);
        url.query_pairs_mut()
            .append_pair("q", &scopes.join(","))
            .append_pair("op_from", "openapi")
            .append_pair("token_type", "tenant");
        url.into()
    }

    /// 飞书应用所有者。只认企业成员(`owner.type == 2`)且 owner_id 非空,其余返回 `Ok(None)`;
    /// 不认 `creator_id`:应用转让后创建人不再管理这个应用。open_id 与 union_id 各查一次,
    /// 一次认出企业成员、另一次没有时按失败处理,由调用方保留旧值。
    pub fn app_owner(&self) -> anyhow::Result<Option<summon_core::config::registry::AppOwner>> {
        anyhow::ensure!(!self.dry_run, summon_i18n::format!("r36db851a5c"));
        let open_id = self.fetch_app_owner_id(&endpoints::application_info_open_id(
            &self.base,
            &self.app_id,
        ))?;
        let union_id = self.fetch_app_owner_id(&endpoints::application_info_union_id(
            &self.base,
            &self.app_id,
        ))?;
        match (open_id, union_id) {
            (Some(open_id), Some(union_id)) => Ok(Some(summon_core::config::registry::AppOwner {
                open_id,
                union_id,
            })),
            (None, None) => Ok(None),
            _ => anyhow::bail!(summon_i18n::format!("r69a3c846df")),
        }
    }

    fn fetch_app_owner_id(&self, url: &str) -> anyhow::Result<Option<String>> {
        let (status, body) = self.request_with_token_refresh(|token| {
            Ok(self
                .http
                .get(url)
                .bearer_auth(token)
                .timeout(Duration::from_secs(5))
                .send()?)
        })?;
        anyhow::ensure!(status.is_success(), summon_i18n::format!("r80500f2ea2"));
        let response: serde_json::Value = serde_json::from_slice(&body)?;
        anyhow::ensure!(
            response["code"].as_i64() == Some(0),
            summon_i18n::format!("rff3f68f704", response["code"], response["msg"])
        );
        let app = &response["data"]["app"];
        anyhow::ensure!(app.is_object(), summon_i18n::format!("r98b490d52c"));
        let owner = &app["owner"];
        Ok(owner["owner_id"]
            .as_str()
            .filter(|id| owner["type"].as_i64() == Some(2) && !id.is_empty())
            .map(str::to_string))
    }

    /// 只认当前 tenant 的已授权 scope；每种能力只推荐一个最小权限。
    pub fn mentionless_missing_permissions(&self) -> anyhow::Result<Vec<&'static str>> {
        anyhow::ensure!(!self.dry_run, summon_i18n::format!("r52dffdb346"));
        let url = endpoints::application_scopes(&self.base);
        let (status, body) = self.request_with_token_refresh(|token| {
            Ok(self
                .http
                .get(&url)
                .bearer_auth(token)
                .timeout(Duration::from_secs(2))
                .send()?)
        })?;
        anyhow::ensure!(status.is_success(), summon_i18n::format!("r28bf1b1b36"));
        let response: serde_json::Value = serde_json::from_slice(&body)?;
        anyhow::ensure!(
            response["code"].as_i64() == Some(0),
            summon_i18n::format!("r0fa52063ab")
        );
        let scopes = response["data"]["scopes"]
            .as_array()
            .ok_or_else(|| anyhow::anyhow!(summon_i18n::format!("r12ae9c9439")))?;
        let granted = |name: &str| {
            scopes.iter().any(|scope| {
                scope["scope_type"].as_str() == Some("tenant")
                    && scope["scope_name"].as_str() == Some(name)
                    && scope["grant_status"].as_i64() == Some(1)
            })
        };
        Ok(MENTIONLESS_CAPABILITIES
            .iter()
            .filter_map(|alternatives| {
                (!alternatives.iter().any(|name| granted(name))).then_some(alternatives[0])
            })
            .collect())
    }

    fn mentionless_chat_get(&self, url: String) -> anyhow::Result<serde_json::Value> {
        let (status, body) = self.request_with_token_refresh(|token| {
            Ok(self
                .http
                .get(&url)
                .bearer_auth(token)
                .timeout(Duration::from_secs(2))
                .send()?)
        })?;
        anyhow::ensure!(status.is_success(), summon_i18n::format!("r69559b50d4"));
        let response: serde_json::Value = serde_json::from_slice(&body)
            .map_err(|error| anyhow::anyhow!(summon_i18n::format!("re5a02dba05")))?;
        anyhow::ensure!(
            response["code"].as_i64() == Some(0),
            summon_i18n::format!("r9f475e10eb", response["code"], response["msg"])
        );
        Ok(response)
    }

    /// 按群信息的计数判断一人一 Bot。外层错误是查询本身失败(权限、网络、响应),内层错误是
    /// 查到了但群不满足条件 —— 快速配置据此区分「先申请权限」和「这个群不行」。
    pub fn mentionless_chat_topology(&self, chat_id: &str) -> anyhow::Result<Result<(), String>> {
        anyhow::ensure!(
            !self.dry_run && !chat_id.is_empty(),
            summon_i18n::format!("r0e990fb73e")
        );
        let info = self.mentionless_chat_get(endpoints::chat_info(&self.base, chat_id))?;
        let data = &info["data"];
        let (Some(users), Some(bots), Some(mode)) = (
            data["user_count"].as_str(),
            data["bot_count"].as_str(),
            data["chat_mode"].as_str(),
        ) else {
            anyhow::bail!(summon_i18n::format!(
                "r5eed431fec",
                data["user_count"],
                data["bot_count"],
                data["chat_mode"]
            ));
        };
        Ok(
            if users == "1" && bots == "1" && matches!(mode, "group" | "topic") {
                Ok(())
            } else {
                Err(format!(
                    "user_count={users} bot_count={bots} chat_mode={mode}"
                ))
            },
        )
    }

    /// 每条免 @ 消息单独验证；成员接口要求当前 Bot 在群内，并核对唯一真人是发信人。
    pub fn prove_mentionless_chat(&self, chat_id: &str, sender: &str) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.dry_run && !chat_id.is_empty() && !sender.is_empty(),
            summon_i18n::format!("r0e990fb73e")
        );
        self.mentionless_chat_topology(chat_id)?
            .map_err(|facts| anyhow::anyhow!(summon_i18n::format!("r52481437db")))?;
        let members =
            self.mentionless_chat_get(endpoints::chat_members(&self.base, chat_id, None))?;
        let data = &members["data"];
        let items = data["items"].as_array();
        anyhow::ensure!(
            data["has_more"].as_bool() == Some(false)
                && items.is_some_and(|items| items.len() == 1
                    && items[0]["member_id_type"].as_str() == Some("open_id")
                    && items[0]["member_id"].as_str() == Some(sender)),
            summon_i18n::format!(
                "r9bf8ffa8db",
                data["has_more"],
                items.map_or_else(
                    || summon_i18n::text!("r65364e6fd8").to_string(),
                    |items| items.len().to_string()
                ),
                items.map_or(&serde_json::Value::Null, |items| items
                    .first()
                    .map_or(&serde_json::Value::Null, |item| &item["member_id_type"])),
                items.is_some_and(|items| items
                    .iter()
                    .any(|item| item["member_id"].as_str() == Some(sender)))
            )
        );
        Ok(())
    }

    /// 在执行会迁移唯一 session owner 的召回回调前，实时证明本 Bot **此刻仍在目标群内**。
    ///
    /// 不读名字缓存、不把“近期收到过群消息”当可达证明：卡发出后群可能已经解散或移除 Bot，
    /// 对应 ChatGone 事件又可能在 sidecar/daemon 离线窗口丢失。Bot 所在群列表是提交点前唯一
    /// 来自飞书当前态的证据；网络、权限、分页或响应结构任一异常都 fail-closed。
    ///
    /// **问「我在哪些群」而不是「这个群里有谁」**:后者(`chats/{id}/members`)要读别人的成员
    /// 信息,门槛是 `im:chat:readonly` / `im:chat.members:read` 那一档;前者只要 `im:chat:read`,
    /// 恰好是本命题需要的最小权限。群详情(`chats/{id}`)不能替代——真机实测它对 Bot **不在**的
    /// 群同样回 200,只证明群存在,不证明成员关系。
    ///
    /// **判否必须翻到底**:列表分页,只看首页找不到就断言「已退群」会误杀在群多的 Bot;
    /// `has_more` 为真却拿不到下一页游标时同样不能判否——那是分页不收敛,只能报「证不了」。
    pub fn prove_bot_in_chat(&self, chat_id: &str) -> anyhow::Result<()> {
        anyhow::ensure!(!chat_id.is_empty(), summon_i18n::format!("rf419165b2a"));
        anyhow::ensure!(!self.dry_run, summon_i18n::format!("rb6fe4ccdc4"));
        let mut page_token: Option<String> = None;
        for _ in 0..MAX_BOT_CHAT_PAGES {
            let url = endpoints::bot_chats(&self.base, page_token.as_deref());
            let (status, body) = self.request_with_token_refresh(|token| {
                Ok(self.http.get(&url).bearer_auth(token).send()?)
            })?;
            let text = String::from_utf8_lossy(&body);
            anyhow::ensure!(status.is_success(), summon_i18n::format!("r1839da35f6"));
            let response: serde_json::Value = serde_json::from_slice(&body)
                .map_err(|error| anyhow::anyhow!(summon_i18n::format!("ra67952541b")))?;
            anyhow::ensure!(
                response["code"].as_i64() == Some(0),
                summon_i18n::format!("r6470db6b5f")
            );
            let items = response["data"]["items"]
                .as_array()
                .ok_or_else(|| anyhow::anyhow!(summon_i18n::format!("rb16dbde305")))?;
            if items
                .iter()
                .any(|item| item["chat_id"].as_str() == Some(chat_id))
            {
                return Ok(());
            }
            let has_more = response["data"]["has_more"].as_bool().unwrap_or(false);
            if !has_more {
                anyhow::bail!(summon_i18n::format!("r3bc82e2cfd"));
            }
            let next = response["data"]["page_token"]
                .as_str()
                .filter(|token| !token.is_empty())
                .ok_or_else(|| anyhow::anyhow!(summon_i18n::format!("r335718a421")))?;
            page_token = Some(next.to_string());
        }
        anyhow::bail!(summon_i18n::format!("r5c90680c0c"))
    }

    /// 按 union_id 在内存里认人:先在 open_id → union_id 缓存里反查 open_id,再查名字缓存。不联网。
    pub fn cached_name_by_union(&self, union_id: &str) -> Option<String> {
        let open_id = self
            .union_ids
            .lock()
            .unwrap()
            .iter()
            .find(|(_, union)| union.as_str() == union_id)
            .map(|(open_id, _)| open_id.clone())?;
        self.resolve_name_cached(&open_id)
    }

    /// 经通讯录接口查一个人的名字(open_id 或 union_id),查到顺手记进名字缓存。只给控制台把名单里的
    /// ID 显示成人名用:通讯录数据范围外的人会被拒(41050),返回 None,页面照旧显示 ID。
    pub fn fetch_user_name(&self, id: &str) -> Option<String> {
        if self.dry_run || id.is_empty() {
            return None;
        }
        let url = if id.starts_with("on_") {
            endpoints::user_info_by_union(&self.base, id)
        } else {
            endpoints::user_info(&self.base, id)
        };
        let (status, body) = self
            .request_with_token_refresh(|token| {
                Ok(self.http.get(&url).bearer_auth(token).send()?)
            })
            .ok()?;
        if !status.is_success() {
            return None;
        }
        let v: serde_json::Value = serde_json::from_slice(&body).ok()?;
        let user = &v["data"]["user"];
        let name = user["name"].as_str().filter(|s| !s.is_empty())?.to_string();
        if let Some(open_id) = user["open_id"].as_str().filter(|s| !s.is_empty()) {
            self.prime_names([(open_id.to_string(), name.clone())]);
        }
        Some(name)
    }

    /// 把 open_id 解析成 union_id(同开发者下跨 app 稳定;带缓存)。operate 鉴权用:
    /// card.action.trigger 回调只回传 operator 的 open_id,要凭 union_id 比对名单时先来转一道。
    /// dry_run / 失败 / 无 union_id 返回空串(调用方据此退回 open_id 比对)。成功才入缓存。
    pub fn resolve_union_id(&self, open_id: &str) -> String {
        if open_id.is_empty() {
            return String::new();
        }
        if let Some(u) = self.union_ids.lock().unwrap().get(open_id) {
            return u.clone();
        }
        let union = self.fetch_union_id(open_id).unwrap_or_default();
        if !union.is_empty() {
            self.union_ids
                .lock()
                .unwrap()
                .insert(open_id.to_string(), union.clone());
        }
        union
    }

    fn fetch_union_id(&self, open_id: &str) -> anyhow::Result<String> {
        if self.dry_run {
            anyhow::bail!("dry_run");
        }
        let url = endpoints::user_info(&self.base, open_id);
        let (status, body) = self.request_with_token_refresh(|token| {
            Ok(self.http.get(&url).bearer_auth(token).send()?)
        })?;
        let body = String::from_utf8_lossy(&body);
        if !status.is_success() {
            anyhow::bail!(summon_i18n::format!("r2181d626b4"));
        }
        let v: serde_json::Value = serde_json::from_str(&body)?;
        Ok(v["data"]["user"]["union_id"]
            .as_str()
            .unwrap_or("")
            .to_string())
    }

    /// 把群 chat_id 解析成群名(带缓存)。缺权限(im:chat:readonly)/失败/dry_run 回退「群聊」。
    /// 失败不缓存:之后补了权限,下一条群消息即能拿到真名。
    pub fn resolve_chat_name(&self, chat_id: &str) -> String {
        self.resolve_source_group(chat_id)
            .unwrap_or_else(|| summon_i18n::text!("r35b49ee58a").to_owned())
    }

    /// Return a verified name, keeping missing names separate from display fallback text.
    pub fn resolve_source_group(&self, chat_id: &str) -> Option<String> {
        if chat_id.is_empty() {
            return None;
        }
        if let Some(name) = self.chat_names.lock().unwrap().get(chat_id) {
            return Some(name.clone());
        }
        let name = self.fetch_chat_name(chat_id).ok()?;
        self.chat_names
            .lock()
            .unwrap()
            .insert(chat_id.to_owned(), name.clone());
        Some(name)
    }

    fn fetch_chat_name(&self, chat_id: &str) -> anyhow::Result<String> {
        if self.dry_run {
            anyhow::bail!("dry_run");
        }
        let url = endpoints::chat_info(&self.base, chat_id);
        let (status, body) = self.request_with_token_refresh(|token| {
            Ok(self.http.get(&url).bearer_auth(token).send()?)
        })?;
        let body = String::from_utf8_lossy(&body);
        if !status.is_success() {
            anyhow::bail!(summon_i18n::format!("r8cd0e65f8c"));
        }
        let v: serde_json::Value = serde_json::from_str(&body)?;
        let name = v["data"]["name"].as_str().unwrap_or("");
        if name.is_empty() {
            anyhow::bail!(summon_i18n::format!("raa1700105e"));
        }
        Ok(name.to_string())
    }

    /// 委派目标的真实群名和类型；详情只证明群身份，成员关系另行验证。
    pub(crate) fn delegation_group_info(&self, chat_id: &str) -> anyhow::Result<(String, String)> {
        use anyhow::Context as _;
        anyhow::ensure!(!self.dry_run, summon_i18n::format!("r8daa9dbcdd"));
        let url = endpoints::chat_info(&self.base, chat_id);
        let (status, body) = self.request_with_token_refresh(|token| {
            Ok(self.http.get(&url).bearer_auth(token).send()?)
        })?;
        let value: serde_json::Value = serde_json::from_slice(&body)?;
        anyhow::ensure!(
            status.is_success() && value["code"] == 0,
            summon_i18n::format!("r6b2624d4b4")
        );
        let name = value["data"]["name"]
            .as_str()
            .filter(|s| !s.is_empty())
            .context(summon_i18n::text!("r21855a3d08"))?;
        let mode = value["data"]["chat_mode"]
            .as_str()
            .context(summon_i18n::text!("r8a5614f506"))?;
        Ok((name.into(), mode.into()))
    }

    pub(crate) fn groups_named(
        &self,
        name: &str,
    ) -> anyhow::Result<Vec<summon_core::session::group_target::GroupCandidate>> {
        use summon_core::session::group_target::{exact_matches, GroupCandidate};
        let mut page: Option<String> = None;
        let mut seen = std::collections::HashSet::new();
        let mut groups = Vec::new();
        for _ in 0..MAX_BOT_CHAT_PAGES {
            let v = self.mentionless_chat_get(endpoints::bot_chats(&self.base, page.as_deref()))?;
            let items = v["data"]["items"]
                .as_array()
                .ok_or_else(|| anyhow::anyhow!(summon_i18n::format!("rb995601d6b")))?;
            for g in items {
                let id = g["chat_id"]
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!(summon_i18n::format!("r6e693aede8")))?;
                let title = g["name"]
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!(summon_i18n::format!("r4163faeb4d")))?;
                groups.push(GroupCandidate {
                    chat_id: id.into(),
                    name: title.into(),
                    description: g["description"].as_str().unwrap_or_default().into(),
                    member_count: None,
                });
            }
            if v["data"]["has_more"].as_bool() == Some(false) {
                return Ok(exact_matches(name, groups));
            }
            let next = v["data"]["page_token"]
                .as_str()
                .filter(|s| !s.is_empty())
                .ok_or_else(|| anyhow::anyhow!(summon_i18n::format!("rc3e9a9d5f5")))?;
            anyhow::ensure!(
                seen.insert(next.to_string()),
                summon_i18n::format!("r781fab3361")
            );
            page = Some(next.into());
        }
        anyhow::bail!(summon_i18n::format!("rcfecb872ad"))
    }

    pub(crate) fn prove_user_in_chat(&self, chat_id: &str, user: &str) -> anyhow::Result<()> {
        anyhow::ensure!(
            !user.is_empty() && !self.dry_run,
            summon_i18n::format!("r2068240513")
        );
        let mut page: Option<String> = None;
        let mut seen = std::collections::HashSet::new();
        for _ in 0..MAX_MEMBER_PAGES {
            let v = self.mentionless_chat_get(endpoints::chat_members(
                &self.base,
                chat_id,
                page.as_deref(),
            ))?;
            let members = v["data"]["items"]
                .as_array()
                .ok_or_else(|| anyhow::anyhow!(summon_i18n::format!("r537d470683")))?;
            if members
                .iter()
                .any(|m| m["member_id"].as_str() == Some(user))
            {
                return Ok(());
            }
            if v["data"]["has_more"].as_bool() == Some(false) {
                break;
            }
            let next = v["data"]["page_token"]
                .as_str()
                .filter(|s| !s.is_empty())
                .ok_or_else(|| anyhow::anyhow!(summon_i18n::format!("r61c28cced5")))?;
            anyhow::ensure!(
                seen.insert(next.to_string()),
                summon_i18n::format!("ra87f4d328c")
            );
            page = Some(next.into());
        }
        anyhow::bail!(summon_i18n::format!("r71e3d19e5b"))
    }

    /// 发一张无 header 的 markdown 卡片到 chat_id(不自报姓名)。dry_run 打印。
    pub fn send(&self, chat_id: &str, markdown: &str) -> anyhow::Result<()> {
        self.send_with_id(chat_id, markdown).map(|_| ())
    }

    /// 同 [`Self::send`],但把飞书返回的 message_id 带回来。
    ///
    /// **归档需要它**:引用一张 bot 发的卡片时,飞书 API 只回「请升级客户端」占位
    /// (实测:即便带 `card_msg_content_type=raw_card_content` 也一样),真正文只能从我们
    /// 自己的 `outbound.ndjson` 按 message_id 查回来 —— 没有 id 就查不了。
    pub fn send_with_id(&self, chat_id: &str, markdown: &str) -> anyhow::Result<String> {
        if self.dry_run {
            println!("[dry-run] say → chat={chat_id}: {markdown}");
            return Ok("om_dry_run".to_string());
        }
        self.send_card_id(chat_id, &summon_core::feishu::card_markdown_v2(markdown))
    }

    /// 发送已经走过“外部文本转义 + 受信 mention 插入”的 markdown,并带回 message_id。
    /// 类型保证调用方不能把裸 agent 文本误走此路径;包装时不再转义其中合法 `<at>`。
    ///
    /// message_id 供出站归档用,理由见 [`Self::send_with_id`]。
    pub fn send_rendered_markdown_with_id(
        &self,
        chat_id: &str,
        reply_target: Option<(&str, bool)>,
        markdown: &summon_core::feishu::RenderedLarkMd,
    ) -> anyhow::Result<String> {
        if self.dry_run {
            println!(
                "[dry-run] say(rendered) → chat={chat_id}: {}",
                markdown.as_str()
            );
            return Ok("om_dry_run".to_string());
        }
        self.send_card_id_replying(
            chat_id,
            reply_target,
            &summon_core::feishu::card_markdown_v2_rendered(markdown),
        )
    }

    /// 发一张任意 interactive 卡片(content 为飞书卡片 JSON 字符串)。仅供 debug 命令探针。
    #[cfg(debug_assertions)]
    pub fn send_card(&self, chat_id: &str, content: &str) -> anyhow::Result<()> {
        self.send_card_id(chat_id, content).map(|_| ())
    }

    /// 发一张 interactive 卡片并返回飞书响应里的 message_id(供后续 update_card 原地刷新用)。
    /// dry_run 时返回固定假 id,不真发网络请求。`receive_id_type=chat_id`(发进某 chat)。
    pub fn send_card_id(&self, chat_id: &str, content: &str) -> anyhow::Result<String> {
        self.send_card_with_receive_type(chat_id, "chat_id", content, None)
    }

    /// 发卡片,但**可以指定挂进哪条 thread**:`anchor` 是 thread 里那条消息的 id。
    ///
    /// `None`(或空)就是照常发进群主干 —— 这个默认很重要:对不在 thread 里的消息带
    /// `reply_in_thread` 会**新建**一条 thread,把普通对话切成一地话题串。
    pub fn send_card_id_anchored(
        &self,
        chat_id: &str,
        anchor: Option<&str>,
        content: &str,
    ) -> anyhow::Result<String> {
        self.send_card_id_replying(
            chat_id,
            anchor.map(|message_id| (message_id, true)),
            content,
        )
    }

    /// 发卡并回复触发消息。`reply_in_thread` 只反映原消息是否已经在 thread 内；主干消息用
    /// `false` 建立引用回复关系，但不凭空创建新 thread。
    pub fn send_card_id_replying(
        &self,
        chat_id: &str,
        target: Option<(&str, bool)>,
        content: &str,
    ) -> anyhow::Result<String> {
        match target.filter(|(message_id, _)| !message_id.is_empty()) {
            Some((message_id, reply_in_thread)) => {
                self.reply_card_id_with_thread(message_id, content, reply_in_thread)
            }
            None => self.send_card_id(chat_id, content),
        }
    }

    fn reply_card_id_with_thread(
        &self,
        message_id: &str,
        content: &str,
        reply_in_thread: bool,
    ) -> anyhow::Result<String> {
        self.post_reply_card(message_id, content, reply_in_thread, None)
            .or_else(|error| match self.masked_after_rejection(&error, content) {
                Some(masked) => self.post_reply_card(message_id, &masked, reply_in_thread, None),
                None => Err(error),
            })
    }

    fn post_reply_card(
        &self,
        message_id: &str,
        content: &str,
        reply_in_thread: bool,
        uuid: Option<&str>,
    ) -> anyhow::Result<String> {
        if self.dry_run {
            println!("[dry-run] reply_card → {message_id}, in_thread={reply_in_thread}");
            return Ok("dry-msg".to_string());
        }
        let url = endpoints::reply_message(&self.base, message_id);
        let request_body = match uuid {
            Some(uuid) => reply_request_body_with_uuid(content, reply_in_thread, Some(uuid))?,
            None => reply_request_body(content, reply_in_thread)?,
        };
        let (status, body) = self.request_with_token_refresh(|token| {
            Ok(self
                .http
                .post(&url)
                .bearer_auth(token)
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(request_body.clone())
                .send()?)
        })?;
        let body = String::from_utf8_lossy(&body);
        if !status.is_success() {
            return Err(send_failure("reply_card", status, &body));
        }
        let v: serde_json::Value = serde_json::from_str(&body)
            .map_err(|e| anyhow::anyhow!(summon_i18n::format!("r2c3169d522")))?;
        v["data"]["message_id"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .ok_or_else(|| anyhow::anyhow!(summon_i18n::format!("r7662d9edb6")))
    }

    /// Bridge durable outbox 专用：把确定性 id 同时作为飞书官方 `uuid`。同一事件重试时
    /// receive_id/content/uuid 的请求 body 逐字节稳定。
    pub fn send_card_idempotent_id(
        &self,
        chat_id: &str,
        content: &str,
        uuid: &str,
    ) -> anyhow::Result<String> {
        self.send_card_with_receive_type(chat_id, "chat_id", content, Some(uuid))
    }

    /// 冻结的过程补充在原话题内复用同一去重键。
    pub fn send_card_idempotent_anchored(
        &self,
        chat_id: &str,
        anchor: Option<&str>,
        content: &str,
        uuid: &str,
    ) -> anyhow::Result<String> {
        validate_message_uuid(uuid)?;
        match anchor.filter(|anchor| !anchor.is_empty()) {
            Some(anchor) => self.post_reply_card(anchor, content, true, Some(uuid)),
            None => self.send_card_idempotent_id(chat_id, content, uuid),
        }
    }

    /// 会话镜像专用：同一 transcript terminal 重试时复用飞书官方 `uuid`，避免 HTTP 成功但
    /// 本地响应/offset 提交丢失后把同一回复再次发进群。
    /// 会话镜像专用:幂等(uuid)+ 正文是**已做出站 @mention 渲染**的文本(受信 `<at>` 不二次转义)。
    ///
    /// 裸 `&str` 版已删除。`card_markdown` 会把 `<at …>` 当普通外部文本转义掉,`@某bot` 于是以
    /// 纯文字发出 —— 飞书据此认为这条消息**没 @ 任何人**,被 @ 的 bot 的 app 收不到消息事件
    /// (2026-08-12 实测,见 `render_mirror_outbound`)。
    pub fn send_rendered_markdown_idempotent_id(
        &self,
        chat_id: &str,
        markdown: &summon_core::feishu::RenderedLarkMd,
        uuid: &str,
    ) -> anyhow::Result<String> {
        if self.dry_run {
            println!(
                "[dry-run] mirror(rendered) → chat={chat_id}: {}",
                markdown.as_str()
            );
            return Ok("om_dry_run".to_string());
        }
        self.send_card_with_receive_type(
            chat_id,
            "chat_id",
            &summon_core::feishu::card_markdown_v2_rendered(markdown),
            Some(uuid),
        )
    }

    /// 按 open_id 主动私聊发 interactive 卡片并返回 message_id。主动私聊响应不保证提供 p2p
    /// chat_id，因此 bridge ticket 只登记 message_id；后续短码兑换从真实私聊入站取得 chat_id。
    pub fn send_card_to_open_id_id(&self, open_id: &str, content: &str) -> anyhow::Result<String> {
        self.send_card_with_receive_type(open_id, "open_id", content, None)
    }

    /// 按 open_id 私聊发一张 markdown 卡片(`receive_id_type=open_id`)。群没了时通知群主用。
    pub fn send_markdown_to_open_id(&self, open_id: &str, markdown: &str) -> anyhow::Result<()> {
        if self.dry_run {
            println!("[dry-run] say → open_id={open_id}: {markdown}");
            return Ok(());
        }
        self.send_card_with_receive_type(
            open_id,
            "open_id",
            &summon_core::feishu::card_markdown_v2(markdown),
            None,
        )
        .map(|_| ())
    }

    /// 发一张 interactive 卡片到任意 receive_id(`receive_id_type` 决定按 chat_id 还是 open_id 投递),
    /// 返回飞书响应的 message_id。`send_card_id`(chat) 与 `send_markdown_to_open_id`(p2p) 共用它,
    /// 行为对 chat_id 路径逐字节不变。
    fn send_card_with_receive_type(
        &self,
        receive_id: &str,
        receive_id_type: &str,
        content: &str,
        uuid: Option<&str>,
    ) -> anyhow::Result<String> {
        self.send_card_content(receive_id, receive_id_type, content, uuid, false)
    }

    /// 已冻结通知的内容逐字投递，包括没有 uuid 的 NotifyChat。
    pub(crate) fn send_frozen_card_id(
        &self,
        receive_id: &str,
        receive_id_type: &str,
        content: &str,
    ) -> anyhow::Result<String> {
        self.send_card_content(receive_id, receive_id_type, content, None, true)
    }

    fn send_card_content(
        &self,
        receive_id: &str,
        receive_id_type: &str,
        content: &str,
        uuid: Option<&str>,
        frozen: bool,
    ) -> anyhow::Result<String> {
        self.post_card(receive_id, receive_id_type, content, uuid, frozen)
            .or_else(|error| match self.masked_after_rejection(&error, content) {
                // 打码版是另一份内容,幂等键也换一个:同一份打码版重投仍逐字节一致、仍去重。
                Some(masked) => self.post_card(
                    receive_id,
                    receive_id_type,
                    &masked,
                    uuid.map(masked_message_uuid).as_deref(),
                    frozen,
                ),
                None => Err(error),
            })
    }

    fn post_card(
        &self,
        receive_id: &str,
        receive_id_type: &str,
        content: &str,
        uuid: Option<&str>,
        frozen: bool,
    ) -> anyhow::Result<String> {
        if self.dry_run {
            println!("[dry-run] send_card → {receive_id_type}={receive_id}");
            return Ok("dry-msg".to_string());
        }
        let url = endpoints::send_message(&self.base, receive_id_type);
        let request_body = if frozen {
            frozen_card_request_body(receive_id, content, uuid)?
        } else {
            card_request_body(receive_id, content, uuid)?
        };
        let (status, body) = self.request_with_token_refresh(|token| {
            Ok(self
                .http
                .post(&url)
                .bearer_auth(token)
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(request_body.clone())
                .send()?)
        })?;
        let body = String::from_utf8_lossy(&body);
        if !status.is_success() {
            return Err(send_failure("send_card", status, &body));
        }
        let v: serde_json::Value = serde_json::from_str(&body)
            .map_err(|e| anyhow::anyhow!(summon_i18n::format!("r78e32c09be")))?;
        v["data"]["message_id"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .ok_or_else(|| anyhow::anyhow!(summon_i18n::format!("r7f55fa19ee")))
    }

    /// 上传一张图片,拿飞书的 `image_key`。
    ///
    /// `filename` 只是 multipart 的一个字段(飞书按内容判类型),但仍要传得像样,便于对端排查。
    /// **调用前须先过 `summon_core::image::validate`**——格式/大小在本地判掉,不拿飞书的 400
    /// 当校验器:那要等一次完整上传才知道错,还白占带宽。
    pub fn upload_image(&self, bytes: Vec<u8>, filename: &str) -> anyhow::Result<String> {
        if self.dry_run {
            return Ok("img_dry_run".to_string());
        }
        let url = endpoints::upload_image(&self.base);
        let (status, body) = self.request_with_token_refresh(|token| {
            let part = reqwest::blocking::multipart::Part::bytes(bytes.clone())
                .file_name(filename.to_string());
            let form = reqwest::blocking::multipart::Form::new()
                // image_type=message:用于发消息(另一取值 avatar 是设头像,不是这里的用途)。
                .text("image_type", "message")
                .part("image", part);
            Ok(self
                .http
                .post(&url)
                .bearer_auth(token)
                .multipart(form)
                .send()?)
        })?;
        let body = String::from_utf8_lossy(&body);
        if !status.is_success() {
            anyhow::bail!(summon_i18n::format!("rd250341a21"));
        }
        let v: serde_json::Value = serde_json::from_str(&body)
            .map_err(|e| anyhow::anyhow!(summon_i18n::format!("r6b697fb752")))?;
        v["data"]["image_key"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .ok_or_else(|| anyhow::anyhow!(summon_i18n::format!("r2cb3ebd18a")))
    }

    /// 上传一个文件,拿飞书的 `file_key`。`file_type=stream` 是飞书的通用二进制类型。
    pub fn upload_file(&self, bytes: Vec<u8>, file_name: &str) -> anyhow::Result<String> {
        if self.dry_run {
            return Ok("file_dry_run".to_string());
        }
        let url = endpoints::upload_file(&self.base);
        let (status, body) = self.request_with_token_refresh(|token| {
            let part = reqwest::blocking::multipart::Part::bytes(bytes.clone())
                .file_name(file_name.to_string());
            let form = reqwest::blocking::multipart::Form::new()
                .text("file_type", "stream")
                .text("file_name", file_name.to_string())
                .part("file", part);
            Ok(self
                .http
                .post(&url)
                .bearer_auth(token)
                .multipart(form)
                .send()?)
        })?;
        let body = String::from_utf8_lossy(&body);
        if !status.is_success() {
            anyhow::bail!(summon_i18n::format!("r881d98bc9a"));
        }
        let v: serde_json::Value = serde_json::from_str(&body)
            .map_err(|e| anyhow::anyhow!(summon_i18n::format!("r64b48e8e0a")))?;
        v["data"]["file_key"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .ok_or_else(|| anyhow::anyhow!(summon_i18n::format!("rac46635bc5")))
    }

    /// 私聊一条文件消息,返回 message_id。`file_key` 来自 [`Self::upload_file`]。
    pub fn send_file_to_open_id(&self, open_id: &str, file_key: &str) -> anyhow::Result<String> {
        if self.dry_run {
            println!("[dry-run] send_file → open_id={open_id} file_key={file_key}");
            return Ok("dry-msg".to_string());
        }
        let url = endpoints::send_message(&self.base, "open_id");
        let request_body = serde_json::json!({
            "receive_id": open_id,
            "msg_type": "file",
            // 飞书要求 content 是**序列化后的 JSON 字符串**,不是嵌套对象。
            "content": serde_json::json!({"file_key": file_key}).to_string(),
        })
        .to_string();
        let (status, body) = self.request_with_token_refresh(|token| {
            Ok(self
                .http
                .post(&url)
                .bearer_auth(token)
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(request_body.clone())
                .send()?)
        })?;
        let body = String::from_utf8_lossy(&body);
        if !status.is_success() {
            anyhow::bail!(summon_i18n::format!("re04832c595"));
        }
        let v: serde_json::Value = serde_json::from_str(&body)
            .map_err(|e| anyhow::anyhow!(summon_i18n::format!("r0c829d4805")))?;
        v["data"]["message_id"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .ok_or_else(|| anyhow::anyhow!(summon_i18n::format!("r286eb68e1b")))
    }

    /// 发一条图片消息到 chat,返回 message_id。`image_key` 来自 [`Self::upload_image`]。
    pub fn send_image(&self, chat_id: &str, image_key: &str) -> anyhow::Result<String> {
        if self.dry_run {
            println!("[dry-run] send_image → chat={chat_id} image_key={image_key}");
            return Ok("dry-msg".to_string());
        }
        let url = endpoints::send_message(&self.base, "chat_id");
        let request_body = serde_json::json!({
            "receive_id": chat_id,
            "msg_type": "image",
            // 飞书要求 content 是**序列化后的 JSON 字符串**,不是嵌套对象。
            "content": summon_core::image::message_content(image_key),
        })
        .to_string();
        let (status, body) = self.request_with_token_refresh(|token| {
            Ok(self
                .http
                .post(&url)
                .bearer_auth(token)
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(request_body.clone())
                .send()?)
        })?;
        let body = String::from_utf8_lossy(&body);
        if !status.is_success() {
            anyhow::bail!(summon_i18n::format!("r8e9aa190fb"));
        }
        let v: serde_json::Value = serde_json::from_str(&body)
            .map_err(|e| anyhow::anyhow!(summon_i18n::format!("r7bc5c2f770")))?;
        v["data"]["message_id"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .ok_or_else(|| anyhow::anyhow!(summon_i18n::format!("rce780b4830")))
    }

    /// union_id → **本 app 命名空间**的 open_id。agent.yaml 的 allowed_users 常存跨 app 稳定的
    /// union_id(on_),但发消息/建群的 receive/owner 字段只吃本 app open_id——差一步反解就是
    /// 99992361 cross app(2026-08-07 sentinel 确认卡真机踩中)。
    pub fn resolve_open_id_by_union(&self, union_id: &str) -> anyhow::Result<String> {
        if self.dry_run {
            return Ok("dry-open-id".to_string());
        }
        let url = endpoints::user_info_by_union(&self.base, union_id);
        let (status, body) = self.request_with_token_refresh(|token| {
            Ok(self.http.get(&url).bearer_auth(token).send()?)
        })?;
        let body = String::from_utf8_lossy(&body);
        if !status.is_success() {
            anyhow::bail!(summon_i18n::format!("r032ad284df"));
        }
        let v: serde_json::Value = serde_json::from_str(&body)
            .map_err(|e| anyhow::anyhow!(summon_i18n::format!("rfb092f0969")))?;
        v["data"]["user"]["open_id"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .ok_or_else(|| anyhow::anyhow!(summon_i18n::format!("r50651568da")))
    }

    /// 以 bot 身份建群并返回 chat_id。`owner_open_id` 指定群主(sentinel:一律 owner 本人,
    /// 管理员诉求被群主身份覆盖);`user_open_ids`/`bot_app_ids` 建群即邀请。
    /// `im.chats.create` 仅支持 tenant token——user 身份建不了群,这是 API 硬约束(2026-08-07 实测)。
    pub fn create_chat(
        &self,
        name: &str,
        owner_open_id: &str,
        user_open_ids: &[String],
        bot_app_ids: &[String],
    ) -> anyhow::Result<String> {
        if self.dry_run {
            println!("[dry-run] create_chat → name={name} owner={owner_open_id}");
            return Ok("dry-chat".to_string());
        }
        let url = endpoints::create_chat(&self.base);
        let request_body = serde_json::json!({
            "name": name,
            "owner_id": owner_open_id,
            "user_id_list": user_open_ids,
            "bot_id_list": bot_app_ids,
        })
        .to_string();
        let (status, body) = self.request_with_token_refresh(|token| {
            Ok(self
                .http
                .post(&url)
                .bearer_auth(token)
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(request_body.clone())
                .send()?)
        })?;
        let body = String::from_utf8_lossy(&body);
        if !status.is_success() {
            anyhow::bail!(summon_i18n::format!("r40c0b63242"));
        }
        let v: serde_json::Value = serde_json::from_str(&body)
            .map_err(|e| anyhow::anyhow!(summon_i18n::format!("r2d6ba2976b")))?;
        anyhow::ensure!(
            v["code"].as_i64() == Some(0),
            summon_i18n::format!("r4af7f1d519")
        );
        v["data"]["chat_id"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .ok_or_else(|| anyhow::anyhow!(summon_i18n::format!("r236a10b924")))
    }

    /// 下载用户随消息发来的资源(图片/文件/语音/视频)到内存字节。
    /// `kind` 决定 type 参数:`image` → `image`,其余 → `file`(飞书资源 API 只分这两类)。
    pub fn download_resource(
        &self,
        message_id: &str,
        file_key: &str,
        kind: &str,
    ) -> anyhow::Result<Vec<u8>> {
        if self.dry_run {
            anyhow::bail!("dry_run");
        }
        let typ = if kind == "image" { "image" } else { "file" };
        let url = endpoints::message_resource(&self.base, message_id, file_key, typ);
        let (status, body) = self.request_with_token_refresh(|token| {
            Ok(self.http.get(&url).bearer_auth(token).send()?)
        })?;
        if !status.is_success() {
            let body = String::from_utf8_lossy(&body);
            anyhow::bail!(summon_i18n::format!("r0cb482d0cd"));
        }
        Ok(body)
    }

    /// 按 message_id 拉回一条消息(GET im/v1/messages/:message_id),用于「引用回复」时取被引用消息。
    /// 普通消息返回单 item；`merge_forward` 返回全部 nested items，过滤外层固定占位。
    /// dry_run 直接 bail(测试/离线不打网络)。
    pub fn fetch_message(&self, message_id: &str) -> anyhow::Result<FetchedMessage> {
        if self.dry_run {
            anyhow::bail!("dry_run");
        }
        let url = endpoints::fetch_message(&self.base, message_id);
        let (status, body) = self.request_with_token_refresh(|token| {
            Ok(self.http.get(&url).bearer_auth(token).send()?)
        })?;
        let body = String::from_utf8_lossy(&body);
        if !status.is_success() {
            anyhow::bail!(summon_i18n::format!("re23a6a15ff"));
        }
        parse_fetched_message_response(message_id, &body)
    }

    /// 本 agent 所在飞书域的 AppLink 主机,拼桌面端跳转链接用。
    pub fn applink_host(&self) -> &'static str {
        summon_core::waiting::applink_host(&self.base)
    }

    /// 某条消息在会话内的序号(`message_position`),桌面端按它定位。发消息的响应里只有
    /// message_id,序号只能事后用 mget 取;与 `fetch_message` 同一个接口。
    pub fn fetch_message_position(&self, message_id: &str) -> anyhow::Result<Option<String>> {
        if self.dry_run {
            anyhow::bail!("dry_run");
        }
        let url = endpoints::fetch_message(&self.base, message_id);
        let (status, body) = self.request_with_token_refresh(|token| {
            Ok(self.http.get(&url).bearer_auth(token).send()?)
        })?;
        let body = String::from_utf8_lossy(&body);
        if !status.is_success() {
            anyhow::bail!(summon_i18n::format!("ra4f39ee31e"));
        }
        parse_message_position(message_id, &body)
    }

    /// 撤回一条本 bot 自己发的消息(DELETE im/v1/messages/:message_id)。
    ///
    /// 撤回失败(超时窗、权限、消息已不在)通常是终局，调用方自行决定是否降级展示。
    pub fn recall_message(&self, message_id: &str) -> anyhow::Result<()> {
        if self.dry_run {
            println!("[dry-run] recall_message → msg={message_id}");
            return Ok(());
        }
        let url = endpoints::recall_message(&self.base, message_id);
        let (status, body) = self.request_with_token_refresh(|token| {
            Ok(self.http.delete(&url).bearer_auth(token).send()?)
        })?;
        if !status.is_success() {
            let body = String::from_utf8_lossy(&body);
            anyhow::bail!(summon_i18n::format!("r4889298586"));
        }
        let response: serde_json::Value = serde_json::from_slice(&body)?;
        anyhow::ensure!(
            response.get("code").and_then(serde_json::Value::as_i64) == Some(0),
            summon_i18n::format!("r75aebe495d")
        );
        self.recalled_cards
            .lock()
            .map_err(|_| anyhow::anyhow!("recalled card lock poisoned"))?
            .insert(message_id.to_string());
        Ok(())
    }

    /// 创建一条飞书原生 CoT 消息。话题归属由 `origin_message_id` 决定，但不把最终回复折进
    /// CoT 气泡；最终正文仍由 summon 的既有独立消息路径发送。
    pub fn cot_create(
        &self,
        chat_id: &str,
        reply_target: Option<(&str, bool)>,
    ) -> anyhow::Result<CotHandle> {
        if self.dry_run {
            println!("[dry-run] cot_create → chat={chat_id}");
            return Ok(CotHandle {
                cot_id: "dry-cot".to_string(),
                message_id: "dry-cot-msg".to_string(),
            });
        }
        let url = endpoints::cot_create(&self.base);
        let mut payload = serde_json::json!({
            "receive_id": chat_id,
            "reply_in_thread": false,
            "enable_badge": false,
            "update_feed_rank": false,
            "cot_hidden": false,
        });
        if let Some((origin, reply_in_thread)) =
            reply_target.filter(|(message_id, _)| !message_id.is_empty())
        {
            payload["origin_message_id"] = serde_json::Value::String(origin.to_string());
            payload["reply_in_thread"] = serde_json::Value::Bool(reply_in_thread);
        }
        let value = self.cot_call("cot_create", |token| {
            Ok(self
                .http
                .post(&url)
                .bearer_auth(token)
                .json(&payload)
                .send()?)
        })?;
        let cot_id = value["data"]["cot_id"]
            .as_str()
            .filter(|id| !id.is_empty())
            .ok_or_else(|| anyhow::anyhow!(summon_i18n::format!("rc9c94f7c72")))?;
        let message_id = value["data"]["message_id"]
            .as_str()
            .filter(|id| !id.is_empty())
            .ok_or_else(|| anyhow::anyhow!(summon_i18n::format!("r6f22930039")))?;
        Ok(CotHandle {
            cot_id: cot_id.to_string(),
            message_id: message_id.to_string(),
        })
    }

    /// 向 CoT 消息追加 1..=50 条事件。事件只追加、不覆盖，调用方负责串行与节流。
    pub fn cot_push(
        &self,
        cot_id: &str,
        message_id: &str,
        events: &[summon_core::cot::CotEvent],
    ) -> anyhow::Result<()> {
        self.push_cot_events(cot_id, message_id, events)
            .or_else(|error| {
                if !self.masks_after(&error) {
                    return Err(error);
                }
                let masked: Vec<summon_core::cot::CotEvent> = events
                    .iter()
                    .map(|event| summon_core::cot::CotEvent {
                        content: summon_core::email_mask::mask_emails_in_json(&event.content)
                            .unwrap_or_else(|| event.content.clone()),
                        ..event.clone()
                    })
                    .collect();
                if masked == events {
                    return Err(error);
                }
                crate::observe::log::log(summon_i18n::text!("rd8e450c67f"));
                self.push_cot_events(cot_id, message_id, &masked)
            })
    }

    fn push_cot_events(
        &self,
        cot_id: &str,
        message_id: &str,
        events: &[summon_core::cot::CotEvent],
    ) -> anyhow::Result<()> {
        if events.is_empty() || events.len() > 50 {
            anyhow::bail!(summon_i18n::format!("rc91a0a6113", events.len()));
        }
        if let Some(event) = events
            .iter()
            .find(|event| event.content.chars().count() > 4096)
        {
            anyhow::bail!(summon_i18n::format!("r75de3945ae", event.event_type));
        }
        if self.dry_run {
            println!("[dry-run] cot_push → cot={cot_id} events={}", events.len());
            return Ok(());
        }
        let url = endpoints::cot_events(&self.base);
        let payload = serde_json::json!({
            "cot_id": cot_id,
            "message_id": message_id,
            "events": events,
        });
        self.cot_call("cot_push", |token| {
            Ok(self
                .http
                .put(&url)
                .bearer_auth(token)
                .json(&payload)
                .send()?)
        })
        .map(|_| ())
    }

    /// `RUN_FINISHED` 未能写入时的异常兜底。正常完成不调用，避免双重判决。
    pub fn cot_complete(
        &self,
        cot_id: &str,
        message_id: &str,
        reason: CotCompleteReason,
    ) -> anyhow::Result<()> {
        if self.dry_run {
            println!(
                "[dry-run] cot_complete → cot={cot_id} reason={}",
                reason.as_str()
            );
            return Ok(());
        }
        let url = endpoints::cot_complete(&self.base, cot_id, message_id, reason.as_str());
        self.cot_call("cot_complete", |token| {
            Ok(self
                .http
                .post(&url)
                .bearer_auth(token)
                .json(&serde_json::json!({}))
                .send()?)
        })
        .map(|_| ())
    }

    fn cot_call(
        &self,
        operation: &str,
        request: impl FnMut(&str) -> anyhow::Result<reqwest::blocking::Response>,
    ) -> anyhow::Result<serde_json::Value> {
        let (status, body) = self.request_with_token_refresh(request)?;
        let body = String::from_utf8_lossy(&body);
        if !status.is_success() {
            return Err(send_failure(operation, status, &body));
        }
        let value: serde_json::Value = serde_json::from_str(&body)
            .map_err(|e| anyhow::anyhow!(summon_i18n::format!("r493593216d")))?;
        let code = value["code"].as_i64().unwrap_or(0);
        if code == CONTENT_AUDIT_REJECTED_CODE {
            return Err(send_failure(operation, status, &body));
        }
        if code != 0 {
            anyhow::bail!(summon_i18n::format!("r22b7b7ea11"));
        }
        Ok(value)
    }

    /// 原地更新一张已发的 interactive 卡片(PATCH im/v1/messages/:message_id)。
    /// 选项卡片被点选后用它换成「已选」态(撤按钮)。dry_run 打印。
    /// 撞 429(飞书更新卡片 5 QPS/消息频控)退避一次再试(U1)。刚 POST 的 message_id
    /// 偶尔会在 PATCH 侧短暂不可见，精确命中 230001 + invalid message_id 时也只在同一
    /// message_id 上做两次有界退避；绝不因这个窗口改写目标。
    pub fn update_card(&self, message_id: &str, content: &str) -> anyhow::Result<()> {
        // 撤回只影响卡片展示；流式任务继续收尾，不把更新失败当成补发新卡的理由。
        if self
            .recalled_cards
            .lock()
            .map_err(|_| anyhow::anyhow!("recalled card lock poisoned"))?
            .contains(message_id)
        {
            return Ok(());
        }
        let result = self.patch_card(message_id, content).or_else(|error| {
            match self.masked_after_rejection(&error, content) {
                Some(masked) => self.patch_card(message_id, &masked),
                None => Err(error),
            }
        });
        if result.is_err()
            && self
                .recalled_cards
                .lock()
                .map_err(|_| anyhow::anyhow!("recalled card lock poisoned"))?
                .contains(message_id)
        {
            return Ok(());
        }
        result
    }

    fn patch_card(&self, message_id: &str, content: &str) -> anyhow::Result<()> {
        if self.dry_run {
            println!("[dry-run] update_card → msg={message_id}");
            return Ok(());
        }
        let url = endpoints::update_card(&self.base, message_id);
        // PATCH 没有幂等键;补的预览只取决于卡片内容,同一张卡重投仍逐字节一致。
        let content = with_feed_preview(content);
        let content = content.as_ref();
        let mut rate_limit_retried = false;
        let mut invalid_message_id_retries = 0;
        let mut card_locked_retries = 0;
        loop {
            let (status, body) = self.request_with_token_refresh(|token| {
                Ok(self
                    .http
                    .patch(&url)
                    .bearer_auth(token)
                    .json(&serde_json::json!({ "content": content }))
                    .send()?)
            })?;
            if status.is_success() {
                return Ok(());
            }
            let body = String::from_utf8_lossy(&body);
            if status.as_u16() == 429 && !rate_limit_retried {
                rate_limit_retried = true;
                std::thread::sleep(std::time::Duration::from_millis(600));
                continue;
            }
            if is_transient_invalid_message_id(status, &body)
                && invalid_message_id_retries < INVALID_MESSAGE_ID_RETRY_DELAYS.len()
            {
                let delay = INVALID_MESSAGE_ID_RETRY_DELAYS[invalid_message_id_retries];
                invalid_message_id_retries += 1;
                std::thread::sleep(delay);
                continue;
            }
            // 卡片动作锁:等 sidecar 那次回调 ACK 完成即释放,退避重试即可(见
            // `is_card_action_locked` 的说明)。**不重试的后果是「点了没反应」**——handler 里
            // 换卡失败只 log,用户只看到卡片一动不动,于是反复点。
            if is_card_action_locked(status, &body)
                && card_locked_retries < CARD_LOCKED_RETRY_DELAYS.len()
            {
                let delay = CARD_LOCKED_RETRY_DELAYS[card_locked_retries];
                card_locked_retries += 1;
                std::thread::sleep(delay);
                continue;
            }
            return Err(send_failure("update_card", status, &body));
        }
    }
}

fn parse_message_position(message_id: &str, body: &str) -> anyhow::Result<Option<String>> {
    let v: serde_json::Value = serde_json::from_str(body)?;
    // 容器名与 `parse_fetched_message_response` 一样两种都认。
    let Some(items) = v["data"]["items"]
        .as_array()
        .or_else(|| v["data"]["messages"].as_array())
    else {
        return Ok(None);
    };
    Ok(items
        .iter()
        .find(|item| item["message_id"].as_str() == Some(message_id))
        .and_then(|item| match &item["message_position"] {
            serde_json::Value::String(s) if !s.is_empty() => Some(s.clone()),
            serde_json::Value::Number(n) => Some(n.to_string()),
            _ => None,
        }))
}

fn parse_fetched_message_response(message_id: &str, body: &str) -> anyhow::Result<FetchedMessage> {
    let v: serde_json::Value = serde_json::from_str(body)?;
    // 容器名两种都认:单条 `messages/{id}` 回 `data.items`,`mget` 按文档也是 `items`,但不同
    // 网关版本上见过 `messages`。判错的代价是整条引用上下文静默丢失,不值得为省一行赌字段名。
    let items = v["data"]["items"]
        .as_array()
        .or_else(|| v["data"]["messages"].as_array())
        .filter(|items| !items.is_empty())
        .ok_or_else(|| anyhow::anyhow!(summon_i18n::format!("r1b1be21a7b")))?;

    // 飞书可能返回「外层 merge_forward + nested items」，也可能只返回 nested items。
    // `upper_message_id` 指向本次请求的外层消息，是比数组顺序更稳定的判据。
    let outer = items.iter().find(|item| {
        item["msg_type"].as_str() == Some("merge_forward")
            && item["upper_message_id"]
                .as_str()
                .unwrap_or_default()
                .is_empty()
    });
    let nested = items
        .iter()
        .filter(|item| {
            let upper = item["upper_message_id"].as_str().unwrap_or_default();
            !upper.is_empty() && (upper == message_id || outer.is_some())
        })
        .map(fetched_message_item)
        .collect::<Vec<_>>();

    // **判据是「有没有 nested」,不是「是不是 merge_forward」。**
    //
    // 2026-08-12 线上回归:`fetch_message` 改走 mget 之后,mget 对 merge_forward **不返回
    // nested items 数组** —— 它只回外层那一条,把转发内容直接渲染进 `body.content`
    // (`<forwarded_messages> [时间] 某人: …`)。旧判据 `outer.is_some() || !nested.is_empty()`
    // 于是命中 outer 分支、拿着空 nested 返回 `MergeForward(vec![])`,引用回文渲染成
    // 「📎 空的转发消息」,owner 实测转发里的两张卡片一个字都没进 agent 上下文。
    //
    // 单条 get 会展开 nested,mget 不会,但 mget 的 content 本身就是展开好的全文 —— 两条路
    // 各自完整,只是形状不同。所以:有 nested 才走 MergeForward,没有就把外层当单条消息,
    // 正文自然是那段 `<forwarded_messages>` 全文。
    //
    // 这个回归没被单测挡住,因为那两个 merge_forward 测试喂的是**手写的 mock 响应**(带
    // nested 的旧形状),换接口后 mock 没跟着换 —— mock 掩盖了真实 API 的形状差异。
    if !nested.is_empty() {
        return Ok(FetchedMessage {
            sender_open_id: outer.map(sender_open_id).unwrap_or_default(),
            content: FetchedMessageContent::MergeForward(nested),
        });
    }

    let item = outer.unwrap_or(&items[0]);
    Ok(FetchedMessage {
        sender_open_id: sender_open_id(item),
        content: FetchedMessageContent::Single(fetched_message_item(item)),
    })
}

/// 取发送人的 **open_id**。bot 发的消息 `sender.id` 是 `cli_…`(`id_type: app_id`),真正的
/// open_id 在 `sender.open_bot_id` —— 2026-08-12 真机 mget 确证。
///
/// 为什么要紧:上层拿它和 `bot_open_id()`(`ou_…`)比对判「是不是自己发的」,拿 app_id 去比
/// **永远不相等**,于是引用自己的卡片时走了「别人的消息」分支、把整段正文又灌一遍进 prompt
/// —— owner 2026-08-10 定的省 token 规则对卡片一直是失效的。
fn sender_open_id(item: &serde_json::Value) -> String {
    item["sender"]["open_bot_id"]
        .as_str()
        .filter(|id| !id.is_empty())
        .or_else(|| item["sender"]["id"].as_str())
        .unwrap_or_default()
        .to_string()
}

fn fetched_message_item(item: &serde_json::Value) -> FetchedMessageItem {
    FetchedMessageItem {
        sender_id: item["sender"]["id"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
        sender_type: item["sender"]["sender_type"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
        msg_type: item["msg_type"].as_str().unwrap_or_default().to_string(),
        mentions: item["mentions"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|m| {
                        let key = m["key"].as_str()?;
                        Some((
                            key.to_string(),
                            m["name"].as_str().unwrap_or("").to_string(),
                        ))
                    })
                    .collect()
            })
            .unwrap_or_default(),
        content: item["body"]["content"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
    }
}

/// 回复消息的请求体。原消息已在 thread 内时传 `true` 继续留在原 thread；主干消息传
/// `false` 只建立引用关系，避免凭空创建新 thread。
fn reply_request_body(content: &str, reply_in_thread: bool) -> anyhow::Result<Vec<u8>> {
    reply_request_body_with_uuid(content, reply_in_thread, None)
}

fn reply_request_body_with_uuid(
    content: &str,
    reply_in_thread: bool,
    uuid: Option<&str>,
) -> anyhow::Result<Vec<u8>> {
    if let Some(uuid) = uuid {
        validate_message_uuid(uuid)?;
    }
    let content = if uuid.is_some() {
        std::borrow::Cow::Borrowed(content)
    } else {
        with_feed_preview(content)
    };
    let content = content.as_ref();
    #[derive(serde::Serialize)]
    struct Request<'a> {
        content: &'a str,
        msg_type: &'static str,
        reply_in_thread: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        uuid: Option<&'a str>,
    }
    Ok(serde_json::to_vec(&Request {
        content,
        msg_type: "interactive",
        reply_in_thread,
        uuid,
    })?)
}

/// 发出与改写的每张卡都带上会话列表预览,见 [`summon_core::feishu::card_with_derived_feed_summary`]。
fn with_feed_preview(content: &str) -> std::borrow::Cow<'_, str> {
    match summon_core::feishu::card_with_derived_feed_summary(content) {
        Some(preview) => std::borrow::Cow::Owned(preview),
        None => std::borrow::Cow::Borrowed(content),
    }
}

fn card_request_body(
    receive_id: &str,
    content: &str,
    uuid: Option<&str>,
) -> anyhow::Result<Vec<u8>> {
    if let Some(uuid) = uuid {
        validate_message_uuid(uuid)?;
    }
    // 带 uuid 的是 outbox 冻结的线上字节:生产时定稿,靠 uuid 让重投逐字节一致,投递这一步不改写。
    let content = match uuid {
        Some(_) => std::borrow::Cow::Borrowed(content),
        None => with_feed_preview(content),
    };
    frozen_card_request_body(receive_id, content.as_ref(), uuid)
}

fn frozen_card_request_body(
    receive_id: &str,
    content: &str,
    uuid: Option<&str>,
) -> anyhow::Result<Vec<u8>> {
    if let Some(uuid) = uuid {
        validate_message_uuid(uuid)?;
    }
    #[derive(serde::Serialize)]
    struct Request<'a> {
        receive_id: &'a str,
        msg_type: &'static str,
        content: &'a str,
        #[serde(skip_serializing_if = "Option::is_none")]
        uuid: Option<&'a str>,
    }
    Ok(serde_json::to_vec(&Request {
        receive_id,
        msg_type: "interactive",
        content,
        uuid,
    })?)
}

/// 打码重发用的幂等键:原键加 `-m`;放不下飞书 50 字符上限时改用原键的摘要,不同原键不会
/// 撞到同一个派生键。
fn masked_message_uuid(uuid: &str) -> String {
    if uuid.len() + 2 <= 50 {
        return format!("{uuid}-m");
    }
    let digest = ring::digest::digest(&ring::digest::SHA256, uuid.as_bytes());
    let hex: String = digest.as_ref()[..16]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("summon-masked-{hex}")
}

fn validate_message_uuid(uuid: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        !uuid.is_empty()
            && uuid.len() <= 50
            && uuid
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')),
        summon_i18n::format!("r3a1ec21f58")
    );
    Ok(())
}

#[cfg(test)]
mod tests {

    /// 假审核:请求里带原文邮箱就按 230028 拒,打码后的放行。打码重发覆盖发卡、带幂等键的
    /// 发卡(会话镜像、outbox)、回复、改卡、CoT 推送;开关关着时只报被拒、不重发。
    #[test]
    fn recall_during_inflight_patch_is_suppressed() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let client = std::sync::Arc::new(FeishuClient::with_test_base(format!(
            "http://{}",
            server.server_addr().to_ip().unwrap()
        )));
        let (patch_seen_tx, patch_seen_rx) = std::sync::mpsc::channel();
        let (recall_done_tx, recall_done_rx) = std::sync::mpsc::channel();
        let remote = std::thread::spawn(move || {
            let token = server.recv().unwrap();
            token
                .respond(tiny_http::Response::from_string(
                    r#"{"tenant_access_token":"token","expire":7200}"#,
                ))
                .unwrap();
            let patch = server.recv().unwrap();
            assert_eq!(patch.method(), &tiny_http::Method::Patch);
            patch_seen_tx.send(()).unwrap();
            let recall = server.recv().unwrap();
            assert_eq!(recall.method(), &tiny_http::Method::Delete);
            recall
                .respond(tiny_http::Response::from_string(r#"{"code":0}"#))
                .unwrap();
            recall_done_rx.recv().unwrap();
            patch
                .respond(
                    tiny_http::Response::from_string(r#"{"code":230011,"msg":"message recalled"}"#)
                        .with_status_code(400),
                )
                .unwrap();
        });
        let patch_client = client.clone();
        let patch = std::thread::spawn(move || {
            patch_client.update_card("om_race", &summon_core::feishu::card_markdown_v2("正文"))
        });
        patch_seen_rx.recv().unwrap();
        client.recall_message("om_race").unwrap();
        recall_done_tx.send(()).unwrap();
        let result = patch.join().unwrap();
        remote.join().unwrap();
        assert!(
            result.is_ok(),
            "recalled card patch must not trigger fallback: {result:?}"
        );
    }

    #[test]
    fn frozen_cards_keep_original_content_in_group_and_private_delivery() {
        let old =
            r#"{ "schema":"2.0", "body":{"elements":[{"tag":"markdown","content":"旧通知"}]} }"#;
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let client = FeishuClient::with_test_base(format!(
            "http://{}",
            server.server_addr().to_ip().unwrap()
        ));
        let expected = old.to_string();
        let remote = std::thread::spawn(move || {
            let token = server
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .unwrap();
            token
                .respond(tiny_http::Response::from_string(
                    r#"{"tenant_access_token":"token","expire":7200}"#,
                ))
                .unwrap();
            for receive_type in ["chat_id", "open_id"] {
                let mut request = server
                    .recv_timeout(Duration::from_secs(2))
                    .unwrap()
                    .unwrap();
                assert!(request.url().ends_with(receive_type));
                let mut body = String::new();
                request.as_reader().read_to_string(&mut body).unwrap();
                let body: serde_json::Value = serde_json::from_str(&body).unwrap();
                assert_eq!(body["content"], expected);
                assert!(body.get("uuid").is_none());
                request
                    .respond(tiny_http::Response::from_string(
                        r#"{"data":{"message_id":"om_frozen"}}"#,
                    ))
                    .unwrap();
            }
        });
        for receive_type in ["chat_id", "open_id"] {
            client
                .send_frozen_card_id("target", receive_type, old)
                .unwrap();
        }
        remote.join().unwrap();
    }

    #[test]
    fn recalling_a_card_suppresses_its_later_updates_only_after_success() {
        for (response, accepted) in [(r#"{"code":0}"#, true), (r#"{"code":230006}"#, false)] {
            let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
            let base = format!("http://{}", server.server_addr().to_ip().unwrap());
            let thread = std::thread::spawn(move || {
                let token = server
                    .recv_timeout(Duration::from_secs(2))
                    .unwrap()
                    .unwrap();
                token
                    .respond(tiny_http::Response::from_string(
                        r#"{"tenant_access_token":"token","expire":7200}"#,
                    ))
                    .unwrap();
                let recall = server
                    .recv_timeout(Duration::from_secs(2))
                    .unwrap()
                    .unwrap();
                assert_eq!(recall.method(), &tiny_http::Method::Delete);
                assert!(recall.url().ends_with("/om_close"));
                recall
                    .respond(tiny_http::Response::from_string(response))
                    .unwrap();
                // 失败后的原卡和未撤回的另一张卡照常可更新。
                let count = if accepted { 1 } else { 2 };
                for _ in 0..count {
                    let patch = server
                        .recv_timeout(Duration::from_secs(2))
                        .unwrap()
                        .unwrap();
                    assert_eq!(patch.method(), &tiny_http::Method::Patch);
                    if accepted {
                        assert!(patch.url().ends_with("/om_other"));
                    }
                    patch
                        .respond(tiny_http::Response::from_string(r#"{"code":0}"#))
                        .unwrap();
                }
            });
            let client = FeishuClient::with_test_base(base);
            assert_eq!(client.recall_message("om_close").is_ok(), accepted);
            let card = summon_core::feishu::card_markdown_v2("任务继续执行");
            client.update_card("om_close", &card).unwrap();
            client.update_card("om_other", &card).unwrap();
            thread.join().unwrap();
        }
    }

    #[test]
    fn email_rejections_are_resent_masked_on_every_outbound_path() {
        let server = std::sync::Arc::new(tiny_http::Server::http("127.0.0.1:0").unwrap());
        let base = format!("http://{}", server.server_addr().to_ip().unwrap());
        let seen: std::sync::Arc<std::sync::Mutex<Vec<String>>> = Default::default();
        let recorded = seen.clone();
        let serving = std::sync::Arc::clone(&server);
        let server_thread = std::thread::spawn(move || {
            // 客户端调用全是同步的:主线程做完就 unblock 收工,2s 只是失败时的兜底上限。
            while let Ok(Some(mut request)) = serving.recv_timeout(Duration::from_secs(2)) {
                let mut body = String::new();
                request.as_reader().read_to_string(&mut body).unwrap();
                let (status, response) = if request.url().contains("tenant_access_token") {
                    (200, r#"{"tenant_access_token":"token","expire":7200}"#)
                } else if body.contains("probe.user@example.com") {
                    // CoT 接口以 200 + 业务码报拒,消息接口以 400 报拒,两种都要认。
                    let status = if body.contains("\"cot_id\"") {
                        200
                    } else {
                        400
                    };
                    recorded.lock().unwrap().push(body);
                    (
                        status,
                        r#"{"code":230028,"msg":"The messages do NOT pass the audit, ext=contain sensitive data: EMAIL_ADDRESS"}"#,
                    )
                } else {
                    recorded.lock().unwrap().push(body);
                    (200, r#"{"code":0,"data":{"message_id":"om_1"}}"#)
                };
                request
                    .respond(tiny_http::Response::from_string(response).with_status_code(status))
                    .unwrap();
            }
        });
        let on = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let switch = on.clone();
        let client =
            FeishuClient::with_test_base(base).with_email_mask(std::sync::Arc::new(move || {
                switch.load(std::sync::atomic::Ordering::SeqCst)
            }));
        let card = summon_core::feishu::card_markdown_v2("联系 probe.user@example.com");
        assert_eq!(client.send_card_id("oc_1", &card).unwrap(), "om_1");
        client
            .send_card_idempotent_id("oc_1", &card, "summon-mirror-0123")
            .unwrap();
        client
            .send_card_id_replying("oc_1", Some(("om_origin", false)), &card)
            .unwrap();
        client.update_card("om_1", &card).unwrap();
        let events = [summon_core::cot::CotEvent {
            event_type: "TEXT_MESSAGE_CONTENT".to_string(),
            content: r#"{"delta":"probe.user@example.com"}"#.to_string(),
            timestamp: "1720000000123".to_string(),
            reply_delivery: None,
        }];
        client.cot_push("cot_1", "om_1", &events).unwrap();
        on.store(false, std::sync::atomic::Ordering::SeqCst);
        let error = client.send_card_id("oc_1", &card).unwrap_err();
        assert!(
            error.downcast_ref::<ContentRejected>().is_some(),
            "{error:#}"
        );
        server.unblock();
        server_thread.join().unwrap();

        let bodies = seen.lock().unwrap();
        assert_eq!(
            bodies.len(),
            11,
            "五条路径各拒一次、重发一次,关掉后只拒不重发"
        );
        for pair in bodies[..10].chunks(2) {
            assert!(pair[0].contains("probe.user@example.com"), "{}", pair[0]);
            assert!(pair[1].contains("p***@example.com"), "{}", pair[1]);
            assert!(!pair[1].contains("probe.user@example.com"), "{}", pair[1]);
        }
        assert!(
            bodies[2].contains(r#""uuid":"summon-mirror-0123""#),
            "{}",
            bodies[2]
        );
        assert!(
            bodies[3].contains(r#""uuid":"summon-mirror-0123-m""#),
            "{}",
            bodies[3]
        );
    }

    /// 不是因为邮箱被拒的,打码也救不了,不重发。
    #[test]
    fn only_email_rejections_are_masked() {
        let client = FeishuClient::new("feishu.cn", "app".into(), "secret".into(), true)
            .with_email_mask(std::sync::Arc::new(|| true));
        let card = summon_core::feishu::card_markdown_v2("probe.user@example.com");
        let other = anyhow::Error::new(ContentRejected::for_test(
            "contain sensitive data: PHONE_NUMBER",
        ));
        assert_eq!(client.masked_after_rejection(&other, &card), None);
        let email = anyhow::Error::new(ContentRejected::for_test(
            "contain sensitive data: EMAIL_ADDRESS",
        ));
        assert!(client.masked_after_rejection(&email, &card).is_some());
        let button = r#"{"behaviors":[{"value":{"to":"probe.user@example.com"}}],"text":"probe.user@example.com"}"#;
        let masked = client.masked_after_rejection(&email, button).unwrap();
        assert!(
            masked.contains(r#""to":"probe.user@example.com""#),
            "回调值原样回传: {masked}"
        );
        assert!(masked.contains(r#""text":"p***@example.com""#), "{masked}");
        assert_eq!(
            masked_message_uuid("summon-mirror-0123"),
            "summon-mirror-0123-m"
        );
        let long_a = format!("{}a", "x".repeat(49));
        let long_b = format!("{}b", "x".repeat(49));
        assert_ne!(masked_message_uuid(&long_a), masked_message_uuid(&long_b));
        assert!(validate_message_uuid(&masked_message_uuid(&long_a)).is_ok());
    }

    #[test]
    fn cot_timeout_reason_has_a_distinct_wire_value() {
        assert_eq!(CotCompleteReason::Timeout.as_str(), "timeout");
    }

    #[test]
    fn cot_requests_keep_event_content_and_timestamp_as_strings() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let base = format!("http://{}", server.server_addr().to_ip().unwrap());
        let seen: std::sync::Arc<std::sync::Mutex<Vec<(String, String, String)>>> =
            Default::default();
        let recorded = seen.clone();
        let server_thread = std::thread::spawn(move || {
            for index in 0..4 {
                let mut request = server
                    .recv_timeout(Duration::from_secs(2))
                    .unwrap()
                    .expect("CoT 调用应逐个到达");
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
                recorded.lock().unwrap().push((
                    request.method().as_str().to_string(),
                    request.url().to_string(),
                    body,
                ));
                let response = if index == 1 {
                    r#"{"code":0,"data":{"cot_id":"cot_1","message_id":"om_1"}}"#
                } else {
                    r#"{"code":0,"data":{}}"#
                };
                request
                    .respond(tiny_http::Response::from_string(response))
                    .unwrap();
            }
        });

        let client = FeishuClient::with_test_base(base);
        let handle = client
            .cot_create("oc_1", Some(("om_origin", true)))
            .unwrap();
        let events = [summon_core::cot::CotEvent {
            event_type: "RUN_STARTED".to_string(),
            content: r#"{"runId":"turn-1"}"#.to_string(),
            timestamp: "1720000000123".to_string(),
            reply_delivery: None,
        }];
        client
            .cot_push(&handle.cot_id, &handle.message_id, &events)
            .unwrap();
        client
            .cot_complete(&handle.cot_id, &handle.message_id, CotCompleteReason::Error)
            .unwrap();
        server_thread.join().unwrap();

        let calls = seen.lock().unwrap();
        let (_, create_url, create_body) = &calls[0];
        assert!(create_url.ends_with("/message_cot?receive_id_type=chat_id"));
        let create: serde_json::Value = serde_json::from_str(create_body).unwrap();
        assert_eq!(create["origin_message_id"], "om_origin");
        assert_eq!(create["reply_in_thread"], true);

        let (method, event_url, event_body) = &calls[1];
        assert_eq!(method, "PUT");
        assert!(event_url.ends_with("/message_cot"));
        let event: serde_json::Value = serde_json::from_str(event_body).unwrap();
        assert!(event["events"][0]["content"].is_string());
        assert!(event["events"][0]["timestamp"].is_string());

        let (method, complete_url, complete_body) = &calls[2];
        assert_eq!(method, "POST");
        assert!(complete_url.ends_with("/complete/cot_1?message_id=om_1&reason=error"));
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(complete_body).unwrap(),
            serde_json::json!({})
        );
    }

    use super::*;

    /// mget 对 merge_forward 的**真实形状**:只回外层一条、没有 nested items,转发内容直接
    /// 渲染在 `body.content` 里。
    ///
    /// 2026-08-12 线上回归的回归测试:旧判据 `outer.is_some() || !nested.is_empty()` 会命中
    /// outer、拿空 nested 返回 `MergeForward(vec![])`,引用回文渲染成「📎 空的转发消息」——
    /// 转发里的卡片一个字都进不了 agent 上下文。夹具照 lark-cli 取到的真机响应写,**不是手搓
    /// 的理想形状**:上一版那两个 merge_forward 测试正是因为 mock 停在旧形状,换接口后照样绿。
    #[test]
    fn mget_merge_forward_without_nested_keeps_the_forwarded_body() {
        let body = r#"{
            "code": 0,
            "data": {
                "items": [
                    {
                        "message_id": "om_merge",
                        "msg_type": "merge_forward",
                        "sender": { "id": "ou_boss", "sender_type": "user" },
                        "body": { "content": "<forwarded_messages> [2026-08-12T18:19:36+08:00] 炼金师: 复验完，前两轮问题都修了 </forwarded_messages>" }
                    }
                ]
            }
        }"#;
        let got = parse_fetched_message_response("om_merge", body).expect("应能解析");
        assert_eq!(got.sender_open_id, "ou_boss");
        match &got.content {
            FetchedMessageContent::Single(item) => {
                assert!(
                    item.content.contains("炼金师") && item.content.contains("复验完"),
                    "转发正文必须留在 Single 里,实得 {}",
                    item.content
                );
            }
            FetchedMessageContent::MergeForward(nested) => panic!(
                "没有 nested 时不能返回 MergeForward(那会渲染成「空的转发消息」),实得 {} 条",
                nested.len()
            ),
        }
    }

    /// 对照:单条 get 会展开 nested,那条路必须**照旧**走 MergeForward,别被上面的修法带偏。
    #[test]
    fn nested_items_still_take_the_merge_forward_path() {
        let body = r#"{
            "code": 0,
            "data": {
                "items": [
                    {
                        "message_id": "om_merge",
                        "msg_type": "merge_forward",
                        "sender": { "id": "ou_boss", "sender_type": "user" },
                        "body": { "content": "" }
                    },
                    {
                        "message_id": "om_child",
                        "upper_message_id": "om_merge",
                        "msg_type": "text",
                        "sender": { "id": "ou_a", "sender_type": "user" },
                        "body": { "content": "{\"text\":\"子消息\"}" }
                    }
                ]
            }
        }"#;
        let got = parse_fetched_message_response("om_merge", body).expect("应能解析");
        match &got.content {
            FetchedMessageContent::MergeForward(nested) => assert_eq!(nested.len(), 1),
            FetchedMessageContent::Single(_) => panic!("有 nested 时必须走 MergeForward"),
        }
    }

    fn dry_client() -> FeishuClient {
        FeishuClient::new("feishu.cn", "app".into(), "secret".into(), true)
    }

    /// 卡片动作锁必须被认成**瞬态**并退避重试。
    ///
    /// 真机复现(2026-08-03 某个群):`schedule_pick` 是纯渲染动作、handler 里零 IO,PATCH
    /// 跑在 sidecar 那次回调 ACK 之前,连续 5 次全撞锁 → 换卡失败 → 用户看到「点了没反应」,
    /// 于是反复点。判据用飞书原样返回的 body 钉死。
    #[test]
    fn card_action_lock_is_treated_as_transient() {
        let locked = r#"{"code":230099,"msg":"Failed to create card content, ext=ErrCode: 11310; ErrMsg: card action is lock; ","error":{"log_id":"202608031735519832C12BA121EDB6BBEC"}}"#;
        assert!(is_card_action_locked(
            reqwest::StatusCode::BAD_REQUEST,
            locked
        ));
        // 只认 400:5xx 另有重试语义,不该混进来。
        assert!(!is_card_action_locked(
            reqwest::StatusCode::INTERNAL_SERVER_ERROR,
            locked
        ));
        // 别的 400 不能被误判成锁——否则真正的坏请求会被白白重试三次再报同样的错。
        let invalid_id = r#"{"code":230001,"msg":"invalid message_id"}"#;
        assert!(!is_card_action_locked(
            reqwest::StatusCode::BAD_REQUEST,
            invalid_id
        ));
        assert!(!is_card_action_locked(
            reqwest::StatusCode::BAD_REQUEST,
            r#"{"code":230099,"msg":"Failed to create card content"}"#
        ));
        assert!(!is_card_action_locked(
            reqwest::StatusCode::BAD_REQUEST,
            "not json"
        ));
        // 两个瞬态判据互不重叠:各自只认自己的 code。
        assert!(is_transient_invalid_message_id(
            reqwest::StatusCode::BAD_REQUEST,
            invalid_id
        ));
        assert!(!is_transient_invalid_message_id(
            reqwest::StatusCode::BAD_REQUEST,
            locked
        ));
    }

    #[test]
    fn bot_neg_cache_blocks_within_window_only() {
        // R4:bot info 失败后的负缓存——窗口内不重试(快 Err),窗口外/无失败记录放行。
        let w = Duration::from_secs(60);
        assert!(!bot_neg_cache_active(None, w), "无失败记录应放行");
        assert!(
            bot_neg_cache_active(Some(Instant::now()), w),
            "刚失败应被负缓存挡下"
        );
        let old = Instant::now().checked_sub(Duration::from_secs(120));
        assert!(
            !bot_neg_cache_active(old, w),
            "失败已过窗(2min 前)应放行重试"
        );
    }

    #[test]
    fn resolve_name_cached_returns_none_when_uncached() {
        let fc = dry_client();
        // 缓存空时应返回 None,不触发网络
        assert_eq!(fc.resolve_name_cached("ou_abc"), None);
        // empty open_id → None
        assert_eq!(fc.resolve_name_cached(""), None);
    }

    #[test]
    fn resolve_name_cached_returns_some_after_manual_insert() {
        let fc = dry_client();
        // 模拟缓存命中(其它路径成功 resolve 后写入缓存)
        fc.names
            .lock()
            .unwrap()
            .insert("ou_xyz".to_string(), "张三".to_string());
        assert_eq!(fc.resolve_name_cached("ou_xyz"), Some("张三".to_string()));
    }

    /// `reply_in_thread` 少了它,回复会挂在群主干上 —— 看起来像答非所问地另起一句。
    #[test]
    fn reply_body_asks_to_stay_in_thread() {
        let body = super::reply_request_body("{\"card\":1}", true).expect("应能构造");
        let v: serde_json::Value = serde_json::from_slice(&body).expect("应是 JSON");
        assert_eq!(v["reply_in_thread"], serde_json::json!(true));
        assert_eq!(v["msg_type"], "interactive");
        assert_eq!(v["content"], "{\"card\":1}");
    }

    #[test]
    fn main_feed_reply_does_not_create_a_thread() {
        let body = super::reply_request_body("{\"card\":1}", false).expect("应能构造");
        let value: serde_json::Value = serde_json::from_slice(&body).expect("应是 JSON");
        assert_eq!(value["reply_in_thread"], false);
    }

    #[test]
    fn fetch_and_cache_name_dry_run_returns_none_and_caches_nothing() {
        // 取名已无网络路径:空缓存必然 None,且不写占位(缓存占位会让补齐后永不重试)。
        let fc = dry_client();
        let result = fc.fetch_and_cache_name("ou_dry");
        assert_eq!(result, None, "dry_run 应返回 None");
        // 确认缓存未写入:resolve_name_cached 仍应 None
        assert_eq!(
            fc.resolve_name_cached("ou_dry"),
            None,
            "dry_run 不应缓存占位"
        );
    }

    #[test]
    fn fetch_and_cache_name_returns_cached_on_hit() {
        let fc = dry_client();
        // 先手动塞入缓存
        fc.names
            .lock()
            .unwrap()
            .insert("ou_hit".to_string(), "李四".to_string());
        // 缓存命中时,即使 dry_run 也直接返回缓存值(不触发网络)
        assert_eq!(fc.fetch_and_cache_name("ou_hit"), Some("李四".to_string()));
    }

    /// 取名路径不许再碰 `contact/v3/users` —— 那个接口只覆盖 app 的通讯录数据权限范围,
    /// 范围外的人恒返回 `41050 no user authority error`,于是发信人永远显示成占位「用户」。
    ///
    /// 行为测试区分不了新旧实现(两者在无网络时都返回 None),所以直接对源码下断言:
    /// 名字来源只能是群成员列表 + identity-cache 预热。
    ///
    /// **`fetch_union_id` 是有意的例外**:它也打同一个 endpoint,但取的是 union_id 供 operate-gate
    /// 鉴权。owner 一定在自己 app 的通讯录范围内,而 agent.yaml 里配的 operators 可能是别的 app 的
    /// open_id(实测 `99992361 open_id cross app`),真正生效的正是 union_id 那条 —— 删了会让
    /// operate-gate 当场失效。
    #[test]
    fn contact_user_api_is_not_used_for_name_resolution() {
        let source = include_str!("client.rs");
        // 搜索串必须用 concat! 在编译期拼:写成字面量的话,本测试自身的文本就会被
        // include_str! 读进来,断言永远命中自己(自指陷阱)。
        assert!(
            !source.contains(concat!("fn ", "fetch_name")),
            "取名不得回退到 contact/v3/users"
        );
        assert!(
            source.contains(concat!("fn ", "fetch_union_id")),
            "union_id 解析必须保留,否则 operate-gate 失效"
        );
    }

    #[test]
    fn prime_names_seeds_cache_without_network() {
        let fc = dry_client();
        fc.prime_names([("ou_seed".to_string(), "李四".to_string())]);
        // 预热后立即可命中:这正是「introduce 那一刻学到的名字,下一条消息就能用上」的机制
        assert_eq!(fc.resolve_name_cached("ou_seed"), Some("李四".to_string()));
        assert_eq!(fc.resolve_name("ou_seed"), "李四".to_string());
    }

    /// L12:`prime_names` 是**覆盖**语义。调用点每条入站消息都用本条 mention 刷新一次,
    /// `or_insert` 会把首次学到的名字变成永久值 —— 人改名后整个 daemon 生命周期都叫不对,
    /// 只能靠重启纠正。两个来源(mention / 磁盘 identity-cache)同源同鲜度,没有「旧值更可信」。
    #[test]
    fn prime_names_refreshes_existing_entry() {
        let fc = dry_client();
        fc.names
            .lock()
            .unwrap()
            .insert("ou_x".into(), "改名前".into());
        fc.prime_names([("ou_x".to_string(), "改名后".to_string())]);
        assert_eq!(fc.resolve_name_cached("ou_x"), Some("改名后".to_string()));
    }

    #[test]
    fn prime_names_skips_empty_pairs() {
        let fc = dry_client();
        fc.prime_names([
            (String::new(), "无 id".to_string()),
            ("ou_empty_name".to_string(), String::new()),
        ]);
        assert_eq!(fc.resolve_name_cached("ou_empty_name"), None);
    }

    #[test]
    fn fetch_and_cache_name_only_reads_cache() {
        // 非 dry_run + 空缓存:退化成纯查询,直接 None,且不写占位。
        let fc = FeishuClient::new("feishu.invalid", "app".into(), "secret".into(), false);
        assert_eq!(fc.fetch_and_cache_name("ou_unknown"), None);
        assert_eq!(fc.resolve_name_cached("ou_unknown"), None, "失败不缓存占位");
        assert_eq!(fc.resolve_name("ou_unknown"), "用户".to_string());
    }

    #[test]
    fn resolve_name_falls_back_to_placeholder_when_no_cache_dry_run() {
        let fc = dry_client();
        // dry_run + 无缓存:resolve_name 应回退占位 "用户"
        assert_eq!(fc.resolve_name("ou_nobody"), "用户".to_string());
        // 回退不应写缓存(fix:旧代码缓存 "用户",导致补权限后永远不重试)
        assert_eq!(fc.resolve_name_cached("ou_nobody"), None);
    }

    #[test]
    fn interactive_card_to_open_id_is_dry_run_safe_and_returns_message_id() {
        let fc = dry_client();
        assert_eq!(
            fc.send_card_to_open_id_id("ou_owner", r#"{"schema":"2.0"}"#)
                .unwrap(),
            "dry-msg"
        );
    }

    #[test]
    fn process_supplement_retries_keep_the_same_thread_payload_and_uuid() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let base = format!("http://{}", server.server_addr().to_ip().unwrap());
        let server_thread = std::thread::spawn(move || {
            let mut bodies = Vec::new();
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
                assert!(request.url().ends_with("/messages/om_origin/reply"));
                let mut body = String::new();
                request.as_reader().read_to_string(&mut body).unwrap();
                bodies.push(body);
                let response = if index == 1 {
                    ""
                } else {
                    r#"{"code":0,"data":{"message_id":"om_supplement"}}"#
                };
                request
                    .respond(tiny_http::Response::from_string(response))
                    .unwrap();
            }
            bodies
        });
        let client = FeishuClient::with_test_base_connection_close(base);
        let card = summon_core::feishu::card_process_supplement("过程里的 & <xml> @名字");
        assert!(client
            .send_card_idempotent_anchored("oc_chat", Some("om_origin"), &card, "supplement-1")
            .is_err());
        assert_eq!(
            client
                .send_card_idempotent_anchored("oc_chat", Some("om_origin"), &card, "supplement-1")
                .unwrap(),
            "om_supplement"
        );
        let bodies = server_thread.join().unwrap();
        assert_eq!(bodies[0], bodies[1]);
        let body: serde_json::Value = serde_json::from_str(&bodies[0]).unwrap();
        assert_eq!(body["uuid"], "supplement-1");
        assert_eq!(body["reply_in_thread"], true);
        assert_eq!(body["content"], card);
    }

    #[test]
    fn connection_close_client_opens_a_fresh_connection_per_request() {
        use std::io::{BufRead, BufReader, Write};
        // 服务端答完不关连接也不再读：等同于 tiny_http 回应已发出、FIN 还没到客户端的那一刻。
        // 客户端此时若从连接池里取回这条连接，下一条请求就没人读，只能等到超时。
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let mut held = Vec::new();
            for _ in 0..3 {
                let (stream, _) = listener.accept().unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                while reader.read_line(&mut line).unwrap() > 0 && line != "\r\n" {
                    line.clear();
                }
                let body = r#"{"code":0,"tenant_access_token":"token","expire":7200,"data":{"name":"Group"}}"#;
                let mut writer = &stream;
                write!(
                    writer,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{body}",
                    body.len()
                )
                .unwrap();
                held.push(stream);
            }
            held.len()
        });
        let client = FeishuClient::with_test_base_connection_close(base);
        assert_eq!(client.fetch_chat_name("oc_chat").unwrap(), "Group");
        assert_eq!(client.fetch_chat_name("oc_chat").unwrap(), "Group");
        assert_eq!(server.join().unwrap(), 3);
    }

    #[test]
    fn idempotent_card_request_body_is_byte_stable_and_validates_uuid() {
        let content = r#"{"schema":"2.0","body":{"elements":[]}}"#;
        let first = card_request_body("oc_group", content, Some("bridge-turn-deadbeef")).unwrap();
        let second = card_request_body("oc_group", content, Some("bridge-turn-deadbeef")).unwrap();
        assert_eq!(first, second);
        let value: serde_json::Value = serde_json::from_slice(&first).unwrap();
        assert_eq!(value["uuid"], "bridge-turn-deadbeef");
        assert_eq!(value["content"], content);
        assert!(card_request_body("oc", content, Some(&"x".repeat(51))).is_err());
    }

    #[test]
    fn audit_rejection_is_its_own_error_and_keeps_the_raw_response() {
        let body = r#"{"code":230028,"msg":"The messages do NOT pass the audit, ext=contain sensitive data: EMAIL_ADDRESS","error":{"log_id":"x"}}"#;
        let error = send_failure("send_card", reqwest::StatusCode::BAD_REQUEST, body);
        let rejected = error
            .downcast_ref::<ContentRejected>()
            .expect("审核拦截要能被认出来");
        assert_eq!(rejected.reason, "contain sensitive data: EMAIL_ADDRESS");
        assert!(
            error.to_string().contains("230028"),
            "日志里仍是原始响应: {error}"
        );

        let other = send_failure(
            "send_card",
            reqwest::StatusCode::BAD_REQUEST,
            r#"{"code":230001,"msg":"invalid message_id"}"#,
        );
        assert!(other.downcast_ref::<ContentRejected>().is_none());
        assert!(other.to_string().contains("send_card 失败 400"));
    }

    #[test]
    fn send_card_refreshes_invalid_tenant_token_once() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let base = format!("http://{}", server.server_addr().to_ip().unwrap());
        let server_thread = std::thread::spawn(move || {
            for request_index in 0..4 {
                let request = server
                    .recv_timeout(Duration::from_secs(2))
                    .unwrap()
                    .expect("失效 token 后应立即换新并重试原发送");
                let response = match request_index {
                    0 => {
                        assert!(request.url().contains("auth/v3/tenant_access_token"));
                        tiny_http::Response::from_string(
                            r#"{"code":0,"tenant_access_token":"stale","expire":7200}"#,
                        )
                    }
                    1 => {
                        assert!(request.url().contains("im/v1/messages"));
                        assert!(request
                            .headers()
                            .iter()
                            .any(|h| h.field.equiv("authorization")
                                && h.value.as_str() == "Bearer stale"));
                        tiny_http::Response::from_string(
                            r#"{"code":99991663,"msg":"tenant access token invalid"}"#,
                        )
                        .with_status_code(400)
                    }
                    2 => {
                        assert!(request.url().contains("auth/v3/tenant_access_token"));
                        tiny_http::Response::from_string(
                            r#"{"code":0,"tenant_access_token":"fresh","expire":7200}"#,
                        )
                    }
                    _ => {
                        assert!(request.url().contains("im/v1/messages"));
                        assert!(request
                            .headers()
                            .iter()
                            .any(|h| h.field.equiv("authorization")
                                && h.value.as_str() == "Bearer fresh"));
                        tiny_http::Response::from_string(
                            r#"{"code":0,"data":{"message_id":"om_recovered"}}"#,
                        )
                    }
                };
                request.respond(response).unwrap();
            }
        });

        let client = FeishuClient::with_test_base(base);
        let result = client.send_card_id("oc_group", r#"{"schema":"2.0"}"#);
        server_thread.join().unwrap();

        assert_eq!(result.unwrap(), "om_recovered");
    }

    #[test]
    fn bot_v3_info_parses_name_and_open_id() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let base = format!("http://{}", server.server_addr().to_ip().unwrap());
        let server_thread = std::thread::spawn(move || {
            for request_index in 0..2 {
                let request = server
                    .recv_timeout(Duration::from_secs(2))
                    .unwrap()
                    .expect("应完成 token + bot info 两跳");
                let response = match request_index {
                    0 => {
                        assert!(request.url().contains("auth/v3/tenant_access_token"));
                        tiny_http::Response::from_string(
                            r#"{"code":0,"tenant_access_token":"t","expire":7200}"#,
                        )
                    }
                    _ => {
                        assert!(request.url().contains("/bot/v3/info"));
                        tiny_http::Response::from_string(
                            r#"{"code":0,"bot":{"app_name":"测试机器人","open_id":"ou_bot"}}"#,
                        )
                    }
                };
                request.respond(response).unwrap();
            }
        });
        let client = FeishuClient::with_test_base(base);
        let info = client.bot_v3_info().expect("bot info 应解析成功");
        server_thread.join().unwrap();
        assert_eq!(info.app_name, "测试机器人");
        assert_eq!(info.open_id, "ou_bot");
    }

    #[test]
    fn bot_v3_info_surfaces_business_error_for_triage() {
        // 机器人能力未启用时飞书回 code!=0(HTTP 200):错误必须带出 code/msg,
        // onboard 据此分流到「启用机器人能力」而不是三条一锅端。
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let base = format!("http://{}", server.server_addr().to_ip().unwrap());
        let server_thread = std::thread::spawn(move || {
            for request_index in 0..2 {
                let request = server
                    .recv_timeout(Duration::from_secs(2))
                    .unwrap()
                    .expect("应完成 token + bot info 两跳");
                let response = match request_index {
                    0 => tiny_http::Response::from_string(
                        r#"{"code":0,"tenant_access_token":"t","expire":7200}"#,
                    ),
                    _ => tiny_http::Response::from_string(
                        r#"{"code":230002,"msg":"bot not enabled"}"#,
                    ),
                };
                request.respond(response).unwrap();
            }
        });
        let client = FeishuClient::with_test_base(base);
        let err = client.bot_v3_info().unwrap_err().to_string();
        server_thread.join().unwrap();
        assert!(err.contains("230002"), "错误应带 code,实得:{err}");
        assert!(err.contains("bot not enabled"), "错误应带 msg,实得:{err}");
    }

    /// 起一条 mock:先应 token,随后按 `pages` 顺序逐页应答 Bot 所在群列表。
    /// 返回收到的群列表请求 URL,供翻页断言。
    fn serve_bot_chats_pages(
        pages: Vec<&'static str>,
    ) -> (String, std::thread::JoinHandle<Vec<String>>) {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let base = format!("http://{}", server.server_addr().to_ip().unwrap());
        let handle = std::thread::spawn(move || {
            let token_request = server
                .recv_timeout(Duration::from_secs(10))
                .unwrap()
                .expect("首跳应是 tenant_access_token");
            assert!(token_request.url().contains("auth/v3/tenant_access_token"));
            token_request
                .respond(tiny_http::Response::from_string(
                    r#"{"code":0,"tenant_access_token":"token","expire":7200}"#,
                ))
                .unwrap();
            let mut urls = Vec::new();
            for page in pages {
                let Ok(Some(request)) = server.recv_timeout(Duration::from_secs(10)) else {
                    break;
                };
                urls.push(request.url().to_string());
                request
                    .respond(tiny_http::Response::from_string(page))
                    .unwrap();
            }
            urls
        });
        (base, handle)
    }

    #[test]
    fn delegation_group_info_requires_success_and_complete_identity() {
        for (body, expected) in [
            (
                r#"{"code":0,"data":{"name":"目标群","chat_mode":"group"}}"#,
                true,
            ),
            (r#"{"code":0,"data":{"name":"目标群"}}"#, false),
            (
                r#"{"code":999,"data":{"name":"目标群","chat_mode":"group"}}"#,
                false,
            ),
        ] {
            let (base, server) = serve_bot_chats_pages(vec![body]);
            let result = FeishuClient::with_test_base(base).delegation_group_info("oc_target");
            assert_eq!(result.is_ok(), expected, "{result:?}");
            assert!(server.join().unwrap()[0].contains("/im/v1/chats/oc_target"));
        }
    }

    #[test]
    fn group_name_search_finishes_all_pages_before_selecting() {
        let (base, server) = serve_bot_chats_pages(vec![
            r#"{"code":0,"data":{"items":[{"chat_id":"oc_a","name":"Target"}],"has_more":true,"page_token":"next"}}"#,
            r#"{"code":0,"data":{"items":[{"chat_id":"oc_b","name":"Target"},{"chat_id":"oc_c","name":"Target extra"}],"has_more":false}}"#,
        ]);
        let found = FeishuClient::with_test_base(base)
            .groups_named("Target")
            .unwrap();
        assert_eq!(
            found.iter().map(|g| g.chat_id.as_str()).collect::<Vec<_>>(),
            ["oc_a", "oc_b"]
        );
        assert!(server.join().unwrap()[1].contains("page_token=next"));
        let (base, server) = serve_bot_chats_pages(vec![
            r#"{"code":0,"data":{"items":[{"chat_id":"oc_a","name":"Target"}],"has_more":true}}"#,
        ]);
        assert!(FeishuClient::with_test_base(base)
            .groups_named("Target")
            .is_err());
        server.join().unwrap();
    }

    #[test]
    fn group_requester_membership_requires_real_user_on_any_complete_page() {
        let (base, server) = serve_bot_chats_pages(vec![
            r#"{"code":0,"data":{"items":[{"member_id":"ou_else"}],"has_more":true,"page_token":"next"}}"#,
            r#"{"code":0,"data":{"items":[{"member_id":"ou_owner"}],"has_more":false}}"#,
        ]);
        assert!(FeishuClient::with_test_base(base)
            .prove_user_in_chat("oc_target", "ou_owner")
            .is_ok());
        let urls = server.join().unwrap();
        assert!(urls[1].contains("member_id_type=open_id") && urls[1].contains("page_token=next"));
        for body in [
            r#"{"code":0,"data":{"items":[{"member_id":"ou_else"}],"has_more":false}}"#,
            r#"{"code":999,"data":{"items":[{"member_id":"ou_owner"}],"has_more":false}}"#,
            r#"{"code":0,"data":{"items":[],"has_more":true}}"#,
        ] {
            let (base, server) = serve_bot_chats_pages(vec![body]);
            assert!(FeishuClient::with_test_base(base)
                .prove_user_in_chat("oc_target", "ou_owner")
                .is_err());
            server.join().unwrap();
        }
    }

    #[test]
    fn bot_membership_proof_accepts_a_group_listed_for_this_bot() {
        let (base, server_thread) = serve_bot_chats_pages(vec![
            r#"{"code":0,"data":{"items":[{"chat_id":"oc_other"},{"chat_id":"oc_target"}],"has_more":false}}"#,
        ]);

        let client = FeishuClient::with_test_base(base);
        let result = client.prove_bot_in_chat("oc_target");
        let urls = server_thread.join().unwrap();

        assert!(
            result.is_ok(),
            "目标群在 Bot 所在群列表里即构成实时可达证明:{result:?}"
        );
        assert!(
            urls[0].contains("/im/v1/chats?"),
            "应查 Bot 所在群列表,实得:{}",
            urls[0]
        );
    }

    const SINGLE_CHAT: &str =
        r#"{"code":0,"data":{"chat_mode":"group","user_count":"1","bot_count":"1"}}"#;
    const SINGLE_MEMBER: &str = r#"{"code":0,"data":{"items":[{"member_id_type":"open_id","member_id":"ou_owner"}],"has_more":false}}"#;

    /// 成员接口始终成功，群计数边界必须由群详情判断独立守住。
    fn serve_mentionless_info(
        infos: Vec<&'static str>,
    ) -> (
        String,
        std::sync::mpsc::Sender<()>,
        std::thread::JoinHandle<Vec<String>>,
    ) {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let base = format!("http://{}", server.server_addr().to_ip().unwrap());
        let (stop, stopped) = std::sync::mpsc::channel();
        let handle = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut urls = Vec::new();
            let mut index = 0;
            while Instant::now() < deadline
                && matches!(
                    stopped.try_recv(),
                    Err(std::sync::mpsc::TryRecvError::Empty)
                )
            {
                let Some(request) = server.recv_timeout(Duration::from_millis(20)).unwrap() else {
                    continue;
                };
                let response = if request.url().contains("auth/v3/tenant_access_token") {
                    r#"{"code":0,"tenant_access_token":"t","expire":7200}"#
                } else {
                    urls.push(request.url().to_string());
                    if request.url().contains("/members?") {
                        SINGLE_MEMBER
                    } else {
                        let info = infos.get(index).unwrap_or(infos.last().unwrap());
                        index += 1;
                        info
                    }
                };
                request
                    .respond(tiny_http::Response::from_string(response))
                    .unwrap();
            }
            urls
        });
        (base, stop, handle)
    }

    #[test]
    fn mentionless_proof_checks_current_counts_and_sender_on_every_message() {
        let (base, stop, server) = serve_mentionless_info(vec![
            SINGLE_CHAT,
            r#"{"code":0,"data":{"chat_mode":"group","user_count":"1","bot_count":"2"}}"#,
        ]);
        let client = FeishuClient::with_test_base(base);
        let first = client.prove_mentionless_chat("oc_target", "ou_owner");
        let second = client.prove_mentionless_chat("oc_target", "ou_owner");
        stop.send(()).unwrap();
        let urls = server.join().unwrap();
        assert!(first.is_ok());
        let rejection = second.expect_err("多 Bot 群必须由计数判断拒收").to_string();
        assert!(rejection.contains("bot_count=2"), "{rejection}");
        assert_eq!(urls.len(), 3);
        assert_eq!(urls[0], "/open-apis/im/v1/chats/oc_target");
        assert!(urls[1].starts_with("/open-apis/im/v1/chats/oc_target/members?"));
        assert!(urls[1].contains("member_id_type=open_id"));
        assert_eq!(urls[0], urls[2]);
    }

    #[test]
    fn mentionless_proof_rejects_incomplete_or_non_single_chat_information() {
        for response in [
            r#"{"code":0,"data":{"chat_mode":"group","user_count":"2","bot_count":"1"}}"#,
            r#"{"code":0,"data":{"chat_mode":"group","user_count":"0","bot_count":"1"}}"#,
            r#"{"code":0,"data":{"chat_mode":"group","user_count":"1","bot_count":"0"}}"#,
            r#"{"code":0,"data":{"chat_mode":"group","user_count":"1","bot_count":"2"}}"#,
            r#"{"code":0,"data":{"chat_mode":"group","user_count":"1"}}"#,
            r#"{"code":0,"data":{"chat_mode":"group","user_count":"1","bot_count":1}}"#,
            r#"{"code":0,"data":{"chat_mode":"p2p","user_count":"1","bot_count":"1"}}"#,
            r#"{"code":0,"data":{"user_count":"1","bot_count":"1"}}"#,
            r#"{"code":0,"data":{"chat_mode":"group","user_count":1,"bot_count":1}}"#,
            r#"{"code":0,"data":{}}"#,
            r#"{"code":99991672,"data":{"chat_mode":"group","user_count":"1","bot_count":"1"}}"#,
            r#"{"data":{"chat_mode":"group","user_count":"1","bot_count":"1"}}"#,
            "not json",
        ] {
            let (base, stop, server) = serve_mentionless_info(vec![response]);
            let client = FeishuClient::with_test_base(base);
            let result = client.prove_mentionless_chat("oc_target", "ou_owner");
            stop.send(()).unwrap();
            let urls = server.join().unwrap();
            assert!(result.is_err(), "{response}");
            assert_eq!(urls.len(), 1);
        }
        assert!(dry_client()
            .prove_mentionless_chat("oc_target", "ou_owner")
            .is_err());
    }

    #[test]
    fn app_owner_accepts_only_enterprise_member_owner() {
        let app = |kind: &str, owner: &str| -> &'static str {
            Box::leak(
                format!(
                    r#"{{"code":0,"data":{{"app":{{"creator_id":"ou_creator","owner":{{"type":{kind},"owner_id":"{owner}"}}}}}}}}"#
                )
                .into_boxed_str(),
            )
        };
        let owner_of = |infos: Vec<&'static str>| {
            let (base, stop, server) = serve_mentionless_info(infos);
            let result = FeishuClient::with_test_base(base).app_owner();
            stop.send(()).unwrap();
            (result, server.join().unwrap())
        };

        let (result, urls) = owner_of(vec![app("2", "ou_owner"), app("2", "on_owner")]);
        assert_eq!(
            result.unwrap(),
            Some(summon_core::config::registry::AppOwner {
                open_id: "ou_owner".into(),
                union_id: "on_owner".into(),
            })
        );
        assert_eq!(urls.len(), 2);
        assert!(urls[0].contains("applications/test-app?") && urls[0].contains("open_id"));
        assert!(urls[1].contains("user_id_type=union_id"));

        // 所有者不是企业成员、或 owner_id 为空:没有可放行的人,也不退回 creator_id。
        for kind in ["0", "1"] {
            let (result, _) = owner_of(vec![app(kind, "ou_x"), app(kind, "on_x")]);
            assert_eq!(result.unwrap(), None, "type={kind}");
        }
        let (result, _) = owner_of(vec![app("2", ""), app("2", "")]);
        assert_eq!(result.unwrap(), None);

        // 业务错误、缺 data.app、两次结论不一致:都是失败,由调用方保留旧值。
        for infos in [
            vec![r#"{"code":99991672,"msg":"access denied"}"#],
            vec![r#"{"code":0,"data":{}}"#],
            vec![app("2", "ou_owner"), app("1", "on_owner")],
            vec!["not json"],
        ] {
            let (result, _) = owner_of(infos.clone());
            assert!(result.is_err(), "{infos:?}");
        }
        assert!(dry_client().app_owner().is_err());
    }

    #[test]
    fn mentionless_proof_requires_complete_membership_for_the_actual_sender() {
        for response in [
            r#"{"code":0,"data":{"items":[{"member_id_type":"open_id","member_id":"ou_other"}],"has_more":false}}"#,
            r#"{"code":0,"data":{"items":[{"member_id_type":"union_id","member_id":"ou_owner"}],"has_more":false}}"#,
            r#"{"code":0,"data":{"items":[{"member_id_type":"open_id","member_id":"ou_owner"}],"has_more":true}}"#,
            r#"{"code":0,"data":{"items":[{"member_id_type":"open_id","member_id":"ou_owner"}]}}"#,
            r#"{"code":0,"data":{"items":[],"has_more":false}}"#,
            r#"{"code":0,"data":{"items":[{"member_id":"ou_owner"},{"member_id":"ou_other"}],"has_more":false}}"#,
            r#"{"code":0,"data":{"has_more":false}}"#,
            r#"{"code":232011,"msg":"operator not in chat"}"#,
            r#"{"code":99991672,"msg":"access denied"}"#,
            "not json",
        ] {
            let (base, server) = serve_bot_chats_pages(vec![SINGLE_CHAT, response]);
            let client = FeishuClient::with_test_base(base);
            let rejection = client
                .prove_mentionless_chat("oc_target", "ou_owner")
                .expect_err(response)
                .to_string();
            assert_eq!(server.join().unwrap().len(), 2);
            if response.contains("ou_other") && !response.contains("ou_owner") {
                assert!(rejection.contains("sender_matches=false"), "{rejection}");
            }
            if response.contains("232011") {
                assert!(rejection.contains("code=232011"), "{rejection}");
            }
            if !response.contains("items") && response.contains("has_more") {
                assert!(rejection.contains("items=缺失"), "{rejection}");
            }
            if response == "not json" {
                assert!(rejection.contains("非 JSON"), "{rejection}");
            }
        }
    }

    /// 判「在」也要跟到底:只看首页会把翻页靠后的群误判成已退群。
    #[test]
    fn bot_membership_proof_keeps_paging_until_the_group_shows_up() {
        let (base, server_thread) = serve_bot_chats_pages(vec![
            r#"{"code":0,"data":{"items":[{"chat_id":"oc_other"}],"has_more":true,"page_token":"page-2"}}"#,
            r#"{"code":0,"data":{"items":[{"chat_id":"oc_target"}],"has_more":false}}"#,
        ]);

        let client = FeishuClient::with_test_base(base);
        let result = client.prove_bot_in_chat("oc_target");
        let urls = server_thread.join().unwrap();

        assert!(result.is_ok(), "目标群在第二页时同样成立:{result:?}");
        assert_eq!(urls.len(), 2, "应翻到第二页,实得:{urls:?}");
        assert!(
            urls[1].contains("page_token=page-2"),
            "第二页应带上一页给的游标,实得:{}",
            urls[1]
        );
    }

    #[test]
    fn bot_membership_proof_rejects_a_group_absent_from_every_page() {
        let (base, server_thread) = serve_bot_chats_pages(vec![
            r#"{"code":0,"data":{"items":[{"chat_id":"oc_a"}],"has_more":true,"page_token":"page-2"}}"#,
            r#"{"code":0,"data":{"items":[{"chat_id":"oc_b"}],"has_more":false}}"#,
        ]);

        let client = FeishuClient::with_test_base(base);
        let error = client
            .prove_bot_in_chat("oc_target")
            .expect_err("Bot 已被移出目标群时必须拒绝召回");
        server_thread.join().unwrap();

        assert!(format!("{error:#}").contains("已不在目标群"));
    }

    /// 分页没走完就下「不在群」的结论会误杀。has_more 为真却拿不到下一页游标时,
    /// 唯一正确的结论是「证不了」,不是「不在」。
    #[test]
    fn bot_membership_proof_refuses_to_conclude_absence_when_paging_stalls() {
        let (base, server_thread) = serve_bot_chats_pages(vec![
            r#"{"code":0,"data":{"items":[{"chat_id":"oc_a"}],"has_more":true,"page_token":""}}"#,
        ]);

        let client = FeishuClient::with_test_base(base);
        let error = client
            .prove_bot_in_chat("oc_target")
            .expect_err("分页不收敛时不能放行");
        server_thread.join().unwrap();

        let error = format!("{error:#}");
        assert!(error.contains("未收敛"), "应报分页不收敛,实得:{error}");
        assert!(
            !error.contains("已不在目标群"),
            "证不了不等于不在群,实得:{error}"
        );
    }

    /// 缺 scope 时飞书回 400 + 99991672。这一跳失败只能 fail-closed,
    /// 且不能把「查不到」说成「不在群」——两者的处置完全不同。
    #[test]
    fn bot_membership_proof_fails_closed_when_the_app_lacks_the_scope() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let base = format!("http://{}", server.server_addr().to_ip().unwrap());
        let server_thread = std::thread::spawn(move || {
            let token_request = server
                .recv_timeout(Duration::from_secs(10))
                .unwrap()
                .unwrap();
            token_request
                .respond(tiny_http::Response::from_string(
                    r#"{"code":0,"tenant_access_token":"token","expire":7200}"#,
                ))
                .unwrap();
            let request = server
                .recv_timeout(Duration::from_secs(10))
                .unwrap()
                .unwrap();
            request
                .respond(
                    tiny_http::Response::from_string(
                        r#"{"code":99991672,"msg":"Access denied. One of the following scopes is required"}"#,
                    )
                    .with_status_code(400),
                )
                .unwrap();
        });

        let client = FeishuClient::with_test_base(base);
        let error = client
            .prove_bot_in_chat("oc_target")
            .expect_err("查不到在群关系时必须拒绝召回");
        server_thread.join().unwrap();

        let error = format!("{error:#}");
        assert!(
            error.contains("99991672"),
            "错误应带上飞书原文以便定位:{error}"
        );
        assert!(
            !error.contains("已不在目标群"),
            "查询失败不等于不在群,实得:{error}"
        );
    }

    fn sent_feed_preview(request_body: &[u8]) -> String {
        let request: serde_json::Value = serde_json::from_slice(request_body).unwrap();
        let card: serde_json::Value =
            serde_json::from_str(request["content"].as_str().unwrap()).unwrap();
        card["config"]["summary"]["content"]
            .as_str()
            .unwrap_or_default()
            .to_string()
    }

    /// 发卡与回复卡都在出口补预览,调用方不必各自记得。
    #[test]
    fn content_cards_keep_payload_across_delivery_update_and_freeze() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let base = format!("http://{}", server.server_addr().to_ip().unwrap());
        let rendered = summon_core::feishu::render_external_lark_md(
            "正文第一行\n第二行",
            &[],
            &std::collections::HashMap::new(),
        );
        let card = summon_core::feishu::card_markdown_v2_rendered(&rendered);
        let expected = card.clone();
        let worker = std::thread::spawn(move || {
            let token = server
                .recv_timeout(Duration::from_secs(10))
                .unwrap()
                .unwrap();
            token
                .respond(tiny_http::Response::from_string(
                    r#"{"tenant_access_token":"token","expire":7200}"#,
                ))
                .unwrap();
            let mut routes = Vec::new();
            for _ in 0..6 {
                let mut request = server
                    .recv_timeout(Duration::from_secs(10))
                    .unwrap()
                    .unwrap();
                routes.push((request.method().to_string(), request.url().to_string()));
                let mut body = String::new();
                request.as_reader().read_to_string(&mut body).unwrap();
                let body: serde_json::Value = serde_json::from_str(&body).unwrap();
                assert_eq!(body["content"], expected);
                request
                    .respond(tiny_http::Response::from_string(
                        r#"{"code":0,"data":{"message_id":"om_content"}}"#,
                    ))
                    .unwrap();
            }
            routes
        });
        let client = FeishuClient::with_test_base(base);
        client.send_card_id("oc_group", &card).unwrap();
        client.send_card_to_open_id_id("ou_owner", &card).unwrap();
        client
            .send_card_id_replying("oc_group", Some(("om_origin", true)), &card)
            .unwrap();
        client.update_card("om_content", &card).unwrap();
        client
            .send_rendered_markdown_idempotent_id("oc_group", &rendered, "content-wire-1")
            .unwrap();
        client
            .send_frozen_card_id("ou_owner", "open_id", &card)
            .unwrap();
        let routes = worker.join().unwrap();
        assert!(routes[0].1.ends_with("chat_id"));
        assert!(routes[1].1.ends_with("open_id"));
        assert!(routes[2].1.ends_with("/om_origin/reply"));
        assert_eq!(routes[3].0, "PATCH");
        assert!(routes[4].1.ends_with("chat_id"));
        assert!(routes[5].1.ends_with("open_id"));
    }

    #[test]
    fn terminal_controls_keep_links_and_close_when_sent_privately_and_updated() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let base = format!("http://{}", server.server_addr().to_ip().unwrap());
        let terminal = |generation| {
            summon_core::feishu::card_terminal(&summon_core::feishu::TerminalCard {
                agent: "agent",
                chat_label: "群聊",
                read_only_url: &format!("http://127.0.0.1/t/read-{generation}"),
                operate_url: &format!("http://127.0.0.1/t/operate-{generation}"),
                target_chat_id: "oc_group",
                target_chat_type: "group",
                recipient_open_id: "ou_owner",
            })
        };
        let initial = terminal(1);
        let renewed = terminal(2);
        let notice = summon_core::feishu::card_notice_with_terminal("请到终端处理");
        let expected = [initial.clone(), renewed.clone(), notice.clone()];
        let worker = std::thread::spawn(move || {
            let token = server
                .recv_timeout(Duration::from_secs(10))
                .unwrap()
                .unwrap();
            token
                .respond(tiny_http::Response::from_string(
                    r#"{"tenant_access_token":"token","expire":7200}"#,
                ))
                .unwrap();
            let mut routes = Vec::new();
            for card in expected {
                let mut request = server
                    .recv_timeout(Duration::from_secs(10))
                    .unwrap()
                    .unwrap();
                routes.push((request.method().to_string(), request.url().to_string()));
                let mut body = String::new();
                request.as_reader().read_to_string(&mut body).unwrap();
                let body: serde_json::Value = serde_json::from_str(&body).unwrap();
                let sent: serde_json::Value =
                    serde_json::from_str(body["content"].as_str().unwrap()).unwrap();
                let mut source: serde_json::Value = serde_json::from_str(&card).unwrap();
                source["config"] = sent["config"].clone();
                assert_eq!(sent, source, "链接和业务回调应完整保留");
                fn close_count(value: &serde_json::Value) -> usize {
                    match value {
                        serde_json::Value::Object(map) => {
                            usize::from(
                                map.get("action").and_then(|a| a.as_str()) == Some("card_close"),
                            ) + map.values().map(close_count).sum::<usize>()
                        }
                        serde_json::Value::Array(items) => items.iter().map(close_count).sum(),
                        _ => 0,
                    }
                }
                assert_eq!(close_count(&sent), 1);
                request
                    .respond(tiny_http::Response::from_string(
                        r#"{"code":0,"data":{"message_id":"om_terminal"}}"#,
                    ))
                    .unwrap();
            }
            routes
        });
        let client = FeishuClient::with_test_base(base);
        client
            .send_card_to_open_id_id("ou_owner", &initial)
            .unwrap();
        client.update_card("om_terminal", &renewed).unwrap();
        client.send_card_id("oc_group", &notice).unwrap();
        let routes = worker.join().unwrap();
        assert!(routes[0].1.ends_with("open_id"));
        assert_eq!(routes[1].0, "PATCH");
        assert!(routes[1].1.ends_with("/im/v1/messages/om_terminal"));
        assert!(routes[2].1.ends_with("chat_id"));
    }

    #[test]
    fn transport_preserves_content_card_structure_in_both_schemas() {
        for card in [
            serde_json::json!({"elements": [{"tag": "div", "text": {"tag": "lark_md", "content": "原文"}}]}),
            serde_json::json!({"schema": "2.0", "body": {"elements": [{"tag": "markdown", "content": "原文"}]}}),
        ] {
            let source = card.to_string();
            for body in [
                card_request_body("oc_group", &source, None).unwrap(),
                reply_request_body(&source, false).unwrap(),
            ] {
                let request: serde_json::Value = serde_json::from_slice(&body).unwrap();
                let mut sent: serde_json::Value =
                    serde_json::from_str(request["content"].as_str().unwrap()).unwrap();
                if card["schema"] == "2.0" {
                    sent.as_object_mut().unwrap().remove("config");
                }
                assert_eq!(sent, card);
            }
        }
    }

    #[test]
    fn sent_and_replied_cards_carry_a_feed_preview() {
        let idle = summon_core::feishu::card_idle_prompt("g-x", 3300);
        for body in [
            card_request_body("oc_1", &idle, None).unwrap(),
            reply_request_body(&idle, false).unwrap(),
        ] {
            let preview = sent_feed_preview(&body);
            assert!(preview.starts_with("会话空闲提醒"), "{preview}");
            assert!(String::from_utf8_lossy(&body).contains("card_close"));
        }
        let frozen = card_request_body("oc_1", &idle, Some("frozen-wire-1")).unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&frozen).unwrap()["content"],
            idle.as_str(),
            "冻结的线上字节原样投递"
        );
    }

    /// 卡片被交互改写走的是 PATCH,预览要跟着新内容走 —— 这条出口同样补。
    #[test]
    fn updated_cards_carry_a_feed_preview() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let base = format!("http://{}", server.server_addr().to_ip().unwrap());
        let server_thread = std::thread::spawn(move || {
            let token = server
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .unwrap();
            token
                .respond(tiny_http::Response::from_string(
                    r#"{"tenant_access_token":"token","expire":7200}"#,
                ))
                .unwrap();
            let mut patch = server
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .unwrap();
            let mut body = Vec::new();
            std::io::Read::read_to_end(patch.as_reader(), &mut body).unwrap();
            patch
                .respond(tiny_http::Response::from_string(r#"{"code":0}"#))
                .unwrap();
            body
        });

        let client = FeishuClient::with_test_base(base);
        let card = serde_json::json!({
            "schema": "2.0",
            "body": {"elements": [{"tag": "markdown", "content": "**任务**已完成"}]},
        });
        client.update_card("om_1", &card.to_string()).unwrap();
        let body = server_thread.join().unwrap();

        assert_eq!(sent_feed_preview(&body), "任务已完成");
        assert!(!String::from_utf8_lossy(&body).contains("card_close"));
    }

    #[test]
    fn update_card_retries_transient_invalid_message_id_in_place() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let base = format!("http://{}", server.server_addr().to_ip().unwrap());
        let server_thread = std::thread::spawn(move || {
            for request_index in 0..3 {
                let request = server
                    .recv_timeout(Duration::from_secs(2))
                    .unwrap()
                    .expect("update_card 应重试同一 message_id");
                let response = match request_index {
                    0 => {
                        assert!(request.url().contains("auth/v3/tenant_access_token"));
                        tiny_http::Response::from_string(
                            r#"{"tenant_access_token":"token","expire":7200}"#,
                        )
                    }
                    1 => {
                        assert_eq!(request.method().as_str(), "PATCH");
                        assert!(request.url().ends_with("/im/v1/messages/om_just_created"));
                        tiny_http::Response::from_string(
                            r#"{"code":230001,"msg":"invalid message_id"}"#,
                        )
                        .with_status_code(400)
                    }
                    _ => {
                        assert_eq!(request.method().as_str(), "PATCH");
                        assert!(request.url().ends_with("/im/v1/messages/om_just_created"));
                        tiny_http::Response::from_string(r#"{"code":0}"#)
                    }
                };
                request.respond(response).unwrap();
            }
        });

        let client = FeishuClient::with_test_base(base);
        let result = client.update_card("om_just_created", r#"{"schema":"2.0"}"#);
        server_thread.join().unwrap();

        assert!(
            result.is_ok(),
            "飞书刚创建卡片的 230001 是可恢复窗口，必须在同一 message_id 上重试:{result:?}"
        );
    }

    #[test]
    fn fetch_response_keeps_normal_message_shape() {
        let fetched = parse_fetched_message_response(
            "om_text",
            r#"{"data":{"items":[{"msg_type":"text","body":{"content":"{\"text\":\"普通引用\"}"},"sender":{"id":"ou_alice"}}]}}"#,
        )
        .unwrap();
        assert_eq!(fetched.sender_open_id, "ou_alice");
        assert_eq!(
            fetched.quoted_plaintext_full_with_sender_names(|_| None),
            "普通引用"
        );
        assert!(fetched.quoted_resources().is_empty());
    }

    #[test]
    fn fetch_response_expands_nested_items_even_when_outer_is_omitted() {
        let fetched = parse_fetched_message_response(
            "om_merge",
            r#"{
                "data": {
                    "items": [
                        {
                            "msg_type": "text",
                            "body": {"content": "{\"text\":\"嵌套正文\"}"},
                            "upper_message_id": "om_merge",
                            "sender": {"id": "ou_alice", "sender_type": "user"}
                        },
                        {
                            "msg_type": "file",
                            "body": {"content": "{\"file_key\":\"file_1\",\"file_name\":\"设计.pdf\"}"},
                            "upper_message_id": "om_merge",
                            "sender": {"id": "ou_bob", "sender_type": "user"}
                        }
                    ]
                }
            }"#,
        )
        .unwrap();
        let text = fetched.quoted_plaintext_full_with_sender_names(|_| None);
        assert!(text.contains("转发了 2 条消息"));
        assert!(text.contains("发送人: ou_alice\n嵌套正文"));
        assert!(text.contains("发送人: ou_bob\n[文件: 设计.pdf]"));
        assert_eq!(fetched.quoted_resources().len(), 1);
    }

    #[test]
    fn merge_forward_resolves_each_distinct_sender_once() {
        let fetched = parse_fetched_message_response(
            "om_merge",
            r#"{
                "data": {
                    "items": [
                        {
                            "msg_type": "text",
                            "body": {"content": "{\"text\":\"第一条\"}"},
                            "upper_message_id": "om_merge",
                            "sender": {"id": "ou_alice", "sender_type": "user"}
                        },
                        {
                            "msg_type": "text",
                            "body": {"content": "{\"text\":\"第二条\"}"},
                            "upper_message_id": "om_merge",
                            "sender": {"id": "ou_alice", "sender_type": "user"}
                        }
                    ]
                }
            }"#,
        )
        .unwrap();
        // resolver 现在是 `Fn`(要同时借给多条嵌套消息的 `Mentions`),计数改用 Cell。
        let calls = std::cell::Cell::new(0);
        let text = fetched.quoted_plaintext_full_with_sender_names(|open_id| {
            calls.set(calls.get() + 1);
            assert_eq!(open_id, "ou_alice");
            Some("Alice".to_string())
        });
        assert_eq!(calls.get(), 1);
        assert_eq!(text.matches("发送人: Alice").count(), 2);
    }

    /// 被引用的文本消息里,`@某人` 在正文里只是 `@_user_N` 占位,名字在响应的 `mentions`
    /// 数组里。不接这个数组,引用到 agent 眼里就是「@_user_1 这两件事」—— 它既认不出被
    /// @ 的是谁,也可能把这串占位原样抄进回复。
    ///
    /// 夹具是 owner 那条消息的**真机 mget 响应**,不是手写形状(手写夹具今天已经骗过我两次)。
    #[test]
    fn quoted_text_substitutes_mention_names_from_response() {
        let body = include_str!("../../tests/fixtures/mget_text_with_mentions.json");
        let fetched = parse_fetched_message_response("om_x100b000000000000000000000000001", body)
            .expect("真机响应应能解析");
        let text = fetched.quoted_plaintext_full_with_sender_names(|_| None);
        assert_eq!(text, "@Arcadia 这两件事先做完");
    }

    /// bot 发的消息,`sender.id` 是 `cli_…`(app_id),open_id 在 `sender.open_bot_id`。
    ///
    /// 上层拿它跟 `bot_open_id()`(`ou_…`)比对判「是不是自己发的」,拿 app_id 去比**永远
    /// 不相等** —— 于是引用自己的卡片时走了「别人的消息」分支,把整段正文又灌一遍进 prompt。
    /// owner 2026-08-10 定的省 token 规则对卡片一直是失效的,直到这条。
    #[test]
    fn bot_card_sender_open_id_comes_from_open_bot_id() {
        let body = include_str!("../../tests/fixtures/mget_card_from_bot.json");
        let fetched = parse_fetched_message_response("om_x100b000000000000000000000000002", body)
            .expect("真机响应应能解析");
        assert_eq!(
            fetched.sender_open_id, "ou_11111111111111111111111111111111",
            "bot 卡片必须取到 open_id,取到 cli_ 开头就说明又拿 app_id 去比 open_id 了"
        );
    }

    #[test]
    fn position_parse_handles_missing_and_numeric() {
        let body = include_str!("../../tests/fixtures/mget_card_from_bot.json");
        assert_eq!(
            parse_message_position("om_x100b000000000000000000000000002", body).unwrap(),
            Some("1485".to_string())
        );
        assert_eq!(
            parse_message_position("om_other", body).unwrap(),
            None,
            "按 id 认条目"
        );
        let numeric = r#"{"data":{"items":[{"message_id":"om_1","message_position":42}]}}"#;
        assert_eq!(
            parse_message_position("om_1", numeric).unwrap(),
            Some("42".to_string())
        );
        let absent = r#"{"data":{"items":[{"message_id":"om_1"}]}}"#;
        assert_eq!(parse_message_position("om_1", absent).unwrap(), None);
        assert!(parse_message_position("om_1", "not json").is_err());
    }
}
