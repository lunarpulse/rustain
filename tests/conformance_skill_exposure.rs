//! Conformance tests for Story 9.6 — Skill Exposure Strategy (Phase A).
//!
//! These tests verify: trait surface, default impl (L1MetadataExposure),
//! opt-in fallback (StaticFullExposure), config wiring, composition-root binding,
//! forward-compat type reservations, and the two-layer cache.

use rustain::adapters::skill_exposure::{
    L1MetadataExposure, SkillExposureKind, SkillExposurePayload,
    SkillRenderDiagnostics, SkillRenderOutcome, StaticFullExposure,
};
use rustain::domain::models::filtered_skill_catalog::FilteredSkillCatalog;
use rustain::domain::models::skill_catalog_delta::SkillCatalogDelta;
use rustain::domain::models::skill_metadata::SkillMetadata;
use rustain::domain::models::{ProviderCapabilities, SkillSource, TransportKind};
use rustain::domain::ports::SkillExposurePort;
use rustain::infrastructure::skill_cache::SkillCache;
use std::sync::Arc;

fn test_metadata(name: &str) -> SkillMetadata {
    SkillMetadata {
        name: name.into(),
        description: format!("{name} provides functionality when the user needs it"),
        source: SkillSource::WorkspaceAgents,
    }
}

fn test_caps() -> ProviderCapabilities {
    ProviderCapabilities {
        supports_streaming: true,
        supports_list_changed: true,
        supports_native_retrieval: None,
        max_tool_count: None,
        transport_kind: TransportKind::Stdio,
    }
}

fn test_cache() -> Arc<SkillCache> {
    Arc::new(SkillCache::new_in_memory())
}

// ── AC-9-6-1: Re-export surface compiles ──

#[test]
fn test_re_export_surface_compiles() {
    // Compile-only: verify all types are reachable from crate root.
    let _kind = SkillExposureKind::L1Metadata;
    let _payload = SkillExposurePayload::Metadata(vec![]);
    let _outcome = SkillRenderOutcome {
        payload: SkillExposurePayload::Metadata(vec![]),
        diagnostics: SkillRenderDiagnostics::clean(0, 0),
    };
}

// ── AC-9-6-3: L1MetadataExposure default kind ──

#[tokio::test]
async fn test_l1_metadata_is_default_kind() {
    let exposure = L1MetadataExposure::new(test_cache());
    assert_eq!(exposure.kind(), SkillExposureKind::L1Metadata);
}

#[tokio::test]
async fn test_l1_metadata_renders_metadata_payload() {
    let exposure = L1MetadataExposure::new(test_cache());
    let catalog = FilteredSkillCatalog::from_metadata(vec![
        test_metadata("code-review"),
        test_metadata("refactor"),
    ]);
    let outcome = exposure.render(&catalog, &test_caps()).await.unwrap();

    match outcome.payload {
        SkillExposurePayload::Metadata(metas) => {
            assert_eq!(metas.len(), 2);
            assert_eq!(metas[0].name, "code-review");
            assert_eq!(metas[1].name, "refactor");
        }
        _ => panic!("L1MetadataExposure must emit Metadata variant"),
    }
    assert_eq!(outcome.diagnostics.catalog_size, 2);
    assert!(outcome.diagnostics.definition_tokens_estimate > 0);
    assert!(!outcome.diagnostics.truncated);
    assert_eq!(outcome.diagnostics.dropped_count, 0);
}

#[tokio::test]
async fn test_l1_metadata_render_empty_catalog() {
    let exposure = L1MetadataExposure::new(test_cache());
    let catalog = FilteredSkillCatalog::empty();
    let outcome = exposure.render(&catalog, &test_caps()).await.unwrap();

    match outcome.payload {
        SkillExposurePayload::Metadata(metas) => assert!(metas.is_empty()),
        _ => panic!("expected Metadata variant"),
    }
    assert_eq!(outcome.diagnostics.catalog_size, 0);
}

