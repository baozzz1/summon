//! 自动升级的 GitHub Releases 渠道:查新版读 Release 的 `manifest.json`,取包、核对复用
//! `summon upgrade` 的那一套([`crate::setup::github_upgrade`]),新版留在
//! `~/.summon/run/auto-upgrade-stage.*` 里等整机空闲换代。
//!
//! 只走 Release 直链,不调 GitHub API。退回不经任何包管理器:新版起不来就放回换代前的落点快照。
//!
//! `upgrade.from` 是演练用的来源,只收绝对路径的本地目录与本机回环地址上的明文 http —— 它不能
//! 成为把来源改到远端的口子:远端明文与任意 https 远端都在联网之前拒绝,macOS 上的签名团队核对照旧。

use super::channel::{Channel, CheckFailed, Fetched, Rechecked};
use super::state::{Pending, Seen};
use crate::setup::github_upgrade::source::{self, DirSource, HttpSource, Redirects};
use crate::setup::github_upgrade::{self, Codesign, Stage, Step, Verify};
use crate::setup::self_install;
use anyhow::{Context, Result};
use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;
use summon_core::companion;
use summon_core::release::{self, Manifest, Source};

/// 自动升级的暂存目录名的前缀(在 `~/.summon/run/` 下)。与 `summon upgrade` 的暂存前缀不同:
/// 待换代的新版要在这里等到整机空闲,不归手动升级的清理规则管。
pub(crate) const STAGE_PREFIX: &str = "auto-upgrade-stage.";

/// 演练用的来源。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Drill {
    Dir(PathBuf),
    /// 本机回环地址上的明文 http。
    Loopback(String),
}

impl Drill {
    /// 认 `upgrade.from`:绝对路径的本地目录,或本机回环地址上的明文 `http://`。别的写法一律不认。
    pub(crate) fn parse(from: &str) -> Result<Self, String> {
        let invalid = || summon_i18n::format!("r68a7cb49cd");
        if let Some(rest) = from.strip_prefix("http://") {
            let url = reqwest::Url::parse(from).map_err(|_| invalid())?;
            if rest.is_empty() || url.scheme() != "http" || !source::is_loopback(&url) {
                return Err(invalid());
            }
            return Ok(Self::Loopback(from.to_string()));
        }
        let path = Path::new(from);
        if path.is_absolute() && path.is_dir() {
            Ok(Self::Dir(path.to_path_buf()))
        } else {
            Err(invalid())
        }
    }

    fn source(&self) -> Result<Box<dyn Source>> {
        Ok(match self {
            Self::Dir(dir) => Box::new(DirSource(dir.clone())),
            // 演练来源只在本机:跳转也只跟到回环地址,不会被引到任何远端。
            Self::Loopback(url) => Box::new(HttpSource::with_redirects(url, Redirects::Loopback)?),
        })
    }

    fn describe(&self) -> String {
        match self {
            Self::Dir(dir) => dir.display().to_string(),
            Self::Loopback(url) => url.clone(),
        }
    }
}

pub(crate) struct GithubChannel {
    pub(super) home: PathBuf,
    pub(super) platform: &'static str,
    pub(super) drill: Option<Drill>,
    pub(super) codesign: Option<Codesign>,
    pub(super) stamp_timeout: Duration,
    /// 这一次检查读到的 manifest;取包时只认它。
    pub(super) manifest: Option<Manifest>,
}

impl GithubChannel {
    /// 本平台在发布范围内、`from` 认得,才走得了这条渠道。
    pub(crate) fn open(home: &Path, from: Option<&str>) -> Result<Self, String> {
        let platform = release::platform(std::env::consts::OS, std::env::consts::ARCH)
            .ok_or_else(|| summon_i18n::text!("rf3c0e98ae6").to_string())?;
        let drill = from.map(str::trim).map(Drill::parse).transpose()?;
        let codesign = Codesign::for_current_exe().map_err(|error| format!("{error:#}"))?;
        Ok(Self {
            home: home.to_path_buf(),
            platform,
            drill,
            codesign,
            stamp_timeout: github_upgrade::STAMP_TIMEOUT,
            manifest: None,
        })
    }

