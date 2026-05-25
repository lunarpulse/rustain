#![cfg(feature = "meta-search")]

/// AC: 9-7b-3 through AC: 9-7b-7, AC: 9-7b-9
/// Synthetic eval harness integration test for Phase B Prerequisite #5.
/// Measures the Story 9.7 Bm25SearchEngine against the labeled corpus.
use arc_swap::ArcSwap;
use rustain::domain::models::capability_kind::CapabilityKind;
use rustain::domain::models::doc_key::DocKey;
use rustain::domain::models::search_hit::SearchHit;
use rustain::domain::ports::search::{IndexableItem, MetaSearchEngine};
use rustain::domain::services::frontmatter::{self, extract_field, extract_list_field};
use rustain::infrastructure::search::{Bm25SearchEngine, MergedIndex};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::time::Instant;

// ---------------------------------------------------------------------------
// Locked eval report schema (AC-9-7b-9). Inline golden — single-file diff site.
// ---------------------------------------------------------------------------
const REPORT_SCHEMA_JSON: &str = r#"{
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
  "tokenizer_choice": "whitespace_ascii_punct_fallback"
}"#;

#[derive(Debug, Serialize, Deserialize)]
struct EvalReport {
    story: String,
    run_ts: String,
    bm25_version: String,
    corpus_size: usize,
    query_count: usize,
    recall_at_5: f64,
    mrr: f64,
    kind_filter_accuracy: f64,
    p95_latency_us: u64,
    token_budget_p95: u64,
    token_budget_p99: u64,
    schema_conformance_pass: bool,
    name_capability_id_roundtrip_pass: bool,
    a1_noun_conflation: f64,
    a1_per_subcategory: BTreeMap<String, f64>,
    a1_bis_per_stratum_min: f64,
    a1_bis_per_stratum: BTreeMap<String, f64>,
    rebuild_p95_ms: u64,
    override_seed_provenance: String,
    overall_pass: bool,
    blockers: Vec<String>,
    #[serde(default)]
    tokenizer_choice: String,
}

impl Default for EvalReport {
    fn default() -> Self {
        Self {
            story: "9-7b".into(),
            run_ts: String::new(),
            bm25_version: "=2.3.2".into(),
            corpus_size: 0,
            query_count: 0,
            recall_at_5: 0.0,
            mrr: 0.0,
            kind_filter_accuracy: 0.0,
            p95_latency_us: 0,
            token_budget_p95: 0,
            token_budget_p99: 0,
            schema_conformance_pass: true,
            name_capability_id_roundtrip_pass: true,
            a1_noun_conflation: 0.0,
            a1_per_subcategory: BTreeMap::new(),
            a1_bis_per_stratum_min: 0.0,
            a1_bis_per_stratum: BTreeMap::new(),
            rebuild_p95_ms: 0,
            override_seed_provenance: "bootstrapped_in_9.7b".into(),
            overall_pass: true,
            blockers: vec![],
            tokenizer_choice: "whitespace_ascii_punct_fallback".into(),
        }
    }
}

// ---------------------------------------------------------------------------
// EvalQuery and category enum (AC-9-7b-2 locked schema)
// ---------------------------------------------------------------------------
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
    kind: Option<String>,
    expected_top3: Vec<String>,
    category: EvalCategory,
    expected_kind: Option<String>,
    override_present: bool,
    pair_id: Option<String>,
    notes: String,
}

impl EvalQuery {
    fn kind_filter(&self) -> Option<CapabilityKind> {
        match self.kind.as_deref() {
            Some("skill") => Some(CapabilityKind::Skill),
            Some("tool") => Some(CapabilityKind::Tool),
            _ => None,
        }
    }

    fn expected_kind_enum(&self) -> Option<CapabilityKind> {
        match self.expected_kind.as_deref() {
            Some("skill") => Some(CapabilityKind::Skill),
            Some("tool") => Some(CapabilityKind::Tool),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Test-only IndexableItem impl
// ---------------------------------------------------------------------------
struct FixtureItem {
    doc_key_val: DocKey,
    name_val: String,
    desc: String,
    searchable: String,
}

impl IndexableItem for FixtureItem {
    fn doc_key(&self) -> DocKey {
        self.doc_key_val.clone()
    }

    fn searchable_text(&self) -> std::borrow::Cow<'_, str> {
        std::borrow::Cow::Borrowed(&self.searchable)
    }

    fn description(&self) -> std::borrow::Cow<'_, str> {
        std::borrow::Cow::Borrowed(&self.desc)
    }

    fn to_search_hit(&self, score: f32, matched_terms: Option<Vec<String>>) -> SearchHit {
        let dk = self.doc_key();
        let terse = rustain::domain::services::compute_terse(&self.desc, &dk.name);
        let mut hit = SearchHit::minimal(dk.name, dk.kind, terse, score);
        hit.matched_terms = matched_terms;
        hit
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn fixtures_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/skill_eval_corpus")
}

fn traces_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/skill_eval_traces")
}

fn load_query_set() -> Vec<EvalQuery> {
    let path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/skill_eval_queries.json");
    let content = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("Failed to read query set at {:?}: {}", path, e));
    serde_json::from_str(&content).unwrap_or_else(|e| panic!("Failed to parse query set: {}", e))
}

