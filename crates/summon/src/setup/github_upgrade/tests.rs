use super::*;
use crate::test_support::write_test_executable;
use std::collections::BTreeMap;
use std::io::BufRead;

pub(crate) const PLATFORM: &str = "linux-x64";

pub(crate) fn build(version: &str, commit: &str, commit_time: u64) -> BuildInfo {
    BuildInfo {
        version: version.to_string(),
        commit: commit.to_string(),
        commit_time: Some(commit_time),
    }
}

/// Release 里的新版;假 summon 默认就报它。
pub(crate) fn new_build() -> BuildInfo {
    build("0.7.0", "abcdef12", 2_000)
}

pub(crate) fn old_build() -> BuildInfo {
    build("0.6.9", "1234567a", 1_000)
}

/// 假 summon,包里与落点里是同一份。它按自己所在的位置认出家目录(落点 `<家>/.summon/bin`,
/// 暂存 `<家>/.summon/run/<暂存>/pkg`),行为由家目录里的文件决定,所以所有用例共用这一份内容:
/// `install-stamp print` 在 `stamp-calls.log` 记一笔再打 `stamp.json`(`stamp-mode` 为 fail / hang 时
/// 失败 / 卡住);`--version`
/// 打 `version-line`;`daemon restart` 记账,`restart-fails` 在时失败,否则暂存那份像新版那样把
/// 自己与 sidecar 换进落点(暂存 + rename;落点那份不换;换之前把落点的 sidecar 抄进
/// `sidecar-at-restart`),daemon 报 `new-version`;`daemon status` 报 `daemon-version`;
/// 其余调用记账。记账带调用者的进程组与免脱离标记。`# team=` 那一行给假 codesign 认团队。
pub(crate) const FAKE_SUMMON: &str = r#"#!/bin/sh
# team=TEAMA
here="$(cd "$(dirname "$0")" && pwd)"
case "$here" in
  */.summon/bin) home="$(cd "$here/../.." && pwd)"; role=install ;;
  *) home="$(cd "$here/../../../.." && pwd)"; role=stage ;;
esac
note() { echo "$role $* pgid=$(ps -o pgid= -p $$ | tr -d ' ') no_detach=${SUMMON_RESTART_NO_DETACH:-}" >> "$home/calls.log"; }
case "$1 ${2:-}" in
  "install-stamp print")
    echo "$role" >> "$home/stamp-calls.log"
    case "$(cat "$home/stamp-mode" 2>/dev/null)" in
      fail) echo "假 install-stamp 坏了" >&2; exit 3 ;;
      hang) exec sleep 30 ;;
    esac
    cat "$home/stamp.json" ;;
  "--version ") echo "summon $(cat "$home/version-line")" ;;
  "daemon restart")
    note "$@"
    if [ -e "$home/restart-fails" ]; then echo "假 restart 失败"; exit 1; fi
    if [ "$role" = stage ]; then
      cp "$home/.summon/bin/summon-sidecar" "$home/sidecar-at-restart"
      for f in summon summon-sidecar; do
        cp "$here/$f" "$home/.summon/bin/.$f.new" && mv -f "$home/.summon/bin/.$f.new" "$home/.summon/bin/$f"
      done
    fi
    cat "$home/new-version" > "$home/daemon-version"
    echo "假 restart 完成" ;;
  "daemon status")
    if [ -e "$home/daemon-down" ]; then echo "daemon 未运行"; exit 0; fi
    echo "running(pid 4242, version $(cat "$home/daemon-version"))" ;;
  *) note "$@"; echo "假 $1 完成" ;;
esac
"#;

/// 假 codesign:按文件里 `# team=` 那一行报团队,有 `# unsigned` 的验签失败。
pub(crate) const FAKE_CODESIGN: &str = r#"#!/bin/sh
eval "path=\${$#}"
team="$(sed -n 's/^# team=//p' "$path" | head -n 1)"
case "$1" in
  --verify)
    if grep -q '^# unsigned' "$path"; then echo "$path: code object is not signed at all" >&2; exit 1; fi ;;
  -dv)
    if [ -n "$team" ]; then echo "TeamIdentifier=$team" >&2; else echo "TeamIdentifier=not set" >&2; fi ;;
esac
"#;

/// tar 成员:名字原样写进头部(要能造出带路径的坏包),`link` 时是指向 /bin/sh 的软链。
pub(crate) struct Member<'a> {
    name: &'a str,
    body: &'a [u8],
    link: bool,
}

pub(crate) fn member<'a>(name: &'a str, body: &'a [u8]) -> Member<'a> {
    Member {
        name,
        body,
        link: false,
    }
}

pub(crate) fn tarball(members: &[Member<'_>]) -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    for member in members {
        let mut header = tar::Header::new_ustar();
        let name = member.name.as_bytes();
        header.as_ustar_mut().unwrap().name[..name.len()].copy_from_slice(name);
        header.set_mode(0o644);
        let body: &[u8] = if member.link {
            header.set_entry_type(tar::EntryType::Symlink);
            header.set_link_name("/bin/sh").unwrap();
            &[]
        } else {
            header.set_entry_type(tar::EntryType::Regular);
            member.body
        };
        header.set_size(body.len() as u64);
        header.set_cksum();
        builder.append(&header, body).unwrap();
    }
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    gz.write_all(&builder.into_inner().unwrap()).unwrap();
    gz.finish().unwrap()
}

pub(crate) fn good_package() -> Vec<u8> {
    tarball(&[
        member("summon", FAKE_SUMMON.as_bytes()),
        member("summon-sidecar", b"new sidecar"),
    ])
}

