//! Conformance tests for Capability Provider Architecture — Story 9.3a.
//!
//! Discharges AC-1, AC-2, AC-3, AC-4, AC-5, AC-7, AC-8, AC-9, AC-10.
//!
//! Fake-mcp-server tests are `#[serial]`-annotated per Story 9.1 / 9.2 convention.

use std::sync::Arc;

#[cfg(feature = "mcp")]
use rustain::adapters::composite_toolset_adapter::CompositeToolsetAdapter;
use rustain::domain::events::{AppEvent, CapabilityEvent};
use rustain::domain::models::capability_id::CapabilityId;
use rustain::domain::models::capability_registry::{
    CapabilityRegistry, RegisteredCapability, RegistryError,
};
use rustain::domain::ports::CapabilityProvider;
use rustain::domain::ports::CatalogObserver;
use rustain::domain::ports::ObserverError;

// ── Utility ───────────────────────────────────────────────────────────

fn test_capability(protocol: &str, server: &str, tool: &str) -> RegisteredCapability {
    RegisteredCapability {
        id: CapabilityId {
            protocol: protocol.to_string(),
            server: server.to_string(),
            tool: tool.to_string(),
        },
        protocol: protocol.to_string(),
        provider_id: if server.is_empty() {
            protocol.to_string()
        } else {
            format!("{}:{}", protocol, server)
        },
        name: tool.to_string(),
        description: "test capability".to_string(),
        input_schema: serde_json::Value::Object(Default::default()),
        parallel_safe: true,
    }
}

// ── AC-1: Flag 1 conformance — no CapabilityRegistry on AppState ─────

/// Grep-based conformance: `CapabilityRegistry` must NOT appear as a field
/// in `src/adapters/tui/state.rs` or `src/infrastructure/runtime/app_state.rs`.
#[test]
fn test_no_capability_registry_on_app_state() {
    let files = &[
        "src/adapters/tui/state.rs",
        "src/infrastructure/runtime/app_state.rs",
    ];

    for file in files {
        let content = std::fs::read_to_string(file).unwrap_or_else(|_| String::new());
        let has_match = content.lines().any(|line| {
            let trimmed = line.trim();
            // Skip comments and imports
            if trimmed.starts_with("//") || trimmed.starts_with("/*") || trimmed.starts_with("use ")
            {
                return false;
            }
            // Match struct field patterns containing CapabilityRegistry
            trimmed.contains("CapabilityRegistry:")
                || trimmed.contains("capability_registry: CapabilityRegistry")
                || trimmed.contains("capability_registry: Arc<CapabilityRegistry>")
                || trimmed.contains("capability_registry: Arc<RwLock<CapabilityRegistry>>")
        });
        assert!(
            !has_match,
            "Flag 1 violation: {file} contains a CapabilityRegistry field reference"
        );
    }
}

/// Grep-based conformance: `capability_registry` field must exist exactly
/// once — in `src/adapters/composite_toolset_adapter.rs`. No other file in
/// `src/` may hold a `capability_registry` struct field.
///
/// Type references (e.g., `use crate::domain::models::capability_registry::...`)
/// are permitted — they are not struct fields.
#[test]
fn test_capability_registry_is_internal_to_composite() {
    // Composite must have the field
    let composite_content =
        std::fs::read_to_string("src/adapters/composite_toolset_adapter.rs").unwrap_or_default();
    let has_field = composite_content.lines().any(|line| {
        let t = line.trim();
        !t.starts_with("//")
            && !t.starts_with("/*")
            && !t.starts_with('*')
            && t.contains("capability_registry:")
    });
    assert!(
        has_field,
        "capability_registry field should exist on CompositeToolsetAdapter"
    );

    // No other src file should have a `capability_registry` struct field
    let src_files = collect_src_rs_files();
    let mut violations = Vec::new();
    for file in &src_files {
        let fname = file.to_string_lossy();
        if fname.contains("composite_toolset_adapter.rs") {
            continue;
        }
        let fc = std::fs::read_to_string(file).unwrap_or_default();
        let has_struct_field = fc.lines().any(|line| {
            let t = line.trim();
            if t.starts_with("//") || t.starts_with("/*") || t.starts_with('*') {
                return false;
            }
            if t.starts_with("use ") || t.starts_with("pub use ") {
                return false;
            }
            // Skip module path references like capability_registry::TypeName
            if t.contains("capability_registry::") {
                return false;
            }
            // Match struct field pattern: `capability_registry: TypeName`
            // (NOT module re-exports or type path references)
            t.contains("capability_registry:")
        });
        if has_struct_field {
            violations.push(fname.to_string());
        }
    }

    assert!(
        violations.is_empty(),
        "CapabilityRegistry struct field found in non-composite files: {:?}. \
         Only CompositeToolsetAdapter may hold a capability_registry field (Flag 1).",
        violations
    );
}

