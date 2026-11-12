//! codex / traex 与走 CLI 的 cursor 的模型目录:面板候选与默认模型的事实源。
//!
//! 目录归 backend 管(codex / traex 是 app-server 的 `model/list`,走 CLI 的 cursor 是
//! `cursor-agent models`)。daemon 在独立线程按需刷新，
//! 面板读取账号隔离的缓存；onboard/bind 等一次性命令同步查询。
//!
//! 缓存按 **账号** 分桶,不按 chat、也不按 cli kind。
//!
//! 不按 chat:目录是账号级的,与哪条会话无关 —— 一个 agent 的任一 chat 开过会话,它所有 chat
//! 的面板都能拿到真实候选。这是它与走 ACP 的 cursor(目录只随会话下发)的差别。
//!
//! 不按 cli kind:同一个 cli 下,配了 `runtime.env` 的 `CODEX_HOME` 或 `runtime.cli_cmd` 的
//! agent 连的是另一个账号,目录可以完全不同。串了的后果是 A 的面板列出 B 账号才有的模型,
//! 点一下写进 agent.yaml,下次冷启 `-m` 一个不存在的 id。仓里 `agent_uses_default_codex_account`
//! 钉的是同一条不变量。所以 daemon 侧按 agent 分桶;onboard/bind 那条路径上 agent 还不存在,
//! 探测用的是标准二进制 + 当前环境,只能代表默认账号,单独占一个桶。
//!
//! 后台刷新使用当前 agent 的 CLI 与环境，每个账号作用域有最短刷新间隔。
//! 查询失败保留最近成功的目录，首次查询完成前允许使用配置候选。

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use summon_core::driver::CliDriver;
use summon_core::registry::{Agent, CaptureMode};

use crate::observe::log::log;

use crate::codex::codex_app_server::{
    CodexAppServerClient, CodexAppServerConfig, CodexModelCatalog,
};

/// 一次性命令里现问目录的耗时上限,与 daemon 侧开会话时那次读同档。它同时约束握手和
/// `model/list` 两步,所以最坏等两倍 —— onboard/bind 是人在终端前等着的命令,不能更宽。
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
const REFRESH_INTERVAL: Duration = Duration::from_secs(60);
/// `cursor-agent models` 要联网问账号目录,本机实测约 2s;后台线程里跑,给足余量。
const CURSOR_MODELS_TIMEOUT: Duration = Duration::from_secs(20);

/// 这个 CLI 的模型目录是否由 backend 自报。traex 是 codex 的 fork,协议同构,走同一条路。
pub fn reports_catalog(cli: &str) -> bool {
    matches!(
        CliDriver::from_kind(cli),
        CliDriver::Codex | CliDriver::Traex
    )
}

/// 这个 agent 的目录由谁报:codex / traex 走 app-server,走 CLI 的 cursor 读 `cursor-agent models`。
/// 走 ACP 的 cursor 目录随会话下发(见 `rpc_pool`),不在这里。
fn agent_reports_catalog(agent: &Agent) -> bool {
    match CliDriver::from_kind(agent.cli_kind()) {
        CliDriver::Codex | CliDriver::Traex => true,
        CliDriver::Cursor => agent.capture_mode() == CaptureMode::CliSession,
        _ => false,
    }
}

/// 后台刷新线程起不起:cursor 无论眼下走哪条通道都起,agent.yaml 热改成 CLI 后不用重启就有目录;
/// 每次刷新前再按 [`agent_reports_catalog`] 判一遍。
fn may_report_catalog(cli: &str) -> bool {
    matches!(
        CliDriver::from_kind(cli),
        CliDriver::Codex | CliDriver::Traex | CliDriver::Cursor
    )
}

/// 解析 `cursor-agent models` 的输出:每个模型一行 `<id> - <显示名>`,默认那项显示名带
/// `(default)`;表头、空行与末尾的 `Tip:` 提示都不是 `<id> - ` 形状,跳过。
pub(crate) fn parse_cursor_models_output(out: &str) -> CodexModelCatalog {
    let mut catalog = CodexModelCatalog::default();
    for line in out.lines().map(str::trim) {
        let Some((id, name)) = line.split_once(" - ") else {
            continue;
        };
        let id = id.trim();
        if id.is_empty() || id.chars().any(char::is_whitespace) {
            continue;
        }
        if catalog.default_model.is_none() && name.trim_end().ends_with("(default)") {
            catalog.default_model = Some(id.to_string());
        }
        catalog.models.push(id.to_string());
    }
    catalog
}

