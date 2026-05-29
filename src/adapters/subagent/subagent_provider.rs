use async_trait::async_trait;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

use crate::domain::models::{
    Capability, CapabilityError, CapabilityId, ProviderCapabilities, ToolResult, TransportKind,
};
use crate::domain::ports::CapabilityProvider;

pub struct SubagentProvider {
    runner: Arc<dyn crate::domain::ports::SubagentRunner>,
    registry: Arc<crate::infrastructure::subagent::SubagentRegistry>,
    agent_registry: Arc<tokio::sync::RwLock<crate::adapters::agent_registry::AgentRegistry>>,
    model_router: Arc<dyn crate::domain::ports::ProviderInfoPort>,
    spool: Arc<crate::infrastructure::subagent::SubagentSpool>,
}

impl SubagentProvider {
    pub fn registry(&self) -> &Arc<crate::infrastructure::subagent::SubagentRegistry> {
        &self.registry
    }

    pub fn spool(&self) -> &Arc<crate::infrastructure::subagent::SubagentSpool> {
        &self.spool
    }

    pub fn runner(&self) -> &Arc<dyn crate::domain::ports::SubagentRunner> {
        &self.runner
    }

    pub fn agent_registry(
        &self,
    ) -> &Arc<tokio::sync::RwLock<crate::adapters::agent_registry::AgentRegistry>> {
        &self.agent_registry
    }

    pub fn new(
        runner: Arc<dyn crate::domain::ports::SubagentRunner>,
        registry: Arc<crate::infrastructure::subagent::SubagentRegistry>,
        agent_registry: Arc<tokio::sync::RwLock<crate::adapters::agent_registry::AgentRegistry>>,
        model_router: Arc<dyn crate::domain::ports::ProviderInfoPort>,
        spool: Arc<crate::infrastructure::subagent::SubagentSpool>,
    ) -> Self {
        Self {
            runner,
            registry,
            agent_registry,
            model_router,
            spool,
        }
    }
}

#[async_trait]
impl CapabilityProvider for SubagentProvider {
    fn protocol(&self) -> &str {
        "subagent"
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            supports_streaming: false,
            supports_list_changed: true,
            supports_native_retrieval: None,
            max_tool_count: Some(10),
            transport_kind: TransportKind::InProcess,
        }
    }

    async fn discover(&self) -> Result<Vec<Capability>, CapabilityError> {
        // Story 10.0 foundation — discover from AgentRegistry.
        let guard = self.agent_registry.read().await;
        let agents = guard.agents();
        Ok(agents
            .iter()
            .cloned()
            .map(|agent| Capability {
                id: CapabilityId {
                    protocol: "subagent".into(),
                    server: String::new(),
                    tool: agent.name.clone(),
                },
                name: agent.name,
                description: agent.description,
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "prompt": { "type": "string" }
                    },
                    "required": ["prompt"]
                }),
                parallel_safe: false,
            })
            .collect())
    }

    async fn invoke(
        &self,
        _id: &CapabilityId,
        _input: serde_json::Value,
        _cancel: CancellationToken,
    ) -> Result<ToolResult, CapabilityError> {
        // Story 10.7 will wire the full invoke → launch → await → read spool tail flow.
        Err(CapabilityError::InvocationFailed(
            "subagent".into(),
            "SubagentProvider::invoke is reserved for Story 10.7".into(),
        ))
    }
}
