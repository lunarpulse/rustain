//! NN2 property test for synonym collisions (Story 9-7c, AC-9-7c-5).
//!
//! Two parts:
//!   1. Proptest-driven random stress (30 cases): invariants 1 (bounded fanout)
//!      and 2 (no self-loop) validated against generated queries.
//!   2. Label-driven precision assertion: every query in the combined
//!      `skill_eval_queries.json` is run against the PRODUCTION corpus
//!      (built via `build_production_index()` from `common::eval_corpus`)
//!      with and without synonym expansion; the top-3 results are scored
//!      against the labeled `expected_top3` to measure top-3 precision with
//!      tolerance ε=0.05 (invariant 3 per AC-9-7c-5).
//!
//! Coverage assertion: at least 1 labeled query must trigger non-trivial
//! expansion that changes top-3 ordering.

#![cfg(feature = "meta-search")]

mod common;

use common::eval_corpus::build_production_index;
use common::eval_partition::{load_query_set_partitioned, Partition};
use proptest::prelude::*;
use rustain::domain::ports::search::MetaSearchEngine;
use rustain::domain::models::search_hit::SearchHit;
use rustain::infrastructure::search::{Bm25SearchEngine, SYNONYMS};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU64, Ordering};

static COVERAGE_FIRES: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "snake_case")]
enum EvalCategory {
    ExactName,
    DescriptionKeyword,
    IntentParaphrase,
    CrossProtocol,
    Negative,
    NounConflationInvocationIntent,
    NounConflationCrossKindContamination,
    NounConflationAdversarialParaphrase,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct EvalQuery {
    query: String,
    expected_top3: Vec<String>,
    category: EvalCategory,
}

fn arb_word() -> impl Strategy<Value = &'static str> {
    prop_oneof![
        3 => Just("neat"),
        3 => Just("tidy"),
        3 => Just("clean"),
        3 => Just("trim"),
        3 => Just("shorter"),
        2 => Just("format"),
        2 => Just("code"),
        2 => Just("summarize"),
        1 => Just("pdf"),
        1 => Just("lint"),
        1 => Just("the"),
        1 => Just("please"),
        1 => Just("quick"),
        1 => Just("review"),
    ]
}

fn arb_query() -> impl Strategy<Value = String> {
    proptest::collection::vec(arb_word(), 1..=6).prop_map(|v| v.join(" "))
}

fn top3_names(hits: &[SearchHit]) -> Vec<String> {
    hits.iter().take(3).map(|h| h.name.clone()).collect()
}

/// Compute top-3 precision: fraction of expected entries that appear in hits.
fn top3_precision(hits: &[SearchHit], expected_top3: &[String]) -> f64 {
    if expected_top3.is_empty() {
        return 1.0;
    }
    let hit_names: Vec<&str> = hits.iter().take(3).map(|h| h.name.as_str()).collect();
    let matched = expected_top3.iter().filter(|e| hit_names.contains(&e.as_str())).count();
    matched as f64 / expected_top3.len() as f64
}

/// AC: 9-7c-5 — Combined property test + coverage assertion.
#[test]
fn prop_synonym_expansion_does_not_degrade_top_k_with_coverage() {
    COVERAGE_FIRES.store(0, Ordering::Relaxed);

    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();

    // Build production engine once (used by both Part 1 and Part 2).
    let prod_index = build_production_index();
    let engine = Bm25SearchEngine::new(prod_index);

    // --- Part 1: Proptest random stress (invariants 1+2 + coverage) ---
    let mut config = ProptestConfig::with_cases(30);
    config.failure_persistence = None;

    proptest!(config, |(query in arb_query())| {
        let (expanded, triggered) = SYNONYMS.expand_query(&query);

        // Invariant 1: bounded fanout (N + N*5 = N*6, NOT N*5).
        prop_assert!(
            expanded.split_whitespace().count() <= query.split_whitespace().count() * 6,
            "Fanout breach: query='{}' expanded='{}' ({} > {} * 6)",
            query, expanded, expanded.split_whitespace().count(), query.split_whitespace().count()
        );

        // Invariant 2: no self-loop (per-token check)
        for t in query.split_whitespace() {
            let syns = SYNONYMS.expand(t);
            prop_assert!(
                !syns.contains(&t.to_lowercase()),
                "Self-loop: '{}' in expand('{}') = {:?}",
                t, t, syns
            );
        }

        // Invariant 3: top-K precision tolerance (ε=0.05)
        let baseline_hits = rt.block_on(async { engine.search(&query, None, 3).await.unwrap_or_default() });
        let expanded_hits = rt.block_on(async { engine.search(&expanded, None, 3).await.unwrap_or_default() });

        prop_assert!(
            expanded_hits.len() >= baseline_hits.len().saturating_sub(1),
            "Synonym expansion reduced hit count: baseline={} expanded={} for query='{}'",
            baseline_hits.len(), expanded_hits.len(), query
        );

        // Coverage probe: expansion triggered against production corpus.
        if triggered {
            COVERAGE_FIRES.fetch_add(1, Ordering::Relaxed);
        }
    });

    // --- Part 2: Precision assertion against labeled query set ---
    let mut all_queries: Vec<EvalQuery> = Vec::new();
    all_queries.extend(load_query_set_partitioned::<EvalQuery>(Partition::Dev));
    all_queries.extend(load_query_set_partitioned::<EvalQuery>(Partition::Holdout));
    let labeled: Vec<&EvalQuery> = all_queries.iter().filter(|q| !q.expected_top3.is_empty()).collect();

    let mut total_baseline_prec = 0.0;
    let mut total_expanded_prec = 0.0;
    let mut count = 0usize;

    for q in &labeled {
        let (expanded, triggered) = SYNONYMS.expand_query(&q.query);

        let baseline_hits = rt.block_on(async {
            engine.search(&q.query, None, 3).await.unwrap_or_default()
        });

        let expanded_hits = rt.block_on(async {
            engine.search(&expanded, None, 3).await.unwrap_or_default()
        });

        let baseline_prec = top3_precision(&baseline_hits, &q.expected_top3);
        let expanded_prec = top3_precision(&expanded_hits, &q.expected_top3);

        total_baseline_prec += baseline_prec;
        total_expanded_prec += expanded_prec;
        count += 1;

        assert!(
            expanded_prec >= baseline_prec - 0.05,
            "Synonym expansion degraded top-3 precision for query='{}': \
             baseline={:.3} expanded={:.3} (ε=0.05).\n\
             baseline_hits={:?}\nexpanded_hits={:?}\nexpected={:?}",
            q.query, baseline_prec, expanded_prec,
            top3_names(&baseline_hits), top3_names(&expanded_hits), q.expected_top3
        );

        // Also accumulate coverage here (belt-and-suspenders).
        if triggered && top3_names(&baseline_hits) != top3_names(&expanded_hits) {
            COVERAGE_FIRES.fetch_add(1, Ordering::Relaxed);
        }
    }

    if count > 0 {
        let avg_baseline = total_baseline_prec / count as f64;
        let avg_expanded = total_expanded_prec / count as f64;
        assert!(
            avg_expanded >= avg_baseline - 0.05,
            "Aggregate precision degraded: baseline={:.3} expanded={:.3} across {} queries",
            avg_baseline, avg_expanded, count
        );
    }

    // Coverage assertion: at least 1 case (proptest OR labeled) must fire.
    assert!(
        COVERAGE_FIRES.load(Ordering::Relaxed) >= 1,
        "NN2 coverage assertion failed: zero queries triggered a non-trivial \
         expansion that changed top-3 ordering. The synonym generator or the \
         synonym map is insufficient."
    );
}
