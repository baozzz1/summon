use super::super::runner::{doctor_lines_at, Runner};
use super::super::state::{today, Event, Pending, State, Store};
use super::*;
use crate::setup::github_upgrade::tests::{self as gh, Fixture};
use summon_core::config::{Config, Upgrade};
use summon_core::install_stamp::BuildInfo;

/// fixture 里 manifest 的发布时间 `2026-01-02T03:04:05Z`。
const PUBLISHED: u64 = 1_767_323_045;
/// 发布满 3 小时(默认 `min_age_hours` 是 2)。
const LATER: u64 = PUBLISHED + 3 * 3600;

fn channel_at(home: &Path, drill: Option<Drill>, codesign: Option<Codesign>) -> GithubChannel {
    GithubChannel {
        home: home.to_path_buf(),
        platform: gh::PLATFORM,
        drill,
        codesign,
        stamp_timeout: Duration::from_secs(30),
        manifest: None,
    }
}

fn policy(from: &str) -> Upgrade {
    Upgrade {
        auto: true,
        channel: Some("github".into()),
        from: Some(from.to_string()),
        ..Upgrade::default()
    }
}

/// 在跑 0.6.9 的一代 daemon 的自动升级线程,渠道固定是本平台无关的 linux-x64(fixture 只发它)。
fn runner(fixture: &Fixture, drill: Drill, codesign: Option<(PathBuf, PathBuf)>) -> Runner {
    Runner::new(fixture.home.clone(), "0.6.9").with_opener(Box::new(move |home, _| {
        let codesign = codesign
            .clone()
            .map(|(tool, current)| Codesign { tool, current });
        Ok(Box::new(channel_at(home, Some(drill.clone()), codesign)) as Box<dyn Channel>)
    }))
}

/// 跑一次检查,返回之后的状态。
fn check(runner: &mut Runner, fixture: &Fixture, now: u64) -> State {
    let store = Store::at_home(&fixture.home);
    let state = store.load().unwrap();
    runner.check_and_install(&policy("unused"), &state, now);
    store.load().unwrap()
}

