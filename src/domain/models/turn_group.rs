//! `TurnGroup` and supporting value objects for the within-session grouped
//! windowing assembler (Story 11.6, Algorithm A+, ADR-11-2).
//!
//! These are **pure domain value objects** consumed by
//! [`turn_grouping::group_turns`](crate::domain::services::turn_grouping::group_turns)
//! (the deterministic grouping fold) and by
//! [`WindowingAssembler`](crate::infrastructure::context::WindowingAssembler)
//! (the Message-tier assembler that materialises active turns + cold-group
//! gists). Like the rest of `domain/models`, they are **serde-free**: nothing
//! here hits disk or wire (the group structure is recomputed per assembly).
//!
//! # Determinism is a hard contract (AC-11.6.1)
//!
//! Every field that can influence output ordering uses an ordered collection
//! ([`BTreeSet`]) so set-iteration order can never leak non-determinism into a
//! gist, signature, or group id. [`GroupId`] is a `blake3`-derived `u64` over
//! `(session_id, first_turn_idx)` — stable across processes, and matching the
//! `ContextSource::Group(u64)` attribution arm baked in by Story 11.4.
//!
//! # Zero user-visible settings (FR121 / ADR-11-2)
//!
//! [`GroupingConfig`] thresholds are **internal constants** with a `Default`
//! impl. They are deliberately NOT `Deserialize`, NOT read from `_config`, and
//! NOT exposed on any TOML surface — the design discipline that chose Algorithm
//! A+ over A++ (Lunarpulse, 2026-06-01). The only config touchpoint Story 11.6
//! adds is an opt-in *strategy selection* (which assembler), never a parameter.

use std::collections::BTreeSet;
use std::path::PathBuf;

/// Stable identifier for a finalised [`TurnGroup`].
///
/// Computed as a `blake3`-derived `u64` over `(seed, first_turn_idx)` — the same
/// content-key convention as `vector_search::adapter::content_key`. The `seed`
/// is a per-group stable string (the group's first turn id), so the id is
/// deterministic and cross-process-stable for a fixed turn list (AC-11.6.1)
/// without `group_turns` needing the session id threaded in. The `u64` is
/// interchangeable with `ContextSource::Group(u64)` so attribution
/// (`[group: {n}]`) stays consistent across the two tiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct GroupId(pub u64);

impl GroupId {
    /// Derive a deterministic, cross-process-stable id from a per-group stable
    /// seed and the group's first turn index. `blake3` (a non-optional
    /// dependency) gives a stable digest unlike `DefaultHasher`; we keep the low
    /// 8 bytes as a `u64`.
    pub fn derive(seed: &str, first_turn_idx: usize) -> Self {
        let mut hasher = blake3::Hasher::new();
        hasher.update(seed.as_bytes());
        hasher.update(&(first_turn_idx as u64).to_le_bytes());
        let digest = hasher.finalize();
        let bytes: [u8; 8] = digest.as_bytes()[..8]
            .try_into()
            .expect("blake3 digest is 32 bytes");
        GroupId(u64::from_le_bytes(bytes))
    }
}

/// Per-role turn counts inside a group (drives gist counts + diagnostics).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RoleCounts {
    pub user: usize,
    pub assistant: usize,
    pub system: usize,
}

/// The structural "fingerprint" of a group, used by the boundary rules (R4/R5)
/// and the S3 relevance trim. **`BTreeSet` (not `HashSet`)** for determinism.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GroupSignature {
    /// Union of all file paths touched by the group's tool invocations.
    pub file_set: BTreeSet<PathBuf>,
    /// Union of tool names invoked across the group.
    pub tool_names: BTreeSet<String>,
    /// `(first_turn_idx, last_turn_idx)` inclusive, into the source turn slice.
    pub turn_span: (usize, usize),
    /// Per-role turn counts.
    pub role_counts: RoleCounts,
}

/// Which ADR-11-2 boundary signal started a group. Closed enum (R2 `/topic` is
/// deferred to Story 11.6a). `None` on a `TurnGroup` marks the session's first
/// group (it was not started by any rule).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoundaryRule {
    /// R1 — tool-chain integrity (open turn has an unresolved tool call).
    R1ToolChain,
    /// R3 — time gap exceeded `t_gap_minutes`.
    R3TimeGap,
    /// R4 — file-set Jaccard drift exceeded `jaccard_threshold`.
    R4FileDrift,
    /// R5 — tool-chain break (disjoint tool name set).
    R5ToolBreak,
}

/// A finalised topic-cluster of consecutive turns.
///
/// Bi-temporal (`first_touched_at` / `last_touched_at`) and provenance-bearing
/// (`boundary_reason`, `supersedes`) per AC-11.6.10 / AC-11.6.11. Timestamps are
/// **unix milliseconds** (matching `Turn.started_at`).
#[derive(Debug, Clone, PartialEq)]
pub struct TurnGroup {
    pub id: GroupId,
    /// Indices into the source `&[Turn]` slice, ascending.
    pub turn_indices: Vec<usize>,
    /// Deterministic, template-based one-line gist (≤3 sentences, ≤280 chars).
    pub gist: String,
    pub signature: GroupSignature,
    /// `turns[turn_indices[0]].started_at` (unix millis).
    pub first_touched_at: i64,
    /// `turns[turn_indices[last]].started_at` (unix millis).
    pub last_touched_at: i64,
    /// (AC-11.6.10) The earlier group this group continues, if its file-set has
    /// Jaccard-similarity ≥ 0.7 with that group's file-set.
    pub supersedes: Option<GroupId>,
    /// (AC-11.6.11) Which rule started this group (`None` for the first group).
    pub boundary_reason: Option<BoundaryRule>,
}

