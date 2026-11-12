//! Cursor 个人额度的主动查询(IO 半边;来源判定、频控与解析在 `summon_core::cursor_quota`)。
//!
//! Cursor 没有任何通道把额度推给 summon(ACP、`-p stream-json`、statusLine 输入都不带),只能去
//! cursor.com 查。cursor 一轮开跑时触发(ACP 与 `-p` 两条无头通道都经 `stream_turn_headless`),
//! 查询在后台线程做,不拖这一轮;同一凭据来源在
//! `cursor.quota.interval_secs` 内只查一次,失败也计入。结果整值写进 agent 额度缓存,凭据失效或
//! 接口不认时清掉,角标随之不显示额度。
//!
//! 凭据只在 daemon 进程内用、只发往 cursor.com:不写日志,不进 agent 子进程环境。取凭据的子进程
//! 与 cursor 子进程一样先清空环境再显式给,看不到 daemon 环境里别的 agent 的凭据。

use std::io::Read as _;
use std::path::PathBuf;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context as _};
use summon_core::cursor_quota::{
    self as core_quota, Begin, FailureKind, IndividualQuota, LoginStore, QuotaSources,
    SourceResult, TokenSource,
};
use summon_core::driver::CliDriver;
use summon_core::registry::Agent;

use crate::observe::log::log;

/// 取凭据的子进程(钥匙串 / 用户命令)从启动到输出结束最多等这么久。
const TOKEN_COMMAND_TIMEOUT: Duration = Duration::from_secs(10);
/// 取凭据的子进程输出上限。token 是几百字节的 JWT,读满这么多还没完就不是在输出 token。
const MAX_TOKEN_OUTPUT_BYTES: u64 = 64 * 1024;
const HTTP_TIMEOUT: Duration = Duration::from_secs(15);
/// `security` 找不到钥匙串条目时的退出码(errSecItemNotFound)。
const SECURITY_ITEM_NOT_FOUND: i32 = 44;

/// 每轮开跑时调用(所有无头通道):到了间隔就在后台查一次个人额度,结果写给此刻用这个凭据来源
/// 的每个 agent。`show_quota` 是这个会话的「额度显示」开关。
pub(crate) fn on_turn_start(agent: &Agent, show_quota: bool) {
    if CliDriver::from_kind(agent.cli_kind()) != CliDriver::Cursor {
        // 不再是 cursor 的 agent(热重载换了 cli)不能留着 cursor 的额度:它和别的 CLI 共用一格缓存。
        detach(sources(), &agent.name, &write_cache);
        return;
    }
    // 只是这个会话没开额度显示:不动缓存,别的会话可能开着。也不为它读配置文件。
    if !show_quota {
        return;
    }
    let cfg = summon_core::config::Config::load().cursor.quota;
    let Some(source) = core_quota::source_for_turn(agent, show_quota, &cfg) else {
        // 整机关掉,或配置改成了证不了账号的形态:已显示的额度一并撤下。
        detach(sources(), &agent.name, &write_cache);
        return;
    };
    let key = source.key();
    let interval = cfg.interval();
    if !start(
        sources(),
        &key,
        &agent.name,
        Instant::now(),
        interval,
        &write_cache,
    ) {
        return;
    }
    let env = command_env(&source, agent);
    let name = agent.name.clone();
    let spawned = std::thread::Builder::new()
        .name("cursor-quota".into())
        .spawn(move || {
            let outcome = query(&source, &env).map_err(|(kind, e)| {
                // 错误链里只有来源、状态码与解析结论,凭据本身从不进 anyhow 上下文。
                let effect = match kind {
                    FailureKind::Transient => summon_i18n::text!("rce5e59e6d3"),
                    FailureKind::Unavailable => summon_i18n::text!("re483ae2ce3"),
                };
                log(&summon_i18n::format!("rd9bf2af423", interval.as_secs()));
                kind
            });
            complete(sources(), &key, outcome, &write_cache);
        });
    // 线程起不来只丢这一次观测;名额已占,下个间隔再试。
    if let Err(e) = spawned {
        log(&summon_i18n::format!("rbf81af55da", agent.name));
    }
}

fn sources() -> &'static Mutex<QuotaSources> {
    static SOURCES: OnceLock<Mutex<QuotaSources>> = OnceLock::new();
    SOURCES.get_or_init(|| Mutex::new(QuotaSources::default()))
}

