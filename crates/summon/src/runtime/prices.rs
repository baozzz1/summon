//! 模型价格表:daemon 后台从公开源拉取,存到 `run/prices/litellm.json`;控制台只读盘上这份,
//! 接口请求的路径上不联网。拉取失败保留旧文件,只记日志。

use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, PoisonError};
use std::time::{Duration, SystemTime};

use anyhow::Context as _;
use summon_core::pricing::{PriceTable, LITELLM_URL};

use crate::observe::log::log;

/// 成功拉取之后隔多久再拉。
const REFRESH_EVERY: Duration = Duration::from_secs(24 * 3600);
/// 拉取失败后隔多久重试:一次网络抖动不该让价格表旧上一整天。
const RETRY_AFTER: Duration = Duration::from_secs(3600);
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);

pub(crate) fn path_in(run_dir: &Path) -> PathBuf {
    run_dir.join("prices").join("litellm.json")
}

/// 读过的那份:同一个文件的 (mtime, 长度) 没变就不重新解析。
type Cached = (PathBuf, SystemTime, u64, Arc<PriceTable>);
static CACHE: LazyLock<Mutex<Option<Cached>>> = LazyLock::new(|| Mutex::new(None));

/// 盘上的价格表;没有或读不出就没有。
pub(crate) fn load(run_dir: &Path) -> Option<Arc<PriceTable>> {
    let path = path_in(run_dir);
    let meta = std::fs::metadata(&path).ok()?;
    let (mtime, len) = (meta.modified().ok()?, meta.len());
    let mut cache = CACHE.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some((p, m, l, table)) = cache.as_ref() {
        if (p, *m, *l) == (&path, mtime, len) {
            return Some(Arc::clone(table));
        }
    }
    let table: PriceTable = serde_json::from_slice(&std::fs::read(&path).ok()?).ok()?;
    let table = Arc::new(table);
    *cache = Some((path, mtime, len, Arc::clone(&table)));
    Some(table)
}

/// 拉一次:取得的原文能用、且能顶掉盘上那份(见 [`PriceTable::can_replace`])才原子替换。
pub(crate) fn refresh(
    run_dir: &Path,
    now_ms: i64,
    fetch: impl FnOnce() -> anyhow::Result<String>,
) -> anyhow::Result<()> {
    let raw = fetch()?;
    let table = PriceTable::from_litellm(&raw, LITELLM_URL, now_ms).map_err(anyhow::Error::msg)?;
    table
        .can_replace(load(run_dir).as_deref())
        .map_err(anyhow::Error::msg)?;
    let path = path_in(run_dir);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .with_context(|| summon_i18n::format!("rc18f37e5e1", dir.display()))?;
    }
    crate::runtime::fsio::atomic_write(&path, &serde_json::to_vec(&table)?)
}

fn fetch_litellm() -> anyhow::Result<String> {
    let response = reqwest::blocking::Client::builder()
        .timeout(HTTP_TIMEOUT)
        .build()?
        .get(LITELLM_URL)
        .header(
            reqwest::header::USER_AGENT,
            concat!("summon/", env!("CARGO_PKG_VERSION")),
        )
        .send()?
        .error_for_status()?;
    Ok(response.text()?)
}

/// 距上次成功拉取还要等多久;盘上没有就是现在。
fn due_in(run_dir: &Path, now_ms: i64) -> Duration {
    let Some(table) = load(run_dir) else {
        return Duration::ZERO;
    };
    let elapsed = u64::try_from(now_ms.saturating_sub(table.fetched_at_ms)).unwrap_or(0);
    REFRESH_EVERY.saturating_sub(Duration::from_millis(elapsed))
}

/// daemon 启动时调:后台线程按节拍拉价格表。线程不被 join,停机不等它。测试进程不联网。
pub(crate) fn spawn_refresher(run_dir: PathBuf) {
    if cfg!(test) {
        return;
    }
    let spawned = std::thread::Builder::new()
        .name("prices".into())
        .spawn(move || loop {
            let now = || chrono::Utc::now().timestamp_millis();
            std::thread::sleep(due_in(&run_dir, now()));
            match refresh(&run_dir, now(), fetch_litellm) {
                Ok(()) => log(summon_i18n::text!("rb6b8b3feef")),
                Err(error) => {
                    log(&summon_i18n::format!("r62bb492325"));
                    std::thread::sleep(RETRY_AFTER);
                }
            }
        });
    if let Err(error) = spawned {
        log(&summon_i18n::format!("rf7c0ba813a"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RAW: &str = r#"{"claude-test-1": {"input_cost_per_token": 4e-06, "output_cost_per_token": 2e-05},
        "claude-test-2": {"input_cost_per_token": 1e-06},
        "gpt-test-1": {"input_cost_per_token": 2e-06}}"#;

    #[test]
    fn a_good_fetch_replaces_the_table_and_a_bad_one_keeps_it() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(load(tmp.path()).is_none());
        assert_eq!(due_in(tmp.path(), 1_000), Duration::ZERO);
        refresh(tmp.path(), 1_000, || Ok(RAW.into())).unwrap();
        let table = load(tmp.path()).unwrap();
        assert_eq!(table.fetched_at_ms, 1_000);
        assert!(table.models.contains_key("claude-test-1"));
        assert_eq!(
            due_in(tmp.path(), 1_000 + 3_600_000),
            REFRESH_EVERY - Duration::from_secs(3600)
        );
        assert_eq!(due_in(tmp.path(), 1_000 + 30 * 3_600_000), Duration::ZERO);
        // 网络失败、拿到错误页:都不动盘上那份
        assert!(refresh(tmp.path(), 2_000, || anyhow::bail!("timeout")).is_err());
        assert!(refresh(tmp.path(), 2_000, || Ok("<html>502</html>".into())).is_err());
        assert!(refresh(tmp.path(), 2_000, || Ok("{}".into())).is_err());
        // 有负的单价、模型数掉到一半以下:也不动
        let negative = RAW.replace("1e-06}", "-1e-06}");
        assert!(refresh(tmp.path(), 2_000, || Ok(negative)).is_err());
        let shrunk = r#"{"claude-test-1": {"input_cost_per_token": 4e-06}}"#;
        assert!(refresh(tmp.path(), 2_000, || Ok(shrunk.into())).is_err());
        assert_eq!(load(tmp.path()).unwrap().fetched_at_ms, 1_000);
        // 少一个模型(不到一半的损失)照常替换
        let fewer = r#"{"claude-test-1": {"input_cost_per_token": 4e-06},
            "gpt-test-1": {"input_cost_per_token": 2e-06}}"#;
        refresh(tmp.path(), 3_000, || Ok(fewer.into())).unwrap();
        assert_eq!(load(tmp.path()).unwrap().models.len(), 2);
    }

    #[test]
    fn a_corrupt_file_reads_as_no_table() {
        let tmp = tempfile::tempdir().unwrap();
        let path = path_in(tmp.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "not json").unwrap();
        assert!(load(tmp.path()).is_none());
    }
}
