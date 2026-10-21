//! The delimited body region shared by host-owned decision cards (TM-09).
//!
//! Story 19.20c extracted [`wrap_text`] from the FR54-b proposal card so the
//! approval card and Review wrap untrusted text the same way: by terminal
//! cells, never splitting a grapheme, never normalizing whitespace. The caller
//! prefixes every returned physical row with the host delimiter.

use ratatui::prelude::*;
use unicode_width::UnicodeWidthStr as _;

use crate::domain::services::approval_presentation::PresentedLine;

/// Wrap without normalizing source whitespace and without splitting graphemes.
/// The caller may bound the number of rows, so large bodies stay cheap.
/// Returns the rows and whether anything past `limit` was not returned.
pub(crate) fn wrap_text(text: &str, width: usize, limit: usize) -> (Vec<String>, bool) {
    let mut rows = Vec::new();
    let mut row = String::new();
    let mut cells = 0usize;
    let mut pieces = text.split_inclusive(['\n', '\t']).peekable();
    while let Some(piece) = pieces.next() {
        // Span filters controls; reintroduce only the two permitted controls.
        let span = Span::raw(piece);
        let mut graphemes = span
            .styled_graphemes(Style::default())
            .map(|grapheme| grapheme.symbol)
            .chain(piece.ends_with('\t').then_some("\t"))
            .chain(piece.ends_with('\n').then_some("\n"))
            .peekable();
        while let Some(grapheme) = graphemes.next() {
            if grapheme == "\n" {
                rows.push(std::mem::take(&mut row));
                cells = 0;
                if rows.len() == limit {
                    return (
                        rows,
                        graphemes.peek().is_some()
                            || pieces.peek().is_some()
                            || text.ends_with('\n'),
                    );
                }
                continue;
            }
            if grapheme == "\t" {
                for _ in 0..8 - cells % 8 {
                    if cells >= width {
                        rows.push(std::mem::take(&mut row));
                        cells = 0;
                        if rows.len() == limit {
                            return (rows, true);
                        }
                    }
                    row.push(' ');
                    cells += 1;
                }
                continue;
            }
            let w = grapheme.width();
            if cells > 0 && cells.saturating_add(w) > width {
                rows.push(std::mem::take(&mut row));
                cells = 0;
                if rows.len() == limit {
                    return (rows, true);
                }
            }
            row.push_str(grapheme);
            cells = cells.saturating_add(w);
        }
    }
    if rows.len() < limit && (!row.is_empty() || rows.is_empty() || text.ends_with('\n')) {
        rows.push(row);
    }
    (rows, false)
}

/// One physical body row of an approval input. `label_len` is the byte
/// length of the field-label prefix (`<field>: `) inside `text`, so a
/// renderer can style the host-minted label apart from the value text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BodyRow {
    pub text: String,
    pub label_len: usize,
}

/// Wrap presented logical lines into at most `limit` physical rows of
/// `width` cells (the delimiter excluded). Returns whether more rows exist.
#[must_use]
pub fn approval_body_rows(
    lines: &[PresentedLine],
    width: usize,
    limit: usize,
) -> (Vec<BodyRow>, bool) {
    let width = width.max(1);
    let mut rows = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        if rows.len() == limit {
            return (rows, true);
        }
        let joined = line.joined();
        let (wrapped, more) = wrap_text(&joined, width, limit - rows.len());
        let mut label_left = line.label_prefix_len();
        for text in wrapped {
            let label_len = label_left.min(text.len());
            label_left -= label_len;
            rows.push(BodyRow { text, label_len });
        }
        if more || (rows.len() == limit && index + 1 < lines.len()) {
            return (rows, true);
        }
    }
    (rows, false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::services::approval_presentation::body_lines;

    #[test]
    fn wrap_preserves_whitespace_and_graphemes() {
        let (rows, more) = wrap_text("  e\u{301}界\t\n", 3, 8);
        assert!(!more);
        assert_eq!(rows.concat(), "  e\u{301}界      ");
        assert_eq!(rows.last().map(String::as_str), Some(""));
    }

    #[test]
    fn body_rows_keep_every_cell_and_report_more() {
        let input = serde_json::json!({ "command": format!("{}\nTAIL", "a".repeat(25)) });
        let lines = body_lines("Bash", &input);
        let (all, more) = approval_body_rows(&lines, 10, usize::MAX);
        assert!(!more);
        let text: String = all.iter().map(|row| row.text.as_str()).collect();
        assert_eq!(text, format!("command: {}TAIL", "a".repeat(25)));
        assert_eq!(all[0].label_len, "command: ".len());
        assert_eq!(all.last().unwrap().text, "TAIL");
        let (card, more) = approval_body_rows(&lines, 10, 2);
        assert!(more);
        assert_eq!(card.len(), 2);
        let (exact, more) = approval_body_rows(&lines, 80, 2);
        assert_eq!(exact.len(), 2);
        assert!(!more, "exactly two rows fit two");
    }

    #[test]
    fn a_label_wider_than_the_row_wraps_without_loss() {
        let input = serde_json::json!({ "an_extremely_long_field_name": "v" });
        let lines = body_lines("Custom", &input);
        let (rows, _) = approval_body_rows(&lines, 8, usize::MAX);
        let text: String = rows.iter().map(|row| row.text.as_str()).collect();
        assert_eq!(text, "an_extremely_long_field_name: v");
        assert_eq!(rows[0].label_len, 8);
        let label_total: usize = rows.iter().map(|row| row.label_len).sum();
        assert_eq!(label_total, "an_extremely_long_field_name: ".len());
    }
}
