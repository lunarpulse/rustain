#![cfg(feature = "meta-search")]

//! Phase B conformance tests for the shared meta-search infrastructure.

use rustain::domain::models::capability_kind::CapabilityKind;
use rustain::domain::models::doc_key::DocKey;
use rustain::domain::models::search_hit::SearchHit;
use rustain::domain::ports::search::{IndexableItem, MetaSearchEngine};

#[test]
fn test_re_export_surface_compiles() {
    let _ = || {
        let _: Box<dyn rustain::domain::ports::search::MetaSearchEngine> = panic!("compile-only");
        let _: Box<dyn rustain::domain::ports::search::IndexableItem> = panic!("compile-only");
        let _: rustain::domain::ports::search::MetaSearchError = panic!("compile-only");
    };
}

#[test]
fn test_search_hit_serialization_field_set_locked() {
    let hit = SearchHit {
        name: "mcp__postgres__query".into(),
        kind: CapabilityKind::Tool,
        terse: "Run SQL against the configured PostgreSQL instance.".into(),
        score: 12.7,
        provider: Some("postgres".into()),
        matched_terms: Some(vec!["sql".into(), "postgres".into()]),
    };
    let v: serde_json::Value = serde_json::to_value(&hit).unwrap();
    let obj = v.as_object().expect("SearchHit serializes to JSON object");
    let mut keys: Vec<&str> = obj.keys().map(|s| s.as_str()).collect();
    keys.sort();
    assert_eq!(
        keys,
        vec!["kind", "matched_terms", "name", "provider", "score", "terse"],
        "SearchHit serialized field set MUST be exactly {{name, kind, terse, score, provider?, matched_terms?}}"
    );
}

#[test]
fn test_search_hit_omits_none_optionals_from_payload() {
    let hit = SearchHit::minimal("Bash", CapabilityKind::Tool, "Execute shell commands.", 8.4);
    let v: serde_json::Value = serde_json::to_value(&hit).unwrap();
    let obj = v.as_object().unwrap();
    let mut keys: Vec<&str> = obj.keys().map(|s| s.as_str()).collect();
    keys.sort();
    assert_eq!(
        keys,
        vec!["kind", "name", "score", "terse"],
        "Minimal SearchHit (provider=None, matched_terms=None) MUST serialize to exactly 4 keys"
    );
}

#[test]
fn test_top_k_clamp_rejects_above_20() {
    use rustain::domain::ports::search::MetaSearchError;
    use rustain::infrastructure::search::Bm25SearchEngine;
    use arc_swap::ArcSwap;
    use std::sync::Arc;

    let index = Arc::new(ArcSwap::from_pointee(rustain::infrastructure::search::MergedIndex::empty()));
    let engine = Bm25SearchEngine::new(index);
    let rt = tokio::runtime::Runtime::new().unwrap();
    let result = rt.block_on(async {
        engine.search("test", None, 25).await
    });
    assert!(matches!(result, Err(MetaSearchError::TopKTooLarge(25))));
}

#[test]
fn test_top_k_clamp_accepts_at_20() {
    use rustain::infrastructure::search::Bm25SearchEngine;
    use arc_swap::ArcSwap;
    use std::sync::Arc;

    let index = Arc::new(ArcSwap::from_pointee(rustain::infrastructure::search::MergedIndex::empty()));
    let engine = Bm25SearchEngine::new(index);
    let rt = tokio::runtime::Runtime::new().unwrap();
    let result = rt.block_on(async {
        engine.search("test", None, 20).await
    });
    assert!(result.is_ok());
}

#[test]
fn test_empty_query_rejected() {
    use rustain::domain::ports::search::MetaSearchError;
    use rustain::infrastructure::search::Bm25SearchEngine;
    use arc_swap::ArcSwap;
    use std::sync::Arc;

    let index = Arc::new(ArcSwap::from_pointee(rustain::infrastructure::search::MergedIndex::empty()));
    let engine = Bm25SearchEngine::new(index);
    let rt = tokio::runtime::Runtime::new().unwrap();
    
    let result = rt.block_on(async {
        engine.search("", None, 5).await
    });
    assert!(matches!(result, Err(MetaSearchError::EmptyQuery)));

    let result = rt.block_on(async {
        engine.search("   ", None, 5).await
    });
    assert!(matches!(result, Err(MetaSearchError::EmptyQuery)));
}

