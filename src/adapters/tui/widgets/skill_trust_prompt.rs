//! Skill trust body (Story 5-2 AC4 workspace-tier skill activation trust gate; the rows
//! of the `Decision: Skill trust` frame since Story 21.6 6-C).
//!
//! The frame owns the border, the rail and the scrolling: these builders only produce
//! the prompt's rows and the whole skill file for the Inspect sub-context (no row cap:
//! the body scrolls).

use ratatui::prelude::*;

use crate::adapters::tui::text_geometry as geom;
use crate::adapters::tui::theme::Theme;

fn name_span(skill_name: &str, theme: &Theme) -> Span<'static> {
    Span::styled(
        format!("\"{skill_name}\""),
        Style::default()
            .fg(theme.colors.accent)
            .add_modifier(Modifier::BOLD),
    )
}

/// The trust prompt's rows:
///
/// ```text
/// New project skill detected: "review-code"
/// Trust and enable this skill for this conversation?
/// [2 more queued]
/// ```
///
/// `queued` counts the other skill prompts waiting behind this one (0 paints no marker).
pub fn trust_lines(skill_name: &str, queued: usize, theme: &Theme) -> Vec<Line<'static>> {
    let secondary = Style::default().fg(theme.colors.fg_secondary);
    let mut lines = vec![
        Line::from(vec![
            Span::styled("New project skill detected: ", secondary),
            name_span(skill_name, theme),
        ]),
        Line::from(Span::styled(
            "Trust and enable this skill for this conversation?",
            secondary,
        )),
    ];
    if queued > 0 {
        lines.push(Line::from(Span::styled(
            format!("[{queued} more queued]"),
            secondary,
        )));
    }
    lines
}

/// The Inspect sub-context's rows: a header, then EVERY line of the skill file, each
/// hard-wrapped to `width` cells on grapheme boundaries (nothing is ellipsized or cut).
pub fn inspect_lines(
    skill_name: &str,
    content: &str,
    width: usize,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(vec![
        Span::styled(
            "Inspect skill: ",
            Style::default().fg(theme.colors.fg_secondary),
        ),
        name_span(skill_name, theme),
    ])];
    let body = Style::default().fg(theme.colors.fg_primary);
    for line in content.lines() {
        for row in geom::wrap_hard(line, width.max(1)) {
            lines.push(Line::from(Span::styled(row.to_owned(), body)));
        }
    }
    lines
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
    fn story21_6_the_prompt_names_the_skill_and_the_conversation_scope() {
        let theme = Theme::dark();
        let rows = text(&trust_lines("review-code", 0, &theme));
        assert_eq!(
            rows,
            [
                "New project skill detected: \"review-code\"",
                "Trust and enable this skill for this conversation?"
            ]
        );
        assert!(!rows[1].contains("session"));
    }

    #[test]
    fn story21_6_queued_prompts_are_counted_only_when_there_are_some() {
        let theme = Theme::dark();
        assert_eq!(
            text(&trust_lines("s", 2, &theme)).last().unwrap(),
            "[2 more queued]"
        );
        assert!(
            !text(&trust_lines("s", 0, &theme))
                .iter()
                .any(|row| row.contains("more queued"))
        );
    }

    #[test]
    fn story21_6_inspect_shows_the_whole_file_wrapped_not_twenty_lines() {
        let theme = Theme::dark();
        let content = (0..200)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let rows = text(&inspect_lines("test", &content, 40, &theme));
        assert_eq!(rows.len(), 201, "header + every line");
        assert_eq!(rows[0], "Inspect skill: \"test\"");
        assert_eq!(rows.last().unwrap(), "line 199");
        let long = text(&inspect_lines("t", &"y".repeat(100), 30, &theme));
        assert_eq!(
            long[1..].concat(),
            "y".repeat(100),
            "a long line wraps losslessly"
        );
    }
}