pub(crate) fn sha256(bytes: &[u8]) -> String {
    ring::digest::digest(&ring::digest::SHA256, bytes)
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

pub(crate) fn manifest(build: &BuildInfo, assets: &[(&str, usize, String)]) -> String {
    let assets: serde_json::Map<String, serde_json::Value> = assets
        .iter()
        .map(|(platform, size, sha)| {
            let asset = serde_json::json!({
                "name": format!("summon-{platform}.tar.gz"),
                "size": size,
                "sha256": sha,
            });
            (platform.to_string(), asset)
        })
        .collect();
    serde_json::json!({
        "version": build.version,
        "commit": build.commit,
        "commit_time": build.commit_time,
        "published_at": "2026-01-02T03:04:05Z",
        "assets": assets,
    })
    .to_string()
}

pub(crate) struct Fixture {
    _tmp: tempfile::TempDir,
    pub(crate) root: PathBuf,
    pub(crate) home: PathBuf,
    /// 本地目录形式的 Release。
    pub(crate) release: PathBuf,
}

impl Fixture {
    /// 落点里是旧版(带构建戳),Release 目录里是新版的 linux-x64 包与 manifest。
    pub(crate) fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let home = root.join("home");
        let install = companion::install_dir(&home);
        std::fs::create_dir_all(&install).unwrap();
        write_test_executable(&install.join("summon"), FAKE_SUMMON);
        std::fs::write(install.join("summon-sidecar"), "old sidecar").unwrap();
        let target = companion::install_target(&home);
        self_install::record_from(&target.canonicalize().unwrap(), &home, &old_build()).unwrap();
        std::fs::write(home.join("daemon-version"), "0.6.9").unwrap();
        let fixture = Self {
            _tmp: tmp,
            release: root.join("release"),
            root,
            home,
        };
        fixture.reports(&new_build());
        std::fs::write(fixture.home.join("new-version"), "0.7.0").unwrap();
        std::fs::create_dir_all(&fixture.release).unwrap();
        fixture.publish_release(&new_build(), &good_package());
        fixture
    }

    /// 包里的 summon 自报 `build`。
    pub(crate) fn reports(&self, build: &BuildInfo) {
        std::fs::write(
            self.home.join("stamp.json"),
            serde_json::to_string(build).unwrap(),
        )
        .unwrap();
        std::fs::write(
            self.home.join("version-line"),
            format!("{} ({})", build.version, build.commit),
        )
        .unwrap();
    }

    /// Release 目录里放 `package` 作 linux-x64 的包,manifest 照实记它。
    pub(crate) fn publish_release(&self, build: &BuildInfo, package: &[u8]) {
        std::fs::write(self.release.join("summon-linux-x64.tar.gz"), package).unwrap();
        self.write_manifest(&manifest(
            build,
            &[(PLATFORM, package.len(), sha256(package))],
        ));
    }

    pub(crate) fn write_manifest(&self, text: &str) {
        std::fs::write(self.release.join("manifest.json"), text).unwrap();
    }

    fn host(&self, daemon: Daemon) -> Host {
        self.host_seeing(vec![daemon])
    }

    /// daemon 的状态依次是 `states`,取完之后停在最后一个:下载期间 daemon 被起来或停掉。
    fn host_seeing(&self, states: Vec<Daemon>) -> Host {
        let calls = std::sync::atomic::AtomicUsize::new(0);
        Host {
            home: self.home.clone(),
            platform: Some(PLATFORM),
            daemon: Box::new(move || {
                let at = calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                states[at.min(states.len() - 1)].clone()
            }),
            menubar_installed: false,
            restore_delay_secs: 0,
            stamp_timeout: Duration::from_secs(30),
            codesign: None,
        }
    }

    fn args(&self) -> UpgradeArgs {
        UpgradeArgs {
            from: Some(self.release.display().to_string()),
            ..UpgradeArgs::default()
        }
    }

    /// 落点目录里每个文件的内容。
    pub(crate) fn install_snapshot(&self) -> BTreeMap<String, Vec<u8>> {
        std::fs::read_dir(companion::install_dir(&self.home))
            .unwrap()
            .map(|entry| {
                let entry = entry.unwrap();
                (
                    entry.file_name().to_string_lossy().into_owned(),
                    std::fs::read(entry.path()).unwrap(),
                )
            })
            .collect()
    }

    pub(crate) fn stages(&self) -> Vec<PathBuf> {
        let Ok(entries) = std::fs::read_dir(self.home.join(".summon/run")) else {
            return Vec::new();
        };
        entries
            .flatten()
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(STAGE_PREFIX)
            })
            .map(|entry| entry.path())
            .collect()
    }

    pub(crate) fn calls(&self) -> String {
        std::fs::read_to_string(self.home.join("calls.log")).unwrap_or_default()
    }

    /// 暂存里的 summon 被执行过几次(`install-stamp print`,一次一行)。
    pub(crate) fn stamp_calls(&self) -> String {
        std::fs::read_to_string(self.home.join("stamp-calls.log")).unwrap_or_default()
    }

    pub(crate) fn read_install(&self, name: &str) -> Vec<u8> {
        std::fs::read(companion::install_dir(&self.home).join(name)).unwrap()
    }
}

fn run(host: &Host, args: &UpgradeArgs) -> (Result<()>, String) {
    let mut out = Vec::new();
    let result = upgrade(host, args, &mut out);
    (result, String::from_utf8(out).unwrap())
}

fn stopped() -> Daemon {
    Daemon::Stopped
}

fn running() -> Daemon {
    Daemon::Running(Some("0.6.9".to_string()))
}

/// 失败的升级:报错(退出码非零)、落点逐字节没变、暂存已清理、daemon 没被碰过。
fn assert_refused(fixture: &Fixture, host: &Host, args: &UpgradeArgs, want: &str) -> String {
    let before = fixture.install_snapshot();
    let (result, out) = run(host, args);
    let error = format!("{:#}", result.expect_err(want));
    assert!(error.contains(want), "要说清 {want}:{error}\n{out}");
    assert_eq!(fixture.install_snapshot(), before, "落点被改了:{error}");
    assert!(
        fixture.stages().is_empty(),
        "暂存没清:{:?}",
        fixture.stages()
    );
    assert!(
        !fixture.calls().contains("daemon"),
        "daemon 被碰了:{}",
        fixture.calls()
    );
    error
}