fn store() -> &'static Mutex<HashMap<String, CodexModelCatalog>> {
    static STORE: OnceLock<Mutex<HashMap<String, CodexModelCatalog>>> = OnceLock::new();
    STORE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 两个命名空间各带前缀,免得有人把 agent 起名叫 `codex` 就和默认账号那桶撞上。
fn agent_scope(agent: &Agent) -> String {
    let config = agent_probe_config(agent);
    scope_for_config(agent, &config)
}

fn scope_for_config(agent: &Agent, config: &CodexAppServerConfig) -> String {
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    config.program.hash(&mut hash);
    config.env.hash(&mut hash);
    agent.dir.hash(&mut hash);
    if let Ok(metadata) = std::fs::metadata(&config.program) {
        metadata.modified().ok().hash(&mut hash);
        metadata.len().hash(&mut hash);
    }
    format!("agent:{}:{:x}", agent.name, hash.finish())
}

fn default_account_scope(cli: &str) -> String {
    format!("default:{cli}")
}

/// 空目录不覆盖已有的:宁可发一份可能旧了的目录,也不退回配置桶那张更旧的表。
fn record(scope: String, catalog: CodexModelCatalog) {
    if catalog.models.is_empty() {
        return;
    }
    store()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(scope, catalog);
}

fn cached(scope: &str) -> Option<CodexModelCatalog> {
    store()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(scope)
        .cloned()
}

/// daemon 开会话时抓到的目录,记在这个 agent 自己的桶里。
pub fn record_for_agent(agent: &Agent, catalog: CodexModelCatalog) {
    record(agent_scope(agent), catalog);
}

/// 这个 agent 的目录。daemon 起来后它还没开过会话 → `None`,调用方回退配置桶。
pub fn cached_for_agent(agent: &Agent) -> Option<CodexModelCatalog> {
    if !agent_reports_catalog(agent) {
        return None;
    }
    let scope = agent_scope(agent);
    cached(&format!("refresh:{scope}")).or_else(|| cached(&scope))
}

fn agent_probe_config(agent: &Agent) -> CodexAppServerConfig {
    let mut env: std::collections::BTreeMap<_, _> = crate::runtime::daemon::essential_system_env()
        .into_iter()
        .filter(|(key, _)| !summon_core::agent_identity::is_runtime_owned_env_key(key))
        .collect();
    for (key, value) in crate::runtime::daemon::resolve_extra_env(agent) {
        if !summon_core::agent_identity::is_runtime_owned_env_key(&key) {
            env.insert(key, value);
        }
    }
    CodexAppServerConfig {
        program: agent.cli_cmd(),
        args: vec!["app-server".into(), "--listen".into(), "stdio://".into()],
        env: env.into_iter().collect(),
        request_timeout: PROBE_TIMEOUT,
        client_name: "summon-model-catalog".into(),
        client_title: "Summon Model Catalog".into(),
        ..CodexAppServerConfig::default()
    }
}

struct RefreshMailbox {
    tx: mpsc::SyncSender<Agent>,
    stopped: AtomicBool,
    active: Mutex<Option<crate::runtime::executor::ChildKiller>>,
}