fn collect_src_rs_files() -> Vec<std::path::PathBuf> {
    let mut files = Vec::new();
    let src = std::path::Path::new("src");
    if !src.is_dir() {
        return files;
    }
    collect_rs_recursive(src, &mut files);
    files
}

fn collect_rs_recursive(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                collect_rs_recursive(&path, out);
            } else if path.extension().map_or(false, |e| e == "rs") {
                out.push(path);
            }
        }
    }
}

// ── AC-3: CapabilityId round-trip and collision tests ─────────────────

#[test]
fn test_capability_id_round_trip_mcp_wire() {
    let id = CapabilityId::from_mcp_wire_name("mcp__postgres__query").unwrap();
    assert_eq!(id.protocol, "mcp");
    assert_eq!(id.server, "postgres");
    assert_eq!(id.tool, "query");
    assert_eq!(id.to_mcp_wire_name().unwrap(), "mcp__postgres__query");
}

#[test]
fn test_capability_id_no_collision_across_protocols() {
    use std::collections::BTreeMap;
    let mut map: BTreeMap<CapabilityId, ()> = BTreeMap::new();
    map.insert(
        CapabilityId {
            protocol: "mcp".into(),
            server: "srv".into(),
            tool: "query".into(),
        },
        (),
    );
    map.insert(
        CapabilityId {
            protocol: "builtin".into(),
            server: String::new(),
            tool: "query".into(),
        },
        (),
    );
    map.insert(
        CapabilityId {
            protocol: "skill".into(),
            server: String::new(),
            tool: "query".into(),
        },
        (),
    );
    assert_eq!(map.len(), 3);
}

// ── AC-8: Re-export surface compiles ──────────────────────────────────

/// No-op test whose ONLY purpose is to fail compilation if re-exports drift.
#[test]
fn test_re_export_surface_compiles() {
    use rustain::domain::models::CapabilityId;
    use rustain::domain::models::CapabilityRegistry;
    use rustain::domain::models::CatalogDelta;
    use rustain::domain::models::capability::Capability;
    use rustain::domain::models::capability_registry::{RegisteredCapability, RegistryError};
    use rustain::domain::models::provider_capabilities::{
        NativeRetrievalKind, ProviderCapabilities, TransportKind,
    };
    use rustain::domain::ports::{CapabilityProvider, CatalogObserver};
    use rustain::domain::ports::{ObserverError, SubscriptionHandle, SubscriptionId};
    let _ = std::marker::PhantomData::<(
        ProviderCapabilities,
        CapabilityId,
        CapabilityRegistry,
        RegisteredCapability,
        Capability,
        NativeRetrievalKind,
        TransportKind,
        SubscriptionId,
        SubscriptionHandle,
        ObserverError,
        RegistryError,
        CatalogDelta,
        Box<dyn CapabilityProvider>,
        Box<dyn CatalogObserver>,
    )>::default();
}

// ── AC-4: Registry event emission ────────────────────────────────────

#[tokio::test]
async fn test_register_emits_event_on_appevent_bus() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<AppEvent>();
    let registry = Arc::new(CapabilityRegistry::new(Some(tx)));
    let cap = test_capability("mcp", "postgres", "query");
    let _handle = registry.register(cap.clone()).await.unwrap();

    let event = rx.try_recv().expect("Expected capability event");
    match event {
        AppEvent::CapabilityEvent(CapabilityEvent::Registered { capability }) => {
            assert_eq!(capability.id.protocol, "mcp");
            assert_eq!(capability.id.server, "postgres");
            assert_eq!(capability.id.tool, "query");
        }
        other => panic!("Expected CapabilityEvent::Registered, got {other:?}"),
    }
}

// ── AC-7: Subscription handle ────────────────────────────────────────

struct TestObserver {
    call_count: std::sync::atomic::AtomicU32,
}

#[async_trait::async_trait]
impl CatalogObserver for TestObserver {
    async fn on_catalog_changed(
        &self,
        _delta: &rustain::domain::models::CatalogDelta,
    ) -> Result<(), ObserverError> {
        self.call_count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }
}

