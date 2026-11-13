//! 构建脚本(分发,`docs/superpowers/specs/2026-06-14-distribution-design.md` §3-A):**opt-in**
//! 编译 Go sidecar 到 `~/.summon/bin/summon-sidecar`,让 `cargo install` 分发路径能一并装上 sidecar
//! (裸 `cargo install` 否则只出 Rust 二进制、sidecar 缺失即不可用 —— 见 spec §1)。
//!
//! **默认 `cargo build`(本地开发)不动**:仅 `SUMMON_BUILD_SIDECAR=1` 时才构建。理由:① 不拖慢
//! 每次 dev build;② 缺 Go 工具链时不破坏构建(开发者用 `summon dev sidecar build` 即可,分发走
//! brew/release archive 同包路径 §3-B 兜底)。

use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    emit_commit();
    emit_dashboard_assets();
    println!("cargo:rerun-if-env-changed=SUMMON_BUILD_SIDECAR");
    // 仅显式 opt-in 才编 sidecar(cargo install 分发路径);默认/dev build 直接返回、零行为变化。
    if std::env::var("SUMMON_BUILD_SIDECAR").as_deref() != Ok("1") {
        return;
    }
    // 缺 Go → warn + 跳过(绝不 panic 破坏构建;分发改走 release archive 同包)。
    if Command::new("go").arg("version").output().is_err() {
        println!(summon_i18n::source!("rb1a0f901b2"));
        return;
    }
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../sidecar/feishu-connector");
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        println!(summon_i18n::source!("rf71fd62ce1"));
        return;
    };
    let out = home.join(".summon/bin/summon-sidecar");
    if let Some(p) = out.parent() {
        let _ = std::fs::create_dir_all(p);
    }
    // 产物按功能命名(arg0=summon-sidecar);构建失败只 warn,不阻断 cargo。
    let status = Command::new("go")
        .current_dir(&src)
        .arg("build")
        .arg("-o")
        .arg(&out)
        .arg(".")
        .status();
    match status {
        Ok(s) if s.success() => {
            println!(summon_i18n::source!("rad9c3b16a2"), out.display())
        }
        _ => println!(summon_i18n::source!("r4fd9a936c5")),
    }
}

/// 把 `dashboard/dist/`(控制台前端的构建产物,入库)编进二进制:生成
/// `OUT_DIR/dashboard_assets.rs`,内容是 `static DIST: &[(&str, &str, &[u8])]`
/// (相对 dist 的路径, Content-Type, `include_bytes!` 的内容)。点文件(`.gitkeep`)不收;目录为空或
/// 不在时生成空表,服务回落内置页面。`cargo build` 因此不需要 Node。
fn emit_dashboard_assets() {
    let dist = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../dashboard/dist");
    // 钉目录:cargo 会扫描目录下任何文件的改动。
    println!("cargo:rerun-if-changed={}", dist.display());
    let mut files = Vec::new();
    collect_files(&dist, &dist, &mut files);
    files.sort();
    let mut table = String::from("static DIST: &[(&str, &str, &[u8])] = &[\n");
    for (rel, abs) in files {
        table.push_str(&format!(
            "    ({rel:?}, {:?}, include_bytes!({abs:?})),\n",
            content_type(&rel)
        ));
    }
    table.push_str("];\n");
    let out =
        PathBuf::from(std::env::var_os("OUT_DIR").expect(summon_i18n::source!("r013815cf9d")))
            .join("dashboard_assets.rs");
    std::fs::write(&out, table).expect(summon_i18n::source!("ra8e254b973"));
}

/// 递归收 `dir` 下的文件:(相对 `root`、以 `/` 分隔的路径, 绝对路径)。跳过点文件与点目录;
/// 路径不是 UTF-8 的跳过并告警(拼不进字面量)。
fn collect_files(root: &Path, dir: &Path, out: &mut Vec<(String, String)>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        if path.is_dir() {
            collect_files(root, &path, out);
            continue;
        }
        let (Some(rel), Some(abs)) = (
            path.strip_prefix(root).ok().and_then(|p| p.to_str()),
            path.to_str(),
        ) else {
            println!(summon_i18n::source!("r545b9c5a6d"), path.display());
            continue;
        };
        out.push((rel.replace(std::path::MAIN_SEPARATOR, "/"), abs.to_string()));
    }
}

