use unicode_width::UnicodeWidthChar;

/// Split `s` into chunks of at most `width` characters.
/// If `width` is 0, returns the whole string as a single chunk.
pub fn wrap_into_chunks(s: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![s.to_string()];
    }
    let mut chunks = Vec::new();
    let mut chunk_start = 0;
    let mut current_width = 0;
    for (byte_idx, ch) in s.char_indices() {
        let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
        if cw > 0 && current_width > 0 && current_width + cw > width {
            chunks.push(s[chunk_start..byte_idx].to_string());
            chunk_start = byte_idx;
            current_width = 0;
        }
        current_width += cw;
    }
    chunks.push(s[chunk_start..].to_string());
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
    fn multibyte_unicode_splits_by_cell_count() {
        // GIVEN narrow multibyte chars (each is 1 cell wide)
        // WHEN wrapped to 2 cells
        // THEN we split by cell count, not byte count
        let s = "éàü!";
        assert_eq!(wrap_into_chunks(s, 2), vec!["éà", "ü!"]);
    }

    #[test]
    fn em_dash_is_one_cell_wide() {
        // GIVEN the em dash is one display cell per the unicode-width crate
        // WHEN "a—b" is wrapped to 3 cells
        // THEN it fits in a single chunk
        assert_eq!(wrap_into_chunks("a—b", 3), vec!["a—b"]);

        // WHEN wrapped to 2 cells
        // THEN it splits after the em dash (1+1 fits, +1 would overflow)
        assert_eq!(wrap_into_chunks("a—b", 2), vec!["a—", "b"]);
    }

    #[test]
    fn wide_char_takes_two_cells() {
        // GIVEN a true wide char (Hiragana 'あ' is 2 cells)
        // WHEN "aあb" is wrapped to 2 cells
        // THEN the wide char forces a chunk break, since 1+2 > 2
        assert_eq!(wrap_into_chunks("aあb", 2), vec!["a", "あ", "b"]);
    }

    #[test]
    fn wide_char_larger_than_width_still_emits_one_chunk() {
        // GIVEN a wide char that exceeds the entire width budget
        // WHEN wrapped to a width too narrow to hold it
        // THEN it is still emitted in its own chunk to make forward progress
        assert_eq!(wrap_into_chunks("あ", 1), vec!["あ"]);
    }

    #[test]
    fn combining_mark_attaches_to_its_base_char() {
        // GIVEN 'e' + U+0301 (combining acute) followed by more text
        // WHEN wrapped to 2 cells
        // THEN the combining mark stays with its base char in the first chunk,
        //      and the cell budget is not consumed by the zero-width mark
        let s = "e\u{0301}fgh";
        assert_eq!(wrap_into_chunks(s, 2), vec!["e\u{0301}f", "gh"]);
    }

    #[test]
    fn combining_mark_never_starts_a_new_chunk() {
        // GIVEN a base char that exactly fills the width, followed by a combining mark
        // WHEN wrapped
        // THEN the combining mark attaches to the base char's chunk, not a new one
        let s = "ab\u{0301}cd";
        // 'a','b' fill width 2; combining mark must stay with 'b'; then 'c','d' wrap.
        assert_eq!(wrap_into_chunks(s, 2), vec!["ab\u{0301}", "cd"]);
    }
}