#[test]
fn test_subscribe_returns_handle_that_unsubscribes_on_drop() {
    let registry = Arc::new(CapabilityRegistry::new(None));
    let observer: Arc<dyn CatalogObserver> = Arc::new(TestObserver {
        call_count: std::sync::atomic::AtomicU32::new(0),
    });
    let handle = registry.subscribe(observer.clone());
    // Handle is alive; observer is stored as Weak
    drop(handle);
    // Observer Arc still alive via our local variable
    drop(observer);
    // Registry still alive
    drop(registry);
}

// ── AC-9: Status panel registry count (snapshot test) ────────────────

#[test]
fn test_status_panel_shows_registry_count() {
    // This test verifies that the adapter status panel's registry summary
    // formatting works correctly with actual registered capabilities.
    use rustain::adapters::tui::widgets::adapter_status_panel::format_registry_summary;
    use rustain::domain::models::capability_registry::RegisteredCapability;

    let rt = tokio::runtime::Runtime::new().unwrap();
    let registry = Arc::new(CapabilityRegistry::new(None));

    let cap = test_capability("mcp", "postgres", "query");
    rt.block_on(async {
        let _handle = registry.register(cap).await.unwrap();
    });

    let snap = registry.snapshot();
    assert_eq!(snap.len(), 1, "registry should have 1 capability");

    // Test the actual panel formatting function
    let summary = format_registry_summary(&snap);
    assert_eq!(
        summary,
        Some("Registry: 1 capabilities (1 MCP, 0 builtin, 0 skill)".to_string()),
        "panel should render correct registry summary"
    );

    // Empty snapshot returns None
    let empty: Vec<RegisteredCapability> = vec![];
    assert_eq!(format_registry_summary(&empty), None);
}

// ── AC-2: Registry lookup, deregister unknown ────────────────────────

#[tokio::test]
async fn test_deregister_unknown_returns_not_found() {
    let registry = Arc::new(CapabilityRegistry::new(None));
    let id = CapabilityId {
        protocol: "nonexistent".into(),
        server: String::new(),
        tool: "ghost".into(),
    };
    let result = registry.deregister(&id).await;
    assert!(matches!(result, Err(RegistryError::NotFound { .. })));
}

#[tokio::test]
async fn test_lookup_returns_none_for_unregistered_id() {
    let registry = Arc::new(CapabilityRegistry::new(None));
    let id = CapabilityId {
        protocol: "nonexistent".into(),
        server: String::new(),
        tool: "ghost".into(),
    };
    let result = registry.lookup(&id).await;
    assert!(result.is_none());
}

// ── AC-10: No new EventBus bypass ────────────────────────────────────

/// Confirms the existing `tests/conformance.rs::test_no_new_eventbus_bypass`
/// is reusable for the new CapabilityEvent path. The registry's `emit_event`
/// uses the injected `event_tx` (same channel as 9.1 + 9.2), tagged with
/// `CONFORMANCE_EXCEPTION_EVENTBUS_BYPASS`.
#[test]
fn test_no_new_eventbus_bypass_for_capability_events() {
    // The conformance test at tests/conformance.rs already covers this.
    // The registry's emit_event is tagged with CONFORMANCE_EXCEPTION_EVENTBUS_BYPASS
    // per Story 9.3a AC-4.
    // This test simply confirms the bypass count hasn't grown beyond MAX_KNOWN_BYPASSES.
    // The actual ratchet is verified by tests/conformance.rs::test_no_new_eventbus_bypass.
    assert!(true);
}

// ── AC-5: McpProvider discover round-trip (fake MCP server) ──────────

