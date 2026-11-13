//! GitHub Releases 渠道的纯逻辑:manifest、本平台的包、下载核对、解包、构建戳与签名团队的判定。
//!
//! Release 的形状由 `scripts/github-release/` 定:附件 `summon-<平台>.tar.gz` 与 `manifest.json`,
//! 后者汇总各平台包里 summon 自报的构建戳、文件名、字节数与 SHA-256。升级只认 manifest:它没列出
//! 的平台就当这一版对该平台不存在。
//!
//! 这里不联网、不起进程、不碰落点:字节流经 [`Source`] 取(HTTP 与本地目录两种实现在 `summon`
//! 里),问暂存那份的构建戳、跑 codesign、发布进落点也在那边做。这里只判「这些字节能不能用」,
//! 任何一条不成立就拒绝,调用方据此不碰落点。
//!
//! 校验能防的是传输损坏与装错平台;manifest 与包同源,防不了仓库或账号被接管。

use crate::install_stamp::{self, BuildInfo};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;

/// manifest 的附件名。
pub const MANIFEST: &str = "manifest.json";
/// manifest 的大小上限。它只有几百字节,超过上限的不是 manifest。
pub const MANIFEST_LIMIT: u64 = 256 * 1024;
/// manifest 记的平台包大小上限。现在的包约 20 MB;超过上限的记录不可信,不为它去下载、占盘。
pub const PACKAGE_LIMIT: u64 = 1024 * 1024 * 1024;
/// 包里单个文件解出来的上限。包已按 manifest 核过哈希,这道只防损坏的包无限解压。
const MEMBER_LIMIT: u64 = 1024 * 1024 * 1024;

/// 一次核对或下载为什么没成。
#[derive(Debug, thiserror::Error)]
pub enum ReleaseError {
    /// 取不到:地址不通、文件不存在、下载中断。
    #[error("{0}")]
    Fetch(String),
    /// 取到了却对不上:manifest 不合法、字节数或哈希不符、包成员不对、构建戳或签名不符。
    #[error("{0}")]
    Rejected(String),
    /// 本机写暂存失败。
    #[error("{0}")]
    Local(String),
}

/// Release 附件的来源:按附件名取字节流。
pub trait Source {
    /// 打开名为 `name` 的附件(`manifest.json` 或某个平台包)。
    fn open(&self, name: &str) -> Result<Box<dyn Read>, ReleaseError>;
    /// 这个附件在哪,给人看。
    fn locate(&self, name: &str) -> String;
}

/// 发布包的平台名(`linux-x64` 这类);不在发布范围内的组合返回 `None`。与
/// `scripts/github-release/lib.sh` 的 `summon_release_platform` 同一张表。darwin-x64 不发。
pub fn platform(os: &str, arch: &str) -> Option<&'static str> {
    match (os, arch) {
        ("linux", "x86_64") => Some("linux-x64"),
        ("linux", "aarch64") => Some("linux-arm64"),
        ("macos", "aarch64") => Some("darwin-arm64"),
        _ => None,
    }
}

/// 平台包里恰有的文件,与 `lib.sh` 的 `summon_release_files` 同一张表。
pub fn package_files(platform: &str) -> Option<&'static [&'static str]> {
    use crate::companion::{MENUBAR, SIDECAR, SUMMON};
    match platform {
        "linux-x64" | "linux-arm64" => Some(&[SUMMON, SIDECAR]),
        "darwin-arm64" => Some(&[SUMMON, SIDECAR, MENUBAR]),
        _ => None,
    }
}

/// 平台包的附件名。附件名不带版本号,`releases/latest/download/<附件名>` 永远指向最新版。
pub fn asset_name(platform: &str) -> String {
    format!("summon-{platform}.tar.gz")
}

/// `manifest.json`,由 `scripts/github-release/manifest.py` 生成。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Manifest {
    pub version: String,
    /// 各平台包构建戳里最长的那个短提交号。
    pub commit: String,
    pub commit_time: u64,
    #[serde(default)]
    pub published_at: Option<String>,
    /// 平台名 → 该平台的包。
    pub assets: BTreeMap<String, Asset>,
}