    /// 取 manifest 的来源;`version` 给了就是那个 tag 下的(取包时用,免得读完 manifest 之后
    /// latest 已经挪到下一版,配不上)。演练来源只有一份。
    fn source(&self, version: Option<&str>) -> Result<Box<dyn Source>> {
        match (&self.drill, release_base(version)) {
            (Some(drill), _) => drill.source(),
            (None, base) => Ok(Box::new(HttpSource::new(&base)?)),
        }
    }

    /// 读到的 manifest 正是 `version` 那一版。
    fn manifest_for(&self, version: &str) -> Result<&Manifest> {
        self.manifest
            .as_ref()
            .filter(|manifest| manifest.version == version)
            .ok_or_else(|| anyhow::anyhow!(summon_i18n::format!("r57eb7b9a3c")))
    }
}

impl Channel for GithubChannel {
    fn platform(&self) -> &str {
        self.platform
    }

    /// 取不到、不成形(私有仓要登录时拿到的是 404 或一张网页)都只是这次没查成:下个周期再查,
    /// 同一原因只计次。
    fn latest(&mut self) -> Result<Seen, CheckFailed> {
        let manifest = self
            .source(None)
            .and_then(|source| Ok(release::read_manifest(source.as_ref())?))
            .map_err(|error| CheckFailed {
                text: summon_i18n::format!("ra1c7f9f5c7"),
                repeats: true,
            })?;
        let seen = Seen {
            version: manifest.version.clone(),
            published_secs: manifest.published_secs(),
        };
        self.manifest = Some(manifest);
        Ok(seen)
    }

    fn platform_ready(&mut self, version: &str) -> Result<bool> {
        Ok(self.manifest_for(version)?.asset(self.platform).is_some())
    }

    fn fetch(&mut self, version: &str, log: &Path) -> Result<Fetched> {
        let manifest = self.manifest_for(version)?.clone();
        let new = manifest.build();
        let installed = self_install::installed_build(&self.home);
        match github_upgrade::decide(&new, &installed, false) {
            Step::Install => {}
            Step::UpToDate => return Ok(Fetched::UpToDate),
            Step::InstallIsNewer => return Ok(Fetched::InstallIsNewer),
            Step::Undecided | Step::Downgrade => return Ok(Fetched::Blocked),
        }
        let platform = self.platform;
        let new_text = new.describe();
        let asset = manifest
            .asset(platform)
            .ok_or_else(|| anyhow::anyhow!(summon_i18n::format!("r86aae26146")))?;
        let mut out = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log)
            .with_context(|| summon_i18n::format!("rad09049d43", log.display()))?;
        writeln!(
            out,
            "{}",
            summon_i18n::format!("ra930b09d43", chrono::Local::now().format("%F %T"))
        )?;
        if let Some(drill) = &self.drill {
            let at = drill.describe();
            writeln!(out, "{}", summon_i18n::format!("r6269729924"))?;
        }
        let source = self.source(Some(version))?;
        let run = self.home.join(".summon/run");
        std::fs::create_dir_all(&run)
            .with_context(|| summon_i18n::format!("rc18f37e5e1", run.display()))?;
        let stage = Stage::create_in(&run, STAGE_PREFIX)?;
        let verify = Verify {
            stamp_timeout: self.stamp_timeout,
            codesign: self.codesign.as_ref(),
        };
        stage.prepare(
            source.as_ref(),
            &manifest,
            asset,
            platform,
            &verify,
            &mut out,
        )?;
        Ok(Fetched::Ready {
            stage: stage.keep(),
        })
    }

    fn recheck(&self, pending: &Pending) -> Result<Result<Rechecked, String>> {
        let version = &pending.version;
        let Some(stage) = &pending.stage else {
            return Ok(Err(summon_i18n::format!(
                "auto_upgrade.pending_without_stage"
            )));
        };
        let entry = stage.join("pkg").join(companion::SUMMON);
        if !entry.is_file() {
            let stage = stage.display();
            return Ok(Err(summon_i18n::format!("rb773c3fa57")));
        }
        match github_upgrade::reported_build(&entry, self.stamp_timeout) {
            Ok(found) if &found.version == version => {}
            Ok(found) => {
                let found = found.describe();
                return Ok(Err(summon_i18n::format!("rc7d2299384")));
            }
            Err(error) => return Ok(Err(summon_i18n::format!("r053f8574c5"))),
        }
        let watch_args = vec![OsString::from("--stage"), stage.clone().into_os_string()];
        Ok(Ok(Rechecked { entry, watch_args }))
    }

    fn doctor_notes(&self) -> Vec<String> {
        let platform = self.platform;
        let mut notes = vec![summon_i18n::format!("r7a70627dcf")];
        if let Some(drill) = &self.drill {
            let at = drill.describe();
            notes.push(summon_i18n::format!("rc3b41ce575"));
        }
        // 带 npm 分发渠道的构建:Release 里是公开构建,换代之后落点那份不再看 npm 包。
        if cfg!(npm_dist) {
            notes.push(summon_i18n::text!("r2c8b639d7a").to_string());
        }
        notes
    }
}

