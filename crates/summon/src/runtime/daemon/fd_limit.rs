//! 启动期把 fd 软上限提到够用。
//!
//! launchd 拉起的进程软上限是 256(`launchctl limit maxfiles`),systemd 用户单元是 1024,从人的
//! shell 起的则继承 shell 的值。daemon 常驻,手里攥着 socket、RPC 管道、pty、转录文件,上限用满
//! 之后 accept / spawn / open 全部 EMFILE,表现是静默降级。所以不管谁拉起的,启动时都把软上限提到 [`WANT`](不超过
//! 硬上限;macOS 还受 `kern.maxfilesperproc` 约束,超了 setrlimit 报 EINVAL)。

use super::*;
use nix::libc;

/// 想要的软上限。只升不降:已经更高就不动。
const WANT: u64 = 65_536;

/// 该把软上限提到多少;不用动为 `None`。
fn soft_target(soft: u64, hard: u64, ceiling: Option<u64>) -> Option<u64> {
    let target = WANT.min(hard).min(ceiling.unwrap_or(u64::MAX));
    (target > soft).then_some(target)
}

/// 提升软上限并记一行日志。失败只记日志:上限低只是少了余量,不该挡住启动。
pub(super) fn raise() {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY:getrlimit 只往 `limit` 里写。
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } != 0 {
        log(&summon_i18n::format!(
            "rc689390d4f",
            std::io::Error::last_os_error()
        ));
        return;
    }
    let (soft, hard) = (limit.rlim_cur, limit.rlim_max);
    let Some(target) = soft_target(soft, hard, per_process_ceiling()) else {
        return;
    };
    limit.rlim_cur = target;
    // SAFETY:setrlimit 只读 `limit`;新软上限不超过硬上限。
    if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limit) } == 0 {
        log(&summon_i18n::format!("receb85ab72"));
    } else {
        log(&summon_i18n::format!(
            "rd2fc44b966",
            std::io::Error::last_os_error()
        ));
    }
}

/// macOS 单进程 fd 上限;其它平台由硬上限约束即可。
#[cfg(target_os = "macos")]
fn per_process_ceiling() -> Option<u64> {
    let mut value: libc::c_int = 0;
    let mut len = std::mem::size_of::<libc::c_int>();
    // SAFETY:kern.maxfilesperproc 的值就是一个 int;缓冲区与长度正好容纳它。
    let rc = unsafe {
        libc::sysctlbyname(
            c"kern.maxfilesperproc".as_ptr(),
            (&mut value as *mut libc::c_int).cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    (rc == 0 && value > 0).then_some(value as u64)
}

#[cfg(not(target_os = "macos"))]
fn per_process_ceiling() -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_launchd_default_is_raised_within_the_hard_limit_and_the_ceiling() {
        assert_eq!(soft_target(256, u64::MAX, None), Some(WANT));
        assert_eq!(soft_target(256, 4096, None), Some(4096));
        assert_eq!(soft_target(256, u64::MAX, Some(10_240)), Some(10_240));
    }

    #[test]
    fn a_limit_that_is_already_high_enough_is_left_alone() {
        assert_eq!(soft_target(1 << 20, u64::MAX, None), None);
        assert_eq!(soft_target(WANT, u64::MAX, None), None);
        assert_eq!(soft_target(4096, 4096, None), None);
    }

    #[test]
    fn raising_leaves_the_process_at_least_at_the_target() {
        raise();
        let mut limit = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        // SAFETY:同上,只写 `limit`。
        assert_eq!(
            unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) },
            0
        );
        let want = WANT
            .min(limit.rlim_max)
            .min(per_process_ceiling().unwrap_or(u64::MAX));
        assert!(limit.rlim_cur >= want, "{} < {want}", limit.rlim_cur);
    }
}
