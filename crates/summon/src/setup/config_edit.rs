//! `~/.summon/config.yaml` 的**行级编辑**:改一个段下的一个键,其余逐字节不动。
//!
//! 为什么不 serde 往返:config.yaml 是 owner 手维护的(模板几乎整份是注释),反序列化再序列化会丢光
//! 注释与排版。所以做文本级局部改写:段内命中的键只换值、行尾注释留在原来那一列,缺键在段首行后插,
//! 连段都没有就在末尾补一段;换行符照原文件(含 CRLF 就整份写 CRLF),末尾有无换行也照原文件。
//!
//! 文本改写认不全 YAML 的写法,所以落盘前要过三道:改写结果能被 [`Config`] 解析;解析出来正是想写的
//! 值(调用方给的读回判据);除了这一个键,其余内容解析出来与原文件一模一样。任一道不过就不写 ——
//! 宁可报错让人去手改,也不把配置改坏:config.yaml 读不出来,daemon 会带着内置默认跑。
//!
//! 读-改-写整段持一把文件锁(`config.yaml` 旁的 `.config.yaml.lock`):Menu Bar 几行设置可以同时在写,
//! `summon sentinel set` 也走这里,不锁就会互相覆盖。

use anyhow::{bail, Context};
use std::path::Path;
use summon_core::config::Config;

/// 把 `section.key` 设成 `value`(YAML 标量的字面写法),写前校验读回值。
pub(crate) fn set_section_key(
    path: &Path,
    section: &str,
    key: &str,
    value: &str,
    read_back: impl Fn(&Config) -> bool,
) -> anyhow::Result<()> {
    let dir = path.parent().context(summon_i18n::text!("ra734729215"))?;
    std::fs::create_dir_all(dir)?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(dir.join(".config.yaml.lock"))
        .context(summon_i18n::text!("rffee7c7ce5"))?;
    lock.lock().context(summon_i18n::text!("r09eae470ab"))?;

    let orig = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e).context(summon_i18n::text!("rbffb7b8281")),
    };
    let out = patch_section_key(&orig, section, key, value)?;
    if out == orig {
        return Ok(());
    }
    let parsed: Config =
        serde_yaml::from_str(&out).with_context(|| summon_i18n::format!("r40488a2f48"))?;
    if !read_back(&parsed) {
        bail!(summon_i18n::format!("r9a9ff305c6"));
    }
    if !only_target_changed(&orig, &out, section, key) {
        bail!(summon_i18n::format!("r3790e38424"));
    }
    atomic_write_preserving_mode(path, &out)
}

/// 两份文本解析出来,去掉 `section.key`(去掉后段空了连段一起去掉)之后是否完全相同。
/// 原文件本身解析不了时无从比较,按「不同」处理。
fn only_target_changed(orig: &str, out: &str, section: &str, key: &str) -> bool {
    let strip = |text: &str| -> Option<serde_yaml::Value> {
        let mut value: serde_yaml::Value = serde_yaml::from_str(text).ok()?;
        if value.is_null() {
            value = serde_yaml::Value::Mapping(Default::default());
        }
        let root = value.as_mapping_mut()?;
        let section_key = serde_yaml::Value::from(section);
        // 段首行下面一个子键都没有(全注释掉了)时,段解析出来是 null,与摘空的段同样看待。
        let emptied = match root.get_mut(&section_key) {
            Some(serde_yaml::Value::Mapping(entries)) => {
                entries.remove(serde_yaml::Value::from(key));
                entries.is_empty()
            }
            Some(serde_yaml::Value::Null) => true,
            _ => false,
        };
        if emptied {
            root.remove(&section_key);
        }
        Some(value)
    };
    matches!((strip(orig), strip(out)), (Some(a), Some(b)) if a == b)
}