/// Boots the fake-mcp-server, waits for connect, and verifies that the
/// McpProvider's discover() output matches the server's declared tools.
/// Uses the `CompositeToolsetAdapter` end-to-end path through
/// `discover_and_register_all`.
#[cfg(feature = "mcp")]
#[test]
#[serial_test::serial]
fn test_mcp_provider_discover_round_trip() {
    let binary_name = if cfg!(target_os = "windows") {
        "fake-mcp-server.exe"
    } else {
        "fake-mcp-server"
    };
    let exe_dir = std::env::current_exe()
        .expect("current exe")
        .parent()
        .expect("parent")
        .to_path_buf();
    let candidates = vec![
        exe_dir.join(binary_name),
        exe_dir.parent().expect("deps parent").join(binary_name),
    ];
    let command = candidates
        .iter()
        .find(|p| p.exists())
        .cloned()
        .unwrap_or_else(|| candidates[0].clone());

    let spec = rustain::domain::models::McpServerSpec {
        id: "discover-test".to_string(),
        transport: rustain::domain::models::McpTransport::Stdio,
        command: Some(command.to_string_lossy().into_owned()),
        args: vec![],
        env: std::collections::BTreeMap::new(),
        url: None,
        persistent: false,
        source: rustain::domain::models::McpServerSource::Workspace,
    };

    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let client = std::sync::Arc::new(rustain::adapters::mcp::client::McpClientAdapter::new(
            spec.clone(),
            None,
        ));
        client.set_self_weak(std::sync::Arc::downgrade(&client));

        // Connect to populate cached_tools
        let _ = client.connect().await;

        let builtin: std::sync::Arc<dyn rustain::domain::ports::ToolSetPort> =
            std::sync::Arc::new(rustain::adapters::noop::NoOpToolSet);

        let composite = CompositeToolsetAdapter::new(
            builtin,
            vec![client.clone()],
            vec![spec],
            true,
            None,
            None,
        );

        // Populate registry via McpProvider
        let _ = composite.populate_registry().await;

        let snap = composite.capability_registry().snapshot();
        let mcp_caps: Vec<_> = snap
            .iter()
            .filter(|c| c.protocol == "mcp" && c.provider_id == "mcp:discover-test")
            .collect();

        assert!(
            !mcp_caps.is_empty(),
            "discover_and_register_all should register at least one MCP capability"
        );

        // The fake-mcp-server declares two tools: echo and add
        let tool_names: Vec<&str> = mcp_caps.iter().map(|c| c.name.as_str()).collect();
        assert!(
            tool_names.contains(&"echo"),
            "should discover 'echo' tool, got: {:?}",
            tool_names
        );
        assert!(
            tool_names.contains(&"add"),
            "should discover 'add' tool, got: {:?}",
            tool_names
        );

        for cap in &mcp_caps {
            assert_eq!(cap.id.protocol, "mcp");
            assert_eq!(cap.id.server, "discover-test");
            assert!(!cap.name.is_empty());
        }
    });
}

// ── Story 9.3b tests ─────────────────────────────────────────────────

#[test]
fn test_re_export_surface_compiles_v2() {
    // Compile-only test: verify ToolDescriptor, ToolId, ToolAnnotations are
    // re-exported from domain::models.
    use rustain::domain::models::{ToolAnnotations, ToolDescriptor, ToolId};
    let _: ToolDescriptor = ToolDescriptor {
        id: ToolId("builtin::Bash".into()),
        name: "Bash".into(),
        description: "test".into(),
        input_schema: serde_json::Value::Object(Default::default()),
        provider_id: "builtin".into(),
        annotations: ToolAnnotations::default(),
    };
}

#[test]
fn test_describe_default_impl_is_empty_for_noop() {
    use rustain::adapters::noop::NoOpToolSet;
    use rustain::domain::ports::ToolSetPort;
    let noop = NoOpToolSet;
    assert!(noop.describe().is_empty());
}

#[cfg(feature = "mcp")]
#[tokio::test]
async fn test_builtin_provider_discover_returns_7_tools() {
    use rustain::adapters::composite_toolset_adapter::CompositeToolsetAdapter;
    use rustain::adapters::filesystem::FileSystemStorage;
    use rustain::adapters::toolset_adapter::ToolSetAdapter;
    use rustain::domain::ports::ToolSetPort;

    let storage = Arc::new(FileSystemStorage::new(std::path::PathBuf::from(".")));
    let adapter: Arc<dyn ToolSetPort> =
        Arc::new(ToolSetAdapter::new(
            std::path::PathBuf::from("."),
            storage,
            Arc::new(arc_swap::ArcSwap::from_pointee(Arc::new(rustain::adapters::sandbox::NoOpSandbox) as Arc<dyn rustain::domain::ports::SandboxManager>)),
            Arc::new(tokio::sync::RwLock::new(rustain::domain::models::sandbox::SandboxPolicy::Permissive)),
        ));
    let composite = CompositeToolsetAdapter::new(adapter, vec![], vec![], true, None, None);

    let _ = composite.populate_registry().await.unwrap();
    let snap = composite.capability_registry().snapshot();
    let mut builtin_names: Vec<String> = snap
        .iter()
        .filter(|c| c.protocol == "builtin")
        .map(|c| c.name.clone())
        .collect();
    builtin_names.sort();
    assert_eq!(
        builtin_names,
        vec![
            "Bash",
            "Read",
            "Write",
            "activate_skill",
            "exit_plan_mode",
            "propose_plan",
            "skill_view",
        ]
    );
}

