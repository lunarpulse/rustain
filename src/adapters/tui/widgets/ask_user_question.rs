//! AskUserQuestion body (Story 21.6 6-C): the question as rows of the `Decision:
//! Question` frame. The frame owns the border, the answer field (`Answer: <text>`), the
//! rail and the scrolling; this builder only wraps the question text.

use ratatui::prelude::*;

use super::plan_card::wrap_description;
use crate::adapters::tui::theme::Theme;

/// The question wrapped to `width` cells, one styled row per wrapped line. A long
/// unbroken word is hard-wrapped on grapheme boundaries so the whole question is always
/// shown; an empty question still yields one blank row. The caller sanitizes what a model
/// wrote before it gets here; the copy path passes the original bytes.
pub fn question_lines(question: &str, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    let style = Style::default().fg(theme.colors.accent);
    let mut rows = wrap_description(question, width.max(1));
    if rows.is_empty() {
        rows.push(String::new());
    }
    rows.into_iter()
        .map(|row| Line::from(Span::styled(row, style)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(lines: &[Line<'_>]) -> Vec<String> {
        lines
            .iter()
            .map(|line| line.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    #[test]
    fn story21_6_the_question_wraps_and_an_empty_one_keeps_a_row() {
        let theme = Theme::dark();
        let rows = text(&question_lines(
            "What is the name of your project?",
            12,
            &theme,
        ));
        assert!(rows.len() > 1, "{rows:?}");
        assert_eq!(rows.join(" "), "What is the name of your project?");
        assert_eq!(text(&question_lines("", 20, &theme)), [String::new()]);
    }

    #[test]
    fn story21_6_an_unbroken_word_is_hard_wrapped_never_cut() {
        let theme = Theme::dark();
        let rows = text(&question_lines(&"x".repeat(25), 10, &theme));
        assert_eq!(rows.concat(), "x".repeat(25));
        assert!(rows.iter().all(|row| row.chars().count() <= 10));
    }
}
