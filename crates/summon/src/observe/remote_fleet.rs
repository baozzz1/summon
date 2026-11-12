//! 远端 summon 机器的 fleet 快照拉取 + 缓存(跨机 fleet 聚合)。
//!
//! **为什么要缓存,而不是用时现拉**:管家(steward)的入站处理是 daemon 内的**同步**路径 ——
//! 收消息 → 读快照 → 回卡片,整段跑在那个 agent 的 dispatcher 线程上。在里面 fork 一个 ssh
//! 等好几秒,等于让一条 `/fleet` 把整个 agent 的消息处理堵住;远端一旦不可达(超时 5s)更糟。
//! 所以拉取只在**后台线程**做,结果写进缓存,渲染侧永远只读缓存、永不阻塞。
//!
//! 代价是数据有滞后 —— 那就**如实标注**「N 秒前」,而不是假装它是此刻的真值。fleet 卡的既有
//! 纪律就是这条:daemon 不在时也宁可明说「以下是盘上事实」,也不把「不知道」渲成「都闲着」。
//!
//! **拉取用 ssh 而不是别的**:daemon 的控制面是 unix socket,跨机不开 HTTP 控制面(控制台只管
//! 本机配置);ssh 是这台机器上已经存在、已鉴权的通道,复用它就不必为跨机再造一套认证与监听端口。

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use summon_core::config::Remote;
use summon_core::fleet::FleetSnapshot;

/// 单次 ssh 拉取的墙钟上限。**必须有**:ssh 卡住的形态是"一直不返回",没有上限就是后台线程
/// 永久泄漏一个。5s 连接 + 15s 总时长,对局域网/内网机器足够宽松。
const SSH_CONNECT_TIMEOUT_SECS: u64 = 5;
const FETCH_TIMEOUT: Duration = Duration::from_secs(15);

/// 后台刷新间隔。fleet 状态的消费场景是"人点开看一眼",30s 的滞后在这个用途下无感;
/// 拉得更勤只是给每台远端机器平白多开 ssh。
pub const REFRESH_INTERVAL: Duration = Duration::from_secs(30);

/// 一台远端机器的最新已知状态。
#[derive(Debug, Clone)]
pub struct RemoteSnapshot {
    /// 配置里的展示名。
    pub name: String,
    /// 最后一次**成功**拉到的快照。从没成功过 = None。
    pub snapshot: Option<FleetSnapshot>,
    /// 那次成功距今多久(用于卡上「N 秒前」)。
    pub age: Option<Duration>,
    /// 最近一次拉取的错误(成功则 None)。**成功过之后再失败,两者会同时有值** ——
    /// 那正是要如实呈现的状态:「数据是 2 分钟前的,现在连不上」。
    pub error: Option<String>,
}

impl RemoteSnapshot {
    /// 这台机器此刻可不可信地提供数据(有过成功拉取)。
    pub fn has_data(&self) -> bool {
        self.snapshot.is_some()
    }
}

/// 缓存条目(内部态)。
struct Entry {
    snapshot: Option<FleetSnapshot>,
    fetched_at: Option<Instant>,
    error: Option<String>,
}

/// 进程内共享缓存:后台线程写,渲染侧读。
///
/// 锁中毒一律降级成"没有远端数据",不 panic —— 跨机聚合是附加信息,不该因为它让 `/fleet` 挂掉。
fn cache() -> &'static Mutex<HashMap<String, Entry>> {
    static CACHE: OnceLock<Mutex<HashMap<String, Entry>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 读某台远端机器的最新已知状态。**永不阻塞、永不发起网络**。
pub fn cached(name: &str) -> Option<RemoteSnapshot> {
    let map = cache().lock().ok()?;
    let entry = map.get(name)?;
    Some(RemoteSnapshot {
        name: name.to_string(),
        snapshot: entry.snapshot.clone(),
        age: entry.fetched_at.map(|t| t.elapsed()),
        error: entry.error.clone(),
    })
}

