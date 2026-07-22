//! ATDD red-phase acceptance scaffolds for ADR-10-5 / corrective story 10.7.1.
//!
//! Defect under test: the `task` tool is unreachable in the default (`coding`)
//! profile because no shipped profile builds a `CompositeToolsetAdapter`, and the
//! `"composite"` arm hard-requires MCP servers. See ADR-10-5 (proposed 2026-06-18)
//! and the improved resolution plan (S1 compile-time gate swap → S2 flip coding to
//! composite → S3 carve-out + hint text).
//!
//! These tests are the RED phase. Run them on `main` to watch them fail:
//!     cargo test --test integration_default_profile_exposes_task -- --ignored
//! Amelia turns each GREEN per the implementation checklist; the `#[ignore]` is
//! removed as each lands. Test IDs follow `{EPIC}.{STORY}-{LEVEL}-{SEQ}`.
//!
//! APIs verified against:
//!   - tests/conformance_subagent_provider_protocol.rs (SubagentProvider::new + stubs)
//!   - tests/conformance_adapter_composition.rs       (ComposeContext + build_tools)
//!   - tests/integration_mcp_routing.rs               (CompositeToolsetAdapter::new)
//!   - tests/profile_loading.rs                       (TomlProfileResolver::resolve_active)

#![cfg(feature = "mcp")] // composite + populate_registry are mcp-gated today (ADR-10-5 §out-of-scope)

use std::sync::Arc;

use rustain::adapters::agent_registry::AgentRegistry;
use rustain::adapters::profile_resolver::toml_resolver::TomlProfileResolver;
use rustain::adapters::subagent::SubagentProvider;
use rustain::domain::models::PortDimension;
use rustain::domain::ports::{ProfileResolver, ProviderInfoPort, SubagentRunner};
use rustain::infrastructure::composition::{ComposeContext, build_tools};
use rustain::infrastructure::subagent::{NodeTree, SubagentSpool};

// ───────────────────────── helpers (mirror conformance_subagent_provider_protocol.rs) ─────────────────────────

struct StubRunner;

#[async_trait::async_trait]
impl SubagentRunner for StubRunner {
    async fn launch(
        &self,
        _spec: rustain::domain::models::AgentLaunchSpec,
        _cancel: tokio_util::sync::CancellationToken,
        _parent: Option<&rustain::domain::models::TaskHandle>,
        _agent_id: rustain::domain::models::AgentId,
    ) -> Result<rustain::domain::models::TaskHandle, rustain::domain::models::SubagentError> {
        unimplemented!("reachability test does not spawn")
    }
}

struct StubInfo;
impl ProviderInfoPort for StubInfo {
    fn active_delegate_id(&self) -> String {
        "stub".into()
    }
    fn get_model(
        &self,
        _provider_id: &str,
        _model_id: &str,
    ) -> Option<rustain::domain::models::provider::ModelDescriptor> {
        None
    }
    fn get_model_provider(&self, _model_id: &str, _prefer: Option<&str>) -> Option<String> {
        None
    }
    fn list_providers(&self) -> Vec<rustain::domain::models::provider::ProviderDescriptor> {
        Vec::new()
    }
    fn list_models_by_provider(
        &self,
        _provider_id: &str,
    ) -> Vec<rustain::domain::models::provider::ModelDescriptor> {
        Vec::new()
    }
    fn get_provider(
        &self,
        _provider_id: &str,
    ) -> Option<Arc<dyn rustain::domain::ports::StreamingProvider>> {
        None
    }
    fn set_active_provider(
        &self,
        _provider_id: &str,
    ) -> Result<(), rustain::domain::errors::ProviderError> {
        Ok(())
    }
    fn now_unix(&self) -> i64 {
        0
    }
    fn today_start_unix_ms(&self) -> i64 {
        0
    }
}

/// Compose context with ZERO mcp servers — the crux of the defect.
fn ctx_zero_mcp() -> ComposeContext {
    ComposeContext {
        workspace_path: std::path::PathBuf::from("/tmp/test-adr-10-5"),
        project_context: rustain::domain::models::project_context::ProjectContext::empty(),
        storage: Arc::new(rustain::adapters::noop::NoOpStorage)
            as Arc<dyn rustain::domain::ports::StoragePort>,
        skill_activator: Arc::new(rustain::adapters::skill_activation::SkillActivator::new()),
        mcp_servers: Vec::new(),
        include_builtin_tools: true,
        domain_tx: None,
        channel_turn_tx: None,
        tool_exposure: "static-full".into(),
        assembler: "passthrough".into(),
        skill_exposure: "l1-metadata".into(),
        skill_cache: Arc::new(rustain::infrastructure::skill_cache::SkillCache::new_in_memory()),
        sandbox_adapter: "noop".into(),
        sandbox_startup_policy: rustain::domain::models::sandbox::SandboxPolicy::Permissive,
        sandbox_slot: Arc::new(arc_swap::ArcSwap::from_pointee(Arc::new(
            rustain::adapters::sandbox::NoOpSandbox,
        )
            as Arc<dyn rustain::domain::ports::SandboxManager>)),
        memory_slot: Arc::new(arc_swap::ArcSwap::from_pointee(Arc::new(
            rustain::adapters::noop::NoOpMemory,
        )
            as Arc<dyn rustain::domain::ports::MemoryPort>)),
        sandbox_policy: Arc::new(tokio::sync::RwLock::new(
            rustain::domain::models::sandbox::SandboxPolicy::Permissive,
        )),
        memory_write_gate: Arc::new(tokio::sync::RwLock::new(())),
        #[cfg(feature = "meta-search")]
        search_config: rustain::domain::models::SearchConfig::default(),
        #[cfg(feature = "meta-search")]
        meta_search_engine: None,
        a2a_peers: Vec::new(),
    }
}

