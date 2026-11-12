//! 运行时小文件的统一原子写原语。所有可恢复状态都必须走同目录 tmp → fsync → rename，
//! 避免崩溃留下半截 YAML/JSON/session 记录；唯一 tmp 名允许不同写者并发而不互相覆盖。

use anyhow::{Context, Result};
use std::fs::{File, OpenOptions};
use std::io::Write as _;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

pub(crate) fn atomic_write(path: &Path, content: &[u8]) -> Result<()> {
    atomic_write_inner(path, content, None, || Ok(()))
}

/// 与 [`atomic_write`] 相同，但提交后的文件权限固定为 `mode`。
///
/// 含凭据、正文或 capability token 的状态不能依赖进程 umask；临时文件从创建起就用目标权限，
/// rename 前再显式校正一次，避免 umask 或平台差异留下短暂的宽权限窗口。
pub(crate) fn atomic_write_mode(path: &Path, content: &[u8], mode: u32) -> Result<()> {
    atomic_write_inner(path, content, Some(mode), || Ok(()))
}

/// 临时文件同步完成后校验提交条件；校验失败时清理临时文件并保留当前目标。
pub(crate) fn atomic_write_mode_checked(
    path: &Path,
    content: &[u8],
    mode: u32,
    before_commit: impl FnOnce() -> Result<()>,
) -> Result<()> {
    atomic_write_inner(path, content, Some(mode), before_commit)
}

fn atomic_write_inner(
    path: &Path,
    content: &[u8],
    mode: Option<u32>,
    before_commit: impl FnOnce() -> Result<()>,
) -> Result<()> {
    let parent = path.parent().context(summon_i18n::text!("r84ada01589"))?;
    std::fs::create_dir_all(parent).with_context(|| summon_i18n::format!("r72ea1f78da"))?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .context(summon_i18n::text!("r054c134e20"))?;
    let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = parent.join(format!(".{file_name}.{}.{}.tmp", std::process::id(), seq));

    let result = (|| -> Result<()> {
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        if let Some(mode) = mode {
            options.mode(mode);
        }
        let mut file = options
            .open(&tmp)
            .with_context(|| summon_i18n::format!("r1ca9a6db3c"))?;
        if let Some(mode) = mode {
            file.set_permissions(std::fs::Permissions::from_mode(mode))
                .with_context(|| summon_i18n::format!("r5450ea3534"))?;
        }
        file.write_all(content)
            .with_context(|| summon_i18n::format!("rce8ce5bf77"))?;
        file.sync_all().with_context(|| format!("sync {tmp:?}"))?;
        before_commit()?;
        std::fs::rename(&tmp, path).with_context(|| format!("rename {tmp:?} → {path:?}"))?;
        // rename 提交后目录 fsync 增强断电耐久性；部分文件系统不支持时不把已提交写误报失败。
        let _ = File::open(parent).and_then(|dir| dir.sync_all());
        Ok(())
    })();

    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atomic_write_replaces_whole_file_and_leaves_no_tmp() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        atomic_write(&path, b"old").unwrap();
        atomic_write(&path, b"new-complete").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"new-complete");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn atomic_write_mode_is_not_weakened_by_umask_defaults() {
        use std::os::unix::fs::MetadataExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secret.json");
        atomic_write_mode(&path, b"secret", 0o600).unwrap();
        assert_eq!(std::fs::metadata(path).unwrap().mode() & 0o777, 0o600);
    }
    #[test]
    fn checked_atomic_write_preserves_the_target_when_commit_validation_fails() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("agent.yaml");
        std::fs::write(&path, "original").unwrap();
        let result = atomic_write_mode_checked(&path, b"stale", 0o600, || {
            assert!(std::fs::read_dir(root.path()).unwrap().any(|entry| entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".tmp")));
            std::fs::write(&path, "concurrent change")?;
            anyhow::bail!("snapshot changed")
        });
        assert!(result.is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "concurrent change");
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
    }
}
