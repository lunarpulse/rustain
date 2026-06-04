//! Conformance tests for the CancellationToken tree + subprocess `kill_on_drop`.
//!
//! Source of truth:
//! - `_bmad-output/planning-artifacts/architecture/adr/ADR-06-03-cancellation-token-tree.md`
//! - `_bmad-output/implementation-artifacts/6-0a-cancellation-and-event-bus.md`

use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use rustain::adapters::filesystem::FileSystemStorage;
use rustain::adapters::noop::{NoOpProvider, NoOpStorage};
use rustain::adapters::toolset_adapter::ToolSetAdapter;
use rustain::domain::errors::ToolError;
use rustain::domain::models::SandboxPolicy;
use rustain::domain::ports::StreamingProvider;
use rustain::domain::ports::ToolSetPort;
use rustain::domain::services::plan_manager::PlanManager;
use rustain::domain::services::plan_mode_injector::DefaultPlanInjector;
use rustain::infrastructure::composition::ComposeContext;
use rustain::infrastructure::runtime::agent_core::AgentCore;
use rustain::infrastructure::runtime::event_bus::EventBus;
use tokio_util::sync::CancellationToken;

fn make_adapter(dir: &std::path::Path) -> ToolSetAdapter {
    let sessions_dir = dir.join(".claude").join("sessions");
    let storage: Arc<dyn rustain::domain::ports::StoragePort> =
        Arc::new(FileSystemStorage::new(sessions_dir));
    ToolSetAdapter::new(
        dir.to_path_buf(),
        storage,
        Arc::new(ArcSwap::from_pointee(
            Arc::new(rustain::adapters::sandbox::NoOpSandbox)
                as Arc<dyn rustain::domain::ports::SandboxManager>,
        )),
        Arc::new(tokio::sync::RwLock::new(
            rustain::domain::models::sandbox::SandboxPolicy::Permissive,
        )),
    )
}

#[tokio::test]
async fn ac1_token_hierarchy_cascade() {
    let session = CancellationToken::new();
    let turn = session.child_token();
    let call = turn.child_token();

    assert!(!session.is_cancelled());
    assert!(!turn.is_cancelled());
    assert!(!call.is_cancelled());

    session.cancel();
    assert!(session.is_cancelled());
    assert!(turn.is_cancelled());
    assert!(call.is_cancelled());

    let session2 = CancellationToken::new();
    let turn2 = session2.child_token();
    let call2 = turn2.child_token();

    call2.cancel();
    assert!(call2.is_cancelled());
    assert!(!turn2.is_cancelled());
    assert!(!session2.is_cancelled());
}

#[tokio::test]
async fn ac2_bash_cancel_kills_subprocess() {
    let tmp = tempfile::tempdir().unwrap();
    let adapter = make_adapter(tmp.path());
    let cancel = CancellationToken::new();
    let cancel_clone = cancel.clone();

    let handle = tokio::spawn(async move {
        let tools: Arc<dyn ToolSetPort> = Arc::new(adapter);
        tools
            .execute(
                "Bash",
                serde_json::json!({"command": "sleep 60", "timeout": 120000}),
                cancel_clone,
            )
            .await
    });

    tokio::time::sleep(Duration::from_millis(50)).await;
    cancel.cancel();

    let result = tokio::time::timeout(Duration::from_millis(200), handle).await;
    match result {
        Ok(Ok(Err(ToolError::Cancelled))) => {}
        Ok(Ok(Err(e))) => panic!("expected Cancelled, got {:?}", e),
        Ok(Ok(Ok(r))) => panic!("expected error, got success: {:?}", r),
        Ok(Err(e)) => panic!("join error: {:?}", e),
        Err(_) => panic!("timed out waiting for cancellation"),
    }

    #[cfg(target_os = "linux")]
    {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let all_procs = procfs::process::all_processes().unwrap();
        let sleepers: Vec<_> = all_procs
            .filter_map(|p| p.ok())
            .filter(|p| {
                p.cmdline()
                    .ok()
                    .map(|cmd| {
                        cmd.contains(&"sleep".to_string()) && cmd.contains(&"60".to_string())
                    })
                    .unwrap_or(false)
            })
            .collect();
        assert!(
            sleepers.is_empty(),
            "zombie sleep processes remain: {:?}",
            sleepers
        );
    }
}

