//! Hybrid-retrieval fusion math (Story 11.3b, AC2) — pure, deterministic, and
//! testable without any model, network, or clock.
//!
//! ## The algorithm (Q2 = RRF + half-life recency; Q4 = 30-day half-life)
//! No fusion/normalization/temporal-decay code existed anywhere in the repo
//! (greenfield). The chosen design:
//!
//! 1. **Reciprocal Rank Fusion (RRF)** of two relevance signals — the vector
//!    (cosine) ranking and the BM25 (keyword) ranking:
//!    `relevance(d) = w_vec/(K + rank_vec(d)) + w_bm25/(K + rank_bm25(d))`
//!    with `K = 60` (canonical RRF constant) and `w_vec = w_bm25 = 1.0`. A doc
//!    absent from one ranked list contributes 0 from that list. RRF is **rank-
//!    based and scale-free**, so cosine ∈ [-1, 1] and BM25 ∈ [0, ∞) — which live
//!    on incompatible scales — fuse with NO normalization parameters.
//!
//! 2. A **multiplicative half-life recency weight**:
//!    `recency(d) = exp(-ln2 · age_days(d) / HALF_LIFE_DAYS)`, `HALF_LIFE_DAYS = 30`.
//!    Recent entries score higher; an irrelevant-but-recent entry cannot outrank
//!    a highly-relevant old one unless the age gap is large.
//!
//! 3. `final(d) = relevance(d) · recency(d)`; sort descending, tie-broken by
//!    `content_key` ascending (deterministic, mirrors index.rs:114-117).
//!
//! Ranks are **1-based** (the best doc in a list is rank 1 → `1/(K+1)`).

use std::collections::HashMap;

/// Canonical Reciprocal Rank Fusion constant. `K = 60` needs no tuning.
pub const RRF_K: f64 = 60.0;
/// Weight of the vector (semantic) signal (Q2: equal weights).
pub const W_VEC: f64 = 1.0;
/// Weight of the BM25 (keyword) signal (Q2: equal weights).
pub const W_BM25: f64 = 1.0;
/// Temporal-decay half-life in days (Q4). After this many days an entry's
/// recency weight halves. Adjustable in code; deliberately NOT a user-facing
/// setting ("keep complexity off the settings surface").
pub const DEFAULT_HALF_LIFE_DAYS: f64 = 30.0;

/// RRF relevance scores over the union of keys in the two ranked lists. Each
/// list is ordered best-first; a key's rank is its 1-based position. A key
/// present in only one list gets only that list's contribution.
pub fn rrf_relevance(vec_ranked: &[u64], bm25_ranked: &[u64]) -> HashMap<u64, f64> {
    let mut scores: HashMap<u64, f64> = HashMap::new();
    for (i, &key) in vec_ranked.iter().enumerate() {
        let rank = (i + 1) as f64;
        *scores.entry(key).or_insert(0.0) += W_VEC / (RRF_K + rank);
    }
    for (i, &key) in bm25_ranked.iter().enumerate() {
        let rank = (i + 1) as f64;
        *scores.entry(key).or_insert(0.0) += W_BM25 / (RRF_K + rank);
    }
    scores
}

/// Multiplicative half-life recency weight in `(0, 1]`. `age_days` is clamped at
/// 0 (a future-dated entry gets the maximum weight `1.0`). For realistic
/// timestamps the result stays representable in `f64` (underflow to exactly 0
/// only past ~88 years of age).
pub fn recency_weight(age_days: f64, half_life_days: f64) -> f64 {
    debug_assert!(half_life_days > 0.0, "half-life must be positive");
    let age = age_days.max(0.0);
    (-std::f64::consts::LN_2 * age / half_life_days).exp()
}

