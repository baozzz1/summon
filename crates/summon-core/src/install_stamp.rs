//! 「两份 summon 谁更新」的判据,以及稳定落点(`~/.summon/bin/summon`)旁边那份构建戳。
//!
//! 落点里那份与当前这份可能来自不同的安装或构建,而两份可以版本号相同、提交不同 —— 只比版本号,
//! 旧的那份照样被当成「一样」。所以判据要能分出同号的两份,而且各处要算出同一个结论:
//!
//! - 运行期自发布(`setup/self_install.rs`):`daemon start` / `autostart enable`。**绝不拿旧的
//!   覆盖新的**;
//! - `deploy.sh`(经隐藏命令 `summon install-stamp` 回到这里):装的比落点旧时提示。
//!
//! 边界用例集中在 `tests/fixtures/build_order_cases.json`,正反两个方向都要过。
//!
//! **判据**:先比 semver(`x.y.z` 数值比较);同号时同一个提交就是同一份构建,否则比构建所在
//! HEAD 的提交时间(committer time,build.rs 嵌入)。分支并行时提交时间只是近似;对「develop
//! 线性前进、发版从 develop 打包」这条主路径它是准确的。
//!
//! **判据拿不到就不下结论**([`compare`] 返回 `None`):旧二进制没嵌提交时间、版本号不是纯
//! `x.y.z`、同号同秒不同提交。拿到 `None` 的一方各自取「不改动现状」的方向 —— 自发布不覆盖落点
//! (daemon 仍用落点那份起,自启单元登记的就是它),deploy.sh 照装但提示。
//!
//! **戳必须证明自己描述的是落点里那个文件**:戳里记着发布那一刻落点文件的 size / mtime(纳秒)/
//! inode。落点被别的途径换过(老版本 deploy.sh、手工 cp、部署失败回滚)时对不上,戳作废 ——
//! 宁可退回「判据拿不到」,也不拿一份过期的戳去做决定。这道证明只用一次 stat,读戳的一方
//! 不必 spawn 落点那份去问版本。

use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::os::unix::fs::MetadataExt;

/// 构建戳的文件名,与落点里的 summon 同目录。
pub const STAMP_FILE: &str = "summon.build.json";

/// 一份 summon 的构建信息:`install-stamp print` 打印它,落点的构建戳把它放在 `build` 下,
/// 两处字段一致。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildInfo {
    /// semver,单一事实源是 `crates/summon/Cargo.toml`。
    pub version: String,
    /// 构建时 HEAD 的短 sha;不在 git 里构建时是 `unknown`。
    pub commit: String,
    /// 构建时 HEAD 的提交时间(Unix 秒);不在 git 里构建时没有。
    #[serde(default)]
    pub commit_time: Option<u64>,
}

impl BuildInfo {
    /// 给人看的一行:`0.6.9 (48698ad7, 提交于 2026-10-16 19:39)`。
    pub fn describe(&self) -> String {
        let when = self
            .commit_time
            .and_then(|secs| chrono::DateTime::from_timestamp(i64::try_from(secs).ok()?, 0))
            .map(|t| {
                t.with_timezone(&chrono::Local)
                    .format(summon_i18n::text!("r698f7ac159"))
                    .to_string()
            })
            .unwrap_or_else(|| summon_i18n::text!("r5f81e0b577").to_string());
        format!("{} ({}, {when})", self.version, self.commit)
    }
}

/// 落点旁边的构建戳。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstallStamp {
    pub build: BuildInfo,
    /// 写戳那一刻落点文件的身份,见 [`file_identity`]。
    pub file: String,
}

impl InstallStamp {
    pub fn new(build: BuildInfo, target: &std::fs::Metadata) -> Self {
        Self {
            build,
            file: file_identity(target),
        }
    }

    /// 戳确实描述 `target` 这个文件时才交出它的构建信息。
    pub fn build_of(&self, target: &std::fs::Metadata) -> Option<&BuildInfo> {
        (self.file == file_identity(target)).then_some(&self.build)
    }
}