/// manifest 里一个平台的包。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Asset {
    pub name: String,
    pub size: u64,
    pub sha256: String,
}

impl Manifest {
    /// 这一版的构建戳,与落点的构建戳按 `install_stamp` 的判据比新旧。
    pub fn build(&self) -> BuildInfo {
        BuildInfo {
            version: self.version.clone(),
            commit: self.commit.clone(),
            commit_time: Some(self.commit_time),
        }
    }

    pub fn asset(&self, platform: &str) -> Option<&Asset> {
        self.assets.get(platform)
    }

    /// 发布时刻(unix 秒)。`manifest.py` 只写 `YYYY-MM-DDTHH:MM:SSZ`(UTC)这一种形状;缺失、别的
    /// 写法、早于提交时间的都当作不知道 —— 自动升级据此不采用这一版,宁可等也不按猜的时间算。
    pub fn published_secs(&self) -> Option<u64> {
        let text = self.published_at.as_deref()?;
        let shaped = text.len() == 20
            && text.bytes().enumerate().all(|(at, b)| match at {
                4 | 7 => b == b'-',
                10 => b == b'T',
                13 | 16 => b == b':',
                19 => b == b'Z',
                _ => b.is_ascii_digit(),
            });
        if !shaped {
            return None;
        }
        let at = chrono::NaiveDateTime::parse_from_str(text, "%Y-%m-%dT%H:%M:%SZ").ok()?;
        let secs = u64::try_from(at.and_utc().timestamp()).ok()?;
        (secs >= self.commit_time).then_some(secs)
    }
}

