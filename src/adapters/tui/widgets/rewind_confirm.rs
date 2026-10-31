//! Rewind confirmation card widget.
//!
//! Renders a Tier-1 overlay (double border) listing how many messages will be
//! removed and which files will be reverted. Mirrors `fork_confirm.rs` structure.

use ratatui::prelude::*;

use crate::adapters::tui::state::RewindPreview;
use crate::adapters::tui::text_geometry as geom;
use crate::adapters::tui::theme::Theme;

use super::plan_card::wrap_description;

/// `║ ┌─ <title><dashes>┐ ║`, exactly as wide as the card (`panel_inner + 8`).
fn panel_top_row(title: &str, panel_inner: usize) -> String {
    format!(
        "║ ┌─ {title}{}┐ ║",
        "─".repeat(panel_inner.saturating_sub(geom::cells(title)))
    )
}

/// `║ │ <content padded to `cells`>│ ║`, exactly as wide as the card.
fn panel_row(content: &str, cells: usize) -> String {
    format!("║ │ {}│ ║", geom::pad_right(content, cells))
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
    let padded_title = format!("║ {} ║", geom::pad_right(title, inner_width));
    lines.push(Line::from(Span::styled(
        padded_title,
        Style::default()
            .fg(theme.colors.fg_primary)
            .add_modifier(Modifier::BOLD),
    )));

    // ── "Will be removed" panel ──────────────────────────────────────────
    let panel_inner = inner_width.saturating_sub(4); // ├──┤ boxing

    // Cells between `│ ` and `│` of every panel content row.
    let content_cells = panel_inner + 1;

    let panel_top = panel_top_row("Will be removed ", panel_inner);
    lines.push(Line::from(Span::styled(
        panel_top,
        Style::default().fg(theme.colors.fg_muted),
    )));

    let remove_text = if preview.messages_to_remove == 0 {
        "No messages after this point (already at end).".to_string()
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
        panel_row(&remove_text, content_cells),
        Style::default().fg(theme.colors.fg_secondary),
    )));

    let panel_bottom = format!("║ └{:─<width$}┘ ║", "", width = panel_inner + 2);
    lines.push(Line::from(Span::styled(
        panel_bottom,
        Style::default().fg(theme.colors.fg_muted),
    )));

    // ── "Files that will be reverted" panel ─────────────────────────────
    let file_panel_top = panel_top_row("Files that will be reverted ", panel_inner);
    lines.push(Line::from(Span::styled(
        file_panel_top,
        Style::default().fg(theme.colors.fg_muted),
    )));

    if preview.files_to_revert.is_empty() {
        lines.push(Line::from(Span::styled(
            panel_row("(no files affected)", content_cells),
            Style::default().fg(theme.colors.fg_muted),
        )));
    } else {
        for item in &preview.files_to_revert {
            // The marker prefix and the conflict suffix come out of the row's
            // cells; the path's "…" marker is inside what is left.
            let (prefix, suffix) = if item.conflict {
                ("⚠ ", " (modified externally)")
            } else {
                ("• ", "")
            };
            let path_budget =
                content_cells.saturating_sub(geom::cells(prefix) + geom::cells(suffix));
            let path_display = geom::ellipsize(&item.display_path, path_budget, "…");
            let label = format!("{prefix}{path_display}{suffix}");
            let color = if item.conflict {
                theme.colors.warning
            } else {
                theme.colors.fg_secondary
            };
            lines.push(Line::from(Span::styled(
                panel_row(&label, content_cells),
                Style::default().fg(color),
            )));
        }

        // AC5: if ALL files are conflicts, add a warning note.
        if preview.files_to_revert.iter().all(|f| f.conflict) {
            let note = "All files modified externally — only messages will be truncated.";
            for chunk in wrap_description(note, content_cells) {
                lines.push(Line::from(Span::styled(
                    panel_row(&chunk, content_cells),
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
    let padded_actions = format!("║ {} ║", geom::pad_right(actions, inner_width));
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

    // ---- Story 21.11 display-width keystones --------------------------------

    use ratatui::widgets::Paragraph;
    use unicode_segmentation::UnicodeSegmentation;

    const G: &str = "👨\u{200d}👩\u{200d}👧";

    /// First (and only) file row of a one-file preview.
    const FILE_Y: u16 = 6;

    /// (terminal width, non-conflict path cells): the panel content is
    /// `width - 7` cells wide and `• ` takes two.
    const BUDGETS: [(u16, usize); 3] = [(60, 51), (80, 71), (130, 121)];

    fn glyphs(buf: &Buffer, y: u16) -> Vec<(u16, String)> {
        let mut out = Vec::new();
        let mut x = buf.area.left();
        while x < buf.area.right() {
            let symbol = buf
                .cell((x, y))
                .map(|c| c.symbol().to_string())
                .unwrap_or_default();
            let step = u16::try_from(geom::cells(&symbol).max(1)).unwrap_or(1);
            out.push((x, symbol));
            x = x.saturating_add(step);
        }
        out
    }

    fn row_str(buf: &Buffer, y: u16) -> String {
        glyphs(buf, y).into_iter().map(|(_, s)| s).collect()
    }

    /// Row text between the two outer border glyphs, edge blanks trimmed.
    fn interior(buf: &Buffer, y: u16) -> String {
        let g = glyphs(buf, y);
        g[1..g.len() - 1]
            .iter()
            .map(|(_, s)| s.as_str())
            .collect::<String>()
            .trim()
            .to_string()
    }

    /// Content of a `│ … │` panel row.
    fn panel_content(buf: &Buffer, y: u16) -> String {
        interior(buf, y)
            .strip_prefix('│')
            .and_then(|s| s.strip_suffix('│'))
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|| panic!("row {y} is not a panel row: {:?}", row_str(buf, y)))
    }

    fn assert_whole_clusters(text: &str) {
        for g in text.graphemes(true) {
            if g.contains('\u{200d}') || g.contains('👨') {
                assert_eq!(g, G, "partial family cluster in {text:?}");
            }
        }
    }

    /// Paint the real card lines into a buffer exactly `width` cells wide.
    fn painted(preview: &RewindPreview, width: u16) -> Buffer {
        let lines = render_rewind_confirmation_lines(preview, width, &Theme::dark());
        let area = Rect::new(0, 0, width, u16::try_from(lines.len()).unwrap_or(u16::MAX));
        let mut buf = Buffer::empty(area);
        Paragraph::new(lines).render(area, &mut buf);
        buf
    }

    /// Every `║` row (title, panel borders, panel content, actions) ends in
    /// `║` at the last column.
    fn assert_aligned(buf: &Buffer) {
        let width = buf.area.width;
        for y in buf.area.top()..buf.area.bottom() {
            let g = glyphs(buf, y);
            if g.first().map(|f| f.1.as_str()) != Some("║") {
                continue;
            }
            let last = g.last().expect("row has cells");
            assert_eq!(
                (last.0, last.1.as_str()),
                (width - 1, "║"),
                "row {y} border misaligned at width {width}: {:?}",
                row_str(buf, y)
            );
            assert_whole_clusters(&row_str(buf, y));
        }
    }

    fn one_file(path: &str, conflict: bool) -> RewindPreview {
        make_preview(
            1,
            vec![RevertPreviewItem {
                display_path: path.to_string(),
                conflict,
            }],
        )
    }

    #[test]
    fn display_width_rewind_every_row_is_exactly_the_card_width() {
        for width in [60u16, 80, 130] {
            for preview in [
                make_preview(0, vec![]),
                make_preview(3, vec![]),
                one_file("src/main.rs", false),
                one_file("src/main.rs", true),
            ] {
                let buf = painted(&preview, width);
                assert_aligned(&buf);
                // Panel top/bottom borders are part of the aligned set.
                let bordered = (0..buf.area.height)
                    .filter(|&y| glyphs(&buf, y).first().map(|f| f.1.as_str()) == Some("║"))
                    .count();
                assert_eq!(bordered, buf.area.height as usize - 2, "all but top/bottom");
            }
        }
    }

    #[test]
    fn display_width_rewind_marker_at_a_wide_grapheme_cut() {
        for (width, budget) in BUDGETS {
            // G needs two cells but one is left: dropped whole.
            let path = format!("{}{G}xx", "x".repeat(budget - 2));
            let buf = painted(&one_file(&path, false), width);
            assert_aligned(&buf);
            assert_eq!(
                panel_content(&buf, FILE_Y),
                format!("• {}…", "x".repeat(budget - 2)),
                "width {width}"
            );
            // G ends exactly on the cut: kept whole.
            let path = format!("{}{G}xx", "x".repeat(budget - 3));
            let buf = painted(&one_file(&path, false), width);
            assert_aligned(&buf);
            assert_eq!(
                panel_content(&buf, FILE_Y),
                format!("• {}{G}…", "x".repeat(budget - 3)),
                "width {width}"
            );
        }
    }

    #[test]
    fn display_width_rewind_ascii_controls() {
        let buf = painted(&one_file("src/main.rs", false), 60);
        assert_eq!(panel_content(&buf, FILE_Y), "• src/main.rs");
        let buf = painted(&one_file("src/main.rs", true), 60);
        assert_eq!(
            panel_content(&buf, FILE_Y),
            "⚠ src/main.rs (modified externally)"
        );
        let buf = painted(&one_file(&"p".repeat(100), false), 60);
        assert_aligned(&buf);
        assert_eq!(
            panel_content(&buf, FILE_Y),
            format!("• {}…", "p".repeat(50))
        );
    }

    #[test]
    fn display_width_rewind_all_conflict_note_wraps_inside_the_panel() {
        // Narrow card (46 cells, just above the 41-cell action row): the note
        // wraps to several panel rows, each aligned.
        let buf = painted(&one_file("a.rs", true), 46);
        assert_aligned(&buf);
        let note: String = (FILE_Y + 1..buf.area.bottom() - 3)
            .map(|y| panel_content(&buf, y))
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(
            note,
            "All files modified externally — only messages will be truncated."
        );
    }
}