fn auto_stages(home: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(home.join(".summon/run")) else {
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

fn dir_drill(fixture: &Fixture) -> Drill {
    Drill::Dir(fixture.release.clone())
}

/// 有合适的新版:取到 `auto-upgrade-stage.*` 里、核对过、记成待换代;落点与 daemon 都不碰。
#[test]
fn a_newer_release_is_staged_without_touching_the_install_or_the_daemon() {
    let fixture = Fixture::new();
    let before = fixture.install_snapshot();
    let mut runner = runner(&fixture, dir_drill(&fixture), None);

    let state = check(&mut runner, &fixture, LATER);

    let pending = state.pending.expect("记成待换代");
    assert_eq!(
        (pending.version.as_str(), pending.previous.as_str()),
        ("0.7.0", "0.6.9")
    );
    let stage = pending.stage.expect("GitHub 渠道的待换代带暂存");
    assert_eq!(auto_stages(&fixture.home), std::slice::from_ref(&stage));
    assert!(stage.join("KEEP").exists(), "待换代的暂存打上保留标记");
    assert_eq!(
        std::fs::read(stage.join("pkg/summon")).unwrap(),
        gh::FAKE_SUMMON.as_bytes()
    );
    assert_eq!(fixture.install_snapshot(), before, "落点不动");
    assert!(!fixture.calls().contains("daemon"), "{}", fixture.calls());
    assert!(
        fixture.stages().is_empty(),
        "不用 summon upgrade 的暂存前缀"
    );
    assert_eq!(state.latest.unwrap().published_secs, Some(PUBLISHED));
    let log =
        std::fs::read_to_string(crate::setup::upgrade_restart::log_path(&fixture.home)).unwrap();
    assert!(log.contains("演练"), "演练来源写进 upgrade.log:{log}");
}

/// 私有期取 manifest 会是 404、一张登录网页或者断连:都只是这次没查成,下个周期再查,不罚;
/// 同一原因只计次,保留第一次的时刻。
#[test]
fn an_unreachable_or_unreadable_manifest_is_retried_next_cycle_and_counted() {
    const HTML: &str = "<!DOCTYPE html><html><body>Sign in to GitHub</body></html>";
    type Setup = Box<dyn Fn(&Fixture) -> Drill>;
    let cases: Vec<(&str, Setup)> = vec![
        (
            "目录里没有 manifest",
            Box::new(|f: &Fixture| {
                std::fs::remove_file(f.release.join("manifest.json")).unwrap();
                dir_drill(f)
            }),
        ),
        (
            "目录里的 manifest 是网页",
            Box::new(|f: &Fixture| {
                f.write_manifest(HTML);
                dir_drill(f)
            }),
        ),
        (
            "HTTP 404",
            Box::new(|f: &Fixture| {
                std::fs::create_dir_all(f.root.join("empty")).unwrap();
                let base = gh::serve(f.root.join("empty"), &[]);
                Drill::Loopback(format!("{base}/r"))
            }),
        ),
        (
            "HTTP 200 但内容是网页",
            Box::new(|f: &Fixture| {
                f.write_manifest(HTML);
                let base = gh::serve(f.release.clone(), &[]);
                Drill::Loopback(format!("{base}/r"))
            }),
        ),
        (
            "HTTP 断连",
            Box::new(|f: &Fixture| {
                let base = gh::serve(f.release.clone(), &["manifest.json"]);
                Drill::Loopback(format!("{base}/files"))
            }),
        ),
    ];
    for (name, drill) in cases {
        let fixture = Fixture::new();
        let before = fixture.install_snapshot();
        let mut runner = runner(&fixture, drill(&fixture), None);
        check(&mut runner, &fixture, LATER);
        let state = check(&mut runner, &fixture, LATER + 7 * 3600);
        let event = state.last_event.clone().unwrap();
        assert!(event.text.contains("下个周期再查"), "{name}:{}", event.text);
        assert_eq!((event.at_secs, event.count), (LATER, 2), "{name}");
        assert_eq!(state.last_check_secs, Some(LATER + 7 * 3600), "{name}");
        assert!(state.pending.is_none(), "{name}");
        assert!(state.held.is_none(), "{name}:不罚");
        assert!(state.rolled_back.is_empty(), "{name}");
        assert!(auto_stages(&fixture.home).is_empty(), "{name}");
        assert_eq!(fixture.install_snapshot(), before, "{name}");
    }
}

fn manifest_with(build: &BuildInfo, published_at: Option<&str>) -> String {
    let package = gh::good_package();
    let text = gh::manifest(
        build,
        &[(gh::PLATFORM, package.len(), gh::sha256(&package))],
    );
    let mut value: serde_json::Value = serde_json::from_str(&text).unwrap();
    match published_at {
        Some(at) => value["published_at"] = serde_json::Value::String(at.into()),
        None => {
            value.as_object_mut().unwrap().remove("published_at");
        }
    }
    value.to_string()
}

/// 发布时间缺失、不成形、在未来或还没满 `min_age_hours`:不取、不罚,下个周期再看。
#[test]
fn a_release_without_a_trustworthy_or_old_enough_publish_time_is_not_taken() {
    let one_hour_later = chrono::DateTime::from_timestamp((PUBLISHED + 3600) as i64, 0)
        .unwrap()
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    let future = chrono::DateTime::from_timestamp((LATER + 3600) as i64, 0)
        .unwrap()
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    for (published_at, now, want) in [
        (None, LATER, "发布时间"),
        (Some("2026-01-02 03:04:05"), LATER, "发布时间"),
        (Some(future.as_str()), LATER, "未满 2 小时"),
        (
            Some(one_hour_later.as_str()),
            PUBLISHED + 2 * 3600,
            "未满 2 小时",
        ),
    ] {
        let fixture = Fixture::new();
        fixture.write_manifest(&manifest_with(&gh::new_build(), published_at));
        let mut runner = runner(&fixture, dir_drill(&fixture), None);
        let state = check(&mut runner, &fixture, now);
        let event = state.last_event.unwrap().text;
        assert!(event.contains(want), "{published_at:?}:{event}");
        assert!(
            state.pending.is_none() && state.held.is_none(),
            "{published_at:?}"
        );
        assert!(auto_stages(&fixture.home).is_empty());
        assert_eq!(fixture.stamp_calls(), "", "不下载、不执行任何东西");
    }
}

/// 这一版没有本平台的包:不取、不罚,下个周期再看。
#[test]
fn a_release_without_this_platform_waits_for_the_next_cycle() {
    let fixture = Fixture::new();
    let package = gh::good_package();
    fixture.write_manifest(&gh::manifest(
        &gh::new_build(),
        &[("linux-arm64", package.len(), gh::sha256(&package))],
    ));
    let mut runner = runner(&fixture, dir_drill(&fixture), None);
    let state = check(&mut runner, &fixture, LATER);
    assert!(state.last_event.unwrap().text.contains("还没发布"));
    assert!(state.pending.is_none() && state.held.is_none());
    assert!(auto_stages(&fixture.home).is_empty());
}

/// 取到的包核对不过(字节数、哈希、成员、构建戳、签名团队):这一版当天不再试,不进退回名单;
/// 落点不碰、暂存清掉、daemon 不碰。签名没过时暂存那份一次都不执行。
#[test]
fn a_package_that_fails_verification_is_held_for_the_day_not_rolled_back() {
    type Setup = Box<dyn Fn(&Fixture) -> Option<(PathBuf, PathBuf)>>;
    let cases: Vec<(&str, Setup)> = vec![
        (
            "字节数",
            Box::new(|f: &Fixture| {
                let package = gh::good_package();
                f.write_manifest(&gh::manifest(
                    &gh::new_build(),
                    &[(gh::PLATFORM, package.len() + 1, gh::sha256(&package))],
                ));
                None
            }),
        ),
        (
            "SHA-256",
            Box::new(|f: &Fixture| {
                let package = gh::good_package();
                f.write_manifest(&gh::manifest(
                    &gh::new_build(),
                    &[(gh::PLATFORM, package.len(), gh::sha256(b"other"))],
                ));
                None
            }),
        ),
        (
            "成员",
            Box::new(|f: &Fixture| {
                let package = gh::tarball(&[
                    gh::member("summon", gh::FAKE_SUMMON.as_bytes()),
                    gh::member("summon-sidecar", b"new sidecar"),
                    gh::member("extra", b"x"),
                ]);
                f.publish_release(&gh::new_build(), &package);
                None
            }),
        ),
        (
            "构建戳",
            Box::new(|f: &Fixture| {
                f.reports(&gh::build("0.7.0", "fedcba98", 2_000));
                None
            }),
        ),
        (
            "签名团队",
            Box::new(|f: &Fixture| {
                std::fs::create_dir_all(f.root.join("tools")).unwrap();
                let tool = f.root.join("tools/codesign");
                crate::test_support::write_test_executable(&tool, gh::FAKE_CODESIGN);
                let current = f.root.join("current-summon");
                std::fs::write(&current, "# team=TEAMB\n").unwrap();
                Some((tool, current))
            }),
        ),
    ];
    for (name, setup) in cases {
        let fixture = Fixture::new();
        let codesign = setup(&fixture);
        let signing = codesign.is_some();
        let before = fixture.install_snapshot();
        let mut runner = runner(&fixture, dir_drill(&fixture), codesign);
        let state = check(&mut runner, &fixture, LATER);
        let held = state
            .held
            .clone()
            .unwrap_or_else(|| panic!("{name}:当天不再试"));
        assert_eq!(
            (held.version.as_str(), held.day),
            ("0.7.0", today()),
            "{name}"
        );
        assert!(
            state.rolled_back.is_empty(),
            "{name}:没跑过的版本不进退回名单"
        );
        assert!(state.pending.is_none(), "{name}");
        assert!(auto_stages(&fixture.home).is_empty(), "{name}:暂存清掉");
        assert_eq!(fixture.install_snapshot(), before, "{name}");
        assert!(!fixture.calls().contains("daemon"), "{name}");
        if signing {
            assert_eq!(fixture.stamp_calls(), "", "{name}:签名没过就执行了暂存那份");
        }
    }
}

/// 落点比 Release 新(源码部署过):不取、不替换。
#[test]
fn an_install_newer_than_the_release_is_left_alone() {
    let fixture = Fixture::new();
    let target = summon_core::companion::install_target(&fixture.home);
    let newer = gh::build("0.8.0", "99999999", 9_000);
    self_install::record_from(&target.canonicalize().unwrap(), &fixture.home, &newer).unwrap();
    std::fs::write(fixture.home.join("version-line"), "0.8.0 (99999999)").unwrap();
    let mut runner = runner(&fixture, dir_drill(&fixture), None);
    let state = check(&mut runner, &fixture, LATER);
    assert!(state.pending.is_none() && state.held.is_none());
    assert!(auto_stages(&fixture.home).is_empty());
}

/// 不演练时查新版看 latest,取包用读到的那一版的 tag;只走 Release 直链。
#[test]
fn the_package_comes_from_the_tag_of_the_version_read() {
    let fixture = Fixture::new();
    let channel = channel_at(&fixture.home, None, None);
    let latest = channel.source(None).unwrap().locate(release::MANIFEST);
    let tagged = channel
        .source(Some("0.7.0"))
        .unwrap()
        .locate("summon-linux-x64.tar.gz");
    assert!(
        latest.ends_with("/releases/latest/download/manifest.json"),
        "{latest}"
    );
    assert!(
        tagged.ends_with("/releases/download/v0.7.0/summon-linux-x64.tar.gz"),
        "{tagged}"
    );
    for url in [&latest, &tagged] {
        assert!(url.starts_with("https://github.com/"), "{url}");
    }
    let source = include_str!("../github.rs");
    let fetch = &source[source.find("fn fetch(").unwrap()..];
    let fetch = &fetch[..fetch.find("\n    }\n").unwrap()];
    assert!(
        fetch.contains("self.source(Some(version))"),
        "取包用那一版的 tag"
    );
}

/// GitHub 渠道自己不起任何进程、不调 GitHub API:取包、核对都经 `summon upgrade` 那一套。
#[test]
fn the_channel_spawns_nothing_itself_and_never_calls_the_api() {
    let source = include_str!("../github.rs");
    let code: Vec<&str> = source
        .lines()
        .map(str::trim)
        .filter(|line| !line.starts_with("//"))
        .collect();
    for needle in ["Command::new", "api.github.com", "tmux"] {
        assert!(!code.iter().any(|line| line.contains(needle)), "{needle}");
    }
}

fn pending_at(stage: Option<PathBuf>) -> Pending {
    Pending {
        version: "0.7.0".into(),
        previous: "0.6.9".into(),
        stage,
    }
}

/// 换代前复核:暂存还在、里面那份报的正是待换的版本才换;否则作废。
#[test]
fn the_stage_is_rechecked_before_the_swap() {
    let fixture = Fixture::new();
    let mut runner = runner(&fixture, dir_drill(&fixture), None);
    let stage = check(&mut runner, &fixture, LATER)
        .pending
        .unwrap()
        .stage
        .unwrap();
    let channel = channel_at(&fixture.home, None, None);

    let rechecked = channel
        .recheck(&pending_at(Some(stage.clone())))
        .unwrap()
        .unwrap();
    assert_eq!(rechecked.entry, stage.join("pkg/summon"));
    assert_eq!(
        rechecked.watch_args,
        [OsString::from("--stage"), stage.clone().into_os_string()]
    );

    let stale = |pending: &Pending| channel.recheck(pending).unwrap().unwrap_err();
    assert!(stale(&pending_at(None)).contains("没有暂存目录"));
    fixture.reports(&gh::build("0.7.1", "abcdef12", 2_000));
    assert!(stale(&pending_at(Some(stage.clone()))).contains("0.7.1"));
    std::fs::remove_dir_all(&stage).unwrap();
    assert!(stale(&pending_at(Some(stage))).contains("已不在"));
}

/// `upgrade.from` 只收绝对路径的本地目录与本机回环地址上的明文 http。
#[test]
fn rehearsal_sources_are_only_local_directories_or_loopback_http() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().canonicalize().unwrap();
    assert_eq!(
        Drill::parse(&dir.display().to_string()),
        Ok(Drill::Dir(dir.clone()))
    );
    for ok in [
        "http://127.0.0.1:8000/r",
        "http://localhost:9/x",
        "http://[::1]:8000",
    ] {
        assert_eq!(Drill::parse(ok), Ok(Drill::Loopback(ok.into())), "{ok}");
    }
    for bad in [
        "https://example.com/releases",
        "https://127.0.0.1/r",
        "http://example.com/r",
        "http://192.0.2.1/r",
        "http://user@127.0.0.1/r",
        "http://127.0.0.1.example.com/r",
        "http://",
        "ftp://127.0.0.1/r",
        "file:///tmp",
        "relative/dir",
        "./dir",
    ] {
        let error = Drill::parse(bad).unwrap_err();
        assert!(error.contains("upgrade.from"), "{bad}:{error}");
    }
    let missing = dir.join("missing");
    assert!(
        Drill::parse(&missing.display().to_string()).is_err(),
        "目录得在"
    );
    let file = dir.join("file");
    std::fs::write(&file, "x").unwrap();
    assert!(
        Drill::parse(&file.display().to_string()).is_err(),
        "得是目录"
    );
}

/// `upgrade.channel` / `upgrade.from` 写错只让自动升级不生效、说明原因。
#[test]
fn a_misconfigured_channel_is_refused_with_a_reason() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let open = |channel: Option<&str>, from: Option<&str>| {
        let policy = Upgrade {
            auto: true,
            channel: channel.map(str::to_string),
            from: from.map(str::to_string),
            ..Upgrade::default()
        };
        super::super::channel::open(home, &policy).err()
    };
    assert!(open(Some("gitlab"), None).unwrap().contains("gitlab"));
    if release::platform(std::env::consts::OS, std::env::consts::ARCH).is_some() {
        assert!(open(Some("github"), Some("http://example.com/r"))
            .unwrap()
            .contains("upgrade.from"));
        assert!(open(Some("github"), Some("relative")).is_some());
        assert!(
            open(Some("npm"), Some("http://example.com/r"))
                .unwrap()
                .contains("upgrade.from"),
            "停用的 npm 写法按 GitHub 渠道走,同样的 upgrade.from 闸"
        );
    }
}

