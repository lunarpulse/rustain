//! Rewind confirmation card widget.
//!
//! Renders a Tier-1 overlay (double border) listing how many messages will be
//! removed and which files will be reverted. Mirrors `fork_confirm.rs` structure.

use ratatui::prelude::*;

use crate::adapters::tui::state::RewindPreview;
use crate::adapters::tui::theme::Theme;

/// Truncate `text` to at most `max_chars` characters using safe char-boundary logic.
fn truncate_chars(text: &str, max_chars: usize) -> String {
    let char_count = text.chars().count();
    if char_count <= max_chars {
        return text.to_string();
    }
    let truncated: String = text.chars().take(max_chars.saturating_sub(1)).collect();
    format!("{}…", truncated)
}

/// Simple word-wrap into chunks of at most `max_width` chars.
fn wrap_to_width(text: &str, max_width: usize) -> Vec<String> {
    if max_width == 0 {
        return vec![text.to_string()];
    }
    let mut lines = Vec::new();
    let mut current = String::new();
    for word in text.split_whitespace() {
        if current.is_empty() {
            current = word.to_string();
        } else if current.chars().count() + 1 + word.chars().count() > max_width {
            lines.push(current);
            current = word.to_string();
        } else {
            current.push(' ');
            current.push_str(word);
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

/// Render the rewind confirmation card as styled `Line`s.
///
/// Layout:
/// ```text
/// ╔══════════════════════════════════╗
/// ║ Rewind conversation to this...  ║
/// ║ ┌─ Will be removed ────────────┐ ║
/// ║ │ N messages after message K   │ ║
/// ║ └──────────────────────────────┘ ║
/// ║ ┌─ Files that will be reverted ┐ ║
/// ║ │ • path/to/file.rs            │ ║
/// ║ │ ⚠ path/conflict (ext. mod.) │ ║
/// ║ └──────────────────────────────┘ ║
/// ║ [y] Rewind  [f] Fork  [n] Cancel ║
/// ╚══════════════════════════════════╝
/// ```
pub fn render_rewind_confirmation_lines(
    preview: &RewindPreview,
    width: u16,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let w = width as usize;
    let inner_width = w.saturating_sub(4); // ║ + space + content + space + ║

    let mut lines: Vec<Line<'static>> = Vec::new();

    // Top border
    let top = format!("╔{}╗", "═".repeat(w.saturating_sub(2)));
    lines.push(Line::from(Span::styled(
        top,
        Style::default()
            .fg(theme.colors.warning)
            .add_modifier(Modifier::BOLD),
    )));

    // Title
    let title = "Rewind conversation to this message?";
    let padded_title = format!("║ {:<width$} ║", title, width = inner_width);
    lines.push(Line::from(Span::styled(
        padded_title,
        Style::default()
            .fg(theme.colors.fg_primary)
            .add_modifier(Modifier::BOLD),
    )));

    // ── "Will be removed" panel ──────────────────────────────────────────
    let panel_inner = inner_width.saturating_sub(4); // ├──┤ boxing

    let panel_top = format!(
        "║ ┌─ {:<width$}─┐ ║",
        "Will be removed ",
        width = panel_inner.saturating_sub(18)
    );
    lines.push(Line::from(Span::styled(
        panel_top,
        Style::default().fg(theme.colors.fg_muted),
    )));

    let remove_label = if preview.messages_to_remove == 0 {
        format!(
            "│ {:<width$}│",
            "No messages after this point (already at end).",
            width = panel_inner + 1
        )
    } else {
        format!(
            "│ {:<width$}│",
            format!(
                "{} {} after message {}",
                preview.messages_to_remove,
                if preview.messages_to_remove == 1 {
                    "message"
                } else {
                    "messages"
                },
                preview.target_message_index + 1
            ),
            width = panel_inner + 1
        )
    };
    lines.push(Line::from(Span::styled(
        format!("║ {}  ║", remove_label),
        Style::default().fg(theme.colors.fg_secondary),
    )));

    let panel_bottom = format!("║ └{:─<width$}┘ ║", "", width = panel_inner + 2);
    lines.push(Line::from(Span::styled(
        panel_bottom,
        Style::default().fg(theme.colors.fg_muted),
    )));

    // ── "Files that will be reverted" panel ─────────────────────────────
    let file_panel_top = format!(
        "║ ┌─ {:<width$}─┐ ║",
        "Files that will be reverted ",
        width = panel_inner.saturating_sub(30)
    );
    lines.push(Line::from(Span::styled(
        file_panel_top,
        Style::default().fg(theme.colors.fg_muted),
    )));

    if preview.files_to_revert.is_empty() {
        let no_files = format!(
            "│ {:<width$}│",
            "(no files affected)",
            width = panel_inner + 1
        );
        lines.push(Line::from(Span::styled(
            format!("║ {}  ║", no_files),
            Style::default().fg(theme.colors.fg_muted),
        )));
    } else {
        let max_path_len = panel_inner.saturating_sub(4); // "• " prefix + margin
        for item in &preview.files_to_revert {
            let path_display = truncate_chars(&item.display_path, max_path_len);
            let label = if item.conflict {
                format!("⚠ {} (modified externally)", path_display)
            } else {
                format!("• {}", path_display)
            };
            let color = if item.conflict {
                theme.colors.warning
            } else {
                theme.colors.fg_secondary
            };
            let file_line = format!("│ {:<width$}│", label, width = panel_inner + 1);
            lines.push(Line::from(Span::styled(
                format!("║ {}  ║", file_line),
                Style::default().fg(color),
            )));
        }

        // AC5: if ALL files are conflicts, add a warning note.
        if preview.files_to_revert.iter().all(|f| f.conflict) {
            let note = "All files modified externally — only messages will be truncated.";
            for chunk in wrap_to_width(note, panel_inner + 1) {
                let note_line = format!("│ {:<width$}│", chunk, width = panel_inner + 1);
                lines.push(Line::from(Span::styled(
                    format!("║ {}  ║", note_line),
                    Style::default().fg(theme.colors.warning),
                )));
            }
        }
    }

    let file_panel_bottom = format!("║ └{:─<width$}┘ ║", "", width = panel_inner + 2);
    lines.push(Line::from(Span::styled(
        file_panel_bottom,
        Style::default().fg(theme.colors.fg_muted),
    )));

    // Footer: actions
    let actions = "[y] Rewind  [f] Fork instead  [n] Cancel";
    let padded_actions = format!("║ {:<width$} ║", actions, width = inner_width);
    lines.push(Line::from(Span::styled(
        padded_actions,
        Style::default()
            .fg(theme.colors.warning)
            .add_modifier(Modifier::BOLD),
    )));

    // Bottom border
    let bottom = format!("╚{}╝", "═".repeat(w.saturating_sub(2)));
    lines.push(Line::from(Span::styled(
        bottom,
        Style::default()
            .fg(theme.colors.warning)
            .add_modifier(Modifier::BOLD),
    )));

    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::tui::state::RevertPreviewItem;
    use crate::adapters::tui::theme::Theme;

    fn make_preview(messages_to_remove: usize, files: Vec<RevertPreviewItem>) -> RewindPreview {
        RewindPreview {
            target_message_index: 2,
            messages_to_remove,
            files_to_revert: files,
        }
    }

    fn lines_to_string(lines: &[Line]) -> String {
        lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.to_string())
            .collect()
    }

    #[test]
    fn test_renders_double_border() {
        let theme = Theme::dark();
        let preview = make_preview(2, vec![]);
        let lines = render_rewind_confirmation_lines(&preview, 60, &theme);

        assert!(lines.len() >= 6, "expected at least 6 lines");

        let first: String = lines[0]
            .spans
            .iter()
            .map(|s| s.content.to_string())
            .collect();
        assert!(first.starts_with('╔'), "top border should start with ╔");
        assert!(first.ends_with('╗'), "top border should end with ╗");

        let last: String = lines
            .last()
            .unwrap()
            .spans
            .iter()
            .map(|s| s.content.to_string())
            .collect();
        assert!(last.starts_with('╚'), "bottom border should start with ╚");
        assert!(last.ends_with('╝'), "bottom border should end with ╝");
    }

    #[test]
    fn test_shows_message_count() {
        let theme = Theme::dark();
        let preview = make_preview(3, vec![]);
        let lines = render_rewind_confirmation_lines(&preview, 60, &theme);
        let all_text = lines_to_string(&lines);
        assert!(
            all_text.contains("3 messages after message 3"),
            "expected message count in output"
        );
    }

    #[test]
    fn test_no_files_shows_placeholder() {
        let theme = Theme::dark();
        let preview = make_preview(1, vec![]);
        let lines = render_rewind_confirmation_lines(&preview, 60, &theme);
        let all_text = lines_to_string(&lines);
        assert!(
            all_text.contains("(no files affected)"),
            "expected placeholder for empty file list"
        );
    }

    #[test]
    fn test_conflict_file_marked() {
        let theme = Theme::dark();
        let preview = make_preview(
            1,
            vec![RevertPreviewItem {
                display_path: "src/main.rs".to_string(),
                conflict: true,
            }],
        );
        let lines = render_rewind_confirmation_lines(&preview, 80, &theme);
        let all_text = lines_to_string(&lines);
        assert!(
            all_text.contains("modified externally"),
            "expected conflict marker"
        );
    }

    #[test]
    fn test_all_conflicts_shows_extra_note() {
        let theme = Theme::dark();
        let preview = make_preview(
            1,
            vec![
                RevertPreviewItem {
                    display_path: "a.rs".to_string(),
                    conflict: true,
                },
                RevertPreviewItem {
                    display_path: "b.rs".to_string(),
                    conflict: true,
                },
            ],
        );
        let lines = render_rewind_confirmation_lines(&preview, 80, &theme);
        let all_text = lines_to_string(&lines);
        assert!(
            all_text.contains("All files modified externally"),
            "expected all-conflict note in output"
        );
    }

    #[test]
    fn test_actions_line_shows_all_keys() {
        let theme = Theme::dark();
        let preview = make_preview(0, vec![]);
        let lines = render_rewind_confirmation_lines(&preview, 60, &theme);
        let all_text = lines_to_string(&lines);
        assert!(all_text.contains("[y] Rewind"), "expected [y] Rewind");
        assert!(
            all_text.contains("[f] Fork instead"),
            "expected [f] Fork instead"
        );
        assert!(all_text.contains("[n] Cancel"), "expected [n] Cancel");
    }

    #[test]
    fn test_utf8_path_truncates_safely() {
        let theme = Theme::dark();
        let long_path: String = "中文路径测试文件名".repeat(10);
        let preview = make_preview(
            1,
            vec![RevertPreviewItem {
                display_path: long_path,
                conflict: false,
            }],
        );
        // Must not panic; all spans are valid UTF-8
        let lines = render_rewind_confirmation_lines(&preview, 60, &theme);
        for line in &lines {
            for span in &line.spans {
                let _ = span.content.to_string();
            }
        }
    }
}
