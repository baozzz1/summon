//! 外部 transcript 的上下文胶囊 IO 边界。

use anyhow::{bail, Context as _, Result};
use std::io::{Read as _, Seek as _, SeekFrom};
use std::path::Path;
use summon_core::claude_transcript::{read_context_capsule, CapsuleLimits, ContextCapsule};

pub const MAX_CONTEXT_MESSAGES: usize = 12;
pub const MAX_CONTEXT_OUTPUT_BYTES: usize = 16 * 1024;
pub const MAX_CONTEXT_LINE_BYTES: usize = 1024 * 1024;
pub const MAX_CONTEXT_TAIL_BYTES: u64 = 8 * 1024 * 1024;

/// 只尾扫最多 8MB；若从文件中段起读，丢弃第一条不完整行。parser 再独立执行 12 条/16KB/
/// 单行 1MB 的隐私收缩，不让 tool/thinking/system/sidechain 进入结果。
pub fn read_bounded_context(
    path: &Path,
    config_dir: &Path,
    expected_session_id: &str,
) -> Result<ContextCapsule> {
    let link_meta = std::fs::symlink_metadata(path).context("stat transcript")?;
    if link_meta.file_type().is_symlink() || !link_meta.is_file() {
        bail!(summon_i18n::format!("r84e4a1735d"));
    }
    let canonical_config = std::fs::canonicalize(config_dir).context("canonicalize config")?;
    let canonical = std::fs::canonicalize(path).context("canonicalize transcript")?;
    let expected_name = format!("{expected_session_id}.jsonl");
    if !canonical.starts_with(&canonical_config)
        || canonical.file_name().and_then(|name| name.to_str()) != Some(expected_name.as_str())
    {
        bail!(summon_i18n::format!("r9e059e0851"));
    }
    let mut file = std::fs::File::open(&canonical).context(summon_i18n::text!("r3b521b81fe"))?;
    let size = file.metadata().context("stat transcript")?.len();
    let start = size.saturating_sub(MAX_CONTEXT_TAIL_BYTES);
    file.seek(SeekFrom::Start(start))
        .context("seek transcript tail")?;
    let mut tail =
        Vec::with_capacity(size.saturating_sub(start).min(MAX_CONTEXT_TAIL_BYTES) as usize);
    file.take(MAX_CONTEXT_TAIL_BYTES)
        .read_to_end(&mut tail)
        .context(summon_i18n::text!("rdf6343783c"))?;
    if start > 0 {
        tail = match tail.iter().position(|byte| *byte == b'\n') {
            Some(end) => tail.split_off(end + 1),
            None => Vec::new(),
        };
    }
    read_context_capsule(
        std::io::Cursor::new(tail),
        CapsuleLimits {
            max_messages: MAX_CONTEXT_MESSAGES,
            max_output_bytes: MAX_CONTEXT_OUTPUT_BYTES,
            max_line_bytes: MAX_CONTEXT_LINE_BYTES,
        },
    )
    .context(summon_i18n::text!("r6f3091529a"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tail_scan_is_bounded_and_discards_initial_partial_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let prefix = vec![b'x'; MAX_CONTEXT_TAIL_BYTES as usize + 100];
        let visible = b"\n{\"type\":\"user\",\"isSidechain\":false,\"userType\":\"external\",\"message\":{\"role\":\"user\",\"content\":\"latest\"}}\n";
        let mut data = prefix;
        data.extend_from_slice(visible);
        std::fs::write(&path, data).unwrap();

        let capsule = read_bounded_context(&path, dir.path(), "session").unwrap();
        assert_eq!(capsule.messages.len(), 1);
        assert_eq!(capsule.messages[0].text, "latest");
    }

    #[cfg(unix)]
    #[test]
    fn symlink_and_outside_config_are_rejected() {
        use std::os::unix::fs::symlink;
        let config = tempfile::tempdir().unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();
        let link = config.path().join("link.jsonl");
        symlink(outside.path(), &link).unwrap();
        assert!(read_bounded_context(&link, config.path(), "link").is_err());
        assert!(read_bounded_context(outside.path(), config.path(), "outside").is_err());
    }

    #[test]
    fn context_rejects_same_root_transcript_with_wrong_session_id() {
        let config = tempfile::tempdir().unwrap();
        let path = config.path().join("session-b.jsonl");
        std::fs::write(&path, "{}\n").unwrap();
        assert!(read_bounded_context(&path, config.path(), "session-a").is_err());
    }
}
