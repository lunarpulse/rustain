//! The bound on durable refusal records (Story 18.4d, D16).
//!
//! # Why a quota and not a dedupe key
//!
//! A refused frame must leave a durable trace, or `peer revoke` has no
//! observable receiver-side consequence. But an unlisted stranger presents no
//! credential at all, and `MAX_CONCURRENT_INBOUND_CONNECTIONS` is a *concurrency
//! semaphore, not a rate limit*: a `connect → one frame → close` loop appends one
//! fsync'd row per handshake, forever, and rotating keys makes the attacker's own
//! identity cardinality unbounded too.
//!
//! Grouping refusals by connection does not fix that — a grouping key is not a
//! quota, and a new connection resets it. The bound here is a leaky bucket **per
//! source identity** plus a **global ceiling**, so both the per-peer and the
//! many-peers shapes are bounded, and the suppressed volume is counted in memory
//! and summarized on a timer rather than journaled.
//!
//! # What is deliberately lost
//!
//! Suppressed repeats are **not** durable and no surface may imply they are. The
//! in-memory summary is the only place volume appears. That is the trade: an
//! unauthenticated stranger must never convert handshakes into unbounded durable
//! writes.
//!
//! # Deterministic by construction
//!
//! Every decision is a pure function of `(peer, now_ms)` and prior state, so the
//! bound is proven with a counter and an injected clock — never a sleep window.

use std::collections::HashMap;

use crate::domain::models::PeerId;

/// Durable refusal rows one source may produce per [`REFUSAL_REFILL_MS`].
pub const REFUSAL_BURST_PER_PEER: u32 = 1;

/// How long one source waits for its next durable refusal row.
pub const REFUSAL_REFILL_MS: i64 = 60_000;

/// Durable refusal rows every source together may produce per refill interval.
///
/// The per-peer bucket alone is defeated by key rotation, which costs an attacker
/// nothing; this ceiling is what makes the total bounded.
pub const REFUSAL_GLOBAL_BURST: u32 = 32;

/// Distinct sources tracked at once. Beyond this the least recently seen source
/// is forgotten, so the tracking table cannot grow with attacker-chosen keys
/// either.
pub const MAX_TRACKED_SOURCES: usize = 256;

/// Whether this refusal earns a durable row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefusalRecordVerdict {
    /// Append the durable row.
    Journal,
    /// Count it in memory only.
    Suppress,
}

#[derive(Clone, Copy, Debug)]
struct Bucket {
    tokens: u32,
    /// Instant the current token allowance was computed from. Advanced in whole
    /// intervals so repeated sub-interval checks cannot drift tokens upward.
    refilled_at_ms: i64,
}

impl Bucket {
    fn new(capacity: u32, now_ms: i64) -> Self {
        Self {
            tokens: capacity,
            refilled_at_ms: now_ms,
        }
    }

    fn refill(&mut self, capacity: u32, now_ms: i64) {
        if now_ms < self.refilled_at_ms {
            // A clock that moved backwards must not mint tokens — and must not
            // re-anchor either: re-anchoring would count the rolled-back span
            // as refill-eligible once the clock recovered. Hold the anchor;
            // the bucket simply refills late.
            return;
        }
        let elapsed = now_ms - self.refilled_at_ms;
        let intervals = elapsed / REFUSAL_REFILL_MS;
        if intervals == 0 {
            return;
        }
        let gained = u32::try_from(intervals).unwrap_or(u32::MAX);
        self.tokens = self.tokens.saturating_add(gained).min(capacity);
        self.refilled_at_ms += intervals * REFUSAL_REFILL_MS;
    }

    fn take(&mut self) -> bool {
        if self.tokens == 0 {
            return false;
        }
        self.tokens -= 1;
        true
    }
}

/// The leaky-bucket bound over durable refusal rows.
#[derive(Debug)]
pub struct RefusalJournalQuota {
    per_peer_capacity: u32,
    global_capacity: u32,
    max_sources: usize,
    sources: HashMap<PeerId, (Bucket, i64)>,
    global: Option<Bucket>,
    suppressed: u64,
    forgotten_sources: u64,
}

