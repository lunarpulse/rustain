//! Conformance tests for Story 9.4 — Tool Exposure Strategy (Phase A).
//!
//! These tests verify the load-bearing seam: trait surface, default impl,
//! config wiring, composition-root binding, and forward-compat types.

use clap::Parser;
use rustain::adapters::cli::commands::Cli;
use rustain::adapters::tool_exposure::{
    Capability, CapabilityMatrix, ExposureKind, ExposurePayload, RenderDiagnostics, RenderOutcome,
    StaticFullExposure,
};
use rustain::domain::models::project_context::ProjectContext;
use rustain::domain::models::{
    AppConfig, FilteredCatalog, ProviderCapabilities, ToolAnnotations, ToolDescriptor, ToolId,
    TransportKind,
};
use rustain::domain::ports::ToolExposurePort;

fn test_descriptor(name: &str) -> ToolDescriptor {
    ToolDescriptor {
        id: ToolId(format!("builtin::{name}")),
        name: name.into(),
        description: format!("{name} description"),
        input_schema: serde_json::json!({"type": "object"}),
        provider_id: "builtin".into(),
        annotations: ToolAnnotations::default(),
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

// ── AC-9-4-1: Re-export surface compiles ──

#[test]
fn test_re_export_surface_compiles() {
    // Compile-only: verify all types are reachable from crate root.
    let _kind = ExposureKind::StaticFull;
    let _payload = ExposurePayload::Tools(vec![]);
    let _outcome = RenderOutcome {
        payload: ExposurePayload::Tools(vec![]),
        diagnostics: RenderDiagnostics::clean(),
    };
    let _matrix = CapabilityMatrix::new();
}

// ── AC-9-4-3: StaticFullExposure passthrough ──

#[tokio::test]
async fn test_static_full_renders_full_catalog_unchanged() {
    let exposure = StaticFullExposure::new();
    let descriptors = vec![
        test_descriptor("Bash"),
        test_descriptor("Read"),
        test_descriptor("Write"),
    ];
    let catalog = FilteredCatalog::from_tool_descriptors(descriptors.clone());
    let outcome = exposure.render(&catalog, &test_caps()).await.unwrap();

    match outcome.payload {
        ExposurePayload::Tools(tools) => {
            assert_eq!(tools.len(), 3);
        }
        ExposurePayload::MetaTool(_) => panic!("StaticFullExposure must never emit MetaTool"),
    }
    assert!(!outcome.diagnostics.truncated);
    assert_eq!(outcome.diagnostics.dropped_count, 0);
    assert_eq!(outcome.diagnostics.reason, None);
}

#[tokio::test]
async fn test_static_full_on_catalog_changed_is_ok() {
    let exposure = StaticFullExposure::new();
    let delta = rustain::domain::models::CatalogDelta::empty(42);
    let result = exposure.on_catalog_changed(&delta).await;
    assert!(result.is_ok());
}

// ── AC-9-4-4: CapabilityMatrix stub ──

#[test]
fn test_capability_matrix_phase_a_returns_full() {
    let matrix = CapabilityMatrix::new();
    for transport in [
        TransportKind::Stdio,
        TransportKind::Http,
        TransportKind::Sse,
        TransportKind::InProcess,
    ] {
        let caps = ProviderCapabilities {
            supports_streaming: true,
            supports_list_changed: true,
            supports_native_retrieval: None,
            max_tool_count: None,
            transport_kind: transport,
        };
        assert_eq!(
            matrix.query(ExposureKind::StaticFull, &caps),
            Capability::Full,
            "Phase A stub must return Full for every provider (transport={:?})",
            transport
        );
    }
}

#[test]
fn test_capability_enum_is_non_exhaustive() {
    let cap = Capability::Full;
    // This match includes a wildcard arm, which compiles because of #[non_exhaustive].
    // Removing the wildcard would cause a compile error.
    #[allow(unreachable_patterns)]
    let _ = match cap {
        Capability::Full => "full",
        Capability::Degraded => "degraded",
        Capability::Incompatible => "incompatible",
        _ => "future",
    };
}

// ── AC-9-4-5: Config schema ──

#[test]
fn test_default_config_resolves_to_static_full() {
    let config = AppConfig::default();
    assert_eq!(config.tools.exposure, "static-full");
}

#[test]
fn test_pre_v94_config_round_trips() {
    let toml = "model = \"claude-sonnet-4-6\"\nlog_level = \"info\"\n";
    let config: AppConfig = toml::from_str(toml).expect("deserialize pre-9.4 config");
    assert_eq!(config.tools.exposure, "static-full");
}

#[cfg(not(feature = "meta-search"))]
#[test]
fn test_meta_search_config_rejected_with_actionable_error() {
    let err = rustain::infrastructure::startup::validate_tools_exposure("meta-search");
    let msg = format!("{}", err.unwrap_err());
    assert!(
        msg.contains("deferred to Story 9.7"),
        "error must mention Story 9.7 deferral: {}",
        msg
    );
    assert!(
        msg.contains("ADR-09-01"),
        "error must cite ADR-09-01: {}",
        msg
    );
}

#[test]
fn test_unknown_exposure_value_rejected() {
    let err = rustain::infrastructure::startup::validate_tools_exposure("semantic");
    let msg = format!("{}", err.unwrap_err());
    assert!(
        msg.contains("unknown exposure strategy"),
        "error must mention unknown strategy: {}",
        msg
    );
    assert!(
        msg.contains("static-full"),
        "error must suggest static-full: {}",
        msg
    );
}

// ── AC-9-4-6: CLI flag ──

#[test]
fn test_cli_flag_static_full_accepted() {
    let cli = Cli::parse_from(["rustain", "--tool-exposure", "static-full"]);
    assert_eq!(cli.tool_exposure, Some("static-full".into()));
}

#[cfg(not(feature = "meta-search"))]
#[test]
fn test_cli_flag_meta_search_rejected_by_clap() {
    let result = Cli::try_parse_from(["rustain", "--tool-exposure", "meta-search"]);
    assert!(result.is_err(), "clap must reject meta-search in Phase A");
}

// ── AC-9-4-7: Composition root ──

#[test]
fn test_compose_with_default_config_binds_static_full() {
    use rustain::domain::models::profile::ProfileSelection;
    use rustain::infrastructure::composition::{ComposeContext, build_tool_exposure};
    use std::path::PathBuf;

    let ctx = ComposeContext {
        workspace_path: PathBuf::from("/tmp/test"),
        project_context: ProjectContext::empty(),
        storage: std::sync::Arc::new(rustain::adapters::noop::NoOpStorage::default())
            as std::sync::Arc<dyn rustain::domain::ports::StoragePort>,
        skill_activator: std::sync::Arc::new(
            rustain::adapters::skill_activation::SkillActivator::new(),
        ),
        mcp_servers: Vec::new(),
        include_builtin_tools: true,
        domain_tx: None,
        tool_exposure: "static-full".into(),
        skill_exposure: "l1-metadata".into(),
        skill_cache: std::sync::Arc::new(
            rustain::infrastructure::skill_cache::SkillCache::new_in_memory(),
        ),
        sandbox_adapter: "noop".into(),
        sandbox_startup_policy: rustain::domain::models::sandbox::SandboxPolicy::Permissive,
        sandbox_slot: std::sync::Arc::new(arc_swap::ArcSwap::from_pointee(std::sync::Arc::new(
            rustain::adapters::sandbox::NoOpSandbox,
        )
            as std::sync::Arc<dyn rustain::domain::ports::SandboxManager>)),
        memory_slot: std::sync::Arc::new(arc_swap::ArcSwap::from_pointee(std::sync::Arc::new(
            rustain::adapters::noop::NoOpMemory,
        )
            as std::sync::Arc<dyn rustain::domain::ports::MemoryPort>)),
        sandbox_policy: std::sync::Arc::new(tokio::sync::RwLock::new(
            rustain::domain::models::sandbox::SandboxPolicy::Permissive,
        )),
        #[cfg(feature = "meta-search")]
        search_config: rustain::domain::models::SearchConfig::default(),
        #[cfg(feature = "meta-search")]
        meta_search_engine: None,
    };
    let selection = ProfileSelection {
        dimensions: std::collections::BTreeMap::new(),
    };
    let result = build_tool_exposure(&selection, &ctx);
    assert!(result.is_ok());
    let port = result.unwrap();
    assert!(port.is_some());
    assert_eq!(port.as_ref().unwrap().kind(), ExposureKind::StaticFull);
}

#[test]
fn test_compose_with_unknown_exposure_returns_error() {
    use rustain::domain::errors::AdapterCompositionError;
    use rustain::domain::models::profile::ProfileSelection;
    use rustain::infrastructure::composition::{ComposeContext, build_tool_exposure};
    use std::path::PathBuf;

    let ctx = ComposeContext {
        workspace_path: PathBuf::from("/tmp/test"),
        project_context: ProjectContext::empty(),
        storage: std::sync::Arc::new(rustain::adapters::noop::NoOpStorage::default())
            as std::sync::Arc<dyn rustain::domain::ports::StoragePort>,
        skill_activator: std::sync::Arc::new(
            rustain::adapters::skill_activation::SkillActivator::new(),
        ),
        mcp_servers: Vec::new(),
        include_builtin_tools: true,
        domain_tx: None,
        tool_exposure: "semantic".into(),
        skill_exposure: "l1-metadata".into(),
        skill_cache: std::sync::Arc::new(
            rustain::infrastructure::skill_cache::SkillCache::new_in_memory(),
        ),
        sandbox_adapter: "noop".into(),
        sandbox_startup_policy: rustain::domain::models::sandbox::SandboxPolicy::Permissive,
        sandbox_slot: std::sync::Arc::new(arc_swap::ArcSwap::from_pointee(std::sync::Arc::new(
            rustain::adapters::sandbox::NoOpSandbox,
        )
            as std::sync::Arc<dyn rustain::domain::ports::SandboxManager>)),
        memory_slot: std::sync::Arc::new(arc_swap::ArcSwap::from_pointee(std::sync::Arc::new(
            rustain::adapters::noop::NoOpMemory,
        )
            as std::sync::Arc<dyn rustain::domain::ports::MemoryPort>)),
        sandbox_policy: std::sync::Arc::new(tokio::sync::RwLock::new(
            rustain::domain::models::sandbox::SandboxPolicy::Permissive,
        )),
        #[cfg(feature = "meta-search")]
        search_config: rustain::domain::models::SearchConfig::default(),
        #[cfg(feature = "meta-search")]
        meta_search_engine: None,
    };
    let selection = ProfileSelection {
        dimensions: std::collections::BTreeMap::new(),
    };
    let result = build_tool_exposure(&selection, &ctx);
    match result {
        Err(AdapterCompositionError::UnknownAdapter { name, .. }) => {
            assert_eq!(name, "semantic");
        }
        other => {
            let _ = other; // suppress unused warning
            panic!("expected UnknownAdapter");
        }
    }
}

// ── AC-9-4-8: Asymmetry guard comment ──

#[test]
fn test_asymmetry_guard_comment_present() {
    let composition_src = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/infrastructure/composition/mod.rs"
    ))
    .expect("composition/mod.rs must exist");
    assert!(
        composition_src.contains("ASYMMETRY-BY-DESIGN"),
        "src/infrastructure/composition/mod.rs MUST contain the asymmetry-by-design \
         guard comment block per SCP-2026-05-21-skill-exposure-strategy John Round-2 \
         directive. The guard prevents silent symmetry restoration during composition \
         root refactors. See AC-9-4-8 in story 9.4."
    );
    assert!(
        composition_src.contains("ADR-09-01") && composition_src.contains("ADR-09-02"),
        "asymmetry guard comment MUST cite BOTH ADR-09-01 (Tools default) \
         AND ADR-09-02 (Skills default) — citing only one would not protect against \
         symmetric-default refactors per the John Round-2 directive"
    );
}

