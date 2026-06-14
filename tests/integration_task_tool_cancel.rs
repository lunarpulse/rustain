use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use async_trait::async_trait;
use futures::StreamExt;
use rustain::adapters::sandbox::NoOpSandbox;
use rustain::adapters::security_adapter::SecurityAdapter;
use rustain::adapters::toolset_adapter::ToolSetAdapter;
use rustain::domain::models::{CompletionOptions, Message, StreamChunk};
use rustain::domain::ports::{CapabilityProvider, StreamingProvider};
use rustain::domain::services::approval_runtime::ApprovalRuntime;
use rustain::domain::services::tool_scheduler::ToolScheduler;
use rustain::infrastructure::runtime::event_bus::EventBus;
use rustain::infrastructure::subagent::{SubagentRegistry, SubagentSpool};
use std::path::PathBuf;
use tokio_util::sync::CancellationToken;

/// Provider that emits one chunk then hangs, so the child is mid-stream.
struct HangingProvider;

#[async_trait]
impl StreamingProvider for HangingProvider {
    async fn stream_completion(
        &self,
        _messages: Vec<Message>,
        _options: CompletionOptions,
    ) -> Result<
        std::pin::Pin<Box<dyn futures::Stream<Item = StreamChunk> + Send>>,
        rustain::domain::errors::ProviderError,
    > {
        let chunks = vec![StreamChunk::Text {
            content: "working...".into(),
            parent_tool_use_id: None,
        }];
        let stream = futures::stream::iter(chunks).chain(futures::stream::pending());
        Ok(Box::pin(stream))
    }

    async fn abort(&self) -> Result<(), rustain::domain::errors::ProviderError> {
        Ok(())
    }

    fn provider_id(&self) -> String {
        "hanging".into()
    }

    fn list_models(&self) -> Vec<rustain::domain::models::provider::ModelDescriptor> {
        vec![]
    }

    async fn health_check(&self) -> Result<(), rustain::domain::errors::ProviderError> {
        Ok(())
    }

    async fn connectivity_probe(
        &self,
    ) -> Result<rustain::domain::ports::ProbeOutcome, rustain::domain::errors::ProviderError> {
        Ok(rustain::domain::ports::ProbeOutcome {
            latency: std::time::Duration::ZERO,
        })
    }
}

async fn make_provider() -> (
    Arc<rustain::adapters::subagent::SubagentProvider>,
    Arc<rustain::adapters::subagent::InProcessSubagentRunner>,
    tempfile::TempDir,
) {
    let tmp = tempfile::tempdir().unwrap();
    let provider = Arc::new(HangingProvider) as Arc<dyn StreamingProvider>;
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

    struct TestRouter;
    impl rustain::domain::ports::ProviderInfoPort for TestRouter {
        fn active_delegate_id(&self) -> String {
            "noop".into()
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

    let model_router: Arc<dyn rustain::domain::ports::ProviderInfoPort> = Arc::new(TestRouter);

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
async fn cancellation_teardown_within_200ms() {
    let (provider, runner, _tmp) = make_provider().await;

    let input = serde_json::json!({"description": "hanging task", "prompt": "hang forever"});
    let parent_cancel = CancellationToken::new();
    let child_cancel = parent_cancel.child_token();

    let cap_id = rustain::domain::models::capability_id::CapabilityId {
        protocol: "subagent".into(),
        server: String::new(),
        tool: "task".into(),
    };

    // Start the task in the background
    let invoke_handle = tokio::spawn({
        let provider = provider.clone();
        let cap_id = cap_id.clone();
        async move {
            CapabilityProvider::invoke(provider.as_ref(), &cap_id, input, child_cancel.clone())
                .await
        }
    });

    // Give it time to start and register
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Verify the child is in the registry
    let entries_before = runner.registry().list().await;
    assert!(!entries_before.is_empty(), "Child should be registered");

    // Cancel the parent token
    let start = tokio::time::Instant::now();
    parent_cancel.cancel();

    // Wait for the invoke to return
    let result = tokio::time::timeout(Duration::from_secs(2), invoke_handle).await;
    let elapsed = start.elapsed();

    assert!(result.is_ok(), "invoke should return after cancel");
    let join_result = result.unwrap();
    assert!(join_result.is_ok(), "join handle should succeed");
    let invoke_result = join_result.unwrap();
    assert!(invoke_result.is_ok(), "invoke should succeed");
    let tool_result = invoke_result.unwrap();

    // Should be killed or have some error status
    assert!(
        tool_result.is_error
            || tool_result.content.contains("Killed")
            || tool_result.content.contains("cancelled"),
        "Expected error or killed after cancel, got: {}",
        tool_result.content
    );

    // The teardown should be fast (≤ 200ms is the goal; use 500ms for CI margin)
    assert!(
        elapsed <= Duration::from_millis(500),
        "Teardown took {:?}, expected ≤ 500ms",
        elapsed
    );

    // Verify spool file exists (preserved)
    let spool_path = _tmp.path().join("spool");
    let spool_files: Vec<_> = std::fs::read_dir(&spool_path)
        .unwrap()
        .filter_map(|e| e.ok())
        .collect();
    assert!(
        !spool_files.is_empty(),
        "Spool file should be preserved after cancel"
    );
}
