//! `summon dev record {on|off|ls|promote}` 的实现(**整模块 `#[cfg(debug_assertions)]` gate** ——
//! release 不编译,同 `summon dev`)。
//!
//! - **on/off**:写/删 `~/.summon/run/record` 运行期标志(内容=场景名)。daemon 的 `record_scenario()`
//!   读它(env `SUMMON_RECORD` 优先),故 daemon 已在跑也能开关、无需重启。
//! - **ls**:列 `~/.summon/raw/` 下的场景目录。
//! - **promote**:把某场景**脱敏**后拷进 `crates/summon/tests/fixtures/scenarios/<name>/`(入仓、CI 可跑)。
//!   脱敏铁律:`~/.summon/raw/` 全程 gitignore(忠实全量);只有 promote 产物入仓,且**绝不含真
//!   token/secret/真 open_id** —— `scrub_text`(open_id→稳定别名、可选正文 redact)+ env 录制时已白名单
//!   过滤(spawn.json 只剩 `SUMMON_*`)。同时由 `timeline.ndjson` 经 `correlate` 生成 `scenario.json`。

#![cfg(debug_assertions)]

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context};
use summon_core::record::{correlate, AliasMap, AtomRef};

use crate::cli::dev::RecordAction;
use crate::runtime::daemon::record_flag_file;
use crate::runtime::paths::summon_home;

pub fn run(action: RecordAction) -> anyhow::Result<()> {
    match action {
        RecordAction::On { scenario } => on(scenario.as_deref().unwrap_or("default")),
        RecordAction::Off => off(),
        RecordAction::Ls => ls(),
        RecordAction::Promote {
            scenario,
            redact_content,
            name,
        } => promote(&scenario, redact_content, name.as_deref()),
    }
}

/// raw 录制根 `~/.summon/raw/`。
fn raw_root() -> PathBuf {
    summon_home().join("raw")
}

fn on(scenario: &str) -> anyhow::Result<()> {
    let flag = record_flag_file();
    if let Some(parent) = flag.parent() {
        fs::create_dir_all(parent)
            .with_context(|| summon_i18n::format!("r63d906b2b0", parent.display()))?;
    }
    fs::write(&flag, scenario)
        .with_context(|| summon_i18n::format!("r4ce465a848", flag.display()))?;
    println!(
        "{}",
        summon_i18n::format!("rf37f57f9a9", raw_root().display())
    );
    println!("{}", summon_i18n::format!("r2ac7d08255"));
    Ok(())
}

fn off() -> anyhow::Result<()> {
    let flag = record_flag_file();
    match fs::remove_file(&flag) {
        Ok(()) => {
            println!("{}", summon_i18n::format!("re24895cd09"))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            println!("{}", summon_i18n::format!("rf1fb2ad62f"))
        }
        Err(e) => {
            return Err(e).with_context(|| summon_i18n::format!("r34632009e5", flag.display()))
        }
    }
    Ok(())
}

fn ls() -> anyhow::Result<()> {
    let root = raw_root();
    let rd = match fs::read_dir(&root) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            println!("{}", summon_i18n::format!("rbf4b552e8f", root.display()));
            return Ok(());
        }
        Err(e) => {
            return Err(e).with_context(|| summon_i18n::format!("rb1a59430f1", root.display()))
        }
    };
    let mut names: Vec<String> = rd
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    if names.is_empty() {
        println!("{}", summon_i18n::format!("rd9aa8c20fa"));
    } else {
        println!("{}", summon_i18n::format!("r98d4fd87a8", root.display()));
        for n in names {
            println!("  {n}");
        }
    }
    Ok(())
}

/// fixture 入仓根:从 current_exe 向上找仓库 `crates/summon/tests/fixtures/scenarios/`。
fn fixtures_root() -> anyhow::Result<PathBuf> {
    let exe = std::env::current_exe().context(summon_i18n::text!("rbcc975dd73"))?;
    let base = exe
        .ancestors()
        .map(|a| a.join("crates/summon/tests/fixtures/scenarios"))
        .find(|p| {
            p.ancestors()
                .nth(3)
                .map(|root| root.join("Cargo.toml").is_file())
                .unwrap_or(false)
        })
        .context(summon_i18n::text!("r33e0f4be1e"))?;
    Ok(base)
}