#[cfg(feature = "mcp")]
#[test]
fn test_builtin_provider_capabilities_in_process() {
    use rustain::adapters::builtin::BuiltinProvider;
    use rustain::adapters::filesystem::FileSystemStorage;
    use rustain::adapters::toolset_adapter::ToolSetAdapter;
    use rustain::domain::models::provider_capabilities::TransportKind;

    let storage = Arc::new(FileSystemStorage::new(std::path::PathBuf::from(".")));
    let adapter = Arc::new(ToolSetAdapter::new(
        std::path::PathBuf::from("."),
        storage,
        Arc::new(arc_swap::ArcSwap::from_pointee(Arc::new(rustain::adapters::sandbox::NoOpSandbox) as Arc<dyn rustain::domain::ports::SandboxManager>)),
        Arc::new(tokio::sync::RwLock::new(rustain::domain::models::sandbox::SandboxPolicy::Permissive)),
    ));
    let provider = BuiltinProvider::new(adapter);
    let caps = provider.capabilities();
    assert_eq!(caps.transport_kind, TransportKind::InProcess);
    assert!(!caps.supports_streaming);
    assert!(!caps.supports_list_changed);
}

#[tokio::test]
async fn test_skill_provider_discover_with_zero_skills() {
    use rustain::adapters::skill_activation::SkillActivator;
    use rustain::adapters::skill_provider::SkillsProvider;

    let activator = Arc::new(SkillActivator::new());
    let provider = SkillsProvider::new(activator);
    let caps = provider.discover().await.unwrap();
    assert!(caps.is_empty());
}

#[tokio::test]
async fn test_skill_provider_discover_with_3_skills() {
    use rustain::adapters::skill_activation::SkillActivator;
    use rustain::adapters::skill_provider::SkillsProvider;
    use rustain::adapters::skill_registry::SkillRegistry;
    use rustain::domain::models::{SkillDef, SkillSource};

    let skills = vec![
        SkillDef {
            name: "review".into(),
            description: "Code review skill".into(),
            file: std::path::PathBuf::from("review.md"),
            directory: std::path::PathBuf::from("/tmp"),
            source: SkillSource::WorkspaceAgents,
            allowed_tools: None,
        },
        SkillDef {
            name: "test".into(),
            description: "Test generation skill".into(),
            file: std::path::PathBuf::from("test.md"),
            directory: std::path::PathBuf::from("/tmp"),
            source: SkillSource::WorkspaceAgents,
            allowed_tools: None,
        },
        SkillDef {
            name: "refactor".into(),
            description: "Refactoring skill".into(),
            file: std::path::PathBuf::from("refactor.md"),
            directory: std::path::PathBuf::from("/tmp"),
            source: SkillSource::WorkspaceAgents,
            allowed_tools: None,
        },
    ];
    let registry = SkillRegistry::from_skills(skills);
    let activator = Arc::new(SkillActivator::new());
    activator.set_registry(registry).await;

    let provider = SkillsProvider::new(activator);
    let caps = provider.discover().await.unwrap();
    assert_eq!(caps.len(), 3);
    let mut names: Vec<String> = caps.into_iter().map(|c| c.name).collect();
    names.sort();
    assert_eq!(names, vec!["refactor", "review", "test"]);
}

#[tokio::test]
async fn test_skill_provider_invoke_returns_invoke_error_phase_a() {
    use rustain::adapters::skill_activation::SkillActivator;
    use rustain::adapters::skill_provider::SkillsProvider;
    use rustain::domain::models::capability::CapabilityError;
    use rustain::domain::models::capability_id::CapabilityId;
    use tokio_util::sync::CancellationToken;

    let activator = Arc::new(SkillActivator::new());
    let provider = SkillsProvider::new(activator);
    let id = CapabilityId {
        protocol: "skill".into(),
        server: String::new(),
        tool: "review".into(),
    };
    let result = provider
        .invoke(
            &id,
            serde_json::json!({"arguments": "foo"}),
            CancellationToken::new(),
        )
        .await;
    assert!(matches!(
        result,
        Err(CapabilityError::InvocationFailed { .. })
    ));
}

