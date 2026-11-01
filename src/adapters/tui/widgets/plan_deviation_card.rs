//! Plan deviation card — Story 6.4: reapproval card inline in chat pane.
//! Key bindings: [y] Approve / [e] Edit / [n] Reject.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;

use crate::adapters::tui::keymap::Context;
use crate::adapters::tui::keymap::render::rail;
use crate::adapters::tui::keymap::table_card::CardCmd;

pub fn render(
    area: Rect,
    buf: &mut Buffer,
    original_step_count: u32,
    current_step_count: u32,
    changed_steps: &[u32],
    summary: &str,
) {
    if area.width < 30 || area.height < 4 {
        return;
    }
    let style = Style::default();
    buf.set_string(
        area.x,
        area.y,
        "╔══ Plan deviation — reapproval required",
        style,
    );
    if area.height > 1 {
        let skipped: Vec<String> = changed_steps.iter().map(|n| n.to_string()).collect();
        buf.set_string(
            area.x,
            area.y + 1,
            format!(
                "║ {} tasks → {} tasks. Skipped: {}",
                original_step_count,
                current_step_count,
                skipped.join(", ")
            ),
            style,
        );
    }
    if area.height > 2 {
        buf.set_string(area.x, area.y + 2, format!("║ {}", summary), style);
    }
    if area.height > 3 {
        buf.set_string(
            area.x,
            area.y + 3,
            format!(
                "║ {} Approve revised  {} Edit  {} Reject",
                rail(Context::PlanDecision, CardCmd::Yes),
                rail(Context::PlanDecision, CardCmd::Edit),
                rail(Context::PlanDecision, CardCmd::No)
            ),
            style,
        );
    }
    if area.height > 4 {
        buf.set_string(area.x, area.y + 4, "╚════════════════════════", style);
    }
}

pub fn desired_height(kind: &crate::domain::models::plan::PlanDeviationKind, summary: &str) -> u16 {
    let _ = (kind, summary);
    6
}