/// GitHub 上 Release 的 URL 前缀:查新版看 latest,取包用读到的那一版的 tag。
fn release_base(version: Option<&str>) -> String {
    source::release_base(version)
}

/// 清掉不再用得着的自动升级暂存:`keep` 是待换代记录引用的那一个,留着;`current`(正在跑的这份
/// summon)所在的也留着 —— 换代时停机之后的发布没成、daemon 从暂存起的那种情形。清不掉只是多占盘。
pub(crate) fn sweep_stages(home: &Path, keep: Option<&Path>, current: Option<&Path>) {
    let run = home.join(".summon/run");
    let Ok(entries) = std::fs::read_dir(&run) else {
        return;
    };
    let current = current.and_then(|exe| exe.canonicalize().ok());
    let same = |a: &Path, b: &Path| match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let ours = entry
            .file_name()
            .to_str()
            .is_some_and(|name| name.starts_with(STAGE_PREFIX));
        if !ours || !path.is_dir() {
            continue;
        }
        if keep.is_some_and(|keep| same(keep, &path)) {
            continue;
        }
        let hosts_us = current
            .as_ref()
            .is_some_and(|exe| path.canonicalize().is_ok_and(|dir| exe.starts_with(dir)));
        if hosts_us {
            continue;
        }
        let _ = std::fs::remove_dir_all(&path);
    }
}

/// 换代刚完成时核对:本平台应有的每个文件(`release::package_files`)暂存里都在,落点里的与暂存里的
/// 逐字节相同;暂存 `pkg/` 里有清单之外的文件也不算。只看 daemon 报的版本不够 —— 停机之后的发布
/// 没成时 daemon 会从暂存起,报的也是新版;半发布(summon 换了、sidecar 没换)也报新版。
pub(crate) fn landed(home: &Path, stage: &Path, platform: Option<&str>) -> Result<(), String> {
    let names = platform
        .and_then(release::package_files)
        .ok_or_else(|| summon_i18n::text!("rf3c0e98ae6").to_string())?;
    let pkg = stage.join("pkg");
    let extra = std::fs::read_dir(&pkg)
        .map_err(|_| {
            let stage = stage.display();
            summon_i18n::format!("rff2713b5a2")
        })?
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .find(|name| !names.contains(&name.as_str()));
    if let Some(name) = extra {
        return Err(summon_i18n::format!("r0bb9a33435"));
    }
    let dir = companion::install_dir(home);
    for &name in names {
        if !pkg.join(name).is_file() {
            let stage = stage.display();
            return Err(summon_i18n::format!("rff2713b5a2"));
        }
        if !github_upgrade::same_contents(&pkg.join(name), &dir.join(name)) {
            return Err(summon_i18n::format!("r10ae6b4375"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
