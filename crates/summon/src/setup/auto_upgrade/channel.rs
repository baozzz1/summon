//! 自动升级的发布渠道:新版从哪里查、怎么取到本机、换代前怎么复核。判据、空闲等待、换代、守望
//! 与退回都与渠道无关,在本模块的其余部分。
//!
//! 只有一条渠道:GitHub Releases(见 [`super::github`]),`upgrade.channel` 不写或写 `github` 都是它。
//! 停用的 npm 渠道的写法 `npm` 照样按 GitHub Releases 走,doctor 提示删掉它(见 [`retired_note`])。

use super::state::{Pending, Seen};
use anyhow::Result;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use summon_core::config::Upgrade;

/// `upgrade.channel` 里 GitHub Releases 渠道的写法。
const GITHUB: &str = "github";

/// 已停用的 npm 渠道在 `upgrade.channel` 里的写法。它是早先带 npm 分发渠道的构建的默认渠道,写着它
/// 的机器是有意打开了自动升级;如今新版只发在 GitHub Releases,按 GitHub Releases 走才不让自动升级
/// 悄悄停掉。
const RETIRED_NPM: &str = "npm";

/// 取新版到本机之后该做什么。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Fetched {
    /// 已取到本机,等整机空闲换代。`stage`:新版所在的暂存目录。
    Ready { stage: PathBuf },
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

/// 这台机器此刻按配置能走的渠道;走不了就说明为什么不自动升级。
pub(crate) fn open(home: &Path, policy: &Upgrade) -> Result<Box<dyn Channel>, String> {
    let from = policy
        .from
        .as_deref()
        .filter(|from| !from.trim().is_empty());
    match policy.channel.as_deref().map(str::trim) {
        None | Some(GITHUB) | Some(RETIRED_NPM) => {
            super::github::GithubChannel::open(home, from).map(|c| Box::new(c) as Box<dyn Channel>)
        }
        Some(name) => Err(summon_i18n::format!("raee0004506")),
    }
}

/// 配置里还写着停用的 npm 渠道时,doctor 多说的一行。
pub(crate) fn retired_note(policy: &Upgrade) -> Option<String> {
    (policy.channel.as_deref().map(str::trim) == Some(RETIRED_NPM))
        .then(|| summon_i18n::text!("upgrade.channel.npm_retired").to_string())
}
