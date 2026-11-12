//! 文件预览服务器:token 能力 URL + HTTP 服务(设计见 `docs/spec/2026-08-25-file-preview-server-design.md`)。
//!
//! 安全模型:服务器只认 token,不接受路径参数;token 是 128-bit 随机数,落盘
//! `~/.summon/run/preview-tokens.json`(run_dir 已是 0700)。token 分两种:回复卡文件按钮里的
//! 只用来定位文件,HTTP 上打不开任何东西;能打开内容的是点击回调判权通过后私聊给点击人的那一条,
//! 每次私聊新签(见 `docs/spec/2026-10-20-link-click-auth-design.md`)。
//! 出站由 [`PreviewServer::link_buttons`] 经 `FeishuFrontend` 生成按钮,agent 无感知。

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use summon_core::config::PreviewServer as PreviewServerConfig;
use summon_core::feishu::PreviewButton;
use summon_core::file_preview::html::{
    detect_kind, dir_listing_shows, directory_link_allowed, file_ext, raster_image_mime,
    render_dir_page, DirEntry, DirIdentity, PreviewKind, DIR_LISTING_LIMIT,
};

use crate::observe::log::log;
use crate::runtime::fsio::atomic_write;
use crate::runtime::paths::run_dir;

/// 本机对外地址:枚举 up 且 running 的接口,判据交给纯函数
/// [`summon_core::config::pick_public_host`](哪些地址「填了也点不开」在那里定义,可单测)。
pub(crate) fn detect_public_host() -> Option<std::net::Ipv4Addr> {
    use nix::net::if_::InterfaceFlags;
    let addrs: Vec<std::net::Ipv4Addr> = nix::ifaddrs::getifaddrs()
        .ok()?
        .filter(|ifa| {
            ifa.flags.contains(InterfaceFlags::IFF_UP)
                && ifa.flags.contains(InterfaceFlags::IFF_RUNNING)
        })
        .filter_map(|ifa| ifa.address?.as_sockaddr_in().map(|s| s.ip()))
        .collect();
    summon_core::config::pick_public_host(&addrs)
}

/// token → path 映射的落盘形态。各表一起原子写。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct TokenStoreFile {
    /// token → 文件路径
    tokens: HashMap<String, String>,
    /// token → 创建时间(unix 秒)
    created_at: HashMap<String, i64>,
    /// path → 定位 token(幂等 mint:同一文件复用同一 token,同一文件反复出现在回复里也不撑爆表)
    by_path: HashMap<String, String>,
    /// 能打开内容的 token。不在这里的只用来定位文件。
    #[serde(default)]
    open: HashSet<String>,
    /// path → 目录页里签出的打开 token(幂等,同一目录页反复刷新也不撑爆表)。私聊发出去的
    /// 打开 token 每次新签,不进这张表。
    #[serde(default)]
    open_by_path: HashMap<String, String>,
    /// token → 相对化基准(agent working_dir)。渲染时据此把绝对路径缩成项目相对路径;
    /// 缺省(老 token / 无 base 上下文 mint 的)回退绝对路径。
    #[serde(default)]
    base: HashMap<String, String>,
}

struct TokenStore {
    file: TokenStoreFile,
    path: PathBuf,
    ttl_secs: i64,
}

impl TokenStore {
    fn load(path: PathBuf, ttl_secs: i64) -> Self {
        let file = match std::fs::read_to_string(&path) {
            Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
            Err(_) => TokenStoreFile::default(),
        };
        Self {
            file,
            path,
            ttl_secs,
        }
    }

    /// mint 一个定位 token(已存在且未过期则复用)。新 mint 落盘;落盘失败只 log——
    /// 内存里仍可用到下次重启,比「磁盘抖一下就不出按钮」强。
    /// 返回空串 = 随机数源不可用,调用方按「不出按钮」处理。
    fn mint(&mut self, path: &str, base: Option<&str>) -> String {
        let (token, is_new) = self.mint_no_persist(path, base);
        if is_new {
            self.persist();
        }
        token
    }

    /// mint 定位 token 但不落盘。返回 (token, 是否新 mint)。
    fn mint_no_persist(&mut self, path: &str, base: Option<&str>) -> (String, bool) {
        if let Some(token) = self.reusable(self.file.by_path.get(path)) {
            return (token, false);
        }
        let token = self.insert(path, base);
        if token.is_empty() {
            return (token, false);
        }
        self.file.by_path.insert(path.to_string(), token.clone());
        (token, true)
    }

    /// mint 打开 token 但不落盘。`reuse` 时同一路径复用(目录页里的条目),否则每次新签(私聊
    /// 发出去的那条)。批量场景(目录列举一次最多 200 条)由调用方统一 persist 一次,否则每条
    /// 全量落盘是 O(N²)。返回 (token, 是否新 mint)。
    fn mint_open_no_persist(
        &mut self,
        path: &str,
        base: Option<&str>,
        reuse: bool,
    ) -> (String, bool) {
        if reuse {
            if let Some(token) = self.reusable(self.file.open_by_path.get(path)) {
                return (token, false);
            }
        }
        let token = self.insert(path, base);
        if token.is_empty() {
            return (token, false);
        }
        self.file.open.insert(token.clone());
        if reuse {
            self.file
                .open_by_path
                .insert(path.to_string(), token.clone());
        }
        (token, true)
    }

    /// 复用要留够余量:新发出去的链接至少还有半个有效期。不够就签新的,旧 token 按自己的
    /// 时间过期,已经发出去的链接不受影响。
    fn reusable(&self, token: Option<&String>) -> Option<String> {
        let token = token?;
        self.file
            .created_at
            .get(token)
            .is_some_and(|created| unix_secs() - created <= self.ttl_secs / 2)
            .then(|| token.clone())
    }

    /// 签一个新 token 记下路径与基准;随机数源不可用时返回空串。
    fn insert(&mut self, path: &str, base: Option<&str>) -> String {
        // 签新 token 时顺手清掉已过期的项,表的大小随在效链接数而不随签发次数增长。
        let _ = self.drop_expired();
        let token = new_token();
        if token.is_empty() {
            return token;
        }
        self.file.tokens.insert(token.clone(), path.to_string());
        self.file.created_at.insert(token.clone(), unix_secs());
        if let Some(b) = base {
            self.file.base.insert(token.clone(), b.to_string());
        }
        token
    }

    /// 打开 token → (path, base)(未过期);定位 token 返回 None。
    fn resolve_open(&mut self, token: &str) -> Option<(String, Option<String>)> {
        if !self.file.open.contains(token) {
            return None;
        }
        self.resolve(token)
    }

    /// token → (path, base)(未过期)。过期项顺手清掉。
    fn resolve(&mut self, token: &str) -> Option<(String, Option<String>)> {
        if self.is_expired(token) {
            self.remove(token);
            return None;
        }
        let path = self.file.tokens.get(token).cloned()?;
        let base = self.file.base.get(token).cloned();
        Some((path, base))
    }

    /// 内存里清掉所有过期 token,返回是否有变化(落盘由调用方决定)。
    fn drop_expired(&mut self) -> bool {
        let expired: Vec<String> = self
            .file
            .tokens
            .keys()
            .filter(|token| self.is_expired(token))
            .cloned()
            .collect();
        for token in &expired {
            self.remove(token);
        }
        !expired.is_empty()
    }

    /// 清掉所有过期 token,有变化才落盘。
    fn sweep(&mut self) {
        if self.drop_expired() {
            self.persist();
        }
    }

    fn is_expired(&self, token: &str) -> bool {
        self.file
            .created_at
            .get(token)
            .is_some_and(|created| unix_secs() - created > self.ttl_secs)
    }

    fn remove(&mut self, token: &str) {
        if let Some(path) = self.file.tokens.remove(token) {
            // 同一路径可能已签了更新的 token,映射只在仍指向本 token 时才删。
            for by_path in [&mut self.file.by_path, &mut self.file.open_by_path] {
                if by_path.get(&path).is_some_and(|current| current == token) {
                    by_path.remove(&path);
                }
            }
        }
        self.file.created_at.remove(token);
        self.file.base.remove(token);
        self.file.open.remove(token);
    }

    fn persist(&self) {
        if let Ok(json) = serde_json::to_string_pretty(&self.file) {
            if let Err(e) = atomic_write(&self.path, json.as_bytes()) {
                log(&summon_i18n::format!("ra93553fa3d"));
            }
        }
    }
}

/// 配置的有效期(秒)换成 token 表用的秒数:至少 1 秒,超出 i64 的取最大值。
fn store_ttl_secs(configured: u64) -> i64 {
    i64::try_from(configured).unwrap_or(i64::MAX).max(1)
}

/// 128-bit 随机 token,hex 编码(32 字符)。读 `/dev/urandom`,不引随机数依赖。
pub(crate) fn new_token() -> String {
    let mut bytes = [0u8; 16];
    match std::fs::File::open("/dev/urandom")
        .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut bytes))
    {
        Ok(()) => bytes.iter().map(|b| format!("{b:02x}")).collect(),
        // 拿不到随机数就不发 token:宁可不出按钮,也不发可预测的链接。
        Err(e) => {
            log(&summon_i18n::format!("r055f3b7e3f"));
            String::new()
        }
    }
}

fn unix_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 把绝对路径缩成相对于 base 的项目相对路径;不在 base 下(或 base 为空)返回 None,
/// 调用方据此回退绝对路径。
fn relativize(path: &str, base: &str) -> Option<String> {
    let base = base.trim();
    if base.is_empty() {
        return None;
    }
    Path::new(path)
        .strip_prefix(Path::new(base))
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
}

pub(crate) struct PreviewServer {
    store: Mutex<TokenStore>,
    cfg: PreviewServerConfig,
    max_bytes: u64,
    listening: AtomicBool,
    /// 家目录及其各级上级的身份,这些目录不给链接;为空时目录一律不给链接。
    protected_dirs: Vec<DirIdentity>,
}

/// 点击回调判权通过后私聊出去的那一条打开链接。
pub(crate) struct OpenLink {
    pub url: String,
    pub label: String,
    pub is_dir: bool,
    /// 链接的有效期(秒)。
    pub ttl_secs: u64,
}

