use async_trait::async_trait;
use rustain::domain::models::{
    Capability, CapabilityError, CapabilityId, ProviderCapabilities, ToolResult, TransportKind,
    TrustTier,
};
use rustain::domain::ports::CapabilityProvider;
use tokio_util::sync::CancellationToken;

struct UnverifiedA2aProvider;

#[async_trait]
impl CapabilityProvider for UnverifiedA2aProvider {
    fn protocol(&self) -> &str {
        "a2a"
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            supports_streaming: false,
            supports_list_changed: false,
            supports_native_retrieval: None,
            max_tool_count: None,
            transport_kind: TransportKind::Http,
        }
    }

    async fn discover(&self) -> Result<Vec<Capability>, CapabilityError> {
        Ok(vec![Capability {
            id: CapabilityId {
                protocol: "a2a".to_owned(),
                server: "remote-peer".to_owned(),
                tool: "scan".to_owned(),
            },
            name: "Scan".to_owned(),
            description: "Remote scan".to_owned(),
            input_schema: serde_json::json!({"type": "object"}),
            parallel_safe: false,
            trust: TrustTier::Unverified,
        }])
    }

    async fn invoke(
        &self,
        _capability_id: &CapabilityId,
        _input: serde_json::Value,
        _cancel: CancellationToken,
    ) -> Result<ToolResult, CapabilityError> {
        Err(CapabilityError::Invoke("not used".to_owned()))
    }
}

#[tokio::test]
async fn registry_preserves_the_providers_typed_trust_tier() {
    let registry = std::sync::Arc::new(
        rustain::domain::models::capability_registry::CapabilityRegistry::new(None),
    );
    let handles = registry
        .discover_and_register_all(&UnverifiedA2aProvider, "a2a:remote-peer")
        .await
        .expect("register discovered capability");

    let snapshot = registry.snapshot();
    assert_eq!(snapshot.len(), 1);
    assert_eq!(snapshot[0].trust, TrustTier::Unverified);
    drop(handles);
}