/// 只有 GitHub Releases 一条渠道:不写 `upgrade.channel`、写 `github`、写停用的 `npm`,所有构建里
/// 打开的都是它;只有写着 `npm` 时 doctor 多一行提示删掉它。
#[test]
fn every_build_defaults_to_github_and_reads_the_retired_npm_channel_as_github() {
    let Some(platform) = release::platform(std::env::consts::OS, std::env::consts::ARCH) else {
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    for channel in [None, Some("github"), Some("npm"), Some(" npm ")] {
        let policy = Upgrade {
            auto: true,
            channel: channel.map(str::to_string),
            ..Upgrade::default()
        };
        let opened = super::super::channel::open(tmp.path(), &policy)
            .unwrap_or_else(|why| panic!("{channel:?}: {why}"));
        assert_eq!(opened.platform(), platform, "{channel:?}");
        assert!(
            opened.doctor_notes().join("\n").contains("GitHub Releases"),
            "{channel:?}"
        );
        let note = super::super::channel::retired_note(&policy);
        assert_eq!(
            note.is_some(),
            channel.is_some_and(|c| c.trim() == "npm"),
            "{channel:?}"
        );
        if let Some(note) = note {
            assert!(note.contains("npm") && note.contains("github"), "{note}");
        }
    }
}

fn make_stage(home: &Path, name: &str) -> PathBuf {
    let dir = home.join(".summon/run").join(name);
    std::fs::create_dir_all(dir.join("pkg")).unwrap();
    std::fs::write(dir.join("pkg/summon"), "x").unwrap();
    dir
}

/// 不再被引用的自动升级暂存清掉;待换代引用的、正在跑的这份所在的、`summon upgrade` 的都不动。
#[test]
fn stale_auto_upgrade_stages_are_swept_but_live_ones_kept() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let pending = make_stage(home, "auto-upgrade-stage.pending");
    let hosting = make_stage(home, "auto-upgrade-stage.hosting");
    let stale = make_stage(home, "auto-upgrade-stage.stale");
    let manual = make_stage(home, "upgrade-stage.manual");
    sweep_stages(home, Some(&pending), Some(&hosting.join("pkg/summon")));
    assert!(pending.exists() && hosting.exists() && manual.exists());
    assert!(!stale.exists());
    sweep_stages(home, None, None);
    assert!(!pending.exists() && !hosting.exists());
    assert!(manual.exists(), "手动升级的暂存归它自己的规则管");
}

