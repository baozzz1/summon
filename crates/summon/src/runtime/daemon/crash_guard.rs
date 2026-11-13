//! 崩溃后自动拉起的熔断。
//!
//! 开机自启单元在 daemon 异常退出时会立刻把它拉起来(launchd `KeepAlive.SuccessfulExit=false`、
//! systemd `Restart=on-failure`)。
//! 每次启动都会补做停机期间积压的事 —— outbox 投递、恢复卡、错过的定时轮,都是往群里发东西。
//! 一代刚起来不久就崩,自动拉起就成了「每十秒往群里补发一遍」,且撤不回来。所以:上一代没走完
//! 停机、又没稳定跑满 [`MIN_STABLE_UPTIME_SECS`],**不是人工发起的启动一律不起**,以退出码 0 退出
//! (单元不再重试),等人来看。拦下的原因写进 [`refusal_file`],`summon doctor` 摆出来。
//!
//! 只拦得出循环的那一种启动 —— 自启单元自己拉起的那一代 —— 其余一律放行:
//! - 人工启动:`daemon start` 在交给自启单元 / 自己 spawn 之前写启动请求,本代读后删;
//!   终端里直接 `daemon run` 不是自启单元拉起的,也放行。
//! - 这台机器没配崩溃拉起:没有谁会自动重启 daemon,就不存在循环。
//! - 上一代早于本次系统启动:开机 / 登录自启面对的是关机前的那一代,不是刚死的那一代。
//!
//! 判据全在启动记录([`boot_record_file`])里:启动时写、就绪时补上就绪时刻、优雅停机末尾删。
//! 还在 = 上一代没走完停机;被自动拉起时本代启动时刻 ≈ 上一代死亡时刻,减去就绪时刻就是它
//! 就绪后活了多久。

use super::*;

/// 上一代就绪后至少要稳定跑这么久,异常退出后才被自动拉起。
const MIN_STABLE_UPTIME_SECS: u64 = 10 * 60;

/// 启动请求的有效期。`daemon start` 写下它到 daemon 读到它,中间只隔一次进程拉起。
const START_REQUEST_TTL_SECS: u64 = 60;

/// 停机请求的有效期。只用来认刚刚那次停机是谁发起的,停机本身不超过一分钟。
const STOP_REQUEST_TTL_SECS: u64 = 15 * 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct BootRecord {
    pid: u32,
    started_at_secs: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    ready_at_secs: Option<u64>,
}

#[derive(Debug, Serialize, Deserialize)]
struct StartRequest {
    pid: u32,
    at_secs: u64,
}

#[derive(Debug, Serialize, Deserialize)]
struct StopRequest {
    /// 被停的那一代。
    daemon_pid: u32,
    at_secs: u64,
}

/// 上一代留下的现场。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Previous {
    /// 没有启动记录:上一代优雅停机,或这是第一次启动。
    Clean,
    /// 启动记录在,但读不懂(损坏、旧格式)。只知道它什么时候写下的(文件 mtime)。
    Unreadable { written_at_secs: Option<u64> },
    /// 没走完停机。
    Unclean(BootRecord),
}

/// 这次启动所处的环境。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Context {
    /// 这次是人发起的。
    requested: bool,
    /// 本进程是配了崩溃拉起的自启单元拉起的。
    restarted_by_unit: bool,
    now_secs: u64,
    /// 本次系统启动的时刻;取不到为 `None`,不据此放行。
    system_boot_secs: Option<u64>,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum BootVerdict {
    Proceed,
    Refuse(String),
}