fn refreshers() -> &'static Mutex<HashMap<String, Arc<RefreshMailbox>>> {
    static REFRESHERS: OnceLock<Mutex<HashMap<String, Arc<RefreshMailbox>>>> = OnceLock::new();
    REFRESHERS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub struct CatalogRefresher {
    agent: String,
    mailbox: Arc<RefreshMailbox>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl Drop for CatalogRefresher {
    fn drop(&mut self) {
        self.mailbox.stopped.store(true, Ordering::Release);
        let mut registry = refreshers().lock().unwrap_or_else(PoisonError::into_inner);
        if registry
            .get(&self.agent)
            .is_some_and(|entry| Arc::ptr_eq(entry, &self.mailbox))
        {
            registry.remove(&self.agent);
        }
        drop(registry);
        if let Some(killer) = self
            .mailbox
            .active
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
        {
            killer.kill();
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

pub fn start_refreshing(agent: &Agent) -> Option<CatalogRefresher> {
    start_refreshing_with_interval(agent, REFRESH_INTERVAL)
}

fn start_refreshing_with_interval(agent: &Agent, interval: Duration) -> Option<CatalogRefresher> {
    if !may_report_catalog(agent.cli_kind()) {
        return None;
    }
    let (tx, rx) = mpsc::sync_channel(1);
    let mailbox = Arc::new(RefreshMailbox {
        tx,
        stopped: AtomicBool::new(false),
        active: Mutex::new(None),
    });
    let worker_mailbox = Arc::clone(&mailbox);
    let worker = std::thread::Builder::new()
        .name(format!("model-catalog-{}", agent.name))
        .spawn(move || refresh_loop(rx, &worker_mailbox, interval))
        .map_err(|error| log(&summon_i18n::format!("r743096142a")))
        .ok()?;
    refreshers()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(agent.name.clone(), Arc::clone(&mailbox));
    let _ = mailbox.tx.try_send(agent.clone());
    Some(CatalogRefresher {
        agent: agent.name.clone(),
        mailbox,
        worker: Some(worker),
    })
}

fn refresh_loop(rx: mpsc::Receiver<Agent>, mailbox: &RefreshMailbox, interval: Duration) {
    let mut last_attempt: Option<(String, Instant)> = None;
    while !mailbox.stopped.load(Ordering::Acquire) {
        let agent = match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(agent) => agent,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };
        if !agent_reports_catalog(&agent) {
            continue;
        }
        let config = agent_probe_config(&agent);
        let scope = scope_for_config(&agent, &config);
        if last_attempt
            .as_ref()
            .is_some_and(|(previous, time)| previous == &scope && time.elapsed() < interval)
        {
            continue;
        }
        last_attempt = Some((scope.clone(), Instant::now()));
        if CliDriver::from_kind(agent.cli_kind()) == CliDriver::Cursor {
            match cursor_cli_models(&config, mailbox) {
                Ok(Some(catalog)) => record(format!("refresh:{scope}"), catalog),
                Ok(None) => break,
                Err(error) => log(&summon_i18n::format!("rc7a1e0d93d", agent.name)),
            }
            continue;
        }
        let outcome = CodexAppServerClient::spawn(config).and_then(|mut client| {
            {
                let mut active = mailbox
                    .active
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                if mailbox.stopped.load(Ordering::Acquire) {
                    return Ok(CodexModelCatalog::default());
                }
                *active = Some(client.killer());
            }
            let result = client.model_list(Some(PROBE_TIMEOUT));
            mailbox
                .active
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take();
            result
        });
        if mailbox.stopped.load(Ordering::Acquire) {
            break;
        }
        match outcome {
            Ok(catalog) => record(format!("refresh:{scope}"), catalog),
            Err(error) => log(&summon_i18n::format!("r02c0d61a86", agent.name)),
        }
    }
}

/// 用 agent 自己的 CLI 与环境跑一次 `cursor-agent models`。停机或超时就杀掉它,不让刷新线程
/// 卡住 dispatcher 的退出。
///
/// 停机打断时返回 `Ok(None)`,不算刷新失败。stdout 也按同一个截止时间等:cursor-agent 若留下
/// 继承了 stdout 的后代进程,管道迟迟不关,读线程就地丢弃,不让刷新线程跟着卡住。
fn cursor_cli_models(
    config: &CodexAppServerConfig,
    mailbox: &RefreshMailbox,
) -> Result<Option<CodexModelCatalog>, String> {
    use std::io::Read;
    let mut child = std::process::Command::new(&config.program)
        .arg("models")
        .env_clear()
        .envs(config.env.iter().map(|(key, value)| (key, value)))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|error| error.to_string())?;
    let Some(mut stdout) = child.stdout.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return Err(summon_i18n::text!("rc7a1e0d93e").to_string());
    };
    let (out_tx, out_rx) = mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let mut out = String::new();
        // 读失败按空输出处理,解析出空目录不会覆盖已有缓存。
        let _ = stdout.read_to_string(&mut out);
        // 等的一方已超时走了就没人收,丢掉即可。
        let _ = out_tx.send(out);
    });
    let deadline = Instant::now() + CURSOR_MODELS_TIMEOUT;
    let timed_out = || summon_i18n::format!("rc7a1e0d93c", CURSOR_MODELS_TIMEOUT.as_secs());
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                let stopped = mailbox.stopped.load(Ordering::Acquire);
                if stopped || Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return if stopped { Ok(None) } else { Err(timed_out()) };
                }
            }
            Err(error) => return Err(error.to_string()),
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    if !status.success() {
        return Err(summon_i18n::format!("rc7a1e0d93b", status));
    }
    loop {
        if mailbox.stopped.load(Ordering::Acquire) {
            return Ok(None);
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(timed_out());
        }
        match out_rx.recv_timeout(left.min(Duration::from_millis(100))) {
            Ok(out) => return Ok(Some(parse_cursor_models_output(&out))),
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(Some(Default::default())),
        }
    }
}

