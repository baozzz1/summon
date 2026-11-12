//! 原始终端输出的状态快照及每轮等待提示。
use crate::runtime::fsio::atomic_write;
use serde::{Deserialize, Serialize};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use summon_core::parse::program_status::{Event, Kind, Report, State};

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Snapshot {
    generation: String,
    sequence: u64,
    report: Option<Report>,
}

/// 原始输出不保存；固定 runtime 文件仅存最后一个根记录。
pub(crate) fn wrap_launch(cmd: &str, path: &Path) -> anyhow::Result<String> {
    use summon_core::launch_env::sh_single_quote as quote;
    let generation = uuid::Uuid::new_v4().to_string();
    std::fs::create_dir_all(
        path.parent()
            .ok_or_else(|| anyhow::anyhow!(summon_i18n::format!("r034c170ca1")))?,
    )?;
    save(
        path,
        &Snapshot {
            generation: generation.clone(),
            sequence: 0,
            report: None,
        },
    )?;
    let binary = std::env::current_exe()?;
    // libtest 可用显式构建的 CLI 执行子命令；测试二进制本身只识别测试参数。
    #[cfg(test)]
    let binary = std::env::var_os("SUMMON_TEST_BIN")
        .map(PathBuf::from)
        .unwrap_or(binary);
    Ok(format!(
        "{} program-status-run {} {} {}",
        quote(&binary.to_string_lossy()),
        quote(&path.to_string_lossy()),
        quote(&generation),
        quote(cmd)
    ))
}

fn save(path: &Path, snapshot: &Snapshot) -> anyhow::Result<()> {
    atomic_write(path, &serde_json::to_vec(snapshot)?)?;
    Ok(())
}
fn load(path: &Path) -> Option<Snapshot> {
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .ok()?
        .take(4097)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() > 4096 {
        return None;
    }
    serde_json::from_slice(&bytes).ok()
}

/// 此刻停在授权确认 / 提问 / 登录上的那一类(控制台的会话页用,判据同 [`Watch::poll`]);
/// 不是 claude 的报告、或没停住为 `None`。
pub(crate) fn current_blocked(path: &Path) -> Option<Kind> {
    load(path)?
        .report
        .filter(|r| {
            r.id.is_none() && r.app.as_deref() == Some("claude-code") && r.state == State::Blocked
        })
        .and_then(|r| r.kind)
}

/// 会话包装进程(tmux 里托着 CLI 的这一层)在 ps 里的名字。进程按功能命名:daemon 是 `summond`,
/// sidecar 是 `summon-sidecar`。
pub(crate) const SESSION_PROCESS_NAME: &str = "summon-session";

/// 改过名的那一代进程靠它认出自己,不再 exec;认出后即摘掉,不传给 CLI 子进程。
const RENAMED_MARKER: &str = "SUMMON_SESSION_RENAMED";

/// 这一层由 tmux 经 shell 拉起,启动方设不了 argv[0]。进来先以 `summon-session` 为 argv[0] 原地
/// exec 自己一次,pid 不变,tmux 与子进程都无感。exec 失败就以原名接着跑:名字只影响 ps 显示,
/// 不能因此起不来会话。
pub(crate) fn adopt_session_process_name() {
    use std::os::unix::process::CommandExt as _;
    if std::env::var_os(RENAMED_MARKER).is_some() {
        std::env::remove_var(RENAMED_MARKER);
        return;
    }
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let _ = std::process::Command::new(exe)
        .arg0(SESSION_PROCESS_NAME)
        .args(std::env::args_os().skip(1))
        .env(RENAMED_MARKER, "1")
        .exec();
}

/// 采集器与 CLI 共用生命周期，原子快照写入失败时只停止观测。
pub(crate) fn run(path: &Path, generation: &str, command: &str) -> anyhow::Result<i32> {
    let mut snapshot = Snapshot {
        generation: generation.to_string(),
        sequence: 0,
        report: None,
    };
    let mut enabled = save(path, &snapshot).is_ok();
    let mut dirty = false;
    let mut last_write = Instant::now() - Duration::from_secs(1);
    let result = super::program_status_pty::run(command, |event| {
        if !enabled {
            return;
        }
        if let Some(event) = event {
            let next = match event {
                Event::Query => return,
                Event::Reset => None,
                Event::ShellPrompt => {
                    if snapshot
                        .report
                        .as_ref()
                        .is_some_and(|r| matches!(r.state, State::Done | State::Error))
                    {
                        return;
                    }
                    None
                }
                Event::Report(report) => {
                    if report.id.is_some() {
                        return;
                    }
                    if report.state == State::Clear {
                        None
                    } else {
                        Some(report)
                    }
                }
            };
            if snapshot.report != next {
                snapshot.report = next;
                snapshot.sequence += 1;
                dirty = true;
            }
        }
        if dirty && last_write.elapsed() >= Duration::from_millis(100) {
            enabled = save(path, &snapshot).is_ok();
            dirty = false;
            last_write = Instant::now();
        }
    });
    if enabled {
        if snapshot
            .report
            .as_ref()
            .is_some_and(|r| !matches!(r.state, State::Done | State::Error))
        {
            snapshot.report = None;
            snapshot.sequence += 1;
        }
        let _ = save(path, &snapshot);
    }
    result
}