fn decide(previous: Previous, ctx: Context) -> BootVerdict {
    if ctx.requested || !ctx.restarted_by_unit {
        return BootVerdict::Proceed;
    }
    let before_this_boot = |at: u64| ctx.system_boot_secs.is_some_and(|boot| at < boot);
    let record = match previous {
        Previous::Clean => return BootVerdict::Proceed,
        Previous::Unreadable { written_at_secs } => {
            if written_at_secs.is_some_and(before_this_boot) {
                return BootVerdict::Proceed;
            }
            return BootVerdict::Refuse(summon_i18n::format!(
                "r40fec295cd",
                boot_record_file().display()
            ));
        }
        Previous::Unclean(record) => record,
    };
    if before_this_boot(record.started_at_secs) {
        return BootVerdict::Proceed;
    }
    let how_long = match record.ready_at_secs {
        // 时钟回拨时 now < ready_at:活了多久证不了,按没跑稳处理。
        Some(ready_at) if ctx.now_secs >= ready_at => {
            let lived = ctx.now_secs - ready_at;
            if lived >= MIN_STABLE_UPTIME_SECS {
                return BootVerdict::Proceed;
            }
            summon_i18n::format!("r93410f956c")
        }
        Some(_) => summon_i18n::text!("r72cc9feed5").to_string(),
        None => summon_i18n::text!("r4d80eeec18").to_string(),
    };
    BootVerdict::Refuse(summon_i18n::format!(
        "rf4bc09b849",
        record.pid,
        MIN_STABLE_UPTIME_SECS / 60
    ))
}

/// 熔断读写的盘上文件。生产走 [`GuardFiles::live`],测试指向临时目录。
struct GuardFiles {
    boot: PathBuf,
    request: PathBuf,
    refused: PathBuf,
}

impl GuardFiles {
    fn live() -> Self {
        Self {
            boot: boot_record_file(),
            request: start_request_file(),
            refused: refusal_file(),
        }
    }
}

/// 启动期调用(拿到单例锁之后、任何会联网的步骤之前):判定这次该不该起,该起就写下本代的启动记录。
pub(super) fn check_and_record(daemon_pid: u32) -> BootVerdict {
    let files = GuardFiles::live();
    let now_secs = unix_now_secs();
    let state = crate::setup::autostart::read_state();
    let ctx = Context {
        requested: take_start_request(&files.request, now_secs),
        restarted_by_unit: state.enabled && launched_by_autostart_unit(),
        now_secs,
        system_boot_secs: system_boot_secs(),
    };
    check_and_record_at(&files, daemon_pid, ctx)
}

/// 读盘出错一律按最保守的现场处理;写盘失败只记日志 —— 启动记录只影响下一次的熔断判断,
/// 拒绝说明只影响 doctor 的展示,都不值得为此挡住本次启动。
fn check_and_record_at(files: &GuardFiles, daemon_pid: u32, ctx: Context) -> BootVerdict {
    let verdict = decide(read_previous(&files.boot), ctx);
    match &verdict {
        BootVerdict::Proceed => {
            let record = BootRecord {
                pid: daemon_pid,
                started_at_secs: ctx.now_secs,
                ready_at_secs: None,
            };
            write_record(&files.boot, &record);
            // 不存在即没被拦过;删不掉只是 doctor 多摆一条旧说明。
            let _ = std::fs::remove_file(&files.refused);
        }
        BootVerdict::Refuse(reason) => {
            let note = format!("{} {reason}", chrono::Local::now().format("%F %T"));
            if let Err(error) = crate::runtime::fsio::atomic_write(&files.refused, note.as_bytes())
            {
                log(&summon_i18n::format!("r0219624c7f"));
            }
        }
    }
    verdict
}

/// 上一代有没有留下启动记录(没走完停机)。须在本代写下自己的记录之前读。
pub(super) fn previous_boot_recorded() -> bool {
    !matches!(read_previous(&boot_record_file()), Previous::Clean)
}

/// 启动时交代上一代怎么结束的,只陈述证据:停机哨兵不在、也没有上一代的启动记录,可能是
/// 第一次起 daemon,也可能是上一代在启动期就被停下。
pub(super) fn previous_exit_log(was_crash: bool, had_boot_record: bool) -> Option<&'static str> {
    match (was_crash, had_boot_record) {
        (false, _) => None,
        (true, true) => Some(summon_i18n::text!("r9626ce3a36")),
        (true, false) => Some(summon_i18n::text!("r9ae86be057")),
    }
}

/// 本代全部入口就绪时调用:把就绪时刻补进启动记录。
pub(super) fn mark_ready(daemon_pid: u32) {
    mark_ready_at(&boot_record_file(), daemon_pid, unix_now_secs());
}

fn mark_ready_at(path: &std::path::Path, daemon_pid: u32, now_secs: u64) {
    let Some(mut record) = read_record(path).filter(|record| record.pid == daemon_pid) else {
        return;
    };
    record.ready_at_secs = Some(now_secs);
    write_record(path, &record);
}

