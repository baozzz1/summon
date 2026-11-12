//! 控制台写 `config.yaml` / `agent.yaml` 的唯一通路:持各文件既有的锁 → 锁内比版本 → 改写 →
//! 三道校验 → 原子替换并保留权限位。任一步不过就不写,文件一字不动。
//!
//! 三道校验:
//! 1. 改完能解析成对应结构体,且原文校验(`raw_check`)没有这次改出来的错误;
//! 2. 读回即所写:每个被设的键解析出来正是所写的值,被删的键确实不在了;
//! 3. 其余内容解析出来与原文件相同(只对按键改写成立;整文替换由人看过差异再保存)。
//!
//! 锁:config.yaml 与 `config_edit`(Menu Bar、`summon sentinel set`)共用 `.config.yaml.lock`;
//! agent.yaml 与 `bind`、免 @ 开启共用 `manifest_write_lock`(非阻塞,被占着回 busy)。

use std::fs::File;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use anyhow::Context;
use serde_yaml::Value;
use summon_core::dashboard::raw_check;
use summon_core::dashboard::RawKind;
use summon_core::yaml_edit::{self, Edit};

/// 文件不存在时的版本。
pub(crate) const ABSENT: &str = "absent";

/// 内容版本:SHA-256 的前 16 个十六进制字符;文件不存在为 `absent`。
pub(crate) fn revision(content: Option<&[u8]>) -> String {
    match content {
        None => ABSENT.to_string(),
        Some(bytes) => {
            let digest = ring::digest::digest(&ring::digest::SHA256, bytes);
            digest.as_ref()[..8]
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect()
        }
    }
}

/// 读文件;不存在为 `None`,其余 IO 错误上抛。
pub(crate) fn read_optional(path: &Path) -> anyhow::Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| summon_i18n::format!("re43971131a", path.display())),
    }
}

/// 由当前全文算出新全文。
pub(crate) type PatchFn = Box<dyn FnOnce(&str) -> anyhow::Result<String>>;

/// 这次改动的内容。
pub(crate) enum Change {
    /// 按键改写(保留注释与无关内容)。
    Edits(Vec<Edit>),
    /// 整文替换(原文模式)。
    Full(String),
    /// 由锁内读到的当前全文算出新全文(专用写入者已有的改写函数,如免 @ 关闭)。
    Patch(PatchFn),
}

#[derive(Debug)]
pub(crate) enum WriteError {
    /// 读取之后文件被别处改过。
    Conflict,
    /// 改写后的内容不合法,或这种写法判不准。`key` 是出错那一项(知道的话)。
    Invalid {
        message: String,
        key: Option<String>,
    },
    /// 拦下这次写入的业务规则(防锁死这类),原样回给页面。
    Refused(String),
    /// 另一个写者正持着锁。
    Busy(String),
    Io(anyhow::Error),
}

impl std::fmt::Display for WriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Conflict => write!(f, "{}", summon_i18n::format!("r7a1678aef9")),
            Self::Invalid { message, .. } | Self::Refused(message) | Self::Busy(message) => {
                f.write_str(message)
            }
            Self::Io(e) => write!(f, "{e:#}"),
        }
    }
}

/// 写成之后的结果。
#[derive(Debug)]
pub(crate) struct Saved {
    pub(crate) revision: String,
    pub(crate) after: String,
    /// 内容与原来相同,没有落盘。
    pub(crate) unchanged: bool,
}

/// 锁内最后一道业务检查:拿到改动前后的全文,不过就拒绝(如防锁死)。
pub(crate) type Guard<'a> = &'a dyn Fn(Option<&str>, &str) -> Result<(), WriteError>;