/// `summon preview` 与 `summon.preview` 工具的回执:文件能预览,告诉 agent 怎么写进回复 ——
/// 打开入口是回复末尾的按钮,裸 URL 没有按钮、点不开。
pub(crate) fn preview_reply_hint(path: &str) -> String {
    summon_i18n::format!("r92536571d7")
}

/// 按钮上的名字:在 agent 工作目录下就显示相对路径(短的时候),否则文件名。
fn button_label(path: &str, base: Option<&str>) -> String {
    base.and_then(|base| relativize(path, base))
        .filter(|relative| relative.chars().count() <= 40)
        .unwrap_or_else(|| {
            Path::new(path)
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or(path)
                .to_string()
        })
}

/// 目录身份(跟随符号链接)。
fn dir_identity(path: &Path) -> Option<DirIdentity> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::metadata(path).ok()?;
    meta.is_dir().then(|| (meta.dev(), meta.ino()))
}

/// 每个家目录连同它的各级上级(直到根目录)的身份,去重后合并。
fn protected_dir_identities(homes: &[PathBuf]) -> Vec<DirIdentity> {
    let mut protected = Vec::new();
    for home in homes {
        let mut dir = std::fs::canonicalize(home).unwrap_or_else(|_| home.clone());
        loop {
            if let Some(identity) = dir_identity(&dir) {
                if !protected.contains(&identity) {
                    protected.push(identity);
                }
            }
            if !dir.pop() {
                break;
            }
        }
    }
    protected
}

/// 家目录取两处:`$HOME` 与账户数据库里本用户的家目录。daemon 的 `HOME` 被改过时仍能认出真实家目录。
fn home_dirs() -> Vec<PathBuf> {
    let account = nix::unistd::User::from_uid(nix::unistd::getuid())
        .ok()
        .flatten()
        .map(|user| user.dir);
    dirs::home_dir().into_iter().chain(account).collect()
}

impl PreviewServer {
    /// 绑定端口、起服务线程。`shutdown` 置位后线程在一个 recv 超时内退出,不挡 daemon 停机。
    pub(crate) fn spawn(
        cfg: &PreviewServerConfig,
        shutdown: Arc<AtomicBool>,
    ) -> anyhow::Result<Arc<Self>> {
        let store = TokenStore::load(
            run_dir().join("preview-tokens.json"),
            store_ttl_secs(cfg.token_ttl_secs),
        );
        let server =
            tiny_http::Server::http(format!("{}:{}", cfg.bind, cfg.port)).map_err(|e| {
                anyhow::anyhow!(summon_i18n::format!("rd5e6b66f96", cfg.bind, cfg.port))
            })?;
        let this = Arc::new(Self {
            store: Mutex::new(store),
            cfg: cfg.clone(),
            max_bytes: cfg.max_file_bytes(),
            listening: AtomicBool::new(true),
            protected_dirs: protected_dir_identities(&home_dirs()),
        });
        this.store.lock().unwrap().sweep();
        let serve = Arc::clone(&this);
        std::thread::Builder::new()
            .name("preview-server".to_string())
            .spawn(move || serve_loop(server, serve, shutdown))?;
        log(&summon_i18n::format!(
            "r85fab97e7a",
            cfg.bind,
            this.base_url()
                .unwrap_or_else(|| summon_i18n::text!("rb0337e50fd").to_string())
        ));
        Ok(this)
    }

    /// 找出 markdown 里提到的可预览本地文件,给每个签一个定位 token、做成回复末尾的按钮;正文里
    /// 的文字链接只留文字(见 [`summon_core::file_preview::links::extract_local_links`])。
    /// `base` 是 agent working_dir:mint 时记下,预览页据此把绝对路径缩成项目相对路径,按钮上的
    /// 名字也据此取短;传 `None` = 不缩。
    pub(crate) fn link_buttons(
        &self,
        md: &str,
        base: Option<&str>,
    ) -> (String, Vec<PreviewButton>) {
        let mut buttons: Vec<PreviewButton> = Vec::new();
        let (text, _) = summon_core::file_preview::links::extract_local_links(md, |path| {
            let Some(token) = self.locator_token(path, base) else {
                return false;
            };
            if !buttons.iter().any(|button| button.token == token) {
                buttons.push(PreviewButton {
                    label: button_label(path, base),
                    is_dir: std::fs::metadata(path).is_ok_and(|meta| meta.is_dir()),
                    token,
                });
            }
            true
        });
        (text, buttons)
    }

    /// 单个文件可预览 / 下载的大小上限(字节)。
    pub(crate) fn max_file_bytes(&self) -> u64 {
        self.max_bytes
    }

    /// 文件按钮点击判权通过之后:按定位 token 找回文件,按签发时的同一套判据再查一遍,新签一条
    /// 打开链接。链接只属于这一次私聊,不与别人共用;过期、文件已不在或服务暂不可用时返回 None。
    pub(crate) fn open_link(&self, locator: &str) -> Option<OpenLink> {
        if !is_well_formed_token(locator) {
            return None;
        }
        let url_base = self.base_url()?;
        let mut store = self.store.lock().ok()?;
        let (path, base) = store.resolve(locator)?;
        let meta = std::fs::metadata(&path).ok()?;
        let previewable = if meta.is_dir() {
            self.directory_link_allowed(&path)
        } else {
            meta.is_file() && meta.len() <= self.max_bytes
        };
        if !previewable {
            return None;
        }
        let (token, _) = store.mint_open_no_persist(&path, base.as_deref(), false);
        if token.is_empty() {
            return None;
        }
        store.persist();
        Some(OpenLink {
            url: format!("{url_base}/p/{token}"),
            label: button_label(&path, base.as_deref()),
            is_dir: meta.is_dir(),
            ttl_secs: u64::try_from(store.ttl_secs).unwrap_or(u64::MAX),
        })
    }

    /// 按钮链接的 base URL。**每次 mint 都现查** —— VPN 地址会在 daemon 运行期间变,
    /// 启动时算一次的话之后 mint 的链接会指向一个已经不属于本机的地址。
    fn base_url(&self) -> Option<String> {
        if !self.listening.load(Ordering::Acquire) {
            return None;
        }
        self.cfg.base_url_with_host(detect_public_host())
    }

    /// 路径 → 定位 token。给「存在、是常规文件或目录、文件未超大小上限」的路径 mint。
    /// 查不到可路由地址时**不 mint**:点了按钮也私聊不出能打开的链接,不如不出按钮。
    /// `base` 是 agent working_dir:mint 时记下,预览页头部据此显示项目相对路径;
    /// 传 `None` = 不缩,显示绝对路径。
    pub(crate) fn locator_token(&self, path: &str, base: Option<&str>) -> Option<String> {
        self.base_url()?;
        let meta = std::fs::metadata(path).ok()?;
        if !meta.is_file() && !meta.is_dir() {
            return None;
        }
        if meta.is_file() && meta.len() > self.max_bytes {
            return None;
        }
        if meta.is_dir() && !self.directory_link_allowed(path) {
            return None;
        }
        let token = self.store.lock().ok()?.mint(path, base);
        (!token.is_empty()).then_some(token)
    }

    /// 按目录身份判,不按路径写法判;读不到就不给。
    fn directory_link_allowed(&self, path: &str) -> bool {
        dir_identity(Path::new(path))
            .is_some_and(|dir| directory_link_allowed(dir, &self.protected_dirs))
    }

    /// 请求入口:只认 GET/HEAD,内容只给打开 token(见 [`TokenStore::resolve_open`])。
    fn handle_request(
        &self,
        method: &tiny_http::Method,
        url: &str,
    ) -> tiny_http::Response<std::io::Cursor<Vec<u8>>> {
        if !matches!(method, tiny_http::Method::Get | tiny_http::Method::Head) {
            return not_found();
        }
        self.serve(url)
    }

    #[cfg(test)]
    fn handle(&self, url: &str) -> tiny_http::Response<std::io::Cursor<Vec<u8>>> {
        self.handle_request(&tiny_http::Method::Get, url)
    }

    #[cfg(test)]
    pub(crate) fn open_token_count(&self) -> usize {
        self.store.lock().unwrap().file.open.len()
    }

    /// 测试入口:签定位 token 再换一条打开链接,等同点了按钮、判权通过。
    #[cfg(test)]
    pub(crate) fn mint_open_url(&self, path: &str, base: Option<&str>) -> Option<String> {
        self.open_link(&self.locator_token(path, base)?)
            .map(|link| link.url)
    }

