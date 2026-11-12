//! `~/.summon/config.yaml` 的 sentinel 开关。行级编辑与写前校验见 [`super::config_edit`]。

use std::path::Path;

/// 把 `sentinel.<key>` 布尔开关写进 config.yaml。`key` 只允许 `enabled` / `dry_run`
/// (封闭白名单,不接受任意键)。
pub(crate) fn set_sentinel_flag(path: &Path, key: &str, value: bool) -> anyhow::Result<()> {
    anyhow::ensure!(
        key == "enabled" || key == "dry_run",
        summon_i18n::format!("rd022934475")
    );
    super::config_edit::set_section_key(path, "sentinel", key, &value.to_string(), |c| match key {
        "enabled" => c.sentinel.enabled == value,
        _ => c.sentinel.dry_run == value,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn patch_sentinel_flag(orig: &str, key: &str, value: bool) -> String {
        crate::setup::config_edit::patch_section_key(orig, "sentinel", key, &value.to_string())
            .expect("测试用例都是块写法")
    }

    #[test]
    fn 段内有键_整行替换且注释保留() {
        let orig = "# 顶部注释\nsentinel:\n  # 开关说明\n  enabled: false\n  dry_run: true\n\nmenubar:\n  terminal: []\n";
        let out = patch_sentinel_flag(orig, "enabled", true);
        assert!(out.contains("  enabled: true"));
        assert!(out.contains("# 顶部注释"));
        assert!(out.contains("# 开关说明"));
        assert!(out.contains("dry_run: true"));
        assert!(out.contains("menubar:"));
    }

    #[test]
    fn 有段无键_插进段首() {
        let orig = "sentinel:\n  enabled: true\npoll_ms: 400\n";
        let out = patch_sentinel_flag(orig, "dry_run", false);
        let dry = out
            .lines()
            .position(|l| l.contains("dry_run: false"))
            .unwrap();
        let poll = out.lines().position(|l| l.starts_with("poll_ms")).unwrap();
        assert!(dry < poll, "新键必须落在 sentinel 段内:\n{out}");
    }

    #[test]
    fn 无段_末尾补整段() {
        let orig = "poll_ms: 400\n";
        let out = patch_sentinel_flag(orig, "enabled", true);
        assert!(out.ends_with("sentinel:\n  enabled: true\n"), "{out}");
    }

    #[test]
    fn 空文件_直接建段() {
        let out = patch_sentinel_flag("", "dry_run", true);
        assert_eq!(out, "sentinel:\n  dry_run: true\n");
    }

    #[test]
    fn 同段其他键不受伤_不误配前缀() {
        // enabled 的编辑绝不能碰 dry_run,也不能把别段同名键当成目标。
        let orig = "messaging:\n  enabled: true\nsentinel:\n  enabled: false\n";
        let out = patch_sentinel_flag(orig, "enabled", true);
        assert!(out.contains("messaging:\n  enabled: true"), "{out}");
        assert!(out.contains("sentinel:\n  enabled: true"), "{out}");
    }

    #[test]
    fn 落盘_原子写可读回() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.yaml");
        set_sentinel_flag(&path, "enabled", true).unwrap();
        set_sentinel_flag(&path, "dry_run", true).unwrap();
        let s = std::fs::read_to_string(&path).unwrap();
        assert!(s.contains("enabled: true"));
        assert!(s.contains("dry_run: true"));
        // 非法键拒绝。
        assert!(set_sentinel_flag(&path, "interval_secs", true).is_err());
    }
}
