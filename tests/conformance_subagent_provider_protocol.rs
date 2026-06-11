use std::sync::Arc;

use rustain::adapters::subagent::SubagentProvider;
use rustain::domain::ports::CapabilityProvider;

// Minimal stub runner
struct StubRunner;

#[async_trait::async_trait]
impl rustain::domain::ports::SubagentRunner for StubRunner {
    async fn launch(
        &self,
        _spec: rustain::domain::models::AgentLaunchSpec,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> Result<rustain::domain::models::TaskHandle, rustain::domain::models::SubagentError> {
        unimplemented!()
    }
}

// Minimal stub info port
struct StubInfo;

impl rustain::domain::ports::ProviderInfoPort for StubInfo {
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

#[tokio::test]
async fn test_subagent_provider_protocol_returns_subagent() {
    let runner = Arc::new(StubRunner) as Arc<dyn rustain::domain::ports::SubagentRunner>;
    let registry = Arc::new(rustain::infrastructure::subagent::SubagentRegistry::new());
    let agent_registry = Arc::new(tokio::sync::RwLock::new(
        rustain::adapters::agent_registry::AgentRegistry::new(),
    ));
    let model_router = Arc::new(StubInfo) as Arc<dyn rustain::domain::ports::ProviderInfoPort>;
    let tmp = tempfile::tempdir().unwrap();
    let spool = Arc::new(
        rustain::infrastructure::subagent::SubagentSpool::new(tmp.path().join("spool"))
            .await
            .unwrap(),
    );

    let provider = SubagentProvider::new(runner, registry, agent_registry, model_router, spool);
    assert_eq!(provider.protocol(), "subagent");

    let caps = provider.capabilities();
    assert_eq!(
        caps.transport_kind,
        rustain::domain::models::TransportKind::InProcess
    );
    assert!(!caps.supports_streaming);
    assert!(caps.supports_list_changed);
    assert_eq!(caps.max_tool_count, Some(10));
}
