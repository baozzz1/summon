/// 对外展示的版本 —— **单一事实源是 `crates/summon/Cargo.toml` 的 `version`**。
///
/// 比较版本的地方(deploy 的产物校验、`daemon status`、ready/version 文件)一律用这个常量:
/// 它是纯 semver,可比较、可预期。commit sha 只进 [`LONG_VERSION`],给人看「这份二进制是哪次
/// 提交编出来的」,不参与任何相等判断 —— 同一版本可以有多次构建,把 sha 卷进比较就会假失败。
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// 构建时所在的 commit 短 sha;拿不到(不在 git 仓、没装 git)就是 `unknown`。
pub const COMMIT: &str = env!("SUMMON_COMMIT");

/// `summon --version` 打的那一行:`0.2.0 (c321c68)`。
pub const LONG_VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), " (", env!("SUMMON_COMMIT"), ")");

/// 构建所在 HEAD 的提交时间(Unix 秒);拿不到 git 时 build.rs 给空串。
const COMMIT_TIME: &str = env!("SUMMON_COMMIT_TIME");

/// 这份二进制的构建信息 —— 两份 summon 比新旧时「我」这一方的全部事实,只来自编译期。
pub fn build_info() -> summon_core::install_stamp::BuildInfo {
    summon_core::install_stamp::BuildInfo {
        version: VERSION.to_string(),
        commit: COMMIT.to_string(),
        commit_time: COMMIT_TIME.parse().ok(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 版本号必须是纯 semver:比较版本的地方(deploy 的产物校验、`daemon status`、
    /// 安装包上标的版本)全靠它可比较。掺进 sha 或构建时间都会让相等判断随构建漂移。
    #[test]
    fn version_is_plain_semver() {
        let parts: Vec<&str> = VERSION.split('.').collect();
        assert_eq!(parts.len(), 3, "版本不是 x.y.z:{VERSION}");
        for part in parts {
            assert!(
                !part.is_empty() && part.chars().all(|c| c.is_ascii_digit()),
                "版本段不是纯数字:{VERSION}"
            );
        }
    }

    /// `summon --version` 打的是 `<semver> (<commit>)`;安装脚本靠 `awk '{print $2}'` 取第二段
    /// 拿版本,所以 sha 必须在版本**之后**、且中间只隔一个空格。
    #[test]
    fn long_version_keeps_semver_in_second_field() {
        assert_eq!(LONG_VERSION, format!("{VERSION} ({COMMIT})"));
        let printed = format!("summon {LONG_VERSION}");
        let second = printed.split_whitespace().nth(1).unwrap();
        assert_eq!(second, VERSION, "--version 的第二段必须正好是版本号");
    }

    /// 拿不到 git 时 build.rs 给 `unknown`,不能让构建失败,也不能是空串
    /// (空串会让 `--version` 变成 `summon 0.2.0 ()`,第二段仍对,但读起来像坏了)。
    #[test]
    fn commit_is_never_empty() {
        assert!(!COMMIT.is_empty());
    }

    /// sha 与提交时间同出一处(构建时的 git):有 sha 就必须有时间,否则同号不同提交的两份
    /// 永远判不出新旧,发版包也会带着残缺的判据出去。
    #[test]
    fn commit_time_is_present_exactly_when_the_commit_is() {
        let build = build_info();
        assert_eq!(build.version, VERSION);
        assert_eq!(build.commit, COMMIT);
        assert_eq!(
            build.commit_time.is_some(),
            COMMIT != "unknown",
            "commit={COMMIT} commit_time={COMMIT_TIME:?}"
        );
    }
}
