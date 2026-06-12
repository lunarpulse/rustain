#![allow(dead_code)] // shared test-support module; helpers used by a subset of integration-test binaries
//! Eval report writer with `LazyLock<Mutex<EvalMetrics>>` accumulator (Story 9-7c, AC-9-7c-4).
//!
//! Metric-bearing tests write their measured values to the `METRICS` accumulator
//! BEFORE their final assertions.  `test_finalize_eval_report` drains the
//! accumulator and writes the JSON report.
//!
//! ## Test ordering
//!
//! Tag every metric-bearing test AND `test_finalize_eval_report` with
//! `#[serial(eval_metrics)]`.  The canonical invocation is:
//!
//! ```bash
//! RUSTAIN_EVAL_PARTITION=all cargo test --features meta-search \
//!   --test integration_skill_eval_harness -- --test-threads=1
//! ```

use super::eval_types::EvalReport;
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{LazyLock, Mutex};

/// Accumulator for all measured metrics during a harness run.
#[derive(Default, Debug, Clone)]
pub struct EvalMetrics {
    // Primary metrics (AC-9-7b-3, populated by test_skill_eval_harness_primary_metrics)
    pub recall_at_5: Option<f64>,
    pub mrr: Option<f64>,
    pub kind_filter_accuracy: Option<f64>,
    pub p95_latency_us: Option<u64>,

    // Token budget (AC-9-7b-5)
    pub token_budget_p95: Option<u64>,
    pub token_budget_p99: Option<u64>,

    // Schema (AC-9-7b-6)
    pub schema_conformance_pass: Option<bool>,
    pub name_capability_id_roundtrip_pass: Option<bool>,

    // Noun-conflation (AC-9-7b-4) — partition-tagged per A2 D14
    pub a1_per_subcategory_dev: BTreeMap<String, f64>,
    pub a1_per_subcategory_holdout: BTreeMap<String, f64>,
    pub a1_aggregate_dev: Option<f64>,
    pub a1_aggregate_holdout: Option<f64>,
    pub a1_bis_per_stratum_dev: BTreeMap<String, f64>,
    pub a1_bis_per_stratum_holdout: BTreeMap<String, f64>,

    // 9-7c additions
    pub synonym_expansion_triggered_count: Option<u64>,
    pub holdout_sha256: Option<String>,
    pub baseline_p95_us: Option<u64>,
    pub partition_run: Option<String>,
    pub generated_at: Option<String>,
}

/// Global accumulator — all metric-bearing tests write here.
pub static METRICS: LazyLock<Mutex<EvalMetrics>> =
    LazyLock::new(|| Mutex::new(EvalMetrics::default()));

/// Drain the accumulator and write the report. Idempotent — safe to call
/// multiple times. Partial accumulators write `Option::None` for missing
/// fields (DO NOT lie by zeroing).
pub fn drain_and_write_report(report_path: &Path) -> std::io::Result<()> {
    let guard = METRICS.lock().unwrap_or_else(|e| {
        eprintln!(
            "WARNING: METRICS Mutex was poisoned by a prior test panic. \
             Proceeding with partial accumulator state."
        );
        e.into_inner()
    });
    let metrics = guard.clone();

    // Read existing on-disk JSON for defensive merge.
    let existing_json = std::fs::read_to_string(report_path).unwrap_or_default();
    let mut report: EvalReport = if existing_json.is_empty() {
        EvalReport::default()
    } else {
        serde_json::from_str(&existing_json).unwrap_or_else(|_| EvalReport::default())
    };

    // Merge accumulator values into report.
    if let Some(v) = metrics.recall_at_5 {
        report.recall_at_5 = v;
    }
    if let Some(v) = metrics.mrr {
        report.mrr = v;
    }
    if let Some(v) = metrics.kind_filter_accuracy {
        report.kind_filter_accuracy = v;
    }
    if let Some(v) = metrics.p95_latency_us {
        report.p95_latency_us = v;
    }
    if let Some(v) = metrics.token_budget_p95 {
        report.token_budget_p95 = v;
    }
    if let Some(v) = metrics.token_budget_p99 {
        report.token_budget_p99 = v;
    }
    if let Some(v) = metrics.schema_conformance_pass {
        report.schema_conformance_pass = v;
    }
    if let Some(v) = metrics.name_capability_id_roundtrip_pass {
        report.name_capability_id_roundtrip_pass = v;
    }
    if let Some(v) = metrics.synonym_expansion_triggered_count {
        report.synonym_expansion_triggered_count = Some(v);
    }
    if let Some(ref v) = metrics.holdout_sha256 {
        report.holdout_sha256 = Some(v.clone());
    }
    if let Some(v) = metrics.baseline_p95_us {
        report.baseline_p95_us = Some(v);
    }
    if let Some(ref v) = metrics.partition_run {
        report.partition_run = Some(v.clone());
    }

    // Noun-conflation fields
    if !metrics.a1_per_subcategory_dev.is_empty() {
        report.a1_per_subcategory_dev = Some(metrics.a1_per_subcategory_dev.clone());
    }
    if !metrics.a1_per_subcategory_holdout.is_empty() {
        report.a1_per_subcategory_holdout = Some(metrics.a1_per_subcategory_holdout.clone());
    }
    if let Some(v) = metrics.a1_aggregate_dev {
        report.aggregate_dev = Some(v);
    }
    if let Some(v) = metrics.a1_aggregate_holdout {
        report.aggregate_holdout = Some(v);
    }
    if !metrics.a1_bis_per_stratum_dev.is_empty() {
        report.a1_bis_per_stratum_dev = Some(metrics.a1_bis_per_stratum_dev.clone());
    }
    if !metrics.a1_bis_per_stratum_holdout.is_empty() {
        report.a1_bis_per_stratum_holdout = Some(metrics.a1_bis_per_stratum_holdout.clone());
    }

    // Compute a1_noun_conflation from dev aggregate (fallback to holdout if dev missing).
    report.a1_noun_conflation = metrics
        .a1_aggregate_dev
        .or(metrics.a1_aggregate_holdout)
        .unwrap_or(0.0);

    // Update a1_per_subcategory from dev (fallback to holdout if dev missing).
    let subcat = if !metrics.a1_per_subcategory_dev.is_empty() {
        &metrics.a1_per_subcategory_dev
    } else {
        &metrics.a1_per_subcategory_holdout
    };
    if !subcat.is_empty() {
        report.a1_per_subcategory = subcat.clone();
    }

    // Update a1_bis_per_stratum from dev (fallback to holdout if dev missing).
    let bis = if !metrics.a1_bis_per_stratum_dev.is_empty() {
        &metrics.a1_bis_per_stratum_dev
    } else {
        &metrics.a1_bis_per_stratum_holdout
    };
    if !bis.is_empty() {
        report.a1_bis_per_stratum = bis.clone();
        report.a1_bis_per_stratum_min = bis.values().cloned().fold(f64::INFINITY, f64::min);
    }

    // Verdict
    let (overall_pass, blockers) = compute_verdict(&metrics);
    report.overall_pass = overall_pass;
    report.blockers = blockers;

    // Timestamp
    report.generated_at = Some(chrono::Utc::now().to_rfc3339());

    // Run_ts as unix seconds
    report.run_ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .to_string();

    // Write atomically.
    let json = serde_json::to_string_pretty(&report).unwrap();
    let tmp_path = report_path.with_extension("json.tmp");
    std::fs::write(&tmp_path, &json)?;
    std::fs::rename(&tmp_path, report_path)?;

    // Round-trip assertion (will be checked by caller).
    let _loaded: EvalReport =
        serde_json::from_str(&json).expect("Eval report must round-trip through serde_json");

    Ok(())
}

