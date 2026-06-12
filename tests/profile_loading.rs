#![allow(dead_code)] // AI-12.1: test fixture scaffolding
//! Integration tests for profile loading — Story 8.2.
//!
//! Exercises TomlProfileResolver end-to-end with temp directories,
//! custom profiles, embedded fallback, and reload behavior.

use std::sync::Arc;

use arc_swap::ArcSwap;

use rustain::adapters::profile_resolver::toml_resolver::TomlProfileResolver;
use rustain::domain::models::PortDimension;
use rustain::domain::ports::ProfileResolver;

fn full_base_toml() -> &'static str {
    r#"name = "base"
[persona]
adapter = "minimal"
[memory]
adapter = "noop"
[session]
adapter = "basic"
[tools]
adapter = "builtin-only"
[channels]
adapter = "terminal"
[scheduler]
adapter = "none"
[context]
adapter = "default"
"#
}

#[test]
fn load_coding_from_embedded() {
    let tmpdir = tempfile::tempdir().unwrap();
    let resolver = TomlProfileResolver::new("coding", tmpdir.path().to_path_buf()).unwrap();
    let profile = resolver.resolve_active().unwrap();
    assert_eq!(profile.name, "coding");
    assert!(
        profile
            .selection
            .dimensions
            .contains_key(&PortDimension::Persona)
    );
    assert_eq!(
        profile.selection.dimensions[&PortDimension::Persona].adapter,
        "coding"
    );
}

#[test]
fn load_personal_assistant_from_embedded() {
    let tmpdir = tempfile::tempdir().unwrap();
    let resolver =
        TomlProfileResolver::new("personal-assistant", tmpdir.path().to_path_buf()).unwrap();
    let profile = resolver.resolve_active().unwrap();
    assert_eq!(profile.name, "personal-assistant");
    assert!(profile.preview);
}

#[test]
fn custom_profile_extends_coding() {
    let tmpdir = tempfile::tempdir().unwrap();
    let custom = r#"
name = "my-dev"
extends = "coding"
[memory]
adapter = "daily-log"
"#;
    std::fs::write(tmpdir.path().join("my-dev.toml"), custom).unwrap();
    let resolver = TomlProfileResolver::new("my-dev", tmpdir.path().to_path_buf()).unwrap();
    let profile = resolver.resolve_active().unwrap();
    assert_eq!(profile.name, "my-dev");
    assert_eq!(
        profile.selection.dimensions[&PortDimension::Memory].adapter,
        "daily-log"
    );
    assert_eq!(
        profile.selection.dimensions[&PortDimension::Persona].adapter,
        "coding"
    );
}

#[test]
fn custom_coding_overrides_embedded() {
    let tmpdir = tempfile::tempdir().unwrap();
    let custom = r#"
name = "coding"
extends = "base"
[persona]
adapter = "personal-assistant"
[memory]
adapter = "daily-log"
[session]
adapter = "basic"
[tools]
adapter = "builtin-only"
[channels]
adapter = "terminal"
[scheduler]
adapter = "none"
[context]
adapter = "default"
"#;
    std::fs::write(tmpdir.path().join("coding.toml"), custom).unwrap();
    let resolver = TomlProfileResolver::new("coding", tmpdir.path().to_path_buf()).unwrap();
    let profile = resolver.resolve_active().unwrap();
    assert_eq!(
        profile.selection.dimensions[&PortDimension::Persona].adapter,
        "personal-assistant"
    );
}

#[test]
fn nonexistent_profile_returns_error() {
    let tmpdir = tempfile::tempdir().unwrap();
    let result = TomlProfileResolver::new("no-such-profile", tmpdir.path().to_path_buf());
    assert!(result.is_err());
}

