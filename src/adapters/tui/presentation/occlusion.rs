//! Successful-draw presentation uses actual painted opaque bounds, never DIM/toasts.

use std::ops::Range;

use ratatui::layout::Rect;

// Closed host inventory: legacy cards/prompts, recipient/Wave surfaces and three
// routed popups. Frames have the stack's separate, fixed depth-four bound.
const HOST_CAPACITY: usize = 24;
const FRAME_CAPACITY: usize = 4;

/// Project rectangles onto the actual transcript pane once, without a per-draw
/// allocation. A partial horizontal intersection covers that logical screen row.
#[derive(Debug)]
pub struct OpaqueRects {
    pane: Rect,
    host_rows: [(u16, u16); HOST_CAPACITY],
    host_count: usize,
    frame_rows: [(u16, u16); FRAME_CAPACITY],
    frame_count: usize,
}

impl OpaqueRects {
    pub fn new(pane: Rect) -> Self {
        Self {
            pane,
            host_rows: [(0, 0); HOST_CAPACITY],
            host_count: 0,
            frame_rows: [(0, 0); FRAME_CAPACITY],
            frame_count: 0,
        }
    }

    pub fn host(&mut self, rect: Option<Rect>) {
        if let Some(rows) = rect.and_then(|rect| projected(self.pane, rect)) {
            record(&mut self.host_rows, &mut self.host_count, rows);
        }
    }

    pub fn frame(&mut self, rect: Rect) {
        if let Some(rows) = projected(self.pane, rect) {
            record(&mut self.frame_rows, &mut self.frame_count, rows);
        }
    }

    /// At least one projected feedback row must survive the UNION of all actual
    /// opaque paint. Frame and host categories remain distinct for their bounds.
    pub fn presented(&self, rows: &Range<u16>) -> bool {
        (rows.start.max(self.pane.y)..rows.end.min(self.pane.bottom())).any(|row| {
            !self.host_rows[..self.host_count]
                .iter()
                .chain(self.frame_rows[..self.frame_count].iter())
                .any(|&(start, end)| row >= start && row < end)
        })
    }
}

fn projected(pane: Rect, rect: Rect) -> Option<(u16, u16)> {
    let clipped = pane.intersection(rect);
    (!clipped.is_empty()).then_some((clipped.y, clipped.bottom()))
}

fn record(slots: &mut [(u16, u16)], count: &mut usize, rows: (u16, u16)) {
    assert!(
        *count < slots.len(),
        "opaque paint inventory exceeds its closed bound"
    );
    slots[*count] = rows;
    *count += 1;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_and_off_pane_projections_are_never_presented() {
        let opaque = OpaqueRects::new(Rect::new(4, 10, 20, 8));
        assert!(!opaque.presented(&(10..10)));
        assert!(!opaque.presented(&(0..10)));
        assert!(!opaque.presented(&(18..20)));
        assert!(opaque.presented(&(10..11)));
    }

    #[test]
    fn partial_horizontal_opaque_intersection_covers_the_logical_row() {
        let mut opaque = OpaqueRects::new(Rect::new(4, 10, 20, 8));
        opaque.frame(Rect::new(0, 10, 5, 2));
        assert!(!opaque.presented(&(10..12)));
        assert!(
            opaque.presented(&(10..13)),
            "the third row is genuinely uncovered"
        );
        opaque.host(Some(Rect::new(24, 12, 10, 1)));
        assert!(
            opaque.presented(&(12..13)),
            "touching the right edge is not intersection"
        );
    }

    #[test]
    fn host_and_frame_union_requires_a_real_gap() {
        let mut opaque = OpaqueRects::new(Rect::new(0, 0, 80, 10));
        opaque.host(Some(Rect::new(0, 0, 80, 2)));
        opaque.frame(Rect::new(10, 3, 20, 2));
        assert!(
            opaque.presented(&(0..5)),
            "row two survives both categories"
        );
        opaque.host(Some(Rect::new(0, 2, 80, 1)));
        assert!(!opaque.presented(&(0..5)));
        assert!(opaque.presented(&(0..6)), "row five independently survives");
        opaque.host(None);
        opaque.frame(Rect::new(0, 0, 0, 10));
        assert!(opaque.presented(&(5..6)));
    }
}