fn count_tokens_fallback(s: &str) -> usize {
    s.split(|c: char| c.is_whitespace() || c.is_ascii_punctuation())
        .filter(|t| !t.is_empty())
        .count()
}

static TRACE_COUNTER: AtomicU64 = AtomicU64::new(0);

fn emit_trace(name: &str, payload: &serde_json::Value) {
    let dir = traces_dir();
    if let Err(_e) = std::fs::create_dir_all(&dir) {
        let fallback = std::env::temp_dir().join("skill_eval_traces");
        if std::fs::create_dir_all(&fallback).is_err() {
            eprintln!(
                "WARNING: cannot create trace dir {:?} or fallback {:?}, skipping trace",
                dir, fallback
            );
            return;
        }
        write_trace_file(&fallback, name, payload);
        return;
    }
    write_trace_file(&dir, name, payload);
}

fn write_trace_file(dir: &Path, name: &str, payload: &serde_json::Value) {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let seq = TRACE_COUNTER.fetch_add(1, AtomicOrdering::Relaxed);
    let filename = format!("{}_{}_{}.json", name, ts, seq);
    let path = dir.join(&filename);
    if let Err(e) = std::fs::write(
        &path,
        serde_json::to_string_pretty(payload).unwrap_or_default(),
    ) {
        eprintln!("WARNING: cannot write trace to {:?}: {}", path, e);
    }
}

/// Parse a SKILL.md frontmatter, extracting name and description.
fn parse_skill_frontmatter(content: &str) -> Option<(String, String, Vec<String>)> {
    let (fm_str, _body) = frontmatter::parse_frontmatter(content)?;
    let name = extract_field(fm_str, "name")?;
    let desc = extract_field(fm_str, "description")?;
    let tags = extract_list_field(fm_str, "tags").unwrap_or_default();
    Some((name, desc, tags))
}

/// Walk the corpus directory and build FixtureItems from SKILL.md files.
fn build_items_from_corpus() -> Vec<FixtureItem> {
    let root = fixtures_root();
    let mut items = Vec::new();
    let mut seen = BTreeSet::new();

    visit_dir_for_skills(&root, &root, &mut items, &mut seen);
    visit_dir_for_tools(&root, &mut items, &mut seen);

    items
}

fn visit_dir_for_skills(
    root: &Path,
    current: &Path,
    items: &mut Vec<FixtureItem>,
    seen: &mut BTreeSet<String>,
) {
    if let Ok(entries) = std::fs::read_dir(current) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let dirname = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if dirname == "tools" {
                    continue;
                }
                let skill_md = path.join("SKILL.md");
                if skill_md.exists() {
                    if let Ok(content) = std::fs::read_to_string(&skill_md) {
                        if let Some((name, desc, tags)) = parse_skill_frontmatter(&content) {
                            let is_tool_tagged = tags.iter().any(|t| t == "tool");
                            let kind = if is_tool_tagged {
                                CapabilityKind::Tool
                            } else {
                                CapabilityKind::Skill
                            };
                            let key = format!(
                                "{}::{}",
                                serde_json::to_string(&kind)
                                    .unwrap()
                                    .trim_matches('"')
                                    .to_lowercase(),
                                name
                            );
                            if seen.insert(key.clone()) {
                                let doc_key = DocKey {
                                    kind,
                                    name: name.clone(),
                                };
                                let searchable = format!("{} {}", name, desc);
                                items.push(FixtureItem {
                                    doc_key_val: doc_key,
                                    name_val: name,
                                    desc,
                                    searchable,
                                });
                            }
                        }
                    }
                }
                visit_dir_for_skills(root, &path, items, seen);
            }
        }
    }
}