impl RefusalJournalQuota {
    #[must_use]
    pub fn new() -> Self {
        Self::with_limits(
            REFUSAL_BURST_PER_PEER,
            REFUSAL_GLOBAL_BURST,
            MAX_TRACKED_SOURCES,
        )
    }

    #[must_use]
    pub fn with_limits(per_peer_capacity: u32, global_capacity: u32, max_sources: usize) -> Self {
        Self {
            per_peer_capacity,
            global_capacity,
            max_sources: max_sources.max(1),
            sources: HashMap::new(),
            global: None,
            suppressed: 0,
            forgotten_sources: 0,
        }
    }

    /// Decide whether this refusal earns a durable row.
    ///
    /// ⚠ Both buckets are consulted, and a token is spent only when **both**
    /// admit. A global refusal must not silently drain the per-peer allowance of
    /// an unrelated source.
    pub fn admit(&mut self, peer: &PeerId, now_ms: i64) -> RefusalRecordVerdict {
        let global_capacity = self.global_capacity;
        let per_peer_capacity = self.per_peer_capacity;

        let mut global = self
            .global
            .unwrap_or_else(|| Bucket::new(global_capacity, now_ms));
        global.refill(global_capacity, now_ms);

        self.evict_if_full(peer);
        let mut bucket = self
            .sources
            .get(peer)
            .map_or_else(|| Bucket::new(per_peer_capacity, now_ms), |slot| slot.0);
        bucket.refill(per_peer_capacity, now_ms);

        let verdict = if bucket.tokens == 0 || global.tokens == 0 {
            self.suppressed += 1;
            RefusalRecordVerdict::Suppress
        } else {
            bucket.take();
            global.take();
            RefusalRecordVerdict::Journal
        };

        self.global = Some(global);
        match self.sources.get_mut(peer) {
            Some(slot) => *slot = (bucket, now_ms),
            None => {
                self.sources.insert(peer.clone(), (bucket, now_ms));
            }
        }
        verdict
    }

    /// Refusals counted in memory instead of journaled, since construction.
    #[must_use]
    pub fn suppressed(&self) -> u64 {
        self.suppressed
    }

    /// Sources dropped from the tracking table to keep it bounded.
    #[must_use]
    pub fn forgotten_sources(&self) -> u64 {
        self.forgotten_sources
    }

    /// Sources currently tracked. Never above the configured cap.
    #[must_use]
    pub fn tracked_sources(&self) -> usize {
        self.sources.len()
    }

    /// Take the suppression figures for one timer summary, resetting the count.
    ///
    /// ⛔ The caller logs this. It is never journaled: a durable record of
    /// suppressed volume is the unbounded durable write this quota exists to
    /// prevent.
    pub fn take_summary(&mut self) -> Option<RefusalSuppressionSummary> {
        if self.suppressed == 0 && self.forgotten_sources == 0 {
            return None;
        }
        let summary = RefusalSuppressionSummary {
            suppressed: self.suppressed,
            forgotten_sources: self.forgotten_sources,
            tracked_sources: self.sources.len(),
        };
        self.suppressed = 0;
        self.forgotten_sources = 0;
        Some(summary)
    }

    fn evict_if_full(&mut self, incoming: &PeerId) {
        if self.sources.len() < self.max_sources || self.sources.contains_key(incoming) {
            return;
        }
        let Some(stalest) = self
            .sources
            .iter()
            .min_by_key(|(_, (_, last_seen_ms))| *last_seen_ms)
            .map(|(peer, _)| peer.clone())
        else {
            return;
        };
        self.sources.remove(&stalest);
        self.forgotten_sources += 1;
    }
}

impl Default for RefusalJournalQuota {
    fn default() -> Self {
        Self::new()
    }
}