#[test]
fn a_newer_release_replaces_the_install_without_starting_a_stopped_daemon() {
    let fixture = Fixture::new();

    let (result, out) = run(&fixture.host(stopped()), &fixture.args());

    result.unwrap();
    assert_eq!(fixture.read_install("summon"), FAKE_SUMMON.as_bytes());
    assert_eq!(fixture.read_install("summon-sidecar"), b"new sidecar");
    assert_eq!(
        self_install::installed_build(&fixture.home),
        Installed::Known(new_build())
    );
    assert_eq!(fixture.calls(), "", "daemon 没在跑就不启动它");
    assert!(fixture.stages().is_empty(), "{:?}", fixture.stages());
    assert!(out.contains("未启动它"), "{out}");
    assert!(out.contains("核对通过"), "{out}");
    let mode = std::fs::metadata(companion::install_target(&fixture.home))
        .unwrap()
        .permissions();
    assert_eq!(
        std::os::unix::fs::PermissionsExt::mode(&mode) & 0o777,
        0o755
    );
}

#[test]
fn a_running_daemon_is_restarted_by_the_new_build_outside_this_session() {
    let fixture = Fixture::new();

    let (result, out) = run(&fixture.host(running()), &fixture.args());

    result.unwrap();
    let calls = fixture.calls();
    let (pgid, no_detach) = calls
        .strip_prefix("stage daemon restart pgid=")
        .and_then(|rest| rest.trim().split_once(" no_detach="))
        .and_then(|(pgid, marker)| Some((pgid.parse::<i32>().ok()?, marker)))
        .unwrap_or_else(|| panic!("没用暂存那份重启:{calls:?}"));
    assert_ne!(
        pgid,
        nix::unistd::getpgrp().as_raw(),
        "重启那段必须脱离当前进程组"
    );
    assert_eq!(
        no_detach, "1",
        "重启段已在独立会话里,daemon restart 应带免脱离标记:{calls:?}"
    );
    assert_eq!(fixture.read_install("summon"), FAKE_SUMMON.as_bytes());
    assert_eq!(fixture.read_install("summon-sidecar"), b"new sidecar");
    assert!(
        out.contains("running(pid 4242, version 0.7.0)"),
        "跟随重启段的日志:{out}"
    );
    assert!(out.contains("tmux"), "{out}");
    assert!(fixture.stages().is_empty(), "{:?}", fixture.stages());
    upgrade_restart::lock(&fixture.home).expect("重启段结束后升级锁应已释放");
}

/// 重启段跑完了新版却没进落点:daemon 可能退回用暂存那份起了,暂存留着、标上记号,报错;下一次
/// 升级在 daemon 没在跑时才清掉它。
#[test]
fn a_restart_that_leaves_the_old_install_keeps_the_stage() {
    let fixture = Fixture::new();
    std::fs::write(fixture.home.join("restart-fails"), "").unwrap();

    let (result, _) = run(&fixture.host(running()), &fixture.args());

    let error = format!("{:#}", result.unwrap_err());
    assert!(error.contains("没能换进落点"), "{error}");
    let stages = fixture.stages();
    assert_eq!(stages.len(), 1, "{error}");
    assert!(error.contains(&stages[0].display().to_string()), "{error}");
    assert!(stages[0].join(KEEP_MARKER).exists());
    assert_eq!(fixture.read_install("summon-sidecar"), b"old sidecar");

    // daemon 在跑、它与落点不同(新版没换进去)时不清它;daemon 不在跑时清。夹具里落点与暂存是同一份
    // 假 summon,先把暂存那份改成另一版。
    std::fs::write(stages[0].join("pkg/summon"), "never landed").unwrap();
    Stage::create(&fixture.home, &running()).unwrap();
    assert_eq!(fixture.stages().len(), 1);
    Stage::create(&fixture.home, &stopped()).unwrap();
    assert!(fixture.stages().is_empty());
}

/// 上一次换代前留下的、带保留标记的暂存。
fn kept_stage(fixture: &Fixture, summon: Option<&[u8]>) -> PathBuf {
    let stage = fixture.home.join(".summon/run").join("upgrade-stage.prev");
    std::fs::create_dir_all(stage.join("pkg")).unwrap();
    std::fs::write(stage.join(KEEP_MARKER), "").unwrap();
    if let Some(summon) = summon {
        std::fs::write(stage.join("pkg/summon"), summon).unwrap();
    }
    stage
}

/// 发起者随停机被打断、没来得及删暂存,而停机后的发布成功了:暂存的 summon 与落点逐字节相同,daemon 在跑
/// 时下一次升级照样清掉它。
#[test]
fn a_kept_stage_that_landed_is_swept_while_the_daemon_runs() {
    let fixture = Fixture::new();
    let stage = kept_stage(&fixture, Some(FAKE_SUMMON.as_bytes()));

    let (result, _) = run(&fixture.host(running()), &fixture.args());

    result.unwrap();
    assert!(!stage.exists(), "已换进落点的暂存没清");
    assert!(fixture.stages().is_empty());
}