/// 查询结论落进账号缓存:整值覆盖或清掉,见 `replace_account_quota_shared` 为什么不排序。
fn write_cache(agent: &str, result: SourceResult) {
    crate::capture::rpc_pool::replace_account_quota_shared(
        agent,
        result.cached_quota(),
        chrono::Utc::now().timestamp(),
    );
}

/// 登记本 agent 用这个来源,返回这一轮要不要去查。间隔内复用该来源最近的结论,直接写给本 agent。
fn start(
    sources: &Mutex<QuotaSources>,
    key: &str,
    agent: &str,
    now: Instant,
    interval: Duration,
    write: &dyn Fn(&str, SourceResult),
) -> bool {
    let begin = sources
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .begin(key, agent, now, interval);
    match begin {
        Begin::Query => true,
        Begin::Reuse(Some(result)) => {
            write(agent, result);
            false
        }
        Begin::Reuse(None) => false,
    }
}

/// 查询回来:把结论写给此刻仍用这个来源的每个 agent;暂时性失败什么也不写。
fn complete(
    sources: &Mutex<QuotaSources>,
    key: &str,
    outcome: Result<IndividualQuota, FailureKind>,
    write: &dyn Fn(&str, SourceResult),
) {
    let Some(result) = core_quota::settle(outcome) else {
        return;
    };
    let agents = sources
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .finish(key, result);
    for agent in agents {
        write(&agent, result);
    }
}

/// 解除 agent 的来源归属;挂过来源的,把它缓存里的额度一并清掉。
fn detach(sources: &Mutex<QuotaSources>, agent: &str, write: &dyn Fn(&str, SourceResult)) {
    let attached = sources
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .detach(agent);
    if attached {
        write(agent, SourceResult::Unavailable);
    }
}

/// 取凭据的子进程看到的环境,与 cursor 子进程同一套基线:系统基础变量,agent 级命令再加上该
/// agent 的 `runtime.env`(它声明的就是「在我的环境里这样取」;runtime-owned 的身份键照旧挡掉)。
/// 全局命令与钥匙串读的是默认登录态,只给基础变量。
fn command_env(source: &TokenSource, agent: &Agent) -> Vec<(String, String)> {
    let mut env = crate::runtime::daemon::essential_system_env();
    if matches!(source, TokenSource::AgentCommand { .. }) {
        env.extend(
            crate::runtime::daemon::resolve_extra_env(agent)
                .into_iter()
                .filter(|(key, _)| !summon_core::agent_identity::is_runtime_owned_env_key(key)),
        );
    }
    env
}

fn query(
    source: &TokenSource,
    env: &[(String, String)],
) -> Result<IndividualQuota, (FailureKind, anyhow::Error)> {
    let unavailable = |e| (FailureKind::Unavailable, e);
    let token = read_token(source, env).map_err(unavailable)?;
    let cookie = core_quota::session_cookie(&token)
        .ok_or_else(|| anyhow!(summon_i18n::format!("r33cdf6c168")))
        .map_err(unavailable)?;
    fetch(&cookie)
}

fn read_token(source: &TokenSource, env: &[(String, String)]) -> anyhow::Result<String> {
    match source {
        TokenSource::AgentCommand { command, .. } | TokenSource::GlobalCommand(command) => {
            let mut shell = clean_command("sh", env);
            shell.arg("-c").arg(command);
            let output = run_with_timeout(shell, TOKEN_COMMAND_TIMEOUT)
                .context(summon_i18n::text!("rd83e491987"))?;
            if !output.status.success() {
                bail!(summon_i18n::format!("r1a026700e0", output.status.code()));
            }
            first_line(&output.stdout).context(summon_i18n::text!("r32c958c763"))
        }
        TokenSource::CursorAgentLogin => read_cursor_agent_login(env),
    }
}