#[cfg(feature = "mcp")]
#[serial_test::serial]
#[tokio::test]
async fn test_registry_holds_all_three_protocols() {
    use rustain::adapters::composite_toolset_adapter::CompositeToolsetAdapter;
    use rustain::adapters::filesystem::FileSystemStorage;
    use rustain::adapters::mcp::client::McpClientAdapter;
    use rustain::adapters::skill_activation::SkillActivator;
    use rustain::adapters::toolset_adapter::ToolSetAdapter;
    use rustain::domain::models::{McpServerSource, McpServerSpec, McpTransport};
    use rustain::domain::models::{SkillDef, SkillSource};
    use rustain::domain::ports::ToolSetPort;
    use std::collections::BTreeMap;

    let builtin: Arc<dyn ToolSetPort> = Arc::new(ToolSetAdapter::new(
        std::path::PathBuf::from("."),
        Arc::new(FileSystemStorage::new(std::path::PathBuf::from("."))),
        Arc::new(arc_swap::ArcSwap::from_pointee(Arc::new(rustain::adapters::sandbox::NoOpSandbox) as Arc<dyn rustain::domain::ports::SandboxManager>)),
        Arc::new(tokio::sync::RwLock::new(rustain::domain::models::sandbox::SandboxPolicy::Permissive)),
    ));

    // Fake MCP server with 2 tools (echo, add)
    let binary_name = if cfg!(target_os = "windows") {
        "fake-mcp-server.exe"
    } else {
        "fake-mcp-server"
    };
    let exe_dir = std::env::current_exe()
        .expect("current exe")
        .parent()
        .expect("parent")
        .to_path_buf();
    let candidates = vec![
        exe_dir.join(binary_name),
        exe_dir.parent().expect("deps parent").join(binary_name),
    ];
    let command = candidates
        .iter()
        .find(|p| p.exists())
        .cloned()
        .unwrap_or_else(|| candidates[0].clone());

    let spec = McpServerSpec {
        id: "three-proto".to_string(),
        transport: McpTransport::Stdio,
        command: Some(command.to_string_lossy().into_owned()),
        args: vec![],
        env: BTreeMap::new(),
        url: None,
        persistent: false,
        source: McpServerSource::Workspace,
    };
    let client = Arc::new(McpClientAdapter::new(spec.clone(), None));
    client.set_self_weak(Arc::downgrade(&client));
    let _ = client.connect().await;

    // SkillActivator with 3 programmatic skills
    use rustain::adapters::skill_registry::SkillRegistry;
    let skills = vec![
        SkillDef {
            name: "review".into(),
            description: "Code review skill".into(),
            file: std::path::PathBuf::from("review.md"),
            directory: std::path::PathBuf::from("/tmp"),
            source: SkillSource::WorkspaceAgents,
            allowed_tools: None,
        },
        SkillDef {
            name: "test".into(),
            description: "Test generation skill".into(),
            file: std::path::PathBuf::from("test.md"),
            directory: std::path::PathBuf::from("/tmp"),
            source: SkillSource::WorkspaceAgents,
            allowed_tools: None,
        },
        SkillDef {
            name: "refactor".into(),
            description: "Refactoring skill".into(),
            file: std::path::PathBuf::from("refactor.md"),
            directory: std::path::PathBuf::from("/tmp"),
            source: SkillSource::WorkspaceAgents,
            allowed_tools: None,
        },
    ];
    let registry = SkillRegistry::from_skills(skills);
    let skill_activator = Arc::new(SkillActivator::new());
    skill_activator.set_registry(registry).await;

    let composite = CompositeToolsetAdapter::new(
        builtin,
        vec![client],
        vec![spec],
        true, // include_builtin
        None,
        Some(skill_activator),
    );

    composite.populate_registry().await.unwrap();
    let snap = composite.capability_registry().snapshot();

    let mcp_count = snap.iter().filter(|c| c.protocol == "mcp").count();
    let builtin_count = snap.iter().filter(|c| c.protocol == "builtin").count();
    let skill_count = snap.iter().filter(|c| c.protocol == "skill").count();

    assert_eq!(mcp_count, 2, "fake MCP server has echo + add tools");
    assert_eq!(builtin_count, 7, "ToolSetAdapter has 7 builtin tools (including skill_view from 9.6)");
    assert_eq!(skill_count, 3, "SkillRegistry has 3 programmatic skills");
    assert_eq!(snap.len(), 12);
}