#[tokio::test]
async fn test_l1_metadata_on_catalog_changed_is_no_op() {
    let exposure = L1MetadataExposure::new(test_cache());
    let delta = SkillCatalogDelta::empty(1);
    // Must not panic or return Err
    exposure.on_catalog_changed(&delta).await.unwrap();
}

// ── AC-9-6-4: StaticFullExposure opt-in fallback ──

#[tokio::test]
async fn test_static_full_kind() {
    let exposure = StaticFullExposure::new(test_cache());
    assert_eq!(exposure.kind(), SkillExposureKind::StaticFull);
}

#[tokio::test]
async fn test_static_full_renders_bodies_payload() {
    let cache = test_cache();
    let body = "# Test\n\nThis is a test skill body with sufficient length for token estimation purposes.\n\n## Usage\n\nRun `/test` to activate this skill.\n";
    cache.insert(
        "test-skill",
        test_metadata("test-skill"),
        body.to_string(),
    )
    .await;

    let exposure = StaticFullExposure::new(cache);
    let catalog = FilteredSkillCatalog::from_metadata(vec![test_metadata("test-skill")]);
    let outcome = exposure.render(&catalog, &test_caps()).await.unwrap();

    match outcome.payload {
        SkillExposurePayload::Bodies(entries) => {
            assert_eq!(entries.len(), 1);
            assert_eq!(entries[0].metadata.name, "test-skill");
            assert_eq!(entries[0].body, body);
        }
        _ => panic!("StaticFullExposure must emit Bodies variant"),
    }
    assert_eq!(outcome.diagnostics.catalog_size, 1);
    assert!(outcome.diagnostics.definition_tokens_estimate > 0);
}

#[tokio::test]
async fn test_static_full_skips_missing_bodies() {
    let cache = test_cache();
    cache.insert(
        "present",
        test_metadata("present"),
        "body content".into(),
    )
    .await;

    let exposure = StaticFullExposure::new(cache);
    let catalog = FilteredSkillCatalog::from_metadata(vec![
        test_metadata("present"),
        test_metadata("missing"),
    ]);
    let outcome = exposure.render(&catalog, &test_caps()).await.unwrap();

    match outcome.payload {
        SkillExposurePayload::Bodies(entries) => assert_eq!(entries.len(), 1),
        _ => panic!("expected Bodies variant"),
    }
    assert_eq!(outcome.diagnostics.dropped_count, 1);
    assert!(outcome.diagnostics.reason.is_some());
}

#[tokio::test]
async fn test_static_full_on_catalog_changed_is_no_op() {
    let exposure = StaticFullExposure::new(test_cache());
    exposure
        .on_catalog_changed(&SkillCatalogDelta::empty(1))
        .await
        .unwrap();
}

// ── AC-9-6-5: skill_view tool registered with BuiltinProvider ──

#[cfg(feature = "mcp")]
#[tokio::test]
async fn test_builtin_provider_includes_skill_view() {
    use rustain::adapters::builtin::builtin_provider::BuiltinProvider;
    
    use rustain::adapters::filesystem::FileSystemStorage;
    use rustain::adapters::toolset_adapter::ToolSetAdapter;
    use rustain::domain::ports::CapabilityProvider;
    use std::sync::Arc;

    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("workspace");
    std::fs::create_dir_all(&ws).unwrap();
    let sessions = ws.join(".claude").join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let storage: Arc<dyn rustain::domain::ports::StoragePort> =
        Arc::new(FileSystemStorage::new(sessions));

    let tools = ToolSetAdapter::new(ws, Arc::clone(&storage));
    let provider = BuiltinProvider::new(Arc::new(tools));
    let caps = provider.discover().await.unwrap();

    let skill_view_cap = caps.iter().find(|c| c.name == "skill_view");
    assert!(
        skill_view_cap.is_some(),
        "skill_view must be registered in BuiltinProvider"
    );
}

// ── AC-9-6-8: Config validation rejects meta-search with actionable error ──