fn read_cursor_agent_login(env: &[(String, String)]) -> anyhow::Result<String> {
    let home = dirs::home_dir().context(summon_i18n::text!("r23eb4fe2ef"))?;
    let xdg = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from);
    match core_quota::cursor_agent_login_store(cfg!(target_os = "macos"), &home, xdg.as_deref()) {
        LoginStore::Keychain { service, account } => {
            let mut security = clean_command("/usr/bin/security", env);
            security.args(["find-generic-password", "-s", service, "-a", account, "-w"]);
            let output = run_with_timeout(security, TOKEN_COMMAND_TIMEOUT)
                .context(summon_i18n::text!("rde586ebe91"))?;
            if output.status.code() == Some(SECURITY_ITEM_NOT_FOUND) {
                bail!(summon_i18n::format!("r362b532827"));
            }
            if !output.status.success() {
                bail!(summon_i18n::format!("rbc92500430", output.status.code()));
            }
            first_line(&output.stdout).context(summon_i18n::text!("re228dfc2fe"))
        }
        LoginStore::File(path) => {
            let text = std::fs::read_to_string(&path)
                .with_context(|| summon_i18n::format!("rb1a59430f1", path.display()))?;
            core_quota::access_token_from_auth_json(&text)
                .with_context(|| summon_i18n::format!("ra5414a4a14", path.display()))
        }
    }
}

fn clean_command(program: &str, env: &[(String, String)]) -> Command {
    let mut command = Command::new(program);
    command.env_clear();
    command.envs(env.iter().map(|(k, v)| (k, v)));
    command
}

fn first_line(stdout: &[u8]) -> Option<String> {
    String::from_utf8_lossy(stdout)
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_string)
}

struct CommandOutput {
    status: ExitStatus,
    stdout: Vec<u8>,
}

/// 跑取凭据的子进程,从启动到输出结束整体受 `timeout` 约束。
///
/// 硬保证是查询线程不会挂住:stdout 在独立线程读,读到 EOF 也受同一个 deadline 约束 ——
/// 孙进程握着管道写端时,子进程退出不等于输出结束。子进程自成进程组,超时 killpg 整组,尽量把
/// 包装脚本拉起的后台 helper 一并收走;组长已退出时 macOS 上偶有成员漏过这一枪,所以这只是尽力。
fn run_with_timeout(mut command: Command, timeout: Duration) -> anyhow::Result<CommandOutput> {
    use std::os::unix::process::CommandExt as _;
    let mut child = command
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let group = nix::unistd::Pid::from_raw(child.id() as i32);
    // 组里已经没人时 killpg 报 ESRCH,正是想要的结果,可忽略。
    let kill_group = || {
        let _ = nix::sys::signal::killpg(group, nix::sys::signal::Signal::SIGKILL);
    };
    let stdout = child
        .stdout
        .take()
        .context(summon_i18n::text!("r6ed6908391"))?;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let read = stdout
            .take(MAX_TOKEN_OUTPUT_BYTES)
            .read_to_end(&mut buf)
            .map(|_| buf);
        // 接收端超时后已放弃,发送失败可忽略。
        let _ = tx.send(read);
    });
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            kill_group();
            let _ = child.wait();
            bail!(summon_i18n::format!("r6d8fd01035", timeout.as_secs_f32()));
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        Ok(read) => Ok(CommandOutput {
            status,
            stdout: read?,
        }),
        Err(_) => {
            kill_group();
            bail!(summon_i18n::format!("r9a8905d08d", timeout.as_secs_f32()));
        }
    }
}