fn promote(scenario: &str, redact_content: bool, name: Option<&str>) -> anyhow::Result<()> {
    let src = raw_root().join(scenario);
    if !src.is_dir() {
        bail!(summon_i18n::format!("r0f041d9dab", raw_root().display()));
    }
    let fixture_name = name.unwrap_or(scenario);
    let dst = fixtures_root()?.join(fixture_name);
    fs::create_dir_all(&dst).with_context(|| summon_i18n::format!("r63d906b2b0", dst.display()))?;

    // 一个 AliasMap 贯穿整个场景:同一 open_id 跨文件得同一稳定别名(保 actor 关系)。
    let mut aliases = AliasMap::new();

    // 逐文件拷贝 + 脱敏:文本类(*.json/*.ndjson/*.jsonl/*.raw)跑 scrub,其余(理论上无)原样拷。
    promote_tree(&src, &dst, &mut aliases, redact_content)?;

    // scenario.json:由 timeline.ndjson 经 correlate 生成(脱敏不影响 chat_id/seq/下标,故用原 timeline)。
    write_scenario_index(scenario, fixture_name, &src, &dst)?;

    println!("{}", summon_i18n::format!("r22911f9131", dst.display()));
    if redact_content {
        println!("{}", summon_i18n::format!("rf57810df29"));
    }
    println!("{}", summon_i18n::format!("rc6bbec5c54"));
    Ok(())
}

/// 递归把 src 子树脱敏拷到 dst。文本文件经 `scrub_text`;`timeline.ndjson` 不入仓(只为生成
/// scenario.json,本身含原始 chat_id/无敏感,但 fixture 不需要它,跳过保持干净)。
fn promote_tree(
    src: &Path,
    dst: &Path,
    aliases: &mut AliasMap,
    redact_content: bool,
) -> anyhow::Result<()> {
    for ent in
        fs::read_dir(src).with_context(|| summon_i18n::format!("rb1a59430f1", src.display()))?
    {
        let ent = ent?;
        let path = ent.path();
        let name = ent.file_name();
        if name == "timeline.ndjson" {
            continue; // 只用于生成 scenario.json,不入仓
        }
        let target = dst.join(&name);
        if path.is_dir() {
            fs::create_dir_all(&target)?;
            promote_tree(&path, &target, aliases, redact_content)?;
        } else if path.is_file() {
            // 全量文本脱敏(spawn.json env 已白名单过滤;这里再兜底 scrub open_id/正文)。
            let bytes = fs::read(&path)
                .with_context(|| summon_i18n::format!("rb1a59430f1", path.display()))?;
            match String::from_utf8(bytes) {
                Ok(text) => {
                    let scrubbed = summon_core::record::scrub_text(&text, aliases, redact_content);
                    fs::write(&target, scrubbed)
                        .with_context(|| summon_i18n::format!("r03fb63021e", target.display()))?;
                }
                // 非 UTF-8(理论上不该有):原样拷(stdout.raw 是 UTF-8 JSONL,正常走上面分支)。
                Err(e) => fs::write(&target, e.into_bytes())
                    .with_context(|| summon_i18n::format!("r03fb63021e", target.display()))?,
            }
        }
    }
    Ok(())
}