fn is_hex(text: &str) -> bool {
    text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

fn rejected(text: String) -> ReleaseError {
    ReleaseError::Rejected(text)
}

/// 解析并校验 manifest。版本须是纯 `x.y.z`、提交是 7–40 位小写十六进制、提交时间为正,每个平台
/// 的附件名、字节数与 SHA-256 都要成形;不认识的平台照样校验形状,只是用不上。
pub fn parse_manifest(bytes: &[u8]) -> Result<Manifest, ReleaseError> {
    let manifest: Manifest = serde_json::from_slice(bytes)
        .map_err(|error| rejected(summon_i18n::format!("r91705d5424")))?;
    let Manifest {
        version,
        commit,
        commit_time,
        assets,
        ..
    } = &manifest;
    if !install_stamp::is_release_version(version) {
        return Err(rejected(summon_i18n::format!("r702f97c63b")));
    }
    if !(7..=40).contains(&commit.len()) || !is_hex(commit) {
        return Err(rejected(summon_i18n::format!("rbb7798aab9")));
    }
    if *commit_time == 0 {
        return Err(rejected(summon_i18n::text!("r7f99dff60f").to_string()));
    }
    for (platform, asset) in assets {
        let shaped = asset.name == asset_name(platform)
            && asset.size > 0
            && asset.size <= PACKAGE_LIMIT
            && asset.sha256.len() == 64
            && is_hex(&asset.sha256);
        if !shaped {
            return Err(rejected(summon_i18n::format!("r9d45b298d3")));
        }
    }
    Ok(manifest)
}

/// 取 manifest:读到上限为止,超过上限即拒绝。
pub fn read_manifest(source: &dyn Source) -> Result<Manifest, ReleaseError> {
    let mut reader = source.open(MANIFEST)?.take(MANIFEST_LIMIT + 1);
    let mut bytes = Vec::new();
    reader.read_to_end(&mut bytes).map_err(|error| {
        let at = source.locate(MANIFEST);
        ReleaseError::Fetch(summon_i18n::format!("r69de4318df"))
    })?;
    if bytes.len() as u64 > MANIFEST_LIMIT {
        return Err(rejected(summon_i18n::format!(
            "rbd53720247",
            limit = MANIFEST_LIMIT
        )));
    }
    parse_manifest(&bytes)
}

/// 把 `asset` 下载到 `dest`(新建,已存在即失败),边下边数字节、算 SHA-256:超过 manifest 记的
/// 大小当场中止,结束时字节数与哈希都要对上。失败时 `dest` 可能留下半截,由调用方连同暂存目录删掉。
pub fn download(source: &dyn Source, asset: &Asset, dest: &Path) -> Result<(), ReleaseError> {
    let at = source.locate(&asset.name);
    let mut reader = source.open(&asset.name)?;
    let local = |error: std::io::Error| {
        ReleaseError::Local(summon_i18n::format!("rb560e7ab93", path = dest.display()))
    };
    let mut file = std::fs::File::create_new(dest).map_err(local)?;
    let mut digest = ring::digest::Context::new(&ring::digest::SHA256);
    let mut received: u64 = 0;
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(ReleaseError::Fetch(summon_i18n::format!("r69de4318df"))),
        };
        received += n as u64;
        if received > asset.size {
            return Err(rejected(summon_i18n::format!(
                "r42332de87f",
                size = asset.size
            )));
        }
        digest.update(&buf[..n]);
        file.write_all(&buf[..n]).map_err(local)?;
    }
    file.flush().map_err(local)?;
    if received != asset.size {
        return Err(rejected(summon_i18n::format!(
            "rc60af94885",
            size = asset.size
        )));
    }
    let actual = hex(digest.finish().as_ref());
    if actual != asset.sha256 {
        return Err(rejected(summon_i18n::format!(
            "r6f28a859a5",
            expected = asset.sha256
        )));
    }
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// 把平台包解进 `dest`(须是空目录)。成员必须恰为该平台应有的文件:平铺(名字里没有路径分隔符)、
/// 普通文件(不收链接、目录、设备),不多不少不重复;落盘一律 0755。任何一条不成立即拒绝,已解出的
/// 文件由调用方连同暂存目录删掉。
pub fn unpack(tarball: &Path, platform: &str, dest: &Path) -> Result<(), ReleaseError> {
    let expected =
        package_files(platform).ok_or_else(|| rejected(summon_i18n::format!("r1903c6ed4d")))?;
    let file = std::fs::File::open(tarball).map_err(|error| {
        ReleaseError::Local(summon_i18n::format!(
            "r27b15d1fb3",
            path = tarball.display()
        ))
    })?;
    let broken = |error: std::io::Error| rejected(summon_i18n::format!("r6e3649cedf"));
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(file));
    let mut seen: Vec<&str> = Vec::new();
    for entry in archive.entries().map_err(broken)? {
        let mut entry = entry.map_err(broken)?;
        let raw = entry.path_bytes().into_owned();
        let name = String::from_utf8_lossy(&raw).into_owned();
        let Some(&member) = expected
            .iter()
            .find(|&&want| want.as_bytes() == raw.as_slice())
        else {
            return Err(rejected(summon_i18n::format!("r59a0678759")));
        };
        if seen.contains(&member) {
            return Err(rejected(summon_i18n::format!("r703dfcb5b7")));
        }
        if entry.header().entry_type() != tar::EntryType::Regular {
            return Err(rejected(summon_i18n::format!("r0c03ca9ae5")));
        }
        // PAX 扩展头可以覆盖头部里的大小,以 tar 读出的实际大小为准。
        let size = entry.size();
        if size > MEMBER_LIMIT {
            return Err(rejected(summon_i18n::format!("r869fa2d37b")));
        }
        let path = dest.join(member);
        let local = |error: std::io::Error| {
            ReleaseError::Local(summon_i18n::format!("rb560e7ab93", path = path.display()))
        };
        let mut out = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o755)
            .open(&path)
            .map_err(local)?;
        let written = std::io::copy(&mut (&mut entry).take(size), &mut out).map_err(broken)?;
        if written != size {
            return Err(rejected(summon_i18n::format!("rb86b7ed986")));
        }
        // 创建时的权限受 umask 影响,落盘后再明确设成 0755。
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).map_err(local)?;
        seen.push(member);
    }
    let missing: Vec<&str> = expected
        .iter()
        .copied()
        .filter(|name| !seen.contains(name))
        .collect();
    if !missing.is_empty() {
        let missing = missing.join(", ");
        return Err(rejected(summon_i18n::format!("r35aa66fad6")));
    }
    Ok(())
}

/// `summon install-stamp print` 的输出(一行 JSON)→ 构建戳。
pub fn parse_reported(stdout: &[u8]) -> Option<BuildInfo> {
    let text = std::str::from_utf8(stdout).ok()?;
    serde_json::from_str(text.lines().next()?.trim()).ok()
}