/// 文件身份 `size:mtime_ns:ino`,全用十进制整数 —— 换别的语言读戳也能逐字节拼出同一个串
/// (浮点装不下纳秒)。
pub fn file_identity(meta: &std::fs::Metadata) -> String {
    let mtime_ns = i128::from(meta.mtime()) * 1_000_000_000 + i128::from(meta.mtime_nsec());
    format!("{}:{mtime_ns}:{}", meta.size(), meta.ino())
}

/// `a` 相对 `b`:`Greater` 是 `a` 更新。判据拿不到返回 `None`(规则见模块文档)。
pub fn compare(a: &BuildInfo, b: &BuildInfo) -> Option<Ordering> {
    let (va, vb) = (semver(&a.version)?, semver(&b.version)?);
    if va != vb {
        return Some(va.cmp(&vb));
    }
    if same_commit(&a.commit, &b.commit) {
        return Some(Ordering::Equal);
    }
    match (a.commit_time, b.commit_time) {
        (Some(ta), Some(tb)) if ta != tb => Some(ta.cmp(&tb)),
        _ => None,
    }
}

/// 是不是发版用的纯 `x.y.z`(与 [`compare`] 认的版本号同一套)。
pub fn is_release_version(version: &str) -> bool {
    semver(version).is_some()
}

/// 发版用的纯 `x.y.z` 拆成 `(x, y, z)`;别的形态为 `None`(与 [`compare`] 认的版本号同一套)。
pub fn release_version(version: &str) -> Option<(u64, u64, u64)> {
    semver(version)
}

/// 只认纯 `x.y.z`:version.rs 的测试把版本号钉成这个形态,读出别的就当判据拿不到,
/// 不去猜预发布号的排序。
fn semver(version: &str) -> Option<(u64, u64, u64)> {
    let mut parts = version.split('.');
    let mut next = || -> Option<u64> {
        let part = parts.next()?;
        if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        part.parse().ok()
    };
    let parsed = (next()?, next()?, next()?);
    parts.next().is_none().then_some(parsed)
}

/// 同一个提交。短 sha 的长度随仓库对象数与 `core.abbrev` 变(CI 与本机可以一个 7 位一个 8 位),
/// 所以按前缀认;`unknown` 不是 sha,两个 `unknown` 不算同一个提交。
fn same_commit(a: &str, b: &str) -> bool {
    let sha = |s: &str| s.len() >= 7 && s.bytes().all(|c| c.is_ascii_hexdigit());
    sha(a) && sha(b) && (a.starts_with(b) || b.starts_with(a))
}

/// `summon --version` 的输出 → 构建信息(没有提交时间)。给没有构建戳的落点兜底用:
/// 早于构建戳的版本也打 `summon <x.y.z> (<sha>)`,再早的只有 `summon <x.y.z>`。
pub fn parse_version_output(text: &str) -> Option<BuildInfo> {
    let line = text.lines().next()?.trim();
    let rest = line.strip_prefix("summon ")?;
    let (version, commit) = match rest.split_once(' ') {
        Some((version, tail)) => {
            let commit = tail.strip_prefix('(')?.strip_suffix(')')?;
            (version, commit)
        }
        None => (rest, "unknown"),
    };
    semver(version)?;
    Some(BuildInfo {
        version: version.to_string(),
        commit: commit.to_string(),
        commit_time: None,
    })
}

/// 落点里那份是什么。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Installed {
    /// 落点没有 summon。
    Absent,
    Known(BuildInfo),
    /// 有文件,问它构建时没在时限内答上(附原因)。刚拷进来的二进制在 macOS 首次执行要过一遍
    /// 扫描、机器负载高时也会这样:慢不等于坏,所以按「判不出」处理,不按「认不出」。
    Unanswered(String),
    /// 有文件但读不出它的构建(附原因):起不来、非零退出、输出不像 summon。
    Unreadable(String),
}

