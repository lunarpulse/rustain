//! Capability-provider projection for cached AgentCards.

use std::sync::Arc;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use crate::domain::models::{
    A2aPeerSpec, Capability, CapabilityError, CapabilityId, ProviderCapabilities, ToolResult,
    TransportKind,
};
use crate::domain::ports::CapabilityProvider;

use super::client::A2aClientAdapter;

pub struct A2aProvider {
    peers: Vec<(A2aPeerSpec, Arc<A2aClientAdapter>)>,
    delegation: std::sync::OnceLock<Arc<super::driver::A2aDelegationRuntime>>,
}

impl A2aProvider {
    pub fn new(peers: Vec<(A2aPeerSpec, Arc<A2aClientAdapter>)>) -> Self {
        Self {
            peers,
            delegation: std::sync::OnceLock::new(),
        }
    }

    /// Inject the delegation runtime (node tree + journal + event sink) after
    /// the composition root has opened them. Until this is set, `invoke()`
    /// returns the Story 17.4b refusal — discovery/inventory still work.
    pub fn set_delegation_runtime(&self, runtime: Arc<super::driver::A2aDelegationRuntime>) {
        let _ = self.delegation.set(runtime);
    }
}

#[async_trait]
impl CapabilityProvider for A2aProvider {
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
        let mut capabilities = Vec::new();
        for (peer, client) in &self.peers {
            let Some((card, trust)) = client.cached_card().await else {
                continue;
            };
            capabilities.reserve(card.skills.len());
            for skill in card.skills {
                if skill.id.contains("::") {
                    tracing::warn!(
                        peer = %peer.id,
                        skill_id = %skill.id,
                        "skipping A2A skill whose id contains the reserved `::` capability-id separator"
                    );
                    continue;
                }
                capabilities.push(Capability {
                    id: CapabilityId {
                        protocol: "a2a".to_owned(),
                        server: peer.id.clone(),
                        tool: skill.id,
                    },
                    name: skill.name,
                    description: skill.description.unwrap_or_default(),
                    input_schema: serde_json::json!({
                        "type": "object",
                        "properties": {
                            "message": {
                                "type": "string",
                                "description": "Task message for the remote A2A skill"
                            }
                        },
                        "required": ["message"],
                        "additionalProperties": false
                    }),
                    parallel_safe: false,
                    trust,
                });
            }
        }
        Ok(capabilities)
    }

    async fn invoke(
        &self,
        capability_id: &CapabilityId,
        input: serde_json::Value,
        cancel: CancellationToken,
    ) -> Result<ToolResult, CapabilityError> {
        // Until the composition root injects the delegation runtime, delegation
        // is intentionally unavailable (Story 17.4b) — discovery still works.
        let Some(runtime) = self.delegation.get() else {
            return Err(CapabilityError::InvocationFailed(
                capability_id.to_string(),
                "A2A task delegation is intentionally unavailable until Story 17.4b".to_owned(),
            ));
        };

        let (spec, client) = self
            .peers
            .iter()
            .find(|(spec, _)| spec.id == capability_id.server)
            .ok_or_else(|| {
                CapabilityError::InvocationFailed(
                    capability_id.to_string(),
                    format!("unknown A2A peer {:?}", capability_id.server),
                )
            })?;

        let (card, trust) = client.cached_card().await.ok_or_else(|| {
            CapabilityError::InvocationFailed(
                capability_id.to_string(),
                "A2A peer AgentCard is not cached; refresh discovery first".to_owned(),
            )
        })?;
        let endpoint = super::endpoint::resolve_jsonrpc_endpoint(&card).map_err(|error| {
            CapabilityError::InvocationFailed(capability_id.to_string(), error.to_string())
        })?;

        let transport = Arc::new(super::driver::TaskClient::new(
            client.clone(),
            endpoint.url().to_owned(),
        ));
        let message = super::driver::build_message(&input);
        match runtime
            .delegate(spec, trust, &capability_id.tool, transport, message, cancel)
            .await
        {
            Ok(result) => Ok(ToolResult {
                tool_use_id: String::new(),
                content: serde_json::to_string_pretty(&result)
                    .unwrap_or_else(|_| result.to_string()),
                is_error: false,
            }),
            Err(error) => Err(CapabilityError::InvocationFailed(
                capability_id.to_string(),
                error.to_string(),
            )),
        }
    }
}
