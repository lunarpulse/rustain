#![allow(clippy::type_complexity)] // AI-12.1: test fixture tuple types
#![cfg(feature = "meta-search")]

//! Router-level conformance tests for the two-door `search_skills` /
//! `search_tools` split (AC: 9-7d-7).
//!
//! **BuiltinProvider construction:** Tests construct a `ToolSetAdapter`, set
//! a `meta_search_engine`, then drive `execute("search_skills", ...)` and
//! `execute("search_tools", ...)` through the adapter's public API.
//!
//! Tests #1, #2, #4, #5, #6 use the real `Bm25SearchEngine` wired to a
//! `MergedIndex` populated from fixture items (same `ScenarioTestItem` /
//! `build_test_index` pattern as `integration_search_doors_invocation.rs`).
//! Test #3 uses a `MockMetaSearchEngine` that captures the dispatch arguments.

use arc_swap::ArcSwap;
use rustain::adapters::toolset_adapter::ToolSetAdapter;
use rustain::domain::errors::ToolError;
use rustain::domain::models::capability_kind::CapabilityKind;
use rustain::domain::models::doc_key::DocKey;
use rustain::domain::models::search_hit::SearchHit;
use rustain::domain::ports::ToolSetPort;
use rustain::domain::ports::search::{IndexableItem, MetaSearchEngine, MetaSearchError};
use rustain::infrastructure::search::{Bm25SearchEngine, MergedIndex};
use std::borrow::Cow;
use std::collections::BTreeMap;
use std::sync::Arc;

// ---------------------------------------------------------------------------
// Shared helpers (inline — kept small for 6 tests)
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

fn build_test_index(items: Vec<ScenarioTestItem>) -> Arc<ArcSwap<MergedIndex>> {
    let refs: Vec<&dyn IndexableItem> = items.iter().map(|i| i as &dyn IndexableItem).collect();
    let overrides: BTreeMap<DocKey, String> = BTreeMap::new();
    let merged = MergedIndex::from_items_with_overrides(&refs, &overrides);
    Arc::new(ArcSwap::from_pointee(merged))
}

fn skill_item(name: &str, desc: &str) -> ScenarioTestItem {
    ScenarioTestItem {
        dk: DocKey::new(CapabilityKind::Skill, name),
        text: format!("{} {}", name, desc),
        desc: desc.to_string(),
    }
}

fn tool_item(name: &str, desc: &str) -> ScenarioTestItem {
    ScenarioTestItem {
        dk: DocKey::new(CapabilityKind::Tool, name),
        text: format!("{} {}", name, desc),
        desc: desc.to_string(),
    }
}

fn make_adapter_with_engine(engine: Arc<dyn MetaSearchEngine>) -> ToolSetAdapter {
    let dir = std::env::current_dir().unwrap();
    use rustain::adapters::filesystem::FileSystemStorage;
    use rustain::domain::ports::StoragePort;
    let sessions_dir = dir.join(".claude").join("sessions");
    let storage: Arc<dyn StoragePort> = Arc::new(FileSystemStorage::new(sessions_dir));
    let mut adapter = ToolSetAdapter::new(
        dir,
        storage,
        Arc::new(ArcSwap::from_pointee(
            Arc::new(rustain::adapters::sandbox::NoOpSandbox)
                as Arc<dyn rustain::domain::ports::SandboxManager>,
        )),
        Arc::new(tokio::sync::RwLock::new(
            rustain::domain::models::sandbox::SandboxPolicy::Permissive,
        )),
    );
    adapter.set_meta_search_engine(engine);
    adapter
}

fn test_cancel() -> tokio_util::sync::CancellationToken {
    tokio_util::sync::CancellationToken::new()
}

// ---------------------------------------------------------------------------
// Mock engine for test #3 (captures dispatch arguments)
// ---------------------------------------------------------------------------

struct MockMetaSearchEngine {
    captured: Arc<tokio::sync::Mutex<Option<(String, Option<CapabilityKind>, usize)>>>,
}

impl MockMetaSearchEngine {
    fn new() -> (
        Self,
        Arc<tokio::sync::Mutex<Option<(String, Option<CapabilityKind>, usize)>>>,
    ) {
        let captured = Arc::new(tokio::sync::Mutex::new(None));
        (
            Self {
                captured: captured.clone(),
            },
            captured,
        )
    }
}