/// 换代刚完成时:本平台清单上的每个文件暂存里都在、落点里的与它逐字节相同,暂存里也没有清单之外的。
#[test]
fn the_install_has_landed_only_when_every_staged_file_matches() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let stage = make_stage(home, "auto-upgrade-stage.x");
    std::fs::write(stage.join("pkg/summon-sidecar"), "new sidecar").unwrap();
    let bin = summon_core::companion::install_dir(home);
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::write(bin.join("summon"), "x").unwrap();
    std::fs::write(bin.join("summon-sidecar"), "new sidecar").unwrap();
    let linux = Some("linux-x64");
    assert_eq!(landed(home, &stage, linux), Ok(()));

    // 半发布:summon 换了,sidecar 没换。
    std::fs::write(bin.join("summon-sidecar"), "old sidecar").unwrap();
    assert!(landed(home, &stage, linux)
        .unwrap_err()
        .contains("summon-sidecar"));
    // 长度相同、内容不同也算没换。
    std::fs::write(bin.join("summon-sidecar"), "NEW SIDECAR").unwrap();
    assert!(landed(home, &stage, linux).is_err());
    std::fs::remove_file(bin.join("summon-sidecar")).unwrap();
    assert!(landed(home, &stage, linux).is_err());
    std::fs::write(bin.join("summon-sidecar"), "new sidecar").unwrap();
    assert_eq!(landed(home, &stage, linux), Ok(()));
    // darwin 的清单还有 Menu Bar:暂存里缺它就证不了换全了。
    assert!(landed(home, &stage, Some("darwin-arm64")).is_err());
    // 认不出平台:证不了。
    assert!(landed(home, &stage, None).is_err());
    // 暂存里有清单之外的文件:不算。
    std::fs::write(stage.join("pkg/extra"), "x").unwrap();
    assert!(landed(home, &stage, linux).unwrap_err().contains("extra"));
    std::fs::remove_file(stage.join("pkg/extra")).unwrap();
    // 暂存里缺清单上的文件(落点里那份对得上也不算):证不了。
    std::fs::remove_file(stage.join("pkg/summon-sidecar")).unwrap();
    assert!(landed(home, &stage, linux).is_err());
    // 暂存里没有新版:证不了。
    std::fs::remove_dir_all(stage.join("pkg")).unwrap();
    assert!(landed(home, &stage, linux).is_err());
}

