//! Owned text and a scalar-indexed, grapheme-boundary caret for modal fields.
//!
//! Edits preserve pasted text verbatim. `edit` reports content changes only;
//! callers must redraw cursor motions independently. Views borrow the current
//! logical line, reserving a visible caret before clipping the surrounding text.

use crate::adapters::tui::keymap::{table_input::FieldCmd, text_edit};
use crate::adapters::tui::text_geometry::{
    byte_of_scalar, cells, clip, clip_tail, next_boundary, prev_boundary, snap_boundary,
};

#[derive(Default, Debug, Clone, PartialEq, Eq)]
pub struct TextField {
    text: String,
    cursor: usize,
}

/// Whole-grapheme slices fitting one row's display-cell budget.
///
/// `caret` is the grapheme at the cursor, or a space at end of line. A grapheme
/// that cannot fit (or has no visible cells) also uses a space, never a partial
/// glyph. The space is static; the other slices borrow the field's storage.
#[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
pub struct FieldView<'a> {
    pub before: &'a str,
    pub caret: &'a str,
    pub after: &'a str,
}

impl From<String> for TextField {
    fn from(text: String) -> Self {
        let cursor = text.chars().count();
        Self { text, cursor }
    }
}

impl From<&str> for TextField {
    fn from(text: &str) -> Self {
        Self::from(text.to_owned())
    }
}

impl TextField {
    pub fn as_str(&self) -> &str {
        &self.text
    }

