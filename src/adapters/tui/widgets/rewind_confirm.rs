//! Rewind confirmation decision frame body.

use ratatui::prelude::*;

use crate::adapters::tui::decision_queue::RewindPreview;
use crate::adapters::tui::text_geometry as geom;
use crate::adapters::tui::theme::Theme;

/// Build the rewind body without borders or an action rail.
pub fn lines(preview: &RewindPreview, theme: &Theme) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::from(Span::styled(
            "Rewind conversation to this message?",
            Style::default()
                .fg(theme.colors.fg_primary)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            "Will be removed",
            Style::default().fg(theme.colors.fg_muted),
        )),
    ];
    let remove_text = if preview.messages_to_remove == 0 {
        "No messages after this point (already at end).".to_owned()
    } else {
        format!(
            "{} {} after message {}",
            preview.messages_to_remove,
            if preview.messages_to_remove == 1 {
                "message"
            } else {
                "messages"
            },
            preview.target_message_index + 1
        )
    };
    lines.push(Line::from(Span::styled(
        remove_text,
        Style::default().fg(theme.colors.fg_secondary),
    )));
    lines.push(Line::from(Span::styled(
        "Files that will be reverted",
        Style::default().fg(theme.colors.fg_muted),
    )));
    if preview.files_to_revert.is_empty() {
        lines.push(Line::from(Span::styled(
            "(no files affected)",
            Style::default().fg(theme.colors.fg_muted),
        )));
    } else {
        for item in preview.files_to_revert.iter().take(200) {
            let (prefix, suffix, color) = if item.conflict {
                ("⚠ ", " (modified externally)", theme.colors.warning)
            } else {
                ("• ", "", theme.colors.fg_secondary)
            };
            let budget = 53usize.saturating_sub(geom::cells(prefix) + geom::cells(suffix));
            let path = geom::ellipsize(&item.display_path, budget, "…");
            lines.push(Line::from(Span::styled(
                format!("{prefix}{path}{suffix}"),
                Style::default().fg(color),
            )));
        }
        let omitted = preview.files_to_revert.len().saturating_sub(200);
        if omitted > 0 {
            lines.push(Line::from(format!("… and {omitted} more files")));
        }
        if preview.files_to_revert.iter().all(|item| item.conflict) {
            lines.push(Line::from(Span::styled(
                "All files modified externally — only messages will be truncated.",
                Style::default().fg(theme.colors.warning),
            )));
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::tui::decision_queue::RevertPreviewItem;

    fn make_preview(messages_to_remove: usize, files: Vec<RevertPreviewItem>) -> RewindPreview {
        RewindPreview {
            target_message_index: 2,
            messages_to_remove,
            files_to_revert: files,
        }
    }

    fn text(preview: &RewindPreview) -> String {
        lines(preview, &Theme::dark())
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn one_file(path: &str, conflict: bool) -> RewindPreview {
        make_preview(
            1,
            vec![RevertPreviewItem {
                display_path: path.into(),
                conflict,
            }],
        )
    }

    #[test]
    fn rewind_body_has_no_double_border() {
        let rendered = text(&make_preview(2, vec![]));
        assert!(rendered.contains("Rewind conversation to this message?"));
        assert!(!rendered.contains('║'));
    }

    #[test]
    fn test_shows_message_count() {
        assert!(text(&make_preview(3, vec![])).contains("3 messages after message 3"));
    }

    #[test]
    fn test_no_files_shows_placeholder() {
        assert!(text(&make_preview(1, vec![])).contains("(no files affected)"));
    }

    #[test]
    fn test_conflict_file_marked() {
        assert!(
            text(&one_file("src/main.rs", true)).contains("⚠ src/main.rs (modified externally)")
        );
    }

    #[test]
    fn test_all_conflicts_shows_extra_note() {
        let preview = make_preview(
            1,
            vec![
                RevertPreviewItem {
                    display_path: "a.rs".into(),
                    conflict: true,
                },
                RevertPreviewItem {
                    display_path: "b.rs".into(),
                    conflict: true,
                },
            ],
        );
        assert!(text(&preview).contains("All files modified externally"));
    }

    #[test]
    fn rewind_body_leaves_actions_to_frame() {
        let rendered = text(&make_preview(0, vec![]));
        assert!(!rendered.contains("[y]"));
        assert!(!rendered.contains("[f]"));
        assert!(!rendered.contains("[n]"));
    }

    #[test]
    fn test_utf8_path_truncates_safely() {
        let rendered = text(&one_file(&"中文路径测试文件名".repeat(10), false));
        assert!(rendered.contains('…'));
    }

    #[test]
    fn display_width_rewind_file_rows_are_cell_bounded() {
        for conflict in [false, true] {
            let rendered = lines(&one_file(&"界".repeat(100), conflict), &Theme::dark());
            let row = rendered
                .iter()
                .find(|line| {
                    line.spans
                        .iter()
                        .any(|span| span.content.starts_with('•') || span.content.starts_with('⚠'))
                })
                .unwrap();
            let row_text: String = row.spans.iter().map(|span| span.content.as_ref()).collect();
            assert!(geom::cells(&row_text) <= 53);
        }
    }

    #[test]
    fn display_width_rewind_marker_at_a_wide_grapheme_cut() {
        const G: &str = "👨\u{200d}👩\u{200d}👧";
        let rendered = text(&one_file(&format!("{}{G}xx", "x".repeat(49)), false));
        assert!(rendered.contains(&format!("• {}…", "x".repeat(49))));
        let rendered = text(&one_file(&format!("{}{G}xx", "x".repeat(48)), false));
        assert!(rendered.contains(&format!("• {}{G}…", "x".repeat(48))));
    }

    #[test]
    fn display_width_rewind_ascii_controls() {
        assert!(text(&one_file("src/main.rs", false)).contains("• src/main.rs"));
        assert!(
            text(&one_file("src/main.rs", true)).contains("⚠ src/main.rs (modified externally)")
        );
        assert!(
            text(&one_file(&"p".repeat(100), false)).contains(&format!("• {}…", "p".repeat(50)))
        );
    }

    #[test]
    fn display_width_rewind_all_conflict_note_survives_for_frame_wrapping() {
        assert!(
            text(&one_file("a.rs", true))
                .contains("All files modified externally — only messages will be truncated.")
        );
    }
}
