use crate::adapters::tui::text_geometry as geom;

/// Word-boundary text wrapping in terminal cells. Falls back to whole-grapheme
/// chunking for single words exceeding width.
pub fn wrap_text(text: &str, width: usize) -> Vec<String> {
    if width == 0 || text.is_empty() {
        return vec![];
    }
    let mut result = Vec::new();
    for line in text.split('\n') {
        if line.is_empty() {
            result.push(String::new());
            continue;
        }
        wrap_line_words(line, width, &mut result);
    }
    result
}

/// Wrap a single line at word boundaries. Accumulate words until line exceeds
/// width (measured in display cells), break at last whitespace. Single words
/// exceeding width fall back to grapheme-boundary chunking.
fn wrap_line_words(line: &str, width: usize, result: &mut Vec<String>) {
    let words: Vec<&str> = line.split_whitespace().collect();
    if words.is_empty() {
        result.push(String::new());
        return;
    }

    let mut current_line = String::new();
    let mut current_cells = 0usize;

    for word in &words {
        let word_cells = geom::cells(word);

        if current_line.is_empty() {
            // First word on line
            if word_cells <= width {
                current_line.push_str(word);
                current_cells = word_cells;
            } else {
                // Single word exceeding width — grapheme-break
                char_wrap_word(word, width, result);
            }
        } else if current_cells + 1 + word_cells <= width {
            // Word fits with a space
            current_line.push(' ');
            current_line.push_str(word);
            current_cells += 1 + word_cells;
        } else {
            // Word doesn't fit — emit current line, start new one
            result.push(std::mem::take(&mut current_line));
            if word_cells <= width {
                current_line.push_str(word);
                current_cells = word_cells;
            } else {
                // Word exceeds full line — grapheme-break
                char_wrap_word(word, width, result);
            }
        }
    }

    if !current_line.is_empty() {
        result.push(current_line);
    }
}