/// daemon 在跑时,与落点不同(daemon 可能正从它运行)或比不了的带标记暂存都留着。
#[test]
fn a_kept_stage_that_may_host_the_daemon_is_left_alone() {
    // 内容不同。
    let fixture = Fixture::new();
    let stage = kept_stage(&fixture, Some(b"a build that never landed"));
    Stage::create(&fixture.home, &running()).unwrap();
    assert!(stage.join("pkg/summon").exists());

    // 长度相同、内容不同。
    let fixture = Fixture::new();
    let mut same_len = FAKE_SUMMON.as_bytes().to_vec();
    *same_len.last_mut().unwrap() ^= 1;
    let stage = kept_stage(&fixture, Some(&same_len));
    Stage::create(&fixture.home, &running()).unwrap();
    assert!(stage.join("pkg/summon").exists());

    // 暂存里没有 summon。
    let fixture = Fixture::new();
    let stage = kept_stage(&fixture, None);
    Stage::create(&fixture.home, &running()).unwrap();
    assert!(stage.exists());

    // 落点里没有 summon。
    let fixture = Fixture::new();
    let stage = kept_stage(&fixture, Some(FAKE_SUMMON.as_bytes()));
    std::fs::remove_file(companion::install_target(&fixture.home)).unwrap();
    Stage::create(&fixture.home, &running()).unwrap();
    assert!(stage.join("pkg/summon").exists());

    // daemon 不在跑时都清。
    Stage::create(&fixture.home, &stopped()).unwrap();
    assert!(!stage.exists());
}

/// 下载期间 daemon 被起来了:按准备完成那一刻的状态走重启,当前进程不换落点 —— 落点直到暂存那份的
/// `daemon restart` 都是旧的。
#[test]
fn a_daemon_started_during_the_download_is_restarted_not_swapped_under() {
    let fixture = Fixture::new();
    let host = fixture.host_seeing(vec![stopped(), running()]);

    let (result, _) = run(&host, &fixture.args());

    result.unwrap();
    assert!(
        fixture.calls().starts_with("stage daemon restart"),
        "{}",
        fixture.calls()
    );
    assert_eq!(
        std::fs::read(fixture.home.join("sidecar-at-restart")).unwrap(),
        b"old sidecar",
        "暂存那份重启之前,落点已经被当前进程换掉了"
    );
    assert_eq!(fixture.read_install("summon-sidecar"), b"new sidecar");
    assert!(fixture.stages().is_empty());
}

/// 下载期间 daemon 停了:只换落点,不把它起来。
#[test]
fn a_daemon_stopped_during_the_download_is_not_started() {
    let fixture = Fixture::new();
    let host = fixture.host_seeing(vec![running(), stopped()]);

    let (result, out) = run(&host, &fixture.args());

    result.unwrap();
    assert_eq!(fixture.calls(), "", "daemon 没在跑就不启动它");
    assert_eq!(fixture.read_install("summon-sidecar"), b"new sidecar");
    assert!(out.contains("未启动它"), "{out}");
}

/// 重启之前暂存就已标成保留:重启段起不来(发起者随停机被打断时同样来不及清理)时暂存留着、带
/// 标记,下一次升级在 daemon 在跑时不删它。
#[test]
fn the_stage_is_marked_kept_before_the_restart() {
    let fixture = Fixture::new();
    // 日志位置被目录占住:重启段起不来,restart_detached 直接返回错误。
    std::fs::create_dir_all(upgrade_restart::log_path(&fixture.home)).unwrap();

    let (result, _) = run(&fixture.host(running()), &fixture.args());

    assert!(result.is_err());
    let stages = fixture.stages();
    assert_eq!(stages.len(), 1, "{stages:?}");
    assert!(stages[0].join(KEEP_MARKER).exists());
    assert!(stages[0].join("pkg").join("summon").exists());
    assert_eq!(fixture.read_install("summon-sidecar"), b"old sidecar");
}

/// 取到并核对过包之后才把这一版从自动升级的退回名单里拿掉,下载或校验没过时名单不动。
#[test]
fn the_rollback_list_is_lifted_only_after_the_package_checks_out() {
    let fixture = Fixture::new();
    let store = crate::setup::auto_upgrade::Store::at_home(&fixture.home);
    store
        .update(|state| state.rolled_back.push("0.7.0".into()))
        .unwrap();
    let package = good_package();
    fixture.write_manifest(&manifest(
        &new_build(),
        &[(PLATFORM, package.len(), sha256(b"another package"))],
    ));
    assert_refused(
        &fixture,
        &fixture.host(stopped()),
        &fixture.args(),
        "SHA-256",
    );
    assert_eq!(store.load().unwrap().rolled_back, ["0.7.0"]);

    fixture.publish_release(&new_build(), &package);
    run(&fixture.host(stopped()), &fixture.args()).0.unwrap();
    assert!(store.load().unwrap().rolled_back.is_empty());
    assert_eq!(fixture.read_install("summon-sidecar"), b"new sidecar");
}

/// 之前中途退出(比如下载时被 Ctrl-C)留下的暂存在下一次升级时清掉。
#[test]
fn leftover_stages_are_swept_by_the_next_upgrade() {
    let fixture = Fixture::new();
    let leftover = fixture.home.join(".summon/run").join("upgrade-stage.left");
    std::fs::create_dir_all(leftover.join("pkg")).unwrap();
    std::fs::write(leftover.join("pkg/summon"), "half").unwrap();

    run(&fixture.host(stopped()), &fixture.args()).0.unwrap();

    assert!(!leftover.exists());
}

/// 落点已是 Release 的这一版:不下载(包不在也成功)。
fn up_to_date_fixture() -> Fixture {
    let fixture = Fixture::new();
    let target = companion::install_target(&fixture.home);
    self_install::record_from(&target, &fixture.home, &new_build()).unwrap();
    std::fs::remove_file(fixture.release.join("summon-linux-x64.tar.gz")).unwrap();
    fixture
}

/// daemon 不在跑、或报的就是这一版:什么都不做,一次 daemon 调用都没有。
#[test]
fn an_up_to_date_install_with_a_current_daemon_is_left_alone() {
    for daemon in [stopped(), Daemon::Running(Some("0.7.0".into()))] {
        let fixture = up_to_date_fixture();
        let before = fixture.install_snapshot();

        let (result, out) = run(&fixture.host(daemon.clone()), &fixture.args());

        result.unwrap();
        assert!(out.contains("已是 Release 的这一版"), "{out}");
        assert_eq!(fixture.install_snapshot(), before);
        assert!(fixture.stages().is_empty());
        assert_eq!(fixture.calls(), "", "{daemon:?}");
    }
}

