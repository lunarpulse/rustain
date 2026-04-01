use crate::adapters::tui::state::Direction;

/// Find the next boundary offset in the given direction from current_offset.
/// Returns `None` at the first/last boundary (no wrap-around).
///
/// For `Down` direction (toward newer/bottom content): finds the next boundary
/// with a *smaller* offset-from-bottom (closer to bottom).
/// For `Up` direction (toward older/top content): finds the next boundary
/// with a *larger* offset-from-bottom (farther from bottom).
///
/// `boundaries` contains line offsets from the *top* of the rendered content.
/// `current_offset` is the offset-from-bottom scroll position.
/// `total_height` is the total rendered content height.
/// `viewport_height` is the visible viewport height.
pub fn find_next_boundary(
    current_offset: usize,
    boundaries: &[usize],
    direction: Direction,
    total_height: usize,
    viewport_height: usize,
) -> Option<usize> {
    if boundaries.is_empty() || total_height <= viewport_height {
        return None;
    }

    // Convert offset-from-bottom to top-of-viewport line index
    let max_offset = total_height.saturating_sub(viewport_height);
    let clamped_offset = current_offset.min(max_offset);
    let top_line = max_offset.saturating_sub(clamped_offset);

    match direction {
        Direction::Down => {
            // Moving down = toward bottom = decreasing offset-from-bottom
            // Find the next boundary *after* the current viewport top (higher line number)
            // and convert it to an offset-from-bottom that is smaller than current.
            for &b in boundaries {
                if b > top_line {
                    let new_offset = max_offset.saturating_sub(b);
                    return Some(new_offset);
                }
            }
            // Past last boundary — jump to bottom
            if current_offset > 0 {
                return Some(0);
            }
            None
        }
        Direction::Up => {
            // Moving up = toward top = increasing offset-from-bottom
            // Find the last boundary *before* the current viewport top (lower line number)
            // and convert it to an offset-from-bottom that is larger than current.
            for &b in boundaries.iter().rev() {
                if b < top_line {
                    let new_offset = max_offset.saturating_sub(b);
                    return Some(new_offset.min(max_offset));
                }
            }
            // Already at or above the first boundary — no-op
            None
        }
    }
}