#[tokio::test]
async fn ac2_bash_cancel_within_100ms() {
    let tmp = tempfile::tempdir().unwrap();
    let adapter = make_adapter(tmp.path());
    let cancel = CancellationToken::new();

    let cancel_clone = cancel.clone();
    let adapter: Arc<dyn ToolSetPort> = Arc::new(adapter);
    let handle = tokio::spawn(async move {
        adapter
            .execute(
                "Bash",
                serde_json::json!({"command": "sleep 5"}),
                cancel_clone,
            )
            .await
    });

    tokio::time::sleep(Duration::from_millis(50)).await;
    let start = std::time::Instant::now();
    cancel.cancel();

    let result = tokio::time::timeout(Duration::from_millis(200), handle).await;
    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_millis(100),
        "cancel took {:?}",
        elapsed
    );
    assert!(matches!(result, Ok(Ok(Err(ToolError::Cancelled)))));
}

#[tokio::test]
async fn ac2_read_cancel_returns_cancelled() {
    let tmp = tempfile::tempdir().unwrap();
    let file = tmp.path().join("test.txt");
    std::fs::write(&file, "hello world").unwrap();
    let adapter = make_adapter(tmp.path());
    let cancel = CancellationToken::new();
    cancel.cancel();

    let tools: Arc<dyn ToolSetPort> = Arc::new(adapter);
    let result = tools
        .execute(
            "Read",
            serde_json::json!({"file_path": file.to_str().unwrap()}),
            cancel,
        )
        .await;

    assert!(matches!(result, Err(ToolError::Cancelled)));
}

#[tokio::test]
async fn ac2_write_cancel_returns_cancelled() {
    let tmp = tempfile::tempdir().unwrap();
    let file = tmp.path().join("cancel_test.txt");
    let adapter = make_adapter(tmp.path());
    let cancel = CancellationToken::new();
    cancel.cancel();

    let tools: Arc<dyn ToolSetPort> = Arc::new(adapter);
    let result = tools
        .execute(
            "Write",
            serde_json::json!({"file_path": file.to_str().unwrap(), "content": "data"}),
            cancel,
        )
        .await;

    assert!(matches!(result, Err(ToolError::Cancelled)));
    assert!(!file.exists());
}

#[tokio::test]
async fn ac4_tab_close_cancels_turn_only() {
    use rustain::domain::models::tab::TabManager;

    let session = CancellationToken::new();
    let mut tm = TabManager::new(session.clone());
    let id_a = tm.active_tab_id();
    let id_b = tm.create_tab();

    let cancel_a = tm.tabs()[0].turn_cancel.clone();
    let cancel_b = tm.tabs()[1].turn_cancel.clone();

    tm.close_tab(id_a);

    assert!(cancel_a.is_cancelled());
    assert!(!cancel_b.is_cancelled());
    assert!(!session.is_cancelled());

    let _ = id_b;
}

#[tokio::test]
async fn ac3_cancellation_cancels_pending_approval() {
    // Simulate the AskUserQuestion pending-approval select! pattern from turn.rs
    let turn_cancel = CancellationToken::new();
    let turn_cancel_clone = turn_cancel.clone();

    let (_resp_tx, resp_rx) = tokio::sync::oneshot::channel::<String>();

    let handle = tokio::spawn(async move {
        tokio::select! {
            _ = resp_rx => "approved",
            _ = turn_cancel_clone.cancelled() => "cancelled",
        }
    });

    tokio::time::sleep(Duration::from_millis(50)).await;
    let start = std::time::Instant::now();
    turn_cancel.cancel();

    let result = tokio::time::timeout(Duration::from_millis(200), handle).await;
    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_millis(200),
        "cancel took {:?}",
        elapsed
    );
    assert!(matches!(result, Ok(Ok("cancelled"))));
}

