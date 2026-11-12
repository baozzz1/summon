//! open_id → name 身份缓存。
//!
//! 飞书 open_id 是 per-app 的:同一个用户在不同 bot app 里看到的 open_id 不同。
//! 本缓存在单个 daemon(=单 app)范围内有效,记录入站消息里见过的 {open_id, name} 对。
//!
//! 用途:
//! - 出站 @mention 回退查找:peers store 没有某名字时,从身份缓存找 open_id
//! - prompt 渲染:如果正文里出现 raw open_id,可解析为名字(当前未启用,预留)

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// 一条身份缓存记录。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IdentityEntry {
    /// 显示名(飞书 mention 里的 name 字段)
    pub name: String,
    /// 最近一次在入站消息里看到的 unix 毫秒
    pub last_seen_ms: i64,
}

/// open_id → IdentityEntry 的缓存。
/// 落盘 JSON,best-effort:损坏/缺失不影响运行。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct IdentityCache {
    pub entries: HashMap<String, IdentityEntry>,
}

impl IdentityCache {
    /// 批量记录身份条目。已存在的更新 name + last_seen_ms,不存在的插入。
    /// `items` = [(open_id, name)]。空 open_id 或空 name 跳过。
    pub fn record(&mut self, items: &[(String, String)], now_ms: i64) {
        for (open_id, name) in items {
            if open_id.is_empty() || name.is_empty() {
                continue;
            }
            self.entries
                .entry(open_id.clone())
                .and_modify(|e| {
                    e.name = name.clone();
                    e.last_seen_ms = now_ms;
                })
                .or_insert_with(|| IdentityEntry {
                    name: name.clone(),
                    last_seen_ms: now_ms,
                });
        }
    }

    /// 按 open_id 查 name。
    pub fn name_for(&self, open_id: &str) -> Option<&str> {
        self.entries.get(open_id).map(|e| e.name.as_str())
    }

    // 仅测试使用:生产路径未调用(2026-08-20 死代码盘点确认)。不进 release 二进制。
    #[cfg(test)]
    /// 按 name 反查 open_id(出站 @mention 回退用)。返回最近 seen 的那个。
    pub(crate) fn open_id_for(&self, name: &str) -> Option<&str> {
        let mut best: Option<(&str, i64)> = None;
        for (oid, entry) in &self.entries {
            if entry.name == name {
                match best {
                    Some((_, ts)) if entry.last_seen_ms > ts => {
                        best = Some((oid, entry.last_seen_ms));
                    }
                    None => {
                        best = Some((oid, entry.last_seen_ms));
                    }
                    _ => {}
                }
            }
        }
        best.map(|(oid, _)| oid)
    }

    /// 裁剪:只保留最近 `max_age_ms` 内见过的条目。返回被删的数量。
    /// 防止缓存无限增长;通常配 30 天(2_592_000_000 ms)。
    pub fn prune(&mut self, now_ms: i64, max_age_ms: i64) -> usize {
        let before = self.entries.len();
        self.entries
            .retain(|_, e| now_ms.saturating_sub(e.last_seen_ms) <= max_age_ms);
        before.saturating_sub(self.entries.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_inserts_new_and_updates_existing() {
        let mut c = IdentityCache::default();
        c.record(&[("ou_1".into(), "星光".into())], 1000);
        assert_eq!(c.name_for("ou_1"), Some("星光"));
        assert_eq!(c.open_id_for("星光"), Some("ou_1"));

        // 更新 name + timestamp
        c.record(&[("ou_1".into(), "星光更新".into())], 2000);
        assert_eq!(c.name_for("ou_1"), Some("星光更新"));
        assert_eq!(c.entries["ou_1"].last_seen_ms, 2000);
    }

    #[test]
    fn record_skips_empty_fields() {
        let mut c = IdentityCache::default();
        c.record(&[("".into(), "x".into()), ("ou_1".into(), "".into())], 1000);
        assert!(c.entries.is_empty());
    }

    #[test]
    fn open_id_for_picks_most_recent() {
        let mut c = IdentityCache::default();
        c.record(&[("ou_a".into(), "星光".into())], 1000);
        c.record(&[("ou_b".into(), "星光".into())], 3000);
        c.record(&[("ou_a".into(), "星光".into())], 2000); // ou_a 刷新但仍早于 ou_b
        assert_eq!(c.open_id_for("星光"), Some("ou_b"));
    }

    #[test]
    fn prune_removes_stale_entries() {
        let mut c = IdentityCache::default();
        c.record(&[("ou_1".into(), "a".into())], 1000);
        c.record(&[("ou_2".into(), "b".into())], 5000);
        let removed = c.prune(6000, 3000); // 保留 3000ms 内的 → 只留 ou_2
        assert_eq!(removed, 1);
        assert_eq!(c.entries.len(), 1);
        assert!(c.name_for("ou_2").is_some());
    }
}
