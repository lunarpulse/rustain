//! Read-time redaction masking (Story 11.4a, Task 5 — defense-in-depth).
//!
//! Pure functions over `u64` content keys: given the set of redacted keys
//! (the tombstone set), drop them from a ranked candidate list at *read* time.
//!
//! This is **not** a substitute for the one-time index purge — a nearest-neighbour
//! query over an un-purged vector still leaks the embedding (architecture.md:176),
//! so the embed-time gate (`refresh()`/rebuild filter) is the load-bearing fix.
//! Masking closes the narrow window in AC-R6 where the tombstone is written and
//! persisted FIRST but the index purge has not yet completed (or was interrupted):
//! for that interval the entry is still in the index but MUST NOT be retrievable.
//! "redacted ⇒ never retrievable" holds at every instant.

use std::collections::HashSet;

/// Whether a key is visible (i.e. NOT redacted).
pub fn is_visible(key: u64, redacted: &HashSet<u64>) -> bool {
    !redacted.contains(&key)
}

/// Return `keys` with every redacted key removed, preserving order. Used to mask
/// a ranked candidate list before it is mapped back to entries.
pub fn retain_visible(keys: Vec<u64>, redacted: &HashSet<u64>) -> Vec<u64> {
    if redacted.is_empty() {
        return keys;
    }
    keys.into_iter()
        .filter(|k| is_visible(*k, redacted))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_redaction_set_is_identity() {
        let redacted = HashSet::new();
        assert_eq!(retain_visible(vec![1, 2, 3], &redacted), vec![1, 2, 3]);
    }

    #[test]
    fn drops_redacted_keys_preserving_order() {
        let redacted: HashSet<u64> = [2, 4].into_iter().collect();
        assert_eq!(
            retain_visible(vec![1, 2, 3, 4, 5], &redacted),
            vec![1, 3, 5]
        );
        assert!(!is_visible(2, &redacted));
        assert!(is_visible(1, &redacted));
    }
}