/// 写一份 YAML。`expected` 为读取时的版本;`None` 只给 [`Change::Patch`] 用(改动本身就基于锁内
/// 读到的当前内容)。
pub(crate) fn write_yaml(
    path: &Path,
    kind: RawKind,
    expected: Option<&str>,
    change: Change,
    guard: Guard<'_>,
) -> Result<Saved, WriteError> {
    let _lock = lock(path, kind)?;
    let before = read_optional(path).map_err(WriteError::Io)?;
    if let Some(expected) = expected {
        if revision(before.as_deref().map(str::as_bytes)) != expected {
            return Err(WriteError::Conflict);
        }
    }
    if kind == RawKind::Agent && before.is_none() {
        return Err(WriteError::Io(anyhow::anyhow!(summon_i18n::format!(
            "r497554369d",
            path.display()
        ))));
    }
    let old = before.clone().unwrap_or_default();
    let (after, edits) = match change {
        Change::Edits(edits) => {
            let after = yaml_edit::apply(&old, &edits).map_err(|e| WriteError::Invalid {
                message: summon_i18n::format!("r7354b1a27f"),
                key: edit_key(&edits),
            })?;
            (after, Some(edits))
        }
        Change::Full(text) => (text, None),
        Change::Patch(patch) => {
            let after = patch(&old).map_err(|e| WriteError::Invalid {
                message: format!("{e:#}"),
                key: None,
            })?;
            (after, None)
        }
    };
    // 第一道:能解析成结构体,且没有这次改出来的错误。
    let check = raw_check::check(kind, &old, &after);
    if !check.ok() {
        let first = check.errors.first();
        return Err(WriteError::Invalid {
            message: first
                .map(|issue| issue.message.clone())
                .unwrap_or_else(|| summon_i18n::text!("rdb14404d5f").to_string()),
            key: first.and_then(|issue| issue.key.clone()),
        });
    }
    if let Some(edits) = &edits {
        verify_edits(&old, &after, edits)?;
    }
    guard(before.as_deref(), &after)?;
    if before.as_deref() == Some(after.as_str()) {
        return Ok(Saved {
            revision: revision(Some(after.as_bytes())),
            after,
            unchanged: true,
        });
    }
    let mode = std::fs::metadata(path)
        .map(|m| m.permissions().mode() & 0o777)
        .unwrap_or(0o600);
    let mut raced = false;
    let committed =
        crate::runtime::fsio::atomic_write_mode_checked(path, after.as_bytes(), mode, || {
            // 不持锁的写者(手改)可能在锁内这段时间里落了盘:提交前再比一次。
            let now = read_optional(path)?;
            if now != before {
                raced = true;
                anyhow::bail!(summon_i18n::format!("r8e82b0c0ac"));
            }
            Ok(())
        });
    match committed {
        Ok(()) => {}
        Err(_) if raced => return Err(WriteError::Conflict),
        Err(e) => return Err(WriteError::Io(e)),
    }
    // 读回即所写。
    let written = read_optional(path).map_err(WriteError::Io)?;
    if written.as_deref() != Some(after.as_str()) {
        return Err(WriteError::Conflict);
    }
    Ok(Saved {
        revision: revision(Some(after.as_bytes())),
        after,
        unchanged: false,
    })
}

pub(crate) fn lock(path: &Path, kind: RawKind) -> Result<File, WriteError> {
    match kind {
        RawKind::Agent => crate::setup::bind::manifest_write_lock(path)
            .map_err(|e| WriteError::Busy(summon_i18n::format!("r3888f92a32"))),
        RawKind::Config => {
            let dir = path
                .parent()
                .context(summon_i18n::text!("ra734729215"))
                .map_err(WriteError::Io)?;
            std::fs::create_dir_all(dir)
                .context(summon_i18n::text!("r521925648e"))
                .map_err(WriteError::Io)?;
            let lock = std::fs::OpenOptions::new()
                .create(true)
                .read(true)
                .write(true)
                .truncate(false)
                .open(dir.join(".config.yaml.lock"))
                .context(summon_i18n::text!("rffee7c7ce5"))
                .map_err(WriteError::Io)?;
            lock.lock()
                .context(summon_i18n::text!("r09eae470ab"))
                .map_err(WriteError::Io)?;
            Ok(lock)
        }
    }
}

fn edit_key(edits: &[Edit]) -> Option<String> {
    edits.first().map(|edit| match edit {
        Edit::Set(path, _) | Edit::Remove(path) => path.join("."),
    })
}

fn parse(text: &str) -> Result<Value, WriteError> {
    serde_yaml::from_str::<Value>(text).map_err(|e| WriteError::Invalid {
        message: summon_i18n::format!("r46016810fa"),
        key: None,
    })
}

fn value_at<'v>(root: &'v Value, path: &[String]) -> Option<&'v Value> {
    path.iter()
        .try_fold(root, |cur, seg| cur.as_mapping()?.get(seg.as_str()))
}