/// Break a single word that exceeds the line width at grapheme boundaries. A
/// single grapheme wider than `width` stays one intact row (A3), so the loop
/// always makes progress.
fn char_wrap_word(word: &str, width: usize, result: &mut Vec<String>) {
    result.extend(geom::wrap_hard(word, width).into_iter().map(str::to_owned));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_word_wrap_exact_boundary() {
        // Word fills line exactly
        let result = wrap_text("hello world", 11);
        assert_eq!(result, vec!["hello world"]);
    }

    #[test]
    fn test_word_wrap_breaks_at_word() {
        let result = wrap_text("hello world foo", 11);
        assert_eq!(result, vec!["hello world", "foo"]);
    }

    #[test]
    fn test_word_wrap_long_single_word() {
        // Single word exceeding width → character fallback
        let result = wrap_text("abcdefghijklmno", 5);
        assert_eq!(result, vec!["abcde", "fghij", "klmno"]);
    }

    #[test]
    fn test_word_wrap_empty_string() {
        let result = wrap_text("", 10);
        assert!(result.is_empty());
    }

    #[test]
    fn test_word_wrap_single_char() {
        let result = wrap_text("a", 10);
        assert_eq!(result, vec!["a"]);
    }

    #[test]
    fn test_word_wrap_consecutive_whitespace() {
        // split_whitespace handles multiple spaces
        let result = wrap_text("hello   world", 20);
        assert_eq!(result, vec!["hello world"]);
    }

    #[test]
    fn test_word_wrap_newlines() {
        let result = wrap_text("line1\nline2\nline3", 20);
        assert_eq!(result, vec!["line1", "line2", "line3"]);
    }

    #[test]
    fn test_word_wrap_empty_line() {
        let result = wrap_text("hello\n\nworld", 20);
        assert_eq!(result, vec!["hello", "", "world"]);
    }

    #[test]
    fn test_word_wrap_zero_width() {
        let result = wrap_text("hello", 0);
        assert!(result.is_empty());
    }

    #[test]
    fn test_word_wrap_mixed_short_long() {
        let result = wrap_text("hi superlongword ok", 5);
        assert_eq!(result, vec!["hi", "super", "longw", "ord", "ok"]);
    }

    // Story 21.11 fixtures: G = 2 cells / 1 grapheme / 6 scalars, C = 2 cells,
    // A = 1 cell / 2 scalars.
    const G: &str = "👨\u{200d}👩\u{200d}👧";
    const C: &str = "界";
    const A: &str = "e\u{301}";

    fn assert_rows_bounded(rows: &[String], width: usize) {
        for row in rows {
            let cells = geom::cells(row);
            let single_grapheme =
                unicode_segmentation::UnicodeSegmentation::graphemes(row.as_str(), true).count()
                    == 1;
            assert!(
                cells <= width || single_grapheme,
                "row {row:?} is {cells} cells, budget {width}"
            );
        }
    }

    #[test]
    fn display_width_ascii_positive_control_chunks_by_cells() {
        assert_eq!(wrap_text("abcdef", 4), vec!["abcd", "ef"]);
        assert_eq!(wrap_text("abcd", 4), vec!["abcd"]);
    }

    #[test]
    fn display_width_cjk_fits_in_cells_not_scalars() {
        let word = C.repeat(3); // 6 cells
        // Fit-minus-one and exact fit at the 6-cell boundary.
        assert_eq!(wrap_text(&word, 6), vec![word.clone()]);
        assert_eq!(wrap_text(&word, 5), vec![C.repeat(2), C.to_string()]);
        // The ledger case: CCC at 4 cells needs two rows, not one.
        assert_eq!(wrap_text(&word, 4), vec![C.repeat(2), C.to_string()]);
        // A wide glyph is never split to fill an odd tail cell.
        assert_eq!(
            wrap_text(&word, 3),
            vec![C.to_string(), C.to_string(), C.to_string()]
        );
    }

    #[test]
    fn display_width_family_stays_whole_in_long_tokens() {
        let word = format!("{G}xx"); // 4 cells
        assert_eq!(wrap_text(&word, 4), vec![word.clone()]);
        assert_eq!(wrap_text(&word, 3), vec![format!("{G}x"), "x".to_string()]);
        assert_eq!(wrap_text(&word, 2), vec![G.to_string(), "xx".to_string()]);
    }

    #[test]
    fn display_width_combining_mark_stays_with_base() {
        let word = format!("{A}{A}"); // 2 cells, 4 scalars
        assert_eq!(wrap_text(&word, 2), vec![word.clone()]);
        assert_eq!(wrap_text(&word, 1), vec![A.to_string(), A.to_string()]);
    }

    #[test]
    fn display_width_oversized_grapheme_is_kept_intact_and_progresses() {
        let word = format!("{G}{G}{G}");
        let rows = wrap_text(&word, 1);
        assert_eq!(rows, vec![G.to_string(), G.to_string(), G.to_string()]);
        assert_rows_bounded(&rows, 1);
        assert_eq!(wrap_text(G, 1), vec![G.to_string()]);
    }

    #[test]
    fn display_width_words_join_by_cells_and_lose_no_suffix() {
        // Two 4-cell words plus a separator are 9 cells but only 5 scalars.
        let text = format!("{0}{0} {0}{0} tail", C);
        let rows = wrap_text(&text, 9);
        assert_eq!(rows, vec![format!("{0}{0} {0}{0}", C), "tail".to_string()]);
        let rows = wrap_text(&text, 8);
        assert_eq!(
            rows,
            vec![
                format!("{0}{0}", C),
                format!("{0}{0}", C),
                "tail".to_string()
            ]
        );
        // Current whitespace policy: reconstructed words are preserved.
        for width in 1..=10 {
            let rows = wrap_text(&text, width);
            assert_rows_bounded(&rows, width);
            let rebuilt: String = rows.concat().split_whitespace().collect();
            assert_eq!(rebuilt, text.split_whitespace().collect::<String>());
        }
    }

    #[test]
    fn display_width_mixed_repeated_token_reconstructs_source() {
        let token = format!("{C}{G}{A}{C}{G}{A}x{C}");
        for width in 1..=12 {
            let rows = wrap_text(&token, width);
            assert_rows_bounded(&rows, width);
            assert_eq!(
                rows.concat(),
                token,
                "width {width} dropped or duplicated text"
            );
        }
    }
}