    /// 打开 token 的内容服务。
    fn serve(&self, url: &str) -> tiny_http::Response<std::io::Cursor<Vec<u8>>> {
        let path = url.split('?').next().unwrap_or(url);
        use summon_core::file_preview::diagram;
        match path {
            diagram::MERMAID_PATH => return javascript_response(diagram::MERMAID_SCRIPT),
            diagram::RESIZE_PATH => return javascript_response(diagram::RESIZE_SCRIPT),
            _ => {}
        }
        let rest = match path.strip_prefix("/p/") {
            Some(r) => r,
            None => return not_found(),
        };
        let (token, route) = rest.split_once('/').unwrap_or((rest, ""));
        let raw = route == "raw";
        let diagram_index = if let Some(index) = route.strip_prefix("diagram/") {
            if index.is_empty() || !index.bytes().all(|byte| byte.is_ascii_digit()) {
                return not_found();
            }
            let Ok(index) = index.parse::<usize>() else {
                return not_found();
            };
            Some(index)
        } else if route.is_empty() || raw {
            None
        } else {
            return not_found();
        };
        if !is_well_formed_token(token) {
            return not_found();
        }
        let Some((file_path, rel_base)) = self
            .store
            .lock()
            .ok()
            .and_then(|mut s| s.resolve_open(token))
        else {
            return not_found();
        };
        let Ok(meta) = std::fs::metadata(&file_path) else {
            return not_found();
        };
        if meta.is_dir() {
            // raw 是文件字节,目录没有 raw;目录走列举页。token 表跨进程保留,表里的目录在
            // 服务时按签发时的同一判据再判一次。
            if raw || diagram_index.is_some() || !self.directory_link_allowed(&file_path) {
                return not_found();
            }
            return self.dir_listing(&file_path, rel_base.as_deref());
        }
        if !meta.is_file() || meta.len() > self.max_bytes {
            return not_found();
        }
        let Ok(bytes) = std::fs::read(&file_path) else {
            return not_found();
        };
        let filename = Path::new(&file_path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("file");
        if let Some(index) = diagram_index {
            if detect_kind(filename) != PreviewKind::Markdown {
                return not_found();
            }
            let Ok(text) = std::str::from_utf8(&bytes) else {
                return not_found();
            };
            let Some(source) = diagram::diagram_sources(text).into_iter().nth(index) else {
                return not_found();
            };
            let nonce = new_token();
            if nonce.is_empty() {
                return not_found();
            }
            return diagram_response(diagram::render_diagram_page(&source, &nonce), &nonce);
        }
        if raw {
            raw_response(filename, bytes)
        } else {
            let display = rel_base
                .as_deref()
                .and_then(|b| relativize(&file_path, b))
                .unwrap_or_else(|| file_path.clone());
            let html = summon_core::file_preview::html::render_page(
                filename,
                &display,
                meta.len(),
                token,
                &bytes,
            );
            if detect_kind(filename) == PreviewKind::Html {
                isolated_html_response(html)
            } else {
                html_response(html)
            }
        }
    }

    /// 目录列举:读直接子项,目录在前文件在后,给文件签打开链接后渲成 HTML。
    /// 只列这一层,子目录只显示名字、不提供父目录导航,以 `.` 开头的条目不列——token 的
    /// 能力范围就是这一层的文件。
    /// `rel_base` 是 mint 该目录 token 时记下的 agent working_dir:目录页头部与子条目
    /// 都据此显示项目相对路径。
    fn dir_listing(
        &self,
        dir_path: &str,
        rel_base: Option<&str>,
    ) -> tiny_http::Response<std::io::Cursor<Vec<u8>>> {
        let Ok(read) = std::fs::read_dir(dir_path) else {
            return not_found();
        };
        // (名字, 全路径, is_dir, size)
        let mut entries: Vec<(String, String, bool, u64)> = Vec::new();
        for entry in read.flatten() {
            let file_name = entry.file_name();
            let Some(name) = file_name.to_str() else {
                continue;
            };
            if !dir_listing_shows(name) {
                continue;
            }
            let full = Path::new(dir_path)
                .join(name)
                .to_string_lossy()
                .into_owned();
            // 跟随符号链接判类型:指向目录的链接按目录处理,只显示名字。
            let Ok(meta) = std::fs::metadata(&full) else {
                continue;
            };
            let is_dir = meta.is_dir();
            entries.push((
                name.to_string(),
                full,
                is_dir,
                if is_dir { 0 } else { meta.len() },
            ));
        }
        entries.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| a.0.cmp(&b.0)));
        let truncated = entries.len() > DIR_LISTING_LIMIT;
        let shown: Vec<_> = entries.into_iter().take(DIR_LISTING_LIMIT).collect();
        // 批量 mint 本层文件:一次锁、一次落盘(逐条全量落盘是 O(N²))。
        let files: Vec<String> = shown
            .iter()
            .filter(|(_, _, is_dir, _)| !is_dir)
            .map(|(_, p, _, _)| p.clone())
            .collect();
        let mut urls = self.mint_urls_batch(&files, rel_base).into_iter();
        let mut view = Vec::with_capacity(shown.len());
        for (name, _, is_dir, size) in shown {
            let url = if is_dir {
                None
            } else {
                // 文件 mint 不出来(竞态删掉了/超限)就不列,fail-closed。
                let Some(url) = urls.next().flatten() else {
                    continue;
                };
                Some(url)
            };
            view.push(DirEntry {
                name,
                is_dir,
                size,
                url,
            });
        }
        let dirname = Path::new(dir_path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("dir");
        let display = rel_base
            .and_then(|b| relativize(dir_path, b))
            .unwrap_or_else(|| dir_path.to_string());
        html_response(render_dir_page(dirname, &display, &view, truncated))
    }

    /// 批量给列表页里的文件签打开链接:能看到这一页就能打开这一层的文件。同一把锁、最多一次
    /// 落盘。只给常规文件(跟随符号链接后)且未超大小上限的签 —— 列举与签发之间被换成目录的
    /// 条目在这里拦下。
    /// 一次批量共用一个 base URL —— 同一张目录页里的条目必须指向同一台机器。
    /// `rel_base` 透传给每个子条目 token,子条目被预览时头部同样显示项目相对路径。
    fn mint_urls_batch(&self, paths: &[String], rel_base: Option<&str>) -> Vec<Option<String>> {
        let Some(base) = self.base_url() else {
            return paths.iter().map(|_| None).collect();
        };
        let mut store = match self.store.lock() {
            Ok(s) => s,
            Err(_) => return paths.iter().map(|_| None).collect(),
        };
        let mut any_new = false;
        let mut out = Vec::with_capacity(paths.len());
        for p in paths {
            let ok = match std::fs::metadata(p) {
                Ok(m) => m.is_file() && m.len() <= self.max_bytes,
                Err(_) => false,
            };
            if !ok {
                out.push(None);
                continue;
            }
            let (token, is_new) = store.mint_open_no_persist(p, rel_base, true);
            any_new |= is_new;
            out.push(if token.is_empty() {
                None
            } else {
                Some(format!("{base}/p/{token}"))
            });
        }
        if any_new {
            store.persist();
        }
        out
    }
}

const RECV_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(500);
const REBIND_DELAY: std::time::Duration = std::time::Duration::from_millis(500);
const MAX_REBIND_DELAY: std::time::Duration = std::time::Duration::from_secs(30);

fn serve_loop(server: tiny_http::Server, preview: Arc<PreviewServer>, shutdown: Arc<AtomicBool>) {
    let Some(address) = server.server_addr().to_ip() else {
        preview.listening.store(false, Ordering::Release);
        log(summon_i18n::text!("rbf7cf12c33"));
        return;
    };
    serve_loop_with(
        server,
        preview,
        shutdown,
        || tiny_http::Server::http(address).map_err(|e| anyhow::anyhow!("{e}")),
        |server, timeout| server.recv_timeout(timeout),
        wait_for_shutdown,
    );
}

/// `wait` 是重新绑定前的退避等待(生产恒为 [`wait_for_shutdown`]),返回 true 表示等待期间收到停机。
/// 抽成参数是为了让测试精确核对退避节奏而不必真睡满秒级的退避。
fn serve_loop_with<L>(
    server: L,
    preview: Arc<PreviewServer>,
    shutdown: Arc<AtomicBool>,
    mut bind: impl FnMut() -> anyhow::Result<L>,
    mut recv: impl FnMut(&L, std::time::Duration) -> std::io::Result<Option<tiny_http::Request>>,
    mut wait: impl FnMut(&AtomicBool, std::time::Duration) -> bool,
) {
    let mut server = Some(server);
    let mut delay = REBIND_DELAY;
    while !shutdown.load(Ordering::Relaxed) {
        let Some(listener) = server.as_ref() else {
            if wait(&shutdown, delay) {
                break;
            }
            match bind() {
                Ok(listener) => {
                    server = Some(listener);
                    if shutdown.load(Ordering::Relaxed) {
                        break;
                    }
                    preview.listening.store(true, Ordering::Release);
                    delay = REBIND_DELAY;
                    log(summon_i18n::text!("rfcef132919"));
                }
                Err(e) => {
                    log(&summon_i18n::format!("r59d3bec801"));
                    delay = (delay * 2).min(MAX_REBIND_DELAY);
                }
            }
            continue;
        };
        match recv(listener, RECV_TIMEOUT) {
            Ok(Some(request)) => {
                let url = request.url().to_string();
                let response = preview.handle_request(request.method(), &url);
                // 客户端断开等发送失败不影响服务:忽略。
                let _ = request.respond(response);
            }
            Ok(None) => continue,
            Err(e) => {
                preview.listening.store(false, Ordering::Release);
                // tiny_http 的 accept 线程在上报错误后退出，同一个 Server 不能继续监听。
                drop(server.take());
                delay = REBIND_DELAY;
                log(&summon_i18n::format!("rd23d73ee7d"));
            }
        }
    }
    preview.listening.store(false, Ordering::Release);
}

fn wait_for_shutdown(shutdown: &AtomicBool, delay: std::time::Duration) -> bool {
    let started = std::time::Instant::now();
    while !shutdown.load(Ordering::Relaxed) {
        let remaining = delay.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            return false;
        }
        std::thread::sleep(remaining.min(std::time::Duration::from_millis(50)));
    }
    true
}

/// token 形态校验:32 位 hex。不合法的直接 404,不查 store(fail-closed)。
fn is_well_formed_token(token: &str) -> bool {
    token.len() == 32 && token.bytes().all(|b| b.is_ascii_hexdigit())
}

fn not_found() -> tiny_http::Response<std::io::Cursor<Vec<u8>>> {
    tiny_http::Response::from_string("not found".to_string()).with_status_code(404)
}

fn html_response(html: String) -> tiny_http::Response<std::io::Cursor<Vec<u8>>> {
    tiny_http::Response::from_string(html).with_header(
        tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"text/html; charset=utf-8"[..])
            .unwrap(),
    )
}

fn javascript_response(script: &str) -> tiny_http::Response<std::io::Cursor<Vec<u8>>> {
    tiny_http::Response::from_string(script)
        .with_header(
            tiny_http::Header::from_bytes("Content-Type", "text/javascript; charset=utf-8")
                .unwrap(),
        )
        .with_header(tiny_http::Header::from_bytes("X-Content-Type-Options", "nosniff").unwrap())
}

fn diagram_response(html: String, nonce: &str) -> tiny_http::Response<std::io::Cursor<Vec<u8>>> {
    let policy = format!("default-src 'none'; sandbox allow-scripts; script-src 'nonce-{nonce}'; style-src 'unsafe-inline'; img-src data:; font-src data:; connect-src 'none'; frame-src 'none'; object-src 'none'; base-uri 'none'; form-action 'none'");
    html_response(html)
        .with_header(tiny_http::Header::from_bytes("Content-Security-Policy", policy).unwrap())
        .with_header(tiny_http::Header::from_bytes("Referrer-Policy", "no-referrer").unwrap())
        .with_header(tiny_http::Header::from_bytes("X-Content-Type-Options", "nosniff").unwrap())
        .with_header(tiny_http::Header::from_bytes("Cache-Control", "no-store").unwrap())
}

