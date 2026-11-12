//! agent 身份/环境(A1 拆分):messaging/凭据解析、FEISHU_* 身份隔离、常驻要求注入、表情面板文件。

use super::*;
use crate::runtime::fsio::atomic_write;

/// 注入 system_prompt 时 agent 自我认知(`<dir>/SOUL.md`)的分节标题。
pub(super) const SOUL_TITLE: &str = "rd2b5827140";
/// 注入 system_prompt 时「常驻要求」各层的分节标题。
pub(super) const INSTR_GLOBAL_TITLE: &str = "rb2cec9d548";
pub(super) const INSTR_AGENT_TITLE: &str = "rdfac9c83b5";
pub(super) const INSTR_CHAT_TITLE: &str = "raa9053abec";

/// 脚手架正文:**空**。曾经是一整段「怎么用这个文件」的 HTML 注释,靠 `strip_md_comments`
/// 保证不进 system_prompt —— 那个保证只对 summon 自己拼 PRIMER 的路成立。codex 会沿目录树
/// 自己找 `AGENTS.md`(就是旁边那条软链)并把**原始文件全文**塞进上下文,注释一个字都不少,
/// 于是每个 agent 白吃 248 字符,还读到一句自相矛盾的元指令(「注释不会注入」——而它正在读)。
/// 所以这两个文件里一律不留给人看的内容:说明去 `~/.summon/docs/claude-md-guide.md`。
/// `strip_md_comments` 仍保留 —— 人手写的注释还要靠它兜(对 claude 路有效,codex 路兜不住)。
pub(super) fn claude_example_md() -> &'static str {
    summon_i18n::include_text!("CLAUDE.example.md")
}

/// 确保某「常驻要求」CLAUDE.md 存在(缺则写脚手架)并旁置 `AGENTS.md → CLAUDE.md` 软链,
/// 返回其文本内容。全程 best-effort:IO 失败只 log,返回读到的内容(或空串),绝不中断会话。
pub(super) fn ensure_instruction_file(claude_md: &std::path::Path) -> String {
    if !claude_md.exists() {
        if let Err(e) = atomic_write(claude_md, claude_example_md().as_bytes()) {
            log(&summon_i18n::format!("r5c053ba327", claude_md.display()));
        }
    }
    // 旁置 AGENTS.md 软链(相对,仅当 AGENTS.md 完全不存在时创建,避免覆盖共享项目里的真文件)。
    if let (Some(parent), Some(name)) = (claude_md.parent(), claude_md.file_name()) {
        let agents = parent.join("AGENTS.md");
        if agents.symlink_metadata().is_err() {
            if let Err(e) = std::os::unix::fs::symlink(name, &agents) {
                log(&summon_i18n::format!("r607a9c4ac0", agents.display()));
            }
        }
    }
    std::fs::read_to_string(claude_md).unwrap_or_default()
}

/// 在基础 PRIMER 末尾依次叠加 agent 自我认知 `<dir>/SOUL.md` 与运营「常驻要求」:全局
/// `~/.summon/CLAUDE.md` + per-agent `<dir>/CLAUDE.md` + 本会话 `<dir>/chats/<chat_id>/CLAUDE.md`。
/// 常驻要求前两层按需脚手架 + 旁置 AGENTS.md 软链;SOUL 只读不脚手架(缺失即空层)。
/// 剥注释/空层跳过由纯函数完成。
///
/// SOUL 必须由这里拼进 PRIMER:claude 冷启不发 `/summon` seed(见 `CliDriver::default_seed`),
/// PRIMER 之外没有任何路径会把它交给模型。
pub(super) fn system_prompt_with_instructions(agent: &Agent, chat_id: &str, base: &str) -> String {
    let soul = std::fs::read_to_string(agent.dir.join("SOUL.md")).unwrap_or_default();
    let global = ensure_instruction_file(&summon_home().join("CLAUDE.md"));
    let per_agent = ensure_instruction_file(&agent.dir.join("CLAUDE.md"));
    let per_chat = chat_instruction_path(agent, chat_id)
        .and_then(|path| std::fs::read_to_string(path).ok())
        .unwrap_or_default();
    summon_core::config::compose_system_prompt(
        base,
        &[
            (::summon_core::i18n::label(SOUL_TITLE), &soul),
            (::summon_core::i18n::label(INSTR_GLOBAL_TITLE), &global),
            (::summon_core::i18n::label(INSTR_AGENT_TITLE), &per_agent),
            (::summon_core::i18n::label(INSTR_CHAT_TITLE), &per_chat),
        ],
    )
}

/// 记录用的「这一轮在跑什么模型」:与回复卡角标同源(`cards::panel_model`)。claude 没配顶层
/// `model` 时,模型由 `runtime.env.ANTHROPIC_MODEL` 决定(claude CLI 无 `--model` 时读它),
/// 角标那条链看不到,这里补上 —— 否则这类 agent 的记录永远缺模型。取不到就是 None,不猜默认值。
pub(crate) fn recorded_model(agent: &Agent, tag: &str) -> Option<String> {
    super::cards::panel_model(agent, tag).or_else(|| {
        if agent.cli_kind() != "claude" {
            return None;
        }
        resolve_extra_env(agent)
            .into_iter()
            .find(|(k, _)| k == "ANTHROPIC_MODEL")
            .map(|(_, v)| v.trim().to_string())
            .filter(|v| !v.is_empty())
    })
}

/// 本轮事件记录的出处:agent + 模型 + turn 凭证。turn 读当前活跃轮的 lease(与渲染侧取
/// fencing token 同一个入口),没有已发布的轮(恢复轮等)就是 None。
pub(crate) fn event_stamp(agent: &Agent, tag: &str) -> summon_core::event_log::EventStamp {
    summon_core::event_log::EventStamp {
        agent: Some(agent.name.clone()),
        model: recorded_model(agent, tag),
        turn: crate::runtime::current_turn_store::CurrentTurnStore::for_agent(&agent.state_dir())
            .current_turn_id(tag),
    }
}

/// 本会话「常驻要求」文件:`<agent dir>/chats/<chat_id>/CLAUDE.md`。只读不脚手架 —— 会话数量
/// 没有上界,给每个 chat 都铺一个空文件只会制造噪音;写不写由人决定。chat_id 要当路径的一节用,
/// 带分隔符或 `.`/`..` 的一律不认,免得指到 agent 目录之外。
pub(crate) fn chat_instruction_path(agent: &Agent, chat_id: &str) -> Option<PathBuf> {
    let mut parts = std::path::Path::new(chat_id).components();
    match (parts.next(), parts.next()) {
        (Some(std::path::Component::Normal(_)), None) => {
            Some(agent.dir.join("chats").join(chat_id).join("CLAUDE.md"))
        }
        _ => None,
    }
}

/// agent + 候选内容共同决定的不可变 emoji 文件路径。配置代际不能共写同一路径，否则旧轮
/// PRIMER 指向的候选表会在热更新时变义。
pub(crate) fn emoji_file_path(agent: &Agent, content: &str) -> PathBuf {
    let digest: String = ring::digest::digest(&ring::digest::SHA256, content.as_bytes())
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    agent
        .state_dir()
        .join("runtime")
        .join(format!("emoji-reactions-{digest}.md"))
}

/// 生成 emoji 文件正文:KEY(含义)格式,方便 agent 按含义选用。
pub(crate) fn emoji_file_doc(
    reactions: &[String],
    meanings: &std::collections::HashMap<String, String>,
    lifecycle_reactions: [&str; 2],
) -> String {
    let mut out = String::from(summon_i18n::text!("r5dc91e7203"));
    for key in reactions {
        if lifecycle_reactions.contains(&key.as_str()) {
            continue;
        }
        out.push_str("- ");
        out.push_str(key);
        if let Some(meaning) = meanings.get(key) {
            out.push('（');
            out.push_str(meaning);
            out.push('）');
        }
        out.push('\n');
    }
    out
}

/// 写内容寻址的不可变 emoji 文件。返回路径供 system_prompt 填 {emoji_file}。
pub(crate) fn write_emoji_file(
    agent: &Agent,
    reactions: &[String],
    meanings: &std::collections::HashMap<String, String>,
    lifecycle_reactions: [&str; 2],
) -> anyhow::Result<PathBuf> {
    let content = emoji_file_doc(reactions, meanings, lifecycle_reactions);
    let path = emoji_file_path(agent, &content);
    if let Ok(existing) = std::fs::read_to_string(&path) {
        if existing == content {
            return Ok(path);
        }
    }
    atomic_write(&path, content.as_bytes())?;
    Ok(path)
}

/// 还没有 agent 时(onboard/bind 正在建它)的消息层配置:内置默认 → 套全局 config.yaml 覆盖。
/// 覆盖顺序只在这里和 `resolve_messaging` 定义一次,别在调用点各拼一遍。
pub(crate) fn resolve_messaging_global() -> Messaging {
    let mut m = summon_core::config::builtin_messaging();
    m.apply(&Config::load().messaging);
    m
}

/// 解析某 agent 当前生效的消息层配置:内置默认(messaging.default.yaml)→ 套全局 config.yaml 覆盖
/// → 套该 agent.yaml 覆盖。每次读盘 → 天然热重载。
pub(crate) fn resolve_messaging(agent: &Agent) -> Messaging {
    let mut m = resolve_messaging_global();
    if let Some(o) = agent.read_messaging_override() {
        m.apply(&o);
    }
    m
}

/// 该 agent 的消息被飞书审核以含邮箱拒发时要不要打码重发(`messaging.mask_email`)。每次被拒
/// 时问一次;config.yaml 与 agent.yaml 的修改时间和长度都没变就用上次的结果,任一变了重新
/// 解析 —— 改了即生效,不用重启 daemon。
pub(crate) fn email_mask_switch(agent: &Agent) -> crate::frontend::client::EmailMaskSwitch {
    let agent = agent.clone();
    let stamp_of = |path: &std::path::Path| {
        std::fs::metadata(path)
            .ok()
            .map(|meta| (meta.modified().ok(), meta.len()))
    };
    type Stamp = Option<(Option<SystemTime>, u64)>;
    let cache: std::sync::Mutex<Option<((Stamp, Stamp), bool)>> = std::sync::Mutex::new(None);
    std::sync::Arc::new(move || {
        let stamp = (
            stamp_of(&summon_core::config::config_path()),
            stamp_of(&agent.manifest_path()),
        );
        let mut cache = cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((seen, mask)) = *cache {
            if seen == stamp {
                return mask;
            }
        }
        let mask = resolve_messaging(&agent).mask_email;
        *cache = Some((stamp, mask));
        mask
    })
}

