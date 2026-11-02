//! Frame geometry is derived from the unreserved shell, never from payload size.

use ratatui::layout::Rect;

use super::super::layout::AppLayout;

/// Everything above the Stage bottom edge; the status row and composer stay out.
#[must_use]
pub fn modal_region(layout: &AppLayout, area: Rect) -> Rect {
    Rect::new(
        area.x,
        area.y,
        area.width,
        layout.chat_pane.bottom().saturating_sub(area.y),
    )
}

/// Narrow terminals use a full sheet; wide terminals use a centered 90% × 85% box.
#[must_use]
pub fn frame_rect(region: Rect) -> Rect {
    if region.width < 100 {
        return region;
    }
    let width = (u32::from(region.width) * 9 / 10) as u16;
    // Keep the header, one body row and footer even with a full composer.
    let height = ((u32::from(region.height) * 85 / 100) as u16)
        .max(5)
        .min(region.height);
    Rect::new(
        region.x + (region.width - width) / 2,
        region.y + (region.height - height) / 2,
        width,
        height,
    )
}
