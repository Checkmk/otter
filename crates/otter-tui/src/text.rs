/// Split `s` into chunks of at most `width` characters.
/// If `width` is 0, returns the whole string as a single chunk.
pub fn wrap_into_chunks(s: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![s.to_string()];
    }
    let mut chunks = Vec::new();
    let mut remaining = s;
    loop {
        let mut byte_end = remaining.len();
        for (char_count, (byte_idx, _)) in remaining.char_indices().enumerate() {
            if char_count == width {
                byte_end = byte_idx;
                break;
            }
        }
        chunks.push(remaining[..byte_end].to_string());
        remaining = &remaining[byte_end..];
        if remaining.is_empty() {
            break;
        }
    }
    chunks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_string_produces_one_empty_chunk() {
        assert_eq!(wrap_into_chunks("", 10), vec![""]);
    }

    #[test]
    fn string_shorter_than_width_is_single_chunk() {
        assert_eq!(wrap_into_chunks("hello", 10), vec!["hello"]);
    }

    #[test]
    fn string_exactly_at_width_is_single_chunk() {
        assert_eq!(wrap_into_chunks("hello", 5), vec!["hello"]);
    }

    #[test]
    fn string_over_width_splits_at_boundary() {
        assert_eq!(wrap_into_chunks("abcdef", 3), vec!["abc", "def"]);
    }

    #[test]
    fn zero_width_returns_whole_string() {
        assert_eq!(wrap_into_chunks("hello", 0), vec!["hello"]);
    }

    #[test]
    fn multibyte_unicode_splits_by_char_not_byte() {
        // "é" is 2 bytes but 1 char
        let s = "éàü!";
        let chunks = wrap_into_chunks(s, 2);
        assert_eq!(chunks, vec!["éà", "ü!"]);
    }
}