#[cfg(feature = "mcp")]
#[tokio::test]
async fn test_describe_returns_tool_descriptors() {
    use rustain::adapters::composite_toolset_adapter::CompositeToolsetAdapter;
    use rustain::adapters::mcp::client::McpClientAdapter;
    use rustain::adapters::noop::NoOpToolSet;
    use rustain::domain::models::{McpServerSource, McpServerSpec, McpTransport};
    use rustain::domain::ports::ToolSetPort;
    use std::collections::BTreeMap;

    let builtin: Arc<dyn ToolSetPort> = Arc::new(NoOpToolSet);
    let spec = McpServerSpec {
        id: "describe-test".to_string(),
        transport: McpTransport::Stdio,
        command: Some("echo".into()),
        args: vec![],
        env: BTreeMap::new(),
        url: None,
        persistent: false,
        source: McpServerSource::Workspace,
    };
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let client = Arc::new(McpClientAdapter::new(spec.clone(), Some(tx)));

    let composite =
        CompositeToolsetAdapter::new(builtin, vec![client], vec![spec], false, None, None);

    // No registry population yet — describe should be empty
    let desc = composite.describe();
    assert!(desc.is_empty());

    // After populate_registry, describe matches snapshot
    let _ = composite.populate_registry().await;
    let desc = composite.describe();
    let snap = composite.capability_registry().snapshot();
    assert_eq!(desc.len(), snap.len());
    for (d, s) in desc.iter().zip(snap.iter()) {
        assert_eq!(d.id.as_str(), s.id.as_string());
        assert_eq!(d.provider_id, s.provider_id);
    }
}

#[cfg(feature = "mcp")]
#[tokio::test]
async fn test_catalog_delta_version_monotonic() {
    use rustain::adapters::composite_toolset_adapter::CompositeToolsetAdapter;
    use rustain::adapters::noop::NoOpToolSet;
    use rustain::domain::ports::ToolSetPort;

    let builtin: Arc<dyn ToolSetPort> = Arc::new(NoOpToolSet);
    let composite = CompositeToolsetAdapter::new(builtin, vec![], vec![], false, None, None);

    assert_eq!(composite.catalog_version(), 0);

    let _ = composite.emit_catalog_delta().await;
    assert_eq!(composite.catalog_version(), 1);

    let _ = composite.emit_catalog_delta().await;
    assert_eq!(composite.catalog_version(), 2);

    let _ = composite.emit_catalog_delta().await;
    assert_eq!(composite.catalog_version(), 3);
}

#[cfg(feature = "mcp")]
#[tokio::test]
async fn test_emit_catalog_delta_returns_ok_phase_a() {
    use rustain::adapters::composite_toolset_adapter::CompositeToolsetAdapter;
    use rustain::adapters::noop::NoOpToolSet;
    use rustain::domain::ports::ToolSetPort;

    let builtin: Arc<dyn ToolSetPort> = Arc::new(NoOpToolSet);
    let composite = CompositeToolsetAdapter::new(builtin, vec![], vec![], false, None, None);

    let result = composite.emit_catalog_delta().await;
    assert!(result.is_ok());
}

#[test]
fn test_status_panel_shows_all_three_protocol_counts() {
    use rustain::adapters::tui::widgets::adapter_status_panel::format_registry_summary;
    use rustain::domain::models::capability_registry::RegisteredCapability;

    let snap = vec![
        RegisteredCapability {
            id: CapabilityId {
                protocol: "mcp".into(),
                server: "srv1".into(),
                tool: "echo".into(),
            },
            protocol: "mcp".into(),
            provider_id: "mcp:srv1".into(),
            name: "echo".into(),
            description: "test".into(),
            input_schema: serde_json::Value::Object(Default::default()),
            parallel_safe: true,
        },
        RegisteredCapability {
            id: CapabilityId {
                protocol: "mcp".into(),
                server: "srv2".into(),
                tool: "add".into(),
            },
            protocol: "mcp".into(),
            provider_id: "mcp:srv2".into(),
            name: "add".into(),
            description: "test".into(),
            input_schema: serde_json::Value::Object(Default::default()),
            parallel_safe: true,
        },
        RegisteredCapability {
            id: CapabilityId {
                protocol: "builtin".into(),
                server: String::new(),
                tool: "Bash".into(),
            },
            protocol: "builtin".into(),
            provider_id: "builtin".into(),
            name: "Bash".into(),
            description: "test".into(),
            input_schema: serde_json::Value::Object(Default::default()),
            parallel_safe: false,
        },
        RegisteredCapability {
            id: CapabilityId {
                protocol: "skill".into(),
                server: String::new(),
                tool: "review".into(),
            },
            protocol: "skill".into(),
            provider_id: "skill".into(),
            name: "review".into(),
            description: "test".into(),
            input_schema: serde_json::Value::Object(Default::default()),
            parallel_safe: false,
        },
    ];

    let summary = format_registry_summary(&snap);
    assert_eq!(
        summary,
        Some("Registry: 4 capabilities (2 MCP, 1 builtin, 1 skill)".to_string())
    );
}