#[test]
fn test_search_config_defaults_asymmetric() {
    use rustain::domain::models::SearchConfig;
    let cfg = SearchConfig::default();
    assert_eq!(cfg.skills, "on");
    assert_eq!(cfg.tools, "off");
}

#[test]
fn test_bm25_engine_search_returns_ranked_hits() {
    use rustain::infrastructure::search::{Bm25SearchEngine, MergedIndex};
    use arc_swap::ArcSwap;
    use std::sync::Arc;

    struct TestItem {
        name: String,
        desc: String,
        kind: CapabilityKind,
    }

    impl IndexableItem for TestItem {
        fn doc_key(&self) -> DocKey {
            DocKey::new(self.kind, self.name.clone())
        }

        fn searchable_text(&self) -> std::borrow::Cow<'_, str> {
            std::borrow::Cow::Owned(format!("{} {}", self.name, self.desc))
        }

        fn description(&self) -> std::borrow::Cow<'_, str> {
            std::borrow::Cow::Borrowed(&self.desc)
        }

        fn to_search_hit(&self, score: f32, _matched: Option<Vec<String>>) -> SearchHit {
            SearchHit::minimal(self.name.clone(), self.kind, &self.desc, score)
        }
    }

    let items: Vec<TestItem> = vec![
        TestItem { name: "query".into(), desc: "Run SQL queries".into(), kind: CapabilityKind::Tool },
        TestItem { name: "review-code".into(), desc: "Review code for style".into(), kind: CapabilityKind::Skill },
    ];
    let refs: Vec<&TestItem> = items.iter().collect();
    let index = Arc::new(ArcSwap::from_pointee(MergedIndex::from_items(&refs)));
    let engine = Bm25SearchEngine::new(index);
    
    let rt = tokio::runtime::Runtime::new().unwrap();
    let hits = rt.block_on(async {
        engine.search("sql", None, 5).await.unwrap()
    });
    
    assert!(!hits.is_empty());
    assert_eq!(hits[0].kind, CapabilityKind::Tool);
}

#[test]
fn test_kind_filter_returns_only_filtered_kind() {
    use rustain::infrastructure::search::{Bm25SearchEngine, MergedIndex};
    use arc_swap::ArcSwap;
    use std::sync::Arc;

    struct TestItem {
        name: String,
        desc: String,
        kind: CapabilityKind,
    }

    impl IndexableItem for TestItem {
        fn doc_key(&self) -> DocKey {
            DocKey::new(self.kind, self.name.clone())
        }

        fn searchable_text(&self) -> std::borrow::Cow<'_, str> {
            std::borrow::Cow::Owned(format!("{} {}", self.name, self.desc))
        }

        fn description(&self) -> std::borrow::Cow<'_, str> {
            std::borrow::Cow::Borrowed(&self.desc)
        }

        fn to_search_hit(&self, score: f32, _matched: Option<Vec<String>>) -> SearchHit {
            SearchHit::minimal(self.name.clone(), self.kind, &self.desc, score)
        }
    }

    let items: Vec<TestItem> = vec![
        TestItem { name: "tool1".into(), desc: "A tool".into(), kind: CapabilityKind::Tool },
        TestItem { name: "tool2".into(), desc: "Another tool".into(), kind: CapabilityKind::Tool },
        TestItem { name: "skill1".into(), desc: "A skill".into(), kind: CapabilityKind::Skill },
        TestItem { name: "skill2".into(), desc: "Another skill".into(), kind: CapabilityKind::Skill },
    ];
    let refs: Vec<&TestItem> = items.iter().collect();
    let index = Arc::new(ArcSwap::from_pointee(MergedIndex::from_items(&refs)));
    let engine = Bm25SearchEngine::new(index);
    
    let rt = tokio::runtime::Runtime::new().unwrap();
    
    // Filter for tools only
    let tool_hits = rt.block_on(async {
        engine.search("tool", Some(CapabilityKind::Tool), 5).await.unwrap()
    });
    assert!(tool_hits.iter().all(|h| h.kind == CapabilityKind::Tool));
    
    // Filter for skills only
    let skill_hits = rt.block_on(async {
        engine.search("skill", Some(CapabilityKind::Skill), 5).await.unwrap()
    });
    assert!(skill_hits.iter().all(|h| h.kind == CapabilityKind::Skill));
}

#[test]
fn test_compute_terse_first_sentence() {
    use rustain::domain::services::meta_search::compute_terse;
    let desc = "Runs ruff format on the file. The result is written back.";
    assert_eq!(compute_terse(desc, "ruff_format"), "Runs ruff format on the file.");
}