pub fn models_for_agent(agent: &Agent) -> Vec<String> {
    if let Some(mailbox) = refreshers()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(&agent.name)
    {
        let _ = mailbox.tx.try_send(agent.clone());
    }
    cached_for_agent(agent)
        .map(|catalog| catalog.models)
        .unwrap_or_default()
}

/// 现起一个 app-server 问一次目录,顺带填进缓存。
///
/// 拿不到就退回配置桶,不让 onboard 失败 —— 但**留一行日志**:降级后的症状只是「候选和默认
/// 模型是旧的」,不说一声的话没人查得出为什么。没装 / 没登录该 CLI 时走的就是这条,属正常。
fn probe(cli: &str) -> Option<CodexModelCatalog> {
    // 走 `for_driver` 而不是 `for_cli_kind`:后者不认 traex,会把 traex 的目录探测解析成 None。
    let program = summon_core::tools::Tool::for_driver(CliDriver::from_kind(cli))
        .map(summon_core::tools::resolve)?;
    let config = CodexAppServerConfig {
        program: program.to_string_lossy().into_owned(),
        // 用 `--listen stdio://` 而不是默认的 `--stdio`:codex 两者都认,traex 只认前者,
        // 传错了 app-server 根本起不来。
        args: vec!["app-server".into(), "--listen".into(), "stdio://".into()],
        request_timeout: PROBE_TIMEOUT,
        client_name: "summon-model-catalog".into(),
        client_title: "Summon Model Catalog".into(),
        ..CodexAppServerConfig::default()
    };
    let outcome = CodexAppServerClient::spawn(config)
        .map_err(|error| error.to_string())
        .and_then(|mut client| {
            client
                .model_list(Some(PROBE_TIMEOUT))
                .map_err(|error| error.to_string())
        });
    match outcome {
        Ok(catalog) => {
            record(default_account_scope(cli), catalog.clone());
            Some(catalog)
        }
        Err(detail) => {
            log(&summon_i18n::format!("rdc9065485d"));
            None
        }
    }
}

/// 默认账号的目录:缓存里有就用缓存,没有才起进程问。
///
/// **只给 daemon 之外的一次性命令用**(onboard/bind/dev):daemon 里绝不能走这条 —— 卡片渲染
/// 在 chat 线程上,起进程问一次会把面板挂住最坏两个 `PROBE_TIMEOUT`。这条红线由
/// `default_account_catalog_call_sites_are_whitelisted` 守着,不只是这句注释。
pub fn cached_or_probe(cli: &str) -> Option<CodexModelCatalog> {
    if !reports_catalog(cli) {
        return None;
    }
    cached(&default_account_scope(cli)).or_else(|| probe(cli))
}

