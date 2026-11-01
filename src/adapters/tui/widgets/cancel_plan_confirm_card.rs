//! Cancel plan confirmation card — Story 6.4.
//! Key bindings: [y] Cancel all / [n] Keep running.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;

use crate::adapters::tui::keymap::Context;
use crate::adapters::tui::keymap::render::rail;
use crate::adapters::tui::keymap::table_card::CardCmd;

pub fn render(area: Rect, buf: &mut Buffer, plan_title: &str, n_pending: u32, n_completed: u32) {
    if area.width < 25 || area.height < 3 {
        return;
    }
    let style = Style::default();
    buf.set_string(area.x, area.y, "┌─ Cancel plan? ──────────────", style);
    if area.height > 1 {
        buf.set_string(area.x, area.y + 1, format!("│ {}", plan_title), style);
    }
    if area.height > 2 {
        buf.set_string(
            area.x,
            area.y + 2,
            format!(
                "│ {} task(s) remaining, {} completed (will be kept)",
                n_pending, n_completed
            ),
            style,
        );
    }
    if area.height > 3 {
        buf.set_string(
            area.x,
            area.y + 3,
            format!(
                "│ {} Cancel all   {} Keep running",
                rail(Context::Confirm, CardCmd::Yes),
                rail(Context::Confirm, CardCmd::No)
            ),
            style,
        );
    }
    if area.height > 4 {
        buf.set_string(area.x, area.y + 4, "└────────────────────────────", style);
    }
}

pub fn desired_height() -> u16 {
    6
}
