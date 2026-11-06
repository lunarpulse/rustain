//! Retained in-frame search shared by the Help and Notification Log inspectors.

use ratatui::prelude::*;

use super::text_field::TextField;
use crate::adapters::tui::theme::Theme;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InspectorSearch {
    pub query: TextField,
    pub editing: bool,
}

impl InspectorSearch {
    pub fn open(&mut self) {
        self.editing = true;
    }

    pub fn accept(&mut self) {
        self.editing = false;
    }

    pub fn back(&mut self) {
        self.query.clear();
        self.editing = false;
    }

    /// Unicode lowercase substring matching, including ordinary/final sigma
    /// equivalence, without normalization, mark removal or folded allocations.
    pub fn matches(&self, text: &str) -> bool {
        let mut query = caseless_chars(self.query.as_str());
        let Some(first) = query.next() else {
            return true;
        };
        let mut starts = caseless_chars(text);
        while let Some(ch) = starts.next() {
            if ch == first {
                let mut candidate = starts.clone();
                if query.clone().all(|ch| candidate.next() == Some(ch)) {
                    return true;
                }
            }
        }
        false
    }

    /// Borrow the actual editor viewport; the compositor grants caret ownership.
    pub fn query_line<'a>(&'a self, width: usize, theme: &Theme, show_caret: bool) -> Line<'a> {
        let view = self.query.view(width.saturating_sub(2));
        let normal = Style::default().fg(theme.colors.fg_primary);
        Line::from(vec![
            Span::styled(
                crate::adapters::tui::text_geometry::clip("/ ", width),
                Style::default().fg(theme.colors.accent),
            ),
            Span::styled(view.before, normal),
            Span::styled(
                view.caret,
                if show_caret && self.editing {
                    normal.add_modifier(Modifier::REVERSED)
                } else {
                    normal
                },
            ),
            Span::styled(view.after, normal),
        ])
    }
}

pub(crate) fn caseless_char(ch: char) -> impl Iterator<Item = char> + Clone {
    ch.to_lowercase().map(|ch| if ch == 'ς' { 'σ' } else { ch })
}

pub(crate) fn caseless_chars(text: &str) -> impl Iterator<Item = char> + Clone {
    text.chars().flat_map(caseless_char)
}

/// KMP over the same caseless stream as the shared inspector predicate. Only
/// the query and a query-sized ring of source grapheme boundaries are retained.
pub(crate) struct CaselessMatcher {
    needle: Vec<char>,
    failure: Vec<usize>,
    boundaries: std::collections::VecDeque<std::ops::Range<usize>>,
}

impl CaselessMatcher {
    pub(crate) fn new(query: &str) -> Self {
        let needle: Vec<_> = caseless_chars(query).collect();
        let mut failure = vec![0; needle.len()];
        let mut matched = 0;
        for at in 1..needle.len() {
            while matched > 0 && needle[at] != needle[matched] {
                matched = failure[matched - 1];
            }
            if needle[at] == needle[matched] {
                matched += 1;
            }
            failure[at] = matched;
        }
        let boundaries = std::collections::VecDeque::with_capacity(needle.len());
        Self {
            needle,
            failure,
            boundaries,
        }
    }

    pub(crate) fn ranges(&mut self, text: &str, mut found: impl FnMut(std::ops::Range<usize>)) {
        use unicode_segmentation::UnicodeSegmentation;
        if self.needle.is_empty() {
            return;
        }
        self.boundaries.clear();
        let mut matched = 0;
        let mut previous = None;
        for (at, grapheme) in text.grapheme_indices(true) {
            for ch in grapheme.chars().flat_map(caseless_char) {
                if self.boundaries.len() == self.needle.len() {
                    self.boundaries.pop_front();
                }
                self.boundaries.push_back(at..at + grapheme.len());
                while matched > 0 && ch != self.needle[matched] {
                    matched = self.failure[matched - 1];
                }
                if ch == self.needle[matched] {
                    matched += 1;
                }
                if matched == self.needle.len() {
                    let bytes =
                        self.boundaries.front().expect("nonempty query").start..at + grapheme.len();
                    if previous.as_ref() != Some(&bytes) {
                        previous = Some(bytes.clone());
                        found(bytes);
                    }
                    matched = self.failure[matched - 1];
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::tui::keymap::table_input::FieldCmd;

    #[test]
    fn matching_is_case_insensitive_substring_with_independent_negatives() {
        let mut search = InspectorSearch::default();
        assert!(search.matches(""));
        search.query.set("Érr");
        assert!(search.matches("an ÉRRor occurred"));
        assert!(!search.matches("an error occurred"));
        assert!(!search.matches("Ér"));
        search.query.set("界e\u{301}");
        assert!(search.matches("before界e\u{301}after"));
        assert!(!search.matches("before界éafter"));
    }

    #[test]
    fn casefold_expansion_preserves_substring_boundaries() {
        let mut search = InspectorSearch::default();
        search.query.set("\u{307}");
        assert!(
            search.matches("İ"),
            "lowered İ contains the combining-dot substring"
        );
        assert!(
            !search.matches("i"),
            "ASCII i is an independent absent-substring control"
        );
        search.query.set("i\u{307}");
        assert!(search.matches("beforeİafter"));
    }

    #[test]
    fn sigma_word_forms_are_caseless_without_normalizing_or_removing_marks() {
        let mut search = InspectorSearch::default();
        for (query, text) in [("ος", "ΟΣ"), ("ΟΣ", "ος"), ("σ", "ς"), ("ς", "σ")] {
            search.query.set(query);
            assert!(
                search.matches(&format!("before{text}after")),
                "{query:?} in {text:?}"
            );
            assert!(
                !search.matches("unrelated οτ"),
                "independent absent-word control"
            );
        }
        search.query.set("Σ\u{301}");
        assert!(search.matches("ς\u{301}"));
        assert!(!search.matches("ς"), "combining accent is not removed");
        search.query.set("όσ");
        assert!(
            !search.matches("ος"),
            "accented and unaccented letters remain distinct"
        );
        search.query.set("e\u{301}");
        assert!(!search.matches("é"), "no NFC/NFD normalization");
    }

    #[test]
    fn editor_lifecycle_retains_field_and_caret_until_back() {
        let mut search = InspectorSearch::default();
        search.open();
        search.query.insert("界e\u{301}x");
        search.query.edit(FieldCmd::CursorLeft);
        let retained = search.query.clone();
        search.accept();
        assert!(!search.editing);
        search.open();
        assert_eq!(search.query, retained);
        search.back();
        assert_eq!(search, InspectorSearch::default());
    }

    #[test]
    fn query_line_borrows_field_and_only_styles_the_active_editor_caret() {
        let mut search = InspectorSearch::default();
        search.query.set("界e\u{301}x");
        search.query.edit(FieldCmd::CursorLeft);
        search.open();
        let theme = Theme::dark();
        let line = search.query_line(20, &theme, true);
        assert_eq!(
            line.spans[1].content.as_ptr(),
            search.query.as_str().as_ptr()
        );
        assert_eq!(line.spans[2].content, "x");
        assert!(
            line.spans[2]
                .style
                .add_modifier
                .contains(Modifier::REVERSED)
        );
        assert!(
            !search.query_line(20, &theme, false).spans[2]
                .style
                .add_modifier
                .contains(Modifier::REVERSED)
        );
        search.accept();
        assert!(
            !search.query_line(20, &theme, true).spans[2]
                .style
                .add_modifier
                .contains(Modifier::REVERSED)
        );
        for width in 0..20 {
            assert!(search.query_line(width, &theme, false).width() <= width);
        }
    }
}