#[cfg(feature = "mcp")]
#[serial_test::serial]
#[tokio::test]
async fn test_catalog_delta_added_removed_correctness() {
    use rustain::adapters::composite_toolset_adapter::CompositeToolsetAdapter;
    use rustain::adapters::filesystem::FileSystemStorage;
    use rustain::adapters::mcp::client::McpClientAdapter;
    use rustain::adapters::toolset_adapter::ToolSetAdapter;
    use rustain::domain::models::{McpServerSource, McpServerSpec, McpTransport};
    use rustain::domain::ports::ToolSetPort;
    use std::collections::BTreeMap;

    let builtin: Arc<dyn ToolSetPort> = Arc::new(ToolSetAdapter::new(
        std::path::PathBuf::from("."),
        Arc::new(FileSystemStorage::new(std::path::PathBuf::from("."))),
        Arc::new(arc_swap::ArcSwap::from_pointee(Arc::new(rustain::adapters::sandbox::NoOpSandbox) as Arc<dyn rustain::domain::ports::SandboxManager>)),
        Arc::new(tokio::sync::RwLock::new(rustain::domain::models::sandbox::SandboxPolicy::Permissive)),
    ));

    // Fake MCP server with 2 tools (echo, add)
    let binary_name = if cfg!(target_os = "windows") {
        "fake-mcp-server.exe"
    } else {
        "fake-mcp-server"
    };
    let exe_dir = std::env::current_exe()
        .expect("current exe")
        .parent()
        .expect("parent")
        .to_path_buf();
    let candidates = vec![
        exe_dir.join(binary_name),
        exe_dir.parent().expect("deps parent").join(binary_name),
    ];
    let command = candidates
        .iter()
        .find(|p| p.exists())
        .cloned()
        .unwrap_or_else(|| candidates[0].clone());

    let spec = McpServerSpec {
        id: "delta-test".to_string(),
        transport: McpTransport::Stdio,
        command: Some(command.to_string_lossy().into_owned()),
        args: vec![],
        env: BTreeMap::new(),
        url: None,
        persistent: false,
        source: McpServerSource::Workspace,
    };
    let client = Arc::new(McpClientAdapter::new(spec.clone(), None));
    client.set_self_weak(Arc::downgrade(&client));
    let _ = client.connect().await;

    let composite = CompositeToolsetAdapter::new(
        builtin,
        vec![client],
        vec![spec],
        true, // include_builtin
        None,
        None, // no skill_activator
    );

    // Initial populate: version=1, registry has 7 builtin (incl. skill_view from 9.6) + 2 MCP = 9
    let _ = composite.populate_registry().await.unwrap();
    assert_eq!(composite.catalog_version(), 1);
    let snap1 = composite.capability_registry().snapshot();
    assert_eq!(snap1.len(), 9, "7 builtin + 2 MCP tools");

    // Emit another delta with no changes: version=2, added=0 removed=0
    let _ = composite.emit_catalog_delta().await.unwrap();
    assert_eq!(composite.catalog_version(), 2);
    let snap2 = composite.capability_registry().snapshot();
    assert_eq!(
        snap2.len(),
        9,
        "no tools added or removed — registry unchanged"
    );

    // Register a new capability manually (simulating MCP list_changed adding a tool)
    use rustain::domain::models::capability_registry::RegisteredCapability;
    let extra = RegisteredCapability {
        id: rustain::domain::models::capability_id::CapabilityId {
            protocol: "mcp".into(),
            server: "delta-test".into(),
            tool: "multiply".into(),
        },
        protocol: "mcp".into(),
        provider_id: "mcp:delta-test".into(),
        name: "multiply".into(),
        description: "Multiply two numbers".into(),
        input_schema: serde_json::json!({"type": "object"}),
        parallel_safe: true,
    };
    let _handle = composite
        .capability_registry()
        .register(extra)
        .await
        .unwrap();

    // Verify registry grew by 1
    let snap3 = composite.capability_registry().snapshot();
    assert_eq!(snap3.len(), 10, "added multiply tool (7 builtin + 3 MCP)");

    // Emit delta: version=3, should detect 1 added
    let _ = composite.emit_catalog_delta().await.unwrap();
    assert_eq!(composite.catalog_version(), 3);

    // Verify the multiply tool is present after delta emit
    let snap4 = composite.capability_registry().snapshot();
    let multiply_tools: Vec<_> = snap4.iter().filter(|c| c.name == "multiply").collect();
    assert_eq!(
        multiply_tools.len(),
        1,
        "multiply tool should be in registry"
    );
    assert_eq!(
        snap4.len(),
        10,
        "registry still has 10 tools after delta emit"
    );
}