#[test]
fn reload_swaps_profile_resolver() {
    let tmpdir = tempfile::tempdir().unwrap();
    let resolver1 = TomlProfileResolver::new("coding", tmpdir.path().to_path_buf()).unwrap();
    let swap: Arc<ArcSwap<Arc<dyn ProfileResolver>>> = Arc::new(ArcSwap::from_pointee(Arc::new(
        resolver1,
    )
        as Arc<dyn ProfileResolver>));

    let profile1 = swap.load_full().resolve_active().unwrap();
    assert_eq!(profile1.name, "coding");

    let resolver2 = TomlProfileResolver::new("base", tmpdir.path().to_path_buf()).unwrap();
    swap.store(Arc::new(Arc::new(resolver2) as Arc<dyn ProfileResolver>));

    let profile2 = swap.load_full().resolve_active().unwrap();
    assert_eq!(profile2.name, "base");
}

#[test]
fn preview_warning_emitted_once() {
    let tmpdir = tempfile::tempdir().unwrap();
    let resolver =
        TomlProfileResolver::new("personal-assistant", tmpdir.path().to_path_buf()).unwrap();
    assert!(resolver.take_preview_warning().is_some());
    assert!(resolver.take_preview_warning().is_none());
}

// ── Story 8.3 composition integration tests ──

#[test]
fn test_coding_profile_composes_seven_ports() {
    let tmpdir = tempfile::tempdir().unwrap();
    let resolver = TomlProfileResolver::new("coding", tmpdir.path().to_path_buf()).unwrap();
    let profile = resolver.resolve_active().unwrap();

    let ctx = rustain::infrastructure::composition::ComposeContext {
        workspace_path: tmpdir.path().to_path_buf(),
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
        memory_slot: std::sync::Arc::new(arc_swap::ArcSwap::from_pointee(std::sync::Arc::new(
            rustain::adapters::noop::NoOpMemory,
        )
            as std::sync::Arc<dyn rustain::domain::ports::MemoryPort>)),
        sandbox_policy: Arc::new(tokio::sync::RwLock::new(
            rustain::domain::models::sandbox::SandboxPolicy::Permissive,
        )),
        memory_write_gate: Arc::new(tokio::sync::RwLock::new(())),
        #[cfg(feature = "meta-search")]
        search_config: rustain::domain::models::SearchConfig::default(),
        #[cfg(feature = "meta-search")]
        meta_search_engine: None,
    };

    let core = rustain::infrastructure::runtime::agent_core::AgentCore::compose(
        &profile.name,
        &profile.selection,
        &ctx,
    )
    .expect("coding profile composition should succeed");

    let _p = core.persona.load_full();
    let _m = core.memory.load_full();
    let _s = core.session.load_full();
    let _t = core.tools.load_full();
    let _ch = core.channels.load_full();
    let _sc = core.scheduler.load_full();
    let _cx = core.context.load_full();
}

#[test]
fn test_base_profile_composes_all_noop() {
    let tmpdir = tempfile::tempdir().unwrap();
    let resolver = TomlProfileResolver::new("base", tmpdir.path().to_path_buf()).unwrap();
    let profile = resolver.resolve_active().unwrap();

    assert_eq!(
        profile.selection.dimensions[&PortDimension::Persona].adapter,
        "minimal"
    );
    assert_eq!(
        profile.selection.dimensions[&PortDimension::Tools].adapter,
        "builtin-only"
    );

    let ctx = rustain::infrastructure::composition::ComposeContext {
        workspace_path: tmpdir.path().to_path_buf(),
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
        memory_slot: std::sync::Arc::new(arc_swap::ArcSwap::from_pointee(std::sync::Arc::new(
            rustain::adapters::noop::NoOpMemory,
        )
            as std::sync::Arc<dyn rustain::domain::ports::MemoryPort>)),
        sandbox_policy: Arc::new(tokio::sync::RwLock::new(
            rustain::domain::models::sandbox::SandboxPolicy::Permissive,
        )),
        memory_write_gate: Arc::new(tokio::sync::RwLock::new(())),
        #[cfg(feature = "meta-search")]
        search_config: rustain::domain::models::SearchConfig::default(),
        #[cfg(feature = "meta-search")]
        meta_search_engine: None,
    };

    let core = rustain::infrastructure::runtime::agent_core::AgentCore::compose(
        &profile.name,
        &profile.selection,
        &ctx,
    )
    .expect("base profile composition should succeed");

    let _ = core.persona.load_full();
    let _ = core.tools.load_full();
}

