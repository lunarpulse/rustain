//! Paint the launchpad rows (Story 21.9 D15/D16, D19).
//!
//! The rows come from `launchpad::plan`, already fitted to the column; this
//! widget only places them: a left-aligned column, horizontally centred in the
//! area, one cell row each, never outside `area`.

use ratatui::prelude::*;

use crate::adapters::tui::launchpad::{self, LaunchpadFacts, RowKind};
use crate::adapters::tui::theme::Theme;

/// Paint the launchpad of an empty conversation into `area`.
pub fn render(frame: &mut Frame, area: Rect, facts: Option<&LaunchpadFacts>, theme: &Theme) {
    let column = launchpad::column_width(usize::from(area.width));
    if column == 0 || area.height == 0 {
        return;
    }
    let rows = launchpad::plan(facts, usize::from(area.height), usize::from(area.width));
    let x = area.x + (area.width - u16::try_from(column).unwrap_or(area.width)) / 2;
    for (offset, row) in rows.iter().enumerate() {
        let Ok(offset) = u16::try_from(offset) else {
            break;
        };
        if offset >= area.height {
            break;
        }
        let style = match row.kind {
            RowKind::Heading => theme.typography.heading.fg(theme.colors.fg_primary),
            RowKind::Meta => theme.typography.meta.fg(theme.colors.fg_muted),
            RowKind::Plain => Style::default().fg(theme.colors.fg_primary),
            RowKind::Muted => Style::default().fg(theme.colors.fg_muted),
            RowKind::Blank => continue,
        };
        frame
            .buffer_mut()
            .set_stringn(x, area.y + offset, &row.text, column, style);
    }
}
