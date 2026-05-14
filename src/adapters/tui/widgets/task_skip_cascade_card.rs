//! Task skip cascade card — Story 6.4.
//! Key bindings: [s] Skip them too / [c] Continue anyway / [n] Cancel skip.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;

use crate::adapters::tui::state::SkipCascadePending;

pub fn render(area: Rect, buf: &mut Buffer, pending: &SkipCascadePending) {
    if area.width < 30 || area.height < 3 {
        return;
    }
    let style = Style::default();
    let count = pending.downstream.len();
    buf.set_string(area.x, area.y, "┌─ Skip cascade", style);
    if area.height > 1 {
        buf.set_string(
            area.x,
            area.y + 1,
            format!(
                "│ Task {} skipped — {} task(s) depend on it.",
                pending.source_task, count
            ),
            style,
        );
    }
    if area.height > 2 {
        buf.set_string(
            area.x,
            area.y + 2,
            "│ [s] Skip them too  [c] Continue anyway  [n] Cancel skip",
            style,
        );
    }
    if area.height > 3 {
        buf.set_string(area.x, area.y + 3, "└────────────────────", style);
    }
}

pub fn desired_height() -> u16 {
    5
}