/// 在值树上做同一组改动(期望值)。
fn apply_to_value(root: &mut Value, edit: &Edit) {
    match edit {
        Edit::Set(path, value) => {
            let mut cur = root;
            for (depth, seg) in path.iter().enumerate() {
                if !cur.is_mapping() {
                    *cur = Value::Mapping(Default::default());
                }
                let Value::Mapping(map) = cur else { return };
                let key = Value::String(seg.clone());
                if depth + 1 == path.len() {
                    map.insert(key, value.clone());
                    return;
                }
                cur = map
                    .entry(key)
                    .or_insert_with(|| Value::Mapping(Default::default()));
            }
        }
        Edit::Remove(path) => {
            let Some((last, parents)) = path.split_last() else {
                return;
            };
            let mut cur = root;
            for seg in parents {
                match cur.as_mapping_mut().and_then(|m| m.get_mut(seg.as_str())) {
                    Some(next) => cur = next,
                    None => return,
                }
            }
            if let Some(map) = cur.as_mapping_mut() {
                map.remove(last.as_str());
            }
        }
    }
}

/// 第二、三道校验。只剩注释的文件解析成 null,与空映射同义。
fn verify_edits(old: &str, after: &str, edits: &[Edit]) -> Result<(), WriteError> {
    let new_value = parse(after)?;
    for edit in edits {
        let ok = match edit {
            Edit::Set(path, value) => value_at(&new_value, path) == Some(value),
            Edit::Remove(path) => value_at(&new_value, path).is_none(),
        };
        if !ok {
            return Err(WriteError::Invalid {
                message: summon_i18n::text!("r0428d93335").to_string(),
                key: edit_key(std::slice::from_ref(edit)),
            });
        }
    }
    let mut expected = parse(old)?;
    for edit in edits {
        apply_to_value(&mut expected, edit);
    }
    let empty = |v: &Value| v.is_null() || v.as_mapping().is_some_and(|m| m.is_empty());
    if expected != new_value && !(empty(&expected) && empty(&new_value)) {
        return Err(WriteError::Invalid {
            message: summon_i18n::text!("rf994b5b9be").to_string(),
            key: None,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use summon_core::yaml_edit::path;

    fn set(key: &str, value: Value) -> Change {
        Change::Edits(vec![Edit::Set(path(key), value)])
    }

    fn no_guard() -> impl Fn(Option<&str>, &str) -> Result<(), WriteError> {
        |_, _| Ok(())
    }

    const CONFIG: &str = "# 顶部注释\nmessaging:\n  cot_visible: true  # 行尾注释\npoll_ms: 400\n";

    fn config_file(text: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        std::fs::write(&path, text).unwrap();
        (dir, path)
    }

    #[test]
    fn revision_is_short_sha256_or_absent() {
        assert_eq!(revision(None), "absent");
        // printf '' | shasum -a 256 → e3b0c442...
        assert_eq!(revision(Some(b"")), "e3b0c44298fc1c14");
        assert_eq!(revision(Some(b"a")).len(), 16);
        assert_ne!(revision(Some(b"a")), revision(Some(b"b")));
    }

    #[test]
    fn patch_rejects_stale_revision_and_leaves_file_untouched() {
        let (_dir, path) = config_file(CONFIG);
        let read_at = revision(Some(CONFIG.as_bytes()));
        // 读取之后别处改了文件(手改 / Menu Bar / 另一个页面)。
        let elsewhere = CONFIG.replace("poll_ms: 400", "poll_ms: 500");
        std::fs::write(&path, &elsewhere).unwrap();
        let err = write_yaml(
            &path,
            RawKind::Config,
            Some(&read_at),
            set("messaging.cot_visible", Value::Bool(false)),
            &no_guard(),
        )
        .unwrap_err();
        assert!(matches!(err, WriteError::Conflict), "{err:?}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), elsewhere);
    }

    #[test]
    fn edits_keep_comments_and_bump_revision() {
        let (_dir, path) = config_file(CONFIG);
        let saved = write_yaml(
            &path,
            RawKind::Config,
            Some(&revision(Some(CONFIG.as_bytes()))),
            set("messaging.cot_visible", Value::Bool(false)),
            &no_guard(),
        )
        .unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            text,
            "# 顶部注释\nmessaging:\n  cot_visible: false  # 行尾注释\npoll_ms: 400\n"
        );
        assert_eq!(saved.revision, revision(Some(text.as_bytes())));
        assert!(!saved.unchanged);
    }

    #[test]
    fn invalid_result_is_not_written() {
        let (_dir, path) = config_file(CONFIG);
        let rev = revision(Some(CONFIG.as_bytes()));
        // 类型不对:poll_ms 是数。
        let err = write_yaml(
            &path,
            RawKind::Config,
            Some(&rev),
            set("poll_ms", Value::String("fast".into())),
            &no_guard(),
        )
        .unwrap_err();
        assert!(matches!(err, WriteError::Invalid { .. }), "{err:?}");
        // 原文模式:严格子结构的未知键整份读不出。
        let err = write_yaml(
            &path,
            RawKind::Config,
            Some(&rev),
            Change::Full(format!(
                "{CONFIG}execution_targets:\n  box:\n    bogus: 1\n"
            )),
            &no_guard(),
        )
        .unwrap_err();
        assert!(matches!(err, WriteError::Invalid { .. }), "{err:?}");
        // 判不准的写法:锚点。
        let anchored = "base: &b\n  x: 1\nmessaging:\n  cot_visible: true\n";
        std::fs::write(&path, anchored).unwrap();
        let err = write_yaml(
            &path,
            RawKind::Config,
            Some(&revision(Some(anchored.as_bytes()))),
            set("messaging.cot_visible", Value::Bool(false)),
            &no_guard(),
        )
        .unwrap_err();
        assert!(matches!(err, WriteError::Invalid { .. }), "{err:?}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), anchored);
    }

    #[test]
    fn guard_refusal_leaves_file_untouched() {
        let (_dir, path) = config_file(CONFIG);
        let err = write_yaml(
            &path,
            RawKind::Config,
            Some(&revision(Some(CONFIG.as_bytes()))),
            set("poll_ms", Value::from(500)),
            &|_, _| Err(WriteError::Refused("不许".into())),
        )
        .unwrap_err();
        assert!(matches!(err, WriteError::Refused(_)));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), CONFIG);
    }

    #[test]
    fn agent_lock_busy_returns_busy() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent.yaml");
        let text = "name: a\nruntime:\n  cli: claude\n";
        std::fs::write(&path, text).unwrap();
        let held = crate::setup::bind::manifest_write_lock(&path).unwrap();
        let err = write_yaml(
            &path,
            RawKind::Agent,
            Some(&revision(Some(text.as_bytes()))),
            set("display", Value::String("A".into())),
            &no_guard(),
        )
        .unwrap_err();
        assert!(matches!(err, WriteError::Busy(_)), "{err:?}");
        drop(held);
        write_yaml(
            &path,
            RawKind::Agent,
            Some(&revision(Some(text.as_bytes()))),
            set("display", Value::String("A".into())),
            &no_guard(),
        )
        .unwrap();
        assert!(std::fs::read_to_string(&path)
            .unwrap()
            .contains("display: A"));
    }

    #[test]
    fn mode_bits_are_preserved() {
        let (_dir, path) = config_file(CONFIG);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
        write_yaml(
            &path,
            RawKind::Config,
            Some(&revision(Some(CONFIG.as_bytes()))),
            set("poll_ms", Value::from(500)),
            &no_guard(),
        )
        .unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o640);
    }

    #[test]
    fn missing_config_is_created_from_absent_revision() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        write_yaml(
            &path,
            RawKind::Config,
            Some(ABSENT),
            set("messaging.cot_visible", Value::Bool(false)),
            &no_guard(),
        )
        .unwrap();
        let cfg: summon_core::config::Config =
            serde_yaml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(cfg.messaging.cot_visible, Some(false));
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "新建的 config.yaml 只给本人读写");
    }

    #[test]
    fn patch_change_runs_on_the_locked_content() {
        let (_dir, path) = config_file(CONFIG);
        write_yaml(
            &path,
            RawKind::Config,
            None,
            Change::Patch(Box::new(|cur: &str| Ok(cur.replace("400", "450")))),
            &no_guard(),
        )
        .unwrap();
        assert!(std::fs::read_to_string(&path)
            .unwrap()
            .contains("poll_ms: 450"));
    }
}