/// 落点已是这一版而 daemon 还在跑别的版本(或报不出版本):不下载,用落点那份走同一段脱离会话的
/// 重启。
#[test]
fn an_up_to_date_install_restarts_a_stale_daemon_from_the_install() {
    for daemon in [running(), Daemon::Running(None)] {
        let fixture = up_to_date_fixture();
        let before = fixture.install_snapshot();

        let (result, out) = run(&fixture.host(daemon.clone()), &fixture.args());

        result.unwrap();
        let calls = fixture.calls();
        let (pgid, no_detach) = calls
            .strip_prefix("install daemon restart pgid=")
            .and_then(|rest| rest.trim().split_once(" no_detach="))
            .and_then(|(pgid, marker)| Some((pgid.parse::<i32>().ok()?, marker)))
            .unwrap_or_else(|| panic!("{daemon:?}:没用落点那份重启:{calls:?}"));
        assert_ne!(pgid, nix::unistd::getpgrp().as_raw(), "重启段必须脱离会话");
        assert_eq!(no_detach, "1", "{calls:?}");
        assert!(out.contains("running(pid 4242, version 0.7.0)"), "{out}");
        assert_eq!(fixture.install_snapshot(), before, "不该动落点");
        assert!(fixture.stages().is_empty(), "不该下载");
        upgrade_restart::lock(&fixture.home).expect("重启段结束后升级锁应已释放");
    }

    // --check 只报告。
    let fixture = up_to_date_fixture();
    let args = UpgradeArgs {
        check: true,
        ..fixture.args()
    };
    let (result, out) = run(&fixture.host(running()), &args);
    result.unwrap();
    assert!(out.contains("在跑的 daemon 仍是 0.6.9"), "{out}");
    assert_eq!(fixture.calls(), "");
}

/// 不降级:指定的版本比落点旧就拒绝;没指定而 Release 比落点旧(落点来自更新的源码构建)只说明、不替换。
#[test]
fn an_older_release_never_replaces_the_install() {
    let fixture = Fixture::new();
    let older = build("0.6.5", "fedcba98", 500);
    fixture.reports(&older);
    fixture.publish_release(&older, &good_package());
    let host = fixture.host(stopped());

    let args = UpgradeArgs {
        version: Some("0.6.5".into()),
        ..fixture.args()
    };
    let error = assert_refused(&fixture, &host, &args, "不降级");
    assert!(error.contains("0.6.5"), "{error}");

    let before = fixture.install_snapshot();
    let (result, out) = run(&host, &fixture.args());
    result.unwrap();
    assert!(out.contains("不替换"), "{out}");
    assert_eq!(fixture.install_snapshot(), before);
    assert!(fixture.stages().is_empty());
}

#[test]
fn a_release_that_is_not_the_requested_version_is_refused() {
    let fixture = Fixture::new();
    let args = UpgradeArgs {
        version: Some("0.7.1".into()),
        ..fixture.args()
    };
    let error = assert_refused(
        &fixture,
        &fixture.host(stopped()),
        &args,
        "不是要求的 0.7.1",
    );
    assert!(error.contains("未改动"), "{error}");
}

#[test]
fn a_package_of_the_wrong_size_or_hash_is_refused() {
    let fixture = Fixture::new();
    let package = good_package();
    let host = fixture.host(running());

    fixture.write_manifest(&manifest(
        &new_build(),
        &[(PLATFORM, package.len() + 1, sha256(&package))],
    ));
    assert_refused(&fixture, &host, &fixture.args(), "字节数");

    fixture.write_manifest(&manifest(
        &new_build(),
        &[(PLATFORM, package.len() - 1, sha256(&package))],
    ));
    assert_refused(&fixture, &host, &fixture.args(), "超过");

    fixture.write_manifest(&manifest(
        &new_build(),
        &[(PLATFORM, package.len(), sha256(b"another package"))],
    ));
    assert_refused(&fixture, &host, &fixture.args(), "SHA-256");
}

#[test]
fn a_package_whose_stamp_differs_from_the_manifest_is_refused() {
    let fixture = Fixture::new();
    fixture.reports(&build("0.7.0", "bbbbbbb1", 2_000));
    assert_refused(
        &fixture,
        &fixture.host(running()),
        &fixture.args(),
        "构建戳",
    );
}

#[test]
fn a_release_without_this_platform_is_refused() {
    let fixture = Fixture::new();
    let package = good_package();
    fixture.write_manifest(&manifest(
        &new_build(),
        &[("linux-arm64", package.len(), sha256(&package))],
    ));
    let error = assert_refused(
        &fixture,
        &fixture.host(running()),
        &fixture.args(),
        "没有本平台(linux-x64)的包",
    );
    assert!(error.contains("未改动"), "{error}");
}