/// 演练来源只在本机:回环 http 跳到远端(哪怕是 https)就拒,在连远端之前;跳到本机照常。
#[test]
fn a_rehearsal_source_never_follows_a_redirect_off_this_machine() {
    use crate::setup::github_upgrade::source::{loopback_redirect_allowed, redirect_allowed};
    let url = |text: &str| reqwest::Url::parse(text).unwrap();
    assert!(loopback_redirect_allowed(&url("http://127.0.0.1:1/x")));
    assert!(loopback_redirect_allowed(&url("https://localhost/x")));
    assert!(!loopback_redirect_allowed(&url("https://example.com/x")));
    assert!(!loopback_redirect_allowed(&url("http://user@127.0.0.1/x")));
    // 人当面给的来源照旧可以跟到 https 远端。
    assert!(redirect_allowed(
        &url("http://127.0.0.1/r"),
        &url("https://example.com/x")
    ));

    let fixture = Fixture::new();
    let base = gh::serve(fixture.release.clone(), &[]);
    let mut away = runner(
        &fixture,
        Drill::Loopback(format!("{base}/away-https")),
        None,
    );
    let started = std::time::Instant::now();
    let state = check(&mut away, &fixture, LATER);
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "跳到远端之前就拒绝,不去连它"
    );
    let event = state.last_event.unwrap().text;
    assert!(event.contains("下个周期再查"), "{event}");
    assert!(state.pending.is_none() && state.held.is_none());

    // 跳到本机:照常取到。
    let mut nearby = runner(&fixture, Drill::Loopback(format!("{base}/r")), None);
    assert!(check(&mut nearby, &fixture, LATER).pending.is_some());
}