/// 文件 mtime(读不到返回 None)。
pub(super) fn file_mtime(path: &std::path::Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

pub(super) fn agent_owner(agent: &Agent) -> Option<String> {
    agent
        .feishu
        .as_ref()
        .and_then(|f| f.allowed_users.first())
        .cloned()
}

/// owner 的某条 id 是否命中本条消息的发信人。owner(= `allowed_users` 首位)可能配成
/// open_id(`ou_`)或 union_id(`on_`,onboard 现在优先写它);两种 id 任一命中即算 owner ——
/// 与 `Agent::allows` 的双判一致,否则 union_id 白名单下 owner 私聊认不出、丢掉裸 `soul-<alias>` 复用。
pub(super) fn owner_id_matches(owner: &str, open_id: &str, union_id: &str) -> bool {
    owner == open_id || (!union_id.is_empty() && owner == union_id)
}

/// 把一轮发言追加到 <agent.dir>/threads/<chat-tag>/transcript.md(完整对话记录,与 summary 同目录)。
/// 失败静默忽略(日志不该阻断会话)。
pub(super) fn append_transcript(
    agent_dir: &std::path::Path,
    chat_type: &str,
    chat_id: &str,
    role: &str,
    body: &str,
) {
    let path = agent_dir
        .join("threads")
        .join(chat_tag(chat_type, chat_id))
        .join("transcript.md");
    if let Some(p) = path.parent() {
        let _ = std::fs::create_dir_all(p);
    }
    // 与同目录 events.jsonl/outbound.ndjson 共用单代上限，完整对话归档不再无界增长。
    crate::observe::event_log::rotate_if_oversized(
        &path,
        crate::observe::event_log::CHAT_LOG_MAX_BYTES,
    );
    let stamp = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
    let line = format!("### {stamp} {role}\n{body}\n\n");
    let _ = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .and_then(|mut f| f.write_all(line.as_bytes()));
}

/// 记 agent 的一轮回复。末尾的总结标签只给会话列表预览用,不进完整对话记录:agent 之后经
/// `summon chats read` 读回这份记录,标签留着会被当成正文。
pub(super) fn append_reply_transcript(
    agent_dir: &std::path::Path,
    chat_type: &str,
    chat_id: &str,
    role: &str,
    reply: &str,
) {
    append_transcript(
        agent_dir,
        chat_type,
        chat_id,
        role,
        &summon_core::feed_tag::strip_feed_tag(reply),
    );
}

/// 读 agent.dir/.env(简单 dotenv:KEY=VALUE,# 注释)。
pub(super) fn env_from_file(path: &std::path::Path) -> HashMap<String, String> {
    let mut m = HashMap::new();
    if let Ok(s) = std::fs::read_to_string(path) {
        for line in s.lines() {
            let t = line.trim();
            if t.is_empty() || t.starts_with('#') {
                continue;
            }
            if let Some((k, v)) = t.split_once('=') {
                m.insert(k.trim().to_string(), v.trim().trim_matches('"').to_string());
            }
        }
    }
    m
}

/// 解析某 agent 的飞书凭据:优先 per-agent .env,回退进程 env;空值视作缺。
pub fn resolve_creds(agent: &Agent) -> (Option<String>, Option<String>) {
    let envfile = env_from_file(&agent.dir.join(".env"));
    let lookup = |name: &str| -> Option<String> {
        envfile
            .get(name)
            .filter(|v| !v.is_empty())
            .cloned()
            .or_else(|| std::env::var(name).ok().filter(|v| !v.is_empty()))
    };
    let f = agent.feishu.as_ref();
    let id = f.and_then(|f| f.app_id_env.as_deref()).and_then(lookup);
    let sec = f.and_then(|f| f.app_secret_env.as_deref()).and_then(lookup);
    (id, sec)
}

/// 展开 `runtime.env` 值里的 `${VAR}` 引用。查找顺序与 `resolve_creds` 一致:
/// per-agent `.env` 优先,回退进程 env;空值视作缺。
///
/// 任一引用解析不到 → 返回 `None`,由调用方**整条丢弃**。不回退成空串:空值会
/// 覆盖上游同名变量(如 CLI 自己从登录态读到的 token),表现为「配了反而更坏」,
/// 比缺失更难查。未闭合的 `${` 不当引用,原样保留。
fn expand_env_value(raw: &str, lookup: &impl Fn(&str) -> Option<String>) -> Option<String> {
    if !raw.contains("${") {
        return Some(raw.to_string());
    }
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find('}') else {
            // 未闭合:剩余部分原样保留,不视作引用
            out.push_str(&rest[start..]);
            return Some(out);
        };
        let val = lookup(&after[..end])?;
        out.push_str(&val);
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Some(out)
}

/// `runtime.env` 的解析版:值里的 `${VAR}` 已按 `.env` → 进程 env 展开。
///
/// 存在的意义是让 `agent.yaml` 能只写引用、把真值留在已 gitignore 的 `.env` 里,
/// 从而整个 `~/.summon` 仓可以安全推到远端备份。所有注入 agent 进程的地方都应
/// 走本函数,不要直接用 `Agent::extra_env()`(那是未展开的原始配置)。
pub fn resolve_extra_env(agent: &Agent) -> Vec<(String, String)> {
    let envfile = env_from_file(&agent.dir.join(".env"));
    let lookup = |name: &str| -> Option<String> {
        envfile
            .get(name)
            .filter(|v| !v.is_empty())
            .cloned()
            .or_else(|| std::env::var(name).ok().filter(|v| !v.is_empty()))
    };
    agent
        .extra_env()
        .into_iter()
        .filter_map(|(k, raw)| match expand_env_value(&raw, &lookup) {
            Some(v) => Some((k, v)),
            None => {
                log(&summon_i18n::format!("rd52f71ac16", agent.name));
                None
            }
        })
        .collect()
}

/// 飞书运行时身份数据的落盘作用域。alias 防止同一 app 下不同 agent 互相污染；app_id 指纹防止
/// agent 换绑 app 后继续使用旧 app 的 open_id。`allow_legacy_import` 只在注册表**干净**且恰好一个
/// 飞书 agent 时为真：旧全局缓存没有 owner 元数据，多 agent 下自动导入会重新制造跨 app 串身份。
/// 判定见 `legacy_import_allowed`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct FeishuStorageScope {
    pub(super) key: String,
    pub(super) allow_legacy_import: bool,
}

fn stable_fingerprint(s: &str) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

fn storage_scope_key(alias: &str, app_id: Option<&str>) -> String {
    let safe_alias: String = alias
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let alias = if safe_alias.is_empty() {
        "agent"
    } else {
        safe_alias.as_str()
    };
    let identity = app_id.filter(|s| !s.is_empty()).unwrap_or("unbound");
    format!("{alias}-{}", stable_fingerprint(identity))
}

/// 是否允许把升级前的全局身份缓存(identity_cache / peers)copy-on-read 进本 agent 的 scope。
///
/// **只有「注册表干净且恰好一个飞书 agent」才允许**,两个条件缺一不可:
/// - 恰好一个:旧全局文件没有 owner 元数据,多 agent 下归属不可判定,导入 = 把别人的
///   open_id→姓名映射认成自己的。
/// - 干净(无解析失败):tolerant 加载器会把坏清单隔离出 `agents`,于是「两个飞书 agent 坏了
///   一个」会被数成「独苗」。这一步误判是粘死的——`feishu-legacy.scope` claim 让导入一次性、
///   之后无法回退。故任何一份清单解析失败,一律不导入(后续入站会自然重建缓存)。
fn legacy_import_allowed(agents_dir: &std::path::Path) -> bool {
    match Registry::load_tolerant(agents_dir) {
        Ok((reg, warnings)) => {
            warnings.is_empty() && reg.agents.iter().filter(|a| a.feishu.is_some()).count() == 1
        }
        // 目录读不到 = 没有事实可依,fail-closed。
        Err(_) => false,
    }
}

/// 存储 scope 的 **key**(纯计算,不碰盘)。
///
/// **必须与 [`feishu_storage_scope`] 分开**:那个函数为了 `allow_legacy_import` 要
/// `Registry::load_tolerant` 扫整个 agents 目录 + 解析每份 agent.yaml,而绝大多数调用方
/// (身份比较、delivery scope)**只要 key**。
///
/// 2026-08-10 实测:`matches_current` 里那句 `feishu_storage_scope(current).key` 让每个
/// dispatcher 主循环的**每一轮**都全盘扫一次;7 个 agent 就是 7 个线程各自空转扫盘,
/// `summond` 稳定占 70% CPU,采样里 `Registry::load_tolerant` 独占 90% 的样本。
/// key 本身只依赖 alias 与 app_id,一次目录读都不需要。
pub(super) fn feishu_storage_scope_key(agent: &Agent) -> String {
    let alias = agent
        .dir
        .file_name()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or(&agent.name);
    let (app_id, _) = resolve_creds(agent);
    storage_scope_key(alias, app_id.as_deref())
}

/// 完整 scope(key + 历史缓存导入策略)。**只给真正需要 `allow_legacy_import` 的冷路径用**
/// (identity_cache / peers_store 构造时各一次),因为它会扫全盘 —— 热路径一律走
/// [`feishu_storage_scope_key`]。
pub(super) fn feishu_storage_scope(agent: &Agent) -> FeishuStorageScope {
    let (app_id, _) = resolve_creds(agent);
    feishu_storage_scope_from_resolved(agent, app_id.as_deref())
}

fn feishu_storage_scope_from_resolved(agent: &Agent, app_id: Option<&str>) -> FeishuStorageScope {
    let agents_dir = summon_home().join("agents");
    let alias = agent
        .dir
        .file_name()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or(&agent.name);
    FeishuStorageScope {
        key: storage_scope_key(alias, app_id),
        allow_legacy_import: legacy_import_allowed(&agents_dir),
    }
}

/// 出站事件的 `delivery_scope`。**CLI 与 daemon 必须共用本函数**——CLI 入队 outbox 时算的 scope
/// 与 daemon 启动时 capture 的必须逐字节相等,否则 drain 的身份闸门会把事件当成「别的 app 的」
/// 静默丢弃,且不产生任何用户可见报错。只导出 key:`allow_legacy_import` 是 daemon 内部的
/// 历史缓存导入策略,CLI 不该看见。
pub fn feishu_delivery_scope(agent: &Agent) -> String {
    // 同上:只要 key,不该为它扫全盘。
    feishu_storage_scope_key(agent)
}

/// sidecar/client 的 resolved app 身份只在 daemon 启动时建立。只保存 SHA-256，不保留凭据；
/// manifest 字段或 `.env` 值任一漂移都要求重启，不能把旧连接的事件解释成新 app 状态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct FeishuEpochIdentity {
    agent_name: String,
    agent_dir: std::path::PathBuf,
    app_id_env: Option<String>,
    app_secret_env: Option<String>,
    declared_scope: String,
    app_id_digest: Option<[u8; 32]>,
    app_secret_digest: Option<[u8; 32]>,
    storage_scope: FeishuStorageScope,
}

impl FeishuEpochIdentity {
    pub(super) fn capture(agent: &Agent) -> Self {
        let (app_id, app_secret) = resolve_creds(agent);
        Self::from_resolved(agent, app_id.as_deref(), app_secret.as_deref())
    }