/// 「我」相对落点那份。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Relation {
    Absent,
    Newer,
    Same,
    Older,
    Unknown,
}

impl Relation {
    /// deploy.sh 按这个词分支,改了要同步改脚本。
    pub fn word(&self) -> &'static str {
        match self {
            Relation::Absent => "absent",
            Relation::Newer => "newer",
            Relation::Same => "same",
            Relation::Older => "older",
            Relation::Unknown => "unknown",
        }
    }
}

pub fn relation(mine: &BuildInfo, installed: &Installed) -> Relation {
    match installed {
        Installed::Absent => Relation::Absent,
        Installed::Unanswered(_) | Installed::Unreadable(_) => Relation::Unknown,
        Installed::Known(theirs) => match compare(mine, theirs) {
            Some(Ordering::Greater) => Relation::Newer,
            Some(Ordering::Equal) => Relation::Same,
            Some(Ordering::Less) => Relation::Older,
            None => Relation::Unknown,
        },
    }
}

/// 运行期自发布该怎么做。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Plan {
    /// 落点为空、或我不比它旧:发布。同一个提交也照发 —— 换成同一提交的构建无害,还顺带修好
    /// 上一次中途失败、只换了一半的落点(三件套逐个替换,戳最后才写)。
    Publish,
    /// 落点那份更新:不碰它,用它。
    KeepInstalled,
    /// 判不出新旧(典型是不带提交时间的老版本,或问它构建时超时):不碰它,也用它 ——
    /// 自启单元登记的就是它,用它起才保得住托管。这条路保守的是「落点不被覆盖」。
    Undecided,
    /// 落点有文件却认不出它的构建(起不来、非零退出、输出不像 summon):不碰它,也不用它。
    Unusable,
}