/// 纯文本改写(可单测)。只认段名顶格、独占一行(可带行尾注释)的块写法;段写成行内值
/// (`messaging: {...}`)时拒绝,不去猜。段内只匹配**直接子键**那一层缩进,深层同名键不碰。
pub(crate) fn patch_section_key(
    orig: &str,
    section: &str,
    key: &str,
    value: &str,
) -> anyhow::Result<String> {
    let newline = if orig.contains("\r\n") { "\r\n" } else { "\n" };
    let mut lines: Vec<String> = orig.lines().map(str::to_string).collect();
    let header = format!("{section}:");
    let start = lines.iter().position(|l| l.starts_with(&header));
    match start {
        Some(start) => {
            let rest = lines[start][header.len()..].trim();
            if !rest.is_empty() && !rest.starts_with('#') {
                bail!(summon_i18n::format!("r861b3e2ee1"));
            }
            let is_top_level = |l: &String| {
                !l.is_empty() && !l.starts_with(' ') && !l.starts_with('\t') && !l.starts_with('#')
            };
            let end = (start + 1..lines.len())
                .find(|&i| is_top_level(&lines[i]))
                .unwrap_or(lines.len());
            let is_entry = |l: &String| {
                let t = l.trim_start();
                !t.is_empty() && !t.starts_with('#')
            };
            let indent: String = (start + 1..end)
                .find(|&i| is_entry(&lines[i]))
                .map(|i| lines[i].chars().take_while(|c| c.is_whitespace()).collect())
                .unwrap_or_else(|| "  ".to_string());
            let prefix = format!("{key}:");
            let target = (start + 1..end).find(|&i| {
                let l = &lines[i];
                l.starts_with(&indent)
                    && !l[indent.len()..].starts_with([' ', '\t'])
                    && l[indent.len()..].starts_with(&prefix)
            });
            match target {
                Some(pos) => {
                    let after_key = &lines[pos][indent.len() + prefix.len()..];
                    lines[pos] = format!(
                        "{indent}{prefix} {value}{}",
                        trailing_comment(after_key, value)
                    );
                }
                None => lines.insert(start + 1, format!("{indent}{key}: {value}")),
            }
        }
        None => {
            lines.push(header);
            lines.push(format!("  {key}: {value}"));
        }
    }
    let joined = lines.join(newline);
    Ok(if orig.ends_with('\n') || orig.is_empty() {
        format!("{joined}{newline}")
    } else {
        joined
    })
}

/// 键后面那段里的行尾注释,连同它前面的空白(没有注释就是空串)。空白按新旧值的宽度差补齐,
/// 注释留在原来那一列;放不下时至少隔一个空格。YAML 的注释从「前面是空白的 `#`」开始;这里只改
/// 标量开关与秒数,值里不会有带引号的 `#`。
fn trailing_comment(after_key: &str, new_value: &str) -> String {
    let bytes = after_key.as_bytes();
    let Some(hash) =
        (1..bytes.len()).find(|&i| bytes[i] == b'#' && bytes[i - 1].is_ascii_whitespace())
    else {
        return String::new();
    };
    // 键后到注释之间原来占的宽度(`key:` 后的空白 + 旧值 + 旧值后的空白)。新行写成 `key: <新值>`,
    // 剩下的用空格补到同一列。
    let width = after_key[..hash].chars().count();
    let used = 1 + new_value.chars().count();
    let pad = width.saturating_sub(used).max(1);
    format!("{}{}", " ".repeat(pad), &after_key[hash..])
}