/// What a timer summary reports. Log-only.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RefusalSuppressionSummary {
    pub suppressed: u64,
    pub forgotten_sources: u64,
    pub tracked_sources: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer(seed: u8) -> PeerId {
        PeerId::from_public_key(
            &ed25519_dalek::SigningKey::from_bytes(&[seed; 32])
                .verifying_key()
                .to_bytes(),
        )
        .expect("peer id")
    }

    /// Ratchet (AC6): N refusals inside one interval produce exactly one row.
    /// Deterministic — the clock never moves.
    #[test]
    fn n_refusals_in_one_interval_earn_exactly_one_row() {
        let mut quota = RefusalJournalQuota::new();
        let source = peer(1);
        let journaled = (0..64)
            .filter(|_| quota.admit(&source, 1_000) == RefusalRecordVerdict::Journal)
            .count();
        assert_eq!(journaled, 1, "the durable record must be rate-bounded");
        assert_eq!(quota.suppressed(), 63);
    }

    /// Positive control: the bound is a quota over time, not a permanent gag.
    #[test]
    fn the_same_source_earns_another_row_once_its_bucket_refills() {
        let mut quota = RefusalJournalQuota::new();
        let source = peer(2);
        assert_eq!(
            quota.admit(&source, 1_000),
            RefusalRecordVerdict::Journal,
            "the first refusal must be journaled"
        );
        assert_eq!(quota.admit(&source, 1_000), RefusalRecordVerdict::Suppress);
        assert_eq!(
            quota.admit(&source, 1_000 + REFUSAL_REFILL_MS),
            RefusalRecordVerdict::Journal,
            "a refilled bucket must record again"
        );
    }

    /// Key rotation is free for an attacker, so the per-source bucket alone is
    /// not a bound. The global ceiling is what makes the total finite.
    #[test]
    fn rotating_the_source_identity_still_hits_the_global_ceiling() {
        let mut quota = RefusalJournalQuota::new();
        let journaled = (0..200)
            .filter(|seed| quota.admit(&peer(*seed as u8), 5_000) == RefusalRecordVerdict::Journal)
            .count();
        assert_eq!(
            journaled, REFUSAL_GLOBAL_BURST as usize,
            "the global ceiling must bound a key-rotating source"
        );
    }

    /// The tracking table is bounded too: attacker-chosen keys cannot grow it.
    #[test]
    fn the_tracking_table_is_bounded_and_forgets_the_stalest_source() {
        let mut quota = RefusalJournalQuota::with_limits(1, u32::MAX, 4);
        for seed in 0..40u8 {
            quota.admit(&peer(seed), 1_000 + i64::from(seed));
        }
        assert!(quota.tracked_sources() <= 4, "the table must stay bounded");
        assert!(
            quota.forgotten_sources() > 0,
            "eviction must be observable, not silent"
        );
    }

    /// A clock that moves backwards must not mint an allowance — not while it
    /// is backwards, and not by counting the rolled-back span once it recovers.
    #[test]
    fn a_backwards_clock_does_not_refill_a_spent_bucket() {
        let mut quota = RefusalJournalQuota::with_limits(1, u32::MAX, 4);
        let source = peer(3);
        assert_eq!(quota.admit(&source, 100_000), RefusalRecordVerdict::Journal);
        assert_eq!(quota.admit(&source, 0), RefusalRecordVerdict::Suppress);
        assert_eq!(quota.admit(&source, 1), RefusalRecordVerdict::Suppress);
        // The clock recovers less than one refill interval past the anchor.
        // Re-anchoring to 0 would call the whole rolled-back span elapsed and
        // mint a token here.
        assert_eq!(
            quota.admit(&source, 100_000 + REFUSAL_REFILL_MS / 2),
            RefusalRecordVerdict::Suppress,
            "the rolled-back span must not count as refill-eligible time"
        );
        // Positive control: a full honest interval past the anchor does refill.
        assert_eq!(
            quota.admit(&source, 100_000 + REFUSAL_REFILL_MS),
            RefusalRecordVerdict::Journal
        );
    }

    /// The summary is take-and-reset so a timer reports each interval once.
    #[test]
    fn the_summary_is_taken_once_per_interval() {
        let mut quota = RefusalJournalQuota::new();
        let source = peer(4);
        for _ in 0..5 {
            quota.admit(&source, 1_000);
        }
        let summary = quota.take_summary().expect("suppression happened");
        assert_eq!(summary.suppressed, 4);
        assert!(
            quota.take_summary().is_none(),
            "a taken summary must not repeat"
        );
    }
}
