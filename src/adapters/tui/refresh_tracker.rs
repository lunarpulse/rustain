//! Tracks which provider catalog refreshes are currently in-flight.
//!
//! Story 7.6 AC8 — `RefreshTracker` newtype encapsulates a `std::sync::Mutex`
//! so the `CONFORMANCE_EXCEPTION_STD_SYNC_LOCK` tag attaches to ONE site.

use std::collections::HashMap;
use std::sync::{Arc, Mutex}; // CONFORMANCE_EXCEPTION_STD_SYNC_LOCK

/// Tracks which provider catalog refreshes are currently in-flight.
/// Owned by `ModelSelectorState` via `Arc<RefreshTracker>`; cloned into each
/// spawned discovery task. The inner Mutex is an implementation detail.
///
/// CONFORMANCE_EXCEPTION_STD_SYNC_LOCK: short ≤1µs HashMap ops (insert/remove/contains),
/// never held across .await. Encapsulated here so the exception attaches to ONE site
/// instead of leaking the lock primitive into ModelSelectorState.
pub struct RefreshTracker {
    inner: Mutex<HashMap<String, usize>>, // CONFORMANCE_EXCEPTION_STD_SYNC_LOCK: short ≤1µs HashMap ops, never across .await
}

impl RefreshTracker {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(HashMap::new()),
        })
    }

    /// Returns true if the given provider_id is mid-fetch.
    pub fn contains(&self, provider_id: &str) -> bool {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(provider_id)
    }

    /// Explicitly remove a provider from the tracker (deterministic cleanup in event loop).
    pub fn remove(&self, provider_id: &str) {
        let mut map = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(c) = map.get_mut(provider_id) {
            if *c == 1 {
                map.remove(provider_id);
            } else {
                *c -= 1;
            }
        }
    }

    /// Mark a provider mid-fetch. Returns a RAII Guard whose Drop decrements the ref-count.
    pub fn insert(self: &Arc<Self>, provider_id: String) -> RefreshGuard {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(provider_id.clone())
            .and_modify(|c| *c += 1)
            .or_insert(1);
        RefreshGuard {
            tracker: Arc::clone(self),
            provider_id,
        }
    }
}

/// RAII guard that decrements the provider_id ref-count from the tracker on drop.
pub struct RefreshGuard {
    tracker: Arc<RefreshTracker>,
    provider_id: String,
}

impl Drop for RefreshGuard {
    fn drop(&mut self) {
        self.tracker.remove(&self.provider_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::panic::catch_unwind;

    #[test]
    fn refresh_tracker_guard_drops_on_err() {
        let tracker = RefreshTracker::new();
        let result = catch_unwind(|| {
            let _g = tracker.insert("foo".to_string());
            assert!(tracker.contains("foo"));
            panic!("boom");
        });
        assert!(result.is_err());
        // Guard dropped when panic unwound past it
        assert!(!tracker.contains("foo"));
    }

    #[test]
    fn refresh_tracker_concurrent_inserts() {
        let tracker = RefreshTracker::new();
        let mut guards = Vec::new();
        for _ in 0..10 {
            guards.push(tracker.insert("foo".to_string()));
        }
        assert!(tracker.contains("foo"));
        drop(guards);
        assert!(!tracker.contains("foo"));
    }
}