fn isolated_html_response(html: String) -> tiny_http::Response<std::io::Cursor<Vec<u8>>> {
    // srcdoc 继承父文档 CSP；网络 frame 全关仍允许内联 srcdoc，并阻止其跳转到其他预览。
    // iframe 的 sandbox 只授予脚本执行权，不能授予 allow-same-origin。
    let policy = "default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; img-src data: blob:; font-src data:; media-src data: blob:; connect-src 'none'; frame-src 'none'; object-src 'none'; base-uri 'none'; form-action 'none'";
    let mut response = html_response(html);
    for (name, value) in [
        ("Content-Security-Policy", policy),
        ("Referrer-Policy", "no-referrer"),
        ("X-Content-Type-Options", "nosniff"),
        (
            "Permissions-Policy",
            "camera=(), microphone=(), geolocation=(), payment=(), usb=()",
        ),
    ] {
        response = response.with_header(tiny_http::Header::from_bytes(name, value).unwrap());
    }
    response
}

/// 原始字节响应:按扩展名给 Content-Type,认不出的按附件下载。
/// svg 不内联(可内嵌 `<script>`,内联即 XSS),与未知类型一起走附件;
/// 所有响应带 `nosniff`,防浏览器把 octet-stream 嗅成可执行类型。
fn raw_response(filename: &str, bytes: Vec<u8>) -> tiny_http::Response<std::io::Cursor<Vec<u8>>> {
    let ext = file_ext(filename);
    let content_type = match ext.as_str() {
        "md" | "markdown" => "text/markdown; charset=utf-8",
        "json" => "application/json; charset=utf-8",
        "txt" | "log" | "rs" | "py" | "toml" | "yaml" | "yml" | "js" | "ts" | "css" | "c"
        | "cpp" | "h" | "hpp" | "go" | "java" | "sh" | "sql" | "xml" => "text/plain; charset=utf-8",
        "pdf" => "application/pdf",
        other => raster_image_mime(other).unwrap_or("application/octet-stream"),
    };
    let mut resp = tiny_http::Response::from_data(bytes).with_header(
        tiny_http::Header::from_bytes(&b"Content-Type"[..], content_type.as_bytes()).unwrap(),
    );
    resp = resp.with_header(
        tiny_http::Header::from_bytes(&b"X-Content-Type-Options"[..], &b"nosniff"[..]).unwrap(),
    );
    if content_type == "application/octet-stream" {
        // 文件名含引号/控制符时构造不出合法 disposition 头:跳过即可,
        // octet-stream + nosniff 浏览器照样下载,不许 panic 打死服务线程。
        if let Some(h) = attachment_disposition(filename) {
            resp = resp.with_header(h);
        }
    }
    resp
}

/// 附件下载头。文件名只保留可见 ASCII 且不含引号/反斜杠(Header 值的合法字符),
/// 过滤后为空返回 None。
fn attachment_disposition(filename: &str) -> Option<tiny_http::Header> {
    let safe: String = filename
        .chars()
        .filter(|c| c.is_ascii_graphic() && *c != '"' && *c != '\\')
        .collect();
    if safe.is_empty() {
        return None;
    }
    let disposition = format!("attachment; filename=\"{safe}\"");
    tiny_http::Header::from_bytes(&b"Content-Disposition"[..], disposition.as_bytes()).ok()
}

#[cfg(test)]
impl PreviewServer {
    /// 测试用:不起 HTTP 服务,自定义 token 表路径与 base_url。
    /// 测试构造:把 `base_url` 塞进 `public_base_url`,天然走「显式配了就照用」那条分支 ——
    /// 测试路不碰网络接口,也不需要为它开特殊通道。
    /// 家目录视作根目录:只有根目录不给链接,临时目录照常可列。
    pub(crate) fn for_test(store_path: PathBuf, base_url: &str, max_bytes: u64) -> Arc<Self> {
        Self::for_test_with_home(store_path, base_url, max_bytes, PathBuf::from("/"))
    }