/// The real `coding` profile's resolved Tools-adapter name (embedded profile).
fn coding_tools_adapter_name() -> String {
    let tmpdir = tempfile::tempdir().unwrap();
    let profile = TomlProfileResolver::new("coding", tmpdir.path().to_path_buf())
        .unwrap()
        .resolve_active()
        .unwrap();
    profile.selection.dimensions[&PortDimension::Tools]
        .adapter
        .clone()
}

/// Resolve `coding`, build its tools, wire a real SubagentProvider, populate the
/// registry, return the resolved tool names. Mirrors the real startup sequence.
async fn coding_resolved_tool_names() -> Vec<String> {
    let adapter_name = coding_tools_adapter_name();
    let tools = build_tools(&adapter_name, None, &ctx_zero_mcp()).unwrap();
    let composite = tools
        .as_any()
        .downcast_ref::<rustain::adapters::composite_toolset_adapter::CompositeToolsetAdapter>()
        .expect("coding profile must compose a CompositeToolsetAdapter after ADR-10-5 S2");

    // Wire a real SubagentProvider (real startup does this in startup.rs Block A).
    let runner = Arc::new(StubRunner) as Arc<dyn SubagentRunner>;
    let registry = Arc::new(NodeTree::new());
    let agent_registry = Arc::new(tokio::sync::RwLock::new(AgentRegistry::new()));
    let model_router = Arc::new(StubInfo) as Arc<dyn ProviderInfoPort>;
    let tmp = tempfile::tempdir().unwrap();
    let spool = Arc::new(SubagentSpool::new(tmp.path().join("spool")).await.unwrap());
    let provider = Arc::new(SubagentProvider::new(
        runner,
        registry,
        agent_registry,
        model_router,
        spool,
    ));
    composite.set_subagent_provider(provider);
    let _ = composite.populate_registry().await;

    tools
        .available_tools()
        .into_iter()
        .map(|t| t.name)
        .collect()
}

// ───────────────────────── S1 — `"composite"` accepts an empty mcp_clients vec ─────────────────────────

/// 10.7.1-INT-001 · P0 · Given a ComposeContext with zero MCP servers, When
/// `build_tools("composite", …)` is called, Then it returns Ok (today it hard-errors
/// at composition/mod.rs:543-549). Turns green with S1.
#[tokio::test]
async fn test_build_tools_composite_succeeds_with_zero_mcp_servers() {
    let ctx = ctx_zero_mcp();
    let tools = build_tools("composite", None, &ctx);
    assert!(
        tools.is_ok(),
        "composite must compose with zero MCP servers after S1 (build_tools hard-errors today at composition/mod.rs:543-549)"
    );
}

/// 10.7.1-INT-002 · P0 · Given the `coding` profile, When resolved, Then its Tools
/// adapter is `composite` (today it is `builtin-full`). Turns green with S2.
#[test]
fn test_coding_profile_resolves_to_composite_adapter() {
    assert_eq!(coding_tools_adapter_name(), "composite");
}

// ───────────────────────── D4 — the missing acceptance criterion (headline) ─────────────────────────

/// 10.7.1-INT-003 · P0 · Given the default `coding` profile with zero MCP servers,
/// When the real startup tool-wiring sequence runs, Then the resolved tool list
/// contains `task`. THIS is the acceptance criterion Epic 10 never had. Fails on
/// `main` (no `task` tool); green after S1+S2.
#[tokio::test]
async fn test_coding_profile_with_zero_mcp_exposes_task_tool() {
    let names = coding_resolved_tool_names().await;
    assert!(
        names.iter().any(|n| n == "task"),
        "default coding profile must expose the `task` tool; got: {names:?}"
    );
}

/// 10.7.1-INT-004 · P0 · companion read_task_output tool is exposed alongside task.
#[tokio::test]
async fn test_coding_profile_exposes_read_task_output() {
    let names = coding_resolved_tool_names().await;
    assert!(
        names.iter().any(|n| n == "read_task_output"),
        "default coding profile must expose `read_task_output`; got: {names:?}"
    );
}