/// 原子写并保留原权限位(文件不存在按 0o600 建——config 可能日后含敏感覆盖)。暂存文件名每次唯一,
/// 两个写者不会踩同一个暂存文件。
pub(crate) fn atomic_write_preserving_mode(path: &Path, content: &str) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(path)
        .map(|m| m.permissions().mode())
        .unwrap_or(0o600);
    let dir = path.parent().context(summon_i18n::text!("ra734729215"))?;
    std::fs::create_dir_all(dir)?;
    let staged = tempfile::Builder::new()
        .prefix(".config.yaml.")
        .tempfile_in(dir)
        .context(summon_i18n::text!("rbe6eb64a6d"))?;
    std::fs::write(staged.path(), content)?;
    std::fs::set_permissions(staged.path(), std::fs::Permissions::from_mode(mode & 0o777))?;
    staged.persist(path).map_err(|e| {
        anyhow::anyhow!(summon_i18n::format!("r932ce6b778", path.display(), e.error))
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replaces_the_key_in_place_and_keeps_comments() {
        let orig = "# 顶部\nmessaging:\n  # 说明\n  cot_visible: true\n  badge_effort: true\npoll_ms: 400\n";
        let out = patch_section_key(orig, "messaging", "cot_visible", "false").unwrap();
        assert_eq!(
            out,
            "# 顶部\nmessaging:\n  # 说明\n  cot_visible: false\n  badge_effort: true\npoll_ms: 400\n"
        );
    }

    #[test]
    fn inserts_a_missing_key_with_the_sections_indent() {
        let orig = "messaging:\n    idle_ttl_secs: 3600\nmenubar:\n  terminal: []\n";
        let out = patch_section_key(orig, "messaging", "cot_tool_details", "true").unwrap();
        assert_eq!(
            out,
            "messaging:\n    cot_tool_details: true\n    idle_ttl_secs: 3600\nmenubar:\n  terminal: []\n"
        );
    }

    #[test]
    fn appends_a_missing_section() {
        let out = patch_section_key("poll_ms: 400\n", "menubar", "show_counts", "false").unwrap();
        assert_eq!(out, "poll_ms: 400\nmenubar:\n  show_counts: false\n");
        assert_eq!(
            patch_section_key("", "menubar", "show_counts", "true").unwrap(),
            "menubar:\n  show_counts: true\n"
        );
    }

    /// 深层同名键与别段同名键都不是目标。
    #[test]
    fn only_direct_children_of_the_section_match() {
        let orig = "messaging:\n  session_exited:\n    idle_ttl_secs: x\n  idle_ttl_secs: 60\nother:\n  idle_ttl_secs: 9\n";
        let out = patch_section_key(orig, "messaging", "idle_ttl_secs", "86400").unwrap();
        assert_eq!(
            out,
            "messaging:\n  session_exited:\n    idle_ttl_secs: x\n  idle_ttl_secs: 86400\nother:\n  idle_ttl_secs: 9\n"
        );
    }

    #[test]
    fn a_key_with_the_same_prefix_is_not_the_target() {
        let orig = "messaging:\n  idle_retire_secs_x: 1\n";
        let out = patch_section_key(orig, "messaging", "idle_retire_secs", "0").unwrap();
        assert_eq!(
            out,
            "messaging:\n  idle_retire_secs: 0\n  idle_retire_secs_x: 1\n"
        );
    }

    #[test]
    fn an_inline_section_is_refused() {
        assert!(patch_section_key("messaging: {}\n", "messaging", "cot_visible", "true").is_err());
        // 段首行带注释仍是块写法。
        assert!(
            patch_section_key("messaging: # 消息层\n", "messaging", "cot_visible", "true").is_ok()
        );
    }

    #[test]
    fn keeps_a_file_without_trailing_newline_that_way() {
        let out = patch_section_key(
            "menubar:\n  show_counts: true",
            "menubar",
            "show_counts",
            "false",
        )
        .unwrap();
        assert_eq!(out, "menubar:\n  show_counts: false");
    }

    /// 照 config.example.yaml 取消注释的写法:键、值、对齐空白、行尾说明。只换值。
    #[test]
    fn keeps_the_trailing_comment_of_the_rewritten_line() {
        let orig = "messaging:\n  hide_thinking: false     # 隐藏思考正文(存的是「隐藏」)\n";
        let out = patch_section_key(orig, "messaging", "hide_thinking", "true").unwrap();
        assert_eq!(
            out,
            "messaging:\n  hide_thinking: true      # 隐藏思考正文(存的是「隐藏」)\n"
        );
        // 值变宽变窄,注释都留在原来那一列。
        let aligned = "messaging:\n  idle_retire_secs: 0        # 关\n";
        assert_eq!(
            patch_section_key(aligned, "messaging", "idle_retire_secs", "86400").unwrap(),
            "messaging:\n  idle_retire_secs: 86400    # 关\n"
        );
        let tight = "menubar:\n  show_counts: true # 计数\n";
        assert_eq!(
            patch_section_key(tight, "menubar", "show_counts", "false").unwrap(),
            "menubar:\n  show_counts: false # 计数\n"
        );
    }

    /// 段首行下面的子键全注释掉了(段解析出来是 null):照常能写进去。
    #[test]
    fn writes_into_a_section_whose_entries_are_all_commented_out() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.yaml");
        std::fs::write(
            &path,
            "messaging: # 消息层\n  # cot_visible: true\npoll_ms: 400\n",
        )
        .unwrap();
        set_section_key(&path, "messaging", "cot_visible", "false", |c| {
            c.messaging.cot_visible == Some(false)
        })
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "messaging: # 消息层\n  cot_visible: false\n  # cot_visible: true\npoll_ms: 400\n"
        );
    }

    #[test]
    fn keeps_crlf_line_endings() {
        let orig = "messaging:\r\n  cot_visible: true\r\n";
        assert_eq!(
            patch_section_key(orig, "messaging", "cot_visible", "false").unwrap(),
            "messaging:\r\n  cot_visible: false\r\n"
        );
    }

    /// 只有目标键可以变;改写若连带改到别的值(段的写法没认对),就不写。
    #[test]
    fn only_the_target_key_may_change() {
        let orig = "messaging:\n  cot_visible: true\n  badge_effort: true\n";
        let good = "messaging:\n  cot_visible: false\n  badge_effort: true\n";
        let bad = "messaging:\n  cot_visible: false\n  badge_effort: false\n";
        assert!(only_target_changed(orig, good, "messaging", "cot_visible"));
        assert!(!only_target_changed(orig, bad, "messaging", "cot_visible"));
        // 新补的段只有目标键:去掉它之后与原文件相同。
        assert!(only_target_changed(
            "poll_ms: 1\n",
            "poll_ms: 1\nmenubar:\n  show_counts: false\n",
            "menubar",
            "show_counts"
        ));
        assert!(only_target_changed(
            "",
            "menubar:\n  show_counts: false\n",
            "menubar",
            "show_counts"
        ));
    }

    /// 两个写者同时改不同的键,两次修改都要留下来。
    #[test]
    fn concurrent_writers_do_not_lose_each_others_update() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.yaml");
        std::fs::write(&path, "messaging:\n  cot_visible: true\n").unwrap();
        let keys = [
            "cot_visible",
            "cot_tool_details",
            "badge_effort",
            "hide_thinking",
        ];
        std::thread::scope(|scope| {
            for key in keys {
                let path = path.clone();
                scope.spawn(move || {
                    for value in ["false", "true"] {
                        set_section_key(&path, "messaging", key, value, |_| true).unwrap();
                    }
                });
            }
        });
        let text = std::fs::read_to_string(&path).unwrap();
        for key in keys {
            assert!(
                text.contains(&format!("  {key}: true")),
                "{key} 丢了:\n{text}"
            );
        }
    }

    #[test]
    fn writes_only_what_reads_back() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.yaml");
        std::fs::write(&path, "# 手写注释\nmessaging:\n  cot_visible: true\n").unwrap();
        set_section_key(&path, "messaging", "cot_visible", "false", |c| {
            !c.messaging.cot_visible.unwrap_or(true)
        })
        .unwrap();
        let written = std::fs::read_to_string(&path).unwrap();
        assert_eq!(written, "# 手写注释\nmessaging:\n  cot_visible: false\n");

        // 读回判据不过就不写,原文件逐字节不变。
        let error =
            set_section_key(&path, "messaging", "cot_visible", "true", |_| false).unwrap_err();
        assert!(format!("{error:#}").contains("未写入"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), written);
    }

    #[test]
    fn writes_nothing_when_the_result_does_not_parse() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.yaml");
        std::fs::write(&path, "messaging:\n  cot_visible: true\n").unwrap();
        assert!(set_section_key(&path, "messaging", "cot_visible", "[unclosed", |_| true).is_err());
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "messaging:\n  cot_visible: true\n"
        );
    }
}