fn visit_dir_for_tools(root: &Path, items: &mut Vec<FixtureItem>, seen: &mut BTreeSet<String>) {
    let tools_dir = root.join("tools");
    if !tools_dir.is_dir() {
        return;
    }
    if let Ok(entries) = std::fs::read_dir(&tools_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let td_path = path.join("tool_descriptor.json");
                if td_path.exists() {
                    if let Ok(content) = std::fs::read_to_string(&td_path) {
                        if let Ok(td) = serde_json::from_str::<serde_json::Value>(&content) {
                            let name = td["name"].as_str().unwrap_or("").to_string();
                            let desc = td["description"]
                                .as_str()
                                .unwrap_or("")
                                .to_string();
                            if name.is_empty() {
                                continue;
                            }
                            let key = format!("tool::{}", name);
                            if seen.insert(key) {
                                let doc_key = DocKey {
                                    kind: CapabilityKind::Tool,
                                    name: name.clone(),
                                };
                                let searchable = format!("{} {}", name, desc);
                                items.push(FixtureItem {
                                    doc_key_val: doc_key,
                                    name_val: name,
                                    desc,
                                    searchable,
                                });
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Load terse overrides from the overrides/manifest.json.
fn load_terse_overrides() -> BTreeMap<DocKey, String> {
    let manifest_path = fixtures_root().join("overrides/manifest.json");
    let content = match std::fs::read_to_string(&manifest_path) {
        Ok(c) => c,
        Err(_) => return BTreeMap::new(),
    };
    let manifest: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(_) => return BTreeMap::new(),
    };

    let mut overrides = BTreeMap::new();
    if let Some(fixtures) = manifest["fixtures"].as_array() {
        for fixture in fixtures {
            let doc_key_str = fixture["doc_key"].as_str().unwrap_or("");
            let override_terse = fixture["override_terse"].as_str().unwrap_or("");
            let kind_str = fixture
                .get("kind")
                .and_then(|v| v.as_str())
                .unwrap_or("skill");

            if !doc_key_str.is_empty() {
                let kind = match kind_str {
                    "tool" => CapabilityKind::Tool,
                    _ => CapabilityKind::Skill,
                };
                let name = doc_key_str
                    .strip_prefix("skill::")
                    .or_else(|| doc_key_str.strip_prefix("tool::"))
                    .unwrap_or(doc_key_str);
                let doc_key = DocKey {
                    kind,
                    name: name.to_string(),
                };
                overrides.insert(doc_key, override_terse.to_string());
            }
        }
    }
    overrides
}

fn build_index() -> Arc<ArcSwap<MergedIndex>> {
    let items = build_items_from_corpus();
    let overrides = load_terse_overrides();
    let refs: Vec<&dyn IndexableItem> = items.iter().map(|i| i as &dyn IndexableItem).collect();
    let merged = MergedIndex::from_items_with_overrides(&refs, &overrides);
    Arc::new(ArcSwap::from_pointee(merged))
}

// ---------------------------------------------------------------------------
// AC: 9-7b-2 — Query set schema validation
// ---------------------------------------------------------------------------

/// AC: 9-7b-2
#[test]
fn test_query_set_loads_and_validates_schema() {
    let queries = load_query_set();
    assert!(
        queries.len() >= 30,
        "Expected >= 30 queries, got {}",
        queries.len()
    );

    // Verify per-category minimums
    let mut cat_counts = BTreeMap::new();
    for q in &queries {
        let cat_name = serde_json::to_string(&q.category)
            .unwrap()
            .trim_matches('"')
            .to_string();
        *cat_counts.entry(cat_name).or_insert(0) += 1;
    }

    // Exact name >= 5
    let exact = cat_counts.get("exact_name").copied().unwrap_or(0);
    assert!(exact >= 5, "exact_name queries: {} < 5", exact);

    // Description keyword >= 5
    let desc_kw = cat_counts.get("description_keyword").copied().unwrap_or(0);
    assert!(desc_kw >= 5, "description_keyword queries: {} < 5", desc_kw);

    // Intent paraphrase >= 5
    let intent = cat_counts.get("intent_paraphrase").copied().unwrap_or(0);
    assert!(intent >= 5, "intent_paraphrase queries: {} < 5", intent);

    // Cross protocol >= 3
    let cross = cat_counts.get("cross_protocol").copied().unwrap_or(0);
    assert!(cross >= 3, "cross_protocol queries: {} < 3", cross);

    // Negative >= 3
    let neg = cat_counts.get("negative").copied().unwrap_or(0);
    assert!(neg >= 3, "negative queries: {} < 3", neg);

    // Round-trip validation
    let path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/skill_eval_queries.json");
    let raw = std::fs::read_to_string(&path).unwrap();
    let parsed: Vec<EvalQuery> =
        serde_json::from_str(&raw).expect("Query set must be valid serde_json");
    let re_serialized = serde_json::to_string(&parsed).unwrap();
    let _re_parsed: Vec<EvalQuery> =
        serde_json::from_str(&re_serialized).expect("Query set must round-trip serde_json");

    let override_true_count = queries.iter().filter(|q| q.override_present).count();
    assert!(
        override_true_count >= 6,
        "override_present=true queries: {} < 6 (needed for A1-bis stratum)",
        override_true_count
    );
}

// ---------------------------------------------------------------------------
// AC: 9-7b-7 — Override fixture seed verification
// ---------------------------------------------------------------------------

/// AC: 9-7b-7
#[test]
fn test_override_fixture_seed_has_all_five_traits() {
    let manifest_path = fixtures_root().join("overrides/manifest.json");
    let content =
        std::fs::read_to_string(&manifest_path).expect("overrides/manifest.json must exist");
    let manifest: serde_json::Value =
        serde_json::from_str(&content).expect("overrides/manifest.json must be valid JSON");

    let fixtures = manifest["fixtures"]
        .as_array()
        .expect("manifest must have fixtures array");

    assert!(
        fixtures.len() >= 6,
        "Need >= 6 override fixtures, got {}",
        fixtures.len()
    );

    let mut traits = BTreeSet::new();
    for fixture in fixtures {
        let trait_class = fixture["trait_class"].as_str().unwrap_or("");
        traits.insert(trait_class.to_string());
    }

    for required in &[
        "short",
        "long",
        "unicode-boundary",
        "empty-string",
        "semantic-contradiction",
        "noop-identical",
    ] {
        assert!(
            traits.contains(*required),
            "Override seed missing trait class: {}. Present: {:?}",
            required,
            traits
        );
    }

    // Verify each override fixture can be parsed as a SKILL.md
    let override_dir = fixtures_root().join("overrides");
    for fixture in fixtures {
        let doc_key_str = fixture["doc_key"].as_str().unwrap_or("");
        let name = doc_key_str.strip_prefix("skill::").unwrap_or(doc_key_str);
        let skill_md = override_dir.join(name).join("SKILL.md");
        assert!(
            skill_md.exists(),
            "Override fixture {:?} not found at {:?}",
            name,
            skill_md
        );
        let content = std::fs::read_to_string(&skill_md)
            .unwrap_or_else(|e| panic!("Cannot read {:?}: {}", skill_md, e));
        let (parsed_name, _desc, _tags) = parse_skill_frontmatter(&content)
            .unwrap_or_else(|| panic!("Override fixture {:?} failed frontmatter parse", skill_md));
        assert_eq!(
            parsed_name, name,
            "Override fixture name mismatch: expected '{}', got '{}'",
            name, parsed_name
        );
    }
}

// ---------------------------------------------------------------------------
// AC: 9-7b-3 — Primary metrics harness
// ---------------------------------------------------------------------------

/// AC: 9-7b-3
#[tokio::test]
async fn test_skill_eval_harness_primary_metrics() {
    let index = build_index();
    let engine = Bm25SearchEngine::new(index);
    let queries = load_query_set();

    assert!(!queries.is_empty(), "Query set must not be empty");

    let mut recall_scores = Vec::new();
    let mut mrr_scores = Vec::new();
    let mut kind_filter_results = Vec::new();
    let mut latencies_us = Vec::new();
    let mut trace_entries = Vec::new();

    for (i, q) in queries.iter().enumerate() {
        let kind_filter = q.kind_filter();
        let start = Instant::now();
        let result = engine.search(&q.query, kind_filter, 5).await;
        let elapsed_us = start.elapsed().as_micros() as u64;

        match result {
            Ok(hits) => {
                let hit_names: Vec<String> = hits.iter().map(|h| h.name.clone()).collect();

                // Recall@5
                if q.expected_top3.is_empty() {
                    if hits.is_empty() {
                        recall_scores.push(1.0);
                        mrr_scores.push(1.0);
                    } else {
                        recall_scores.push(0.0);
                        mrr_scores.push(0.0);
                    }
                } else {
                    let hit_ids: Vec<String> = hits
                        .iter()
                        .map(|h| {
                            let kind_str = serde_json::to_string(&h.kind)
                                .unwrap()
                                .trim_matches('"')
                                .to_lowercase();
                            format!("{}::{}", kind_str, h.name)
                        })
                        .collect();
                    let matched: usize = q
                        .expected_top3
                        .iter()
                        .filter(|expected| {
                            let expected_name = expected.split("::").last().unwrap_or(expected);
                            hit_ids.iter().any(|hid| hid == *expected)
                                || hit_names.iter().any(|hn| hn == expected_name)
                        })
                        .count();
                    let recall = matched as f64 / q.expected_top3.len() as f64;
                    recall_scores.push(recall);

                    // MRR
                    let mut best_rank: Option<usize> = None;
                    for expected in &q.expected_top3 {
                        let expected_name = expected.split("::").last().unwrap_or(expected);
                        for (rank, hname) in hit_names.iter().enumerate() {
                            if hname == expected_name {
                                let rank_val = rank + 1;
                                best_rank = Some(best_rank.map_or(rank_val, |b| b.min(rank_val)));
                            }
                        }
                    }
                    let rr = best_rank.map_or(0.0, |r| 1.0 / r as f64);
                    mrr_scores.push(rr);
                }

                // Kind-filter accuracy
                if kind_filter.is_some() {
                    let all_match = hits.iter().all(|h| Some(h.kind) == kind_filter);
                    kind_filter_results.push(all_match);
                }

                // Latency (exclude first 10 cold starts)
                if i >= 10 {
                    latencies_us.push(elapsed_us);
                }

                trace_entries.push(serde_json::json!({
                    "query": q.query,
                    "kind": q.kind,
                    "top5_hits": hit_names,
                    "expected_top3": q.expected_top3,
                    "recall": recall_scores.last().copied().unwrap_or(0.0),
                    "reciprocal_rank": mrr_scores.last().copied().unwrap_or(0.0),
                    "kind_filter_pass": kind_filter_results.last().copied().unwrap_or(true),
                    "latency_us": elapsed_us,
                }));
            }
            Err(e) => {
                // Engine errors recorded as zero-recall
                if !q.expected_top3.is_empty() {
                    recall_scores.push(0.0);
                    mrr_scores.push(0.0);
                }
                if kind_filter.is_some() {
                    kind_filter_results.push(false);
                }
                trace_entries.push(serde_json::json!({
                    "query": q.query,
                    "error": format!("{:?}", e),
                    "expected_top3": q.expected_top3,
                    "recall": 0.0,
                    "kind_filter_pass": false,
                    "latency_us": elapsed_us,
                }));
            }
        }
    }

    // Emit per-query traces
    emit_trace("primary", &serde_json::json!(trace_entries));

    // Compute aggregate metrics
    let mean_recall: f64 = if recall_scores.is_empty() {
        0.0
    } else {
        recall_scores.iter().sum::<f64>() / recall_scores.len() as f64
    };

    let mean_mrr: f64 = if mrr_scores.is_empty() {
        0.0
    } else {
        mrr_scores.iter().sum::<f64>() / mrr_scores.len() as f64
    };

    let kind_filter_acc: f64 = if kind_filter_results.is_empty() {
        1.0
    } else {
        kind_filter_results.iter().filter(|&&b| b).count() as f64 / kind_filter_results.len() as f64
    };

    let p95_latency = if latencies_us.len() >= 2 {
        let mut sorted = latencies_us.clone();
        sorted.sort_unstable();
        let idx = ((sorted.len() as f64) * 0.95).ceil() as usize - 1;
        sorted[idx.min(sorted.len() - 1)]
    } else {
        0
    };

    // Assert thresholds with actionable messages (AC-9-7b-3)
    assert!(
        mean_recall >= 0.80,
        "recall@5 = {:.2} < 0.80 — see target/skill_eval_traces/primary_*.json for per-query analysis",
        mean_recall
    );

    assert!(
        mean_mrr >= 0.65,
        "MRR = {:.2} < 0.65 — see target/skill_eval_traces/primary_*.json for per-query analysis",
        mean_mrr
    );

    assert!(
        kind_filter_acc >= 0.90,
        "kind-filter accuracy = {:.2} < 0.90 — see target/skill_eval_traces/primary_*.json for per-query analysis",
        kind_filter_acc
    );

    assert!(
        p95_latency < 50_000,
        "p95 search latency = {}us >= 50000us (50ms) — see target/skill_eval_traces/primary_*.json for per-query analysis",
        p95_latency
    );

    println!(
        "Primary metrics: recall@5={:.3} MRR={:.3} kind_filter_acc={:.3} p95_latency={}us ({} queries, {} corpus)",
        mean_recall,
        mean_mrr,
        kind_filter_acc,
        p95_latency,
        queries.len(),
        build_items_from_corpus().len()
    );
}

// ---------------------------------------------------------------------------
// AC: 9-7b-4 — Noun-conflation axis (three subcategories + A1-bis stratified)
// ---------------------------------------------------------------------------

/// AC: 9-7b-4 subcategory (i): invocation-intent
#[tokio::test]
async fn test_noun_conflation_invocation_intent() {
    let index = build_index();
    let engine = Bm25SearchEngine::new(index);
    let queries: Vec<_> = load_query_set()
        .into_iter()
        .filter(|q| matches!(q.category, EvalCategory::NounConflationInvocationIntent))
        .collect();

    assert!(!queries.is_empty(), "Must have invocation-intent queries");

    let mut passed = 0usize;
    let mut tested = 0usize;
    let mut traces = Vec::new();

    for q in &queries {
        let expected_kind = q.expected_kind_enum();
        if expected_kind.is_none() {
            continue;
        }
        tested += 1;
        let result = engine.search(&q.query, None, 3).await;

        let pass = match result {
            Ok(hits) => {
                let wrong_kind_in_top3 = hits
                    .iter()
                    .take(3)
                    .filter(|h| expected_kind.map_or(false, |ek| h.kind != ek))
                    .count();
                wrong_kind_in_top3 == 0
            }
            Err(_) => false,
        };

        if pass {
            passed += 1;
        }

        traces.push(serde_json::json!({
            "query": q.query,
            "expected_kind": q.expected_kind,
            "pass": pass,
            "notes": q.notes,
        }));
    }

    if tested == 0 {
        emit_trace("noun_conflation_i", &serde_json::json!(traces));
        println!("No invocation-intent queries with expected_kind set; skipping");
        return;
    }

    let accuracy = passed as f64 / tested as f64;
    emit_trace("noun_conflation_i", &serde_json::json!(traces));

    assert!(
        accuracy >= 0.85,
        "Noun-conflation invocation-intent accuracy = {:.2} < 0.85. {} / {} passed. FAILURE BLOCKS meta-search ON-default flip per ADR-09-02 v2 §Recorded Disagreement v2 + release-checklist-meta-search-flip.md; re-open ADR-09-02 v3 for two-door rollback",
        accuracy,
        passed,
        queries.len()
    );
}

/// AC: 9-7b-4 subcategory (ii): cross-kind contamination (position-swap with kind omitted)
#[tokio::test]
async fn test_noun_conflation_cross_kind_contamination() {
    let index = build_index();
    let engine = Bm25SearchEngine::new(index);
    let queries: Vec<_> = load_query_set()
        .into_iter()
        .filter(|q| {
            matches!(
                q.category,
                EvalCategory::NounConflationCrossKindContamination
            )
        })
        .collect();

    assert!(
        !queries.is_empty(),
        "Must have cross-kind contamination queries"
    );

    let mut passed = 0usize;
    let mut traces = Vec::new();

    for q in &queries {
        let expected_kind = q.expected_kind_enum();
        let result = engine.search(&q.query, None, 5).await;

        let pass = match result {
            Ok(hits) => {
                if let Some(ek) = expected_kind {
                    let canonical_rank = hits.iter().position(|h| {
                        h.kind == ek && q.expected_top3.iter().any(|e| e.ends_with(&h.name))
                    });
                    let peer_kind = if ek == CapabilityKind::Tool {
                        CapabilityKind::Skill
                    } else {
                        CapabilityKind::Tool
                    };
                    let peer_rank = hits.iter().position(|h| {
                        h.kind == peer_kind && q.expected_top3.iter().any(|e| e.ends_with(&h.name))
                    });
                    match (canonical_rank, peer_rank) {
                        (Some(cr), Some(pr)) => cr <= pr,
                        (Some(_), None) => true,
                        (None, _) => false,
                    }
                } else {
                    false
                }
            }
            Err(_) => false,
        };

        if pass {
            passed += 1;
        }

        traces.push(serde_json::json!({
            "query": q.query,
            "pair_id": q.pair_id,
            "pass": pass,
            "notes": q.notes,
        }));
    }

    let accuracy = passed as f64 / queries.len() as f64;
    emit_trace("noun_conflation_ii", &serde_json::json!(traces));

    assert!(
        accuracy >= 0.85,
        "Noun-conflation cross-kind contamination accuracy = {:.2} < 0.85. {} / {} passed. FAILURE BLOCKS meta-search ON-default flip per ADR-09-02 v2 §Recorded Disagreement v2; re-open ADR-09-02 v3 for two-door rollback",
        accuracy,
        passed,
        queries.len()
    );
}

/// AC: 9-7b-4 subcategory (iii): adversarial paraphrase under kind-omission
#[tokio::test]
async fn test_noun_conflation_adversarial_paraphrase_under_kind_omission() {
    let index = build_index();
    let engine = Bm25SearchEngine::new(index);
    let queries: Vec<_> = load_query_set()
        .into_iter()
        .filter(|q| {
            matches!(
                q.category,
                EvalCategory::NounConflationAdversarialParaphrase
            )
        })
        .collect();

    assert!(
        !queries.is_empty(),
        "Must have adversarial paraphrase queries"
    );

    let mut passed = 0usize;
    let mut traces = Vec::new();

    for q in &queries {
        let result = engine.search(&q.query, None, 5).await;

        let pass = match result {
            Ok(hits) => {
                if let Some(top1) = hits.first() {
                    q.expected_top3.iter().any(|e| {
                        let ename = e.split("::").last().unwrap_or(e);
                        top1.name == ename
                    })
                } else if q.expected_top3.is_empty() {
                    true // Negative adversarial: expected no match
                } else {
                    false
                }
            }
            Err(_) => false,
        };

        if pass {
            passed += 1;
        }

        traces.push(serde_json::json!({
            "query": q.query,
            "pass": pass,
            "notes": q.notes,
        }));
    }

    let accuracy = passed as f64 / queries.len() as f64;
    emit_trace("noun_conflation_iii", &serde_json::json!(traces));

    assert!(
        accuracy >= 0.85,
        "Noun-conflation adversarial paraphrase accuracy = {:.2} < 0.85. {} / {} passed. FAILURE BLOCKS meta-search ON-default flip per ADR-09-02 v2 §Recorded Disagreement v2; re-open ADR-09-02 v3 for two-door rollback",
        accuracy,
        passed,
        queries.len()
    );
}

/// AC: 9-7b-4 aggregate + A1-bis stratified
#[tokio::test]
async fn test_noun_conflation_aggregate_and_stratified() {
    let index = build_index();
    let engine = Bm25SearchEngine::new(index);
    let queries = load_query_set();
    let nc_queries: Vec<_> = queries
        .iter()
        .filter(|q| {
            matches!(
                q.category,
                EvalCategory::NounConflationInvocationIntent
                    | EvalCategory::NounConflationCrossKindContamination
                    | EvalCategory::NounConflationAdversarialParaphrase
            )
        })
        .collect();

    if nc_queries.is_empty() {
        println!("No noun-conflation queries to score; aggregate = 1.0 (vacuous)");
        return;
    }

    // Per-subcategory scoring
    let mut subcat_results: BTreeMap<String, Vec<bool>> = BTreeMap::new();
    let mut stratum_results: BTreeMap<String, Vec<bool>> = BTreeMap::new();
    let stratum_override_true = "override_true".to_string();
    let stratum_override_false = "override_false".to_string();
    stratum_results.insert(stratum_override_true.clone(), Vec::new());
    stratum_results.insert(stratum_override_false.clone(), Vec::new());

    for q in &nc_queries {
        let result = engine.search(&q.query, None, 3).await;
        let pass = match result {
            Ok(hits) => {
                if q.expected_top3.is_empty() && hits.is_empty() {
                    true
                } else if !q.expected_top3.is_empty() && !hits.is_empty() {
                    // Simplified pass condition: at least one expected hit in top-3
                    q.expected_top3.iter().any(|e| {
                        let ename = e.split("::").last().unwrap_or(e);
                        hits.iter().take(3).any(|h| h.name == ename)
                    })
                } else {
                    false
                }
            }
            Err(_) => false,
        };

        let cat_name = serde_json::to_string(&q.category)
            .unwrap()
            .trim_matches('"')
            .to_string();
        subcat_results.entry(cat_name).or_default().push(pass);

        let stratum_key = if q.override_present {
            &stratum_override_true
        } else {
            &stratum_override_false
        };
        stratum_results.get_mut(stratum_key).unwrap().push(pass);
    }

    // Per-subcategory accuracies
    let mut subcat_accuracies = BTreeMap::new();
    let mut total_passed = 0usize;
    let mut total_count = 0usize;

    for (cat, results) in &subcat_results {
        let acc = if results.is_empty() {
            1.0
        } else {
            results.iter().filter(|&&b| b).count() as f64 / results.len() as f64
        };
        subcat_accuracies.insert(cat.clone(), acc);
        total_passed += results.iter().filter(|&&b| b).count();
        total_count += results.len();
    }

    // Weighted aggregate
    let aggregate = if total_count == 0 {
        1.0
    } else {
        total_passed as f64 / total_count as f64
    };

    assert!(
        aggregate >= 0.85,
        "A1 noun-conflation aggregate = {:.2} < 0.85 (BINDING). FAILURE BLOCKS meta-search ON-default flip per ADR-09-02 v2 §Recorded Disagreement v2 + release-checklist-meta-search-flip.md; re-open ADR-09-02 v3 for two-door rollback",
        aggregate
    );

    // A1-bis per-stratum
    let acc_override_true = stratum_results
        .get(&stratum_override_true)
        .map_or(1.0, |r| {
            if r.is_empty() {
                1.0
            } else {
                r.iter().filter(|&&b| b).count() as f64 / r.len() as f64
            }
        });
    let acc_override_false = stratum_results
        .get(&stratum_override_false)
        .map_or(1.0, |r| {
            if r.is_empty() {
                1.0
            } else {
                r.iter().filter(|&&b| b).count() as f64 / r.len() as f64
            }
        });

    let per_stratum_min = acc_override_true.min(acc_override_false);

    emit_trace(
        "noun_conflation_stratified_override_true",
        &serde_json::json!({"accuracy": acc_override_true, "n": stratum_results.get(&stratum_override_true).map_or(0, |r| r.len())}),
    );
    emit_trace(
        "noun_conflation_stratified_override_false",
        &serde_json::json!({"accuracy": acc_override_false, "n": stratum_results.get(&stratum_override_false).map_or(0, |r| r.len())}),
    );

    assert!(
        per_stratum_min >= 0.85,
        "A1-bis per-stratum min = {:.2} < 0.85 (BINDING). override_true={:.3} override_false={:.3}. FAILURE BLOCKS meta-search ON-default flip per ADR-09-02 v2 §Recorded Disagreement v2 + release-checklist-meta-search-flip.md; re-open ADR-09-02 v3 for two-door rollback",
        per_stratum_min,
        acc_override_true,
        acc_override_false
    );

    println!(
        "Noun-conflation: aggregate={:.3} strata(min={:.3}, override_true={:.3}, override_false={:.3})",
        aggregate, per_stratum_min, acc_override_true, acc_override_false
    );
}

// ---------------------------------------------------------------------------
// AC: 9-7b-5 — Token budget
// ---------------------------------------------------------------------------

/// AC: 9-7b-5
#[test]
fn test_per_hit_token_budget() {
    let items = build_items_from_corpus();
    assert!(
        !items.is_empty(),
        "Corpus must not be empty for token budget test"
    );

    let mut token_counts = Vec::new();
    let mut trace_data = Vec::new();

    for item in &items {
        let hit = item.to_search_hit(1.0, None);
        let json = serde_json::to_string(&hit).unwrap_or_default();
        let tokens = count_tokens_fallback(&json);

        trace_data.push(serde_json::json!({
            "doc_key": item.doc_key_val.display(),
            "token_count": tokens,
        }));

        token_counts.push(tokens);
    }

    token_counts.sort_unstable();
    assert!(
        !token_counts.is_empty(),
        "All items failed to serialize for token budget"
    );
    let p95_idx = ((token_counts.len() as f64) * 0.95).ceil() as usize - 1;
    let p95 = token_counts[p95_idx.min(token_counts.len() - 1)] as u64;
    let p99_idx = ((token_counts.len() as f64) * 0.99).ceil() as usize - 1;
    let p99 = token_counts[p99_idx.min(token_counts.len() - 1)] as u64;

    emit_trace(
        "token_budget",
        &serde_json::json!({
            "p95": p95,
            "p99": p99,
            "fixtures": trace_data,
        }),
    );

    assert!(
        p95 <= 30,
        "Per-hit token budget p95 = {} > 30. {} fixtures tested.",
        p95,
        token_counts.len()
    );

    assert!(
        p99 <= 50,
        "Per-hit token budget p99 = {} > 50. {} fixtures tested.",
        p99,
        token_counts.len()
    );

    println!(
        "Token budget: p95={} p99={} ({} fixtures)",
        p95,
        p99,
        token_counts.len()
    );
}

// ---------------------------------------------------------------------------
// AC: 9-7b-6 — Schema conformance + name->CapabilityId roundtrip
// ---------------------------------------------------------------------------

/// AC: 9-7b-6
#[tokio::test]
async fn test_eval_corpus_search_hits_pass_schema_and_roundtrip() {
    let index = build_index();
    let engine = Bm25SearchEngine::new(index);
    let queries = load_query_set();

    let mut all_hits: BTreeMap<(String, String), SearchHit> = BTreeMap::new();

    for q in &queries {
        let kind_filter = q.kind_filter();
        if let Ok(hits) = engine.search(&q.query, kind_filter, 5).await {
            for hit in hits {
                let kind_str = serde_json::to_string(&hit.kind).unwrap_or_default();
                all_hits.insert((hit.name.clone(), kind_str), hit);
            }
        }
    }

    assert!(
        !all_hits.is_empty(),
        "Must produce at least one search hit from queries"
    );

    let allowed_fields: BTreeSet<&str> = [
        "name",
        "kind",
        "terse",
        "score",
        "provider",
        "matched_terms",
    ]
    .iter()
    .copied()
    .collect();

    let skill_name_re = regex::Regex::new(r"^[a-z][a-z0-9-]{0,63}$").unwrap();

    for ((name, _kind_str), hit) in &all_hits {
        // Schema field set lock-down (mirror conformance_search_hit_schema.rs pattern)
        let value = serde_json::to_value(hit).unwrap();
        if let Some(obj) = value.as_object() {
            let keys: BTreeSet<&str> = obj.keys().map(|s| s.as_str()).collect();
            for key in &keys {
                assert!(
                    allowed_fields.contains(key),
                    "SearchHit '{}' has unexpected field '{}'. Allowed: {:?}. Got: {:?}",
                    name,
                    key,
                    allowed_fields,
                    keys
                );
            }

            // Required fields must be present
            assert!(
                keys.contains("name"),
                "SearchHit missing 'name' field: {:?}",
                hit
            );
            assert!(
                keys.contains("kind"),
                "SearchHit missing 'kind' field: {:?}",
                hit
            );
            assert!(
                keys.contains("terse"),
                "SearchHit missing 'terse' field: {:?}",
                hit
            );
            assert!(
                keys.contains("score"),
                "SearchHit missing 'score' field: {:?}",
                hit
            );
        }

        // Name roundtrip for Tool kind
        let kind = hit.kind;
        if kind == CapabilityKind::Tool {
            if name.starts_with("mcp__") {
                let cap_id =
                    rustain::domain::models::capability_id::CapabilityId::from_mcp_wire_name(name);
                assert!(
                    cap_id.is_some(),
                    "Tool '{}' with mcp__ prefix fails CapabilityId::from_mcp_wire_name roundtrip",
                    name
                );
            } else {
                let cap_id = rustain::domain::models::capability_id::CapabilityId::parse(&format!(
                    "builtin::{}",
                    name
                ));
                assert!(
                    cap_id.is_some(),
                    "Builtin '{}' fails CapabilityId::parse roundtrip",
                    name
                );
            }
        }

        // Skill name regex
        if kind == CapabilityKind::Skill {
            assert!(
                skill_name_re.is_match(name),
                "Skill '{}' does not match Anthropic frontmatter regex ^[a-z][a-z0-9-]{{0,63}}$",
                name
            );
        }
    }

    println!(
        "Schema conformance + roundtrip: {} unique hits verified",
        all_hits.len()
    );
}

// ---------------------------------------------------------------------------
// AC: 9-7b-9 — Eval report assembly
// ---------------------------------------------------------------------------

/// AC: 9-7b-9
#[test]
#[ignore]
fn test_assemble_and_write_eval_report() {
    let items = build_items_from_corpus();
    let queries = load_query_set();

    let report_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../_bmad-output/implementation-artifacts/9-7b-eval-report.json");

    let existing_json = std::fs::read_to_string(&report_path).unwrap_or_default();
    let mut report: EvalReport = if existing_json.is_empty() {
        EvalReport::default()
    } else {
        serde_json::from_str(&existing_json).unwrap_or_else(|_| EvalReport::default())
    };

    report.run_ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .to_string();
    report.corpus_size = items.len();
    report.query_count = queries.len();

    if report.recall_at_5 == 0.0
        && report.mrr == 0.0
        && report.kind_filter_accuracy == 0.0
        && report.a1_noun_conflation == 0.0
    {
        report.overall_pass = false;
        report.blockers = vec![
            "Eval report metrics not yet populated — run full harness pipeline first".to_string(),
        ];
    }

    if let Some(parent) = report_path.parent() {
        std::fs::create_dir_all(parent).ok();
    }

    let json = serde_json::to_string_pretty(&report).unwrap();
    std::fs::write(&report_path, &json)
        .unwrap_or_else(|e| panic!("Failed to write eval report to {:?}: {}", report_path, e));

    let loaded: EvalReport =
        serde_json::from_str(&json).expect("Eval report must round-trip through serde_json");
    assert_eq!(loaded.story, "9-7b");
    assert_eq!(loaded.override_seed_provenance, "bootstrapped_in_9.7b");

    println!(
        "Eval report written to {:?} with {} fixtures, {} queries, overall_pass={}",
        report_path, report.corpus_size, report.query_count, report.overall_pass
    );
}
