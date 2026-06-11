//! Shared eval report types (Story 9-7b/9-7c).
//!
//! Both `integration_skill_eval_harness.rs` and `eval_report_writer.rs`
//! consume `EvalReport` — a single definition prevents schema drift.
//! The `REPORT_SCHEMA_JSON` inline golden is the canonical lock (Mary D
//! single-file-diff-site discipline).

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Locked eval report schema (AC-9-7b-9). Inline golden — single-file diff site.
pub const REPORT_SCHEMA_JSON: &str = r#"{
  "story": "9-7b",
  "run_ts": "",
  "bm25_version": "=2.3.2",
  "corpus_size": 0,
  "query_count": 0,
  "recall_at_5": 0.0,
  "mrr": 0.0,
  "kind_filter_accuracy": 0.0,
  "p95_latency_us": 0,
  "token_budget_p95": 0,
  "token_budget_p99": 0,
  "schema_conformance_pass": true,
  "name_capability_id_roundtrip_pass": true,
  "a1_noun_conflation": 0.0,
  "a1_per_subcategory": {
    "invocation_intent": 0.0,
    "cross_kind_contamination": 0.0,
    "adversarial_paraphrase_under_kind_omission": 0.0
  },
  "a1_bis_per_stratum_min": 0.0,
  "a1_bis_per_stratum": {
    "override_true": 0.0,
    "override_false": 0.0
  },
  "rebuild_p95_ms": 0,
  "override_seed_provenance": "bootstrapped_in_9.7b",
  "overall_pass": true,
  "blockers": [],
  "tokenizer_choice": "whitespace_ascii_punct_fallback",
  "aggregate_dev": null,
  "aggregate_holdout": null,
  "a1_per_subcategory_dev": null,
  "a1_per_subcategory_holdout": null,
  "a1_bis_per_stratum_dev": null,
  "a1_bis_per_stratum_holdout": null,
  "synonym_expansion_triggered_count": null,
  "holdout_sha256": "",
  "baseline_p95_us": null,
  "partition_run": "",
  "generated_at": ""
}"#;

#[derive(Debug, Serialize, Deserialize)]
pub struct EvalReport {
    pub story: String,
    pub run_ts: String,
    pub bm25_version: String,
    pub corpus_size: usize,
    pub query_count: usize,
    pub recall_at_5: f64,
    pub mrr: f64,
    pub kind_filter_accuracy: f64,
    pub p95_latency_us: u64,
    pub token_budget_p95: u64,
    pub token_budget_p99: u64,
    pub schema_conformance_pass: bool,
    pub name_capability_id_roundtrip_pass: bool,
    pub a1_noun_conflation: f64,
    pub a1_per_subcategory: BTreeMap<String, f64>,
    pub a1_bis_per_stratum_min: f64,
    pub a1_bis_per_stratum: BTreeMap<String, f64>,
    pub rebuild_p95_ms: u64,
    pub override_seed_provenance: String,
    pub overall_pass: bool,
    pub blockers: Vec<String>,
    #[serde(default)]
    pub tokenizer_choice: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aggregate_dev: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aggregate_holdout: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub a1_per_subcategory_dev: Option<BTreeMap<String, f64>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub a1_per_subcategory_holdout: Option<BTreeMap<String, f64>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub a1_bis_per_stratum_dev: Option<BTreeMap<String, f64>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub a1_bis_per_stratum_holdout: Option<BTreeMap<String, f64>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub synonym_expansion_triggered_count: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub holdout_sha256: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub baseline_p95_us: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub partition_run: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub generated_at: Option<String>,
}

impl Default for EvalReport {
    fn default() -> Self {
        serde_json::from_str(REPORT_SCHEMA_JSON).expect("REPORT_SCHEMA_JSON must be valid")
    }
}