#[async_trait::async_trait]
impl MetaSearchEngine for MockMetaSearchEngine {
    async fn search(
        &self,
        query: &str,
        kind_filter: Option<CapabilityKind>,
        top_k: usize,
    ) -> Result<Vec<SearchHit>, MetaSearchError> {
        *self.captured.lock().await = Some((query.to_string(), kind_filter, top_k));
        Ok(vec![])
    }
}

// ---------------------------------------------------------------------------
// Test 1: search_skills returns only skills
// ---------------------------------------------------------------------------

/// AC: 9-7d-7 — search_skills returns only skills
#[tokio::test]
async fn test_search_skills_returns_only_skills() {
    let items = vec![
        skill_item("format-code", "Format source code with formatter."),
        tool_item("Bash", "Execute shell commands."),
        skill_item("lint-project", "Run lint on a code project."),
    ];
    let index = build_test_index(items);
    let engine = Arc::new(Bm25SearchEngine::new(index));
    let adapter = make_adapter_with_engine(engine);

    let result = adapter
        .execute(
            "search_skills",
            serde_json::json!({"query": "format code"}),
            test_cancel(),
        )
        .await
        .unwrap();

    let hits: Vec<SearchHit> = serde_json::from_str(&result.content).unwrap();
    assert!(!hits.is_empty(), "search_skills should return hits");
    for hit in &hits {
        assert_eq!(
            hit.kind,
            CapabilityKind::Skill,
            "search_skills MUST return only skill hits; got {:?}",
            hit.kind
        );
    }
}

// ---------------------------------------------------------------------------
// Test 2: search_tools returns only tools
// ---------------------------------------------------------------------------

/// AC: 9-7d-7 — search_tools returns only tools
#[tokio::test]
async fn test_search_tools_returns_only_tools() {
    let items = vec![
        skill_item("format-code", "Format source code with formatter."),
        tool_item("Bash", "Execute shell commands."),
        tool_item("Read", "Read file contents."),
    ];
    let index = build_test_index(items);
    let engine = Arc::new(Bm25SearchEngine::new(index));
    let adapter = make_adapter_with_engine(engine);

    let result = adapter
        .execute(
            "search_tools",
            serde_json::json!({"query": "execute commands"}),
            test_cancel(),
        )
        .await
        .unwrap();

    let hits: Vec<SearchHit> = serde_json::from_str(&result.content).unwrap();
    assert!(!hits.is_empty(), "search_tools should return hits");
    for hit in &hits {
        assert_eq!(
            hit.kind,
            CapabilityKind::Tool,
            "search_tools MUST return only tool hits; got {:?}",
            hit.kind
        );
    }
}

// ---------------------------------------------------------------------------
// Test 3: router dispatch correctness (mock engine captures kind_filter)
// ---------------------------------------------------------------------------

/// AC: 9-7d-7 — router dispatches correct kind_filter per door
#[tokio::test]
async fn test_router_dispatches_correctly() {
    let (mock, captured) = MockMetaSearchEngine::new();
    let engine: Arc<dyn MetaSearchEngine> = Arc::new(mock);
    let adapter = make_adapter_with_engine(engine);

    // Test search_skills dispatches Some(Skill)
    let result = adapter
        .execute(
            "search_skills",
            serde_json::json!({"query": "x"}),
            test_cancel(),
        )
        .await
        .unwrap();
    assert!(!result.is_error);
    {
        let cap = captured.lock().await;
        let (_query, kind, _top_k) = cap.as_ref().expect("mock should have captured args");
        assert_eq!(
            *kind,
            Some(CapabilityKind::Skill),
            "search_skills MUST dispatch with kind_filter=Some(Skill)"
        );
    }

    // Reset capture
    *captured.lock().await = None;

    // Test search_tools dispatches Some(Tool)
    let result = adapter
        .execute(
            "search_tools",
            serde_json::json!({"query": "x"}),
            test_cancel(),
        )
        .await
        .unwrap();
    assert!(!result.is_error);
    {
        let cap = captured.lock().await;
        let (_query, kind, _top_k) = cap.as_ref().expect("mock should have captured args");
        assert_eq!(
            *kind,
            Some(CapabilityKind::Tool),
            "search_tools MUST dispatch with kind_filter=Some(Tool)"
        );
    }

    // Test search_capabilities route is GONE
    let result = adapter
        .execute(
            "search_capabilities",
            serde_json::json!({"query": "x"}),
            test_cancel(),
        )
        .await;
    assert!(
        matches!(result, Err(ToolError::NotFound(ref msg)) if msg == "search_capabilities"),
        "search_capabilities route MUST be gone — expected ToolError::NotFound(\"search_capabilities\"), got {:?}",
        result
    );
}