/// Internal grouping thresholds (Algorithm A+). **NOT user-exposed** — no
/// `Deserialize`, no TOML key (FR121 / ADR-11-2 "zero user-visible settings").
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GroupingConfig {
    /// R3 time-gap threshold, in minutes.
    pub t_gap_minutes: u32,
    /// R4 file-set Jaccard-distance threshold (0.0–1.0).
    pub jaccard_threshold: f32,
    /// Minimum turns before a boundary may split a group (suppression).
    pub min_group_turns: usize,
    /// How many trailing turns count as "active" (default 1).
    pub active_window_k: usize,
}

impl Default for GroupingConfig {
    fn default() -> Self {
        Self {
            t_gap_minutes: 15,
            jaccard_threshold: 0.4,
            min_group_turns: 2,
            active_window_k: 1,
        }
    }
}

/// Jaccard similarity `|A ∩ B| / |A ∪ B|` over two ordered sets.
///
/// **Convention:** `∅ ∩ ∅ → 0.0` (avoids a `0/0` NaN). An empty intersection
/// against any set is `0.0`. Generic over the element type so it serves both
/// `file_set` (`PathBuf`) and `tool_names` (`String`).
pub fn jaccard_similarity<T: Ord>(a: &BTreeSet<T>, b: &BTreeSet<T>) -> f32 {
    if a.is_empty() && b.is_empty() {
        return 0.0;
    }
    let intersection = a.intersection(b).count();
    let union = a.len() + b.len() - intersection;
    if union == 0 {
        0.0
    } else {
        intersection as f32 / union as f32
    }
}

/// Jaccard distance `1 - similarity`. With the `∅ ∩ ∅ → 0.0` similarity
/// convention this yields a distance of `1.0` for two empty sets, but R4 also
/// guards on `files(t_i)` being non-empty so that case never fires a boundary.
pub fn jaccard_distance<T: Ord>(a: &BTreeSet<T>, b: &BTreeSet<T>) -> f32 {
    1.0 - jaccard_similarity(a, b)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(items: &[&str]) -> BTreeSet<PathBuf> {
        items.iter().map(PathBuf::from).collect()
    }

    #[test]
    fn group_id_is_deterministic_across_calls() {
        let a = GroupId::derive("sess-1", 0);
        let b = GroupId::derive("sess-1", 0);
        assert_eq!(a, b);
    }

    #[test]
    fn group_id_varies_by_session_and_index() {
        assert_ne!(GroupId::derive("sess-1", 0), GroupId::derive("sess-2", 0));
        assert_ne!(GroupId::derive("sess-1", 0), GroupId::derive("sess-1", 1));
    }

    #[test]
    fn jaccard_identical_sets_is_one() {
        let a = paths(&["src/a.rs", "src/b.rs"]);
        assert_eq!(jaccard_similarity(&a, &a), 1.0);
    }

    #[test]
    fn jaccard_disjoint_sets_is_zero() {
        let a = paths(&["src/a.rs"]);
        let b = paths(&["tests/b.rs"]);
        assert_eq!(jaccard_similarity(&a, &b), 0.0);
    }

    #[test]
    fn jaccard_partial_overlap() {
        // {a,b} ∩ {b,c} = {b}; ∪ = {a,b,c}; 1/3.
        let a = paths(&["a", "b"]);
        let b = paths(&["b", "c"]);
        assert!((jaccard_similarity(&a, &b) - (1.0 / 3.0)).abs() < 1e-6);
    }

    #[test]
    fn jaccard_empty_both_is_zero_not_nan() {
        let a: BTreeSet<PathBuf> = BTreeSet::new();
        let s = jaccard_similarity(&a, &a);
        assert_eq!(s, 0.0);
        assert!(!s.is_nan());
    }

    #[test]
    fn jaccard_distance_is_complement() {
        let a = paths(&["a", "b"]);
        let b = paths(&["b", "c"]);
        assert!((jaccard_distance(&a, &b) - (1.0 - 1.0 / 3.0)).abs() < 1e-6);
    }

    #[test]
    fn jaccard_works_for_string_tool_names() {
        let a: BTreeSet<String> = ["read", "bash"].iter().map(|s| s.to_string()).collect();
        let b: BTreeSet<String> = ["bash", "grep"].iter().map(|s| s.to_string()).collect();
        assert!((jaccard_similarity(&a, &b) - (1.0 / 3.0)).abs() < 1e-6);
    }

    #[test]
    fn grouping_config_defaults_match_adr() {
        let c = GroupingConfig::default();
        assert_eq!(c.t_gap_minutes, 15);
        assert_eq!(c.jaccard_threshold, 0.4);
        assert_eq!(c.min_group_turns, 2);
        assert_eq!(c.active_window_k, 1);
    }
}
