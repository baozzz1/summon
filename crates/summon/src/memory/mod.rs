//! P2 跨会话记忆分层 IO:读层(global + per-agent)供冷启注入 prompt、写回事实文件。
//! 纯件(索引合成/slug/渲染)在 `summon-core::memory`;这里只读写文件。
//! 设计权威:`docs/superpowers/specs/2026-06-14-p2-memory-layering-design.md`。
//!
//! §7 决策点按 spec 保守默认落地:global 注入默认开(`resolve_layers` 含 global);
//! 写回经 `summon remember`(CLI 入口,owner/agent 可跑)。

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use summon_core::memory::{index_line, render_fact_md, slugify, MemoryLayer};
use summon_core::registry::Agent;

/// 全局记忆层根:`~/.summon/memory/global/`(fleet 共享,所有 agent 冷启都注入)。
pub(crate) fn global_root() -> PathBuf {
    crate::runtime::paths::summon_home()
        .join("memory")
        .join("global")
}

/// 某 agent 的记忆层根:`<agent.dir>/memory/`(与 identity 同处,P2 不搬)。
pub(crate) fn agent_root(agent: &Agent) -> PathBuf {
    agent.dir.join("memory")
}

/// 读一个记忆层:读 `root/MEMORY.md`(不存在/读不到 → 空 index,`compose_memory_index` 会跳过)。
fn load_layer(title: &str, root: &Path) -> MemoryLayer {
    let index = std::fs::read_to_string(root.join("MEMORY.md")).unwrap_or_default();
    MemoryLayer {
        title: title.to_string(),
        root_display: tildify(root),
        index,
    }
}

/// 解析某 agent 冷启要注入的记忆层:global ⊕ 本 agent。
/// 顺序 = 全局 → 本 agent(由宽到窄,本 agent 最贴身放最后)。空层由 compose 跳过。
pub(crate) fn resolve_layers(agent: &Agent) -> Vec<MemoryLayer> {
    let layers = vec![
        load_layer(summon_i18n::text!("r63d6b47116"), &global_root()),
        load_layer(summon_i18n::text!("rf62eb4bbb2"), &agent_root(agent)),
    ];
    layers
}

/// 写回一条事实到某层根:落 `<slug>.md`(slug 冲突加 `-2`/`-3`… 防覆盖)+ 追加 `MEMORY.md` 索引行。
/// 返回最终 slug。`root` 不存在则建。纯件负责 slug/渲染,这里只编排 IO。
///
/// 两处落盘都走 `atomic_write`:索引是 read-modify-write 全量重写,裸 `fs::write` 就地截断,
/// 崩溃可能留 0 字节索引 —— `compose_memory_index` 见空索引会跳过整层,连 `root_display` 根路径
/// 都注不进 prompt,agent 冷启既拿不到索引也不知道去哪 ls 重建。
/// 顺序也是约束:**先提交事实文件、再重写索引**。索引漏掉一个已存在的事实可事后补,
/// 索引指向不存在的文件则会误导 agent 去读空。
pub(crate) fn write_fact(
    root: &Path,
    title: &str,
    description: &str,
    fact_type: &str,
    body: &str,
    hook: &str,
    provenance: &summon_core::event_log::EventStamp,
) -> Result<String> {
    std::fs::create_dir_all(root).with_context(|| summon_i18n::format!("r01d4f8fbee"))?;
    let slug = unique_slug(root, &slugify(title));
    let fact_path = root.join(format!("{slug}.md"));
    crate::runtime::fsio::atomic_write(
        &fact_path,
        render_fact_md(&slug, description, fact_type, body, provenance).as_bytes(),
    )
    .with_context(|| summon_i18n::format!("r846464bb3f"))?;
    let idx_path = root.join("MEMORY.md");
    let mut idx = std::fs::read_to_string(&idx_path).unwrap_or_default();
    if !idx.is_empty() && !idx.ends_with('\n') {
        idx.push('\n');
    }
    idx.push_str(&index_line(title, &slug, hook));
    idx.push('\n');
    crate::runtime::fsio::atomic_write(&idx_path, idx.as_bytes())
        .with_context(|| summon_i18n::format!("r5b4a728564"))?;
    Ok(slug)
}

/// `<base>.md` 已存在则尝试 `<base>-2`/`-3`…(防覆盖既有记忆)。封顶 999 兜底返 base。
fn unique_slug(root: &Path, base: &str) -> String {
    if !root.join(format!("{base}.md")).exists() {
        return base.to_string();
    }
    for n in 2..1000 {
        let cand = format!("{base}-{n}");
        if !root.join(format!("{cand}.md")).exists() {
            return cand;
        }
    }
    base.to_string()
}