/// 按配置顺序读出全部远端状态(配置里有、但还没拉过的也给一条"还没拉到"的占位)。
///
/// **占位不能省**:配了 remote 却一条都不显示,和"没配"在卡上长得一模一样,人会以为配置没生效。
pub fn cached_all(remotes: &[Remote]) -> Vec<RemoteSnapshot> {
    remotes
        .iter()
        .filter(|r| r.is_usable())
        .map(|r| {
            cached(&r.name).unwrap_or_else(|| RemoteSnapshot {
                name: r.name.clone(),
                snapshot: None,
                age: None,
                error: Some(summon_i18n::text!("r5912e38af3").to_string()),
            })
        })
        .collect()
}

/// 拉一次某台远端机器的 fleet 快照并写进缓存。**这是会阻塞的那一半**,只许后台线程调。
///
/// 失败**不清掉**上一次成功的数据:一次网络抖动不该让整台机器从卡上消失。旧数据 + 错误
/// 同时呈现,由渲染侧说清楚「这是 N 秒前的,现在连不上」。
pub fn refresh_one(remote: &Remote) {
    if !remote.is_usable() {
        return;
    }
    let outcome = fetch(remote);
    let Ok(mut map) = cache().lock() else {
        return;
    };
    let entry = map.entry(remote.name.clone()).or_insert(Entry {
        snapshot: None,
        fetched_at: None,
        error: None,
    });
    // **只在状态翻转时记日志**:后台每 30s 拉一次,每次都写就是一天 2880 行噪音,
    // 真正该被看见的是「本来好的连不上了」和「连不上的恢复了」这两个时刻。
    let was_ok = entry.error.is_none() && entry.snapshot.is_some();
    match outcome {
        Ok(snapshot) => {
            let agents = snapshot.agents.len();
            let first = entry.snapshot.is_none();
            entry.snapshot = Some(snapshot);
            entry.fetched_at = Some(Instant::now());
            entry.error = None;
            if !was_ok {
                let what = if first {
                    summon_i18n::text!("rdb70d14959")
                } else {
                    summon_i18n::text!("r3617f737f4")
                };
                crate::observe::log::log(&summon_i18n::format!("r9178c94f6d", remote.name));
            }
        }
        Err(e) => {
            if was_ok || entry.error.as_deref() != Some(e.as_str()) {
                // 保留上次成功的数据,只把错误记上 —— 一次网络抖动不该让整台机器从卡上消失。
                crate::observe::log::log(&summon_i18n::format!("rff927cb7c3", remote.name));
            }
            entry.error = Some(e);
        }
    }
}

/// ssh 过去跑一次 `summon agents status --json` 并解析。
///
/// 三个 ssh 选项都不是可选的:
/// - `BatchMode=yes`:非交互。少了它,一台需要密码/确认指纹的机器会让 ssh **挂在提示符上**
///   直到超时,而不是立刻告诉你"没配好"。
/// - `ConnectTimeout`:连接阶段的上限(总时长另有 `FETCH_TIMEOUT` 兜底)。
/// - `StrictHostKeyChecking=accept-new`:第一次连不卡在指纹确认上,但**不接受变更的 key**
///   (那是中间人的形态,该失败就得失败)。
fn fetch(remote: &Remote) -> Result<FleetSnapshot, String> {
    let output = run_with_timeout(remote).map_err(|e| e.to_string())?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let tail = stderr.trim().lines().last().unwrap_or("").trim();
        return Err(if tail.is_empty() {
            summon_i18n::format!("r4d46c69e06", output.status.code())
        } else {
            // 只带最后一行:ssh 的多行提示大多是噪音,而卡片位窄。
            clip(tail, 120)
        });
    }
    serde_json::from_slice::<FleetSnapshot>(&output.stdout)
        .map_err(|e| summon_i18n::format!("r4a6ee1d124", clip(&e.to_string(), 120)))
}