#[test]
fn test_meta_search_config_rejected_with_actionable_error() {
    
    // Validate via the startup validation function (re-exported)
    let result = rustain::infrastructure::startup::validate_skill_exposure("meta-search");
    assert!(result.is_err(), "meta-search must be rejected");

    let err_msg = format!("{}", result.unwrap_err());
    assert!(
        err_msg.contains("Story 9.7") || err_msg.contains("ADR-09-02"),
        "error must cite Story 9.7 or ADR-09-02: got '{}'",
        err_msg
    );
    assert!(
        err_msg.contains("Phase B"),
        "error must mention Phase B: got '{}'",
        err_msg
    );
}

#[test]
fn test_l1_metadata_config_accepted() {
    let result = rustain::infrastructure::startup::validate_skill_exposure("l1-metadata");
    assert!(result.is_ok(), "l1-metadata must be accepted");
}

#[test]
fn test_static_full_config_accepted() {
    let result = rustain::infrastructure::startup::validate_skill_exposure("static-full");
    assert!(result.is_ok(), "static-full must be accepted");
}

#[test]
fn test_unknown_kind_rejected() {
    let result = rustain::infrastructure::startup::validate_skill_exposure("semantic");
    assert!(result.is_err(), "unknown kind must be rejected");
}

#[test]
fn test_empty_kind_rejected() {
    let result = rustain::infrastructure::startup::validate_skill_exposure("");
    assert!(result.is_err(), "empty kind must be rejected");
}

// ── CLI flag restricted to valid values ──

#[test]
fn test_cli_skill_exposure_flag_valid_values() {
    use clap::Parser;
    use rustain::adapters::cli::commands::Cli;

    let cli = Cli::try_parse_from(["rustain", "--skill-exposure", "l1-metadata"]);
    assert!(
        cli.is_ok(),
        "--skill-exposure l1-metadata must parse successfully"
    );

    let cli = Cli::try_parse_from(["rustain", "--skill-exposure", "static-full"]);
    assert!(cli.is_ok(), "--skill-exposure static-full must parse successfully");
}

#[test]
fn test_cli_skill_exposure_rejects_invalid_value() {
    use clap::Parser;
    use rustain::adapters::cli::commands::Cli;

    let cli = Cli::try_parse_from(["rustain", "--skill-exposure", "meta-search"]);
    assert!(
        cli.is_err(),
        "--skill-exposure meta-search must be rejected by clap"
    );
}

// ── AC-9-6-9: Composition root binds L1Metadata as default ──

#[test]
fn test_compose_with_default_config_binds_l1_metadata() {
    use rustain::domain::models::profile::ProfileSelection;
    use rustain::domain::models::project_context::ProjectContext;
    use rustain::infrastructure::composition::ComposeContext;
    use std::collections::BTreeMap;

    let ctx = ComposeContext {
        workspace_path: std::path::PathBuf::from("/tmp/test"),
        project_context: ProjectContext::empty(),
        storage: Arc::new(rustain::adapters::noop::NoOpStorage),
        skill_activator: Arc::new(
            rustain::adapters::skill_activation::SkillActivator::new(),
        ),
        mcp_servers: Vec::new(),
        include_builtin_tools: true,
        domain_tx: None,
        tool_exposure: "static-full".into(),
        skill_exposure: "l1-metadata".into(),
        skill_cache: test_cache(),
    };
    let selection = ProfileSelection {
        dimensions: BTreeMap::new(),
    };

    let result =
        rustain::infrastructure::composition::build_skill_exposure(&selection, &ctx);
    assert!(result.is_ok(), "default config must compose successfully");
    let built = result.unwrap();
    assert!(built.is_some(), "l1-metadata must yield Some");
    assert_eq!(
        built.unwrap().kind(),
        SkillExposureKind::L1Metadata,
        "default must be L1Metadata"
    );
}

