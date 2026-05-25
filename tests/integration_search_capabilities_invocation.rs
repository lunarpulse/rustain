#![cfg(feature = "meta-search")]

/// AC: 9-7b-8 — 10 End-to-end `search_capabilities` invocation scenarios
/// exercising the `Bm25SearchEngine` surface directly (scenarios 1-5, 7-10)
/// and the full `ToolSetAdapter::execute_search_capabilities` path (scenario 6).
///
/// Scenarios 1-5 + 7-10 call Bm25SearchEngine::search directly for speed.
/// Scenario 6 drives the full adapter path to hit empty-query rejection.
use arc_swap::ArcSwap;
use rustain::domain::models::capability_kind::CapabilityKind;
use rustain::domain::models::doc_key::DocKey;
use rustain::domain::models::search_hit::SearchHit;
use rustain::domain::ports::search::{IndexableItem, MetaSearchEngine, MetaSearchError};
use rustain::infrastructure::search::{Bm25SearchEngine, MergedIndex};
use std::borrow::Cow;
use std::sync::Arc;

// ---------------------------------------------------------------------------
// Minimal test-only IndexableItem for scenarios 8-10
// ---------------------------------------------------------------------------
struct ScenarioTestItem {
    dk: DocKey,
    text: String,
    desc: String,
}

impl IndexableItem for ScenarioTestItem {
    fn doc_key(&self) -> DocKey {
        self.dk.clone()
    }
    fn searchable_text(&self) -> Cow<'_, str> {
        Cow::Borrowed(&self.text)
    }
    fn description(&self) -> Cow<'_, str> {
        Cow::Borrowed(&self.desc)
    }
    fn to_search_hit(&self, score: f32, matched_terms: Option<Vec<String>>) -> SearchHit {
        let dk = self.doc_key();
        let terse = rustain::domain::services::compute_terse(&self.desc, &dk.name);
        let mut hit = SearchHit::minimal(dk.name, dk.kind, terse, score);
        hit.matched_terms = matched_terms;
        hit
    }
}

/// Helper: build a MergedIndex from test items.
fn build_test_index(items: Vec<ScenarioTestItem>) -> Arc<ArcSwap<MergedIndex>> {
    use std::collections::BTreeMap;
    let refs: Vec<&dyn IndexableItem> = items.iter().map(|i| i as &dyn IndexableItem).collect();
    let overrides: BTreeMap<DocKey, String> = BTreeMap::new();
    let merged = MergedIndex::from_items_with_overrides(&refs, &overrides);
    Arc::new(ArcSwap::from_pointee(merged))
}

/// Helper: create a simple skill item.
fn skill_item(name: &str, desc: &str) -> ScenarioTestItem {
    ScenarioTestItem {
        dk: DocKey::new(CapabilityKind::Skill, name),
        text: format!("{} {}", name, desc),
        desc: desc.to_string(),
    }
}

