//! Release 附件的两种来源:GitHub 上的直链(或任意 URL 前缀)与本地目录。
//!
//! **只走 `github.com/<仓库>/releases/...` 的直链,不调 GitHub API**:直链不计 API 配额,未登录的
//! API 配额在共享出口 IP 上可能早已用光。`latest` 由 `releases/latest/download/<附件名>` 的跳转
//! 解析,不需要先问「最新版是哪个 tag」。
//!
//! **明文 http 只许本机**:manifest 与包同源,明文链路上两者能被一起换掉,字节数、哈希与构建戳的
//! 核对就形同虚设。所以 `--from http://` 只收本机地址(本地演练用),从 https 出发的请求也不跟随
//! 降到 http 的跳转。

use std::error::Error as _;
use std::io::Read;
use std::path::PathBuf;
use std::time::Duration;
use summon_core::release::{ReleaseError, Source, MANIFEST};

/// 仓库地址,来自 `crates/summon/Cargo.toml` 的 `repository`(导出到公开仓的同一份)。
pub(crate) const REPOSITORY: &str = env!("CARGO_PKG_REPOSITORY");

/// 连上服务器的时限。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// 取 manifest 的总时限:它只有几百字节。
const MANIFEST_TIMEOUT: Duration = Duration::from_secs(60);
/// 取平台包的总时限(连接、跳转与读完响应体)。
const PACKAGE_TIMEOUT: Duration = Duration::from_secs(15 * 60);
/// 跳转次数上限,与 reqwest 默认的相同。
const MAX_REDIRECTS: usize = 10;

/// 默认来源的 URL 前缀:不指定版本取 latest,指定了取那个 tag。
pub(crate) fn release_base(version: Option<&str>) -> String {
    match version {
        None => format!("{REPOSITORY}/releases/latest/download"),
        Some(version) => format!("{REPOSITORY}/releases/download/v{version}"),
    }
}

/// 主机是本机(`127.0.0.1`、`localhost`、`[::1]`),且不带用户信息。
pub(crate) fn is_loopback(url: &reqwest::Url) -> bool {
    url.username().is_empty()
        && url.password().is_none()
        && matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "[::1]"))
}

/// 来源 URL 能不能用:https 都行,明文 http 只许本机。
pub(crate) fn source_allowed(url: &str) -> bool {
    match reqwest::Url::parse(url) {
        Ok(url) if url.scheme() == "https" => url.host_str().is_some_and(|host| !host.is_empty()),
        Ok(url) if url.scheme() == "http" => is_loopback(&url),
        _ => false,
    }
}

/// 跳转能不能跟:目标是 https 就跟;目标是明文 http 时,只有出发点本就是本机的 http、目标也是
/// 本机才跟 —— 从 https 出发的请求绝不降到明文,本机来源也不被引到别处的明文地址。
pub(crate) fn redirect_allowed(origin: &reqwest::Url, to: &reqwest::Url) -> bool {
    match to.scheme() {
        "https" => true,
        "http" => origin.scheme() == "http" && is_loopback(origin) && is_loopback(to),
        _ => false,
    }
}

/// 跟随跳转的规矩。`Any`:[`redirect_allowed`],给人当面给的来源与 GitHub 直链;`Loopback`:只跟到
/// 本机回环地址(http 或 https),给自动升级的演练来源 —— 它不许落到任何远端。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Redirects {
    Any,
    Loopback,
}

/// 演练来源的跳转能不能跟:目标必须是本机回环地址。
pub(crate) fn loopback_redirect_allowed(to: &reqwest::Url) -> bool {
    matches!(to.scheme(), "http" | "https") && is_loopback(to)
}

fn redirect_policy(redirects: Redirects) -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::custom(move |attempt| {
        let to = attempt.url().to_string();
        if attempt.previous().len() > MAX_REDIRECTS {
            let limit = MAX_REDIRECTS;
            return attempt.error(summon_i18n::format!("rf504ee7add"));
        }
        let allowed = match redirects {
            Redirects::Any => attempt
                .previous()
                .first()
                .is_some_and(|origin| redirect_allowed(origin, attempt.url())),
            Redirects::Loopback => loopback_redirect_allowed(attempt.url()),
        };
        if allowed {
            attempt.follow()
        } else {
            attempt.error(summon_i18n::format!("r160be7d633"))
        }
    })
}

/// reqwest 的错误连同它的来源链(跳转被拒的原因在来源里)。
fn describe(error: &reqwest::Error) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}

/// 本地目录:其下直接有 `manifest.json` 与各平台的包。
pub(crate) struct DirSource(pub PathBuf);

impl Source for DirSource {
    fn open(&self, name: &str) -> Result<Box<dyn Read>, ReleaseError> {
        let path = self.0.join(name);
        std::fs::File::open(&path)
            .map(|file| Box::new(file) as Box<dyn Read>)
            .map_err(|error| {
                ReleaseError::Fetch(summon_i18n::format!("rda8fb21209", path = path.display()))
            })
    }

    fn locate(&self, name: &str) -> String {
        self.0.join(name).display().to_string()
    }
}

/// URL 前缀:附件在 `<前缀>/<附件名>`。跟随跳转(GitHub 的直链要跳两次,见 [`redirect_allowed`]),
/// 有连接与总时限。调用方先用 [`source_allowed`] 挡掉非本机的明文地址。
pub(crate) struct HttpSource {
    base: String,
    client: reqwest::blocking::Client,
}

impl HttpSource {
    pub(crate) fn new(base: &str) -> anyhow::Result<Self> {
        Self::with_redirects(base, Redirects::Any)
    }

    pub(crate) fn with_redirects(base: &str, redirects: Redirects) -> anyhow::Result<Self> {
        let mut builder = reqwest::blocking::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .user_agent(concat!("summon/", env!("CARGO_PKG_VERSION")))
            .redirect(redirect_policy(redirects));
        // 本机地址(本地演练的 Release)不经代理:环境里的代理够不着它。
        if reqwest::Url::parse(base).is_ok_and(|url| is_loopback(&url)) {
            builder = builder.no_proxy();
        }
        let client = builder.build()?;
        Ok(Self {
            base: base.trim_end_matches('/').to_string(),
            client,
        })
    }

    fn url(&self, name: &str) -> String {
        format!("{}/{name}", self.base)
    }
}

impl Source for HttpSource {
    fn open(&self, name: &str) -> Result<Box<dyn Read>, ReleaseError> {
        let url = self.url(name);
        let timeout = if name == MANIFEST {
            MANIFEST_TIMEOUT
        } else {
            PACKAGE_TIMEOUT
        };
        let response = self
            .client
            .get(&url)
            .timeout(timeout)
            .send()
            .map_err(|error| {
                ReleaseError::Fetch(summon_i18n::format!(
                    "rb0784a9986",
                    error = describe(&error)
                ))
            })?;
        let status = response.status();
        if !status.is_success() {
            return Err(ReleaseError::Fetch(summon_i18n::format!("r9404566943")));
        }
        Ok(Box::new(response))
    }

    fn locate(&self, name: &str) -> String {
        self.url(name)
    }
}