#[test]
fn a_manifest_that_is_not_valid_is_refused() {
    let fixture = Fixture::new();
    let host = fixture.host(running());
    fixture.write_manifest("<html>not found</html>");
    assert_refused(&fixture, &host, &fixture.args(), "JSON");
    fixture.write_manifest(r#"{"version":"0.7.0","commit":"abcdef12","assets":{}}"#);
    assert_refused(&fixture, &host, &fixture.args(), "commit_time");
    std::fs::remove_file(fixture.release.join("manifest.json")).unwrap();
    assert_refused(&fixture, &host, &fixture.args(), "manifest.json");
}

#[test]
fn packages_with_the_wrong_members_are_refused() {
    let summon = member("summon", FAKE_SUMMON.as_bytes());
    let sidecar = || member("summon-sidecar", b"new sidecar");
    let cases = [
        (
            "多成员",
            tarball(&[
                member("summon", FAKE_SUMMON.as_bytes()),
                sidecar(),
                member("extra", b"x"),
            ]),
            "extra",
        ),
        ("少成员", tarball(&[summon]), "summon-sidecar"),
        (
            "带路径",
            tarball(&[member("bin/summon", FAKE_SUMMON.as_bytes()), sidecar()]),
            "bin/summon",
        ),
        (
            "上一级",
            tarball(&[member("../summon", FAKE_SUMMON.as_bytes()), sidecar()]),
            "../summon",
        ),
        (
            "链接",
            tarball(&[
                member("summon", FAKE_SUMMON.as_bytes()),
                Member {
                    name: "summon-sidecar",
                    body: b"",
                    link: true,
                },
            ]),
            "不是普通文件",
        ),
    ];
    for (what, package, want) in cases {
        let fixture = Fixture::new();
        fixture.publish_release(&new_build(), &package);
        let error = assert_refused(&fixture, &fixture.host(running()), &fixture.args(), want);
        assert!(error.contains("未改动"), "{what}: {error}");
        assert!(
            !fixture.root.join("summon").exists() && !fixture.home.join("summon").exists(),
            "{what}:解包写到了暂存之外"
        );
    }
}

#[test]
fn a_staged_summon_that_cannot_report_its_stamp_is_refused() {
    let fixture = Fixture::new();
    std::fs::write(fixture.home.join("stamp-mode"), "fail").unwrap();
    let error = assert_refused(
        &fixture,
        &fixture.host(running()),
        &fixture.args(),
        "install-stamp print 失败",
    );
    assert!(error.contains("假 install-stamp 坏了"), "{error}");

    let fixture = Fixture::new();
    std::fs::write(fixture.home.join("stamp-mode"), "hang").unwrap();
    let host = Host {
        stamp_timeout: Duration::from_millis(500),
        ..fixture.host(running())
    };
    let started = Instant::now();
    assert_refused(&fixture, &host, &fixture.args(), "没有结束");
    assert!(started.elapsed() < Duration::from_secs(20), "等待不得无界");
}

#[test]
fn an_unsupported_platform_is_told_to_build_from_source() {
    let fixture = Fixture::new();
    let host = Host {
        platform: None,
        ..fixture.host(running())
    };
    assert_refused(&fixture, &host, &fixture.args(), "从源码安装");
}

#[test]
fn bad_arguments_are_refused_before_anything_is_fetched() {
    let fixture = Fixture::new();
    for (args, want) in [
        (
            UpgradeArgs {
                version: Some("v0.7.0".into()),
                ..fixture.args()
            },
            "--version",
        ),
        (
            UpgradeArgs {
                from: Some(fixture.root.join("missing").display().to_string()),
                ..UpgradeArgs::default()
            },
            "--from",
        ),
        (
            UpgradeArgs {
                from: Some("https://".into()),
                ..UpgradeArgs::default()
            },
            "--from",
        ),
    ] {
        assert_refused(&fixture, &fixture.host(running()), &args, want);
    }
}

#[test]
fn check_reports_without_downloading_or_locking() {
    let fixture = Fixture::new();
    let _held = upgrade_restart::lock(&fixture.home).unwrap();
    let before = fixture.install_snapshot();
    let args = UpgradeArgs {
        check: true,
        ..fixture.args()
    };

    let (result, out) = run(&fixture.host(running()), &args);

    result.unwrap();
    assert!(out.contains("0.6.9 (1234567a"), "{out}");
    assert!(out.contains("0.7.0 (abcdef12"), "{out}");
    assert!(out.contains("有新版"), "{out}");
    assert_eq!(fixture.install_snapshot(), before);
    assert!(fixture.stages().is_empty());

    // 没有本平台的包也如实说。
    let package = good_package();
    fixture.write_manifest(&manifest(
        &new_build(),
        &[("linux-arm64", package.len(), sha256(&package))],
    ));
    let (result, out) = run(&fixture.host(running()), &args);
    result.unwrap();
    assert!(out.contains("没有本平台(linux-x64)的包"), "{out}");
}

#[test]
fn a_second_upgrade_is_refused_while_one_is_running() {
    let fixture = Fixture::new();
    let _held = upgrade_restart::lock(&fixture.home).unwrap();
    assert_refused(
        &fixture,
        &fixture.host(stopped()),
        &fixture.args(),
        "另一个 summon upgrade 正在进行",
    );
}

/// macOS 的签名核对,用假 codesign 在任何系统上跑:三件套都要验签通过、团队一致,当前这份有团队时
/// 要与它相同,没有团队时打印新包的团队。
#[test]
fn darwin_packages_must_keep_the_signing_team() {
    let package = |menubar: &[u8]| {
        tarball(&[
            member("summon", FAKE_SUMMON.as_bytes()),
            member("summon-sidecar", b"#!/bin/sh\n# team=TEAMA\n"),
            member("summon-menubar", menubar),
        ])
    };
    let setup = |menubar: &[u8], current: &str| {
        let fixture = Fixture::new();
        let package = package(menubar);
        std::fs::write(fixture.release.join("summon-darwin-arm64.tar.gz"), &package).unwrap();
        fixture.write_manifest(&manifest(
            &new_build(),
            &[("darwin-arm64", package.len(), sha256(&package))],
        ));
        let tool = fixture.root.join("tools/codesign");
        std::fs::create_dir_all(tool.parent().unwrap()).unwrap();
        write_test_executable(&tool, FAKE_CODESIGN);
        let current_exe = fixture.root.join("current-summon");
        std::fs::write(&current_exe, current).unwrap();
        let host = Host {
            platform: Some("darwin-arm64"),
            codesign: Some(Codesign {
                tool,
                current: current_exe,
            }),
            ..fixture.host(stopped())
        };
        (fixture, host)
    };

    let (fixture, host) = setup(b"# team=TEAMA\n", "# team=TEAMA\n");
    let (result, out) = run(&host, &fixture.args());
    result.unwrap();
    assert!(out.contains("签名团队 TEAMA 与当前这份相同"), "{out}");
    assert_eq!(fixture.stamp_calls(), "stage\n", "签名过了才执行暂存那份");
    assert_eq!(fixture.read_install("summon-menubar"), b"# team=TEAMA\n");

    let (fixture, host) = setup(b"# team=TEAMA\n", "ad-hoc\n");
    let (result, out) = run(&host, &fixture.args());
    result.unwrap();
    assert!(out.contains("新包的签名团队是 TEAMA"), "{out}");

    // 签名没过:暂存里的东西一个都没执行。
    for (menubar, current, want) in [
        (&b"# team=TEAMA\n"[..], "# team=TEAMB\n", "TEAMB"),
        (&b"# team=TEAMC\n"[..], "# team=TEAMA\n", "TEAMC"),
        (
            &b"# unsigned\n"[..],
            "# team=TEAMA\n",
            "codesign --verify --strict",
        ),
    ] {
        let (fixture, host) = setup(menubar, current);
        assert_refused(&fixture, &host, &fixture.args(), want);
        assert_eq!(fixture.stamp_calls(), "", "{want}:签名没过就执行了暂存那份");
    }
}

/// 本地 HTTP 模拟的 Release:`/r/<名字>` 先 302 到 `/files/<名字>`(GitHub 的直链也是跳转),
/// `/files/<名字>` 取 `dir` 里的文件;`cut` 里列出的文件只发出一半就断开。`/away/<名字>` 与
/// `/away-https/<名字>` 跳到文档专用、不可路由的远端地址(明文与 https 各一)。
pub(crate) fn serve(dir: PathBuf, cut: &'static [&'static str]) -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let dir = dir.clone();
            std::thread::spawn(move || {
                let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
                let mut first = String::new();
                if reader.read_line(&mut first).is_err() {
                    return;
                }
                loop {
                    let mut line = String::new();
                    match reader.read_line(&mut line) {
                        Ok(0) | Err(_) => return,
                        Ok(_) if line == "\r\n" => break,
                        Ok(_) => {}
                    }
                }
                let path = first.split_whitespace().nth(1).unwrap_or("/").to_string();
                let response = |head: String, body: &[u8]| {
                    let mut bytes = head.into_bytes();
                    bytes.extend_from_slice(body);
                    bytes
                };
                let bytes = if let Some(name) = path.strip_prefix("/away-https/") {
                    response(
                        format!(
                            "HTTP/1.1 302 Found\r\nLocation: https://192.0.2.1/{name}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        ),
                        b"",
                    )
                } else if let Some(name) = path.strip_prefix("/away/") {
                    response(
                        format!(
                            "HTTP/1.1 302 Found\r\nLocation: http://192.0.2.1/{name}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        ),
                        b"",
                    )
                } else if let Some(name) = path.strip_prefix("/r/") {
                    response(
                        format!(
                            "HTTP/1.1 302 Found\r\nLocation: /files/{name}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        ),
                        b"",
                    )
                } else if let Some(body) = path
                    .strip_prefix("/files/")
                    .and_then(|name| std::fs::read(dir.join(name)).ok())
                {
                    let name = path.trim_start_matches("/files/");
                    let head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let body = if cut.contains(&name) {
                        &body[..body.len() / 2]
                    } else {
                        &body[..]
                    };
                    response(head, body)
                } else {
                    response(
                        "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                            .to_string(),
                        b"",
                    )
                };
                let _ = stream.write_all(&bytes);
                let _ = stream.flush();
            });
        }
    });
    base
}