fn fetch(cookie: &str) -> Result<IndividualQuota, (FailureKind, anyhow::Error)> {
    let unavailable = |e: anyhow::Error| (FailureKind::Unavailable, e);
    let transient = |e: anyhow::Error| (FailureKind::Transient, e);
    let mut cookie = reqwest::header::HeaderValue::from_str(cookie)
        .context(summon_i18n::text!("re156f4a91e"))
        .map_err(unavailable)?;
    cookie.set_sensitive(true);
    let response = reqwest::blocking::Client::builder()
        .timeout(HTTP_TIMEOUT)
        // cookie 只发给这一个端点:不跟随跳转,跳走按接口变更处理。
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| transient(e.into()))?
        .get(core_quota::USAGE_SUMMARY_URL)
        .header(reqwest::header::COOKIE, cookie)
        .header(
            reqwest::header::USER_AGENT,
            concat!("summon/", env!("CARGO_PKG_VERSION")),
        )
        .send()
        .map_err(|e| transient(e.into()))?;
    let status = response.status();
    if let Some(kind) = core_quota::classify_http_status(status.as_u16()) {
        return Err((kind, anyhow!(summon_i18n::format!("r9188957c46"))));
    }
    let body: serde_json::Value = response
        .json()
        .context(summon_i18n::text!("r6d41ddbecf"))
        .map_err(unavailable)?;
    core_quota::quota_from_usage_summary(&body)
        .ok_or_else(|| unavailable(anyhow!(summon_i18n::format!("rc3b7076f36"))))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_env() -> Vec<(String, String)> {
        crate::runtime::daemon::essential_system_env()
    }

    fn global(command: &str) -> TokenSource {
        TokenSource::GlobalCommand(command.into())
    }

    /// 调用点是这套机制唯一的入口;删掉它,单测照样全绿,角标却再也不会有 Cursor 额度。
    #[test]
    fn headless_turns_reach_the_query_entry() {
        let chat_thread = include_str!("daemon/chat_thread.rs");
        assert!(chat_thread
            .contains("let quota_badge_on = super::cards::quota_badge_enabled(agent, &tag);"));
        assert!(chat_thread
            .contains("crate::runtime::cursor_quota::on_turn_start(agent, quota_badge_on);"));
    }

    #[test]
    fn token_command_output_is_its_first_non_empty_line() {
        let token = read_token(&global("printf '\\n  tok \\nignored\\n'"), &base_env());
        assert_eq!(token.unwrap(), "tok");
    }

    #[test]
    fn failing_or_silent_token_command_is_an_error() {
        assert!(read_token(&global("echo tok; exit 3"), &base_env()).is_err());
        assert!(read_token(&global("true"), &base_env()).is_err());
    }

    #[test]
    fn token_command_sees_only_the_environment_it_is_given() {
        let mut shell = clean_command("sh", &base_env());
        shell.arg("-c").arg("env");
        let output = run_with_timeout(shell, TOKEN_COMMAND_TIMEOUT).unwrap();
        let allowed: std::collections::HashSet<String> = base_env()
            .into_iter()
            .map(|(k, _)| k)
            .chain(["PWD", "OLDPWD", "SHLVL", "_"].map(String::from))
            .collect();
        let seen: Vec<String> = String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|line| line.split_once('=').map(|(k, _)| k.to_string()))
            .filter(|k| !allowed.contains(k))
            .collect();
        assert!(seen.is_empty(), "daemon 环境漏进了取凭据的命令: {seen:?}");
    }

    #[test]
    fn agent_command_runs_in_the_agents_own_environment() {
        let temp = tempfile::tempdir().unwrap();
        let mut agent: Agent = serde_yaml::from_str(
            "name: c\nruntime:\n  cli: cursor\n  env:\n    QUOTA_PROBE_HOME: /srv/alt\n    SUMMON_AGENT: someone-else\n",
        )
        .unwrap();
        agent.dir = temp.path().join("agents").join("c");
        let own = TokenSource::AgentCommand {
            agent: "c".into(),
            command: "x".into(),
        };
        let env = command_env(&own, &agent);
        assert!(env.contains(&("QUOTA_PROBE_HOME".into(), "/srv/alt".into())));
        assert!(
            !env.iter().any(|(k, _)| k == "SUMMON_AGENT"),
            "身份键只由 runtime 注入"
        );
        assert!(
            !command_env(&global("x"), &agent)
                .iter()
                .any(|(k, _)| k == "QUOTA_PROBE_HOME"),
            "全局命令读默认账号,不带 agent 的环境"
        );
    }

    #[test]
    fn hung_token_command_is_killed_at_the_deadline() {
        let mut shell = clean_command("sh", &base_env());
        shell.arg("-c").arg("sleep 5");
        let started = Instant::now();
        assert!(run_with_timeout(shell, Duration::from_millis(200)).is_err());
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    /// 子进程退出了、后台孙进程还握着 stdout:读输出同样受 deadline 约束,查询线程不会挂住。
    #[test]
    fn a_grandchild_holding_stdout_cannot_outlive_the_deadline() {
        let mut shell = clean_command("sh", &base_env());
        shell.arg("-c").arg("echo tok; sleep 5 &");
        let started = Instant::now();
        assert!(run_with_timeout(shell, Duration::from_millis(300)).is_err());
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    type Writes = std::cell::RefCell<Vec<(String, SourceResult)>>;

    fn recorder(writes: &Writes) -> impl Fn(&str, SourceResult) + '_ {
        |agent: &str, result: SourceResult| writes.borrow_mut().push((agent.to_string(), result))
    }

    fn quota(left_pct: u8) -> IndividualQuota {
        IndividualQuota {
            left_pct,
            resets_at: None,
        }
    }

    #[test]
    fn a_finished_query_reaches_every_agent_on_its_source() {
        let sources = Mutex::new(QuotaSources::default());
        let writes = Writes::default();
        let write = recorder(&writes);
        let (t0, interval) = (Instant::now(), Duration::from_secs(600));

        assert!(start(&sources, "k", "a", t0, interval, &write));
        assert!(
            !start(&sources, "k", "b", t0, interval, &write),
            "查询在路上"
        );
        complete(&sources, "k", Ok(quota(96)), &write);
        let got = writes.take();
        assert_eq!(
            got,
            vec![
                ("a".to_string(), SourceResult::Quota(quota(96))),
                ("b".to_string(), SourceResult::Quota(quota(96))),
            ]
        );

        assert!(!start(&sources, "k", "c", t0, interval, &write));
        assert_eq!(
            writes.take(),
            vec![("c".to_string(), SourceResult::Quota(quota(96)))],
            "间隔内复用最近结论,直接写给本 agent"
        );
    }

    #[test]
    fn unavailable_clears_every_agent_and_transient_touches_nothing() {
        let sources = Mutex::new(QuotaSources::default());
        let writes = Writes::default();
        let write = recorder(&writes);
        let (t0, interval) = (Instant::now(), Duration::from_secs(600));
        start(&sources, "k", "a", t0, interval, &write);
        start(&sources, "k", "b", t0, interval, &write);

        complete(&sources, "k", Err(FailureKind::Transient), &write);
        assert!(writes.take().is_empty(), "暂时性失败保留已知值");

        complete(&sources, "k", Err(FailureKind::Unavailable), &write);
        assert_eq!(
            writes.take(),
            vec![
                ("a".to_string(), SourceResult::Unavailable),
                ("b".to_string(), SourceResult::Unavailable),
            ]
        );
    }

    #[test]
    fn detaching_clears_only_agents_that_had_a_source() {
        let sources = Mutex::new(QuotaSources::default());
        let writes = Writes::default();
        let write = recorder(&writes);
        start(
            &sources,
            "k",
            "a",
            Instant::now(),
            Duration::from_secs(600),
            &write,
        );

        detach(&sources, "a", &write);
        detach(&sources, "never-attached", &write);
        assert_eq!(
            writes.take(),
            vec![("a".to_string(), SourceResult::Unavailable)]
        );
        complete(&sources, "k", Ok(quota(50)), &write);
        assert!(writes.take().is_empty(), "解除后不再收这个来源的结果");
    }

    /// 换了账号的新周期可能比旧账号的更早结束,写缓存时照样要压过去;失效时清掉。
    #[test]
    fn cache_writes_follow_the_latest_pull_even_across_accounts() {
        let agent = "cursor-quota-cache-write";
        let pulled = |left_pct, resets_at| {
            SourceResult::Quota(IndividualQuota {
                left_pct,
                resets_at: Some(resets_at),
            })
        };
        write_cache(agent, pulled(20, 1_790_812_800));
        write_cache(agent, pulled(80, 1_790_294_400));
        assert_eq!(
            crate::capture::rpc_pool::current_quota_shared(agent, "g"),
            Some(80)
        );
        write_cache(agent, SourceResult::Unavailable);
        assert_eq!(
            crate::capture::rpc_pool::current_quota_shared(agent, "g"),
            None
        );
    }

    /// 真机:用本机 cursor-agent 的登录态查一次 cursor.com。需要网络与已登录的 cursor-agent。
    #[test]
    #[ignore]
    fn real_cursor_agent_login_yields_an_individual_quota() {
        let quota = query(&TokenSource::CursorAgentLogin, &base_env())
            .map_err(|(kind, e)| format!("{kind:?}: {e:#}"))
            .expect("查询应成功");
        assert!(quota.left_pct <= 100);
        assert!(quota.resets_at.is_some(), "响应应带计费周期结束时刻");
        eprintln!("cursor 个人额度剩余 {}%", quota.left_pct);
    }
}
