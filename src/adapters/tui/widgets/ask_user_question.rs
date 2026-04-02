use ratatui::prelude::*;

use crate::adapters::tui::theme::Theme;

/// State for an AskUserQuestion card.
#[derive(Debug, Clone)]
pub struct AskUserQuestionState {
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
        let padded = format!("║ {:<width$} ║", q_line, width = inner_width);
        lines.push(Line::from(Span::styled(
            padded,
            Style::default().fg(theme.colors.accent),
        )));
    }

    // Input area or submitted answer
    if let Some(answer) = &state.submitted_answer {
        // Static display: shows the submitted answer
        let display = format!("║ > {} ", answer);
        let padded = format!("{:<width$}║", display, width = w.saturating_sub(1));
        lines.push(Line::from(Span::styled(
            padded,
            Style::default().fg(theme.colors.fg_muted),
        )));
    } else {
        // Editable input with cursor
        let prompt = "> ";
        let display_text = format!("{}{}_", prompt, &state.input_buffer);
        let input_line = format!("║ {:<width$} ║", display_text, width = inner_width);
        lines.push(Line::from(Span::styled(
            input_line,
            Style::default().fg(theme.colors.fg_primary),
        )));
    }

    // Bottom border: ╚═══════╝
    let bottom = format!("╚{}╝", "═".repeat(w.saturating_sub(2)));
    lines.push(Line::from(Span::styled(
        bottom,
        Style::default().fg(theme.colors.accent),
    )));

    lines
}

/// Simple word-wrapping for question text.
fn wrap_question(text: &str, max_width: usize) -> Vec<String> {
    if max_width == 0 {
        return vec![text.to_string()];
    }
    let mut lines = Vec::new();
    let mut current_line = String::new();

    for word in text.split_whitespace() {
        if current_line.is_empty() {
            current_line = word.to_string();
        } else if current_line.len() + 1 + word.len() > max_width {
            lines.push(current_line);
            current_line = word.to_string();
        } else {
            current_line.push(' ');
            current_line.push_str(word);
        }
    }
    if !current_line.is_empty() {
        lines.push(current_line);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
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
