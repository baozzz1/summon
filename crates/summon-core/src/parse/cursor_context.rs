//! Cursor 本地 ConversationStateStructure 的上下文快照。
//!
//! CLI 状态栏读取 token_details（字段 5）里的 used_tokens/max_tokens（字段 1/2）。
//! 这里只认这组原生观测，不把 result.usage 的计费用量当作上下文占用。

use crate::context_budget::ContextBudget;

const MAX_ROOT_BYTES: usize = 4 * 1024 * 1024;

pub fn parse_context(root: &[u8]) -> Option<ContextBudget> {
    if root.len() > MAX_ROOT_BYTES {
        return None;
    }
    let mut bytes = root;
    let mut details = None;
    while !bytes.is_empty() {
        let (number, value) = field(&mut bytes)?;
        if number == 5 {
            if details.is_some() {
                return None;
            }
            let Field::Bytes(value) = value else {
                return None;
            };
            details = Some(value);
        }
    }
    let mut bytes = details?;
    let mut used = None;
    let mut window = None;
    while !bytes.is_empty() {
        let (number, value) = field(&mut bytes)?;
        if matches!(number, 1 | 2) {
            let Field::Varint(value) = value else {
                return None;
            };
            // 上游两项是 uint32；超范围意味着 schema 或数据已不可信。
            let value = u32::try_from(value).ok()?;
            let slot = if number == 1 { &mut used } else { &mut window };
            if slot.replace(u64::from(value)).is_some() {
                return None;
            }
        }
    }
    let window = window.filter(|value| *value > 0)?;
    Some(ContextBudget::from_tokens(used.unwrap_or(0), Some(window)))
}

enum Field<'a> {
    Varint(u64),
    Bytes(&'a [u8]),
    Fixed,
}

fn varint(bytes: &mut &[u8]) -> Option<u64> {
    let mut value = 0;
    for shift in (0..70).step_by(7) {
        let (&byte, rest) = bytes.split_first()?;
        *bytes = rest;
        if shift == 63 && byte > 1 {
            return None;
        }
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Some(value);
        }
    }
    None
}

fn field<'a>(bytes: &mut &'a [u8]) -> Option<(u64, Field<'a>)> {
    let tag = varint(bytes)?;
    let number = tag >> 3;
    if number == 0 || number > 0x1fff_ffff {
        return None;
    }
    let value = match tag & 7 {
        0 => Field::Varint(varint(bytes)?),
        wire @ (1 | 2 | 5) => {
            let size = match wire {
                1 => 8,
                2 => usize::try_from(varint(bytes)?).ok()?,
                _ => 4,
            };
            let (value, rest) = bytes.split_at_checked(size)?;
            *bytes = rest;
            if wire == 2 {
                Field::Bytes(value)
            } else {
                Field::Fixed
            }
        }
        _ => return None,
    };
    Some((number, value))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_native_token_details_and_skips_other_conversation_fields() {
        // used=24854，max=1000000；前后夹着无关字段，且不包含任何 result.usage。
        let root = [10, 2, 1, 2, 42, 8, 8, 150, 194, 1, 16, 192, 132, 61, 80, 2];
        assert_eq!(
            parse_context(&root),
            Some(ContextBudget::from_tokens(24_854, Some(1_000_000)))
        );
    }

    #[test]
    fn rejects_truncated_ambiguous_and_missing_window_snapshots() {
        for root in [
            &[][..],
            &[42, 2, 8, 4][..],
            &[42, 2, 16, 0][..],
            &[42, 4, 8, 4, 16][..],
            &[42, 6, 8, 4, 16, 5, 16, 6][..],
            &[42, 4, 8, 4, 16, 5, 42, 4, 8, 4, 16, 5][..],
            &[42, 4, 8, 4, 16, 5, 128][..],
        ] {
            assert_eq!(parse_context(root), None, "root={root:?}");
        }
    }

    #[test]
    fn accepts_zero_occupancy_and_preserves_over_window_observations() {
        assert_eq!(
            parse_context(&[42, 2, 16, 5]),
            Some(ContextBudget::from_tokens(0, Some(5)))
        );
        assert_eq!(
            parse_context(&[42, 4, 8, 6, 16, 5]),
            Some(ContextBudget::from_tokens(6, Some(5)))
        );
    }
}