    /// 用创建 FeishuClient 的**同一次**凭据解析结果冻结身份。先建 client、稍后再重读 `.env`
    /// 会留下 A-client/B-epoch 窗口：后续校验 B 全绿，实际网络连接却仍拿 A 主动出站。
    pub(super) fn from_resolved(
        agent: &Agent,
        app_id: Option<&str>,
        app_secret: Option<&str>,
    ) -> Self {
        let feishu = agent.feishu.as_ref();
        Self {
            agent_name: agent.name.clone(),
            agent_dir: agent.dir.clone(),
            app_id_env: feishu.and_then(|config| config.app_id_env.clone()),
            app_secret_env: feishu.and_then(|config| config.app_secret_env.clone()),
            declared_scope: feishu.map_or_else(String::new, |config| config.scope.clone()),
            app_id_digest: credential_digest(app_id),
            app_secret_digest: credential_digest(app_secret),
            storage_scope: feishu_storage_scope_from_resolved(agent, app_id),
        }
    }

    pub(super) fn matches_current(&self, current: &Agent) -> bool {
        let current_feishu = current.feishu.as_ref();
        let (app_id, app_secret) = resolve_creds(current);
        self.agent_name == current.name
            && self.agent_dir == current.dir
            && self.app_id_env == current_feishu.and_then(|config| config.app_id_env.clone())
            && self.app_secret_env
                == current_feishu.and_then(|config| config.app_secret_env.clone())
            && self.declared_scope
                == current_feishu.map_or_else(String::new, |config| config.scope.clone())
            && self.app_id_digest == credential_digest(app_id.as_deref())
            && self.app_secret_digest == credential_digest(app_secret.as_deref())
            // 只比 key。**不能用 `feishu_storage_scope`** —— 它为了 `allow_legacy_import` 会扫
            // 全盘,而这里每轮 dispatcher 循环都要跑一次(见 `feishu_storage_scope_key` 的注释)。
            && self.storage_scope.key == feishu_storage_scope_key(current)
    }

    /// 本身份的稳定指纹(SHA-256 hex)。**只是摘要,不是凭据**:daemon 经 control.sock 上报它,
    /// `doctor` 拿盘上重算的同源指纹比对,判「daemon 内存里用的还是不是这份凭据」。
    ///
    /// 必须与 `matches_current` 覆盖**同一组字段**——两者判定分叉就意味着 doctor 报绿而入站已被
    /// 掐,正是这次要修的缺陷本身。回归测 `fingerprint_agrees_with_matches_current` 钉死它。
    pub(super) fn fingerprint(&self) -> String {
        let mut ctx = ring::digest::Context::new(&ring::digest::SHA256);
        // 逐字段「标签 + 长度 + 值」喂入:长度前缀让相邻字段不可能拼接歧义
        //(否则 ("ab","c") 与 ("a","bc") 会撞同一个指纹)。
        let mut feed = |label: &str, value: Option<&[u8]>| {
            ctx.update(label.as_bytes());
            match value {
                Some(bytes) => {
                    ctx.update(b"=");
                    ctx.update(&(bytes.len() as u64).to_le_bytes());
                    ctx.update(bytes);
                }
                // None 与空串必须可区分:app_id_env 未配 vs 配成空是两件事。
                None => ctx.update(b"~"),
            }
            ctx.update(b";");
        };
        feed("agent_name", Some(self.agent_name.as_bytes()));
        feed(
            "agent_dir",
            Some(self.agent_dir.as_os_str().as_encoded_bytes()),
        );
        feed("app_id_env", self.app_id_env.as_deref().map(str::as_bytes));
        feed(
            "app_secret_env",
            self.app_secret_env.as_deref().map(str::as_bytes),
        );
        feed("declared_scope", Some(self.declared_scope.as_bytes()));
        feed("app_id_digest", self.app_id_digest.as_ref().map(|d| &d[..]));
        feed(
            "app_secret_digest",
            self.app_secret_digest.as_ref().map(|d| &d[..]),
        );
        feed("storage_scope", Some(self.storage_scope.key.as_bytes()));
        hex_lower(ctx.finish().as_ref())
    }

    pub(super) fn storage_scope(&self) -> &FeishuStorageScope {
        &self.storage_scope
    }

    pub(super) fn agent_name(&self) -> &str {
        &self.agent_name
    }
}

/// 盘上凭据身份的指纹(与 daemon epoch 同源同算法)。`doctor` 在 CLI 进程里重算它,
/// 与 daemon 上报的 epoch 指纹比对 —— 两个进程必须走同一个 `capture`,不能各写一份。
pub fn feishu_identity_fingerprint(agent: &Agent) -> String {
    FeishuEpochIdentity::capture(agent).fingerprint()
}

fn credential_digest(value: Option<&str>) -> Option<[u8; 32]> {
    let digest = ring::digest::digest(&ring::digest::SHA256, value?.as_bytes());
    digest.as_ref().try_into().ok()
}

fn hex_lower(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::new(), |mut out, b| {
        let _ = write!(out, "{b:02x}");
        out
    })
}

/// 带 mtime 缓存的 alias→name 映射:(agents_dir mtime, 映射表, 上次已 log 的 registry 诊断)。
/// 第三项只服务日志去重:坏清单会让映射为空、进而下面的缓存条件恒不命中(每轮重扫),
/// 不按内容去重就会每轮刷同一行。
type AliasMapCache = (
    Option<std::time::SystemTime>,
    HashMap<String, String>,
    Vec<String>,
);

/// 扫 `~/.summon/agents/*/agent.yaml`,建 `HashMap<alias, name>`(alias = 目录名,name = agent.yaml 里的 name 字段)。
/// name 缺失时 value = alias(回退)。用于出站 @mention 替换时把 `@starlight` 转成 name="星光" 再查 peers。
///
/// 带 mtime 缓存:agents 目录没变就复用上次结果,避免每轮读盘扫目录。缓存用 Mutex 保护(多 chat 线程
/// 并发调),mtime 比对粒度粗但够用——agent 增删不频繁。
pub fn build_alias_name_map() -> HashMap<String, String> {
    use std::sync::{Mutex, OnceLock};
    static CACHE: OnceLock<Mutex<AliasMapCache>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new((None, HashMap::new(), Vec::new())));

    let agents_dir = summon_home().join("agents");
    let dir_mtime = std::fs::metadata(&agents_dir)
        .and_then(|m| m.modified())
        .ok();

    let mut guard = cache.lock().unwrap_or_else(|e| e.into_inner());
    if let (Some(cached_mtime), Some(current_mtime)) = (guard.0, dir_mtime) {
        if cached_mtime == current_mtime && !guard.1.is_empty() {
            return guard.1.clone();
        }
    }

    let mut map = HashMap::new();
    // tolerant:一份 agent.yaml 坏掉只该让它自己的 @mention 替换失效,不该把全体兄弟的映射一起清零
    // (旧的 strict 语义就是这样:任一坏清单 → 整张表为空 → 所有人的 @alias 都不再替换)。
    let warnings = match Registry::load_tolerant(&agents_dir) {
        Ok((reg, warnings)) => {
            for a in &reg.agents {
                let alias = a
                    .dir
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or(&a.name)
                    .to_string();
                map.insert(alias, a.name.clone());
            }
            warnings
        }
        // 目录整体读不到:映射降级为空(@mention 不替换,不影响会话本身),但同样不静默。
        Err(e) => vec![summon_i18n::format!("r07f9d7fec5")],
    };
    if !warnings.is_empty() && warnings != guard.2 {
        for warning in &warnings {
            log(&summon_i18n::format!("r2c70695a4b"));
        }
    }
    *guard = (dir_mtime, map.clone(), warnings);
    map
}

/// 注入子进程的「本 agent 自己」飞书身份环境(规范名 `FEISHU_APP_ID`/`SECRET` + 它自己的 alias 名)。
/// 每个 agent 子进程(无头 / tmux)起前都要叠加它,配合 spawn 的 `FEISHU_*` 清理,保证进程里只有
/// 自己的凭据、绝不串到兄弟身份。缺凭据则空(fail-closed)。
///
/// 同时叠加 lark-cli 身份隔离(见 `agent_lark_cli_env`):lark-cli 不读 `FEISHU_APP_ID` 环境变量、
/// 只读自己的 `~/.lark-cli/config.json`,故需为每个 agent 备一个 app_id 对得上的 profile + wrapper
/// 脚本、把 wrapper 目录前置 PATH,让 agent 会话里的 `lark-cli` 自动 pin 到它自己的 app 身份。
pub fn agent_identity_env(agent: &Agent) -> Vec<(String, String)> {
    let (id, sec) = resolve_creds(agent);
    let f = agent.feishu.as_ref();
    let mut env = summon_core::config::agent_identity::feishu_self_inject(
        f.and_then(|f| f.app_id_env.as_deref()),
        id.as_deref(),
        f.and_then(|f| f.app_secret_env.as_deref()),
        sec.as_deref(),
    );
    // lark-cli 身份隔离:内部 fail-closed(对不上账就禁掉 lark-cli),但不阻断本轮会话。
    env.extend(agent_lark_cli_env(agent));
    env
}

/// lark-cli 身份隔离:为 agent 备好一个「app_id 与它自己凭据逐字符一致」的 profile + wrapper 脚本,
/// 把 wrapper 目录前置到 PATH,让 agent 会话里所有 `lark-cli` 调用自动 pin 到那个 profile。
///
/// 背景:summon 的 `FEISHU_*` 环境变量隔离对 lark-cli 无效——它只读自己的 `~/.lark-cli/config.json`,
/// 全局只绑了一个 app(通常是首个配置的 agent),导致其他 agent 用 lark-cli 时串身份(如 agent A
/// 用 `--as bot` 发评论,实际落的是 agent B 的 bot)。
///
/// **fail-closed**:对账拿不到结论、或修复 profile 失败时,写的是「拒绝执行」的 wrapper 而不是
/// 撤掉 PATH 前置。撤掉等于把调用放回全局默认 profile,那正是别的 agent 的身份(红线)。
///
/// 返回要注入的环境变量(目前只有 PATH 前置)。只有两种情况完全不介入:agent 没有自己的飞书凭据
/// (没有可 pin 的身份),或本机根本没装 lark-cli(无从串身份)。
pub fn agent_lark_cli_env(agent: &Agent) -> Vec<(String, String)> {
    let Some(alias) = agent_alias(agent) else {
        return Vec::new();
    };
    // lark-cli 没装:既没有可串的身份,也写不出 wrapper —— wrapper 只能 exec 裸名 `lark-cli`,
    // 而 wrapper 目录就在 PATH 最前,会解析回自己变成无限 exec。
    let Some(real_lark_cli) = which_lark_cli() else {
        return Vec::new();
    };
    // 非受管 agent(单测用 tmp 目录伪造的那种)不该在真实 home 下落 wrapper,也没有真身份要隔离。
    if !is_managed_agent_dir(&agent.dir) {
        return Vec::new();
    }
    // 没配飞书凭据的 agent 没有"自己的"身份可 pin,维持现状不介入。
    let (Some(app_id), Some(app_secret)) = resolve_creds(agent) else {
        return Vec::new();
    };
    // ① 对账 + 修复,拿到该 pin 的 profile 名
    let pinned = match ensure_lark_cli_profile(&real_lark_cli, &alias, &app_id, &app_secret) {
        Ok(name) => Some(name),
        Err(e) => {
            log(&summon_i18n::format!("rf2ff3d10c4"));
            None
        }
    };
    // ② 写 wrapper 脚本(pin 或拒绝)
    let wrapper = match pinned.as_deref() {
        Some(profile) => LarkCliWrapper::Pin {
            real_lark_cli: &real_lark_cli,
            profile,
        },
        None => LarkCliWrapper::Deny,
    };
    let wrapper_dir = match write_lark_cli_wrapper(&alias, &wrapper) {
        Ok(dir) => dir,
        Err(e) => {
            // 落盘失败就无法施加隔离。这里只能放弃 PATH 前置(下游会回落到全局 profile),
            // 故必须留下明确日志,而不是静默。
            log(&summon_i18n::format!("r63627414a0"));
            return Vec::new();
        }
    };
    // ③ 把 wrapper 目录前置到 PATH(daemon 当前 PATH;agent 子进程会从 essential_system_env 继承)
    let current_path = std::env::var("PATH").unwrap_or_default();
    let new_path = format!("{}:{}", wrapper_dir.display(), current_path);
    vec![("PATH".to_string(), new_path)]
}