#[test]
fn test_compose_with_static_full_binds_static_full() {
    use rustain::domain::models::profile::ProfileSelection;
    use rustain::domain::models::project_context::ProjectContext;
    use rustain::infrastructure::composition::ComposeContext;
    use std::collections::BTreeMap;

    let ctx = ComposeContext {
        workspace_path: std::path::PathBuf::from("/tmp/test"),
        project_context: ProjectContext::empty(),
        storage: Arc::new(rustain::adapters::noop::NoOpStorage),
        skill_activator: Arc::new(
            rustain::adapters::skill_activation::SkillActivator::new(),
        ),
        mcp_servers: Vec::new(),
        include_builtin_tools: true,
        domain_tx: None,
        tool_exposure: "static-full".into(),
        skill_exposure: "static-full".into(),
        skill_cache: test_cache(),
    };
    let selection = ProfileSelection {
        dimensions: BTreeMap::new(),
    };

    let result =
        rustain::infrastructure::composition::build_skill_exposure(&selection, &ctx);
    assert!(result.is_ok());
    assert_eq!(
        result.unwrap().unwrap().kind(),
        SkillExposureKind::StaticFull
    );
}

// ── AC-9-6-12: Phase B forward-compat — MetaSearch + SearchStub reserved ──

#[test]
fn test_meta_search_variant_reserved() {
    // Verify the enum variant compiles (serde round-trip)
    let kind = SkillExposureKind::MetaSearch;
    let json = serde_json::to_string(&kind).unwrap();
    assert_eq!(json, "\"meta-search\"");
    let back: SkillExposureKind = serde_json::from_str(&json).unwrap();
    assert_eq!(back, SkillExposureKind::MetaSearch);
}

#[test]
fn test_search_stub_payload_variant_reserved() {
    // Compile-only: verify SearchStub variant exists and can be named
    use rustain::domain::models::tool_descriptor::{ToolAnnotations, ToolDescriptor, ToolId};
    let stub_desc = ToolDescriptor {
        id: ToolId("search_capabilities".into()),
        name: "search_capabilities".into(),
        description: "Search available skills and tools by query".into(),
        input_schema: serde_json::json!({"type": "object"}),
        provider_id: "builtin".into(),
        annotations: ToolAnnotations {
            title: None,
            read_only_hint: Some(true),
            destructive_hint: None,
            idempotent_hint: None,
            open_world_hint: None,
        },
    };
    let _stub: SkillExposurePayload = SkillExposurePayload::SearchStub(stub_desc);
    // Verify we can match it (with wildcard for #[non_exhaustive])
    match &_stub {
        SkillExposurePayload::Metadata(_) => {}
        SkillExposurePayload::Bodies(_) => {}
        SkillExposurePayload::SearchStub(_) => {}
        _ => {} // #[non_exhaustive] forward-compat
    }
}

#[test]
fn test_phase_a_no_meta_search_feature() {
    let cargo_toml =
        std::fs::read_to_string("Cargo.toml").expect("Cargo.toml must be readable");
    assert!(
        !cargo_toml.contains("meta-search"),
        "Cargo.toml must not contain meta-search feature"
    );
}

// ── AC-9-6-7: Two-layer cache ──

#[test]
fn test_cache_in_memory_constructor_no_l2_path() {
    let _cache = SkillCache::new_in_memory();
    // Construction succeeds; no L2 path means save/load are no-ops
}

#[tokio::test]
async fn test_cache_insert_and_retrieve_metadata() {
    let cache = test_cache();
    cache
        .insert(
            "test-skill",
            test_metadata("test-skill"),
            "body text".into(),
        )
        .await;

    let meta = cache.metadata("test-skill").await;
    assert!(meta.is_some());
    assert_eq!(meta.unwrap().name, "test-skill");
}

#[tokio::test]
async fn test_cache_body_retrieval() {
    let cache = test_cache();
    cache
        .insert(
            "test-skill",
            test_metadata("test-skill"),
            "body text".into(),
        )
        .await;

    let body = cache.body("test-skill").await.unwrap();
    assert_eq!(body, "body text");
}

#[tokio::test]
async fn test_cache_missing_skill_returns_error() {
    let cache = test_cache();
    let result = cache.body("nonexistent").await;
    assert!(result.is_err());
}