fn content_type(path: &str) -> &'static str {
    match path.rsplit('.').next().unwrap_or_default() {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" | "map" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "woff2" => "font/woff2",
        "txt" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

/// 把构建时的 commit 短 sha 交给 `env!("SUMMON_COMMIT")`,HEAD 的提交时间(committer time,
/// Unix 秒)交给 `env!("SUMMON_COMMIT_TIME")`。
///
/// 版本号的事实源是 Cargo.toml;sha 与提交时间只在**版本号相同**时用来分新旧(两条安装渠道
/// 可以装出同号不同提交的两份,见 `summon_core::install_stamp`)。拿不到就给 `unknown` / 空串:
/// 源码包、无 git 的构建环境都属正常情况,**不能因此让构建失败** —— 比较方把缺失当作「判不出」。
///
/// rerun 判据钉在 `HEAD` 和它指向的那个 ref 文件上:前者管「换了分支」,后者管「同一分支上
/// 又提交了一次」。少钉后者的话,在同一分支上连续提交时 sha 会停在第一次构建的值。
///
/// **两个文件不在同一个目录下**:worktree 里 HEAD 是每个 worktree 私有的,在
/// `--git-dir`(`.git/worktrees/<x>/`)下;而 `refs/heads/<branch>` 是全仓共享的,在
/// `--git-common-dir`(主仓 `.git/`)下。都拼 git-dir 的话,worktree 里 ref 那半永远
/// `is_file()` 为假,于是这道判据在 worktree 上静默失效 —— 而开发就在 worktree 里做。
///
/// ref 被 packed-refs 收走(`git gc` 会这么做)时盘上没有那个文件,就钉 `refs/` 下离它最近、
/// 还在盘上的那层目录:下一次提交把 ref 写回散文件时目录随之变化,保证重跑本脚本,sha 与提交时间
/// 跟上 HEAD(停在旧值的话,这份构建比新旧时会显得比实际更旧)。不钉那个不存在的路径本身:
/// cargo 把缺失的文件当作「变了」,会每次构建都重跑本脚本。
fn emit_commit() {
    let commit = git_short_head().unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=SUMMON_COMMIT={commit}");
    let commit_time = git_head_commit_time().unwrap_or_default();
    println!("cargo:rustc-env=SUMMON_COMMIT_TIME={commit_time}");

    let Some(git_dir) = git_path("--absolute-git-dir") else {
        return;
    };
    let head = git_dir.join("HEAD");
    if !head.is_file() {
        return;
    }
    println!("cargo:rerun-if-changed={}", head.display());
    // `ref: refs/heads/xxx` → 再钉一份那个 ref 文件;detached HEAD 时 HEAD 里直接是 sha,无需追加。
    let Ok(text) = std::fs::read_to_string(&head) else {
        return;
    };
    let Some(reference) = text.trim().strip_prefix("ref: ") else {
        return;
    };
    let Some(common_dir) = git_path("--git-common-dir") else {
        return;
    };
    let ref_path = common_dir.join(reference);
    if ref_path.is_file() {
        println!("cargo:rerun-if-changed={}", ref_path.display());
        return;
    }
    // 只在 `refs/` 里往上找:钉到 `.git` 本身的话,任何 git 操作(index、objects)都会触发重编。
    let refs = common_dir.join("refs");
    if let Some(dir) = ref_path
        .ancestors()
        .skip(1)
        .take_while(|dir| dir.starts_with(&refs))
        .find(|dir| dir.is_dir())
    {
        println!("cargo:rerun-if-changed={}", dir.display());
    }
}

fn git_short_head() -> Option<String> {
    let out = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let sha = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (!sha.is_empty()).then_some(sha)
}

/// HEAD 的 committer time。用 committer 而不是 author:rebase / merge 落到主干的那一刻才是
/// 「这份代码出现在这条历史上」的时间。浅克隆(CI)里 HEAD 自己总在,照样取得到。
fn git_head_commit_time() -> Option<String> {
    let out = Command::new("git")
        .args(["log", "-1", "--format=%ct", "HEAD"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let secs = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (!secs.is_empty() && secs.bytes().all(|b| b.is_ascii_digit())).then_some(secs)
}

/// 问 git 要某个目录路径。worktree 里 `.git` 是**文件**不是目录(内容
/// `gitdir: /path/to/.git/worktrees/xxx`),自己拼路径拼不对,只能问它。
///
/// `--git-common-dir` 在普通仓里会回一个相对路径(就是 `.git`),相对的是**进程的 cwd**,
/// 这里的 cwd 是 manifest 目录,所以按它来补全。
fn git_path(flag: &str) -> Option<PathBuf> {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let out = Command::new("git")
        .args(["rev-parse", flag])
        .current_dir(manifest_dir)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let dir = String::from_utf8(out.stdout).ok()?.trim().to_string();
    if dir.is_empty() {
        return None;
    }
    let path = PathBuf::from(dir);
    Some(if path.is_relative() {
        PathBuf::from(manifest_dir).join(path)
    } else {
        path
    })
}