// ── AC-9-4-9: No CatalogObserverRegistry in Phase A (now Phase B shipped) ──

#[cfg(not(feature = "meta-search"))]
#[test]
fn test_phase_a_no_catalog_observer_registry_file() {
    let phase_b_file = std::path::Path::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/infrastructure/composition/catalog_observer_registry.rs"
    ));
    assert!(
        !phase_b_file.exists(),
        "src/infrastructure/composition/catalog_observer_registry.rs MUST NOT exist \
         in Phase A — that file is owned by Story 9.7 Phase B per ADR-09-01 v2.2 \
         §Phase B. If Story 9.7 has shipped, remove this test."
    );
}

// ── AC-9-4-10: Phase B forward-compat ──

#[test]
fn test_meta_search_variant_reserved() {
    // ExposureKind::MetaSearch exists in the type but no Phase A impl constructs it.
    let kind = ExposureKind::MetaSearch;
    let json = serde_json::to_string(&kind).unwrap();
    assert_eq!(json, "\"meta-search\"");
}

#[test]
fn test_meta_tool_payload_variant_reserved() {
    let descriptor = test_descriptor("search_tools");
    let _payload = ExposurePayload::MetaTool(descriptor);
    // Compile-only: the variant exists in the type.
}

#[test]
fn test_phase_a_no_meta_search_construction_in_production() {
    // Scan src/adapters/tool_exposure/ for MetaSearch or MetaTool constructions
    // outside of tests and the enum definition itself.
    let mod_rs = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/adapters/tool_exposure/mod.rs"
    ))
    .unwrap();
    let static_full_rs = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/adapters/tool_exposure/static_full.rs"
    ))
    .unwrap();
    let cap_matrix_rs = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/adapters/tool_exposure/capability_matrix.rs"
    ))
    .unwrap();

    // Count occurrences of MetaSearch outside enum definition and tests.
    // In mod.rs, it appears in the enum definition + doc comments.
    let mod_meta_search_count = mod_rs.matches("MetaSearch").count();
    // Expected: enum def (1) + doc comments (~3) + test (1) = ~5
    // We assert it's <= 10 to catch accidental new constructions.
    assert!(
        mod_meta_search_count <= 10,
        "Unexpected MetaSearch references in mod.rs ({}). Phase A must not construct MetaSearch.",
        mod_meta_search_count
    );

    // static_full.rs should NOT construct MetaSearch. Doc comments mentioning
    // MetaSearchExposure are OK (they're forward-compat documentation).
    // We check that there's no `ExposureKind::MetaSearch` or `ExposurePayload::MetaTool`
    // construction in the non-test code.
    let static_full_code: String = static_full_rs
        .lines()
        .take_while(|l| !l.contains("#[cfg(test)]"))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        !static_full_code.contains("ExposureKind::MetaSearch"),
        "static_full.rs must not construct ExposureKind::MetaSearch in Phase A"
    );
    assert!(
        !static_full_code.contains("ExposurePayload::MetaTool"),
        "static_full.rs must not construct ExposurePayload::MetaTool in Phase A"
    );

    // capability_matrix.rs may mention MetaSearch in doc comments and tests only.
    let cap_prod_code: String = cap_matrix_rs
        .lines()
        .take_while(|l| !l.contains("#[cfg(test)]"))
        .filter(|l| !l.trim_start().starts_with("//") && !l.trim_start().starts_with("//!"))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        !cap_prod_code.contains("MetaSearch"),
        "capability_matrix.rs must not reference MetaSearch in non-doc non-test code in Phase A"
    );
}
