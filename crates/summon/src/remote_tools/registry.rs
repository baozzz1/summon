use anyhow::{Context, Result};
use std::fs::{File, OpenOptions};
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::Duration;
use summon_core::remote_tools::{validate_name, LocalReply, LocalRequest, TargetStatus};

pub(super) fn root() -> Result<PathBuf> {
    Ok(dirs::home_dir()
        .context(summon_i18n::text!("rfc134c5eb3"))?
        .join(".summon/run/remote-tools"))
}

pub(super) fn socket_path(root: &Path, name: &str) -> Result<PathBuf> {
    validate_name(name).map_err(anyhow::Error::msg)?;
    let hash = ring::digest::digest(&ring::digest::SHA256, name.as_bytes());
    let short: String = hash.as_ref()[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let path = root.join(format!("{short}.sock"));
    anyhow::ensure!(
        path.as_os_str().len() < 104,
        summon_i18n::format!("r0ee792091d", path.display())
    );
    Ok(path)
}

pub(super) fn secure_dir(root: &Path) -> Result<()> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(root)?;
    let meta = std::fs::symlink_metadata(root)?;
    anyhow::ensure!(
        meta.is_dir() && !meta.file_type().is_symlink(),
        summon_i18n::format!("rcd413531b0")
    );
    anyhow::ensure!(
        meta.uid() == nix::unistd::getuid().as_raw() && meta.mode() & 0o077 == 0,
        summon_i18n::format!("rd814b8164a", root.display())
    );
    Ok(())
}

pub(super) struct Registration {
    _lock: File,
    pub listener: UnixListener,
    socket: PathBuf,
    metadata: PathBuf,
}

impl Registration {
    pub fn bind(root: &Path, name: &str) -> Result<Self> {
        secure_dir(root)?;
        let socket = socket_path(root, name)?;
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .mode(0o600)
            .custom_flags(nix::libc::O_NOFOLLOW)
            .open(root.join(format!("{name}.lock")))?;
        lock.try_lock().context(summon_i18n::text!("rd059a3ab9b"))?;
        if let Ok(meta) = std::fs::symlink_metadata(&socket) {
            anyhow::ensure!(
                meta.file_type().is_socket() && meta.uid() == nix::unistd::getuid().as_raw(),
                summon_i18n::format!("r3e9e5081ac")
            );
            std::fs::remove_file(&socket)?;
        }
        let listener = UnixListener::bind(&socket)?;
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))?;
        listener.set_nonblocking(true)?;
        Ok(Self {
            _lock: lock,
            listener,
            socket,
            metadata: root.join(format!("{name}.json")),
        })
    }

    pub fn publish(&self, status: &TargetStatus) -> Result<()> {
        crate::runtime::fsio::atomic_write(&self.metadata, &serde_json::to_vec(status)?)
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        // 持有目标锁直到清理完成，另一代连接不会被此代的退出删除。
        let _ = std::fs::remove_file(&self.socket);
        let _ = std::fs::remove_file(&self.metadata);
        // 并行 fork 的子进程在 exec 前可能短暂持有相同文件描述；主动解锁固定租约终点。
        let _ = self._lock.unlock();
    }
}

pub(super) fn connect(root: &Path, name: &str) -> Result<UnixStream> {
    let stream = UnixStream::connect(socket_path(root, name)?)
        .with_context(|| summon_i18n::format!("rfe924b8f18"))?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    Ok(stream)
}

pub(super) fn status(root: &Path, name: &str) -> Result<TargetStatus> {
    let mut stream = connect(root, name)?;
    super::wire::write_json(&mut stream, &LocalRequest::Status)?;
    let reply = super::wire::read_json::<LocalReply>(&mut std::io::BufReader::new(stream))?;
    match reply {
        Some(LocalReply::Status { status }) => Ok(status),
        _ => anyhow::bail!(summon_i18n::format!("r07c314dde0")),
    }
}

pub(super) fn resolve_directory(
    root: &Path,
    name: &str,
    working_dir: Option<String>,
) -> Result<String> {
    let mut stream = connect(root, name)?;
    super::wire::write_json(&mut stream, &LocalRequest::Attach { working_dir })?;
    let reply = super::wire::read_json::<LocalReply>(&mut std::io::BufReader::new(stream))?;
    match reply {
        Some(LocalReply::Ready { working_dir }) => Ok(working_dir),
        Some(LocalReply::Error { message }) => anyhow::bail!(message),
        _ => anyhow::bail!(summon_i18n::format!("r047b014a44")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reject_duplicate_and_release_socket_with_lease() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("registry");
        let first = Registration::bind(&root, "laptop").unwrap();
        assert!(Registration::bind(&root, "laptop").is_err());
        assert!(first.socket.exists());
        drop(first);
        assert!(!socket_path(&root, "laptop").unwrap().exists());
        Registration::bind(&root, "laptop").unwrap();
    }

    #[test]
    fn preserve_non_socket_collision() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("registry");
        secure_dir(&root).unwrap();
        let path = socket_path(&root, "laptop").unwrap();
        std::fs::write(&path, b"keep").unwrap();
        assert!(Registration::bind(&root, "laptop").is_err());
        assert_eq!(std::fs::read(path).unwrap(), b"keep");
    }
}