/// 优雅停机末尾调用:删掉本代的启动记录,下一代据此认定上次正常退出。
pub(super) fn clear_boot_record(daemon_pid: u32) {
    clear_boot_record_at(&boot_record_file(), daemon_pid);
}

/// 只删属于本代的那份。删不掉的后果是下次误判为「上一代异常退出」,只会让熔断更保守,可忽略。
fn clear_boot_record_at(path: &std::path::Path, daemon_pid: u32) {
    if read_record(path).is_some_and(|record| record.pid == daemon_pid) {
        let _ = std::fs::remove_file(path);
    }
}

/// 这一代 daemon 没走完停机就没了:它的启动记录还在。优雅停机在删 pidfile 之前就删掉了自己的记录,
/// 所以进程不在了、记录还在,就是崩溃或被杀。
pub(crate) fn left_unfinished(daemon_pid: u32) -> bool {
    read_record(&boot_record_file()).is_some_and(|record| record.pid == daemon_pid)
}

/// 最近一次被熔断拦下的说明(带时刻)。`summon doctor` 展示用;下一次放行的启动会删掉它。
pub(crate) fn last_refusal() -> Option<String> {
    std::fs::read_to_string(refusal_file())
        .ok()
        .map(|note| note.trim().to_string())
        .filter(|note| !note.is_empty())
}

/// `summon daemon start` 调用:声明接下来这次启动是人发起的。
pub(crate) fn write_start_request() -> anyhow::Result<()> {
    write_start_request_at(&start_request_file(), std::process::id(), unix_now_secs())
}

fn write_start_request_at(path: &std::path::Path, pid: u32, at_secs: u64) -> anyhow::Result<()> {
    let request = StartRequest { pid, at_secs };
    crate::runtime::fsio::atomic_write(path, &serde_json::to_vec(&request)?)
}

/// `summon daemon stop` 在发 SIGTERM 之前调用:声明接下来这一代的退出是人发起的。
pub(crate) fn write_stop_request(daemon_pid: u32) -> anyhow::Result<()> {
    write_stop_request_at(&stop_request_file(), daemon_pid, unix_now_secs())
}

fn write_stop_request_at(
    path: &std::path::Path,
    daemon_pid: u32,
    at_secs: u64,
) -> anyhow::Result<()> {
    let request = StopRequest {
        daemon_pid,
        at_secs,
    };
    crate::runtime::fsio::atomic_write(path, &serde_json::to_vec(&request)?)
}

/// 这一代 daemon 是人经 CLI 停下的:停机请求点名它,且在有效期内。
pub(crate) fn stop_requested(daemon_pid: u32) -> bool {
    stop_requested_at(&stop_request_file(), daemon_pid, unix_now_secs())
}

fn stop_requested_at(path: &std::path::Path, daemon_pid: u32, now_secs: u64) -> bool {
    std::fs::read(path)
        .ok()
        .and_then(|raw| serde_json::from_slice::<StopRequest>(&raw).ok())
        .is_some_and(|request| {
            request.daemon_pid == daemon_pid
                && request.at_secs <= now_secs
                && now_secs - request.at_secs <= STOP_REQUEST_TTL_SECS
        })
}

/// 读后即删;在有效期内才算数。删不掉的请求会在有效期内被下一次启动误用一次,过期后自然失效。
fn take_start_request(path: &std::path::Path, now_secs: u64) -> bool {
    let request = std::fs::read(path)
        .ok()
        .and_then(|raw| serde_json::from_slice::<StartRequest>(&raw).ok());
    let _ = std::fs::remove_file(path);
    request.is_some_and(|request| {
        request.at_secs <= now_secs && now_secs - request.at_secs <= START_REQUEST_TTL_SECS
    })
}

fn read_previous(path: &std::path::Path) -> Previous {
    let raw = match std::fs::read(path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Previous::Clean,
        Err(_) => {
            return Previous::Unreadable {
                written_at_secs: None,
            }
        }
    };
    match serde_json::from_slice::<BootRecord>(&raw) {
        Ok(record) => Previous::Unclean(record),
        Err(_) => Previous::Unreadable {
            written_at_secs: std::fs::metadata(path)
                .and_then(|meta| meta.modified())
                .ok()
                .and_then(|at| at.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|since| since.as_secs()),
        },
    }
}

