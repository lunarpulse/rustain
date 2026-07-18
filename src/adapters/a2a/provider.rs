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
}

impl A2aProvider {
    pub fn new(peers: Vec<(A2aPeerSpec, Arc<A2aClientAdapter>)>) -> Self {
        Self { peers }
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
        _input: serde_json::Value,
        _cancel: CancellationToken,
    ) -> Result<ToolResult, CapabilityError> {
        Err(CapabilityError::InvocationFailed(
            capability_id.to_string(),
            "A2A task delegation is intentionally unavailable until Story 17.4b".to_owned(),
        ))
    }
}