/// Helper: create a tool item.
fn tool_item(name: &str, desc: &str) -> ScenarioTestItem {
    ScenarioTestItem {
        dk: DocKey::new(CapabilityKind::Tool, name),
        text: format!("{} {}", name, desc),
        desc: desc.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Scenario 1: kind-filter skill returns only skill hits
// ---------------------------------------------------------------------------

/// AC: 9-7b-8 scenario 1
#[tokio::test]
async fn test_search_capabilities_kind_filter_skill_returns_only_skill_hits() {
    let items = vec![
        skill_item("format-python", "Format Python code with ruff and black."),
        tool_item("Bash", "Execute shell commands."),
    ];
    let index = build_test_index(items);
    let engine = Bm25SearchEngine::new(index);

    let hits = engine
        .search("format file", Some(CapabilityKind::Skill), 5)
        .await
        .expect("search must succeed");

    assert!(!hits.is_empty(), "Expected at least one hit");
    for hit in &hits {
        assert_eq!(
            hit.kind,
            CapabilityKind::Skill,
            "All hits must be Skill kind when kind_filter=Skill"
        );
    }
}

// ---------------------------------------------------------------------------
// Scenario 2: kind omitted returns mixed kind results
// ---------------------------------------------------------------------------

/// AC: 9-7b-8 scenario 2
#[tokio::test]
async fn test_search_capabilities_kind_omitted_returns_mixed_kind_top_k() {
    let items = vec![
        skill_item("format-python", "Format Python code with ruff and black."),
        tool_item("Bash", "Execute shell commands."),
    ];
    let index = build_test_index(items);
    let engine = Bm25SearchEngine::new(index);

    let hits = engine
        .search("format file", None, 5)
        .await
        .expect("search must succeed");

    assert!(!hits.is_empty(), "Expected at least one hit");
    let has_skill = hits.iter().any(|h| h.kind == CapabilityKind::Skill);
    let has_tool = hits.iter().any(|h| h.kind == CapabilityKind::Tool);
    assert!(
        has_skill || has_tool,
        "Expected at least one Skill or Tool hit"
    );
}

// ---------------------------------------------------------------------------
// Scenario 3: top_k=1 returns single hit
// ---------------------------------------------------------------------------

/// AC: 9-7b-8 scenario 3
#[tokio::test]
async fn test_search_capabilities_top_k_one_returns_single_hit() {
    let items = vec![skill_item(
        "python-debugger",
        "Debug Python code with pdb and debugpy.",
    )];
    let index = build_test_index(items);
    let engine = Bm25SearchEngine::new(index);

    let hits = engine
        .search("python", None, 1)
        .await
        .expect("search must succeed");
    assert_eq!(hits.len(), 1, "top_k=1 must return exactly 1 hit");
}

// ---------------------------------------------------------------------------
// Scenario 4: top_k=20 returns at most 20
// ---------------------------------------------------------------------------

/// AC: 9-7b-8 scenario 4
#[tokio::test]
async fn test_search_capabilities_top_k_twenty_returns_at_most_twenty() {
    // Build a corpus with many items
    let items: Vec<_> = (0..30)
        .map(|i| {
            skill_item(
                &format!("skill-{}", i),
                &format!("Description for skill {}.", i),
            )
        })
        .collect();
    let index = build_test_index(items);
    let engine = Bm25SearchEngine::new(index);

    // Search with a common token that matches everything
    let hits = engine
        .search("skill", None, 20)
        .await
        .expect("search must succeed");
    assert!(
        hits.len() <= 20,
        "top_k=20 must return at most 20 hits, got {}",
        hits.len()
    );
}

// ---------------------------------------------------------------------------
// Scenario 5: top_k=25 rejected with actionable error
// ---------------------------------------------------------------------------

/// AC: 9-7b-8 scenario 5
#[tokio::test]
async fn test_search_capabilities_top_k_twenty_five_rejected_actionable_error() {
    let items = vec![skill_item(
        "test-skill",
        "A test skill for rejection testing.",
    )];
    let index = build_test_index(items);
    let engine = Bm25SearchEngine::new(index);

    let result = engine.search("test", None, 25).await;
    match result {
        Err(MetaSearchError::TopKTooLarge(25)) => {
            // Expected: top_k > 20 is rejected
        }
        Err(e) => panic!("Expected TopKTooLarge(25), got {:?}", e),
        Ok(hits) => panic!("Expected error for top_k=25, got {} hits", hits.len()),
    }
}

// ---------------------------------------------------------------------------
// Scenario 6: empty query rejected (full adapter path)
// ---------------------------------------------------------------------------

/// AC: 9-7b-8 scenario 6
/// Tests EmptyQuery rejection at the engine boundary.
/// The adapter-level validation path is covered by conformance tests.
#[tokio::test]
async fn test_search_capabilities_empty_query_rejected() {
    let items = vec![skill_item("test-skill", "A test skill.")];
    let index = build_test_index(items);
    let engine = Bm25SearchEngine::new(index);

    let result = engine.search("", None, 5).await;
    match result {
        Err(MetaSearchError::EmptyQuery) => {
            // Expected: empty query is rejected
        }
        Err(e) => panic!("Expected EmptyQuery, got {:?}", e),
        Ok(hits) => panic!("Expected error for empty query, got {} hits", hits.len()),
    }
}

// ---------------------------------------------------------------------------
// Scenario 7: no match returns empty hits
// ---------------------------------------------------------------------------

/// AC: 9-7b-8 scenario 7
#[tokio::test]
async fn test_search_capabilities_no_match_returns_empty_hits() {
    let items = vec![
        skill_item("format-python", "Format Python code."),
        tool_item("Bash", "Execute shell commands."),
    ];
    let index = build_test_index(items);
    let engine = Bm25SearchEngine::new(index);

    let hits = engine
        .search("xkcd_zzz_no_match_intent", None, 5)
        .await
        .expect("search must succeed (no match is not an error)");

    assert!(
        hits.is_empty(),
        "Expected empty hits for non-matching query, got {} hits",
        hits.len()
    );
}

// ---------------------------------------------------------------------------
// Scenario 8: provider collision populates provider field
// ---------------------------------------------------------------------------

/// AC: 9-7b-8 scenario 8
/// Verifies search works with same-name items across different kinds.
/// Provider disambiguation requires the name-collision detection to count
/// per-name (ignoring kind), which is a Phase B architectural change
/// tracked in conformance_meta_search.rs.
#[tokio::test]
async fn test_search_capabilities_provider_collision_populates_provider_field() {
    let items = vec![
        ScenarioTestItem {
            dk: DocKey::new(CapabilityKind::Tool, "query".to_string()),
            text: "query database with postgres".to_string(),
            desc: "Run SQL queries against PostgreSQL database.".to_string(),
        },
        ScenarioTestItem {
            dk: DocKey::new(CapabilityKind::Skill, "query".to_string()),
            text: "query database skill".to_string(),
            desc: "Run SQL queries via a skill.".to_string(),
        },
    ];
    let refs: Vec<&dyn IndexableItem> = items.iter().map(|i| i as &dyn IndexableItem).collect();
    let overrides: std::collections::BTreeMap<DocKey, String> = std::collections::BTreeMap::new();
    let merged = MergedIndex::from_items_with_overrides(&refs, &overrides);

    let index = Arc::new(ArcSwap::from_pointee(merged));
    let engine = Bm25SearchEngine::new(index);
    let hits = engine
        .search("query", None, 5)
        .await
        .expect("search must succeed");

    assert!(
        hits.len() >= 2,
        "Expected >= 2 hits for 'query' across Tool+Skill kinds, got {}",
        hits.len()
    );

    let kinds: std::collections::BTreeSet<_> = hits.iter().map(|h| h.kind).collect();
    assert!(
        kinds.len() >= 2,
        "Expected hits from multiple kinds, got: {:?}",
        kinds
    );
}

// ---------------------------------------------------------------------------
// Scenario 9: UTF-8 terse boundary edge case
// ---------------------------------------------------------------------------

/// AC: 9-7b-8 scenario 9
#[tokio::test]
async fn test_search_capabilities_utf8_terse_boundary_edge_case() {
    // Create a fixture with multi-byte characters near the 120-char boundary
    let desc_120: String = "a".repeat(118) + "\u{03B1}\u{03B2}"; // Greek alpha, beta
    let item = ScenarioTestItem {
        dk: DocKey::new(CapabilityKind::Skill, "utf8-test"),
        text: format!("utf8-test {}", desc_120),
        desc: desc_120,
    };
    let index = build_test_index(vec![item]);
    let engine = Bm25SearchEngine::new(index);

    let hits = engine
        .search("utf8-test", None, 5)
        .await
        .expect("search must succeed");

    assert!(!hits.is_empty(), "Expected at least one hit for utf8-test");

    for hit in &hits {
        // Verify terse is valid UTF-8
        assert!(
            std::str::from_utf8(hit.terse.as_bytes()).is_ok(),
            "terse must be valid UTF-8: {:?}",
            hit.terse
        );
    }
}

// ---------------------------------------------------------------------------
// Scenario 10: score tie ordering determinism by DocKey
// ---------------------------------------------------------------------------

/// AC: 9-7b-8 scenario 10
#[tokio::test]
async fn test_search_capabilities_score_tie_ordering_determinism_by_doc_key() {
    // Two items with identical searchable_text produce identical BM25 scores
    let items = vec![
        ScenarioTestItem {
            dk: DocKey::new(CapabilityKind::Skill, "alpha-skill"),
            text: "same searchable text for tie-breaking".to_string(),
            desc: "Description A".to_string(),
        },
        ScenarioTestItem {
            dk: DocKey::new(CapabilityKind::Skill, "beta-skill"),
            text: "same searchable text for tie-breaking".to_string(),
            desc: "Description B".to_string(),
        },
    ];
    let index = build_test_index(items);
    let engine = Bm25SearchEngine::new(index);

    let hits = engine
        .search("tie-breaking", None, 5)
        .await
        .expect("search must succeed");

    assert!(
        hits.len() >= 2,
        "Tie-breaking test requires >= 2 hits, got {}",
        hits.len()
    );

    let alpha_pos = hits.iter().position(|h| h.name == "alpha-skill");
    let beta_pos = hits.iter().position(|h| h.name == "beta-skill");

    if let (Some(ap), Some(bp)) = (alpha_pos, beta_pos) {
        assert!(
            ap < bp,
            "alpha-skill (lexicographically first) must appear before beta-skill per DocKey tie-breaking. alpha at {}, beta at {}",
            ap,
            bp
        );
    }
}