fn agent_alias(agent: &Agent) -> Option<String> {
    agent
        .dir
        .file_name()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
}

/// 全部 wrapper 脚本的落盘根目录。单独抽出来,是为了能判断「某个路径是不是我们自己写的 wrapper」。
fn lark_cli_wrapper_root() -> PathBuf {
    summon_home()
        .join("runtime")
        .join(LARK_CLI_WRAPPER_DIR_NAME)
}

/// 该路径是否落在 wrapper 根目录里(= 它是我们自己写的 wrapper,不是真 lark-cli)。
/// 这里按**路径分量**比(`Path::starts_with`),不用字符串包含:PATH 条目过滤宁可多剔一点,
/// 但写进 wrapper 的 exec 目标要精确判断,不该把 `lark-cli-wrappers-bak/` 之类误判掉。
fn is_lark_cli_wrapper_path(path: &str) -> bool {
    std::path::Path::new(path).starts_with(lark_cli_wrapper_root())
}

/// 只有 `<summon_home>/agents/<alias>/` 下的 agent 才是「受管 agent」。
///
/// 为什么要这道判断:`agent_alias()` 取的是 `agent.dir` 的目录基名,而单测会用 tmp 目录伪造
/// `Agent`(`session.rs` 的飞书身份隔离回归就是,基名恰好是 `self`)。没有这道判断,跑一次
/// `cargo test` 就会在**真实** `~/.summon/runtime/lark-cli-wrappers/` 下落一个 `self/` 目录 ——
/// 2026-08-04 在 owner 机器上真实出现过这个不存在的 alias。伪造的 Agent 也没有真身份要隔离。
fn is_managed_agent_dir(dir: &std::path::Path) -> bool {
    dir.parent() == Some(summon_home().join("agents").as_path())
}

fn lark_cli_wrapper_dir(alias: &str) -> PathBuf {
    lark_cli_wrapper_root().join(alias)
}

const LARK_CLI_WRAPPER_DIR_NAME: &str = "lark-cli-wrappers";

/// 真实 lark-cli 的绝对路径。找不到 → None。
///
/// **必须先把 wrapper 目录从 PATH 里剔掉再 `which`。** 原实现假设「daemon 自身 PATH 干净」——
/// 这条假设是错的:daemon 从一个被 summon 托管的 pane 里启动时(在里面跑 `deploy.sh` 就是),
/// 继承的 PATH 第一项正是 `<runtime>/lark-cli-wrappers/<alias>`。于是 `which lark-cli` 解析到
/// wrapper 自己,再把它写进新 wrapper → `exec 自己` → 任何 lark-cli 调用无限 fork/exec 空转。
/// 2026-08-04 真实踩到:arcadia 的 wrapper 被写成自我递归,连带把
/// `augment_launch_isolates_feishu_identity_from_sibling` 挂死 1 小时 34 分(CPU 空转、不报错)。
fn which_lark_cli() -> Option<String> {
    let sanitized = crate::setup::env::path_without_lark_cli_wrappers(
        &std::env::var_os("PATH").unwrap_or_default(),
    );
    let requested = summon_core::tools::resolve(summon_core::tools::Tool::LarkCli);
    summon_core::tools::find_executable(&requested, &sanitized)
        .filter(|path| !is_lark_cli_wrapper_path(&path.to_string_lossy()))
        .map(|path| path.to_string_lossy().to_string())
}

/// 「本 agent 的飞书身份」与 lark-cli profile 的对账结论。
///
/// 不变量:wrapper 只允许 pin 到一个 `appId` 与 agent 自己凭据逐字符相等的 profile,
/// 否则宁可什么都不 pin。原实现只比 profile name,凭据轮换后会一路沿用旧 app 身份。
#[derive(Debug, Clone, PartialEq, Eq)]
enum LarkProfileAction {
    /// 同名 profile 已经绑着正确的 app → 不动。
    Keep,
    /// 正确的 app 挂在别的 profile 名下 → 改 pin 到它,不做任何写操作。
    /// lark-cli 强制 app-id 全局唯一,这种情况下重建同名 profile 必然被拒。
    Adopt(String),
    /// 没有任何 profile 绑这个 app,且 alias 名空闲 → 新建。
    Create,
    /// 同名 profile 绑着别的 app,且正确的 app 还没被占用 → 先删再建。
    Recreate,
    /// 判不出结论 → fail-closed,绝不沿用。
    Error(String),
}