#[test]
fn upgrades_from_an_http_release_through_redirects() {
    let fixture = Fixture::new();
    let base = serve(fixture.release.clone(), &[]);
    let args = UpgradeArgs {
        from: Some(format!("{base}/r")),
        ..UpgradeArgs::default()
    };

    let (result, out) = run(&fixture.host(stopped()), &args);

    result.unwrap();
    assert!(
        out.contains(&format!("{base}/r/summon-linux-x64.tar.gz")),
        "{out}"
    );
    assert_eq!(fixture.read_install("summon-sidecar"), b"new sidecar");
    assert!(fixture.stages().is_empty());
}

#[test]
fn an_interrupted_http_download_is_refused() {
    let fixture = Fixture::new();
    let base = serve(fixture.release.clone(), &["summon-linux-x64.tar.gz"]);
    let args = UpgradeArgs {
        from: Some(format!("{base}/files")),
        ..UpgradeArgs::default()
    };
    let error = assert_refused(&fixture, &fixture.host(running()), &args, "未改动");
    assert!(
        error.contains("中断") || error.contains("字节数"),
        "{error}"
    );
}

#[test]
fn an_http_release_without_a_manifest_is_refused() {
    let fixture = Fixture::new();
    let base = serve(fixture.root.join("empty"), &[]);
    let args = UpgradeArgs {
        from: Some(format!("{base}/files")),
        ..UpgradeArgs::default()
    };
    assert_refused(&fixture, &fixture.host(running()), &args, "404");
}

/// 明文 http 只许本机:别处的 http 地址(含借用户信息伪装成本机的)在联网之前就拒绝 —— 192.0.2.1
/// 是文档专用、不可路由的地址,真去连只会等到连接超时,所以「立刻返回且报的是这条」说明没有发请求。
#[test]
fn plaintext_sources_other_than_this_machine_are_refused_before_any_request() {
    let fixture = Fixture::new();
    for from in [
        "http://192.0.2.1/release",
        "http://example.invalid/release",
        "http://localhost@192.0.2.1/release",
        "http://127.0.0.1.example.invalid/release",
    ] {
        let args = UpgradeArgs {
            from: Some(from.to_string()),
            ..UpgradeArgs::default()
        };
        let started = Instant::now();
        assert_refused(&fixture, &fixture.host(running()), &args, "https://");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{from} 发出了请求"
        );
    }
    for from in [
        "http://127.0.0.1:8080/release",
        "http://localhost/release",
        "http://[::1]:8080/release",
        "https://mirror.example/release",
    ] {
        let args = UpgradeArgs {
            from: Some(from.to_string()),
            ..UpgradeArgs::default()
        };
        check_args(&args).unwrap_or_else(|error| panic!("{from}:{error:#}"));
    }
}

