//! 进程 fd 表快照(诊断用):哪些编号开着、是什么类型、指向哪里。
//!
//! 给「某个 fd 被进程里别处关掉」这类问题留现场:只有编号、没有内容,拿到快照的人能对出
//! 那个编号此刻是空着、还是已被别的文件/socket 复用 —— 后者直接指向关错 fd 的那一方。

use nix::libc;
use std::os::fd::RawFd;

/// 列出 `0..limit` 里开着的 fd,格式 `<fd>=<类型>[:<路径>]`,空格分隔。
pub fn fd_snapshot(limit: RawFd) -> String {
    (0..limit)
        .filter_map(|fd| describe_fd(fd).map(|what| format!("{fd}={what}")))
        .collect::<Vec<_>>()
        .join(" ")
}

/// 单个 fd 的类型与路径;没开着(或编号非法)→ `None`。
pub fn describe_fd(fd: RawFd) -> Option<String> {
    // SAFETY:fstat 只往 `st` 里写;fd 没开着时返回 -1(EBADF),不会读写任何别的内存。
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(fd, &mut st) } != 0 {
        return None;
    }
    let kind = match st.st_mode & libc::S_IFMT {
        libc::S_IFSOCK => "sock",
        libc::S_IFIFO => "fifo",
        libc::S_IFREG => "file",
        libc::S_IFCHR => "chr",
        libc::S_IFDIR => "dir",
        _ => "other",
    };
    Some(match fd_path(fd) {
        Some(path) => format!("{kind}:{path}"),
        None => kind.to_string(),
    })
}

#[cfg(target_os = "macos")]
fn fd_path(fd: RawFd) -> Option<String> {
    let mut buf = vec![0u8; libc::PATH_MAX as usize];
    // SAFETY:F_GETPATH 往 buf 写至多 PATH_MAX 字节、以 NUL 结尾的路径;socket/管道这类没有路径的
    // fd 返回 -1,buf 不被使用。
    if unsafe { libc::fcntl(fd, libc::F_GETPATH, buf.as_mut_ptr()) } == -1 {
        return None;
    }
    let end = buf.iter().position(|&b| b == 0)?;
    (end > 0).then(|| String::from_utf8_lossy(&buf[..end]).into_owned())
}

#[cfg(not(target_os = "macos"))]
fn fd_path(fd: RawFd) -> Option<String> {
    std::fs::read_link(format!("/proc/self/fd/{fd}"))
        .ok()
        .map(|path| path.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsRawFd;

    #[test]
    fn describes_an_open_file_by_type_and_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("probe.txt");
        let file = std::fs::File::create(&path).unwrap();
        let what = describe_fd(file.as_raw_fd()).unwrap();
        assert!(what.starts_with("file:"), "{what}");
        // macOS 的临时目录经 /private 软链,只比对文件名。
        assert!(what.ends_with("/probe.txt"), "{what}");
    }

    #[test]
    fn describes_a_socket_as_sock() {
        let (a, _b) = std::os::unix::net::UnixStream::pair().unwrap();
        let what = describe_fd(a.as_raw_fd()).unwrap();
        assert!(what.starts_with("sock"), "{what}");
    }

    #[test]
    fn a_closed_or_invalid_number_is_absent() {
        assert_eq!(describe_fd(-1), None);
        // 远超任何进程上限的编号:不可能开着,也不会与并行测试打开的 fd 撞号。
        assert_eq!(describe_fd(1 << 30), None);
    }

    #[test]
    fn snapshot_lists_the_open_numbers_it_can_see() {
        let (a, _b) = std::os::unix::net::UnixStream::pair().unwrap();
        let fd = a.as_raw_fd();
        let snapshot = fd_snapshot(fd + 1);
        assert!(snapshot.contains(&format!("{fd}=sock")), "{snapshot}");
    }
}