/// 自成一组、命令行像守望进程的进程。
fn fake_watcher() -> std::process::Child {
    use std::os::unix::process::CommandExt as _;
    std::process::Command::new("/bin/sh")
        .args(["-c", "sleep 30; :", "auto-upgrade-watch", "--entry", "x"])
        .process_group(0)
        .spawn()
        .unwrap()
}

/// 换代成了:新一代起来对账时撤下待换代记录,并清掉它的暂存;守望进程还在时暂存留着(它要拿暂存
/// 核对落点)。旧版留下的旧名字 pid 文件随痕迹一并清掉。
#[test]
fn a_finished_swap_clears_its_stage_once_the_watcher_is_gone() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().to_path_buf();
    let stage = make_stage(&home, "auto-upgrade-stage.done");
    let store = Store::at_home(&home);
    store
        .update(|state| state.pending = Some(pending_at(Some(stage.clone()))))
        .unwrap();
    let run = home.join(".summon/run");
    let mut watcher = fake_watcher();
    std::fs::write(
        run.join("auto-upgrade.watcher.pid"),
        watcher.id().to_string(),
    )
    .unwrap();

    Runner::new(home.clone(), "0.7.0").reconcile();
    assert!(store.load().unwrap().pending.is_none(), "换代成了");
    assert!(stage.exists(), "守望进程还在,暂存留着");

    // 整组结束:sh 底下的 sleep 也要一起收掉。
    let _ = nix::sys::signal::killpg(
        nix::unistd::Pid::from_raw(watcher.id() as i32),
        nix::sys::signal::Signal::SIGKILL,
    );
    let _ = watcher.wait();
    std::fs::create_dir_all(run.join("auto-upgrade-previous")).unwrap();
    for legacy in ["auto-upgrade.npm.pid", "auto-upgrade.restore.pid"] {
        std::fs::write(run.join(legacy), "1").unwrap();
    }
    Runner::new(home.clone(), "0.7.0").reconcile();
    assert!(!stage.exists(), "守望进程不在了,暂存清掉");
    for legacy in ["auto-upgrade.npm.pid", "auto-upgrade.restore.pid"] {
        assert!(
            !run.join(legacy).exists(),
            "更早版本退回时写的 pid 文件一并清掉:{legacy}"
        );
    }
}