/// 起 ssh 子进程并等它,超时就杀掉。
///
/// `std::process` 没有带超时的 `wait`,所以用「轮询 `try_wait` + 到点 kill」——比引入一个
/// 异步运行时轻得多,而这里只需要"别永久挂着"这一个保证。
fn run_with_timeout(remote: &Remote) -> std::io::Result<std::process::Output> {
    use std::process::{Command, Stdio};
    let mut child = Command::new(summon_core::tools::resolve(summon_core::tools::Tool::Ssh))
        .arg("-o")
        .arg("BatchMode=yes")
        .arg("-o")
        .arg(format!("ConnectTimeout={SSH_CONNECT_TIMEOUT_SECS}"))
        .arg("-o")
        .arg("StrictHostKeyChecking=accept-new")
        .arg(remote.ssh_host.trim())
        .arg(format!("{} agents status --json", remote.summon_command()))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    let deadline = Instant::now() + FETCH_TIMEOUT;
    loop {
        match child.try_wait()? {
            Some(_) => return child.wait_with_output(),
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    summon_i18n::format!("r4dbc5ffcb4", FETCH_TIMEOUT.as_secs()),
                ));
            }
            None => std::thread::sleep(Duration::from_millis(100)),
        }
    }
}

fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max).collect();
    format!("{head}…")
}

/// 后台刷新线程:每 [`REFRESH_INTERVAL`] 把配置里的每台远端机器拉一遍,结果进缓存。
///
/// **无条件 spawn、tick 内读配置**(与 sentinel 同一套路):`remotes` 为空时纯空转,
/// 而配置热改之后不必重启 daemon 就能生效。
///
/// 睡眠切成小片轮询 `shutdown`,让停机不必等满一整个刷新间隔 —— daemon 优雅停机的预算是秒级,
/// 而刷新间隔是 30s。
pub fn spawn_refresher(
    shutdown: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let tick = Duration::from_millis(250);
        // 首轮立刻拉一次:否则 daemon 刚起来的头 30s 里,`/fleet` 只能显示「还没拉到」。
        let mut next_run = Instant::now();
        while !shutdown.load(std::sync::atomic::Ordering::SeqCst) {
            if Instant::now() >= next_run {
                let remotes = summon_core::config::Config::load().remotes;
                for remote in remotes.iter().filter(|r| r.is_usable()) {
                    if shutdown.load(std::sync::atomic::Ordering::SeqCst) {
                        return;
                    }
                    refresh_one(remote);
                }
                next_run = Instant::now() + REFRESH_INTERVAL;
            }
            std::thread::sleep(tick);
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn remote(name: &str, host: &str) -> Remote {
        Remote {
            name: name.into(),
            ssh_host: host.into(),
            summon_bin: String::new(),
        }
    }

    /// 配了 remote 却还没拉到时**必须给占位**:一条都不显示,和「没配」在卡上长得一样,
    /// 人会以为配置没生效。
    #[test]
    fn 未拉取过的远端也给一条占位() {
        let all = cached_all(&[remote("devbox", "devbox")]);
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].name, "devbox");
        assert!(!all[0].has_data());
        assert!(all[0].error.is_some(), "要说清为什么没有数据");
    }

    /// 名字或 ssh 目标缺一半的配置直接跳过,不拿去 ssh 一个空串。
    #[test]
    fn 残缺配置被跳过而不是去连空主机() {
        assert!(cached_all(&[remote("", "devbox")]).is_empty());
        assert!(cached_all(&[remote("devbox", "  ")]).is_empty());
    }

    /// 未配 `summon_bin` 时裸名走远端 PATH;配了就用配的(ssh 非交互的 PATH 常常不含
    /// `~/.local/bin`,这个出口是必需的)。
    #[test]
    fn 远端可执行文件可显式指定() {
        assert_eq!(remote("d", "d").summon_command(), "summon");
        let mut r = remote("d", "d");
        r.summon_bin = "/home/me/.local/bin/summon".into();
        assert_eq!(r.summon_command(), "/home/me/.local/bin/summon");
    }

    /// 拉不到的机器**不能从卡上消失**,也不能把失败伪装成「没有会话」。
    #[test]
    fn 拉取失败保留上次数据并带出错误() {
        let name = "unreachable-host-for-test";
        refresh_one(&remote(name, "no.such.host.invalid"));
        let got = cached(name).expect("失败也要留一条记录");
        assert!(!got.has_data());
        assert!(got.error.is_some(), "必须说明失败原因");
    }
}