    /// 同 [`Self::for_test`],家目录由测试指定。
    pub(crate) fn for_test_with_home(
        store_path: PathBuf,
        base_url: &str,
        max_bytes: u64,
        home: PathBuf,
    ) -> Arc<Self> {
        Arc::new(Self {
            store: Mutex::new(TokenStore::load(store_path, 3600)),
            cfg: PreviewServerConfig {
                public_base_url: base_url.to_string(),
                ..PreviewServerConfig::default()
            },
            max_bytes,
            listening: AtomicBool::new(true),
            protected_dirs: protected_dir_identities(&[home]),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rebinds_after_receive_failure_and_reuses_existing_token() {
        use std::sync::atomic::AtomicUsize;

        struct Listener {
            generation: usize,
            drops: Arc<AtomicUsize>,
        }
        impl Drop for Listener {
            fn drop(&mut self) {
                self.drops.fetch_add(1, Ordering::Relaxed);
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("report.md");
        std::fs::write(&file, "# preserved").unwrap();
        let preview = PreviewServer::for_test(
            dir.path().join("tokens.json"),
            "http://localhost:7765",
            1024,
        );
        let url = preview.mint_open_url(file.to_str().unwrap(), None).unwrap();
        let shutdown = Arc::new(AtomicBool::new(false));
        let drops = Arc::new(AtomicUsize::new(0));
        let mut binds = 0;
        let mut generations = Vec::new();
        let mut failed = false;
        let mut waits = Vec::new();
        serve_loop_with(
            Listener {
                generation: 0,
                drops: Arc::clone(&drops),
            },
            Arc::clone(&preview),
            Arc::clone(&shutdown),
            || {
                assert_eq!(drops.load(Ordering::Relaxed), 1, "先释放旧监听器再绑定");
                binds += 1;
                Ok(Listener {
                    generation: 1,
                    drops: Arc::clone(&drops),
                })
            },
            |listener, _| {
                generations.push(listener.generation);
                if !failed {
                    failed = true;
                    return Err(std::io::Error::from(std::io::ErrorKind::ConnectionAborted));
                }
                shutdown.store(true, Ordering::Relaxed);
                Ok(None)
            },
            |shutdown, delay| {
                waits.push(delay);
                shutdown.load(Ordering::Relaxed)
            },
        );
        assert_eq!(binds, 1, "接收失败后必须重新绑定");
        assert_eq!(waits, [REBIND_DELAY], "接收失败后按基础退避等待一次再绑定");
        assert_eq!(generations, [0, 1], "不能继续轮询失效监听器");
        let token = url.rsplit('/').next().unwrap();
        assert_eq!(preview.handle(&format!("/p/{token}")).status_code(), 200);
        assert_eq!(
            preview.handle(&format!("/p/{token}/raw")).status_code(),
            200
        );
    }

    #[test]
    fn suppresses_links_during_failed_rebind_and_restores_them_after_recovery() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("report.md");
        std::fs::write(&file, "# preserved").unwrap();
        let path = file.to_str().unwrap();
        let preview = PreviewServer::for_test(
            dir.path().join("tokens.json"),
            "http://localhost:7765",
            1024,
        );
        let token = preview.locator_token(path, None).unwrap();
        let markdown = format!("[report]({path})");
        let shutdown = Arc::new(AtomicBool::new(false));
        let mut attempts = 0;
        // 退避节奏按「请求等待多久、等完才绑定」逐步记录核对,不靠真睡满 0.5s + 1s 再量间隔。
        let steps = std::cell::RefCell::new(Vec::new());
        serve_loop_with(
            0,
            Arc::clone(&preview),
            Arc::clone(&shutdown),
            || {
                steps.borrow_mut().push(None);
                attempts += 1;
                assert!(preview.locator_token(path, None).is_none());
                assert_eq!(
                    preview.link_buttons(&markdown, None),
                    (markdown.clone(), vec![])
                );
                assert_eq!(preview.mint_urls_batch(&[path.to_string()], None), [None]);
                if attempts == 1 {
                    anyhow::bail!("address in use");
                }
                Ok(1)
            },
            |generation, _| {
                if *generation == 0 {
                    return Err(std::io::Error::from(std::io::ErrorKind::ConnectionAborted));
                }
                assert_eq!(
                    preview.locator_token(path, None).as_deref(),
                    Some(token.as_str())
                );
                let batch = preview.mint_urls_batch(&[path.to_string()], None);
                assert!(batch[0].is_some());
                assert_eq!(preview.mint_urls_batch(&[path.to_string()], None), batch);
                shutdown.store(true, Ordering::Relaxed);
                Ok(None)
            },
            |shutdown, delay| {
                steps.borrow_mut().push(Some(delay));
                shutdown.load(Ordering::Relaxed)
            },
        );
        assert_eq!(attempts, 2);
        assert_eq!(
            steps.into_inner(),
            [Some(REBIND_DELAY), None, Some(REBIND_DELAY * 2), None],
            "每次绑定前先退避,绑定失败后退避翻倍"
        );
        assert!(
            preview.locator_token(path, None).is_none(),
            "停机后不生成链接"
        );
    }

    #[test]
    fn stops_rebinding_when_shutdown_is_requested() {
        let dir = tempfile::tempdir().unwrap();
        let preview = PreviewServer::for_test(
            dir.path().join("tokens.json"),
            "http://localhost:7765",
            1024,
        );
        let shutdown = Arc::new(AtomicBool::new(false));
        serve_loop_with(
            (),
            Arc::clone(&preview),
            Arc::clone(&shutdown),
            || panic!("停机后不能重新绑定"),
            |_, _| {
                shutdown.store(true, Ordering::Relaxed);
                Err(std::io::Error::from(std::io::ErrorKind::ConnectionAborted))
            },
            wait_for_shutdown,
        );
        assert!(!preview.listening.load(Ordering::Acquire));
    }

    #[test]
    fn stops_without_receiving_from_listener_bound_during_shutdown() {
        let dir = tempfile::tempdir().unwrap();
        let preview = PreviewServer::for_test(
            dir.path().join("tokens.json"),
            "http://localhost:7765",
            1024,
        );
        let shutdown = Arc::new(AtomicBool::new(false));
        let mut receives = 0;
        serve_loop_with(
            (),
            Arc::clone(&preview),
            Arc::clone(&shutdown),
            || {
                shutdown.store(true, Ordering::Relaxed);
                Ok(())
            },
            |_, _| {
                receives += 1;
                Err(std::io::Error::from(std::io::ErrorKind::ConnectionAborted))
            },
            |shutdown, _| shutdown.load(Ordering::Relaxed),
        );
        assert_eq!(receives, 1);
        assert!(!preview.listening.load(Ordering::Acquire));
    }

    #[test]
    fn interrupts_long_rebind_delay_for_shutdown() {
        let shutdown = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&shutdown);
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            done_tx
                .send(wait_for_shutdown(&flag, MAX_REBIND_DELAY))
                .unwrap();
        });
        started_rx.recv().unwrap();
        shutdown.store(true, Ordering::Relaxed);
        assert!(done_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap());
        worker.join().unwrap();
    }

    #[test]
    fn rebind_wait_lasts_the_full_delay_without_shutdown() {
        let delay = std::time::Duration::from_millis(30);
        let started = std::time::Instant::now();
        assert!(!wait_for_shutdown(&AtomicBool::new(false), delay));
        assert!(started.elapsed() >= delay, "没有停机就必须等满退避");
    }

    #[test]
    #[ignore = "回环 HTTP 故障注入：独立监听器，不启动 daemon"]
    fn recovers_http_listener_after_error_and_port_collision() {
        use std::net::TcpListener;

        struct Worker {
            shutdown: Arc<AtomicBool>,
            thread: Option<std::thread::JoinHandle<()>>,
        }
        impl Drop for Worker {
            fn drop(&mut self) {
                self.shutdown.store(true, Ordering::Relaxed);
                self.thread.take().unwrap().join().unwrap();
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("report.md");
        std::fs::write(&file, "# preserved").unwrap();
        let listener = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let address = listener.server_addr().to_ip().unwrap();
        let preview = PreviewServer::for_test(
            dir.path().join("tokens.json"),
            &format!("http://{address}"),
            1024,
        );
        let url = preview.mint_open_url(file.to_str().unwrap(), None).unwrap();
        let shutdown = Arc::new(AtomicBool::new(false));
        let worker_preview = Arc::clone(&preview);
        let worker_shutdown = Arc::clone(&shutdown);
        let blocker = Arc::new(Mutex::new(None::<TcpListener>));
        let worker_blocker = Arc::clone(&blocker);
        let (failed_tx, failed_rx) = std::sync::mpsc::channel();
        let mut injected = false;
        let mut blocked = false;
        let worker = Worker {
            shutdown,
            thread: Some(std::thread::spawn(move || {
                serve_loop_with(
                    listener,
                    worker_preview,
                    worker_shutdown,
                    || {
                        if !blocked {
                            *worker_blocker.lock().unwrap() =
                                Some(TcpListener::bind(address).unwrap());
                            blocked = true;
                            let result = tiny_http::Server::http(address)
                                .map_err(|e| anyhow::anyhow!("{e}"));
                            assert!(result.is_err(), "端口占用必须导致重新绑定失败");
                            failed_tx.send(()).unwrap();
                            return result;
                        }
                        tiny_http::Server::http(address).map_err(|e| anyhow::anyhow!("{e}"))
                    },
                    |server, timeout| {
                        if !injected {
                            injected = true;
                            return Err(std::io::Error::from(
                                std::io::ErrorKind::ConnectionAborted,
                            ));
                        }
                        server.recv_timeout(timeout)
                    },
                    wait_for_shutdown,
                );
            })),
        };
        failed_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        assert!(preview
            .mint_open_url(file.to_str().unwrap(), None)
            .is_none());
        drop(blocker.lock().unwrap().take());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !preview.listening.load(Ordering::Acquire) {
            assert!(std::time::Instant::now() < deadline, "监听未恢复");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let client = reqwest::blocking::Client::builder()
            .no_proxy()
            .timeout(std::time::Duration::from_secs(3))
            .build()
            .unwrap();
        let response = client.get(&url).send().unwrap();
        assert_eq!(response.status(), 200);
        assert!(response.text().unwrap().contains("preserved"));
        let raw = client.get(format!("{url}/raw")).send().unwrap();
        assert_eq!(raw.status(), 200);
        assert_eq!(raw.text().unwrap(), "# preserved");
        assert_eq!(
            preview
                .mint_open_url(file.to_str().unwrap(), None)
                .as_deref(),
            Some(url.as_str())
        );
        drop(worker);
        assert!(!preview.listening.load(Ordering::Acquire));
    }

    #[test]
    fn token_is_32_hex_chars() {
        let t = new_token();
        assert_eq!(t.len(), 32);
        assert!(t.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_ne!(new_token(), t, "两次 mint 不应相同");
    }

    #[test]
    fn well_formed_token_check() {
        assert!(is_well_formed_token("0123456789abcdef0123456789abcdef"));
        assert!(!is_well_formed_token("short"));
        assert!(!is_well_formed_token("g123456789abcdef0123456789abcdef0"));
        assert!(!is_well_formed_token(""));
    }

    #[test]
    fn mint_is_idempotent_per_path() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = TokenStore::load(dir.path().join("tokens.json"), 3600);
        let t1 = store.mint("/tmp/a.md", None);
        let t2 = store.mint("/tmp/a.md", None);
        assert_eq!(t1, t2, "同一路径复用同一 token");
        assert_eq!(
            store.resolve(&t1).map(|(p, _)| p).as_deref(),
            Some("/tmp/a.md")
        );
    }

    /// 新回复里的链接至少还有半个有效期;旧链接按自己的时间过期,清掉它不影响新映射。
    #[test]
    fn reminting_keeps_half_the_lifetime_and_leaves_issued_links_alone() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = TokenStore::load(dir.path().join("tokens.json"), 100);
        let first = store.mint("/tmp/a.md", None);
        store
            .file
            .created_at
            .insert(first.clone(), unix_secs() - 40);
        assert_eq!(store.mint("/tmp/a.md", None), first, "余量够就复用");

        store
            .file
            .created_at
            .insert(first.clone(), unix_secs() - 60);
        let second = store.mint("/tmp/a.md", None);
        assert_ne!(second, first, "余量不足半个有效期就签新的");
        assert!(store.resolve(&first).is_some(), "已发出的旧链接照常可用");

        store
            .file
            .created_at
            .insert(first.clone(), unix_secs() - 200);
        assert!(store.resolve(&first).is_none());
        assert_eq!(
            store.file.by_path.get("/tmp/a.md"),
            Some(&second),
            "旧链接过期不带走新映射"
        );

        // 签新 token 时清掉过期项。
        store
            .file
            .created_at
            .insert(second.clone(), unix_secs() - 200);
        store.mint("/tmp/b.md", None);
        assert!(!store.file.tokens.contains_key(&second));
        assert_eq!(store.file.tokens.len(), 1);
    }

    #[test]
    fn configured_ttl_maps_to_store_seconds() {
        assert_eq!(store_ttl_secs(0), 1);
        assert_eq!(store_ttl_secs(3600), 3600);
        assert_eq!(store_ttl_secs(u64::MAX), i64::MAX);
    }

    #[test]
    fn expired_token_is_removed() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = TokenStore::load(dir.path().join("tokens.json"), 100);
        let t = store.mint("/tmp/a.md", None);
        store.file.created_at.insert(t.clone(), unix_secs() - 200);
        assert!(store.resolve(&t).is_none(), "过期 token 解析为 None");
        assert!(!store.file.tokens.contains_key(&t), "过期即删");
    }

    #[test]
    fn store_survives_reload() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tokens.json");
        let t = {
            let mut store = TokenStore::load(path.clone(), 3600);
            store.mint("/tmp/a.md", None)
        };
        let store = TokenStore::load(path, 3600);
        assert_eq!(
            store.file.tokens.get(&t).map(String::as_str),
            Some("/tmp/a.md")
        );
    }

    #[test]
    fn relativize_strips_base_prefix() {
        assert_eq!(
            relativize("/proj/src/main.rs", "/proj").as_deref(),
            Some("src/main.rs")
        );
        assert_eq!(relativize("/proj/a.md", "/proj/").as_deref(), Some("a.md"));
    }

    #[test]
    fn relativize_returns_none_outside_base() {
        assert_eq!(relativize("/other/x.md", "/proj"), None);
        assert_eq!(relativize("/proj/a.md", ""), None);
        assert_eq!(relativize("/proj/a.md", "  "), None);
    }

    #[test]
    fn mint_with_base_shows_relative_path_in_preview() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.md");
        std::fs::write(&file, "# 标题\n").unwrap();
        let server =
            PreviewServer::for_test(dir.path().join("t.json"), "http://localhost:7765", 1024);
        let url = server
            .mint_open_url(file.to_str().unwrap(), Some(dir.path().to_str().unwrap()))
            .unwrap();
        let token = url.rsplit('/').next().unwrap();
        let page = server.handle(&format!("/p/{token}"));
        assert_eq!(page.status_code(), 200);
        let body = String::from_utf8(page.into_reader().into_inner()).unwrap();
        assert!(
            body.contains(">a.md<") || body.contains("a.md ·"),
            "头部应显示相对路径 a.md:{body}"
        );
        assert!(
            !body.contains(dir.path().to_str().unwrap()),
            "头部不应再含绝对路径前缀:{body}"
        );
    }

    #[test]
    fn unknown_route_404s() {
        let dir = tempfile::tempdir().unwrap();
        let server =
            PreviewServer::for_test(dir.path().join("t.json"), "http://localhost:7765", 1024);
        let resp = server.handle("/other");
        assert_eq!(resp.status_code(), 404);
    }

    #[test]
    fn unknown_token_404s() {
        let dir = tempfile::tempdir().unwrap();
        let server =
            PreviewServer::for_test(dir.path().join("t.json"), "http://localhost:7765", 1024);
        let resp = server.handle("/p/0123456789abcdef0123456789abcdef");
        assert_eq!(resp.status_code(), 404);
    }