#[test]
fn test_compute_terse_empty_falls_back_to_name() {
    use rustain::domain::services::meta_search::compute_terse;
    assert_eq!(compute_terse("", "review-code"), "review-code");
    assert_eq!(compute_terse("   ", "review-code"), "review-code");
}

#[test]
fn test_compute_terse_long_truncates_with_ellipsis() {
    use rustain::domain::services::meta_search::compute_terse;
    let desc = "a".repeat(200);
    let out = compute_terse(&desc, "x");
    assert!(out.ends_with('\u{2026}'), "must end with ellipsis");
}

#[test]
fn test_compute_terse_short_sentence_fits() {
    use rustain::domain::services::meta_search::compute_terse;
    assert_eq!(compute_terse("Quick description.", "x"), "Quick description.");
}

#[test]
fn test_compute_terse_question_mark() {
    use rustain::domain::services::meta_search::compute_terse;
    assert_eq!(compute_terse("Can you format Python? Yes, with ruff.", "x"), "Can you format Python?");
}

#[test]
fn test_compute_terse_exclamation() {
    use rustain::domain::services::meta_search::compute_terse;
    assert_eq!(compute_terse("Run this now! It's important.", "x"), "Run this now!");
}

#[test]
fn test_search_config_validate_accepts_on_off() {
    use rustain::domain::models::SearchConfig;
    let cfg = SearchConfig { skills: "on".into(), tools: "off".into() };
    assert!(cfg.validate().is_ok());
}

#[test]
fn test_search_config_validate_rejects_typos() {
    use rustain::domain::models::SearchConfig;
    let cfg = SearchConfig { skills: "onn".into(), tools: "off".into() };
    assert!(cfg.validate().is_err());
}

#[test]
fn test_search_config_validate_rejects_empty() {
    use rustain::domain::models::SearchConfig;
    let cfg = SearchConfig { skills: "".into(), tools: "off".into() };
    assert!(cfg.validate().is_err());
}

#[test]
fn test_doc_key_display_format() {
    let dk = DocKey::new(CapabilityKind::Tool, "Bash");
    assert_eq!(dk.display(), "tool::Bash");
    let dk = DocKey::new(CapabilityKind::Skill, "review-code");
    assert_eq!(dk.display(), "skill::review-code");
}

#[test]
fn test_capability_kind_as_str() {
    assert_eq!(CapabilityKind::Tool.as_str(), "tool");
    assert_eq!(CapabilityKind::Skill.as_str(), "skill");
}

#[test]
fn test_search_hit_minimal_has_no_optionals() {
    let hit = SearchHit::minimal("test", CapabilityKind::Tool, "desc", 1.0);
    assert!(hit.provider.is_none());
    assert!(hit.matched_terms.is_none());
}

#[test]
fn test_search_hit_is_send_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<SearchHit>();
}

#[test]
fn test_meta_search_error_display_messages() {
    use rustain::domain::ports::search::MetaSearchError;
    assert!(format!("{}", MetaSearchError::EmptyQuery).contains("empty query"));
    assert!(format!("{}", MetaSearchError::TopKTooLarge(25)).contains("25"));
    assert!(format!("{}", MetaSearchError::IndexNotReady("warm".into())).contains("warm"));
    assert!(format!("{}", MetaSearchError::Internal("oops".into())).contains("oops"));
}

#[test]
fn test_catalog_observer_registry_broadcast() {
    use rustain::infrastructure::composition::catalog_observer_registry::CatalogObserverRegistry;
    use rustain::infrastructure::search::MergedIndex;
    use std::sync::Arc;

    let rebuild_fn: std::sync::Arc<dyn Fn() -> Arc<MergedIndex> + Send + Sync> =
        std::sync::Arc::new(|| Arc::new(MergedIndex::empty()));
    let reg = CatalogObserverRegistry::new(
        Arc::new(arc_swap::ArcSwap::from_pointee(MergedIndex::empty())),
        rebuild_fn,
    );
    let mut rx = reg.tool_sender.subscribe();
    let delta = rustain::domain::models::CatalogDelta {
        added: vec![],
        removed: vec![],
        version: 1,
    };
    let _ = reg.tool_sender.send(delta.clone());
    let received = rx.try_recv().unwrap();
    assert_eq!(received.version, 1);
}

