//! 自动升级的发布渠道:新版从哪里查、怎么取到本机、换代前怎么复核。判据、空闲等待、换代、守望
//! 与退回都与渠道无关,在本模块的其余部分。
//!
//! 渠道由 `upgrade.channel` 选:`github`(GitHub Releases,见 [`super::github`])在所有构建里都有;
//! 不写时用本构建的默认渠道 —— 带 npm 分发渠道的构建是 npm,其余是 github。

use super::state::{Pending, Seen};
use anyhow::Result;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;
use summon_core::config::Upgrade;

/// 守望进程退回时让发布源那一侧回到旧版(见 [`RestoreSource`])的时限。兜底进程等守望进程的上限
/// 把它算在里面。
pub(crate) const RESTORE_LIMIT: Duration = Duration::from_secs(600);

/// `upgrade.channel` 里 GitHub Releases 渠道的写法。
pub(crate) const GITHUB: &str = "github";

/// 取新版到本机之后该做什么。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Fetched {
    /// 已取到本机,等整机空闲换代。`stage`:新版所在的暂存目录(新版留在别处的渠道没有)。
    Ready { stage: Option<PathBuf> },
    /// 落点里是更新的版本,不替换。
    InstallIsNewer,
    /// 落点换不进去,换代也没用。
    Blocked,
    /// 已是这一版,无需换代。
    UpToDate,
}

/// 查新版没成。`repeats`:这是一件会接连出现的事(比如发布源取不到),同一原因只计次、不刷新时刻。
#[derive(Debug)]
pub(crate) struct CheckFailed {
    pub(crate) text: String,
    pub(crate) repeats: bool,
}

/// 换代前复核过的新版入口,以及守望进程要多带的参数。
#[derive(Debug)]
pub(crate) struct Rechecked {
    pub(crate) entry: PathBuf,
    pub(crate) watch_args: Vec<OsString>,
}

/// 一条发布渠道。每次检查、每次换代尝试都按此刻的本机与配置现开一个(见 [`open`]),不跨两次复用。
pub(crate) trait Channel {
    /// 本平台在这条渠道上的名字,给人看。
    fn platform(&self) -> &str;
    /// 发布源上的最新版与它的发布时刻。
    fn latest(&mut self) -> Result<Seen, CheckFailed>;
    /// 这一版本平台的包发布了没有。
    fn platform_ready(&mut self, version: &str) -> Result<bool>;
    /// 把这一版取到本机并核对,不碰 daemon 与落点;过程追加进 `log`。
    fn fetch(&mut self, version: &str, log: &Path) -> Result<Fetched>;
    /// 换代前复核:新版入口此刻必须仍是待换的那一版(取到之后可能有人手动动过,或者待换代记录是
    /// 别的渠道取的)。外层错误是这条渠道此刻用不了;内层错误是入口已不是那一版,待换代记录作废。
    fn recheck(&self, pending: &Pending) -> Result<Result<Rechecked, String>>;
    /// `summon doctor` 里关于这条渠道要多说的几行。
    fn doctor_notes(&self) -> Vec<String>;
}

/// 守望进程退回时,发布源那一侧怎么回到旧版。没有这一侧的渠道什么都不做。
pub(crate) trait RestoreSource {
    /// 成了(或无事可做)返回 `None`;没成返回这次退回的结局说明 —— 落点照样放回旧版。
    fn restore(&self, home: &Path, new: &str, previous: &str, why: &str) -> Option<String>;
}

/// 没有发布源那一侧可退的渠道(GitHub Releases:退回只靠换代前的落点快照)。
#[cfg(any(test, not(npm_dist)))]
pub(crate) struct NoRestore;

#[cfg(any(test, not(npm_dist)))]
impl RestoreSource for NoRestore {
    fn restore(&self, _home: &Path, _new: &str, _previous: &str, _why: &str) -> Option<String> {
        None
    }
}

/// 这台机器此刻按配置能走的渠道;走不了就说明为什么不自动升级。
pub(crate) fn open(home: &Path, policy: &Upgrade) -> Result<Box<dyn Channel>, String> {
    let from = policy
        .from
        .as_deref()
        .filter(|from| !from.trim().is_empty());
    match policy.channel.as_deref().map(str::trim) {
        Some(GITHUB) => github(home, from),
        Some(other) => other_channel(home, other, from),
        None => default_channel(home, from),
    }
}

fn github(home: &Path, from: Option<&str>) -> Result<Box<dyn Channel>, String> {
    super::github::GithubChannel::open(home, from).map(|c| Box::new(c) as Box<dyn Channel>)
}

/// 带 npm 分发渠道的构建:默认 npm。
#[cfg(npm_dist)]
fn default_channel(home: &Path, from: Option<&str>) -> Result<Box<dyn Channel>, String> {
    npm(home, from)
}

/// 只有 GitHub Releases 渠道的构建。
#[cfg(not(npm_dist))]
fn default_channel(home: &Path, from: Option<&str>) -> Result<Box<dyn Channel>, String> {
    github(home, from)
}

#[cfg(npm_dist)]
fn npm(home: &Path, from: Option<&str>) -> Result<Box<dyn Channel>, String> {
    if from.is_some() {
        return Err(summon_i18n::text!("re4d19e09fd").to_string());
    }
    crate::setup::npm_dist::auto_upgrade::NpmChannel::open(home)
        .map(|channel| Box::new(channel) as Box<dyn Channel>)
}

fn other_channel(home: &Path, name: &str, from: Option<&str>) -> Result<Box<dyn Channel>, String> {
    #[cfg(npm_dist)]
    if name == crate::setup::npm_dist::auto_upgrade::CHANNEL {
        return npm(home, from);
    }
    let _ = (home, from);
    Err(summon_i18n::format!("raee0004506"))
}