/// 跳转的判定:https 都跟;明文 http 只在本机来源跳本机时跟,从 https 出发的绝不降到明文。
#[test]
fn redirects_never_downgrade_to_plaintext() {
    let url = |text: &str| reqwest::Url::parse(text).unwrap();
    let github = url("https://github.com/owner/name/releases/latest/download/manifest.json");
    let local = url("http://127.0.0.1:8080/r/manifest.json");
    for (origin, to, allowed) in [
        (&github, "https://objects.example/manifest.json", true),
        (&github, "http://objects.example/manifest.json", false),
        (&github, "http://127.0.0.1/manifest.json", false),
        (&local, "http://127.0.0.1:8080/files/manifest.json", true),
        (&local, "http://localhost:8080/files/manifest.json", true),
        (&local, "https://mirror.example/manifest.json", true),
        (&local, "http://192.0.2.1/manifest.json", false),
        (&local, "ftp://127.0.0.1/manifest.json", false),
    ] {
        assert_eq!(
            source::redirect_allowed(origin, &url(to)),
            allowed,
            "{origin} → {to}"
        );
    }
    for (from, allowed) in [
        ("https://github.com/owner/name", true),
        ("http://127.0.0.1:1", true),
        ("http://localhost", true),
        ("http://[::1]", true),
        ("http://192.0.2.1", false),
        ("http://user@localhost", false),
        ("ftp://127.0.0.1", false),
    ] {
        assert_eq!(source::source_allowed(from), allowed, "{from}");
    }
}

/// 本机来源被跳到别处的明文地址:跳转被拒,不去连那个地址。
#[test]
fn a_redirect_away_from_this_machine_is_refused() {
    let fixture = Fixture::new();
    let base = serve(fixture.release.clone(), &[]);
    let args = UpgradeArgs {
        from: Some(format!("{base}/away")),
        ..UpgradeArgs::default()
    };
    let started = Instant::now();
    let error = assert_refused(&fixture, &fixture.host(running()), &args, "拒绝跳转");
    assert!(error.contains("http://192.0.2.1/manifest.json"), "{error}");
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "去连了跳转目标"
    );
}

/// 默认来源只走仓库的 releases 直链:latest 用 `releases/latest/download`,指定版本用那个 tag;
/// 升级相关的代码里没有任何地方请求 GitHub API。
#[test]
fn the_default_source_uses_release_download_links_only() {
    let repo = source::REPOSITORY;
    let path = repo
        .strip_prefix("https://github.com/")
        .unwrap_or_else(|| panic!("仓库地址要是 https://github.com/<owner>/<name>:{repo}"));
    let parts: Vec<&str> = path.split('/').collect();
    assert!(
        parts.len() == 2 && parts.iter().all(|part| !part.is_empty()),
        "{repo}"
    );
    assert_eq!(
        source::release_base(None),
        format!("{repo}/releases/latest/download")
    );
    assert_eq!(
        source::release_base(Some("0.7.0")),
        format!("{repo}/releases/download/v0.7.0")
    );
    let api = ["api", "github", "com"].join(".");
    for (name, text) in [
        ("mod.rs", include_str!("mod.rs")),
        ("source.rs", include_str!("source.rs")),
        ("upgrade_restart.rs", include_str!("../upgrade_restart.rs")),
        (
            "release.rs",
            include_str!("../../../../summon-core/src/release.rs"),
        ),
    ] {
        assert!(!text.contains(&api), "{name} 请求了 GitHub API");
    }
}

/// 重启 daemon 的路径不碰 tmux:换代只经新版自己的 `daemon restart`(那段代码同样不杀会话)。
#[test]
fn the_restart_path_never_touches_tmux() {
    for (name, text) in [
        ("mod.rs", include_str!("mod.rs")),
        ("upgrade_restart.rs", include_str!("../upgrade_restart.rs")),
    ] {
        let code: Vec<&str> = text
            .lines()
            .map(str::trim)
            .filter(|line| !line.starts_with("//"))
            .filter(|line| line.contains("tmux"))
            .collect();
        assert!(code.is_empty(), "{name}: {code:?}");
    }
}

/// 命令面:`summon upgrade` 落到 GitHub 渠道。隐藏的 `--channel` 只认 `github`,其余取值报错;
/// `--registry` 不是合法参数。
#[test]
fn the_command_line_reaches_the_github_channel_in_every_build() {
    use clap::Parser;
    let parse = |extra: &[&str]| {
        let argv = [
            "summon",
            "upgrade",
            "--version",
            "0.7.0",
            "--from",
            "release-dir",
            "--check",
        ];
        crate::cli::cli::Cli::try_parse_from(argv.iter().chain(extra))
    };
    for extra in [&[][..], &["--channel", "github"][..]] {
        let cli = parse(extra).unwrap();
        let crate::cli::cli::Cmd::Upgrade(command) = cli.cmd else {
            panic!("summon upgrade 没有落到 GitHub 渠道");
        };
        assert_eq!(command.args.version.as_deref(), Some("0.7.0"));
        assert_eq!(command.args.from.as_deref(), Some("release-dir"));
        assert!(command.args.check);
        assert_eq!(command.channel.is_some(), !extra.is_empty(), "{extra:?}");
    }
    for channel in ["npm", "gitlab"] {
        let Err(error) = parse(&["--channel", channel]) else {
            panic!("--channel {channel} 应被拒绝");
        };
        let error = error.to_string();
        assert!(
            error.contains("GitHub Releases") && error.contains("npm"),
            "{channel}: {error}"
        );
    }
    assert!(parse(&["--registry", "https://registry.test"]).is_err());
}