/// 读 `<src>/timeline.ndjson` → `correlate` → 写 `<dst>/scenario.json`(无 timeline 则空 flows)。
/// cli 从 spawn.json 推断(turn-0 的 program);拿不到则 "unknown"(回放据 flows 走,不强依赖)。
fn write_scenario_index(
    scenario: &str,
    fixture_name: &str,
    src: &Path,
    dst: &Path,
) -> anyhow::Result<()> {
    let store = crate::runtime::record_store::RecordStore::new(raw_root());
    let atoms: Vec<AtomRef> = store
        .read_ndjson(scenario, "timeline.ndjson")
        .unwrap_or_default();
    let cli = infer_cli(src).unwrap_or_else(|| "unknown".to_string());
    let index = correlate(fixture_name, &cli, atoms);
    let json = serde_json::to_string_pretty(&index).context(summon_i18n::text!("r1919ab1e10"))?;
    let path = dst.join("scenario.json");
    fs::write(&path, json).with_context(|| summon_i18n::format!("r03fb63021e", path.display()))?;
    Ok(())
}

/// 从 turn-0 的 spawn.json 推断 cli(program basename 含 cursor/codex/claude)。
fn infer_cli(src: &Path) -> Option<String> {
    let spawn = src.join("turns").join("turn-0").join("spawn.json");
    let json = fs::read_to_string(spawn).ok()?;
    let turn: summon_core::record::CassetteTurn = serde_json::from_str(&json).ok()?;
    let prog = turn.spec.program.to_lowercase();
    for cli in ["cursor", "codex", "claude"] {
        if prog.contains(cli) {
            return Some(cli.to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// promote 脱敏:含真 open_id 的 inbound/outbound 经 scrub 后,fixture 里**无真 open_id**(换成
    /// 稳定别名),且同一 open_id 跨文件得同一别名(保 actor 关系)。env 白名单已在录制时过滤,这里
    /// 验「正文里的 open_id」也被脱敏(promote 兜底)。
    #[test]
    fn promote_scrubs_open_ids_to_stable_aliases() {
        let raw = tempfile::tempdir().unwrap();
        let scenario = "scn";
        let scn = raw.path().join(scenario);
        fs::create_dir_all(&scn).unwrap();
        let oid = "ou_1a2b3c4d5e6f7a8b";
        fs::write(
            scn.join("inbound.ndjson"),
            format!("{{\"sender\":\"{oid}\",\"text\":\"hi\"}}\n"),
        )
        .unwrap();
        fs::write(
            scn.join("outbound.ndjson"),
            format!("{{\"chat_id\":\"oc_x\",\"to\":\"{oid}\"}}\n"),
        )
        .unwrap();

        // promote_tree 直接验(绕开 fixtures_root 的仓库路径依赖)。
        let dst = tempfile::tempdir().unwrap();
        let mut aliases = AliasMap::new();
        promote_tree(&scn, dst.path(), &mut aliases, false).unwrap();

        let inb = fs::read_to_string(dst.path().join("inbound.ndjson")).unwrap();
        let outb = fs::read_to_string(dst.path().join("outbound.ndjson")).unwrap();
        // 真 open_id 不得出现在入仓产物。
        assert!(!inb.contains(oid), "fixture 不得含真 open_id: {inb}");
        assert!(!outb.contains(oid), "fixture 不得含真 open_id: {outb}");
        // 换成了稳定别名,且两文件同一别名(同 id 同名)。
        assert!(inb.contains("ou_alice"), "应换成稳定别名: {inb}");
        assert!(outb.contains("ou_alice"), "跨文件同 id 同别名: {outb}");
    }

    /// promote 不把 timeline.ndjson 带进 fixture(只用于生成 scenario.json),且结构其余文件都拷。
    #[test]
    fn promote_excludes_timeline_keeps_rest() {
        let raw = tempfile::tempdir().unwrap();
        let scn = raw.path().join("scn");
        fs::create_dir_all(&scn).unwrap();
        fs::write(scn.join("timeline.ndjson"), "{}\n").unwrap();
        fs::write(scn.join("inbound.ndjson"), "{}\n").unwrap();

        let dst = tempfile::tempdir().unwrap();
        let mut aliases = AliasMap::new();
        promote_tree(&scn, dst.path(), &mut aliases, false).unwrap();
        assert!(
            !dst.path().join("timeline.ndjson").exists(),
            "timeline 不入仓"
        );
        assert!(dst.path().join("inbound.ndjson").exists(), "其余文件应拷入");
    }
}