    /// Scalar index, always at a boundary of an extended grapheme cluster.
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// Reset the entire buffer, unlike the line-local `FieldCmd::Clear`.
    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
    }

    /// Replace the buffer and put the caret at the end, retaining capacity.
    pub fn set(&mut self, text: &str) {
        self.text.clear();
        self.text.push_str(text);
        self.cursor = text.chars().count();
    }

    /// Insert verbatim at the caret, advancing past any newly joined cluster.
    pub fn insert(&mut self, text: &str) -> bool {
        if text.is_empty() {
            return false;
        }
        let at = byte_of_scalar(&self.text, self.cursor);
        self.text.insert_str(at, text);
        self.cursor += text.chars().count();
        // Inserting a ZWJ or a base before combining marks can join the text
        // on both sides. Keep the caret after the resulting whole grapheme.
        if snap_boundary(&self.text, self.cursor) != self.cursor {
            self.cursor = next_boundary(&self.text, self.cursor);
        }
        true
    }

    /// Encode on the stack; only growing the owned buffer may allocate.
    pub fn insert_char(&mut self, ch: char) -> bool {
        let mut bytes = [0; 4];
        self.insert(ch.encode_utf8(&mut bytes))
    }

    /// Return whether content changed, not whether the caret moved.
    pub fn edit(&mut self, command: FieldCmd) -> bool {
        let old_len = self.text.len();
        self.cursor = match command {
            FieldCmd::DeleteWordBack => text_edit::kill_word_back(&mut self.text, self.cursor),
            FieldCmd::Clear => text_edit::kill_to_line_start(&mut self.text, self.cursor),
            FieldCmd::DeleteBack => text_edit::delete_back(&mut self.text, self.cursor),
            FieldCmd::KillToLineEnd => {
                // The logical end before LF can lie inside a CRLF grapheme.
                // Snap the endpoint before deleting, not just the resulting caret.
                let end = snap_boundary(&self.text, text_edit::line_end(&self.text, self.cursor));
                if end == self.cursor {
                    return false;
                }
                let from = byte_of_scalar(&self.text, self.cursor);
                let to = byte_of_scalar(&self.text, end);
                self.text.replace_range(from..to, "");
                self.cursor
            }
            FieldCmd::DeleteForward => {
                let next = next_boundary(&self.text, self.cursor);
                if next == self.cursor {
                    return false;
                }
                text_edit::delete_back(&mut self.text, next)
            }
            FieldCmd::LineStart => {
                self.cursor = text_edit::line_start(&self.text, self.cursor);
                return false;
            }
            FieldCmd::LineEnd => {
                // CRLF is one grapheme; the helper's position before LF must
                // snap back before the entire line separator.
                self.cursor =
                    snap_boundary(&self.text, text_edit::line_end(&self.text, self.cursor));
                return false;
            }
            FieldCmd::CursorLeft => {
                self.cursor = prev_boundary(&self.text, self.cursor);
                return false;
            }
            FieldCmd::CursorRight => {
                self.cursor = next_boundary(&self.text, self.cursor);
                return false;
            }
            FieldCmd::NoCursor => return false,
        };
        // Removing a separator can join the surviving neighbors (for example,
        // two regional indicators). The old deletion boundary may disappear.
        self.cursor = snap_boundary(&self.text, self.cursor);
        self.text.len() != old_len
    }

    /// Clip the current logical line around a visible, whole-grapheme caret.
    /// Left context has priority; any remaining cells show right context.
    /// A zero-width row returns three empty slices without allocating.
    pub fn view(&self, width: usize) -> FieldView<'_> {
        self.view_with_gap(width, 0)
    }

    /// Leave a gap before a trailing suffix within `width` cells.
    /// The EOL caret's space supplies one gap cell; callers pad the remainder.
    pub fn view_with_gap(&self, width: usize, gap: usize) -> FieldView<'_> {
        if width == 0 {
            return FieldView::default();
        }
        let start = byte_of_scalar(&self.text, text_edit::line_start(&self.text, self.cursor));
        let end = byte_of_scalar(
            &self.text,
            snap_boundary(&self.text, text_edit::line_end(&self.text, self.cursor)),
        );
        let at = byte_of_scalar(&self.text, self.cursor);
        let width = width.saturating_sub(gap.saturating_sub(usize::from(at == end)));
        if width == 0 {
            return FieldView::default();
        }
        let next = byte_of_scalar(&self.text, next_boundary(&self.text, self.cursor)).min(end);
        let current = &self.text[at..next];
        let current_cells = cells(current);
        let caret = if current_cells == 0 || current_cells > width {
            " "
        } else {
            current
        };
        let remaining = width - cells(caret);
        let before = clip_tail(&self.text[start..at], remaining);
        let after = clip(&self.text[next..end], remaining - cells(before));
        FieldView {
            before,
            caret,
            after,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FAMILY: &str = "👩\u{200d}👩\u{200d}👧\u{200d}👦";
    const ACCENT: &str = "e\u{301}";

    fn assert_field(field: &TextField, text: &str, cursor: usize) {
        assert_eq!(field.as_str(), text);
        assert_eq!(field.cursor(), cursor);
        assert_eq!(snap_boundary(field.as_str(), cursor), cursor);
    }

    fn assert_view(field: &TextField, width: usize, expected: (&str, &str, &str)) {
        let view = field.view(width);
        assert_eq!((view.before, view.caret, view.after), expected);
        assert!(cells(view.before) + cells(view.caret) + cells(view.after) <= width);
    }

    #[test]
    fn constructors_move_storage_and_count_scalars() {
        let owned = String::from("界e\u{301}");
        let ptr = owned.as_ptr();
        let field = TextField::from(owned);
        assert_eq!(field.as_str().as_ptr(), ptr);
        assert_field(&field, "界e\u{301}", 3);
        assert_eq!(field, field.clone());
        assert!(!field.is_empty());
        assert_eq!(TextField::from(""), TextField::default());
    }

    #[test]
    fn set_and_clear_reset_the_whole_buffer_and_caret() {
        let mut field = TextField::from("old\nline");
        assert!(!field.edit(FieldCmd::LineStart));
        field.set("界e\u{301}");
        assert_field(&field, "界e\u{301}", 3);
        field.clear();
        assert_field(&field, "", 0);
        assert!(field.is_empty());
        field.clear();
        field.set("");
        assert_eq!(field, TextField::default());
    }

    #[test]
    fn insert_and_insert_char_use_the_actual_caret() {
        let mut field = TextField::from("a界z");
        assert!(!field.edit(FieldCmd::CursorLeft));
        assert!(field.insert("XY"));
        assert_field(&field, "a界XYz", 4);
        assert!(field.insert_char('é'));
        assert_field(&field, "a界XYéz", 5);
        let unchanged = field.clone();
        assert!(!field.insert(""));
        assert_eq!(field, unchanged);
        assert!(field.insert_char('🦀'));
        assert_field(&field, "a界XYé🦀z", 6);
    }

    #[test]
    fn zwj_insertion_merges_existing_clusters_and_advances_past_join() {
        let mut field = TextField::from("👩👧!");
        field.edit(FieldCmd::LineStart);
        field.edit(FieldCmd::CursorRight);
        assert_field(&field, "👩👧!", 1);
        assert!(field.insert("\u{200d}"));
        assert_field(&field, "👩\u{200d}👧!", 3);
        assert!(!field.edit(FieldCmd::CursorLeft));
        assert_field(&field, "👩\u{200d}👧!", 0);
        assert!(!field.edit(FieldCmd::CursorRight));
        assert_field(&field, "👩\u{200d}👧!", 3);
        assert!(field.edit(FieldCmd::DeleteBack));
        assert_field(&field, "!", 0);
    }

    #[test]
    fn combining_insertions_preserve_legal_caret_on_both_sides() {
        let mut field = TextField::from("ex");
        field.edit(FieldCmd::CursorLeft);
        assert!(field.insert_char('\u{301}'));
        assert_field(&field, "e\u{301}x", 2);
        assert!(field.edit(FieldCmd::DeleteBack));
        assert_field(&field, "x", 0);

        field.set("\u{301}x");
        field.edit(FieldCmd::LineStart);
        assert!(field.insert("e"));
        assert_field(&field, "e\u{301}x", 2);
    }

    #[test]
    fn left_right_roundtrip_visits_only_whole_graphemes() {
        let text = format!("a界{FAMILY}{ACCENT}");
        let mut field = TextField::from(text.as_str());
        for cursor in [9, 2, 1, 0, 0] {
            assert!(!field.edit(FieldCmd::CursorLeft));
            assert_field(&field, &text, cursor);
        }
        for cursor in [1, 2, 9, 11, 11] {
            assert!(!field.edit(FieldCmd::CursorRight));
            assert_field(&field, &text, cursor);
        }
    }

    #[test]
    fn back_and_forward_delete_one_whole_grapheme() {
        let mut field = TextField::from(format!("a{FAMILY}界{ACCENT}z"));
        field.edit(FieldCmd::LineStart);
        field.edit(FieldCmd::CursorRight);
        assert!(field.edit(FieldCmd::DeleteForward));
        assert_field(&field, "a界e\u{301}z", 1);
        field.edit(FieldCmd::CursorRight);
        assert!(field.edit(FieldCmd::DeleteBack));
        assert_field(&field, "ae\u{301}z", 1);
        assert!(field.edit(FieldCmd::DeleteForward));
        assert_field(&field, "az", 1);
    }

    #[test]
    fn deletions_snap_when_surviving_neighbors_join() {
        for command in [FieldCmd::DeleteBack, FieldCmd::DeleteForward] {
            let mut field = TextField::from("🇦x🇧");
            field.edit(FieldCmd::LineStart);
            field.edit(FieldCmd::CursorRight);
            if command == FieldCmd::DeleteBack {
                field.edit(FieldCmd::CursorRight);
            }
            assert!(field.edit(command));
            assert_field(&field, "🇦🇧", 0);
            field.edit(FieldCmd::CursorRight);
            assert_field(&field, "🇦🇧", 2);
        }
    }

    #[test]
    fn word_delete_uses_whitespace_delimited_grapheme_words() {
        let mut field = TextField::from(format!("keep 界{FAMILY}{ACCENT}  tail"));
        for _ in 0..4 {
            field.edit(FieldCmd::CursorLeft);
        }
        assert!(field.edit(FieldCmd::DeleteWordBack));
        assert_field(&field, "keep tail", 5);
        assert!(field.edit(FieldCmd::DeleteWordBack));
        assert_field(&field, "tail", 0);
    }

    #[test]
    fn logical_line_motions_and_kills_preserve_other_lines() {
        let mut field = TextField::from("first\na界e\u{301}z\nlast");
        field.edit(FieldCmd::LineStart);
        field.edit(FieldCmd::CursorLeft);
        assert!(!field.edit(FieldCmd::LineStart));
        assert_field(&field, "first\na界e\u{301}z\nlast", 6);
        assert!(!field.edit(FieldCmd::LineEnd));
        assert_field(&field, "first\na界e\u{301}z\nlast", 11);
        field.edit(FieldCmd::CursorLeft);
        assert!(field.edit(FieldCmd::Clear));
        assert_field(&field, "first\nz\nlast", 6);
        assert!(field.edit(FieldCmd::KillToLineEnd));
        assert_field(&field, "first\n\nlast", 6);
        assert!(!field.edit(FieldCmd::KillToLineEnd));
        assert_field(&field, "first\n\nlast", 6);
    }

    #[test]
    fn boundary_edits_and_no_cursor_are_unchanged() {
        for text in ["", "界e\u{301}"] {
            let mut field = TextField::from(text);
            let at_end = field.clone();
            for command in [
                FieldCmd::DeleteForward,
                FieldCmd::KillToLineEnd,
                FieldCmd::CursorRight,
                FieldCmd::LineEnd,
                FieldCmd::NoCursor,
            ] {
                assert!(!field.edit(command));
                assert_eq!(field, at_end);
            }
            field.edit(FieldCmd::LineStart);
            let at_start = field.clone();
            for command in [
                FieldCmd::DeleteBack,
                FieldCmd::DeleteWordBack,
                FieldCmd::Clear,
                FieldCmd::CursorLeft,
                FieldCmd::LineStart,
                FieldCmd::NoCursor,
            ] {
                assert!(!field.edit(command));
                assert_eq!(field, at_start);
            }
        }
    }

    #[test]
    fn views_reserve_eol_caret_and_scroll_left_context() {
        let mut field = TextField::from("abcdef");
        assert_view(&field, 0, ("", "", ""));
        assert_view(&field, 1, ("", " ", ""));
        assert_view(&field, 4, ("def", " ", ""));
        assert_view(&field, 7, ("abcdef", " ", ""));
        field.edit(FieldCmd::LineStart);
        assert_view(&field, 4, ("", "a", "bcd"));
        field.edit(FieldCmd::CursorRight);
        field.edit(FieldCmd::CursorRight);
        assert_view(&field, 4, ("ab", "c", "d"));
        assert_view(&field, 2, ("b", "c", ""));
    }

    #[test]
    fn views_keep_cjk_zwj_and_combining_clusters_whole() {
        let mut field = TextField::from(format!("a界{FAMILY}{ACCENT}z"));
        field.edit(FieldCmd::LineStart);
        field.edit(FieldCmd::CursorRight);
        assert_view(&field, 1, ("", " ", ""));
        assert_view(&field, 4, ("a", "界", ""));
        assert_view(&field, 5, ("a", "界", FAMILY));
        field.edit(FieldCmd::CursorRight);
        assert_view(&field, 1, ("", " ", ""));
        assert_view(&field, 4, ("界", FAMILY, ""));
        field.edit(FieldCmd::CursorRight);
        assert_view(&field, 2, ("", ACCENT, "z"));
        assert_view(&field, 4, (FAMILY, ACCENT, "z"));
    }

    #[test]
    fn views_borrow_source_slices_and_do_not_sanitize_paste() {
        let mut field = TextField::default();
        assert!(field.insert("head\na\t\u{301}界\ntail"));
        assert_field(&field, "head\na\t\u{301}界\ntail", 14);
        field.edit(FieldCmd::LineStart);
        field.edit(FieldCmd::CursorLeft);
        field.edit(FieldCmd::LineStart);
        let view = field.view(20);
        assert_eq!(
            (view.before, view.caret, view.after),
            ("", "a", "\t\u{301}界")
        );
        assert_eq!(view.caret.as_ptr(), field.as_str()[5..].as_ptr());
        assert_eq!(view.after.as_ptr(), field.as_str()[6..].as_ptr());
        field.edit(FieldCmd::LineEnd);
        assert_view(&field, 20, ("a\t\u{301}界", " ", ""));
        field.edit(FieldCmd::CursorRight);
        assert_view(&field, 5, ("", "t", "ail"));
    }

    #[test]
    fn empty_and_zero_cell_carets_still_have_a_visible_space() {
        assert_view(&TextField::default(), 1, ("", " ", ""));
        let mut field = TextField::from("\u{301}a");
        field.edit(FieldCmd::LineStart);
        assert_view(&field, 1, ("", " ", ""));
        assert_view(&field, 2, ("", " ", "a"));
        assert_field(&field, "\u{301}a", 0);
    }

    #[test]
    fn crlf_line_end_and_views_do_not_split_the_separator() {
        let mut field = TextField::from("a\r\nb");
        field.edit(FieldCmd::LineStart);
        field.edit(FieldCmd::CursorLeft);
        assert_field(&field, "a\r\nb", 1);
        assert!(!field.edit(FieldCmd::LineEnd));
        assert_field(&field, "a\r\nb", 1);
        assert_view(&field, 4, ("a", " ", ""));
        field.edit(FieldCmd::CursorRight);
        assert_field(&field, "a\r\nb", 3);
        assert_view(&field, 4, ("", "b", ""));
    }

    #[test]
    fn crlf_kill_to_line_end_preserves_the_whole_separator() {
        for (cursor, expected, changed) in [(1, "a\r\nb", false), (0, "\r\nb", true)] {
            let mut field = TextField::from("a\r\nb");
            field.cursor = cursor;
            assert_eq!(field.edit(FieldCmd::KillToLineEnd), changed);
            assert_field(&field, expected, cursor);
        }
    }

    #[test]
    fn crlf_neighbor_deletions_keep_or_remove_the_complete_grapheme() {
        for (command, cursor, expected, after) in [
            (FieldCmd::Clear, 1, "\r\nb", 0),
            (FieldCmd::Clear, 3, "a\r\nb", 3),
            (FieldCmd::Clear, 4, "a\r\n", 3),
            (FieldCmd::DeleteWordBack, 1, "\r\nb", 0),
            (FieldCmd::DeleteWordBack, 3, "b", 0),
            (FieldCmd::DeleteWordBack, 4, "a\r\n", 3),
            (FieldCmd::DeleteBack, 3, "ab", 1),
            (FieldCmd::DeleteBack, 1, "\r\nb", 0),
            (FieldCmd::DeleteForward, 1, "ab", 1),
            (FieldCmd::DeleteForward, 0, "\r\nb", 0),
            (FieldCmd::DeleteForward, 4, "a\r\nb", 4),
            (FieldCmd::KillToLineEnd, 3, "a\r\n", 3),
            (FieldCmd::KillToLineEnd, 4, "a\r\nb", 4),
        ] {
            let mut field = TextField::from("a\r\nb");
            field.cursor = cursor;
            assert_eq!(field.edit(command), expected != "a\r\nb", "{command:?}");
            assert_field(&field, expected, after);
        }
    }

    #[test]
    fn suffix_gap_reuses_the_eol_caret_without_clipping_an_exact_fit() {
        let mut field = TextField::from("a界");
        let view = field.view_with_gap(4, 1);
        assert_eq!((view.before, view.caret, view.after), ("a界", " ", ""));
        field.edit(FieldCmd::LineStart);
        field.edit(FieldCmd::CursorRight);
        let view = field.view_with_gap(4, 1);
        assert_eq!((view.before, view.caret, view.after), ("a", "界", ""));
        for width in 0usize..=3 {
            let view = field.view_with_gap(width, 1);
            assert!(
                cells(view.before) + cells(view.caret) + cells(view.after)
                    <= width.saturating_sub(1)
            );
        }
        assert_field(&field, "a界", 1);
        field.edit(FieldCmd::LineEnd);
        let view = field.view_with_gap(1, 1);
        assert_eq!((view.before, view.caret, view.after), ("", " ", ""));
        let view = field.view_with_gap(4, 2);
        assert_eq!((view.before, view.caret, view.after), ("界", " ", ""));
    }
}