/// backend 自报的默认模型,喂给 [`summon_core::model_effort::default_model_for`] 的第三个参数。
pub fn upstream_default_model(cli: &str) -> Option<String> {
    cached_or_probe(cli).and_then(|catalog| catalog.default_model)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CURSOR_MODELS_OUTPUT: &str = "Available models\n\nauto - Auto (default)\n\
gpt-5.6-sol-xhigh - GPT-5.6 Sol Extra High\ngemini-4-carbon-high - Gemini 4 Carbon High\n\n\
Tip: use --model <id> (or /model <id> in interactive mode) to switch. Parameterized models \
also accept quoted overrides, e.g. --model 'claude-opus-4-8[context=1m,effort=high]'.\n";

    #[test]
    fn cursor_models_output_parses_ids_and_the_default() {
        let catalog = parse_cursor_models_output(CURSOR_MODELS_OUTPUT);
        assert_eq!(
            catalog.models,
            ["auto", "gpt-5.6-sol-xhigh", "gemini-4-carbon-high"]
        );
        assert_eq!(catalog.default_model.as_deref(), Some("auto"));
    }

    fn cursor_agent(name: &str, capture: Option<&str>) -> (Agent, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let script = tmp.path().join("fake-cursor-agent");
        crate::test_support::write_test_executable(
            &script,
            &format!(
                "#!/bin/sh\n[ \"$1\" = models ] || exit 2\ncat <<'EOF'\n{CURSOR_MODELS_OUTPUT}EOF\n"
            ),
        );
        let capture_line = capture
            .map(|c| format!("  capture: {c}\n"))
            .unwrap_or_default();
        let yaml = format!(
            "name: {name}\nstatus: active\nruntime:\n  cli: cursor\n  cli_cmd: {}\n{capture_line}",
            script.display()
        );
        let mut agent: Agent = serde_yaml::from_str(&yaml).unwrap();
        agent.dir = tmp.path().to_path_buf();
        (agent, tmp)
    }

    #[test]
    fn cli_cursor_agents_get_the_full_cursor_agent_models_catalog() {
        let (agent, _tmp) = cursor_agent("cursor-cli-catalog", Some("cli"));
        let refresher = start_refreshing_with_interval(&agent, Duration::ZERO).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while cached_for_agent(&agent).is_none() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        let catalog = cached_for_agent(&agent).expect("CLI 模式的 cursor 应拿到目录");
        assert_eq!(
            catalog.models,
            ["auto", "gpt-5.6-sol-xhigh", "gemini-4-carbon-high"]
        );
        assert_eq!(catalog.default_model.as_deref(), Some("auto"));
        drop(refresher);
    }

    #[test]
    fn acp_cursor_agents_keep_reading_the_session_catalog() {
        let (agent, _tmp) = cursor_agent("cursor-acp-catalog", None);
        assert!(!agent_reports_catalog(&agent));
        record(
            agent_scope(&agent),
            parse_cursor_models_output(CURSOR_MODELS_OUTPUT),
        );
        assert!(
            cached_for_agent(&agent).is_none(),
            "ACP 的候选只认会话下发的目录"
        );
    }

    fn catalog_agent(name: &str) -> (Agent, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let script = tmp.path().join("fake-codex");
        crate::test_support::write_test_executable(
            &script,
            r#"#!/bin/sh
cd "$(dirname "$0")"
while IFS= read -r line; do
  printf '%s\n' "$line" >> requests.jsonl
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
  case "$line" in
    *'"method":"initialize"'*) printf '{"id":%s,"result":{"userAgent":"catalog-test"}}\n' "$id" ;;
    *'"method":"model/list"'*)
      printf '%s\n' "$CODEX_HOME" >> observed-homes
      printf '{"id":%s,"result":' "$id"
      cat "$CODEX_HOME/catalog.json"
      printf '}\n' ;;
  esac
done
"#,
        );
        let mut agent: Agent = serde_yaml::from_str(&format!(
            "name: {name}\nruntime:\n  cli: codex\n  cli_cmd: {}\n  env:\n    CODEX_HOME: {}\n",
            script.display(),
            tmp.path().display()
        ))
        .unwrap();
        agent.dir = tmp.path().join("agents").join(name);
        write_catalog(tmp.path(), "first-model");
        (agent, tmp)
    }

    fn write_catalog(home: &std::path::Path, model: &str) {
        std::fs::write(
            home.join("catalog.json"),
            serde_json::json!({
                "data": [{"id": model, "hidden": false, "isDefault": true}], "nextCursor": null,
            })
            .to_string(),
        )
        .unwrap();
    }

    fn wait_for_model(agent: &Agent, model: &str) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if cached_for_agent(agent).is_some_and(|catalog| catalog.models == [model]) {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("expected {model}, got {:?}", cached_for_agent(agent));
    }

    #[test]
    fn background_catalog_refresh_updates_candidates_without_opening_a_session() {
        let (agent, tmp) = catalog_agent("catalog-refresh");
        let refresh = start_refreshing_with_interval(&agent, Duration::ZERO).unwrap();
        wait_for_model(&agent, "first-model");
        write_catalog(tmp.path(), "gpt-6.1-sol");
        let started = Instant::now();
        let _ = models_for_agent(&agent);
        assert!(started.elapsed() < Duration::from_millis(500));
        wait_for_model(&agent, "gpt-6.1-sol");
        record_for_agent(
            &agent,
            CodexModelCatalog {
                models: vec!["older-session-model".into()],
                default_model: None,
            },
        );
        assert_eq!(cached_for_agent(&agent).unwrap().models, ["gpt-6.1-sol"]);
        drop(refresh);
        let requests = std::fs::read_to_string(tmp.path().join("requests.jsonl")).unwrap();
        let methods: Vec<_> = requests
            .lines()
            .map(|line| {
                let value: serde_json::Value = serde_json::from_str(line).unwrap();
                value["method"].as_str().unwrap().to_string()
            })
            .collect();
        assert_eq!(
            methods
                .iter()
                .filter(|method| *method == "model/list")
                .count(),
            2
        );
        assert!(methods
            .iter()
            .all(|method| ["initialize", "initialized", "model/list"].contains(&method.as_str())));
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("observed-homes"))
                .unwrap()
                .trim()
                .lines()
                .collect::<Vec<_>>(),
            [tmp.path().to_str().unwrap(); 2]
        );
    }

    #[test]
    fn background_catalog_refresh_is_throttled_and_stops_with_its_dispatcher() {
        let (agent, tmp) = catalog_agent("catalog-throttle");
        let refresh = start_refreshing(&agent).unwrap();
        wait_for_model(&agent, "first-model");
        for _ in 0..20 {
            let _ = models_for_agent(&agent);
        }
        std::thread::sleep(Duration::from_millis(150));
        drop(refresh);
        let requests = std::fs::read_to_string(tmp.path().join("requests.jsonl")).unwrap();
        assert_eq!(requests.matches("model/list").count(), 1);
        let _ = models_for_agent(&agent);
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("requests.jsonl")).unwrap(),
            requests
        );
    }

    #[test]
    fn background_catalog_refresh_isolates_account_changes() {
        let (mut agent, _tmp) = catalog_agent("catalog-account-change");
        let refresh = start_refreshing(&agent).unwrap();
        wait_for_model(&agent, "first-model");
        let other = tempfile::tempdir().unwrap();
        write_catalog(other.path(), "other-account-model");
        agent
            .runtime
            .as_mut()
            .unwrap()
            .env
            .as_mut()
            .unwrap()
            .insert(
                "CODEX_HOME".into(),
                other.path().to_string_lossy().into_owned(),
            );
        assert!(cached_for_agent(&agent).is_none());
        let _ = models_for_agent(&agent);
        wait_for_model(&agent, "other-account-model");
        drop(refresh);
    }

    #[test]
    fn background_catalog_refresh_keeps_a_good_catalog_on_failure() {
        let (agent, tmp) = catalog_agent("catalog-refresh-failure");
        let refresh = start_refreshing_with_interval(&agent, Duration::ZERO).unwrap();
        wait_for_model(&agent, "first-model");
        std::fs::write(tmp.path().join("catalog.json"), "null").unwrap();
        let _ = models_for_agent(&agent);
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let requests = std::fs::read_to_string(tmp.path().join("requests.jsonl")).unwrap();
            if requests.matches("model/list").count() == 2 {
                break;
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(10));
        }
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(cached_for_agent(&agent).unwrap().models, ["first-model"]);
        drop(refresh);
    }

    #[test]
    fn catalog_scope_tracks_resolved_environment_and_cli() {
        let (mut agent, _tmp) = catalog_agent("catalog-scope");
        let original = agent_scope(&agent);
        agent.runtime.as_mut().unwrap().cli_cmd = Some("another-codex".into());
        assert_ne!(agent_scope(&agent), original);
        let config = agent_probe_config(&agent);
        assert!(!config
            .env
            .iter()
            .any(|(key, _)| summon_core::agent_identity::is_runtime_owned_env_key(key)));
    }

    #[test]
    fn only_the_codex_family_reports_a_catalog() {
        assert!(reports_catalog("codex"));
        assert!(reports_catalog("traex"));
        assert!(!reports_catalog("claude"));
        // cursor 也有自报目录,但它走 ACP 会话那条路,不经这里。
        assert!(!reports_catalog("cursor"));
    }

    /// 覆盖 onboard/bind 真正走的那条链:cli kind → 二进制路径 → 起进程 → 目录 → 缓存。
    /// 单测里这一段全是替身,只有真机能证明它接得上。
    #[test]
    #[ignore = "需要本机已登录的 codex，仅读 model/list，不起会话、不消耗 token"]
    fn a_local_codex_resolves_all_the_way_to_a_cached_catalog() {
        let catalog = cached_or_probe("codex").expect("本机装了并登录了 codex 就该拿到目录");
        assert!(!catalog.models.is_empty());
        assert!(
            cached(&default_account_scope("codex")).is_some(),
            "探测到的目录要留在缓存里,第二次问不该再起进程"
        );
        assert_eq!(
            upstream_default_model("codex"),
            catalog.default_model,
            "onboard 的默认值取的就是目录里那个 isDefault"
        );
    }

    /// traex 的二进制名与 codex 不同,而 `Tool::for_cli_kind` 不认 traex —— 走错解析函数
    /// 会让它静默拿不到目录(没有报错,只是默认值悄悄退回配置桶)。
    #[test]
    #[ignore = "需要本机已登录的 traex，仅读 model/list"]
    fn a_local_traex_resolves_its_own_binary_and_reports_a_catalog() {
        let catalog = cached_or_probe("traex").expect("traex 走的是同一个 model/list");
        assert!(!catalog.models.is_empty());
    }

    #[test]
    fn an_empty_catalog_never_overwrites_a_good_one() {
        let scope = "test:empty-guard".to_string();
        record(
            scope.clone(),
            CodexModelCatalog {
                models: vec!["gpt-6-astra".into()],
                default_model: Some("gpt-6-astra".into()),
            },
        );
        record(scope.clone(), CodexModelCatalog::default());
        assert_eq!(cached(&scope).unwrap().models, ["gpt-6-astra"]);
    }

    /// 两个 codex agent 连不同账号(一个配了自己的 `CODEX_HOME`)时,目录不能互相看见 ——
    /// 串了就会让一个 agent 的面板列出另一个账号才有的模型,点一下写出起不来的 agent.yaml。
    #[test]
    fn two_codex_agents_never_see_each_others_catalog() {
        let enterprise: Agent = serde_yaml::from_str(
            "name: enterprise\nruntime:\n  cli: codex\n  env:\n    CODEX_HOME: /tmp/enterprise\n",
        )
        .unwrap();
        let personal: Agent =
            serde_yaml::from_str("name: personal\nruntime:\n  cli: codex\n").unwrap();

        record_for_agent(
            &enterprise,
            CodexModelCatalog {
                models: vec!["enterprise-only-model".into()],
                default_model: Some("enterprise-only-model".into()),
            },
        );

        assert_eq!(
            cached_for_agent(&enterprise).unwrap().models,
            ["enterprise-only-model"]
        );
        assert!(
            cached_for_agent(&personal).is_none(),
            "另一个账号的 agent 只能拿到空,回退配置桶"
        );
    }

    /// 目录只对自报的那家有意义;claude/cursor 走别的路,读到的必须是空。
    #[test]
    fn agents_without_a_backend_catalog_read_nothing() {
        let claude: Agent = serde_yaml::from_str("name: c\nruntime:\n  cli: claude\n").unwrap();
        assert!(cached_for_agent(&claude).is_none());
    }

    /// 起进程问目录只允许发生在人在终端前等着的一次性命令里。daemon 的 chat 线程上跑一次
    /// 就是把面板挂住最坏两个 `PROBE_TIMEOUT`,而那只有真机能看出来 —— 所以在这里钉死。
    #[test]
    fn default_account_catalog_call_sites_are_whitelisted() {
        const ALLOWED: &[&str] = &[
            "crates/summon/src/setup/bind.rs",
            "crates/summon/src/setup/onboard.rs",
            "crates/summon/src/cli/dev.rs",
            "crates/summon/src/codex/model_catalog.rs",
        ];
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(std::path::Path::parent)
            .expect("workspace 根");
        let mut offenders = Vec::new();
        let mut stack = vec![root.join("crates")];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).expect("读目录").flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                if path.extension().is_none_or(|ext| ext != "rs") {
                    continue;
                }
                let rel = path
                    .strip_prefix(root)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .replace('\\', "/");
                if ALLOWED.contains(&rel.as_str()) {
                    continue;
                }
                let text = std::fs::read_to_string(&path).unwrap_or_default();
                if text.contains("cached_or_probe") || text.contains("upstream_default_model") {
                    offenders.push(rel);
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "这些文件调了会起进程的目录探测,不在白名单里:{offenders:?}"
        );
    }
}