/// Reverse-map the topmost visible line to a message index for status bar display.
/// Returns `(current_msg_index_1based, total_msg_count)`.
///
/// `scroll_offset` is offset-from-bottom.
/// `viewport_height` is the visible area height.
/// `message_boundaries` contains line offsets (from top) where each message starts.
/// `total_height` is the total rendered content height.
pub fn offset_to_message_index(
    scroll_offset: usize,
    viewport_height: u16,
    message_boundaries: &[usize],
    total_height: usize,
) -> (usize, usize) {
    let total = message_boundaries.len();
    if total == 0 {
        return (0, 0);
    }

    let vp = viewport_height as usize;
    let max_offset = total_height.saturating_sub(vp);
    let clamped = scroll_offset.min(max_offset);

    // Top visible line (from top of content)
    let top_line = max_offset.saturating_sub(clamped);

    // Binary search: find the last boundary <= top_line
    let idx = match message_boundaries.binary_search(&top_line) {
        Ok(i) => i,
        Err(i) => i.saturating_sub(1),
    };

    (idx + 1, total) // 1-based index
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_find_next_boundary_empty_boundaries() {
        assert_eq!(find_next_boundary(0, &[], Direction::Up, 100, 24), None);
        assert_eq!(find_next_boundary(0, &[], Direction::Down, 100, 24), None);
    }

    #[test]
    fn test_find_next_boundary_content_fits_viewport() {
        // Content fits entirely in viewport — no scrolling possible
        assert_eq!(
            find_next_boundary(0, &[0, 5, 10], Direction::Up, 20, 24),
            None
        );
    }

    #[test]
    fn test_find_next_boundary_up_from_bottom() {
        // total=100, viewport=24, max_offset=76
        // boundaries at lines 0, 25, 50, 75
        // At bottom (offset=0), top_line = 76
        // Up: last boundary < 76 is 75. new_offset = 76 - 75 = 1.
        let boundaries = vec![0, 25, 50, 75];
        let result = find_next_boundary(0, &boundaries, Direction::Up, 100, 24);
        assert_eq!(result, Some(1));
    }

    #[test]
    fn test_find_next_boundary_up_steps_through_boundaries() {
        // total=100, viewport=24, max_offset=76
        // At offset=1 (top_line=75), Up: last boundary < 75 is 50. new_offset=76-50=26.
        let boundaries = vec![0, 25, 50, 75];
        let result = find_next_boundary(1, &boundaries, Direction::Up, 100, 24);
        assert_eq!(result, Some(26));

        // At offset=26 (top_line=50), Up: last boundary < 50 is 25. new_offset=76-25=51.
        let result = find_next_boundary(26, &boundaries, Direction::Up, 100, 24);
        assert_eq!(result, Some(51));

        // At offset=51 (top_line=25), Up: last boundary < 25 is 0. new_offset=76-0=76.
        let result = find_next_boundary(51, &boundaries, Direction::Up, 100, 24);
        assert_eq!(result, Some(76));
    }

    #[test]
    fn test_find_next_boundary_down_steps_through_boundaries() {
        // total=100, viewport=24, max_offset=76
        // At top (offset=76, top_line=0), Down: first boundary > 0 is 25. new_offset=76-25=51.
        let boundaries = vec![0, 25, 50, 75];
        let result = find_next_boundary(76, &boundaries, Direction::Down, 100, 24);
        assert_eq!(result, Some(51));

        // At offset=51 (top_line=25), Down: first boundary > 25 is 50. new_offset=76-50=26.
        let result = find_next_boundary(51, &boundaries, Direction::Down, 100, 24);
        assert_eq!(result, Some(26));

        // At offset=26 (top_line=50), Down: first boundary > 50 is 75. new_offset=76-75=1.
        let result = find_next_boundary(26, &boundaries, Direction::Down, 100, 24);
        assert_eq!(result, Some(1));

        // At offset=1 (top_line=75), Down: no boundary > 75. Jump to 0.
        let result = find_next_boundary(1, &boundaries, Direction::Down, 100, 24);
        assert_eq!(result, Some(0));
    }

    #[test]
    fn test_find_next_boundary_at_top_up_is_noop() {
        let boundaries = vec![0, 25, 50, 75];
        // max_offset = 76, at top (offset=76, top_line=0)
        // Up: no boundary < 0 => None
        let result = find_next_boundary(76, &boundaries, Direction::Up, 100, 24);
        assert_eq!(result, None);
    }

    #[test]
    fn test_find_next_boundary_at_bottom_down_is_noop() {
        let boundaries = vec![0, 25, 50, 75];
        // At bottom (offset=0, top_line=76). Down: no boundary > 76. offset already 0. None.
        let result = find_next_boundary(0, &boundaries, Direction::Down, 100, 24);
        assert_eq!(result, None);
    }

    #[test]
    fn test_offset_to_message_index_empty() {
        assert_eq!(offset_to_message_index(0, 24, &[], 0), (0, 0));
    }

    #[test]
    fn test_offset_to_message_index_single_message() {
        assert_eq!(offset_to_message_index(0, 24, &[0], 10), (1, 1));
    }

    #[test]
    fn test_offset_to_message_index_multiple_messages() {
        // total=100, viewport=24, max_offset=76
        // boundaries at 0, 30, 60
        // At bottom (offset=0), top_line=76. Binary search for 76 in [0,30,60] => Err(3) => idx=2
        assert_eq!(offset_to_message_index(0, 24, &[0, 30, 60], 100), (3, 3));

        // At top (offset=76), top_line=0. Binary search for 0 => Ok(0) => idx=0
        assert_eq!(offset_to_message_index(76, 24, &[0, 30, 60], 100), (1, 3));

        // Scrolled to middle (offset=40), top_line=36. Binary search for 36 => Err(2) => idx=1
        assert_eq!(offset_to_message_index(40, 24, &[0, 30, 60], 100), (2, 3));
    }

    #[test]
    fn test_offset_to_message_index_at_boundary() {
        // top_line exactly at a boundary
        // total=100, vp=24, max=76. offset=46 => top_line=30, exact match at idx=1
        assert_eq!(offset_to_message_index(46, 24, &[0, 30, 60], 100), (2, 3));
    }

}