// ---------------------------------------------------------------------------
// Test 4: synonym expansion through skills door
// ---------------------------------------------------------------------------

/// AC: 9-7d-7 — synonym expansion through search_skills door
#[tokio::test]
async fn test_synonym_expansion_through_skills_door() {
    // "neat → format" synonym from synonyms.toml should fire through the
    // skills door: searching "neat" should find "format-code" skill.
    let items = vec![
        skill_item(
            "format-code",
            "Format source code and keep it neat and tidy.",
        ),
        skill_item("greet-user", "Greet the user."),
    ];
    let index = build_test_index(items);
    let engine = Arc::new(Bm25SearchEngine::new(index));
    let adapter = make_adapter_with_engine(engine);

    let result = adapter
        .execute(
            "search_skills",
            serde_json::json!({"query": "how do i keep my code neat"}),
            test_cancel(),
        )
        .await
        .unwrap();

    let hits: Vec<SearchHit> = serde_json::from_str(&result.content).unwrap();
    let names: Vec<&str> = hits.iter().map(|h| h.name.as_str()).collect();
    assert!(
        names.contains(&"format-code"),
        "format-code should appear in search_skills hits via neat→format synonym expansion; got: {:?}",
        names
    );
}

// ---------------------------------------------------------------------------
// Test 5: synonym expansion through tools door
// ---------------------------------------------------------------------------

/// AC: 9-7d-7 — synonym expansion through search_tools door
#[tokio::test]
async fn test_synonym_expansion_through_tools_door() {
    // "tidy → format" synonym from synonyms.toml — use on a tool fixture
    // to demonstrate cross-cutting expansion through the tools door.
    let items = vec![
        tool_item(
            "format-doc-tool",
            "Format documents neatly and keep them tidy.",
        ),
        tool_item("echo-tool", "Echo text."),
    ];
    let index = build_test_index(items);
    let engine = Arc::new(Bm25SearchEngine::new(index));
    let adapter = make_adapter_with_engine(engine);

    let result = adapter
        .execute(
            "search_tools",
            serde_json::json!({"query": "make documents tidy"}),
            test_cancel(),
        )
        .await
        .unwrap();

    let hits: Vec<SearchHit> = serde_json::from_str(&result.content).unwrap();
    let names: Vec<&str> = hits.iter().map(|h| h.name.as_str()).collect();
    assert!(
        names.contains(&"format-doc-tool"),
        "format-doc-tool should appear in search_tools hits via tidy→format synonym expansion; got: {:?}",
        names
    );
}

// ---------------------------------------------------------------------------
// Test 6: no cross-contamination under concurrent load
// ---------------------------------------------------------------------------

/// AC: 9-7d-7 — no cross-contamination under concurrent load
#[tokio::test]
async fn test_no_cross_contamination_concurrent() {
    let items = vec![
        skill_item("format-code", "Format source code."),
        skill_item("lint-project", "Run linter."),
        skill_item("test-runner", "Run tests."),
        tool_item("Bash", "Execute shell commands."),
        tool_item("Read", "Read file contents."),
        tool_item("Write", "Write file contents."),
    ];
    let index = build_test_index(items);
    let engine: Arc<dyn MetaSearchEngine> = Arc::new(Bm25SearchEngine::new(index));
    let adapter = make_adapter_with_engine(engine);
    let adapter = Arc::new(adapter);

    let barrier = Arc::new(tokio::sync::Barrier::new(50));
    let mut handles = Vec::new();

    for i in 0..50 {
        let adapter = adapter.clone();
        let barrier = barrier.clone();
        let is_skills = i < 25;
        handles.push(tokio::spawn(async move {
            barrier.wait().await;
            let tool_name = if is_skills {
                "search_skills"
            } else {
                "search_tools"
            };
            let expected_kind = if is_skills {
                CapabilityKind::Skill
            } else {
                CapabilityKind::Tool
            };
            let cancel = test_cancel();
            let result = adapter
                .execute(tool_name, serde_json::json!({"query": "format"}), cancel)
                .await
                .unwrap();
            let hits: Vec<SearchHit> = serde_json::from_str(&result.content).unwrap();
            (is_skills, expected_kind, hits)
        }));
    }

    let results = futures::future::join_all(handles).await;
    for r in results {
        let (is_skills, expected_kind, hits) = r.unwrap();
        for hit in &hits {
            assert_eq!(
                hit.kind,
                expected_kind,
                "concurrent {} task got cross-kind contamination: {:?}",
                if is_skills { "skills" } else { "tools" },
                hit.kind
            );
        }
    }
}