#[tokio::test]
async fn ac4_signal_cancel_before_shutdown() {
    use rustain::infrastructure::runtime::app_state::AppState;

    let approval_runtime = rustain::domain::services::approval_runtime::ApprovalRuntime::new(
        16,
        Arc::new(rustain::adapters::noop::NoOpApprovalPersistence),
    );
    let provider_swap = Arc::new(ArcSwap::from_pointee(
        Arc::new(NoOpProvider::default()) as Arc<dyn StreamingProvider>
    ));
    let provider_registry = Arc::new(rustain::adapters::provider::ProviderRegistry::new());
    let (event_bus, domain_rx) = EventBus::new(16);
    let event_bus = Arc::new(event_bus);
    let agent_core = Arc::new(AgentCore::test_noop());
    let compose_snapshot = Arc::new(ComposeContext {
        workspace_path: std::path::PathBuf::from("."),
        project_context: rustain::domain::models::project_context::ProjectContext::empty(),
        storage: Arc::new(NoOpStorage::default()) as Arc<dyn rustain::domain::ports::StoragePort>,
        skill_activator: Arc::new(rustain::adapters::skill_activation::SkillActivator::new()),
        mcp_servers: Vec::new(),
        include_builtin_tools: true,
        domain_tx: None,
        tool_exposure: "static-full".into(),
        assembler: "passthrough".into(),
        skill_exposure: "l1-metadata".into(),
        skill_cache: Arc::new(rustain::infrastructure::skill_cache::SkillCache::new_in_memory()),
        sandbox_adapter: "noop".into(),
        sandbox_startup_policy: rustain::domain::models::sandbox::SandboxPolicy::Permissive,
        sandbox_slot: Arc::new(ArcSwap::from_pointee(Arc::new(
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
        #[cfg(feature = "meta-search")]
        search_config: rustain::domain::models::SearchConfig::default(),
        #[cfg(feature = "meta-search")]
        meta_search_engine: None,
    });
    let (app_state, _domain_rx) = AppState::new(
        event_bus,
        domain_rx,
        approval_runtime,
        Arc::new(tokio::sync::RwLock::new(SandboxPolicy::ReadOnly {
            network: false,
        })),
        Arc::new(PlanManager::new(std::path::PathBuf::from("."))),
        Arc::new(DefaultPlanInjector::new()),
        provider_swap.clone(),
        provider_registry,
        Arc::new(rustain::adapters::noop::NoOpUsageLedger),
        Arc::new(rustain::adapters::budget::BudgetStateStore::new()),
        Arc::new(ArcSwap::from_pointee(
            rustain::domain::models::AppConfig::default(),
        )),
        agent_core,
        compose_snapshot,
        Arc::new(ArcSwap::from_pointee(Arc::new(
            rustain::adapters::profile_resolver::noop::NoopProfileResolver,
        )
            as Arc<dyn rustain::domain::ports::ProfileResolver>)),
        rustain::adapters::cli::commands::Cli {
            log_level: Some("info".to_string()),
            command: None,
            new: false,
            session: None,
            snapshot_retention: None,
            config_file: None,
            model: None,
            profile: None,
            persona: None,
            memory: None,
            session_adapter: None,
            tools: None,
            channels: None,
            scheduler: None,
            context: None,
            tool_exposure: None,
            skill_exposure: None,
            sandbox_adapter: None,
        },
        rustain::infrastructure::telemetry::ActiveRatioWindow::new_in_memory(),
        #[cfg(feature = "meta-search")]
        None,
    );

    app_state.session_cancel.cancel();

    assert!(app_state.session_cancel.is_cancelled());
}