    #[test]
    fn diagrams_share_the_file_token_and_keep_scripts_in_sandboxed_pages() {
        use summon_core::file_preview::diagram;
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("chart.md");
        std::fs::write(
            &file,
            "# 图表\n```PLAIN_TEXT\ngraph TB\n A --> B\n```\n```rust\nlet value = 1;\n```\n",
        )
        .unwrap();
        let server = PreviewServer::for_test(
            dir.path().join("tokens.json"),
            "http://localhost:7765",
            100_000,
        );
        let url = server.mint_open_url(file.to_str().unwrap(), None).unwrap();
        let token = url.rsplit('/').next().unwrap();
        let page = server.handle(&format!("/p/{token}"));
        let body = String::from_utf8(page.into_reader().into_inner()).unwrap();
        assert!(body.contains(&format!("src=\"/p/{token}/diagram/0\"")));
        assert!(body.contains("sandbox=\"allow-scripts\""));
        assert!(!body.contains("allow-same-origin"));
        assert!(body.contains(diagram::RESIZE_PATH));
        assert!(!body.contains(diagram::MERMAID_PATH));
        assert!(body.contains("let value = 1;"));
        let response = server.handle(&format!("/p/{token}/diagram/0"));
        assert_eq!(response.status_code(), 200);
        let csp = response
            .headers()
            .iter()
            .find(|header| header.field.equiv("Content-Security-Policy"))
            .unwrap()
            .value
            .as_str();
        for directive in [
            "sandbox allow-scripts",
            "connect-src 'none'",
            "frame-src 'none'",
            "script-src 'nonce-",
        ] {
            assert!(csp.contains(directive));
        }
        assert!(!csp.contains("script-src 'unsafe-inline'"));
        let body = String::from_utf8(response.into_reader().into_inner()).unwrap();
        assert!(body.contains("graph TB\n A --&gt; B"));
        assert!(body.contains(diagram::MERMAID_PATH));
        for path in [
            format!("/p/{token}/diagram/1"),
            format!("/p/{token}/diagram/0/extra"),
            format!("/p/{token}/diagram/-1"),
            "/p/0123456789abcdef0123456789abcdef/diagram/0".into(),
            "/assets/other.js".into(),
        ] {
            assert_eq!(server.handle(&path).status_code(), 404, "{path}");
        }
        for target in [dir.path().to_path_buf(), dir.path().join("ordinary.txt")] {
            if target.extension().is_some() {
                std::fs::write(&target, "```mermaid\ngraph TB; A --> B\n```").unwrap();
            }
            let url = server
                .mint_open_url(target.to_str().unwrap(), None)
                .unwrap();
            assert_eq!(
                server
                    .handle(&format!(
                        "{}/diagram/0",
                        url.strip_prefix("http://localhost:7765").unwrap()
                    ))
                    .status_code(),
                404
            );
        }
        assert_eq!(server.handle(diagram::MERMAID_PATH).status_code(), 200);
        server
            .store
            .lock()
            .unwrap()
            .file
            .created_at
            .insert(token.into(), unix_secs() - 999_999);
        assert_eq!(
            server
                .handle(&format!("/p/{token}/diagram/0"))
                .status_code(),
            404
        );
        std::fs::write(&file, "```text\nordinary code\n```\n").unwrap();
        let ordinary = summon_core::file_preview::html::render_page(
            "chart.md",
            "chart.md",
            0,
            "test",
            b"```text\nordinary code\n```\n",
        );
        assert!(!ordinary.contains("<script"));
        assert!(!ordinary.contains("mermaid-frame\""));
    }

    #[test]
    #[ignore = "manual browser acceptance fixture"]
    fn diagram_browser_fixture() {
        let root = std::path::PathBuf::from(
            std::env::var("SUMMON_PREVIEW_BROWSER_DIR").expect("set fixture directory"),
        );
        std::fs::create_dir_all(&root).unwrap();
        let file = root.join("diagrams.md");
        std::fs::write(&file, r#"# 图表验收

<script>window.summonSourceExecuted = true</script>

```PLAIN_TEXT
graph TB
    A[开始] --> B[完成]
```

```mermaid
sequenceDiagram
    Alice->>Bob: Hello
    Bob-->>Alice: Ready
```

```mermaid
this is not a diagram
```

```mermaid
%%{init: {"securityLevel":"loose", "flowchart":{"htmlLabels":true}}}%%
graph TB
    X["<img src='https://example.invalid/leak' onerror='window.summonSourceExecuted=true'>"] --> Y[SafeSelf]
    X --> Z[SafeDefault]
    X --> R[SafeRelative]
    click Y "https://example.invalid/click-self" _self
    click Z "https://example.invalid/click-default"
    click R "/p/review-relative/raw" _self
```

```rust
fn ordinary_code() {}
```
"#).unwrap();
        let listener = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.server_addr().to_ip().unwrap());
        let preview = PreviewServer::for_test(root.join("tokens.json"), &base, 100_000);
        let url = preview.mint_open_url(file.to_str().unwrap(), None).unwrap();
        std::fs::write(root.join("url.txt"), url).unwrap();
        let shutdown = Arc::new(AtomicBool::new(false));
        let flag = shutdown.clone();
        let worker = std::thread::spawn(move || serve_loop(listener, preview, flag));
        let until = std::time::Instant::now() + std::time::Duration::from_secs(240);
        while !root.join("stop").exists() && std::time::Instant::now() < until {
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        shutdown.store(true, Ordering::Release);
        worker.join().unwrap();
    }

    #[test]
    fn serves_preview_and_raw() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.md");
        std::fs::write(&file, "# 标题\n").unwrap();
        let server =
            PreviewServer::for_test(dir.path().join("t.json"), "http://localhost:7765", 1024);
        let url = server.mint_open_url(file.to_str().unwrap(), None).unwrap();
        let token = url.rsplit('/').next().unwrap();

        let page = server.handle(&format!("/p/{token}"));
        assert_eq!(page.status_code(), 200);
        let body = String::from_utf8(page.into_reader().into_inner()).unwrap();
        assert!(
            body.contains("<h1 id=\"section-1\">标题</h1>"),
            "预览页渲染 markdown:{body}"
        );
        assert!(
            body.contains("<nav class='toc'") && body.contains("href=\"#section-1\">标题</a>"),
            "预览页应包含可点击目录:{body}"
        );
        assert!(
            body.contains(&format!("/p/{token}/raw")),
            "带原文件下载链接:{body}"
        );

        let raw = server.handle(&format!("/p/{token}/raw"));
        assert_eq!(raw.status_code(), 200);
    }

