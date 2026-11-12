//! 控制台的静态资源:编译期嵌进二进制的 `dashboard/dist/`(由 build.rs 生成文件表),以及
//! dist 缺页时顶上的几张内置页面。内置页面不含任何数据,脚本与样式都走外链(CSP 不放行内联)。

// 生成的文件定义 `static DIST: &[(&str, &str, &[u8])]`:(相对 dist 的路径, Content-Type, 内容)。
include!(concat!(env!("OUT_DIR"), "/dashboard_assets.rs"));

/// 一份静态资源。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Asset {
    pub(super) ctype: &'static str,
    pub(super) bytes: &'static [u8],
}

fn localized_page(chinese: &'static str, english: &'static str) -> &'static str {
    match summon_core::i18n::language() {
        summon_core::i18n::Language::Chinese => chinese,
        summon_core::i18n::Language::English => english,
    }
}

const HTML: &str = "text/html; charset=utf-8";

fn lookup(
    table: &'static [(&'static str, &'static str, &'static [u8])],
    path: &str,
) -> Option<Asset> {
    table
        .iter()
        .find(|(p, _, _)| *p == path)
        .map(|(_, ctype, bytes)| Asset { ctype, bytes })
}

/// dist 里的一个文件(路径相对 `dashboard/dist/`,如 `assets/index-1a2b3c.js`)。
pub(super) fn dist(path: &str) -> Option<Asset> {
    lookup(DIST, path)
}

/// 前端的三张入口页。dist 里有就用它的,没有就用内置的最小页面(落地页在 dist 里叫 `waiting.html`)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Page {
    /// 单页应用入口(已认证)。
    Index,
    /// 远程链接的落地页:表单 POST 到 `/l/<链接>/enter` 进入(见 [`landing_page`])。
    Landing,
    /// 本机码落地页:POST 进入。
    Local,
}

impl Page {
    fn file(self) -> &'static str {
        match (self, summon_core::i18n::language()) {
            (Self::Index, summon_core::i18n::Language::English) => "index.en.html",
            (Self::Landing, summon_core::i18n::Language::English) => "waiting.en.html",
            (Self::Local, summon_core::i18n::Language::English) => "local.en.html",
            (Self::Index, _) => "index.html",
            (Self::Landing, _) => "waiting.html",
            (Self::Local, _) => "local.html",
        }
    }

    fn builtin(self) -> &'static str {
        match self {
            Self::Index => localized_page(
                BUILTIN_INDEX,
                include_str!("../../../../../locales/pages/en/index.html"),
            ),
            Self::Landing => localized_page(
                BUILTIN_LANDING,
                include_str!("../../../../../locales/pages/en/waiting.html"),
            ),
            Self::Local => localized_page(
                BUILTIN_LOCAL,
                include_str!("../../../../../locales/pages/en/local.html"),
            ),
        }
    }
}

fn page_from(table: &'static [(&'static str, &'static str, &'static [u8])], page: Page) -> Asset {
    lookup(table, page.file()).unwrap_or(Asset {
        ctype: HTML,
        bytes: page.builtin().as_bytes(),
    })
}

pub(super) fn page(page: Page) -> Asset {
    page_from(DIST, page)
}

/// 落地页里表单地址的占位,发页时换成这条链接的 token(token 只有十六进制字符,放进属性里不用转义)。
pub(super) const LINK_MARKER: &str = "__SUMMON_LINK__";

/// 远程链接的落地页:表单提交到这条链接自己的 `/l/<token>/enter`,不靠脚本也能进入。
pub(super) fn landing_page(link: &str) -> Vec<u8> {
    landing_from(DIST, link)
}

fn landing_from(
    table: &'static [(&'static str, &'static str, &'static [u8])],
    link: &str,
) -> Vec<u8> {
    String::from_utf8_lossy(page_from(table, Page::Landing).bytes)
        .replace(LINK_MARKER, link)
        .into_bytes()
}

