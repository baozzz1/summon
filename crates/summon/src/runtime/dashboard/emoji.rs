//! 飞书表情图。控制台里的表情画飞书官方图,由控制台同源提供:浏览器只访问控制台自己(CSP 不放行外部
//! 资源,内网与离线照样能用,也不向外部泄露访问)。一张图第一次被用到时 daemon 从飞书的 CDN 取来,存进
//! `~/.summon/run/dashboard-emoji/`,之后都从盘上给。
//!
//! 表情键到图片地址的对照表与前端共用一份(`dashboard/src/preview/feishu-emoji.json`,由
//! `dashboard/scripts/feishu-emoji.mjs` 从飞书开放平台的「表情文案说明」生成)。只取表里的地址,
//! 浏览器给的键只用来查表。

use std::collections::HashMap;
use std::io::Read as _;
use std::path::Path;
use std::sync::LazyLock;
use std::time::Duration;

use anyhow::{bail, Context};

const TABLE_JSON: &str = include_str!("../../../../../dashboard/src/preview/feishu-emoji.json");

static TABLE: LazyLock<HashMap<String, String>> =
    LazyLock::new(|| serde_json::from_str(TABLE_JSON).unwrap_or_default());

/// 一张表情图最多这么大(飞书的表情图都在几十 KB 以内)。
const MAX_BYTES: u64 = 256 * 1024;
const FETCH_TIMEOUT: Duration = Duration::from_secs(10);
const PNG_MAGIC: &[u8] = b"\x89PNG\r\n\x1a\n";

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum LoadError {
    /// 对照表里没有这个键。
    Unknown,
    /// 盘上没有,取也没取到。
    Unavailable(String),
}

/// 盘上有就读盘,没有就用 `fetch` 按对照表里的地址取来,确认是 PNG 再落盘。
pub(crate) fn load(
    dir: &Path,
    key: &str,
    fetch: impl FnOnce(&str) -> anyhow::Result<Vec<u8>>,
) -> Result<Vec<u8>, LoadError> {
    // 文件名用表里的键,不用浏览器给的串。
    let (key, url) = TABLE.get_key_value(key).ok_or(LoadError::Unknown)?;
    let path = dir.join(format!("{key}.png"));
    if let Ok(bytes) = std::fs::read(&path) {
        if bytes.starts_with(PNG_MAGIC) {
            return Ok(bytes);
        }
    }
    let bytes = fetch(url).map_err(|e| LoadError::Unavailable(format!("{e:#}")))?;
    if !bytes.starts_with(PNG_MAGIC) {
        return Err(LoadError::Unavailable(
            summon_i18n::text!("r77fc0cefa2").into(),
        ));
    }
    // 落盘失败只是下次还得再取,这张图照样给出去。
    let _ = store(dir, key, &bytes);
    Ok(bytes)
}

fn store(dir: &Path, key: &str, bytes: &[u8]) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(".{key}.png.{}.tmp", std::process::id()));
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, dir.join(format!("{key}.png")))
}

/// 从飞书的 CDN 取一张图:有时限、限大小、只收图片。
pub(crate) fn fetch_from_cdn(url: &str) -> anyhow::Result<Vec<u8>> {
    static CLIENT: LazyLock<Option<reqwest::blocking::Client>> = LazyLock::new(|| {
        reqwest::blocking::Client::builder()
            .timeout(FETCH_TIMEOUT)
            .build()
            .ok()
    });
    let client = CLIENT.as_ref().context(summon_i18n::text!("r22aeb06ce3"))?;
    let response = client.get(url).send()?;
    if !response.status().is_success() {
        bail!(summon_i18n::format!("r0f2f5545a3", response.status()));
    }
    let image = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.starts_with("image/"));
    if !image {
        bail!(summon_i18n::format!("ref33312342"));
    }
    let mut bytes = Vec::new();
    response.take(MAX_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_BYTES {
        bail!(summon_i18n::format!("rb59e6d018c"));
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn png() -> Vec<u8> {
        [PNG_MAGIC, b"rest"].concat()
    }

    #[test]
    fn every_builtin_reaction_has_a_feishu_image() {
        let builtin = summon_core::config::builtin_messaging();
        let missing: Vec<&String> = builtin
            .reactions
            .iter()
            .filter(|k| !TABLE.contains_key(*k))
            .collect();
        assert!(missing.is_empty(), "飞书表情文档里没有:{missing:?}");
        assert!(TABLE.values().all(|u| u.starts_with("https://")));

        let candidates: std::collections::HashSet<_> = builtin.reactions.iter().collect();
        assert_eq!(candidates.len(), builtin.reactions.len(), "表情键不能重复");
        // 图片表保留旧年份图，默认候选使用当前客户端的通用表情。
        let expected: std::collections::HashSet<_> =
            TABLE.keys().filter(|k| k.as_str() != "2022").collect();
        assert_eq!(candidates, expected, "官方图片与默认候选必须保持完整对应");
        for key in candidates {
            let meaning = builtin
                .reaction_meanings
                .get(key)
                .expect("候选必须有中文含义");
            assert!(
                meaning
                    .chars()
                    .any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c)),
                "{key} 缺少中文搜索词"
            );
        }
    }

    #[test]
    fn first_load_fetches_once_then_serves_from_disk() {
        let dir = tempfile::tempdir().unwrap();
        let calls = Cell::new(0);
        let fetch = |url: &str| {
            calls.set(calls.get() + 1);
            assert_eq!(url, TABLE["OK"]);
            Ok(png())
        };
        assert_eq!(load(dir.path(), "OK", fetch), Ok(png()));
        assert_eq!(
            load(dir.path(), "OK", |_| -> anyhow::Result<Vec<u8>> {
                panic!("盘上已有,不该再取")
            }),
            Ok(png())
        );
        assert_eq!(calls.get(), 1);
        assert!(dir.path().join("OK.png").exists());
    }

    #[test]
    fn unknown_keys_never_fetch_and_bad_bytes_are_not_cached() {
        let dir = tempfile::tempdir().unwrap();
        for key in ["NO_SUCH_KEY", "../OK", "OK.png", ""] {
            assert_eq!(
                load(dir.path(), key, |_| -> anyhow::Result<Vec<u8>> {
                    panic!("{key} 不在表里,不该联网")
                }),
                Err(LoadError::Unknown)
            );
        }
        let html = load(dir.path(), "DONE", |_| Ok(b"<html>".to_vec()));
        assert!(matches!(html, Err(LoadError::Unavailable(_))));
        assert!(!dir.path().join("DONE.png").exists());
        let down = load(dir.path(), "DONE", |_| anyhow::bail!("连不上"));
        assert!(matches!(down, Err(LoadError::Unavailable(_))));
    }
}
