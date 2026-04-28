//! Cancel plan confirmation card — Story 6.4.
//! Key bindings: [y] Cancel all / [n] Keep running.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;

pub fn render(
    area: Rect,
    buf: &mut Buffer,
    plan_title: &str,
    n_pending: u32,
    n_completed: u32,
) {
    if area.width < 25 || area.height < 3 {
        return;
    }
    let style = Style::default();
    buf.set_string(area.x, area.y, format!("┌─ Cancel plan? ──────────────"), style);
    if area.height > 1 {
        buf.set_string(area.x, area.y + 1, format!("│ {}", plan_title), style);
    }
    if area.height > 2 {
        buf.set_string(area.x, area.y + 2, format!("│ {} task(s) remaining, {} completed (will be kept)", n_pending, n_completed), style);
    }
    if area.height > 3 {
        buf.set_string(area.x, area.y + 3, "│ [y] Cancel all   [n] Keep running", style);
    }
    if area.height > 4 {
        buf.set_string(area.x, area.y + 4, "└────────────────────────────", style);
    }
}

pub fn desired_height() -> u16 {
    6
}
