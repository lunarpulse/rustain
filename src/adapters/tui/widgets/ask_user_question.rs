use ratatui::prelude::*;

use super::plan_card::wrap_description;
use crate::adapters::tui::keymap::Context;
use crate::adapters::tui::keymap::render::rail;
use crate::adapters::tui::keymap::table_card::CardCmd;
use crate::adapters::tui::text_geometry as geom;
use crate::adapters::tui::theme::Theme;

/// State for an AskUserQuestion card.
#[derive(Debug, Clone)]
pub struct AskUserQuestionState {
    #[allow(dead_code)]
    pub tool_use_id: String,
    pub question: String,
    pub input_buffer: String,
    pub cursor_position: usize,
    /// Once submitted, stores the answer and becomes non-editable.
    pub submitted_answer: Option<String>,
}

/// Render an AskUserQuestion card as styled lines.
///
/// Uses double border `╔═╗` with question text and inline input area.
pub fn render_ask_user_lines(
    state: &AskUserQuestionState,
    width: u16,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let w = width as usize;
    let inner_width = w.saturating_sub(4); // ║ + space + content + space + ║

    let mut lines = Vec::new();

    // Top border: ╔═══════╗
    let top = format!("╔{}╗", "═".repeat(w.saturating_sub(2)));
    lines.push(Line::from(Span::styled(
        top,
        Style::default().fg(theme.colors.accent),
    )));

    // Question text (may wrap)
    let question_lines = wrap_question(&state.question, inner_width);
    for q_line in &question_lines {
        let padded = format!("║ {} ║", geom::pad_right(q_line, inner_width));
        lines.push(Line::from(Span::styled(
            padded,
            Style::default().fg(theme.colors.accent),
        )));
    }

    // Input area or submitted answer
    if let Some(answer) = &state.submitted_answer {
        // Static display: the full submitted answer, wrapped under the prompt
        for (i, row) in answer_rows(answer, inner_width.saturating_sub(2))
            .into_iter()
            .enumerate()
        {
            let prefix = if i == 0 { "> " } else { "  " };
            let padded = format!(
                "║ {} ║",
                geom::pad_right(&format!("{prefix}{row}"), inner_width)
            );
            lines.push(Line::from(Span::styled(
                padded,
                Style::default().fg(theme.colors.fg_muted),
            )));
        }
    } else {
        // Editable input with cursor: keep the editable end of the answer and
        // the caret visible; the answer bytes are never modified.
        let prompt = "> ";
        let visible = geom::clip_tail(
            &state.input_buffer,
            inner_width.saturating_sub(geom::cells(prompt) + 1),
        );
        let display_text = format!("{}{}_", prompt, visible);
        let input_line = format!("║ {} ║", geom::pad_right(&display_text, inner_width));
        lines.push(Line::from(Span::styled(
            input_line,
            Style::default().fg(theme.colors.fg_primary),
        )));
    }

    // Bottom border: ╚═══════╝. An unanswered question carries its two keys in the border
    // (rows stay untouched); a submitted one, or a border too narrow for them, stays plain.
    let border_cells = w.saturating_sub(2);
    let hint = format!(
        " {} Send  {} Skip question ",
        rail(Context::Question, CardCmd::Send),
        rail(Context::Question, CardCmd::Skip)
    );
    let hint_cells = geom::cells(&hint);
    let bottom = if state.submitted_answer.is_none() && hint_cells < border_cells {
        format!("╚═{hint}{}╝", "═".repeat(border_cells - 1 - hint_cells))
    } else {
        format!("╚{}╝", "═".repeat(border_cells))
    };
    lines.push(Line::from(Span::styled(
        bottom,
        Style::default().fg(theme.colors.accent),
    )));

    lines
}

/// Word-wrap the question by display cells (shared card wrapper). A long
/// unbroken word is hard-wrapped on grapheme boundaries so the whole question
/// is always shown; an empty question still yields one blank row.
fn wrap_question(text: &str, max_width: usize) -> Vec<String> {
    let mut lines = wrap_description(text, max_width);
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

/// Display rows of a submitted answer: every logical line hard-wrapped on
/// grapheme boundaries (the answer text is shown exactly, never reflowed).
fn answer_rows(answer: &str, budget: usize) -> Vec<&str> {
    answer
        .split('\n')
        .flat_map(|line| geom::wrap_hard(line, budget))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::tui::theme::Theme;

    #[test]
    fn test_ask_user_question_renders_double_border() {
        let state = AskUserQuestionState {
            tool_use_id: "tu-1".to_string(),
            question: "What is the name of your project?".to_string(),
            input_buffer: String::new(),
            cursor_position: 0,
            submitted_answer: None,
        };
        let theme = Theme::dark();
        let lines = render_ask_user_lines(&state, 60, &theme);

        // Should have: top border, question, input, bottom border = 4 lines
        assert!(lines.len() >= 4);

        let first: String = lines[0]
            .spans
            .iter()
            .map(|s| s.content.to_string())
            .collect();
        assert!(first.starts_with('╔'));
        assert!(first.ends_with('╗'));

        let last: String = lines
            .last()
            .unwrap()
            .spans
            .iter()
            .map(|s| s.content.to_string())
            .collect();
        assert!(last.starts_with('╚'));
        assert!(last.ends_with('╝'));
    }

    #[test]
    fn test_ask_user_question_shows_submitted_answer() {
        let state = AskUserQuestionState {
            tool_use_id: "tu-1".to_string(),
            question: "Project name?".to_string(),
            input_buffer: String::new(),
            cursor_position: 0,
            submitted_answer: Some("MyProject".to_string()),
        };
        let theme = Theme::dark();
        let lines = render_ask_user_lines(&state, 60, &theme);
        let answer_line: String = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.to_string())
            .collect::<String>();
        assert!(answer_line.contains("MyProject"));
    }
}