fn read_record(path: &std::path::Path) -> Option<BootRecord> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

fn write_record(path: &std::path::Path, record: &BootRecord) {
    let written = serde_json::to_vec(record)
        .map_err(anyhow::Error::from)
        .and_then(|raw| crate::runtime::fsio::atomic_write(path, &raw));
    if let Err(error) = written {
        log(&summon_i18n::format!("r0eec28e5fc"));
    }
}

/// 本进程是不是自启单元**直接**拉起的那一个。环境变量会被子孙继承(托管 daemon 起的 tmux 里,
/// 人手动敲的 `daemon run` 也带着它们),所以都再核一道「就是我自己」:
/// - launchd 给拉起的进程设 `XPC_SERVICE_NAME=<label>`(实测),且它的父进程是 launchd(pid 1)。
/// - systemd 设 `SYSTEMD_EXEC_PID=<被 exec 的那个 pid>`,且进程落在以单元名结尾的 cgroup 里。
fn launched_by_autostart_unit() -> bool {
    match summon_core::autostart::LoginItemBackend::current() {
        summon_core::autostart::LoginItemBackend::LaunchdUser => launched_by_launchd(
            std::env::var("XPC_SERVICE_NAME").ok().as_deref(),
            nix::unistd::getppid().as_raw(),
        ),
        summon_core::autostart::LoginItemBackend::SystemdUser => launched_by_systemd(
            std::env::var("SYSTEMD_EXEC_PID").ok().as_deref(),
            std::process::id(),
            std::fs::read_to_string("/proc/self/cgroup").ok().as_deref(),
        ),
    }
}

fn launched_by_launchd(service_name: Option<&str>, ppid: i32) -> bool {
    service_name == Some(summon_core::autostart::LAUNCHD_LABEL) && ppid == 1
}

fn launched_by_systemd(exec_pid: Option<&str>, self_pid: u32, cgroup: Option<&str>) -> bool {
    exec_pid.and_then(|pid| pid.parse::<u32>().ok()) == Some(self_pid)
        && cgroup
            .is_some_and(|cgroup| in_unit_cgroup(cgroup, summon_core::autostart::SYSTEMD_UNIT_NAME))
}

/// `/proc/self/cgroup` 里有没有一行落在这个单元的 cgroup 上(v1 的 `name=systemd` 行与 v2 的
/// `0::` 行都是 `<层级>:<控制器>:<路径>`,路径以单元名结尾)。
fn in_unit_cgroup(cgroup: &str, unit: &str) -> bool {
    cgroup
        .lines()
        .filter_map(|line| line.splitn(3, ':').nth(2))
        .any(|path| path.rsplit('/').next() == Some(unit))
}