/// 未认证时打开控制台看到的提示页。
pub(super) fn hint_page() -> Asset {
    Asset {
        ctype: HTML,
        bytes: localized_page(
            HINT_PAGE,
            include_str!("../../../../../locales/pages/en/hint.html"),
        )
        .as_bytes(),
    }
}

/// 链接或本机码不认识、已过期、已用过。
pub(super) fn invalid_page() -> Asset {
    Asset {
        ctype: HTML,
        bytes: localized_page(
            INVALID_PAGE,
            include_str!("../../../../../locales/pages/en/invalid.html"),
        )
        .as_bytes(),
    }
}

/// 签发人此刻已没有操作权限。
pub(super) fn forbidden_page() -> Asset {
    Asset {
        ctype: HTML,
        bytes: localized_page(
            FORBIDDEN_PAGE,
            include_str!("../../../../../locales/pages/en/forbidden.html"),
        )
        .as_bytes(),
    }
}

/// 内置页面用的脚本与样式,挂在 `/_/` 下。不含任何数据,公开。
pub(super) fn builtin(name: &str) -> Option<Asset> {
    let (ctype, body) = match name {
        "enter.js" => ("text/javascript; charset=utf-8", ENTER_JS),
        "page.css" => ("text/css; charset=utf-8", PAGE_CSS),
        _ => return None,
    };
    Some(Asset {
        ctype,
        bytes: body.as_bytes(),
    })
}

const HINT_PAGE: &str = include_str!("../../../../../locales/pages/zh-CN/hint.html");
const INVALID_PAGE: &str = include_str!("../../../../../locales/pages/zh-CN/invalid.html");
const FORBIDDEN_PAGE: &str = include_str!("../../../../../locales/pages/zh-CN/forbidden.html");
const BUILTIN_INDEX: &str = include_str!("../../../../../locales/pages/zh-CN/index.html");
const BUILTIN_LANDING: &str = include_str!("../../../../../locales/pages/zh-CN/waiting.html");
const BUILTIN_LOCAL: &str = include_str!("../../../../../locales/pages/zh-CN/local.html");

/// 落地页与本机码页的「进入」:页面加载后替人提交表单;没有脚本时人点按钮也一样。
const ENTER_JS: &str = r#"(function () {
  var form = document.getElementById("enter");
  if (form) form.submit();
})();
"#;