/// Fuse RRF relevance with per-key recency (`final = relevance · recency`) and
/// return up to `limit` keys ordered by descending final score, ties broken by
/// `key` ascending (deterministic). A key missing from `recency` is treated as
/// fully recent (weight `1.0`) so a relevance-only fusion still ranks sensibly.
pub fn fuse_rank(
    relevance: &HashMap<u64, f64>,
    recency: &HashMap<u64, f64>,
    limit: usize,
) -> Vec<u64> {
    if limit == 0 {
        return Vec::new();
    }
    let mut scored: Vec<(u64, f64)> = relevance
        .iter()
        .map(|(&key, &rel)| {
            let rec = recency.get(&key).copied().unwrap_or(1.0);
            (key, rel * rec)
        })
        .collect();
    scored.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.0.cmp(&b.0))
    });
    scored.into_iter().take(limit).map(|(k, _)| k).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rrf_first_rank_scores_higher_than_later() {
        let r = rrf_relevance(&[10, 20, 30], &[]);
        assert!(
            r[&10] > r[&20] && r[&20] > r[&30],
            "earlier rank → higher RRF"
        );
        // Exact: rank 1 → 1/61.
        assert!((r[&10] - 1.0 / 61.0).abs() < 1e-12);
        assert!((r[&30] - 1.0 / 63.0).abs() < 1e-12);
    }

    #[test]
    fn rrf_sums_both_lists_and_rewards_agreement() {
        // key 1 is top of BOTH lists; key 2 only in vec; key 3 only in bm25.
        let r = rrf_relevance(&[1, 2], &[1, 3]);
        // key 1: 1/61 + 1/61; key 2: 1/62; key 3: 1/62.
        assert!((r[&1] - (2.0 / 61.0)).abs() < 1e-12);
        assert!((r[&2] - (1.0 / 62.0)).abs() < 1e-12);
        assert!((r[&3] - (1.0 / 62.0)).abs() < 1e-12);
        assert!(
            r[&1] > r[&2],
            "a doc ranked highly by both beats single-list docs"
        );
    }

    #[test]
    fn rrf_ordering_is_what_fuse_returns_without_decay() {
        // No recency info → relevance-only ordering, deterministic tie-break.
        let rel = rrf_relevance(&[1, 2], &[1, 3]);
        let recency = HashMap::new();
        let order = fuse_rank(&rel, &recency, 10);
        assert_eq!(order[0], 1, "agreement doc first");
        // keys 2 and 3 tie on relevance (both 1/62) → tie-break by key asc.
        assert_eq!(order, vec![1, 2, 3]);
    }

    #[test]
    fn recency_is_monotonic_and_halves_at_half_life() {
        let h = DEFAULT_HALF_LIFE_DAYS;
        assert!(
            (recency_weight(0.0, h) - 1.0).abs() < 1e-12,
            "age 0 → weight 1"
        );
        assert!(
            (recency_weight(h, h) - 0.5).abs() < 1e-12,
            "age = half-life → 0.5"
        );
        assert!(
            (recency_weight(2.0 * h, h) - 0.25).abs() < 1e-12,
            "2 half-lives → 0.25"
        );
        // Strictly decreasing in age.
        assert!(recency_weight(1.0, h) > recency_weight(10.0, h));
        // Future-dated (negative age) clamps to the max weight.
        assert!((recency_weight(-5.0, h) - 1.0).abs() < 1e-12);
    }

    #[test]
    fn temporal_decay_breaks_relevance_ties_toward_recent() {
        // Two docs with IDENTICAL relevance; the more-recent one must rank first.
        let rel: HashMap<u64, f64> = [(1u64, 0.01), (2u64, 0.01)].into_iter().collect();
        // key 1 is old (60 days), key 2 is fresh (1 day).
        let recency: HashMap<u64, f64> = [
            (1u64, recency_weight(60.0, DEFAULT_HALF_LIFE_DAYS)),
            (2u64, recency_weight(1.0, DEFAULT_HALF_LIFE_DAYS)),
        ]
        .into_iter()
        .collect();
        let order = fuse_rank(&rel, &recency, 10);
        assert_eq!(order, vec![2, 1], "equal relevance → recent ranks higher");
    }

    #[test]
    fn temporal_decay_cannot_trivially_resurrect_irrelevant_recent() {
        // A highly-relevant OLD doc still beats a barely-relevant FRESH doc when
        // the relevance gap dominates the modest age gap.
        let rel: HashMap<u64, f64> = [(1u64, 2.0 / 61.0), (2u64, 1.0 / 120.0)]
            .into_iter()
            .collect();
        let recency: HashMap<u64, f64> = [
            (1u64, recency_weight(20.0, DEFAULT_HALF_LIFE_DAYS)), // old-ish
            (2u64, recency_weight(0.0, DEFAULT_HALF_LIFE_DAYS)),  // brand new
        ]
        .into_iter()
        .collect();
        let order = fuse_rank(&rel, &recency, 10);
        assert_eq!(
            order[0], 1,
            "strong relevance survives a moderate age penalty"
        );
    }

    #[test]
    fn fuse_respects_limit_and_zero_limit() {
        let rel = rrf_relevance(&[1, 2, 3, 4, 5], &[]);
        assert_eq!(fuse_rank(&rel, &HashMap::new(), 3).len(), 3);
        assert!(fuse_rank(&rel, &HashMap::new(), 0).is_empty());
    }
}