/// 本次系统启动的时刻(unix 秒)。
#[cfg(target_os = "macos")]
fn system_boot_secs() -> Option<u64> {
    let mut boot = nix::libc::timeval {
        tv_sec: 0,
        tv_usec: 0,
    };
    let mut len = std::mem::size_of::<nix::libc::timeval>();
    // SAFETY:kern.boottime 的值就是一个 timeval;传入的缓冲区与长度正好容纳它。
    let rc = unsafe {
        nix::libc::sysctlbyname(
            c"kern.boottime".as_ptr(),
            (&mut boot as *mut nix::libc::timeval).cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    (rc == 0 && boot.tv_sec > 0).then_some(boot.tv_sec as u64)
}

#[cfg(not(target_os = "macos"))]
fn system_boot_secs() -> Option<u64> {
    std::fs::read_to_string("/proc/stat")
        .ok()?
        .lines()
        .find_map(|line| line.strip_prefix("btime "))
        .and_then(|secs| secs.trim().parse().ok())
}

fn unix_now_secs() -> u64 {
    SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stop_request_names_the_generation_it_stopped_and_expires() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stop-request");
        assert!(!stop_requested_at(&path, 42, NOW), "没有请求");
        write_stop_request_at(&path, 42, NOW).unwrap();
        assert!(stop_requested_at(&path, 42, NOW + 60));
        assert!(!stop_requested_at(&path, 43, NOW + 60), "点名的是别的一代");
        assert!(!stop_requested_at(
            &path,
            42,
            NOW + STOP_REQUEST_TTL_SECS + 1
        ));
        assert!(!stop_requested_at(&path, 42, NOW - 1), "时钟回拨不算");
    }

    /// 停机哨兵不在、也没有上一代的启动记录 = 第一次起 daemon,不报崩溃;有启动记录才是。
    #[test]
    fn first_start_is_not_reported_as_a_crash() {
        assert_eq!(previous_exit_log(false, false), None);
        assert_eq!(previous_exit_log(false, true), None);
        assert_eq!(
            previous_exit_log(true, true),
            Some("检测到上次为非正常退出(崩溃/被杀)")
        );
        assert!(previous_exit_log(true, false)
            .is_some_and(|line| line.contains("首次启动") && !line.contains("崩溃")));
    }

    const NOW: u64 = 1_800_000_000;
    /// 本次系统启动在一天前。
    const BOOTED: u64 = NOW - 24 * 3600;

    fn ctx() -> Context {
        Context {
            requested: false,
            restarted_by_unit: true,
            now_secs: NOW,
            system_boot_secs: Some(BOOTED),
        }
    }

    fn unclean(ready_ago: Option<u64>) -> Previous {
        Previous::Unclean(BootRecord {
            pid: 42,
            started_at_secs: NOW - 3600,
            ready_at_secs: ready_ago.map(|ago| NOW - ago),
        })
    }

    fn refused(verdict: BootVerdict) -> bool {
        matches!(verdict, BootVerdict::Refuse(_))
    }

    #[test]
    fn a_clean_previous_generation_always_proceeds() {
        assert_eq!(decide(Previous::Clean, ctx()), BootVerdict::Proceed);
    }

    #[test]
    fn a_crash_after_a_stable_run_is_restarted() {
        // 2026-09-28 的形状:跑了几天才崩,该自动拉起。
        let days_ago = NOW - 4 * 24 * 3600;
        let record = Previous::Unclean(BootRecord {
            pid: 42,
            started_at_secs: days_ago,
            ready_at_secs: Some(days_ago + 10),
        });
        let booted_long_ago = Context {
            system_boot_secs: Some(NOW - 9 * 24 * 3600),
            ..ctx()
        };
        assert_eq!(decide(record, booted_long_ago), BootVerdict::Proceed);
        assert_eq!(
            decide(unclean(Some(MIN_STABLE_UPTIME_SECS)), ctx()),
            BootVerdict::Proceed
        );
    }

    #[test]
    fn a_young_crash_is_not_restarted_automatically() {
        for previous in [
            unclean(Some(MIN_STABLE_UPTIME_SECS - 1)),
            unclean(Some(5)),
            unclean(None),
            Previous::Unreadable {
                written_at_secs: Some(NOW - 30),
            },
            Previous::Unreadable {
                written_at_secs: None,
            },
        ] {
            assert!(
                refused(decide(previous, ctx())),
                "{previous:?} 不该被自动拉起"
            );
        }
    }

    #[test]
    fn a_ready_time_in_the_future_counts_as_not_proven_stable() {
        let previous = Previous::Unclean(BootRecord {
            pid: 42,
            started_at_secs: NOW - 60,
            ready_at_secs: Some(NOW + 60),
        });
        assert!(refused(decide(previous, ctx())));
    }

    #[test]
    fn a_human_start_is_never_refused() {
        let human = Context {
            requested: true,
            ..ctx()
        };
        for previous in [unclean(None), unclean(Some(5))] {
            assert_eq!(decide(previous, human), BootVerdict::Proceed);
        }
    }

    #[test]
    fn nothing_is_refused_unless_the_autostart_unit_restarted_it() {
        // 没配崩溃拉起的机器、终端里直接 `daemon run`:都不存在拉起循环,熔断只剩误拦的风险。
        let unsupervised = Context {
            restarted_by_unit: false,
            ..ctx()
        };
        assert_eq!(decide(unclean(None), unsupervised), BootVerdict::Proceed);
    }

    #[test]
    fn a_generation_from_before_this_system_boot_never_blocks_autostart() {
        // 关机 / 登出时停机没走完(被 SIGKILL),重启后的开机自启面对的是关机前那一代,不是崩溃循环。
        let before_boot = Previous::Unclean(BootRecord {
            pid: 42,
            started_at_secs: BOOTED - 60,
            ready_at_secs: None,
        });
        assert_eq!(decide(before_boot, ctx()), BootVerdict::Proceed);
        let unreadable_before_boot = Previous::Unreadable {
            written_at_secs: Some(BOOTED - 60),
        };
        assert_eq!(decide(unreadable_before_boot, ctx()), BootVerdict::Proceed);
        // 取不到开机时刻就不据此放行。
        let unknown_boot = Context {
            system_boot_secs: None,
            ..ctx()
        };
        assert!(refused(decide(before_boot, unknown_boot)));
    }

    #[test]
    fn only_the_process_the_unit_itself_started_counts() {
        let label = summon_core::autostart::LAUNCHD_LABEL;
        assert!(launched_by_launchd(Some(label), 1));
        // 托管 daemon 起的 tmux 里手动敲的:变量继承下来了,父进程是 shell。
        assert!(!launched_by_launchd(Some(label), 4242));
        assert!(!launched_by_launchd(Some("com.summon.menubar"), 1));
        assert!(!launched_by_launchd(None, 1));

        let in_unit =
            "1:name=systemd:/user.slice/user-1001.slice/user@1001.service/summon-daemon.service\n";
        assert!(launched_by_systemd(Some("777"), 777, Some(in_unit)));
        // 变量从被 exec 的那个进程继承下来,pid 对不上。
        assert!(!launched_by_systemd(Some("777"), 888, Some(in_unit)));
        assert!(!launched_by_systemd(None, 777, Some(in_unit)));
        let ssh = "1:name=systemd:/user.slice/user-1001.slice/session-84758.scope\n";
        assert!(!launched_by_systemd(Some("777"), 777, Some(ssh)));
        assert!(!launched_by_systemd(Some("777"), 777, None));
    }

    #[test]
    fn only_the_units_own_cgroup_counts_as_launched_by_it() {
        let unit = "summon-daemon.service";
        let v1_in_unit = "1:name=systemd:/user.slice/user-1001.slice/user@1001.service/app.slice/summon-daemon.service\n";
        assert!(in_unit_cgroup(v1_in_unit, unit));
        let v2_in_unit =
            "0::/user.slice/user-1001.slice/user@1001.service/app.slice/summon-daemon.service\n";
        assert!(in_unit_cgroup(v2_in_unit, unit));
        // ssh 会话里手动起的:在 session scope 下。
        let ssh =
            "1:name=systemd:/user.slice/user-1001.slice/session-84758.scope\n10:pids:/user.slice\n";
        assert!(!in_unit_cgroup(ssh, unit));
        assert!(!in_unit_cgroup("0::/x/summon-daemon.service.d\n", unit));
    }

    #[test]
    fn the_refusal_names_the_way_out() {
        let BootVerdict::Refuse(reason) = decide(unclean(Some(30)), ctx()) else {
            panic!("应当拒绝");
        };
        assert!(reason.contains("pid 42"), "{reason}");
        assert!(reason.contains("summon daemon start"), "{reason}");
    }

    #[test]
    fn the_system_boot_time_is_readable_and_in_the_past() {
        let boot = system_boot_secs().expect("本机应能读到开机时刻");
        assert!(boot > 1_500_000_000 && boot <= unix_now_secs(), "{boot}");
    }

    struct Sandbox {
        _dir: tempfile::TempDir,
        files: GuardFiles,
    }

    fn sandbox() -> Sandbox {
        let dir = tempfile::tempdir().unwrap();
        let files = GuardFiles {
            boot: dir.path().join("daemon.boot"),
            request: dir.path().join("start-request"),
            refused: dir.path().join("daemon.refused"),
        };
        Sandbox { _dir: dir, files }
    }

    fn at(now_secs: u64) -> Context {
        Context { now_secs, ..ctx() }
    }

    fn record(sb: &Sandbox) -> BootRecord {
        read_record(&sb.files.boot).expect("应有启动记录")
    }

    #[test]
    fn a_first_boot_proceeds_and_leaves_its_record() {
        let sb = sandbox();
        assert_eq!(
            check_and_record_at(&sb.files, 7, at(NOW)),
            BootVerdict::Proceed
        );
        assert_eq!(
            record(&sb),
            BootRecord {
                pid: 7,
                started_at_secs: NOW,
                ready_at_secs: None
            }
        );
    }

    #[test]
    fn a_generation_that_died_before_ready_blocks_the_next_unrequested_boot() {
        // 启动期出错退出(或刚起就崩):记录在、没有就绪时刻。下一次自动拉起必须被拦下,拦下时不
        // 覆盖记录 —— 否则再下一次就看不出上一代没跑稳了;拦下的原因留给 doctor。
        let sb = sandbox();
        check_and_record_at(&sb.files, 7, at(NOW));
        assert!(refused(check_and_record_at(&sb.files, 8, at(NOW + 10))));
        assert_eq!(record(&sb).pid, 7, "被拦下的一代不写记录");
        let note = std::fs::read_to_string(&sb.files.refused).unwrap();
        assert!(note.contains("pid 7"), "{note}");
        // 人工启动放行后,拒绝说明随之清掉。
        write_start_request_at(&sb.files.request, 1, NOW + 15).unwrap();
        let human = Context {
            requested: take_start_request(&sb.files.request, NOW + 20),
            ..at(NOW + 20)
        };
        assert_eq!(
            check_and_record_at(&sb.files, 9, human),
            BootVerdict::Proceed
        );
        assert!(!sb.files.refused.exists());
    }

    #[test]
    fn readiness_is_kept_in_the_record_so_dying_mid_shutdown_still_counts_as_stable() {
        // 停机一开始就删 ready 哨兵;停机途中被 SIGKILL 的那一代,就绪时刻仍在启动记录里。
        let sb = sandbox();
        check_and_record_at(&sb.files, 7, at(NOW));
        mark_ready_at(&sb.files.boot, 7, NOW + 5);
        assert_eq!(record(&sb).ready_at_secs, Some(NOW + 5));
        let later = NOW + 5 + MIN_STABLE_UPTIME_SECS;
        assert_eq!(
            check_and_record_at(&sb.files, 8, at(later)),
            BootVerdict::Proceed
        );
    }

    #[test]
    fn marking_ready_ignores_another_generations_record() {
        let sb = sandbox();
        check_and_record_at(&sb.files, 7, at(NOW));
        mark_ready_at(&sb.files.boot, 99, NOW + 5);
        assert_eq!(record(&sb).ready_at_secs, None);
    }

    #[test]
    fn a_graceful_stop_clears_the_record_so_the_next_boot_is_clean() {
        let sb = sandbox();
        check_and_record_at(&sb.files, 7, at(NOW));
        clear_boot_record_at(&sb.files.boot, 7);
        assert!(!sb.files.boot.exists());
        assert_eq!(
            check_and_record_at(&sb.files, 8, at(NOW + 5)),
            BootVerdict::Proceed
        );
    }

    #[test]
    fn clearing_leaves_another_generations_record_alone() {
        let sb = sandbox();
        check_and_record_at(&sb.files, 7, at(NOW));
        clear_boot_record_at(&sb.files.boot, 99);
        assert!(sb.files.boot.exists());
    }

    #[test]
    fn a_stale_start_request_is_ignored_and_removed() {
        let sb = sandbox();
        write_start_request_at(&sb.files.request, 1, NOW - START_REQUEST_TTL_SECS - 1).unwrap();
        assert!(!take_start_request(&sb.files.request, NOW));
        assert!(!sb.files.request.exists());
    }

    #[test]
    fn a_fresh_start_request_counts_once() {
        let sb = sandbox();
        write_start_request_at(&sb.files.request, 1, NOW - 5).unwrap();
        assert!(take_start_request(&sb.files.request, NOW));
        assert!(!take_start_request(&sb.files.request, NOW), "读后即删");
    }

    #[test]
    fn an_unreadable_record_written_this_boot_blocks_unrequested_boots() {
        let sb = sandbox();
        std::fs::write(&sb.files.boot, "not json").unwrap();
        let now = unix_now_secs();
        let this_boot = Context {
            now_secs: now,
            system_boot_secs: Some(now - 3600),
            ..ctx()
        };
        assert!(refused(check_and_record_at(&sb.files, 8, this_boot)));
    }
}