pub fn plan(mine: &BuildInfo, installed: &Installed) -> Plan {
    if let Installed::Unreadable(_) = installed {
        return Plan::Unusable;
    }
    match relation(mine, installed) {
        Relation::Absent | Relation::Newer | Relation::Same => Plan::Publish,
        Relation::Older => Plan::KeepInstalled,
        Relation::Unknown => Plan::Undecided,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_versions_are_plain_x_y_z() {
        assert!(is_release_version("0.6.15"));
        for bad in [
            "0.6",
            "next",
            "latest",
            "0.6.15-beta.1",
            "v0.6.15",
            "0.6.15.1",
            "",
        ] {
            assert!(!is_release_version(bad), "{bad}");
        }
    }

    fn build(version: &str, commit: &str, commit_time: Option<u64>) -> BuildInfo {
        BuildInfo {
            version: version.to_string(),
            commit: commit.to_string(),
            commit_time,
        }
    }

    #[derive(Deserialize)]
    struct Case {
        name: String,
        a: BuildInfo,
        b: BuildInfo,
        expect: String,
    }

    fn word(order: Option<Ordering>) -> &'static str {
        match order {
            Some(Ordering::Greater) => "newer",
            Some(Ordering::Equal) => "same",
            Some(Ordering::Less) => "older",
            None => "unknown",
        }
    }

    /// 用例集中在一份夹具里:改判据要让每条边界用例在正反两个方向上都过。
    #[test]
    fn compare_matches_the_shared_cases_in_both_directions() {
        let cases: Vec<Case> =
            serde_json::from_str(include_str!("../tests/fixtures/build_order_cases.json")).unwrap();
        assert!(cases.len() >= 10, "用例被删掉了一大半");
        for case in cases {
            assert_eq!(
                word(compare(&case.a, &case.b)),
                case.expect,
                "{}",
                case.name
            );
            let mirrored = compare(&case.b, &case.a).map(Ordering::reverse);
            assert_eq!(word(mirrored), case.expect, "{}(反过来比)", case.name);
        }
    }

    #[test]
    fn a_stamp_only_vouches_for_the_file_it_was_written_for() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("summon");
        std::fs::write(&path, "v1").unwrap();
        let stamp = InstallStamp::new(
            build("0.6.9", "abcdef1", Some(1)),
            &std::fs::metadata(&path).unwrap(),
        );
        assert!(stamp.build_of(&std::fs::metadata(&path).unwrap()).is_some());

        // 换一个 inode 戳就作废,哪怕内容、大小、mtime 全都一样 —— deploy.sh 失败回滚时 mv 回来的
        // 正是这样一份 `cp -p` 备份,能把它和戳描述的那份分开的只有 inode。
        let original = std::fs::metadata(&path).unwrap();
        let other = tmp.path().join("other");
        std::fs::write(&other, "v1").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&other)
            .unwrap()
            .set_modified(original.modified().unwrap())
            .unwrap();
        std::fs::rename(&other, &path).unwrap();
        let swapped = std::fs::metadata(&path).unwrap();
        assert_eq!(
            (swapped.size(), swapped.mtime(), swapped.mtime_nsec()),
            (original.size(), original.mtime(), original.mtime_nsec()),
            "前提:两份只差 inode"
        );
        assert!(stamp.build_of(&swapped).is_none());
    }

    #[test]
    fn a_stamp_survives_a_json_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("summon");
        std::fs::write(&path, "v1").unwrap();
        let stamp = InstallStamp::new(
            build("0.6.9", "abcdef1", None),
            &std::fs::metadata(&path).unwrap(),
        );
        let text = serde_json::to_string(&stamp).unwrap();
        assert_eq!(serde_json::from_str::<InstallStamp>(&text).unwrap(), stamp);
        // 缺 commit_time 字段(手写、或更早的写法)按「没有」读,不报错。
        let bare: BuildInfo =
            serde_json::from_str(r#"{"version":"0.6.9","commit":"abcdef1"}"#).unwrap();
        assert_eq!(bare.commit_time, None);
    }

    #[test]
    fn version_output_of_older_binaries_parses_without_commit_time() {
        assert_eq!(
            parse_version_output("summon 0.6.9 (48698ad7)\n"),
            Some(build("0.6.9", "48698ad7", None))
        );
        assert_eq!(
            parse_version_output("summon 0.2.0"),
            Some(build("0.2.0", "unknown", None))
        );
        assert_eq!(parse_version_output("summon unknown"), None);
        assert_eq!(parse_version_output("error: unexpected argument"), None);
        assert_eq!(parse_version_output(""), None);
    }

    #[test]
    fn publishing_never_replaces_a_newer_or_unknown_install() {
        let mine = build("0.6.9", "aaaaaaa", Some(200));
        let older = Installed::Known(build("0.6.9", "bbbbbbb", Some(100)));
        let newer = Installed::Known(build("0.6.9", "ccccccc", Some(300)));
        let same = Installed::Known(build("0.6.9", "aaaaaaa", None));
        let unknown = Installed::Known(build("0.6.9", "ddddddd", None));

        assert_eq!(plan(&mine, &Installed::Absent), Plan::Publish);
        assert_eq!(plan(&mine, &older), Plan::Publish);
        assert_eq!(plan(&mine, &same), Plan::Publish);
        assert_eq!(plan(&mine, &newer), Plan::KeepInstalled);
        assert_eq!(plan(&mine, &unknown), Plan::Undecided);
        assert_eq!(
            plan(&mine, &Installed::Unreadable("坏了".into())),
            Plan::Unusable
        );
        assert_eq!(
            plan(&mine, &Installed::Unanswered("超时".into())),
            Plan::Undecided
        );
    }

    #[test]
    fn describe_names_the_commit_and_says_when_time_is_missing() {
        assert_eq!(
            build("0.6.9", "48698ad7", None).describe(),
            "0.6.9 (48698ad7, 无提交时间)"
        );
        assert!(build("0.6.9", "48698ad7", Some(1_760_000_000))
            .describe()
            .starts_with("0.6.9 (48698ad7, 提交于 20"));
    }
}