#[tokio::test]
async fn test_cache_source_retrieval() {
    let cache = test_cache();
    let meta = SkillMetadata {
        name: "ws-skill".into(),
        description: "workspace skill when needed".into(),
        source: SkillSource::WorkspaceClaude,
    };
    cache.insert("ws-skill", meta, "body".into()).await;

    let source = cache.source("ws-skill").await.unwrap();
    assert_eq!(source, SkillSource::WorkspaceClaude);
}

#[tokio::test]
async fn test_cache_snapshot_catalog() {
    let cache = test_cache();
    cache
        .insert(
            "a",
            test_metadata("a"),
            "body-a".into(),
        )
        .await;
    cache
        .insert(
            "b",
            test_metadata("b"),
            "body-b".into(),
        )
        .await;

    let catalog = cache.snapshot_catalog().await;
    assert_eq!(catalog.len(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn test_cache_concurrent_reads_no_deadlock() {
    let cache = test_cache();
    cache
        .insert(
            "shared",
            test_metadata("shared"),
            "shared body".into(),
        )
        .await;

    let cache = Arc::new(cache);
    let mut handles = Vec::new();
    for _ in 0..8 {
        let c = Arc::clone(&cache);
        handles.push(tokio::spawn(async move {
            for _ in 0..50 {
                let _ = c.metadata("shared").await;
                let _ = c.body("shared").await;
                let _ = c.source("shared").await;
            }
        }));
    }
    for h in handles {
        h.await.unwrap();
    }
}

// ── AC-9-6-11: Diagnostics carry telemetry hooks without PII ──

#[tokio::test]
async fn test_diagnostics_carry_catalog_size_and_token_estimate() {
    let exposure = L1MetadataExposure::new(test_cache());
    let catalog = FilteredSkillCatalog::from_metadata(vec![
        test_metadata("a"),
        test_metadata("b"),
        test_metadata("c"),
    ]);
    let outcome = exposure.render(&catalog, &test_caps()).await.unwrap();

    assert_eq!(outcome.diagnostics.catalog_size, 3);
    assert!(outcome.diagnostics.definition_tokens_estimate > 0);
}

#[test]
fn test_diagnostics_no_skill_name_in_reason_field() {
    let diag = SkillRenderDiagnostics::clean(3, 300);
    assert!(diag.reason.is_none(), "clean diagnostics must have no reason");
}

// ── Manifest computation ──

#[test]
fn test_cache_manifest_deterministic_for_empty_dirs() {
    
    let m1 = SkillCache::manifest(&[]);
    let m2 = SkillCache::manifest(&[]);
    assert_eq!(m1, m2, "manifest must be deterministic for empty input");
}

#[test]
fn test_cache_manifest_differs_for_different_inputs() {
    let tmp = tempfile::tempdir().unwrap();
    let a = tmp.path().join("a");
    let b = tmp.path().join("b");
    std::fs::create_dir_all(&a).unwrap();
    std::fs::create_dir_all(&b).unwrap();
    std::fs::write(a.join("SKILL.md"), "---\nname: skill-a\ndescription: desc when needed\n---\n\nbody\n").unwrap();
    std::fs::write(b.join("SKILL.md"), "---\nname: skill-b\ndescription: desc when needed\n---\n\nbody\n").unwrap();

    
    let m1 = SkillCache::manifest(&[a.as_path()]);
    std::thread::sleep(std::time::Duration::from_millis(10)); // ensure mtime differs
    std::fs::write(a.join("SKILL.md"), "---\nname: skill-a\ndescription: modified desc when needed\n---\n\nmodified\n").unwrap();
    let m2 = SkillCache::manifest(&[a.as_path()]);

    assert_ne!(m1, m2, "manifest must change when file content changes");
}

#[test]
fn test_skill_metadata_estimated_tokens_positive() {
    let meta = test_metadata("test");
    assert!(meta.estimated_tokens() > 0);
}

#[test]
fn test_filtered_skill_catalog_serde_round_trip() {
    let catalog = FilteredSkillCatalog::from_metadata(vec![test_metadata("a")]);
    let json = serde_json::to_string(&catalog).unwrap();
    let back: FilteredSkillCatalog = serde_json::from_str(&json).unwrap();
    assert_eq!(back.len(), 1);
}