fn doctor(fixture_home: &Path, from: &str, event: Option<Event>) -> String {
    Store::at_home(fixture_home)
        .update(|state| state.last_event = event)
        .unwrap();
    let cfg = Config {
        upgrade: policy(from),
        ..Config::default()
    };
    doctor_lines_at(&cfg, fixture_home).join("\n")
}

/// doctor:写明渠道、标出演练来源;同一原因接连出现时写出次数;还写着停用的 npm 渠道时提示删掉。
#[test]
fn doctor_names_the_channel_the_rehearsal_source_and_repeats() {
    if release::platform(std::env::consts::OS, std::env::consts::ARCH).is_none() {
        return;
    }
    let fixture = Fixture::new();
    let from = fixture.release.display().to_string();
    let lines = doctor(
        &fixture.home,
        &from,
        Some(Event {
            at_secs: PUBLISHED,
            text: "取不到".into(),
            count: 3,
        }),
    );
    assert!(lines.contains("GitHub Releases"), "{lines}");
    assert!(lines.contains(&from) && lines.contains("演练"), "{lines}");
    assert!(lines.contains("连续 3 次"), "{lines}");
    assert!(!lines.contains("已停用"), "{lines}");

    // 配置里还写着停用的 npm 渠道:照样是 GitHub Releases,多一行提示删掉它。
    let cfg = Config {
        upgrade: Upgrade {
            channel: Some("npm".into()),
            ..policy(&from)
        },
        ..Config::default()
    };
    let lines = doctor_lines_at(&cfg, &fixture.home).join("\n");
    assert!(lines.contains("GitHub Releases"), "{lines}");
    assert!(lines.contains("upgrade.channel: npm 已停用"), "{lines}");
    assert!(!lines.contains("不生效"), "{lines}");

    let lines = doctor(&fixture.home, "http://example.com/r", None);
    assert!(
        lines.contains("不生效") && lines.contains("upgrade.from"),
        "{lines}"
    );
}

/// 两种构建里运行期发布都按退回名单拒绝:换代后退回过的版本不发布进(非空的)落点。
#[test]
fn a_rolled_back_version_is_never_published_into_the_install() {
    use crate::setup::self_install::{tests as installs, Daemon};
    use summon_core::install_stamp::Installed;
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let source = installs::source_dir(tmp.path(), "release");
    installs::installed_as(&home, &gh::old_build());
    Store::at_home(&home)
        .update(|state| state.rolled_back.push("0.7.0".into()))
        .unwrap();

    let outcome = self_install::publish_over(
        &source.join("summon"),
        &home,
        &gh::new_build(),
        Installed::Known(gh::old_build()),
        &Daemon::Stopped,
    )
    .unwrap();

    assert_eq!(outcome.published, None);
    assert!(
        outcome
            .notice
            .as_deref()
            .is_some_and(|n| n.contains("退回过")),
        "{:?}",
        outcome.notice
    );
    let target = summon_core::companion::install_target(&home);
    assert_eq!(std::fs::read_to_string(target).unwrap(), "summon-落点");
}