/// 从 profile 条目里取 app_id。lark-cli 实际输出 camelCase 的 `appId`,同时容忍 `app_id`
/// 以防上游改写风格;空串按"没有"处理(空 app_id 证明不了任何身份)。
fn profile_app_id(entry: &serde_json::Value) -> Option<&str> {
    entry
        .get("appId")
        .or_else(|| entry.get("app_id"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
}

/// 纯对账:`lark-cli profile list` 的 stdout + 本 agent 的 alias/app_id → 该做什么。
/// 不碰进程/文件,便于把全部分支钉在单测里。
fn reconcile_lark_cli_profile(
    list_stdout: &str,
    alias: &str,
    expected_app_id: &str,
) -> LarkProfileAction {
    if expected_app_id.is_empty() {
        return LarkProfileAction::Error(summon_i18n::text!("r6f3787014f").to_string());
    }
    let parsed: serde_json::Value = match serde_json::from_str(list_stdout.trim()) {
        Ok(v) => v,
        Err(e) => {
            return LarkProfileAction::Error(summon_i18n::format!("rf2cb32ffd6"));
        }
    };
    let Some(entries) = parsed.as_array() else {
        return LarkProfileAction::Error(summon_i18n::text!("r924d93c1d0").to_string());
    };

    let mut same_name: Vec<&serde_json::Value> = Vec::new();
    let mut same_app: Vec<&str> = Vec::new();
    for entry in entries {
        // 条目形状不认识就整体判负:无法排除它就是 alias 那条。
        if !entry.is_object() {
            return LarkProfileAction::Error(summon_i18n::text!("r5907b5e112").to_string());
        }
        let name = entry
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        if name == alias {
            same_name.push(entry);
        } else if !name.is_empty() && profile_app_id(entry) == Some(expected_app_id) {
            // 没名字的条目 pin 不上(`--profile ''` 只会报 not found),不收进候选。
            same_app.push(name);
        }
    }
    if same_name.len() > 1 {
        return LarkProfileAction::Error(summon_i18n::format!("re124a26a69"));
    }

    match same_name.first() {
        Some(entry) => match profile_app_id(entry) {
            // 上游哪天不再输出 app_id,就不能再假装"同名即同身份"。
            None => LarkProfileAction::Error(summon_i18n::format!("r168124925e")),
            Some(current) if current == expected_app_id => LarkProfileAction::Keep,
            Some(_) => match same_app.first() {
                Some(other) => LarkProfileAction::Adopt((*other).to_string()),
                None => LarkProfileAction::Recreate,
            },
        },
        None => match same_app.first() {
            Some(other) => LarkProfileAction::Adopt((*other).to_string()),
            None => LarkProfileAction::Create,
        },
    }
}

/// 对账 + 按需修复,返回 wrapper 应当 pin 的 profile 名。
/// 任何不确定(读不到 list、解析不了、修不好)一律 `Err` —— 调用方据此写拒绝型 wrapper。
fn ensure_lark_cli_profile(
    lark_cli: &str,
    alias: &str,
    app_id: &str,
    app_secret: &str,
) -> anyhow::Result<String> {
    let listed = lark_cli_profile_list(lark_cli).context(summon_i18n::text!("r4fae6adccc"))?;
    match reconcile_lark_cli_profile(&listed, alias, app_id) {
        LarkProfileAction::Keep => Ok(alias.to_string()),
        LarkProfileAction::Adopt(name) => Ok(name),
        LarkProfileAction::Create => {
            lark_cli_profile_add(lark_cli, alias, app_id, app_secret)?;
            Ok(alias.to_string())
        }
        LarkProfileAction::Recreate => {
            // 顺序不能反:先删掉绑着旧 app 的同名 profile。删成功而建失败时盘上留下的是
            // "没有这个 profile" → wrapper 的 `--profile` 会硬报错(fail-closed);反过来
            // 则会留着旧 app 继续可用(fail-open,正是本次要堵的口子)。
            lark_cli_profile_remove(lark_cli, alias)
                .with_context(|| summon_i18n::format!("r71e7ff8d9e"))?;
            lark_cli_profile_add(lark_cli, alias, app_id, app_secret)?;
            Ok(alias.to_string())
        }
        LarkProfileAction::Error(why) => anyhow::bail!(summon_i18n::format!("rc3e10ecfd9")),
    }
}

fn lark_cli_profile_list(lark_cli: &str) -> anyhow::Result<String> {
    let out = Command::new(lark_cli).args(["profile", "list"]).output()?;
    if !out.status.success() {
        anyhow::bail!(summon_i18n::format!("r76cd525175", out.status.code()));
    }
    String::from_utf8(out.stdout).context(summon_i18n::text!("rcce9f01220"))
}

/// 只删 summon 自己按 alias 建的那条 profile;`Adopt` 分支下别人的 profile 一律不碰。
fn lark_cli_profile_remove(lark_cli: &str, alias: &str) -> anyhow::Result<()> {
    let status = Command::new(lark_cli)
        .args(["profile", "remove", alias])
        // 若上游哪天加了交互确认,stdin 立刻 EOF 会让它失败而不是挂死;失败即 fail-closed。
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    if !status.success() {
        anyhow::bail!(summon_i18n::format!("r6df2331e44", status.code()));
    }
    Ok(())
}

fn lark_cli_profile_add(
    lark_cli: &str,
    alias: &str,
    app_id: &str,
    app_secret: &str,
) -> anyhow::Result<()> {
    // secret 走 stdin,避免出现在 ps 的命令行里。
    let mut child = Command::new(lark_cli)
        .args([
            "profile",
            "add",
            "--name",
            alias,
            "--app-id",
            app_id,
            "--app-secret-stdin",
            "--brand",
            "feishu",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        // 写失败不单独报错:紧接着的 wait 会因缺 secret 拿到非零退出码,由那里统一判负。
        let _ = stdin.write_all(app_secret.as_bytes());
    }
    let status = child.wait()?;
    if !status.success() {
        anyhow::bail!(summon_i18n::format!("rb0b456a021", status.code()));
    }
    Ok(())
}

/// wrapper 脚本的两种形态。
enum LarkCliWrapper<'a> {
    /// 对账通过:pin 到确认过 app_id 的 profile。
    Pin {
        real_lark_cli: &'a str,
        profile: &'a str,
    },
    /// 对账未达成:拒绝执行。不能放行——放行会落到全局默认 profile 或残留的旧 app profile 上,
    /// 两者都是别的 agent 的身份。
    Deny,
}

/// 78 = sysexits.h 的 `EX_CONFIG`,让 agent 一眼看出是配置/身份问题而不是命令本身失败。
const LARK_CLI_DENY_EXIT: u8 = 78;

fn lark_cli_wrapper_script(alias: &str, wrapper: &LarkCliWrapper) -> String {
    match wrapper {
        // 最后一道闸:exec 目标落在 wrapper 根目录里就是自引用,会无限 exec。上游 `which_lark_cli`
        // 已经先剔 PATH 再解析,这里再兜一次 —— 挂死比 fail-closed 更糟(agent 只会超时空转,
        // 连「隔离失效了」都无从察觉),所以宁可降级成拒绝型,也绝不写出一个会挂死的 wrapper。
        LarkCliWrapper::Pin {
            real_lark_cli,
            profile,
        } if !is_lark_cli_wrapper_path(real_lark_cli) => format!(
            "#!/bin/sh\nexec {} --profile {} \"$@\"\n",
            shell_escape(real_lark_cli),
            shell_escape(profile)
        ),
        LarkCliWrapper::Pin { .. } | LarkCliWrapper::Deny => format!(
            "#!/bin/sh\nprintf '%s\\n' {} >&2\nexit {LARK_CLI_DENY_EXIT}\n",
            // 只给通用提示,不带 app_id / 错误详情:wrapper 脚本是要落盘的。
            shell_escape(&summon_i18n::format!("rea24696fa4"))
        ),
    }
}

/// 写 wrapper 脚本到 `<runtime>/lark-cli-wrappers/<alias>/lark-cli`,返回其所在目录(供 PATH 前置)。
/// 每次重写:pin 的 profile / lark-cli 路径都可能变,而且要能把上一轮的 Deny 覆盖回 Pin。
fn write_lark_cli_wrapper(alias: &str, wrapper: &LarkCliWrapper) -> anyhow::Result<PathBuf> {
    let dir = lark_cli_wrapper_dir(alias);
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("lark-cli");
    atomic_write(&path, lark_cli_wrapper_script(alias, wrapper).as_bytes())?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
    Ok(dir)
}

fn shell_escape(s: &str) -> String {
    // 单引号转义:把 ' 替换为 '\''
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// tmux 路径的飞书身份隔离材料:`(要 env -u 清掉的全部身份键, 要设回的本 agent 自己凭据)`。
/// tmux pane 继承的是 tmux server 环境(无法在 Rust 里按前缀枚举),故按「所有已知 agent 的身份键名」
/// 精确清——从 agents 目录(本 agent.dir 的父目录)整份 tolerant 加载收集每个 agent 的
/// `app_id_env`/`app_secret_env` 名,再 `feishu_unset_keys` 去重 + 补规范名。registry 读不到则只清
/// 规范名(降级,仍清掉 stray `FEISHU_APP_*`)。
///
/// why tolerant:这里收的是「要清掉的键名」,收得越全越安全。旧的 strict 语义下,任何一份
/// agent.yaml 坏掉都会让 ① 整个失效——一个兄弟键都清不到,反而是更危险的降级。
pub fn agent_feishu_isolation_tmux(agent: &Agent) -> (Vec<String>, Vec<(String, String)>) {
    let inject = agent_identity_env(agent);
    let mut names: Vec<String> = Vec::new();
    // ① 按在册 agent 配置名收集(覆盖所有兄弟身份键)。
    if let Some(parent) = agent.dir.parent() {
        match summon_core::registry::Registry::load_tolerant(parent) {
            Ok((reg, warnings)) => {
                // 坏清单里声明的身份键名收集不到:② 的前缀枚举兜得住「当前 daemon 环境里真实存在」
                // 的那些,兜不住「只写在配置里、此刻不在继承链上」的,故必须留诊断而非静默。
                for warning in warnings {
                    log(&summon_i18n::format!("rdf1d6dce89"));
                }
                for a in &reg.agents {
                    if let Some(f) = a.feishu.as_ref() {
                        if let Some(n) = f.app_id_env.as_deref() {
                            names.push(n.to_string());
                        }
                        if let Some(n) = f.app_secret_env.as_deref() {
                            names.push(n.to_string());
                        }
                    }
                }
            }
            Err(e) => log(&summon_i18n::format!("r867714299a")),
        }
    }
    // ② 前缀级补全(review #10):额外枚举 daemon 自身环境里所有 `FEISHU_*` 键(它是 tmux server 环境的
    // 上界近似),与 headless 路的「无条件清前缀」对称——即便某键不在任何在册 agent 配置里(stray /
    // 已下线 / 手动 export),只要在继承链上就被 -u 掉,fail-closed。
    for (k, _) in std::env::vars() {
        if summon_core::config::agent_identity::is_feishu_identity_key(&k) {
            names.push(k);
        }
    }
    let unset = summon_core::config::agent_identity::feishu_unset_keys(&names);
    (unset, inject)
}

/// 启动 agent 会话时必须保留的系统环境变量白名单。
/// 这些是 CLI 正常运行的最低依赖(PATH/HOME/TERM 等),其余(ANTHROPIC_*、HTTP_PROXY 等)
/// 一律不继承——agent 需要的在自己 agent.yaml 的 runtime.env 里显式配。
///
/// 与 `agent_feishu_isolation_tmux` 配合:tmux 路走 `env -i`(清空继承)+ 本函数注入系统基础变量;
/// 无头路走 `cmd.env_clear()` + 本函数注入。两条路径都只保留这里列出的变量,杜绝 daemon 环境里的
/// ANTHROPIC_* 等泄漏到兄弟 agent 会话。
pub fn essential_system_env() -> Vec<(String, String)> {
    let keys = [
        "PATH",
        "HOME",
        "USER",
        "SHELL",
        "TERM",
        "LANG",
        "LC_ALL",
        "LC_CTYPE",
        "LC_COLLATE",
        "LC_MESSAGES",
        "LC_NUMERIC",
        "LC_TIME",
        "TMPDIR",
        "TMP",
        "TEMP",
        // 代理:agent 可能需要(CLI 访问 API)
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "NO_PROXY",
        "http_proxy",
        "https_proxy",
        "no_proxy",
        // XDG
        "XDG_CACHE_HOME",
        "XDG_CONFIG_HOME",
        "XDG_DATA_HOME",
        "XDG_RUNTIME_DIR",
        // GUI / 密钥
        "DISPLAY",
        "WAYLAND_DISPLAY",
        "SSH_AUTH_SOCK",
        "SSH_AGENT_PID",
        "GPG_AGENT_INFO",
        "DBUS_SESSION_BUS_ADDRESS",
        // macOS 特有
        "__CF_USER_TEXT_ENCODING",
    ];
    let mut out = Vec::new();
    for k in keys {
        if let Ok(v) = std::env::var(k) {
            out.push((k.to_string(), v));
        }
    }
    // SUMMON_* 前缀:daemon 注入的运行时变量(如 SUMMON_HOME、SUMMON_LOG 等)。
    //
    // **身份三键除外**(`SUMMON_AGENT` / `SUMMON_CHAT_ID` / `SUMMON_CONVERSATION_TAG`):它们回答
    // 的是「你是谁、在哪个会话」,只能由调用方按**当前上下文**显式注入,绝不能从 daemon 自己的
    // 环境继承 —— daemon 常常是从某个 agent 的会话里被拉起来的,那份身份对它的子进程全是错的。
    //
    // 不排除会怎样(2026-08-14 现场):daemon 带着 agent B 的 `SUMMON_AGENT` 启动,这个循环
    // 把它复制进每个子进程的环境,子进程里的 CLI 据此认领 agent B 的身份 —— `summon preview`
    // 把内容发进 agent B 的群、`summon chats` 读出 agent B 的会话索引。
    // 是跨群数据泄露,不是显示问题。
    for (k, v) in std::env::vars() {
        if k.starts_with("SUMMON_") && !is_runtime_identity_env(&k) {
            out.push((k, v));
        }
    }
    out
}

/// 是不是「当前轮身份」这一类环境变量 —— 它们必须每次显式注入,不得继承。
///
/// 单独抽出来是为了让「哪些键属于身份」只有一份定义:daemon 启动剥离、子进程环境构造、
/// 将来任何新的 spawn 边界都读它,不各写一份清单(漏一个就是一条泄漏路径)。
pub fn is_runtime_identity_env(key: &str) -> bool {
    matches!(
        key,
        "SUMMON_AGENT" | "SUMMON_CHAT_ID" | "SUMMON_CONVERSATION_TAG"
    )
}

#[cfg(test)]
mod lark_cli_profile_tests {
    use super::*;

    /// wrapper 目录必须先从 PATH 里剔掉再 `which`。
    ///
    /// **不剔会怎样**(2026-08-04 真实事故):daemon 若从一个已被注入 wrapper 目录的会话启动
    /// (在被 summon 托管的 pane 里跑 `deploy.sh` 就是),`which lark-cli` 解析到的是 wrapper
    /// 自己,写进新 wrapper 就成了 `exec 自己` —— 任何 lark-cli 调用无限 fork/exec 空转、
    /// 永不返回,还把 `augment_launch_isolates_feishu_identity_from_sibling` 一起挂死 1.5 小时。
    #[test]
    fn path_sanitizer_drops_wrapper_dirs_and_keeps_the_rest() {
        let wrapper = lark_cli_wrapper_dir("arcadia");
        let dirty = format!("{}:/opt/homebrew/bin:/usr/bin", wrapper.display());
        assert_eq!(
            crate::setup::env::path_without_lark_cli_wrappers(std::ffi::OsStr::new(&dirty)),
            std::ffi::OsString::from("/opt/homebrew/bin:/usr/bin"),
            "wrapper 目录必须被剔除,其余顺序原样保留"
        );
        // 别的 agent 的 wrapper 目录同样要剔:自举污染不分 alias。
        let sibling = lark_cli_wrapper_dir("ambrose");
        assert_eq!(
            crate::setup::env::path_without_lark_cli_wrappers(std::ffi::OsStr::new(&format!(
                "/usr/bin:{}",
                sibling.display()
            ))),
            std::ffi::OsString::from("/usr/bin")
        );
        // 干净 PATH 原样返回;空 PATH 不 panic。
        assert_eq!(
            crate::setup::env::path_without_lark_cli_wrappers(std::ffi::OsStr::new(
                "/usr/bin:/bin"
            )),
            std::ffi::OsString::from("/usr/bin:/bin")
        );
        assert_eq!(
            crate::setup::env::path_without_lark_cli_wrappers(std::ffi::OsStr::new("")),
            std::ffi::OsString::new()
        );
    }

    /// 真机 `lark-cli profile list` 的形状(键名是 camelCase 的 `appId`)。不含任何密钥——
    /// profile list 本来就不输出 app_secret,fixture 里也绝不能出现。
    const REAL_SHAPE: &str = r#"[
      {"name":"cli_app_default","appId":"cli_app_default","brand":"feishu","active":true,
       "user":"someone","tokenStatus":"needs_refresh"},
      {"name":"blue-wing","appId":"cli_blue_wing","brand":"feishu","active":false}
    ]"#;

    #[test]
    fn keeps_profile_when_app_id_matches() {
        assert_eq!(
            reconcile_lark_cli_profile(REAL_SHAPE, "blue-wing", "cli_blue_wing"),
            LarkProfileAction::Keep
        );
    }

    #[test]
    fn recreates_profile_when_app_id_drifted() {
        // 凭据轮换后同名 profile 还绑着旧 app —— 原实现只比 name,会一路沿用旧身份。
        assert_eq!(
            reconcile_lark_cli_profile(REAL_SHAPE, "blue-wing", "cli_rotated_new"),
            LarkProfileAction::Recreate
        );
    }

    #[test]
    fn creates_profile_when_absent() {
        assert_eq!(
            reconcile_lark_cli_profile(REAL_SHAPE, "ambrose", "cli_ambrose"),
            LarkProfileAction::Create
        );
    }

    #[test]
    fn adopts_profile_bound_to_same_app_under_another_name() {
        // lark-cli 强制 app-id 全局唯一("each profile must have a unique app-id"),
        // 同一个 app 已被别名 profile 占了就只能复用它,不然 add 必失败。
        assert_eq!(
            reconcile_lark_cli_profile(REAL_SHAPE, "ambrose", "cli_app_default"),
            LarkProfileAction::Adopt("cli_app_default".to_string())
        );
    }

    #[test]
    fn adopts_instead_of_recreating_when_expected_app_taken_by_other_profile() {
        // 同名 profile 绑错 app,而对的 app 挂在别的名字下:重建会撞唯一约束,只能改 pin。
        assert_eq!(
            reconcile_lark_cli_profile(REAL_SHAPE, "blue-wing", "cli_app_default"),
            LarkProfileAction::Adopt("cli_app_default".to_string())
        );
    }

    #[test]
    fn accepts_snake_case_app_id_key() {
        let list = r#"[{"name":"blue-wing","app_id":"cli_blue_wing"}]"#;
        assert_eq!(
            reconcile_lark_cli_profile(list, "blue-wing", "cli_blue_wing"),
            LarkProfileAction::Keep
        );
    }

    #[test]
    fn errors_when_output_is_not_json() {
        // 对账拿不到事实就必须报错:沿用 = 继续用未知身份(fail-open),正是要堵的口子。
        assert!(matches!(
            reconcile_lark_cli_profile("lark-cli: command failed", "blue-wing", "cli_blue_wing"),
            LarkProfileAction::Error(_)
        ));
    }

    #[test]
    fn errors_when_output_is_not_array() {
        assert!(matches!(
            reconcile_lark_cli_profile(r#"{"profiles":[]}"#, "blue-wing", "cli_blue_wing"),
            LarkProfileAction::Error(_)
        ));
    }

    #[test]
    fn errors_when_matching_profile_hides_app_id() {
        // 上游哪天不再输出 appId,就不能再假装"同名即同身份"。
        let list = r#"[{"name":"blue-wing","brand":"feishu"}]"#;
        assert!(matches!(
            reconcile_lark_cli_profile(list, "blue-wing", "cli_blue_wing"),
            LarkProfileAction::Error(_)
        ));
    }

    #[test]
    fn errors_on_duplicate_profile_names() {
        let list = r#"[{"name":"blue-wing","appId":"cli_a"},{"name":"blue-wing","appId":"cli_b"}]"#;
        assert!(matches!(
            reconcile_lark_cli_profile(list, "blue-wing", "cli_a"),
            LarkProfileAction::Error(_)
        ));
    }

    #[test]
    fn errors_on_malformed_entry() {
        let list = r#"["blue-wing",{"name":"blue-wing","appId":"cli_blue_wing"}]"#;
        assert!(matches!(
            reconcile_lark_cli_profile(list, "blue-wing", "cli_blue_wing"),
            LarkProfileAction::Error(_)
        ));
    }

    #[test]
    fn never_adopts_a_nameless_profile() {
        // 匿名条目 pin 不上,只能当它不存在 → 老老实实新建自己的。
        let list = r#"[{"appId":"cli_ambrose","brand":"feishu"}]"#;
        assert_eq!(
            reconcile_lark_cli_profile(list, "ambrose", "cli_ambrose"),
            LarkProfileAction::Create
        );
    }

    #[test]
    fn errors_when_expected_app_id_is_empty() {
        assert!(matches!(
            reconcile_lark_cli_profile(REAL_SHAPE, "blue-wing", ""),
            LarkProfileAction::Error(_)
        ));
    }

    #[test]
    fn pin_wrapper_execs_real_binary_with_quoted_profile() {
        let script = lark_cli_wrapper_script(
            "blue-wing",
            &LarkCliWrapper::Pin {
                real_lark_cli: "/opt/bin/lark cli",
                profile: "cli_app_default",
            },
        );
        assert!(script.starts_with("#!/bin/sh\n"));
        assert!(script.contains("exec '/opt/bin/lark cli' --profile 'cli_app_default' \"$@\""));
    }

    #[test]
    fn deny_wrapper_never_execs_lark_cli() {
        let script = lark_cli_wrapper_script("blue-wing", &LarkCliWrapper::Deny);
        assert!(
            !script.contains("exec "),
            "拒绝型 wrapper 绝不能放行到任何 lark-cli"
        );
        assert!(script.contains("exit 78"));
        assert!(script.contains("blue-wing"));
    }

    #[test]
    fn wrapper_script_escapes_single_quotes() {
        let script = lark_cli_wrapper_script(
            "a'b",
            &LarkCliWrapper::Pin {
                real_lark_cli: "/bin/lark-cli",
                profile: "a'b",
            },
        );
        assert!(script.contains(r#"--profile 'a'\''b'"#));
    }

    /// 把脚本落成可执行文件后真跑一遍(只依赖 /bin/sh),验证的是 shell 语义而不是字符串长相。
    fn run_wrapper(wrapper: &LarkCliWrapper, args: &[&str]) -> (Option<i32>, String, String) {
        let tmp = tempfile::tempdir().expect("建临时 wrapper 目录");
        let path = tmp.path().join("lark-cli");
        crate::test_support::write_test_executable(
            &path,
            &lark_cli_wrapper_script("blue-wing", wrapper),
        );
        let out = Command::new(&path).args(args).output().expect("跑 wrapper");
        (
            out.status.code(),
            String::from_utf8_lossy(&out.stdout).to_string(),
            String::from_utf8_lossy(&out.stderr).to_string(),
        )
    }

    #[test]
    fn pin_wrapper_forwards_profile_and_user_args() {
        // 用 /bin/echo 冒充真 lark-cli,把最终 argv 打出来比对。
        let (code, stdout, _) = run_wrapper(
            &LarkCliWrapper::Pin {
                real_lark_cli: "/bin/echo",
                profile: "cli_app_default",
            },
            &["im", "+chat-messages-list"],
        );
        assert_eq!(code, Some(0));
        assert_eq!(
            stdout.trim(),
            "--profile cli_app_default im +chat-messages-list"
        );
    }

    #[test]
    fn deny_wrapper_refuses_and_reports_on_stderr() {
        let (code, stdout, stderr) = run_wrapper(&LarkCliWrapper::Deny, &["im", "+chat-list"]);
        assert_eq!(code, Some(LARK_CLI_DENY_EXIT as i32), "必须非零退出");
        assert!(stdout.is_empty(), "拒绝路径不该往 stdout 吐东西污染解析");
        assert!(stderr.contains("blue-wing"), "实际 stderr: {stderr}");
    }

    #[test]
    fn wrapper_own_path_is_recognized_as_wrapper() {
        let own = lark_cli_wrapper_dir("arcadia").join("lark-cli");
        assert!(
            is_lark_cli_wrapper_path(&own.to_string_lossy()),
            "wrapper 自己的路径必须被认出来: {}",
            own.display()
        );
    }

    /// exec 目标的判定按路径分量比,不用字符串包含 —— 否则 `lark-cli-wrappers-bak/` 这类
    /// 同前缀的兄弟目录会被误判成 wrapper,把本来可用的真二进制挡掉降级成拒绝型。
    /// (PATH 条目过滤是另一回事,那边宁可多剔,见 `path_without_wrapper_dirs`。)
    #[test]
    fn sibling_dir_sharing_the_prefix_is_not_a_wrapper() {
        let sibling =
            PathBuf::from(format!("{}-bak", lark_cli_wrapper_root().display())).join("lark-cli");
        assert!(
            !is_lark_cli_wrapper_path(&sibling.to_string_lossy()),
            "同前缀兄弟目录不该被误判: {}",
            sibling.display()
        );
        assert!(!is_lark_cli_wrapper_path("/usr/local/bin/lark-cli"));
    }

    /// 纵深防御:即使上游漏判,渲染层也不许产出自引用的 Pin。
    /// 自引用 wrapper 会无限 exec —— 2026-08-04 真实把一个测试挂死 1 小时 34 分且不报错。
    #[test]
    fn pin_pointing_into_wrapper_root_degrades_to_deny() {
        let own = lark_cli_wrapper_dir("arcadia").join("lark-cli");
        let script = lark_cli_wrapper_script(
            "arcadia",
            &LarkCliWrapper::Pin {
                real_lark_cli: &own.to_string_lossy(),
                profile: "cli_arcadia",
            },
        );
        assert!(
            !script.contains("exec "),
            "自引用的 Pin 必须降级成拒绝型,实际脚本: {script}"
        );
        assert!(script.contains(&format!("exit {LARK_CLI_DENY_EXIT}")));
    }

    /// `self` 污染回归:tmp 目录里伪造的 Agent(`session.rs` 的身份隔离回归用的那个,
    /// dir 基名恰为 `self`)不该在真实 home 下落 wrapper 目录。
    #[test]
    fn only_agents_under_the_real_agents_root_are_managed() {
        assert!(is_managed_agent_dir(
            &summon_home().join("agents").join("arcadia")
        ));
        assert!(
            !is_managed_agent_dir(&std::env::temp_dir().join("summon-iso-123").join("self")),
            "tmp 里伪造的 agent 不该被当成受管 agent"
        );
        assert!(
            !is_managed_agent_dir(&summon_home().join("agents")),
            "agents 根目录自身不是某个 agent"
        );
    }

    /// 真机前提验证:`--profile <不存在>` 必须硬报错而不是回落到默认 profile。
    /// 整个 fail-closed 设计(对账失败就不留可用 profile)都建立在这条上。
    /// 需要本机装了 lark-cli,故 ignore;`--dry-run` 不发网络请求。
    #[test]
    #[ignore]
    fn unknown_profile_is_rejected_by_real_lark_cli() {
        let out = Command::new("lark-cli")
            .args([
                "--profile",
                "__summon_missing_profile__",
                "api",
                "GET",
                "/open-apis/contact/v3/users/me",
                "--dry-run",
            ])
            .output()
            .expect("本机需可执行 lark-cli");
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            text.contains("not found") && text.contains("__summon_missing_profile__"),
            "lark-cli 必须拒绝未知 profile,实际输出:{text}"
        );
    }
}

#[cfg(test)]
mod storage_scope_tests {
    use super::*;

    #[test]
    fn storage_scope_isolated_by_agent_and_app() {
        let a1 = storage_scope_key("starlight", Some("cli_app_a"));
        let a2 = storage_scope_key("starlight", Some("cli_app_b"));
        let b1 = storage_scope_key("arcadia", Some("cli_app_a"));
        assert_ne!(a1, a2, "同 agent 换 app 后不能复用旧 open_id 缓存");
        assert_ne!(a1, b1, "同 app 的不同 agent 也要隔离运行时身份数据");
        assert!(a1.starts_with("starlight-"));
    }

    #[test]
    fn storage_scope_sanitizes_path_component() {
        let key = storage_scope_key("../坏/agent", None);
        assert!(!key.contains('/'));
        assert!(!key.contains(".."));
    }

    /// 表驱动用的临时 agents 目录:`(目录名, agent.yaml 内容)`。
    fn temp_agents_dir(label: &str, entries: &[(&str, &str)]) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "summon-legacy-import-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        for (name, manifest) in entries {
            let agent_dir = dir.join(name);
            std::fs::create_dir_all(&agent_dir).unwrap();
            std::fs::write(agent_dir.join("agent.yaml"), manifest).unwrap();
        }
        dir
    }

    const FEISHU_A: &str = "name: a\nfeishu:\n  app_id_env: A_APP_ID\n";
    const FEISHU_B: &str = "name: b\nfeishu:\n  app_id_env: B_APP_ID\n";
    /// 坏在 feishu 块:半解析放行的话它看着就是个普通的飞书 agent。
    const BROKEN: &str = "name: broken\nfeishu:\n  app_id_env: [OOPS\n";

    #[test]
    fn legacy_import_only_when_exactly_one_feishu_agent() {
        let one = temp_agents_dir("one", &[("a", FEISHU_A)]);
        assert!(legacy_import_allowed(&one), "独苗飞书 agent 才允许导入");
        let _ = std::fs::remove_dir_all(one);

        let two = temp_agents_dir("two", &[("a", FEISHU_A), ("b", FEISHU_B)]);
        assert!(
            !legacy_import_allowed(&two),
            "多 agent 下旧数据归属不可判定"
        );
        let _ = std::fs::remove_dir_all(two);

        let none = temp_agents_dir("none", &[("a", "name: a\n")]);
        assert!(!legacy_import_allowed(&none));
        let _ = std::fs::remove_dir_all(none);

        // 目录都不在 → 没有事实可依,fail-closed。
        assert!(!legacy_import_allowed(std::path::Path::new(
            "/no/such/summon/agents"
        )));
    }

    #[test]
    fn broken_manifest_forbids_legacy_import() {
        // 这条是翻转成 tolerant 加载器时会引入的 fail-open:坏清单被隔离出 registry 后,
        // 「两个飞书 agent 坏了一个」会被数成「独苗」→ 幸存者把另一个 app 的旧 open_id→姓名
        // 缓存 + peers 导进自己 scope,而 feishu-legacy.scope claim 让这次误导入一次性粘死。
        // 故:只要有任何一份清单解析失败,一律不导入。
        let dir = temp_agents_dir("broken", &[("a", FEISHU_A), ("broken", BROKEN)]);
        assert!(
            !legacy_import_allowed(&dir),
            "有清单解析失败时必须拒绝导入旧全局身份缓存"
        );
        let _ = std::fs::remove_dir_all(dir);

        // 连独苗场景也一样:坏的那份可能正是第二个飞书 agent。
        let solo = temp_agents_dir("broken-solo", &[("a", FEISHU_A), ("x", "")]);
        assert!(!legacy_import_allowed(&solo));
        let _ = std::fs::remove_dir_all(solo);
    }

    #[test]
    fn delivery_scope_matches_storage_scope_key() {
        // CLI 与 daemon 必须算出同一个 scope:daemon 走 FeishuEpochIdentity::capture →
        // storage_scope().key,CLI 走 feishu_delivery_scope。两者漂移会让回执被 drain 的
        // 身份闸门静默丢弃(无报错),故用测试钉死它们同源。
        let (agent, _tmp) = crate::test_support::agent_dir("claude", "");
        assert_eq!(
            feishu_delivery_scope(&agent),
            feishu_storage_scope(&agent).key,
            "CLI 侧 scope 必须等于 daemon 侧 storage_scope key"
        );
    }

    #[test]
    fn resolved_epoch_never_mixes_a_client_with_b_environment_scope() {
        let (agent, _tmp) = crate::test_support::agent_dir(
            "claude",
            "feishu:\n  app_id_env: T_APP_ID\n  app_secret_env: T_APP_SECRET\n",
        );
        // 模拟 client 已从快照 A 构造、线程启动前盘上环境切到 B。from_resolved 的所有凭据派生
        // 字段都必须只认传入 A，不能为 storage scope 偷偷再读一次 B。
        std::fs::write(
            agent.dir.join(".env"),
            "T_APP_ID=app-b\nT_APP_SECRET=secret-b\n",
        )
        .unwrap();
        let epoch = FeishuEpochIdentity::from_resolved(&agent, Some("app-a"), Some("secret-a"));
        let alias = agent.dir.file_name().unwrap().to_str().unwrap();
        assert_eq!(
            epoch.storage_scope().key,
            storage_scope_key(alias, Some("app-a"))
        );
        assert_eq!(epoch.app_id_digest, credential_digest(Some("app-a")));
        assert_eq!(epoch.app_secret_digest, credential_digest(Some("secret-a")));
        assert!(
            !epoch.matches_current(&agent.reread().unwrap()),
            "盘上 B 必须被识别为与持有 A 凭据的 client 不同代"
        );
    }

    /// **热路径不许碰盘**(owner 2026-08-10:`summond` 稳定 70% CPU)。
    ///
    /// `matches_current` 跑在每个 dispatcher 主循环的每一轮上;它此前经 `feishu_storage_scope`
    /// 连带触发 `Registry::load_tolerant`,把整个 agents 目录扫一遍、每份 agent.yaml 解析一遍。
    /// 采样里这一条独占 90% 的样本。
    ///
    /// 这条测试用一个**不存在的 agents 目录**当环境:真去扫盘的实现在那里只会得到空 registry
    /// (或错误),而纯计算的 key 不受影响 —— 两者结果一致才说明它没在碰盘。
    #[test]
    fn storage_scope_key_is_pure_computation() {
        let (agent, _tmp) = crate::test_support::agent_dir(
            "claude",
            "feishu:\n  app_id_env: T_APP_ID\n  app_secret_env: T_APP_SECRET\n",
        );
        // 同一个 agent 反复算,结果稳定。
        let first = feishu_storage_scope_key(&agent);
        let again = feishu_storage_scope_key(&agent);
        assert_eq!(first, again);
        assert!(!first.is_empty());
        // 与带扫盘的完整版本给出**同一个 key** —— 分离出来的纯函数不能改变 scope 语义,
        // 否则 outbox 的 delivery_scope 会与 daemon 启动时 capture 的对不上,事件被静默丢弃。
        assert_eq!(first, feishu_storage_scope(&agent).key);
    }

    #[test]
    fn fingerprint_agrees_with_matches_current_on_secret_rotation() {
        // 真机缺陷复刻:owner 轮换 app secret 并改 .env,daemon 内存仍是旧的。
        // `matches_current` 判 false(入站已 fail-closed),指纹**必须同时**变 —— 两者分叉
        // 就是「doctor 报绿而 agent 是聋的」。
        let (agent, _tmp) = crate::test_support::agent_dir(
            "claude",
            "feishu:\n  app_id_env: T_APP_ID\n  app_secret_env: T_APP_SECRET\n",
        );
        std::fs::write(
            agent.dir.join(".env"),
            "T_APP_ID=fixture-app\nT_APP_SECRET=fixture-secret-old\n",
        )
        .unwrap();
        let epoch = FeishuEpochIdentity::capture(&agent);
        let before = epoch.fingerprint();

        let unchanged = agent.reread().expect("重读 agent");
        assert!(epoch.matches_current(&unchanged));
        assert_eq!(
            before,
            feishu_identity_fingerprint(&unchanged),
            "凭据没动时指纹必须稳定,否则每次 doctor 都误报漂移"
        );

        std::fs::write(
            agent.dir.join(".env"),
            "T_APP_ID=fixture-app\nT_APP_SECRET=fixture-secret-new\n",
        )
        .unwrap();
        let rotated = agent.reread().expect("重读 agent");
        assert!(
            !epoch.matches_current(&rotated),
            "secret 变了必须 fail-closed(本条语义不许放松)"
        );
        assert_ne!(
            before,
            feishu_identity_fingerprint(&rotated),
            "指纹必须与 matches_current 同步翻转"
        );
    }

    #[test]
    fn fingerprint_never_leaks_credential_material() {
        let (agent, _tmp) = crate::test_support::agent_dir(
            "claude",
            "feishu:\n  app_id_env: T_APP_ID\n  app_secret_env: T_APP_SECRET\n",
        );
        std::fs::write(
            agent.dir.join(".env"),
            "T_APP_ID=fixture-app\nT_APP_SECRET=fixture-secret-old\n",
        )
        .unwrap();
        let fp = feishu_identity_fingerprint(&agent);
        assert_eq!(fp.len(), 64, "SHA-256 hex");
        assert!(fp.chars().all(|c| c.is_ascii_hexdigit()));
        assert!(!fp.contains("fixture-app"));
        assert!(!fp.contains("fixture-secret-old"));
    }
}

#[cfg(test)]
mod runtime_env_expand_tests {
    use super::*;

    /// 手搓 agent YAML —— `test_support::agent_dir` 会自带 `runtime:` 段，
    /// 再传一个就成了 YAML 重复键（其文档也建议这种场景直接手搓）。
    ///
    /// `env_body` 是 `runtime.env` 的条目，需带 4 空格缩进。
    fn agent_with_runtime_env(env_body: &str, dotenv: &str) -> (Agent, tempfile::TempDir) {
        let tmp = tempfile::tempdir().expect("建临时 agent 目录");
        let yaml = format!("name: test\nruntime:\n  cli: claude\n  env:\n{env_body}");
        let mut agent: Agent = serde_yaml::from_str(&yaml).expect("测试 YAML 应可解析");
        agent.dir = tmp.path().join("agents").join(&agent.name);
        std::fs::create_dir_all(&agent.dir).unwrap();
        std::fs::create_dir_all(agent.state_dir()).unwrap();
        std::fs::write(agent.manifest_path(), &yaml).expect("写 manifest");
        std::fs::write(agent.dir.join(".env"), dotenv).expect("写 .env");
        (agent, tmp)
    }

    #[test]
    fn recorded_model_falls_back_to_anthropic_model_env_for_claude() {
        let (agent, _t) =
            agent_with_runtime_env("    ANTHROPIC_MODEL: \"ark/seed-evolving\"\n", "");
        assert_eq!(
            recorded_model(&agent, "g1").as_deref(),
            Some("ark/seed-evolving")
        );
    }

    #[test]
    fn recorded_model_prefers_top_level_model() {
        let (mut agent, _t) =
            agent_with_runtime_env("    ANTHROPIC_MODEL: \"ark/seed-evolving\"\n", "");
        agent.model = Some("opus".into());
        assert_eq!(recorded_model(&agent, "g1").as_deref(), Some("opus"));
    }

    #[test]
    fn recorded_model_is_none_without_any_source() {
        let (agent, _t) = agent_with_runtime_env("    PLAIN: \"literal\"\n", "");
        assert_eq!(recorded_model(&agent, "g1"), None);
    }

    #[test]
    fn event_stamp_carries_agent_and_active_turn() {
        let (agent, _t) = agent_with_runtime_env("    PLAIN: \"literal\"\n", "");
        let tag = "g1";
        let none = event_stamp(&agent, tag);
        assert_eq!(none.agent.as_deref(), Some("test"));
        assert_eq!(none.turn, None, "没有活跃轮就不编一个 turn");

        let store =
            crate::runtime::current_turn_store::CurrentTurnStore::for_agent(&agent.state_dir());
        let ctx = crate::runtime::current_turn_store::CurrentTurnContext {
            turn_id: "turn-xyz".into(),
            agent_alias: "test".into(),
            delivery_scope: "scope".into(),
            chat_id: "oc_1".into(),
            chat_type: "group".into(),
            conversation_tag: tag.into(),
            message_id: "om_1".into(),
            origin: Default::default(),
            schedule_output: Default::default(),
            requester_open_id: "ou_1".into(),
            requester_union_id: String::new(),
            requester_is_bot: false,
            requester_is_peer: false,
            thread_id: None,
            root_id: None,
            started_at_ms: 1,
            reattached: false,
        };
        let _lease = store.activate(&ctx).unwrap();
        assert_eq!(event_stamp(&agent, tag).turn.as_deref(), Some("turn-xyz"));
    }

    #[test]
    fn plain_value_passes_through() {
        let (agent, _t) = agent_with_runtime_env("    PLAIN: \"literal\"\n", "");
        assert_eq!(
            resolve_extra_env(&agent),
            vec![("PLAIN".to_string(), "literal".to_string())]
        );
    }

    #[test]
    fn reference_is_expanded_from_dotenv() {
        // 本改动存在的理由：agent.yaml 里只写引用，真值留在 gitignore 的 .env，
        // 这样整个 ~/.summon 仓才能安全推到远端。
        let (agent, _t) = agent_with_runtime_env(
            "    ANTHROPIC_AUTH_TOKEN: \"${MY_TOKEN}\"\n",
            "MY_TOKEN=secret-value\n",
        );
        assert_eq!(
            resolve_extra_env(&agent),
            vec![(
                "ANTHROPIC_AUTH_TOKEN".to_string(),
                "secret-value".to_string()
            )],
            "agent.yaml 不该再需要明文密钥"
        );
    }

    #[test]
    fn undefined_reference_drops_the_entry_not_empties_it() {
        // 关键语义：宁可缺失也不注入空串 —— 空值会覆盖 CLI 自己的登录态，
        // 表现为「配了反而更坏」，比缺失更难排查。
        let (agent, _t) = agent_with_runtime_env("    TOK: \"${NOT_DEFINED_ANYWHERE_XYZ}\"\n", "");
        assert!(
            resolve_extra_env(&agent).is_empty(),
            "未定义引用必须整条丢弃，不能变成空串注入"
        );
    }

    #[test]
    fn mixed_literal_and_reference() {
        let (agent, _t) =
            agent_with_runtime_env("    URL: \"https://${HOST}/v1\"\n", "HOST=example.com\n");
        assert_eq!(
            resolve_extra_env(&agent),
            vec![("URL".to_string(), "https://example.com/v1".to_string())]
        );
    }

    #[test]
    fn unclosed_brace_is_literal_not_reference() {
        let (agent, _t) = agent_with_runtime_env("    ODD: \"a${UNCLOSED\"\n", "");
        assert_eq!(
            resolve_extra_env(&agent),
            vec![("ODD".to_string(), "a${UNCLOSED".to_string())],
            "未闭合的花括号不是引用，原样保留"
        );
    }

    #[test]
    fn dotenv_wins_over_process_env() {
        // 与 resolve_creds 的查找顺序保持一致：per-agent .env 优先。
        std::env::set_var("SUMMON_TEST_PRIORITY_VAR", "from-process");
        let (agent, _t) = agent_with_runtime_env(
            "    V: \"${SUMMON_TEST_PRIORITY_VAR}\"\n",
            "SUMMON_TEST_PRIORITY_VAR=from-dotenv\n",
        );
        let got = resolve_extra_env(&agent);
        std::env::remove_var("SUMMON_TEST_PRIORITY_VAR");
        assert_eq!(got, vec![("V".to_string(), "from-dotenv".to_string())]);
    }

    #[test]
    fn falls_back_to_process_env_when_absent_from_dotenv() {
        std::env::set_var("SUMMON_TEST_FALLBACK_VAR", "from-process");
        let (agent, _t) = agent_with_runtime_env("    V: \"${SUMMON_TEST_FALLBACK_VAR}\"\n", "");
        let got = resolve_extra_env(&agent);
        std::env::remove_var("SUMMON_TEST_FALLBACK_VAR");
        assert_eq!(got, vec![("V".to_string(), "from-process".to_string())]);
    }

    #[test]
    fn empty_dotenv_value_falls_through_to_process_env() {
        // 空值视作缺，与 resolve_creds 的 `.filter(|v| !v.is_empty())` 语义一致。
        std::env::set_var("SUMMON_TEST_EMPTY_VAR", "from-process");
        let (agent, _t) = agent_with_runtime_env(
            "    V: \"${SUMMON_TEST_EMPTY_VAR}\"\n",
            "SUMMON_TEST_EMPTY_VAR=\n",
        );
        let got = resolve_extra_env(&agent);
        std::env::remove_var("SUMMON_TEST_EMPTY_VAR");
        assert_eq!(got, vec![("V".to_string(), "from-process".to_string())]);
    }
}

#[cfg(test)]
mod email_mask_switch_tests {
    use super::*;

    /// 打码开关跟着 agent.yaml 走,改了下一次就用新值(不重启 daemon)。
    #[test]
    fn the_email_mask_switch_follows_agent_yaml_edits() {
        let (agent, _dir) =
            crate::test_support::agent_dir("claude", "messaging:\n  mask_email: false\n");
        let switch = email_mask_switch(&agent);
        assert!(!switch(), "agent.yaml 关掉了打码");
        let off = std::fs::read_to_string(agent.manifest_path()).unwrap();
        std::fs::write(
            agent.manifest_path(),
            off.replace("mask_email: false", "mask_email: true"),
        )
        .unwrap();
        assert!(switch(), "改了 agent.yaml,下一次就打码");
    }

    /// 生产代码里每个会往群里发文本的飞书客户端都要挂打码开关;漏一处,那里发出的邮箱被拒
    /// 后不会打码重发。只做凭据验真、贴表情、发确认卡(只有显示名)的几处在白名单里。
    #[test]
    fn every_feishu_client_that_sends_text_carries_the_email_mask_switch() {
        let allowed_unmasked = [
            ("setup/bind.rs", 1),    // 凭据验真
            ("setup/onboard.rs", 3), // 凭据验真、查应用所有者、绑定成功确认卡(只有显示名)
            ("cli/cli.rs", 2),       // 贴表情、activate 的凭据验真
        ];
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut pending = vec![src.clone()];
        let mut unmasked = std::collections::BTreeMap::new();
        while let Some(dir) = pending.pop() {
            for entry in std::fs::read_dir(&dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    pending.push(path);
                    continue;
                }
                let name = path.file_name().unwrap().to_string_lossy();
                if !name.ends_with(".rs") || name == "tests.rs" || name == "test_support.rs" {
                    continue;
                }
                let source = std::fs::read_to_string(&path).unwrap();
                // 测试夹具一律用字面量域名(`FeishuClient::new("feishu.cn", …)`),生产代码取配置。
                let count = source
                    .match_indices("FeishuClient::new(")
                    .filter(|(at, needle)| {
                        let statement = &source[*at..];
                        let args = statement[needle.len()..].trim_start();
                        let end = statement.find(';').unwrap_or(statement.len());
                        !args.starts_with('"') && !statement[..end].contains(".with_email_mask(")
                    })
                    .count();
                if count > 0 {
                    let relative = path.strip_prefix(&src).unwrap().to_string_lossy();
                    unmasked.insert(relative.into_owned(), count);
                }
            }
        }
        let expected: std::collections::BTreeMap<String, usize> = allowed_unmasked
            .iter()
            .map(|(file, count)| (file.to_string(), *count))
            .collect();
        assert_eq!(
            unmasked, expected,
            "新建的飞书客户端要挂打码开关,或说明为什么不需要"
        );
    }
}