/// 绝对路径里的 home 前缀缩成 `~`(注入 prompt 更短、人读);读不到 home 则原样。
fn tildify(p: &Path) -> String {
    if let Some(home) = dirs::home_dir() {
        if let Ok(rest) = p.strip_prefix(&home) {
            return format!("~/{}", rest.display());
        }
    }
    p.display().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_fact_creates_file_and_appends_index_unique() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let slug = write_fact(
            root,
            "Owner Prefs",
            "中文回复",
            "feedback",
            "正文。",
            "中文回复",
            &summon_core::event_log::EventStamp::default(),
        )
        .unwrap();
        assert_eq!(slug, "owner-prefs");
        let fact = std::fs::read_to_string(root.join("owner-prefs.md")).unwrap();
        assert!(fact.contains("name: owner-prefs") && fact.contains("正文。"));
        let idx = std::fs::read_to_string(root.join("MEMORY.md")).unwrap();
        assert!(idx.contains("- [Owner Prefs](owner-prefs.md) — 中文回复"));
        // 同名再写 → slug 唯一化,不覆盖。
        let slug2 = write_fact(
            root,
            "Owner Prefs",
            "x",
            "user",
            "y",
            "",
            &summon_core::event_log::EventStamp::default(),
        )
        .unwrap();
        assert_eq!(slug2, "owner-prefs-2");
        assert!(root.join("owner-prefs-2.md").exists());
        // 索引追加了两行。
        let idx = std::fs::read_to_string(root.join("MEMORY.md")).unwrap();
        assert_eq!(idx.lines().filter(|l| l.starts_with("- [")).count(), 2);
    }

    #[test]
    fn write_fact_commits_index_by_rename_not_in_place() {
        use std::os::unix::fs::MetadataExt;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write_fact(
            root,
            "Alpha",
            "第一条",
            "user",
            "正文 A",
            "钩子 A",
            &summon_core::event_log::EventStamp::default(),
        )
        .unwrap();
        let idx_path = root.join("MEMORY.md");
        let ino_before = std::fs::metadata(&idx_path).unwrap().ino();
        write_fact(
            root,
            "Beta",
            "第二条",
            "user",
            "正文 B",
            "钩子 B",
            &summon_core::event_log::EventStamp::default(),
        )
        .unwrap();
        let ino_after = std::fs::metadata(&idx_path).unwrap().ino();
        // 索引是 read-modify-write 全量重写:裸 fs::write 就地截断(inode 不变),断电会留 0 字节索引,
        // 而 compose_memory_index 见空索引整层跳过 —— 连 root_display 根路径都注不进去,agent 无从自救。
        // 原子写走同目录 tmp → rename 提交,必然换 inode,以此断言走的是原子写路径。
        assert_ne!(ino_before, ino_after, "索引必须经 rename 提交,不能就地重写");

        let idx = std::fs::read_to_string(&idx_path).unwrap();
        assert!(
            idx.contains("- [Alpha](alpha.md) — 钩子 A"),
            "首行仍在:{idx}"
        );
        assert!(idx.contains("- [Beta](beta.md) — 钩子 B"), "追加行在:{idx}");
        assert!(root.join("alpha.md").exists() && root.join("beta.md").exists());
        // 提交后目录里只剩两个事实文件 + 索引,不留半截 tmp。
        assert_eq!(std::fs::read_dir(root).unwrap().count(), 3);
    }

    #[test]
    fn write_fact_commits_fact_before_index() {
        // MEMORY.md 占成目录 → 索引提交必失败(rename 覆不上目录)。此时事实文件必须已经落盘:
        // 「索引漏了一个已存在的事实」可事后补,「索引指向一个不存在的文件」会误导 agent 去读空。
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir(root.join("MEMORY.md")).unwrap();
        let err = write_fact(
            root,
            "Gamma",
            "第三条",
            "user",
            "正文 C",
            "钩子 C",
            &summon_core::event_log::EventStamp::default(),
        )
        .unwrap_err();
        assert!(
            format!("{err:#}").contains("MEMORY.md"),
            "错误须点名索引:{err:#}"
        );
        assert!(root.join("gamma.md").exists(), "事实文件应先于索引提交");
    }

    #[test]
    fn load_layer_empty_when_no_index() {
        let dir = tempfile::tempdir().unwrap();
        let l = load_layer("全局", dir.path());
        assert!(l.index.is_empty(), "缺 MEMORY.md → 空 index");
        assert_eq!(l.title, "全局");
    }

    #[test]
    fn resolve_layers_titles_global_then_agent() {
        // 不依赖真 home/agent 内容:只断言层顺序与标题(global 在前、agent 在后)。
        let dir = tempfile::tempdir().unwrap();
        let agent = summon_core::registry::Agent {
            name: "sk".into(),
            display: String::new(),
            status: "active".into(),
            role: Default::default(),
            model: None,
            effort: None,
            reasoning_summary: None,
            feishu: None,
            runtime: None,
            aliases: Default::default(),
            messaging: None,
            dir: dir.path().to_path_buf(),
            global_config_path: None,
            app_owner: Default::default(),
        };
        let layers = resolve_layers(&agent);
        // 全局最前、本 agent 最后。
        assert_eq!(layers.first().unwrap().title, "全局");
        assert_eq!(layers.last().unwrap().title, "本 agent");
    }
}
