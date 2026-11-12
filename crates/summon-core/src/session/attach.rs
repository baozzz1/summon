//! 入站附件(图片/文件等)落地的纯辅助:文件名消毒、图片扩展名嗅探、喂给 agent 的提示文案。
//! IO(下载/落盘)在 summon 层;这里只做可单测的纯逻辑。

/// 由文件头魔数猜图片扩展名(下载的图片消息无文件名时用)。认不出回 `bin`。
pub fn guess_image_ext(bytes: &[u8]) -> &'static str {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G']) {
        "png"
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        "jpg"
    } else if bytes.starts_with(b"GIF8") {
        "gif"
    } else if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        "webp"
    } else if bytes.starts_with(&[0x42, 0x4D]) {
        "bmp"
    } else {
        "bin"
    }
}

/// 把可能含路径分隔符/控制字符的原始文件名收敛成安全 basename(挡路径穿越);为空/退化回 fallback。
pub fn safe_file_name(raw: &str, fallback: &str) -> String {
    let base = raw.rsplit(['/', '\\']).next().unwrap_or(raw);
    let cleaned: String = base.chars().filter(|c| !c.is_control()).collect();
    let cleaned = cleaned.trim();
    if cleaned.is_empty() || cleaned == "." || cleaned == ".." {
        fallback.to_string()
    } else {
        cleaned.to_string()
    }
}

/// 把已落地的附件(类型, 本地绝对路径)汇成一段提示,附到注入 prompt 末尾,
/// 让 agent 知道可用 Read 等工具查看。无附件回空串(调用方据此决定是否拼接)。
pub fn resource_note(items: &[(String, String)]) -> String {
    if items.is_empty() {
        return String::new();
    }
    let mut s = summon_i18n::format!("r25009a7d78", items.len());
    for (kind, path) in items {
        let label = match kind.as_str() {
            "image" => summon_i18n::text!("rd24c10d37d"),
            "file" => summon_i18n::text!("r39932f24fe"),
            "audio" => summon_i18n::text!("r89fcbbfdca"),
            "media" => summon_i18n::text!("rc20f7618d3"),
            _ => summon_i18n::text!("rc7225dbf10"),
        };
        s.push_str(&format!("\n- [{label}] {path}"));
    }
    s.push(')');
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guesses_common_image_exts() {
        assert_eq!(guess_image_ext(&[0x89, b'P', b'N', b'G', 0x0d]), "png");
        assert_eq!(guess_image_ext(&[0xFF, 0xD8, 0xFF, 0xE0]), "jpg");
        assert_eq!(guess_image_ext(b"GIF89a"), "gif");
        assert_eq!(guess_image_ext(b"RIFF\x00\x00\x00\x00WEBPVP8 "), "webp");
        assert_eq!(guess_image_ext(b"\x00\x01"), "bin");
    }

    #[test]
    fn safe_file_name_strips_path_and_falls_back() {
        assert_eq!(safe_file_name("报告.pdf", "fb"), "报告.pdf");
        // 路径穿越被收敛成 basename
        assert_eq!(safe_file_name("../../etc/passwd", "fb"), "passwd");
        assert_eq!(safe_file_name("a/b/c.png", "fb"), "c.png");
        // 空 / 退化 → fallback
        assert_eq!(safe_file_name("", "fb"), "fb");
        assert_eq!(safe_file_name("..", "fb"), "fb");
    }

    #[test]
    fn resource_note_lists_each_with_label() {
        assert_eq!(resource_note(&[]), "");
        let note = resource_note(&[
            ("image".to_string(), "/tmp/a.png".to_string()),
            ("file".to_string(), "/tmp/b.pdf".to_string()),
        ]);
        assert!(note.contains("2 个附件"));
        assert!(note.contains("[图片] /tmp/a.png"));
        assert!(note.contains("[文件] /tmp/b.pdf"));
    }
}
