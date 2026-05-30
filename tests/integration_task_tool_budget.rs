use std::sync::Arc;

use arc_swap::ArcSwap;
use rustain::adapters::noop::NoOpProvider;
use rustain::adapters::sandbox::NoOpSandbox;
use rustain::adapters::security_adapter::SecurityAdapter;
use rustain::adapters::toolset_adapter::ToolSetAdapter;
use rustain::domain::ports::CapabilityProvider;
use rustain::domain::services::approval_runtime::ApprovalRuntime;
use rustain::domain::services::tool_scheduler::ToolScheduler;
use rustain::infrastructure::runtime::event_bus::EventBus;
use rustain::infrastructure::subagent::{SubagentRegistry, SubagentSpool};
use std::path::PathBuf;
use tokio_util::sync::CancellationToken;

async fn make_provider() -> (
    Arc<rustain::adapters::subagent::SubagentProvider>,
    Arc<rustain::adapters::subagent::InProcessSubagentRunner>,
    tempfile::TempDir,
) {
    let tmp = tempfile::tempdir().unwrap();
    let provider = Arc::new(NoOpProvider) as Arc<dyn rustain::domain::ports::StreamingProvider>;
    let storage = Arc::new(rustain::adapters::filesystem::FileSystemStorage::new(
        tmp.path().to_path_buf(),
    )) as Arc<dyn rustain::domain::ports::StoragePort>;
    let security = Arc::new(SecurityAdapter::new(PathBuf::from(".")))
        as Arc<dyn rustain::domain::ports::SecurityPort>;
    let sandbox = Arc::new(ArcSwap::from_pointee(
        Arc::new(NoOpSandbox) as Arc<dyn rustain::domain::ports::SandboxManager>
    ));
    let tools = Arc::new(ToolSetAdapter::new(
        PathBuf::from("."),
        storage.clone(),
        sandbox,
        Arc::new(tokio::sync::RwLock::new(
            rustain::domain::models::SandboxPolicy::Permissive,
        )),
    )) as Arc<dyn rustain::domain::ports::ToolSetPort>;
    let approval = ApprovalRuntime::new(
        1024,
        Arc::new(rustain::adapters::noop::NoOpApprovalPersistence),
    );
    let scheduler = ToolScheduler::new(security.clone(), tools.clone(), approval.clone(), 1024);
    let (event_bus, _event_rx) = EventBus::new(1024);
    let event_bus = Arc::new(event_bus);
    let registry = Arc::new(SubagentRegistry::new());
    let parent_sandbox = Arc::new(tokio::sync::RwLock::new(
        rustain::domain::models::SandboxPolicy::Permissive,
    ));
    let spool = Arc::new(SubagentSpool::new(tmp.path().join("spool")).await.unwrap());

    let runner = Arc::new(rustain::adapters::subagent::InProcessSubagentRunner::new(
        provider.clone(),
        storage.clone(),
        security.clone(),
        tools.clone(),
        approval.clone(),
        scheduler.clone(),
        event_bus.clone(),
        registry.clone(),
        parent_sandbox,
        spool.clone(),
    ));

    let agent_registry = Arc::new(tokio::sync::RwLock::new(
        rustain::adapters::agent_registry::AgentRegistry::new(),
    ));

    // Router that reports a small context window so budget guard fires easily
    struct SmallWindowRouter;
    impl rustain::domain::ports::ProviderInfoPort for SmallWindowRouter {
        fn active_delegate_id(&self) -> String {
            "noop".into()
        }
        fn get_model(
            &self,
            _provider_id: &str,
            _model_id: &str,
        ) -> Option<rustain::domain::models::provider::ModelDescriptor> {
            Some(rustain::domain::models::provider::ModelDescriptor {
                model_id: "test".into(),
                display_name: "Test".into(),
                provider_id: "noop".into(),
                context_window: 10_000,
                capabilities: std::collections::HashSet::new(),
                pricing_tier: None,
                stale: false,
            })
        }
        fn get_model_provider(&self, _model_id: &str, _prefer: Option<&str>) -> Option<String> {
            None
        }
        fn list_providers(&self) -> Vec<rustain::domain::models::provider::ProviderDescriptor> {
            vec![]
        }
        fn list_models_by_provider(
            &self,
            _provider_id: &str,
        ) -> Vec<rustain::domain::models::provider::ModelDescriptor> {
            vec![]
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
            chrono::Utc::now().timestamp()
        }
        fn today_start_unix_ms(&self) -> i64 {
            chrono::Utc::now().timestamp_millis()
        }
    }

    let model_router: Arc<dyn rustain::domain::ports::ProviderInfoPort> =
        Arc::new(SmallWindowRouter);

    let subagent_provider = Arc::new(rustain::adapters::subagent::SubagentProvider::new(
        runner.clone(),
        registry.clone(),
        agent_registry,
        model_router,
        spool.clone(),
    ));

    (subagent_provider, runner, tmp)
}

#[tokio::test]
async fn budget_guard_rejects_high_parent_tokens() {
    let (provider, _runner, _tmp) = make_provider().await;

    // D1 fix: inject parent context via input JSON instead of set_task_context
    let input = serde_json::json!({
        "description": "test",
        "prompt": "hello",
        "__parent_ctx_tokens": 8_000,
        "__conversation_id": "test-conv"
    });
    let cancel = CancellationToken::new();
    let result = provider
        .invoke(
            &rustain::domain::models::capability_id::CapabilityId {
                protocol: "subagent".into(),
                server: String::new(),
                tool: "task".into(),
            },
            input,
            cancel,
        )
        .await;

    assert!(result.is_ok());
    let tool_result = result.unwrap();
    assert!(tool_result.is_error, "Expected budget rejection error");
    assert!(
        tool_result.content.contains("exceeds 50%"),
        "Expected budget message, got: {}",
        tool_result.content
    );
}

#[tokio::test]
async fn budget_guard_allows_low_parent_tokens() {
    let (provider, _runner, _tmp) = make_provider().await;

    // D1 fix: inject parent context via input JSON instead of set_task_context
    let input = serde_json::json!({
        "description": "test",
        "prompt": "hello",
        "__parent_ctx_tokens": 1_000,
        "__conversation_id": "test-conv"
    });
    let cancel = CancellationToken::new();

    let cap_id = rustain::domain::models::capability_id::CapabilityId {
        protocol: "subagent".into(),
        server: String::new(),
        tool: "task".into(),
    };
    let invoke_fut = provider.invoke(&cap_id, input, cancel.child_token());

    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        cancel.cancel();
    });

    let result = tokio::time::timeout(std::time::Duration::from_secs(2), invoke_fut).await;
    assert!(result.is_ok(), "invoke should complete within timeout");
    let tool_result = result.unwrap().unwrap();

    // Budget guard should NOT reject it (the error should be from NoOpProvider failure or cancellation, not budget)
    assert!(
        !tool_result.content.contains("exceeds 50%"),
        "Budget should not reject low token count: {}",
        tool_result.content
    );
}