/// Compute overall_pass + blockers from the current accumulator state.
pub fn compute_verdict(m: &EvalMetrics) -> (bool, Vec<String>) {
    let mut blockers = Vec::new();

    // Primary metric thresholds
    if let Some(recall) = m.recall_at_5 {
        if recall < 0.80 {
            blockers.push(format!("recall_at_5 = {:.3} < 0.80", recall));
        }
    }
    if let Some(mrr) = m.mrr {
        if mrr < 0.65 {
            blockers.push(format!("mrr = {:.3} < 0.65", mrr));
        }
    }
    if let Some(kfa) = m.kind_filter_accuracy {
        if kfa < 0.90 {
            blockers.push(format!("kind_filter_accuracy = {:.3} < 0.90", kfa));
        }
    }
    if let Some(lat) = m.p95_latency_us {
        if lat >= 50_000 {
            blockers.push(format!("p95_latency_us = {} >= 50000us", lat));
        }
    }
    if let Some(tb) = m.token_budget_p95 {
        if tb > 30 {
            blockers.push(format!("token_budget_p95 = {} > 30", tb));
        }
    }
    if let Some(tb) = m.token_budget_p99 {
        if tb > 50 {
            blockers.push(format!("token_budget_p99 = {} > 50", tb));
        }
    }
    if let Some(false) = m.schema_conformance_pass {
        blockers.push("schema_conformance_pass = false".to_string());
    }
    if let Some(false) = m.name_capability_id_roundtrip_pass {
        blockers.push("name_capability_id_roundtrip_pass = false".to_string());
    }

    // A1 aggregate on DEV (binding for 9-7c)
    if let Some(agg) = m.a1_aggregate_dev {
        if agg < 0.85 {
            blockers.push(format!("a1_aggregate_dev = {:.3} < 0.85", agg));
        }
    }

    // A1 conjunctive holdout gate — skip unless explicitly enabled
    let a1_gate_enabled = std::env::var("RUSTAIN_A1_GATE").as_deref() == Ok("enabled");
    if a1_gate_enabled {
        if let Some(agg) = m.a1_aggregate_holdout {
            if agg < 0.85 {
                blockers.push(format!("a1_aggregate_holdout = {:.3} < 0.85", agg));
            }
        }
        for (subcat, threshold) in [
            ("invocation_intent", 0.60_f64),
            ("cross_kind_contamination", 0.50),
            ("adversarial_paraphrase_under_kind_omission", 0.50),
        ] {
            let val = m
                .a1_per_subcategory_holdout
                .get(subcat)
                .copied()
                .unwrap_or(0.0);
            if val < threshold {
                blockers.push(format!(
                    "a1_per_subcategory_holdout.{} = {:.3} < {:.2}",
                    subcat, val, threshold
                ));
            }
        }
    } else {
        blockers.push(
            "A1 conjunctive holdout gate not yet evaluated (coupled to 9-7d merge per A2 D9)"
                .to_string(),
        );
    }

    let overall_pass = blockers.is_empty();
    (overall_pass, blockers)
}