#[test]
fn test_personal_assistant_preview_composes_with_fallback() {
    let tmpdir = tempfile::tempdir().unwrap();
    let resolver =
        TomlProfileResolver::new("personal-assistant", tmpdir.path().to_path_buf()).unwrap();
    let profile = resolver.resolve_active().unwrap();

    let ctx = rustain::infrastructure::composition::ComposeContext {
        workspace_path: tmpdir.path().to_path_buf(),
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
        memory_slot: std::sync::Arc::new(arc_swap::ArcSwap::from_pointee(std::sync::Arc::new(
            rustain::adapters::noop::NoOpMemory,
        )
            as std::sync::Arc<dyn rustain::domain::ports::MemoryPort>)),
        sandbox_policy: Arc::new(tokio::sync::RwLock::new(
            rustain::domain::models::sandbox::SandboxPolicy::Permissive,
        )),
        memory_write_gate: Arc::new(tokio::sync::RwLock::new(())),
        #[cfg(feature = "meta-search")]
        search_config: rustain::domain::models::SearchConfig::default(),
        #[cfg(feature = "meta-search")]
        meta_search_engine: None,
    };

    let core = rustain::infrastructure::runtime::agent_core::AgentCore::compose(
        &profile.name,
        &profile.selection,
        &ctx,
    )
    .expect("personal-assistant composition should succeed (with fallbacks)");

    // channels and scheduler should have fallen back to terminal/none
    assert_eq!(
        profile.selection.dimensions[&PortDimension::Channels].adapter,
        "terminal"
    );
    assert_eq!(
        profile.selection.dimensions[&PortDimension::Scheduler].adapter,
        "none"
    );
    let _ch = core.channels.load_full();
    let _sc = core.scheduler.load_full();
}

#[test]
fn test_reload_recomposes_agent_core() {
    let tmpdir = tempfile::tempdir().unwrap();

    // Compose with coding profile
    let resolver1 = TomlProfileResolver::new("coding", tmpdir.path().to_path_buf()).unwrap();
    let profile1 = resolver1.resolve_active().unwrap();

    let ctx = rustain::infrastructure::composition::ComposeContext {
        workspace_path: tmpdir.path().to_path_buf(),
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
        memory_slot: std::sync::Arc::new(arc_swap::ArcSwap::from_pointee(std::sync::Arc::new(
            rustain::adapters::noop::NoOpMemory,
        )
            as std::sync::Arc<dyn rustain::domain::ports::MemoryPort>)),
        sandbox_policy: Arc::new(tokio::sync::RwLock::new(
            rustain::domain::models::sandbox::SandboxPolicy::Permissive,
        )),
        memory_write_gate: Arc::new(tokio::sync::RwLock::new(())),
        #[cfg(feature = "meta-search")]
        search_config: rustain::domain::models::SearchConfig::default(),
        #[cfg(feature = "meta-search")]
        meta_search_engine: None,
    };
    let ctx_arc = Arc::new(ctx);

    let core1 = Arc::new(
        rustain::infrastructure::runtime::agent_core::AgentCore::compose(
            &profile1.name,
            &profile1.selection,
            &ctx_arc,
        )
        .unwrap(),
    );

    let old_persona_ptr = Arc::as_ptr(&core1.persona.load_full()) as usize;

    // Simulate reload: compose with base profile, swap per-port
    let resolver2 = TomlProfileResolver::new("base", tmpdir.path().to_path_buf()).unwrap();
    let profile2 = resolver2.resolve_active().unwrap();

    let core2 = rustain::infrastructure::runtime::agent_core::AgentCore::compose(
        &profile2.name,
        &profile2.selection,
        &ctx_arc,
    )
    .unwrap();

    // Swap persona port
    core1.persona.store(Arc::clone(&*core2.persona.load()));

    let new_persona_ptr = Arc::as_ptr(&core1.persona.load_full()) as usize;
    assert_ne!(
        old_persona_ptr, new_persona_ptr,
        "persona pointer should change after reload re-composition"
    );
}