#[test]
fn test_merged_index_from_items_with_overrides() {
    use rustain::infrastructure::search::MergedIndex;
    use std::collections::BTreeMap;

    struct TestItem {
        name: String,
        desc: String,
        kind: CapabilityKind,
    }
    impl IndexableItem for TestItem {
        fn doc_key(&self) -> DocKey { DocKey::new(self.kind, self.name.clone()) }
        fn searchable_text(&self) -> std::borrow::Cow<'_, str> {
            std::borrow::Cow::Owned(format!("{} {}", self.name, self.desc))
        }
        fn description(&self) -> std::borrow::Cow<'_, str> {
            std::borrow::Cow::Borrowed(&self.desc)
        }
        fn to_search_hit(&self, score: f32, _: Option<Vec<String>>) -> SearchHit {
            SearchHit::minimal(self.name.clone(), self.kind, &self.desc, score)
        }
    }
    let items = vec![
        TestItem { name: "review-code".into(), desc: "Review code for style violations.".into(), kind: CapabilityKind::Skill },
    ];
    let refs: Vec<&TestItem> = items.iter().collect();
    let mut overrides = BTreeMap::new();
    overrides.insert(DocKey::new(CapabilityKind::Skill, "review-code"), "Override terse".into());
    let index = MergedIndex::from_items_with_overrides(&refs, &overrides);
    let hits = index.search("review", None, 5);
    assert_eq!(hits[0].terse, "Override terse");
}

#[test]
fn test_validate_tools_exposure_accepts_meta_search() {
    let result = rustain::infrastructure::startup::validate_tools_exposure("meta-search");
    assert!(result.is_ok(), "meta-search must be accepted with the feature enabled");
}

#[test]
fn test_validate_skill_exposure_accepts_meta_search() {
    let result = rustain::infrastructure::startup::validate_skill_exposure("meta-search");
    assert!(result.is_ok(), "meta-search must be accepted with the feature enabled");
}

#[test]
fn test_bm25_engine_index_swap() {
    use rustain::infrastructure::search::{Bm25SearchEngine, MergedIndex};
    use arc_swap::ArcSwap;
    use std::sync::Arc;

    let index = Arc::new(ArcSwap::from_pointee(MergedIndex::empty()));
    let engine = Bm25SearchEngine::new(Arc::clone(&index));

    struct TestItem {
        name: String,
        desc: String,
        kind: CapabilityKind,
    }
    impl IndexableItem for TestItem {
        fn doc_key(&self) -> DocKey { DocKey::new(self.kind, self.name.clone()) }
        fn searchable_text(&self) -> std::borrow::Cow<'_, str> {
            std::borrow::Cow::Owned(format!("{} {}", self.name, self.desc))
        }
        fn description(&self) -> std::borrow::Cow<'_, str> {
            std::borrow::Cow::Borrowed(&self.desc)
        }
        fn to_search_hit(&self, score: f32, _: Option<Vec<String>>) -> SearchHit {
            SearchHit::minimal(self.name.clone(), self.kind, &self.desc, score)
        }
    }

    let rt = tokio::runtime::Runtime::new().unwrap();
    let hits = rt.block_on(async { engine.search("test", None, 5).await.unwrap() });
    assert!(hits.is_empty(), "empty index returns no hits");

    let items = vec![TestItem { name: "query".into(), desc: "Run SQL queries".into(), kind: CapabilityKind::Tool }];
    let refs: Vec<&TestItem> = items.iter().collect();
    engine.swap_index(Arc::new(MergedIndex::from_items(&refs)));

    let hits = rt.block_on(async { engine.search("sql", None, 5).await.unwrap() });
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].name, "query");
}

#[test]
fn test_search_hit_serde_round_trip() {
    let hit = SearchHit {
        name: "mcp__postgres__query".into(),
        kind: CapabilityKind::Tool,
        terse: "Run SQL.".into(),
        score: 7.0,
        provider: Some("postgres".into()),
        matched_terms: Some(vec!["sql".into()]),
    };
    let json = serde_json::to_string(&hit).unwrap();
    let back: SearchHit = serde_json::from_str(&json).unwrap();
    assert_eq!(hit, back);
}

#[test]
fn test_doc_key_ordering_deterministic() {
    let dk1 = DocKey::new(CapabilityKind::Tool, "a-tool");
    let dk2 = DocKey::new(CapabilityKind::Skill, "a-skill");
    assert!(dk1 < dk2, "Tool < Skill by variant declaration order");
}
