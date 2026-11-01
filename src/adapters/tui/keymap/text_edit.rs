//! TextEntry editing (Story 21.3 AC11, A4): the readline motions and deletions
//! every text field shares. Pure functions over `(text, cursor)`; the cursor is a
//! scalar index and every step lands on a grapheme boundary (21-11
//! `text_geometry`), so a ZWJ sequence, a CJK character or a combining mark is
//! never split. Logical lines are `\n`-separated (unbounded width).

use unicode_segmentation::UnicodeSegmentation;

use crate::adapters::tui::text_geometry::{byte_of_scalar, next_boundary, prev_boundary};

/// Remove the scalars `from..to` and return the new cursor (`from`).
fn remove(text: &mut String, from: usize, to: usize) -> usize {
    let (start, end) = (byte_of_scalar(text, from), byte_of_scalar(text, to));
    text.replace_range(start..end, "");
    from
}

/// Scalar index of the start of the logical line holding `cursor`.
pub fn line_start(text: &str, cursor: usize) -> usize {
    let before = &text[..byte_of_scalar(text, cursor)];
    before
        .rfind('\n')
        .map_or(0, |byte| before[..=byte].chars().count())
}

/// Scalar index of the end of the logical line holding `cursor` (before its `\n`).
pub fn line_end(text: &str, cursor: usize) -> usize {
    let rest = &text[byte_of_scalar(text, cursor)..];
    cursor
        + rest
            .find('\n')
            .map_or_else(|| rest.chars().count(), |byte| rest[..byte].chars().count())
}

/// `Ctrl+H` / `Backspace`: delete the grapheme before the cursor.
pub fn delete_back(text: &mut String, cursor: usize) -> usize {
    if cursor == 0 {
        return 0;
    }
    let from = prev_boundary(text, cursor);
    let to = next_boundary(text, from).max(cursor);
    remove(text, from, to)
}

/// `Ctrl+W`: delete the whitespace-delimited word before the cursor (and the
/// whitespace between it and the cursor): ONE reverse grapheme scan, so a pasted
/// blob with no space stays linear.
pub fn kill_word_back(text: &mut String, cursor: usize) -> usize {
    let end = byte_of_scalar(text, cursor);
    let (mut from, mut in_word) = (end, false);
    for (at, grapheme) in text[..end].grapheme_indices(true).rev() {
        let blank = grapheme.chars().all(char::is_whitespace);
        if blank && in_word {
            break;
        }
        in_word |= !blank;
        from = at;
    }
    let start = text[..from].chars().count();
    text.replace_range(from..end, "");
    start
}

/// `Ctrl+U`: delete from the start of the logical line to the cursor.
pub fn kill_to_line_start(text: &mut String, cursor: usize) -> usize {
    remove(text, line_start(text, cursor), cursor)
}

/// `Ctrl+K`: delete from the cursor to the end of the logical line.
pub fn kill_to_line_end(text: &mut String, cursor: usize) -> usize {
    remove(text, cursor, line_end(text, cursor));
    cursor
}