    #[test]
    fn mint_rejects_nonexistent_and_oversized() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("big.md");
        std::fs::write(&file, vec![0u8; 2048]).unwrap();
        let server =
            PreviewServer::for_test(dir.path().join("t.json"), "http://localhost:7765", 1024);
        assert!(server.mint_open_url("/nonexistent/x.md", None).is_none());
        assert!(
            server.mint_open_url(file.to_str().unwrap(), None).is_none(),
            "超上限不 mint"
        );
    }

    #[test]
    fn serve_rechecks_size_and_type_after_mint() {
        // mint 后文件被撑大:serve 侧大小校验挡掉(404);换成目录:目录是合法目标,走列举页(200)。
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("grow.md");
        std::fs::write(&file, b"# hi").unwrap();
        let server =
            PreviewServer::for_test(dir.path().join("t.json"), "http://localhost:7765", 1024);
        let url = server.mint_open_url(file.to_str().unwrap(), None).unwrap();
        let token = url.rsplit('/').next().unwrap();
        assert_eq!(server.handle(&format!("/p/{token}")).status_code(), 200);

        std::fs::write(&file, vec![0u8; 2048]).unwrap();
        assert_eq!(
            server.handle(&format!("/p/{token}")).status_code(),
            404,
            "撑超上限后必须 404"
        );

        std::fs::remove_file(&file).unwrap();
        std::fs::create_dir(&file).unwrap();
        let resp = server.handle(&format!("/p/{token}"));
        assert_eq!(resp.status_code(), 200, "目录是合法目标,返回列举页");
        let body = String::from_utf8(resp.into_reader().into_inner()).unwrap();
        assert!(body.contains("空目录"), "目录列举页:{body}");
    }

    #[test]
    #[ignore = "需要 Node、playwright 与 Chromium；NODE_PATH 指向 playwright 安装目录"]
    fn html_preview_browser_enforces_sandbox_and_content_policy() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("demo.html");
        std::fs::write(
            &file,
            include_bytes!("../../../../tests/fixtures/html-preview.html"),
        )
        .unwrap();
        let listener = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.server_addr());
        let preview = PreviewServer::for_test(dir.path().join("tokens.json"), &base, 1024 * 1024);
        let url = preview.mint_open_url(file.to_str().unwrap(), None).unwrap();
        let page_path = url.strip_prefix(&base).unwrap().to_string();
        let stop = Arc::new(AtomicBool::new(false));
        let done = Arc::clone(&stop);
        let worker = std::thread::spawn(move || {
            let mut unexpected = Vec::new();
            while !done.load(Ordering::Relaxed) {
                if let Some(request) = listener
                    .recv_timeout(std::time::Duration::from_millis(20))
                    .unwrap()
                {
                    if request.url() != page_path
                        && request.url() != format!("{page_path}/raw")
                        && request.url() != "/favicon.ico"
                    {
                        unexpected.push(request.url().to_string());
                    }
                    let response = preview.handle(request.url());
                    request.respond(response).unwrap();
                }
            }
            unexpected
        });
        let result = std::process::Command::new("node")
            .arg(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../tests/html_preview_browser.cjs"
            ))
            .arg(&url)
            .output();
        stop.store(true, Ordering::Relaxed);
        let unexpected = worker.join().unwrap();
        let output = result.unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            unexpected.is_empty(),
            "浏览器不应向服务器请求外部资源或导航：{unexpected:?}"
        );
    }

    #[test]
    fn html_preview_is_isolated_and_raw_remains_an_exact_attachment() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("page.HTML");
        let source = b"<!doctype html><h1>page</h1><script>document.body.dataset.ready=1</script>";
        std::fs::write(&file, source).unwrap();
        let server =
            PreviewServer::for_test(dir.path().join("t.json"), "http://localhost:7765", 1024);
        let url = server.mint_open_url(file.to_str().unwrap(), None).unwrap();
        let token = url.rsplit('/').next().unwrap();
        let page = server.handle(&format!("/p/{token}"));
        let policy = page
            .headers()
            .iter()
            .find(|header| header.field.equiv("Content-Security-Policy"))
            .expect("HTML 预览必须带 CSP")
            .value
            .as_str();
        for directive in [
            "default-src 'none'",
            "script-src 'unsafe-inline'",
            "style-src 'unsafe-inline'",
            "frame-src 'none'",
            "connect-src 'none'",
            "form-action 'none'",
            "base-uri 'none'",
        ] {
            assert!(
                policy.split(';').any(|value| value.trim() == directive),
                "missing {directive}: {policy}"
            );
        }
        assert!(page
            .headers()
            .iter()
            .any(|header| header.field.equiv("Referrer-Policy") && header.value == "no-referrer"));
        let body = String::from_utf8(page.into_reader().into_inner()).unwrap();
        assert!(body.contains("sandbox=\"allow-scripts\""));
        let raw = server.handle(&format!("/p/{token}/raw"));
        assert!(raw
            .headers()
            .iter()
            .any(|header| header.field.equiv("Content-Disposition")
                && header.value.as_str().starts_with("attachment;")));
        assert_eq!(raw.into_reader().into_inner(), source);
        assert_eq!(
            server
                .handle(&format!("/p/{token}/style.css"))
                .status_code(),
            404
        );
    }

    #[test]
    fn svg_is_served_as_attachment_not_inline() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("x.svg");
        std::fs::write(
            &file,
            b"<svg xmlns='http://www.w3.org/2000/svg'><script>alert(1)</script></svg>",
        )
        .unwrap();
        let server =
            PreviewServer::for_test(dir.path().join("t.json"), "http://localhost:7765", 1024);
        let url = server.mint_open_url(file.to_str().unwrap(), None).unwrap();
        let token = url.rsplit('/').next().unwrap();
        let resp = server.handle(&format!("/p/{token}/raw"));
        assert_eq!(resp.status_code(), 200);
        let headers = resp.headers();
        let attachment = headers.iter().any(|h| {
            h.field.equiv("Content-Disposition") && h.value == "attachment; filename=\"x.svg\""
        });
        let nosniff = headers
            .iter()
            .any(|h| h.field.equiv("X-Content-Type-Options"));
        assert!(attachment, "svg 必须走附件下载,不内联:{headers:?}");
        assert!(nosniff, "/raw 统一带 nosniff:{headers:?}");
    }

    #[test]
    fn image_preview_page_shows_image_served_inline_by_raw() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("chart.png");
        std::fs::write(&file, [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]).unwrap();
        let server =
            PreviewServer::for_test(dir.path().join("t.json"), "http://localhost:7765", 1024);
        let url = server.mint_open_url(file.to_str().unwrap(), None).unwrap();
        let token = url.rsplit('/').next().unwrap();

        let page = server.handle(&format!("/p/{token}"));
        assert_eq!(page.status_code(), 200);
        let body = String::from_utf8(page.into_reader().into_inner()).unwrap();
        assert!(
            body.contains(&format!("<img src=\"/p/{token}/raw\"")),
            "预览页直接显示图片:{body}"
        );

        let raw = server.handle(&format!("/p/{token}/raw"));
        let headers = raw.headers();
        assert!(
            headers
                .iter()
                .any(|h| h.field.equiv("Content-Type") && h.value == "image/png"),
            "{headers:?}"
        );
        assert!(
            !headers.iter().any(|h| h.field.equiv("Content-Disposition")),
            "位图内联服务,不走附件:{headers:?}"
        );
    }

    #[test]
    fn preview_page_and_raw_agree_on_extension_of_dotfile_name() {
        // 文件名整个就是 `.png`:预览页判成图片、`<img>` 指向 `/raw`,`/raw` 就必须按图片服务。
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join(".png");
        std::fs::write(&file, [0x89, b'P', b'N', b'G']).unwrap();
        let server =
            PreviewServer::for_test(dir.path().join("t.json"), "http://localhost:7765", 1024);
        let url = server.mint_open_url(file.to_str().unwrap(), None).unwrap();
        let token = url.rsplit('/').next().unwrap();

        let body = String::from_utf8(
            server
                .handle(&format!("/p/{token}"))
                .into_reader()
                .into_inner(),
        )
        .unwrap();
        assert!(body.contains("<img src="), "{body}");
        let raw = server.handle(&format!("/p/{token}/raw"));
        assert!(
            raw.headers()
                .iter()
                .any(|h| h.field.equiv("Content-Type") && h.value == "image/png"),
            "{:?}",
            raw.headers()
        );
    }

    #[test]
    fn raw_with_quote_in_filename_does_not_panic() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a\"b.md");
        std::fs::write(&file, b"# hi").unwrap();
        let server =
            PreviewServer::for_test(dir.path().join("t.json"), "http://localhost:7765", 1024);
        let url = server.mint_open_url(file.to_str().unwrap(), None).unwrap();
        let token = url.rsplit('/').next().unwrap();
        let resp = server.handle(&format!("/p/{token}/raw"));
        assert_eq!(resp.status_code(), 200, "引号文件名不该打死服务线程");
    }

    #[test]
    fn mints_and_serves_dir_listing() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.md"), b"# hi").unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        let server =
            PreviewServer::for_test(dir.path().join("t.json"), "http://localhost:7765", 1024);
        let url = server
            .mint_open_url(dir.path().to_str().unwrap(), None)
            .unwrap();
        let token = url.rsplit('/').next().unwrap();

        let resp = server.handle(&format!("/p/{token}"));
        assert_eq!(resp.status_code(), 200);
        let body = String::from_utf8(resp.into_reader().into_inner()).unwrap();
        assert!(body.contains("📁 sub</td>"), "子目录只显示名字:{body}");
        assert!(body.contains(">a.md<"), "文件条目:{body}");
        // 文件链接是 mint 出来的预览 URL,不是裸路径;子目录不给链接。
        assert!(body.contains("http://localhost:7765/p/"), "文件链接:{body}");
        assert!(!body.contains(">sub</a>"), "子目录不是链接:{body}");
        assert!(!body.contains(".."), "不提供父目录导航:{body}");

        // 目录的 raw 不存在。
        let raw = server.handle(&format!("/p/{token}/raw"));
        assert_eq!(raw.status_code(), 404, "目录没有 raw");
    }

    #[test]
    fn empty_dir_lists_as_empty() {
        let dir = tempfile::tempdir().unwrap();
        // token 存盘放在被列举目录之外,否则存盘文件会让「空目录」不空。
        let empty = dir.path().join("empty");
        std::fs::create_dir(&empty).unwrap();
        let server =
            PreviewServer::for_test(dir.path().join("t.json"), "http://localhost:7765", 1024);
        let url = server.mint_open_url(empty.to_str().unwrap(), None).unwrap();
        let token = url.rsplit('/').next().unwrap();
        let resp = server.handle(&format!("/p/{token}"));
        let body = String::from_utf8(resp.into_reader().into_inner()).unwrap();
        assert!(body.contains("空目录"), "{body}");
    }

    #[test]
    fn dir_listing_escapes_entry_names() {
        // 文件名含 HTML 特殊字符:列举页必须转义,不许原始标签进 DOM(spec 硬约束的守门测试)。
        let dir = tempfile::tempdir().unwrap();
        let listed = dir.path().join("listed");
        std::fs::create_dir(&listed).unwrap();
        std::fs::write(listed.join("<img src=x onerror=alert(1)>.txt"), b"x").unwrap();
        let server =
            PreviewServer::for_test(dir.path().join("t.json"), "http://localhost:7765", 1024);
        let url = server
            .mint_open_url(listed.to_str().unwrap(), None)
            .unwrap();
        let token = url.rsplit('/').next().unwrap();
        let resp = server.handle(&format!("/p/{token}"));
        let body = String::from_utf8(resp.into_reader().into_inner()).unwrap();
        assert!(!body.contains("<img src=x"), "原始标签不许进 DOM:{body}");
        assert!(body.contains("&lt;img"), "文件名必须转义:{body}");
    }

    #[test]
    fn dir_listing_truncates_at_200_boundary() {
        // 临界值 199/200/201:200 是「正好不截断」的边界,201 才截断(off-by-one 守门)。
        // 三档共用一个目录逐档补文件,每档换全新的 token 表:建文件是这条的大头,不必建三遍。
        let dir = tempfile::tempdir().unwrap();
        let listed = dir.path().join("listed");
        std::fs::create_dir(&listed).unwrap();
        let mut created = 0;
        for count in [199usize, 200, 201] {
            for i in created..count {
                std::fs::write(listed.join(format!("f{i:04}.md")), b"x").unwrap();
            }
            created = count;
            let server = PreviewServer::for_test(
                dir.path().join(format!("t-{count}.json")),
                "http://localhost:7765",
                1024,
            );
            let url = server
                .mint_open_url(listed.to_str().unwrap(), None)
                .unwrap();
            let token = url.rsplit('/').next().unwrap();
            let resp = server.handle(&format!("/p/{token}"));
            let body = String::from_utf8(resp.into_reader().into_inner()).unwrap();
            let rendered = body.matches("href=\"http://localhost:7765/p/").count();
            if count <= 200 {
                assert_eq!(rendered, count, "{count} 项应全显示:{body}");
                assert!(!body.contains("只显示前"), "{count} 项不该截断:{body}");
            } else {
                assert_eq!(rendered, 200, "{count} 项只显示前 200:{body}");
                assert!(body.contains("只显示前"), "{count} 项必须有截断提示:{body}");
            }
        }
    }

    /// 家目录、它的上级(含根目录)及其各种别名不签链接,token 表里已有的这类项服务时也打不开;
    /// 目录页只给本层的常规文件链接,子目录与指向目录的符号链接只显示名字,以 `.` 开头的条目不列。
    #[test]
    fn directory_links_cover_one_level_and_never_root_or_home() {
        use std::os::unix::fs::symlink;
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let project = home.join("project");
        let other = tmp.path().join("other");
        std::fs::create_dir_all(project.join("sub")).unwrap();
        std::fs::create_dir_all(project.join(".ssh")).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(project.join("a.md"), b"# a").unwrap();
        std::fs::write(project.join(".env"), b"SECRET=1").unwrap();
        std::fs::write(project.join("sub").join("b.md"), b"# b").unwrap();
        std::fs::write(other.join("secret.txt"), b"s").unwrap();
        symlink(&other, project.join("linkdir")).unwrap();
        symlink(project.join(".ssh"), project.join("ssh")).unwrap();
        symlink(&home, tmp.path().join("home-alias")).unwrap();
        let server = PreviewServer::for_test_with_home(
            tmp.path().join("t.json"),
            "http://localhost:7765",
            1024,
            home.clone(),
        );

        let blocked = [
            "/".to_string(),
            tmp.path().display().to_string(),
            home.display().to_string(),
            format!("{}/", home.display()),
            format!("{}/..", project.display()),
            tmp.path().join("home-alias").display().to_string(),
        ];
        for path in &blocked {
            assert!(server.mint_open_url(path, None).is_none(), "{path}");
        }
        // macOS firmlink:同一目录、不经符号链接的另一条路径,canonicalize 不会归一。
        #[cfg(target_os = "macos")]
        {
            let canonical = std::fs::canonicalize(&home).unwrap();
            let firmlink = format!("/System/Volumes/Data{}", canonical.display());
            assert!(std::path::Path::new(&firmlink).exists(), "{firmlink}");
            assert!(
                server.mint_open_url(&firmlink, None).is_none(),
                "{firmlink}"
            );
        }
        let md = "以 `/` 开头的内容";
        assert_eq!(server.link_buttons(md, None), (md.to_string(), vec![]));
        assert_eq!(
            server.mint_urls_batch(&[other.display().to_string()], None),
            [None],
            "列表页的批量签发只给文件"
        );

        let url = server
            .mint_open_url(project.to_str().unwrap(), None)
            .unwrap();
        let token = url.rsplit('/').next().unwrap();
        let resp = server.handle(&format!("/p/{token}"));
        assert_eq!(resp.status_code(), 200);
        let body = String::from_utf8(resp.into_reader().into_inner()).unwrap();
        assert!(body.contains(">a.md<"), "{body}");
        for dir in ["sub", "linkdir", "ssh"] {
            assert!(body.contains(&format!("📁 {dir}</td>")), "{dir}:{body}");
        }
        assert!(!body.contains(".env") && !body.contains(".ssh"), "{body}");
        // 页面上的每个链接都只打开常规文件:目录没有 raw。
        let tokens: Vec<&str> = body
            .split("href=\"http://localhost:7765/p/")
            .skip(1)
            .map(|link| &link[..link.find('"').unwrap()])
            .collect();
        assert_eq!(tokens.len(), 1, "{body}");
        let raw = server.handle(&format!("/p/{}/raw", tokens[0]));
        assert_eq!(raw.status_code(), 200);
        assert_eq!(raw.into_reader().into_inner(), b"# a");

        for stored in ["/", home.to_str().unwrap()] {
            let token = server.store.lock().unwrap().mint(stored, None);
            let resp = server.handle(&format!("/p/{token}"));
            assert_eq!(resp.status_code(), 404, "{stored}");
        }
    }

    /// `$HOME` 与账户里的家目录不一致时两条链都保护。
    #[test]
    fn protected_identities_merge_every_home_chain() {
        let tmp = tempfile::tempdir().unwrap();
        let (first, second) = (tmp.path().join("h1"), tmp.path().join("h2"));
        std::fs::create_dir_all(first.join("sub")).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        let protected = protected_dir_identities(&[first.clone(), second.clone()]);
        for dir in [
            first.as_path(),
            second.as_path(),
            tmp.path(),
            Path::new("/"),
        ] {
            assert!(protected.contains(&dir_identity(dir).unwrap()), "{dir:?}");
        }
        assert!(!protected.contains(&dir_identity(&first.join("sub")).unwrap()));
    }

    fn body_of(resp: tiny_http::Response<std::io::Cursor<Vec<u8>>>) -> String {
        String::from_utf8(resp.into_reader().into_inner()).unwrap()
    }

    fn token_of(url: &str) -> String {
        url.rsplit('/').next().unwrap().to_string()
    }

    fn get(server: &PreviewServer, url: &str) -> (u16, String) {
        let resp = server.handle(url);
        (resp.status_code().0, body_of(resp))
    }

    /// 回复卡按钮里的定位 token 在 HTTP 上什么也打不开;能打开的是判权通过后新签的那一条,
    /// 每次新签、互不相同,也只认只读请求。
    #[test]
    fn a_button_token_only_locates_and_each_open_link_is_freshly_issued() {
        use tiny_http::Method;
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.md");
        std::fs::write(&file, b"# secret").unwrap();
        let server =
            PreviewServer::for_test(dir.path().join("t.json"), "http://localhost:7765", 1024);
        let locator = server
            .locator_token(file.to_str().unwrap(), Some(dir.path().to_str().unwrap()))
            .unwrap();
        for route in ["", "/raw", "/diagram/0"] {
            assert_eq!(
                get(&server, &format!("/p/{locator}{route}")).0,
                404,
                "{route}"
            );
        }

        let first = server.open_link(&locator).unwrap();
        assert_eq!(first.label, "a.md");
        assert!(!first.is_dir);
        assert_eq!(first.ttl_secs, 3600);
        let second = server.open_link(&locator).unwrap();
        assert_ne!(first.url, second.url, "每次私聊都新签一条");
        assert_ne!(token_of(&first.url), locator);
        let page = format!("/p/{}", token_of(&first.url));
        let (status, body) = get(&server, &page);
        assert_eq!(status, 200);
        assert!(body.contains("secret"), "{body}");
        assert_eq!(get(&server, &format!("{page}/raw")).1, "# secret");
        assert!(get(&server, &format!("/p/{}", token_of(&second.url)))
            .1
            .contains("secret"));
        for method in [Method::Post, Method::Put, Method::Delete] {
            assert_eq!(
                server.handle_request(&method, &page).status_code(),
                404,
                "只认只读请求:{method}"
            );
        }
        assert!(
            server.open_link(&token_of(&first.url)).is_some(),
            "打开 token 本身也能定位,私聊链接被转出去的残余风险不因此变大"
        );
        assert!(server.open_link(&"0".repeat(32)).is_none());
        assert!(server.open_link("not-a-token").is_none());
    }

    /// 点击那一刻按签发时的判据再查一遍:文件没了、超了大小上限、服务暂不可用都不签。
    #[test]
    fn an_open_link_is_issued_only_while_the_file_is_still_previewable() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.md");
        std::fs::write(&file, b"# a").unwrap();
        let server = PreviewServer::for_test(dir.path().join("t.json"), "http://localhost:7765", 8);
        let locator = server.locator_token(file.to_str().unwrap(), None).unwrap();
        server.listening.store(false, Ordering::Release);
        assert!(server.open_link(&locator).is_none(), "服务不在听");
        server.listening.store(true, Ordering::Release);
        std::fs::write(&file, b"# grown past the limit").unwrap();
        assert!(server.open_link(&locator).is_none(), "超了大小上限");
        std::fs::remove_file(&file).unwrap();
        assert!(server.open_link(&locator).is_none(), "文件已不在");
    }

    /// 定位 token 与打开 token 都按 token 表的有效期过期;过期后重新点按钮。
    #[test]
    fn locators_and_open_links_expire_with_the_token_table() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.md");
        std::fs::write(&file, b"# a").unwrap();
        let server =
            PreviewServer::for_test(dir.path().join("t.json"), "http://localhost:7765", 1024);
        let locator = server.locator_token(file.to_str().unwrap(), None).unwrap();
        let open = token_of(&server.open_link(&locator).unwrap().url);
        {
            let mut store = server.store.lock().unwrap();
            let stale = unix_secs() - store.ttl_secs - 1;
            store.file.created_at.insert(open.clone(), stale);
            store.file.created_at.insert(locator.clone(), stale);
        }
        assert_eq!(get(&server, &format!("/p/{open}")).0, 404);
        assert!(server.open_link(&locator).is_none());
        let store = server.store.lock().unwrap();
        assert!(!store.file.open.contains(&open), "过期的打开 token 清出表");
    }

    /// 目录页上的文件没有自己的按钮:打开了目录的人直接拿到这一层文件的打开链接;同一目录页
    /// 反复刷新复用同一批链接,不撑爆 token 表。
    #[test]
    fn an_open_directory_lists_files_whose_links_open() {
        let dir = tempfile::tempdir().unwrap();
        let listed = dir.path().join("listed");
        std::fs::create_dir(&listed).unwrap();
        std::fs::write(listed.join("b.md"), b"# b").unwrap();
        let server =
            PreviewServer::for_test(dir.path().join("t.json"), "http://localhost:7765", 1024);
        let locator = server
            .locator_token(listed.to_str().unwrap(), None)
            .unwrap();
        let link = server.open_link(&locator).unwrap();
        assert!(link.is_dir);
        let page = format!("/p/{}", token_of(&link.url));
        let child_of = |listing: &str| {
            let link = listing
                .split("href=\"http://localhost:7765/p/")
                .nth(1)
                .unwrap();
            link[..link.find('"').unwrap()].to_string()
        };
        let child = child_of(&get(&server, &page).1);
        assert!(get(&server, &format!("/p/{child}")).1.contains("b.md"));
        assert_eq!(child_of(&get(&server, &page).1), child, "刷新复用同一条");
        let file_locator = server
            .locator_token(listed.join("b.md").to_str().unwrap(), None)
            .unwrap();
        assert_ne!(
            file_locator, child,
            "目录页里的打开链接不是按钮里的定位 token"
        );
        assert_eq!(get(&server, &format!("/p/{file_locator}")).0, 404);
    }

    #[test]
    fn link_buttons_label_files_and_keep_the_reply_text() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        std::fs::create_dir_all(project.join("docs")).unwrap();
        std::fs::write(project.join("docs").join("plan.md"), b"# p").unwrap();
        let server =
            PreviewServer::for_test(dir.path().join("t.json"), "http://localhost:7765", 1024);
        let plan = project.join("docs").join("plan.md");
        let md = format!(
            "看 `{}` 与 [目录]({}),再看 `{}`",
            plan.display(),
            project.join("docs").display(),
            plan.display()
        );
        let (text, buttons) = server.link_buttons(&md, project.to_str());
        assert!(text.contains(&format!("`{}`", plan.display())), "{text}");
        assert!(text.contains("与 目录,"), "{text}");
        assert_eq!(buttons.len(), 2, "同一文件只出一个按钮");
        assert_eq!(buttons[0].label, "docs/plan.md");
        assert!(!buttons[0].is_dir);
        assert_eq!(buttons[1].label, "docs");
        assert!(buttons[1].is_dir);
        assert_eq!(get(&server, &format!("/p/{}", buttons[0].token)).0, 404);
    }
}
