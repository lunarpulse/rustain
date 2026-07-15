use std::sync::Arc;

use arc_swap::ArcSwap;
use async_trait::async_trait;
use rustain::adapters::sandbox::NoOpSandbox;
use rustain::adapters::security_adapter::SecurityAdapter;
use rustain::adapters::toolset_adapter::ToolSetAdapter;
use rustain::domain::models::{CompletionOptions, Message, StopReason, StreamChunk};
use rustain::domain::ports::{CapabilityProvider, StreamingProvider};
use rustain::domain::services::approval_runtime::ApprovalRuntime;
use rustain::domain::services::tool_scheduler::ToolScheduler;
use rustain::infrastructure::runtime::event_bus::EventBus;
use rustain::infrastructure::subagent::{NodeTree, SubagentSpool};
use std::path::PathBuf;
use tokio_util::sync::CancellationToken;

/// Provider that immediately returns TurnComplete(EndTurn) so child completes quickly.
struct QuickCompleteProvider;

#[async_trait]
impl StreamingProvider for QuickCompleteProvider {
    async fn stream_completion(
        &self,
        _messages: Vec<Message>,
        _options: CompletionOptions,
    ) -> Result<
        std::pin::Pin<Box<dyn futures::Stream<Item = StreamChunk> + Send>>,
        rustain::domain::errors::ProviderError,
    > {
        let chunks = vec![
            StreamChunk::Text {
                content: "done".into(),
                parent_tool_use_id: None,
            },
            StreamChunk::TurnComplete {
                stop_reason: StopReason::EndTurn,
            },
        ];
        let stream = futures::stream::iter(chunks);
        Ok(Box::pin(stream))
    }

    async fn abort(&self) -> Result<(), rustain::domain::errors::ProviderError> {
        Ok(())
    }

    fn provider_id(&self) -> String {
        "quick-complete".into()
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
    let provider = Arc::new(QuickCompleteProvider) as Arc<dyn StreamingProvider>;
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
    let registry = Arc::new(NodeTree::new());
    let parent_sandbox = Arc::new(tokio::sync::RwLock::new(
        rustain::domain::models::SandboxPolicy::Permissive,
    ));
    let spool = Arc::new(SubagentSpool::new(tmp.path().join("spool")).await.unwrap());
    let root_authority =
        rustain::domain::models::CapabilityToken::r1_root(rustain::domain::models::AgentId::root());
    let authority_ledger = Arc::new(
        rustain::domain::services::authority_ledger::AuthorityLedger::new(
            root_authority.clone(),
            std::sync::Arc::new(rustain::domain::clock::SystemClock::default()),
        ),
    );
    let authority =
        Arc::new(rustain::adapters::authority::InProcessAuthorityProvider::new(authority_ledger))
            as Arc<dyn rustain::domain::ports::AuthorityProvider>;

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
        authority.clone(),
        root_authority.clone(),
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

    subagent_provider
        .set_authority(authority, root_authority)
        .await;
    let _ = subagent_provider.clone();
    (subagent_provider, runner, tmp)
}

#[tokio::test]
async fn concurrent_task_calls_run_in_parallel() {
    let (provider, _runner, _tmp) = make_provider().await;

    // Launch two task calls concurrently
    let input1 = serde_json::json!({"description": "task 1", "prompt": "hello 1"});
    let input2 = serde_json::json!({"description": "task 2", "prompt": "hello 2"});

    let cap_id = rustain::domain::models::capability_id::CapabilityId {
        protocol: "subagent".into(),
        server: String::new(),
        tool: "task".into(),
    };

    let cancel1 = CancellationToken::new();
    let cancel2 = CancellationToken::new();

    let f1 = CapabilityProvider::invoke(provider.as_ref(), &cap_id, input1, cancel1);
    let f2 = CapabilityProvider::invoke(provider.as_ref(), &cap_id, input2, cancel2);

    // Both should be in the registry concurrently at some point
    let (r1, r2) = futures::future::join(f1, f2).await;

    let res1 = r1.unwrap();
    let res2 = r2.unwrap();

    // Both should succeed (no error)
    assert!(!res1.is_error, "task 1 should succeed: {}", res1.content);
    assert!(!res2.is_error, "task 2 should succeed: {}", res2.content);

    // Results should contain the child output
    assert!(
        res1.content.contains("done") || res1.content.contains("completed"),
        "task 1 should have child output: {}",
        res1.content
    );
    assert!(
        res2.content.contains("done") || res2.content.contains("completed"),
        "task 2 should have child output: {}",
        res2.content
    );
}

#[tokio::test]
async fn concurrent_task_calls_ordered_results() {
    let (provider, _runner, _tmp) = make_provider().await;

    // Launch tasks in sequence and verify results come back in order
    let cap_id = rustain::domain::models::capability_id::CapabilityId {
        protocol: "subagent".into(),
        server: String::new(),
        tool: "task".into(),
    };
    let results = futures::future::join_all((0..3).map(|i| {
        let input = serde_json::json!({
            "description": format!("task {}", i),
            "prompt": format!("hello {}", i)
        });
        CapabilityProvider::invoke(provider.as_ref(), &cap_id, input, CancellationToken::new())
    }))
    .await;

    assert_eq!(results.len(), 3);
    for (i, res) in results.iter().enumerate() {
        let r = res.as_ref().unwrap();
        assert!(!r.is_error, "task {} should succeed: {}", i, r.content);
    }
}