/// 每轮固定 generation 和序号基线，旧状态与其它代采集器不能进入这一轮。
pub(crate) struct Watch {
    path: PathBuf,
    baseline: Option<Snapshot>,
    blocked: Option<Option<Kind>>,
}
impl Watch {
    pub(crate) fn new(path: PathBuf) -> Self {
        let baseline = load(&path);
        Self {
            path,
            baseline,
            blocked: None,
        }
    }
    pub(crate) fn poll(&mut self) -> Option<&'static str> {
        let current = load(&self.path);
        match (&mut self.baseline, current) {
            (Some(base), Some(next))
                if base.generation == next.generation && next.sequence >= base.sequence =>
            {
                if next.sequence > base.sequence {
                    self.blocked = next
                        .report
                        .as_ref()
                        .filter(|r| {
                            r.id.is_none()
                                && r.app.as_deref() == Some("claude-code")
                                && r.state == State::Blocked
                        })
                        .map(|r| r.kind);
                    *base = next;
                }
            }
            _ => {
                self.blocked = None;
                self.baseline = None;
            }
        }
        self.blocked.map(|k| {
            k.map(Kind::label)
                .unwrap_or(summon_i18n::text!("ra382fd1a53"))
        })
    }
}

pub(crate) fn probe_due(idle: Duration, since_probe: Option<Duration>, blocked: bool) -> bool {
    (blocked || idle >= Duration::from_secs(4))
        && since_probe.is_none_or(|age| age >= Duration::from_secs(2))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 改过名的那一代只摘标记、不再 exec:改名最多发生一次,标记也到不了 CLI 子进程。
    #[test]
    fn the_renamed_generation_drops_the_marker_without_exec() {
        std::env::set_var(RENAMED_MARKER, "1");
        adopt_session_process_name();
        assert!(std::env::var_os(RENAMED_MARKER).is_none());
    }

    /// 改名必须在包装开始干活之前:CLI 入口先改名、再进 `run`。
    #[test]
    fn the_cli_renames_the_wrapper_before_running_it() {
        let cli: String = include_str!("../cli/cli.rs")
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
        assert!(cli.contains(
            "crate::capture::program_status::adopt_session_process_name();letcode=crate::capture::program_status::run(path,generation,command)?;"
        ));
    }

    fn write(path: &Path, generation: &str, sequence: u64, state: State) {
        save(
            path,
            &Snapshot {
                generation: generation.into(),
                sequence,
                report: Some(Report {
                    state,
                    app: Some("claude-code".into()),
                    id: None,
                    kind: Some(Kind::Question),
                }),
            },
        )
        .unwrap();
    }
    #[test]
    fn program_status_watch_fences_old_turn_generation_and_corruption() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("status");
        write(&path, "a", 1, State::Blocked);
        let mut watch = Watch::new(path.clone());
        assert_eq!(watch.poll(), None);
        write(&path, "a", 2, State::Blocked);
        assert!(watch.poll().is_some());
        write(&path, "a", 3, State::Working);
        assert_eq!(watch.poll(), None);
        write(&path, "b", 4, State::Blocked);
        assert_eq!(watch.poll(), None);
        write(&path, "a", 5, State::Blocked);
        assert_eq!(watch.poll(), None);
        let mut watch = Watch::new(path.clone());
        std::fs::write(&path, "{").unwrap();
        assert_eq!(watch.poll(), None);
        write(&path, "a", 6, State::Blocked);
        assert_eq!(watch.poll(), None);
    }
    #[test]
    fn program_status_probe_comparison_and_throttle() {
        let poll = Duration::from_millis(300);
        let old = (1..20).find(|n| probe_due(poll * *n, None, false)).unwrap();
        let new = (1..20).find(|n| probe_due(poll * *n, None, true)).unwrap();
        assert_eq!((old, new), (14, 1));
        assert!(!probe_due(
            Duration::ZERO,
            Some(Duration::from_millis(1999)),
            true
        ));
        assert!(probe_due(
            Duration::ZERO,
            Some(Duration::from_secs(2)),
            true
        ));
        for n in 0..30 {
            assert_eq!(
                probe_due(poll * n, None, false),
                poll * n >= Duration::from_secs(4)
            );
        }
    }
    #[test]
    fn program_status_never_promotes_completion_or_foreign_app() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("status");
        write(&path, "a", 0, State::Idle);
        let mut watch = Watch::new(path.clone());
        for (i, state) in [
            State::Done,
            State::Error,
            State::Working,
            State::Idle,
            State::Clear,
        ]
        .into_iter()
        .enumerate()
        {
            write(&path, "a", i as u64 + 1, state);
            assert_eq!(watch.poll(), None);
        }
        let mut s = load(&path).unwrap();
        s.sequence += 1;
        s.report.as_mut().unwrap().state = State::Blocked;
        s.report.as_mut().unwrap().app = Some("cargo".into());
        save(&path, &s).unwrap();
        assert_eq!(watch.poll(), None);
    }
}
