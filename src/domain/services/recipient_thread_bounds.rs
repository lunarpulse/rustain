//! Shared count and byte bounds for recipient-thread reads.

pub const MAX_THREAD_TURNS: usize = 50;
pub const MAX_THREAD_RESPONSE_BYTES: usize = 896 * 1024;
pub const MAX_THREAD_TURN_BYTES: usize = 448 * 1024;
pub const MAX_REPLY_BYTES: usize = 64 * 1024;

/// Select pinned turns and a caller-ordered window. Costs are lazy so a read
/// never serializes or copies representations of turns it does not consider.
pub fn window_indices(
    count: usize,
    pins: &[usize],
    candidates: impl IntoIterator<Item = usize>,
    mut cost: impl FnMut(usize) -> usize,
) -> Vec<usize> {
    let mut selected = Vec::with_capacity(MAX_THREAD_TURNS);
    let mut spent = 0usize;
    for &pin in pins {
        if pin < count && !selected.contains(&pin) {
            spent = spent.saturating_add(cost(pin));
            selected.push(pin);
        }
    }
    for index in candidates {
        if selected.len() >= MAX_THREAD_TURNS {
            break;
        }
        if index >= count || selected.contains(&index) {
            continue;
        }
        let bytes = cost(index);
        if bytes > MAX_THREAD_TURN_BYTES {
            continue;
        }
        if spent.saturating_add(bytes) > MAX_THREAD_RESPONSE_BYTES {
            break;
        }
        spent += bytes;
        selected.push(index);
    }
    selected.sort_unstable();
    selected
}

/// Root-pinned newest window used by the remote A2A thread contract.
pub fn newest_indices(
    count: usize,
    root: Option<usize>,
    cost: impl FnMut(usize) -> usize,
) -> Vec<usize> {
    let pins = [root.unwrap_or(count)];
    window_indices(count, &pins, (0..count).rev(), cost)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newest_keeps_root_and_last_forty_nine_chronologically() {
        let selected = newest_indices(70, Some(0), |_| 1);
        assert_eq!(selected.len(), MAX_THREAD_TURNS);
        assert_eq!(selected[0], 0);
        assert_eq!(selected[1], 21);
        assert_eq!(selected[49], 69);
    }

    #[test]
    fn paged_window_deduplicates_pins_and_respects_turn_and_page_budgets() {
        let selected = window_indices(8, &[0, 4, 4], [1, 2, 3, 5, 6, 7], |index| {
            if index == 2 {
                MAX_THREAD_TURN_BYTES + 1
            } else {
                4
            }
        });
        assert_eq!(selected, vec![0, 1, 3, 4, 5, 6, 7]);

        let selected = window_indices(4, &[0, 3], [2, 1], |_| MAX_THREAD_RESPONSE_BYTES / 3);
        assert_eq!(selected, vec![0, 2, 3]);
    }
}