const PAGE_CSS: &str = r#"body {
  margin: 0;
  font: 14px/1.6 -apple-system, "PingFang SC", "Segoe UI", "Microsoft YaHei", sans-serif;
  color: #1f2328;
  background: #ffffff;
}
main { max-width: 32rem; margin: 20vh auto; padding: 0 16px; text-align: center; }
h1 { font-size: 20px; font-weight: 600; }
.note { color: #6e7781; font-size: 13px; }
code { font: 12px ui-monospace, SFMono-Regular, Menlo, monospace; }
button { font: inherit; min-height: 44px; padding: 0 24px; border-radius: 8px;
  border: 1px solid #d0d7de; background: #f6f8fa; color: inherit; cursor: pointer; }
@media (prefers-color-scheme: dark) {
  body { color: #e6edf3; background: #0d1117; }
  .note { color: #8b949e; }
  button { border-color: #30363d; background: #161b22; }
}
"#;

#[cfg(test)]
mod tests {
    use super::*;

    static TABLE: &[(&str, &str, &[u8])] = &[
        ("index.html", HTML, b"<!doctype html><title>app</title>"),
        (
            "assets/index-1a2b3c4d.js",
            "text/javascript; charset=utf-8",
            b"console.log(1)",
        ),
        (
            "assets/index-5e6f7a8b.css",
            "text/css; charset=utf-8",
            b"body{}",
        ),
    ];

    #[test]
    fn assets_table_serves_index_and_hashed_files() {
        assert_eq!(
            page_from(TABLE, Page::Index).bytes,
            b"<!doctype html><title>app</title>"
        );
        let js = lookup(TABLE, "assets/index-1a2b3c4d.js").unwrap();
        assert_eq!(js.ctype, "text/javascript; charset=utf-8");
        assert!(lookup(TABLE, "assets/missing.js").is_none());
        assert!(
            lookup(TABLE, "/index.html").is_none(),
            "路径相对 dist,不带前导斜杠"
        );

        // dist 缺页时回落内置页面;落地页与本机码页的脚本都走外链。
        let landing = page_from(TABLE, Page::Landing);
        assert_eq!(landing.ctype, HTML);
        let landing = std::str::from_utf8(landing.bytes).unwrap();
        assert!(landing.contains("<script src=\"/_/enter.js\">"));
        assert!(!page_from(&[], Page::Index).bytes.is_empty());
        let local = std::str::from_utf8(page_from(&[], Page::Local).bytes).unwrap();
        assert!(local.contains("method=\"post\"") && local.contains("/_/enter.js"));
    }

    /// 落地页的表单提交到这条链接自己的 `/l/<token>/enter`:内置页与打包进来的前端页都一样,
    /// 不靠脚本也能进入。
    #[test]
    fn the_landing_page_posts_to_its_own_enter_path() {
        let link = "0123456789abcdef0123456789abcdef";
        let expected = format!("action=\"/l/{link}/enter\"");
        for table in [&[][..], DIST] {
            let page = String::from_utf8(landing_from(table, link)).unwrap();
            assert!(page.contains(&expected), "{page}");
            assert!(page.contains("method=\"post\""), "{page}");
            assert!(!page.contains(LINK_MARKER), "{page}");
        }
        if let Some(dist) = dist("waiting.html") {
            let dist = std::str::from_utf8(dist.bytes).unwrap();
            assert_eq!(
                dist.matches(LINK_MARKER).count(),
                1,
                "前端落地页要留一处占位"
            );
            assert!(
                !dist.contains("正在确认身份"),
                "前端落地页是进入页,不是等待页"
            );
        }
    }

    /// 生成的表不收点文件(`.gitkeep` 之类),路径一律相对 dist。
    #[test]
    fn the_embedded_dist_skips_dotfiles() {
        for (path, ctype, _) in DIST {
            assert!(!path.starts_with('/'), "{path}");
            assert!(!path.split('/').any(|seg| seg.starts_with('.')), "{path}");
            assert!(!ctype.is_empty());
        }
    }

    /// 打进前端产物的第三方代码(Lit)与图标(Lucide)的许可证要求随产物附上声明:
    /// 声明合成在 dist 里,跟着 dist 编进二进制,并能在 `/assets/` 下取到。
    #[test]
    fn the_embedded_dist_carries_third_party_licenses() {
        let notice = dist("assets/third-party-licenses.txt").expect("dist 里要有第三方许可声明");
        assert_eq!(notice.ctype, "text/plain; charset=utf-8");
        let text = std::str::from_utf8(notice.bytes).unwrap();
        assert!(
            text.contains("Copyright (c) 2017 Google LLC"),
            "缺 Lit 的声明"
        );
        assert!(
            text.contains("Lucide Icons and Contributors"),
            "缺 Lucide 的声明"
        );
        assert!(text.contains("Cole Bemis"), "缺 Feather 的声明");
    }

    /// 内置页面没有内联脚本与样式(CSP 是 `script-src 'self'; style-src 'self'`)。
    #[test]
    fn builtin_pages_carry_no_inline_script_or_style() {
        for page in [
            HINT_PAGE,
            INVALID_PAGE,
            FORBIDDEN_PAGE,
            BUILTIN_INDEX,
            BUILTIN_LANDING,
            BUILTIN_LOCAL,
        ] {
            assert!(!page.contains("<style"), "{page}");
            assert!(!page.contains("style="), "{page}");
            for script in page.split("<script").skip(1) {
                assert!(script.trim_start().starts_with("src="), "{page}");
            }
        }
        assert!(builtin("enter.js").is_some() && builtin("nope.js").is_none());
    }
}