/// 包里 summon 自报的构建戳必须就是 manifest 记的:版本相同、提交按前缀相同(短提交号的长度随
/// 仓库对象数变,同 `install_stamp` 的判据)、提交时间相同。
pub fn check_reported(reported: &BuildInfo, manifest: &Manifest) -> Result<(), ReleaseError> {
    let matches = reported.version == manifest.version
        && install_stamp::same_commit(&reported.commit, &manifest.commit)
        && reported.commit_time == Some(manifest.commit_time);
    if matches {
        return Ok(());
    }
    let (reported, expected) = (reported.describe(), manifest.build().describe());
    Err(rejected(summon_i18n::format!("r6549b229dd")))
}

/// `codesign -dv` 的详情(它写在 stderr)里的签名团队;ad-hoc 签名报 `not set`,算没有团队。
pub fn team_identifier(details: &str) -> Option<String> {
    details
        .lines()
        .find_map(|line| line.trim().strip_prefix("TeamIdentifier="))
        .map(str::trim)
        .filter(|team| !team.is_empty() && *team != "not set")
        .map(str::to_string)
}

/// 新包的签名团队:每个文件都要有团队且彼此相同;当前在跑的这份有团队时还要与它相同。返回团队。
/// `new` 是 `(文件名, 它的团队)`。
pub fn check_teams(
    current: Option<&str>,
    new: &[(&str, Option<String>)],
) -> Result<String, ReleaseError> {
    let mut team: Option<&str> = None;
    for (name, found) in new {
        let Some(found) = found.as_deref() else {
            return Err(rejected(summon_i18n::format!("r16fdd7c23b")));
        };
        match team {
            Some(first) if first != found => {
                return Err(rejected(summon_i18n::format!("r10c3d9da92")));
            }
            _ => team = Some(found),
        }
    }
    let team = team.ok_or_else(|| rejected(summon_i18n::text!("r9de1c6915d").to_string()))?;
    if let Some(current) = current.filter(|current| *current != team) {
        return Err(rejected(summon_i18n::format!("rf1ffdf8290")));
    }
    Ok(team.to_string())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;

    /// 本地目录作来源。
    pub(crate) struct Dir(pub std::path::PathBuf);

    impl Source for Dir {
        fn open(&self, name: &str) -> Result<Box<dyn Read>, ReleaseError> {
            std::fs::File::open(self.0.join(name))
                .map(|file| Box::new(file) as Box<dyn Read>)
                .map_err(|error| ReleaseError::Fetch(format!("{name}: {error}")))
        }
        fn locate(&self, name: &str) -> String {
            self.0.join(name).display().to_string()
        }
    }

    /// 读到 `limit` 字节后报错的来源:下载中途断开。
    struct Broken {
        bytes: Vec<u8>,
        limit: usize,
    }

    impl Source for Broken {
        fn open(&self, _name: &str) -> Result<Box<dyn Read>, ReleaseError> {
            let head = std::io::Cursor::new(self.bytes[..self.limit].to_vec());
            Ok(Box::new(head.chain(Failing)))
        }
        fn locate(&self, name: &str) -> String {
            name.to_string()
        }
    }

    struct Failing;

    impl Read for Failing {
        fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
            Err(std::io::Error::new(
                std::io::ErrorKind::ConnectionReset,
                "连接被重置",
            ))
        }
    }

    /// tar 成员:名字、内容、类型。
    pub(crate) struct Member<'a> {
        pub name: &'a str,
        pub body: &'a [u8],
        pub kind: tar::EntryType,
    }

    pub(crate) fn file<'a>(name: &'a str, body: &'a [u8]) -> Member<'a> {
        Member {
            name,
            body,
            kind: tar::EntryType::Regular,
        }
    }

    /// 打一个 tar.gz;名字原样写进头部,不经 tar crate 的路径检查(要能造出带 `../` 的坏包)。
    pub(crate) fn tarball(members: &[Member<'_>]) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        for member in members {
            let mut header = tar::Header::new_ustar();
            let name = member.name.as_bytes();
            header.as_ustar_mut().unwrap().name[..name.len()].copy_from_slice(name);
            header.set_size(member.body.len() as u64);
            header.set_mode(0o644);
            header.set_entry_type(member.kind);
            if member.kind == tar::EntryType::Symlink {
                header.set_link_name("/bin/sh").unwrap();
                header.set_size(0);
            }
            header.set_cksum();
            let body: &[u8] = if member.kind == tar::EntryType::Symlink {
                &[]
            } else {
                member.body
            };
            builder.append(&header, body).unwrap();
        }
        let tar = builder.into_inner().unwrap();
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gz.write_all(&tar).unwrap();
        gz.finish().unwrap()
    }

    pub(crate) fn sha256(bytes: &[u8]) -> String {
        hex(ring::digest::digest(&ring::digest::SHA256, bytes).as_ref())
    }

    pub(crate) fn manifest_json(version: &str, commit: &str, time: u64, assets: &str) -> String {
        format!(
            r#"{{"version":"{version}","commit":"{commit}","commit_time":{time},"published_at":"2026-01-02T03:04:05Z","assets":{{{assets}}}}}"#
        )
    }

    pub(crate) fn asset_json(platform: &str, bytes: &[u8]) -> String {
        format!(
            r#""{platform}":{{"name":"summon-{platform}.tar.gz","size":{},"sha256":"{}"}}"#,
            bytes.len(),
            sha256(bytes)
        )
    }

    fn sample() -> Manifest {
        let body = b"package";
        parse_manifest(
            manifest_json("0.7.0", "abcdef12", 2_000, &asset_json("linux-x64", body)).as_bytes(),
        )
        .unwrap()
    }

    #[test]
    fn platforms_match_the_release_scripts() {
        assert_eq!(platform("linux", "x86_64"), Some("linux-x64"));
        assert_eq!(platform("linux", "aarch64"), Some("linux-arm64"));
        assert_eq!(platform("macos", "aarch64"), Some("darwin-arm64"));
        assert_eq!(platform("macos", "x86_64"), None, "darwin-x64 不发");
        assert_eq!(platform("windows", "x86_64"), None);
        assert_eq!(
            package_files("darwin-arm64").unwrap(),
            ["summon", "summon-sidecar", "summon-menubar"]
        );
        assert_eq!(
            package_files("linux-arm64").unwrap(),
            ["summon", "summon-sidecar"]
        );
        assert_eq!(asset_name("linux-x64"), "summon-linux-x64.tar.gz");
        // 与打包脚本的平台表、文件表逐字一致。
        let lib = include_str!("../../../scripts/github-release/lib.sh");
        for (uname, name) in [
            ("Linux/x86_64", "linux-x64"),
            ("Linux/aarch64", "linux-arm64"),
            ("Darwin/arm64", "darwin-arm64"),
        ] {
            assert!(
                lib.contains(&format!("{uname} ")) && lib.contains(&format!("'{name}\\n'")),
                "{uname} → {name}"
            );
        }
        assert!(lib.contains(r"linux-x64 | linux-arm64) printf 'summon\nsummon-sidecar\n'"));
        assert!(lib.contains(r"darwin-arm64) printf 'summon\nsummon-sidecar\nsummon-menubar\n'"));
    }

    /// 发布时间只认 `manifest.py` 写的那一种形状,缺失、别的写法、早于提交时间都当作不知道。
    #[test]
    fn the_publish_time_is_read_only_in_the_shape_the_release_scripts_write() {
        let with = |published_at: Option<&str>| Manifest {
            published_at: published_at.map(str::to_string),
            ..sample()
        };
        assert_eq!(
            with(Some("2026-01-02T03:04:05Z")).published_secs(),
            Some(1_767_323_045)
        );
        for bad in [
            None,
            Some(""),
            Some("2026-01-02T03:04:05+00:00"),
            Some("2026-01-02 03:04:05Z"),
            Some("2026-01-02T03:04:05.000Z"),
            Some("2026-1-02T03:04:05Z"),
            Some("2026-13-02T03:04:05Z"),
            Some("2026-02-30T03:04:05Z"),
            Some("１９７0-01-02T03:04:05Z"),
        ] {
            assert_eq!(with(bad).published_secs(), None, "{bad:?}");
        }
        let early = Manifest {
            commit_time: 1_767_323_046,
            ..with(Some("2026-01-02T03:04:05Z"))
        };
        assert_eq!(early.published_secs(), None, "早于提交时间的发布时间不可信");
        assert_eq!(
            Manifest {
                commit_time: 1_767_323_045,
                ..early
            }
            .published_secs(),
            Some(1_767_323_045)
        );
    }

    #[test]
    fn a_manifest_from_the_release_scripts_parses() {
        let manifest = sample();
        assert_eq!(
            manifest.build(),
            BuildInfo {
                version: "0.7.0".into(),
                commit: "abcdef12".into(),
                commit_time: Some(2_000),
            }
        );
        assert_eq!(manifest.asset("linux-x64").unwrap().size, 7);
        assert!(manifest.asset("darwin-arm64").is_none());
    }

    #[test]
    fn malformed_manifests_are_rejected() {
        let asset = asset_json("linux-x64", b"package");
        let bad_sha = asset.replace(&sha256(b"package"), "zz");
        let wrong_name = asset.replace("summon-linux-x64.tar.gz", "../evil.tar.gz");
        let too_large = asset.replace(r#""size":7"#, &format!(r#""size":{}"#, PACKAGE_LIMIT + 1));
        let cases = [
            ("not json".to_string(), "JSON"),
            (r#"{"version":"0.7.0"}"#.to_string(), "JSON"),
            (manifest_json("0.7", "abcdef12", 2_000, &asset), "0.7"),
            (
                manifest_json("0.7.0-rc.1", "abcdef12", 2_000, &asset),
                "0.7.0-rc.1",
            ),
            (manifest_json("0.7.0", "abc", 2_000, &asset), "abc"),
            (
                manifest_json("0.7.0", "ABCDEF12", 2_000, &asset),
                "ABCDEF12",
            ),
            (manifest_json("0.7.0", "abcdef12", 0, &asset), "commit_time"),
            (
                manifest_json("0.7.0", "abcdef12", 2_000, &bad_sha),
                "linux-x64",
            ),
            (
                manifest_json("0.7.0", "abcdef12", 2_000, &wrong_name),
                "linux-x64",
            ),
            (
                manifest_json("0.7.0", "abcdef12", 2_000, &too_large),
                "linux-x64",
            ),
            (
                r#"{"version":"0.7.0","commit":"abcdef12","commit_time":-1,"assets":{}}"#
                    .to_string(),
                "JSON",
            ),
        ];
        for (text, want) in cases {
            let error = parse_manifest(text.as_bytes()).unwrap_err();
            assert!(matches!(error, ReleaseError::Rejected(_)), "{text}");
            assert!(error.to_string().contains(want), "{text}: {error}");
        }
    }

    #[test]
    fn an_oversized_manifest_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let mut text = manifest_json("0.7.0", "abcdef12", 2_000, "");
        text.push_str(&" ".repeat(MANIFEST_LIMIT as usize));
        std::fs::write(tmp.path().join(MANIFEST), text).unwrap();
        let error = read_manifest(&Dir(tmp.path().to_path_buf())).unwrap_err();
        assert!(error.to_string().contains("上限"), "{error}");
    }

    fn asset_for(bytes: &[u8]) -> Asset {
        Asset {
            name: asset_name("linux-x64"),
            size: bytes.len() as u64,
            sha256: sha256(bytes),
        }
    }

    #[test]
    fn a_download_must_match_the_size_and_hash() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        std::fs::create_dir(&src).unwrap();
        let body = b"0123456789".repeat(10_000);
        std::fs::write(src.join(asset_name("linux-x64")), &body).unwrap();
        let source = Dir(src);

        let ok = tmp.path().join("ok");
        download(&source, &asset_for(&body), &ok).unwrap();
        assert_eq!(std::fs::read(&ok).unwrap(), body);

        let mut longer = asset_for(&body);
        longer.size += 1;
        let error = download(&source, &longer, &tmp.path().join("short")).unwrap_err();
        assert!(error.to_string().contains("字节数"), "{error}");

        let mut smaller = asset_for(&body);
        smaller.size -= 1;
        let error = download(&source, &smaller, &tmp.path().join("long")).unwrap_err();
        assert!(error.to_string().contains("超过"), "{error}");

        let mut other = asset_for(&body);
        other.sha256 = sha256(b"something else");
        let error = download(&source, &other, &tmp.path().join("hash")).unwrap_err();
        assert!(error.to_string().contains("SHA-256"), "{error}");

        // 已存在的目标不覆盖。
        assert!(download(&source, &asset_for(&body), &ok).is_err());
    }

    #[test]
    fn an_interrupted_download_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let body = vec![7u8; 200_000];
        let source = Broken {
            bytes: body.clone(),
            limit: 100_000,
        };
        let error = download(&source, &asset_for(&body), &tmp.path().join("x")).unwrap_err();
        assert!(matches!(error, ReleaseError::Fetch(_)), "{error}");
        assert!(error.to_string().contains("中断"), "{error}");
    }

    fn unpack_bytes(bytes: &[u8], platform: &str) -> (tempfile::TempDir, Result<(), ReleaseError>) {
        let tmp = tempfile::tempdir().unwrap();
        let tarball = tmp.path().join("pkg.tar.gz");
        std::fs::write(&tarball, bytes).unwrap();
        let dest = tmp.path().join("out");
        std::fs::create_dir(&dest).unwrap();
        let result = unpack(&tarball, platform, &dest);
        (tmp, result)
    }

    #[test]
    fn unpacks_exactly_the_platform_files_as_0755() {
        let bytes = tarball(&[file("summon", b"cli"), file("summon-sidecar", b"sidecar")]);
        let (tmp, result) = unpack_bytes(&bytes, "linux-x64");
        result.unwrap();
        let out = tmp.path().join("out");
        assert_eq!(std::fs::read(out.join("summon")).unwrap(), b"cli");
        assert_eq!(
            std::fs::read(out.join("summon-sidecar")).unwrap(),
            b"sidecar"
        );
        for name in ["summon", "summon-sidecar"] {
            let mode = std::fs::metadata(out.join(name)).unwrap().mode();
            assert_eq!(mode & 0o777, 0o755, "{name}");
        }
        assert_eq!(std::fs::read_dir(&out).unwrap().count(), 2);
    }

    #[test]
    fn packages_with_the_wrong_members_are_rejected() {
        let link = Member {
            name: "summon-sidecar",
            body: b"",
            kind: tar::EntryType::Symlink,
        };
        let hard = Member {
            name: "summon-sidecar",
            body: b"",
            kind: tar::EntryType::Link,
        };
        let dir = Member {
            name: "summon-sidecar",
            body: b"",
            kind: tar::EntryType::Directory,
        };
        let cases: [(&str, Vec<u8>, &str); 9] = [
            (
                "多一个",
                tarball(&[
                    file("summon", b"a"),
                    file("summon-sidecar", b"b"),
                    file("README", b"c"),
                ]),
                "README",
            ),
            ("少一个", tarball(&[file("summon", b"a")]), "summon-sidecar"),
            (
                "带路径",
                tarball(&[file("bin/summon", b"a"), file("summon-sidecar", b"b")]),
                "bin/summon",
            ),
            (
                "上一级",
                tarball(&[file("../summon", b"a"), file("summon-sidecar", b"b")]),
                "../summon",
            ),
            (
                "点前缀",
                tarball(&[file("./summon", b"a"), file("summon-sidecar", b"b")]),
                "./summon",
            ),
            (
                "软链",
                tarball(&[file("summon", b"a"), link]),
                "summon-sidecar",
            ),
            (
                "硬链",
                tarball(&[file("summon", b"a"), hard]),
                "summon-sidecar",
            ),
            (
                "目录",
                tarball(&[file("summon", b"a"), dir]),
                "summon-sidecar",
            ),
            (
                "重复",
                tarball(&[
                    file("summon", b"a"),
                    file("summon", b"a"),
                    file("summon-sidecar", b"b"),
                ]),
                "summon",
            ),
        ];
        for (what, bytes, want) in cases {
            let (tmp, result) = unpack_bytes(&bytes, "linux-x64");
            let error = result.expect_err(what);
            assert!(
                matches!(error, ReleaseError::Rejected(_)),
                "{what}: {error}"
            );
            assert!(error.to_string().contains(want), "{what}: {error}");
            // 解包只写进给它的目录。
            assert!(!tmp.path().join("summon").exists(), "{what}");
        }
        let (_tmp, result) = unpack_bytes(b"not a tarball", "linux-x64");
        assert!(matches!(result, Err(ReleaseError::Rejected(_))));
        // darwin 包少了 Menu Bar。
        let bytes = tarball(&[file("summon", b"a"), file("summon-sidecar", b"b")]);
        let (_tmp, result) = unpack_bytes(&bytes, "darwin-arm64");
        assert!(result.unwrap_err().to_string().contains("summon-menubar"));
    }

    #[test]
    fn the_reported_stamp_must_be_the_manifest_build() {
        let manifest = sample();
        let reported = |version: &str, commit: &str, time: Option<u64>| BuildInfo {
            version: version.into(),
            commit: commit.into(),
            commit_time: time,
        };
        check_reported(&reported("0.7.0", "abcdef12", Some(2_000)), &manifest).unwrap();
        // 短提交号长度不同、前缀相同:同一个提交。
        check_reported(&reported("0.7.0", "abcdef1", Some(2_000)), &manifest).unwrap();
        for bad in [
            reported("0.7.1", "abcdef12", Some(2_000)),
            reported("0.7.0", "abcdef13", Some(2_000)),
            reported("0.7.0", "abcdef12", Some(2_001)),
            reported("0.7.0", "abcdef12", None),
            reported("0.7.0", "unknown", Some(2_000)),
        ] {
            let error = check_reported(&bad, &manifest).unwrap_err();
            assert!(error.to_string().contains("构建戳"), "{bad:?}: {error}");
        }
        assert_eq!(
            parse_reported(br#"{"version":"0.7.0","commit":"abcdef12","commit_time":2000}"#),
            Some(reported("0.7.0", "abcdef12", Some(2_000)))
        );
        assert_eq!(parse_reported(b"summon 0.7.0"), None);
        assert_eq!(parse_reported(b""), None);
    }

    #[test]
    fn reads_the_team_from_codesign_details() {
        let signed = "Executable=/x/summon\nIdentifier=com.summon.cli\nAuthority=Developer ID Application: Example (TEAM123456)\nTeamIdentifier=TEAM123456\n";
        assert_eq!(team_identifier(signed).as_deref(), Some("TEAM123456"));
        let adhoc = "Executable=/x/summon\nSignature=adhoc\nTeamIdentifier=not set\n";
        assert_eq!(team_identifier(adhoc), None);
        assert_eq!(
            team_identifier("/x/summon: code object is not signed at all\n"),
            None
        );
    }

    #[test]
    fn teams_must_be_present_consistent_and_unchanged() {
        let some = |team: &str| Some(team.to_string());
        let trio = |a: Option<String>, b: Option<String>| {
            vec![
                ("summon", some("TEAMA")),
                ("summon-sidecar", a),
                ("summon-menubar", b),
            ]
        };
        assert_eq!(
            check_teams(Some("TEAMA"), &trio(some("TEAMA"), some("TEAMA"))).unwrap(),
            "TEAMA"
        );
        // 当前这份没有团队(源码自建):新包有效且团队一致即可,团队交给调用方打印。
        assert_eq!(
            check_teams(None, &trio(some("TEAMA"), some("TEAMA"))).unwrap(),
            "TEAMA"
        );
        let changed = check_teams(Some("TEAMB"), &trio(some("TEAMA"), some("TEAMA")));
        assert!(changed.unwrap_err().to_string().contains("TEAMB"));
        let mixed = check_teams(None, &trio(some("TEAMA"), some("TEAMC")));
        assert!(mixed.unwrap_err().to_string().contains("TEAMC"));
        let missing = check_teams(None, &trio(some("TEAMA"), None));
        assert!(missing.unwrap_err().to_string().contains("summon-menubar"));
        assert!(check_teams(None, &[]).is_err());
    }
}
