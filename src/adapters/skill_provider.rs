//! Skills capability provider — wraps `SkillActivator` + `SkillRegistry`
//! behind the `CapabilityProvider` trait (Story 9.3b).

use async_trait::async_trait;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

use crate::adapters::skill_activation::SkillActivator;
use crate::domain::models::ToolResult;
use crate::domain::models::capability::{Capability, CapabilityError};
use crate::domain::models::capability_id::CapabilityId;
use crate::domain::models::provider_capabilities::{ProviderCapabilities, TransportKind};
use crate::domain::ports::CapabilityProvider;

pub struct SkillsProvider {
    activator: Arc<SkillActivator>,
}

impl SkillsProvider {
    pub fn new(activator: Arc<SkillActivator>) -> Self {
        Self { activator }
    }
}

#[async_trait]
impl CapabilityProvider for SkillsProvider {
    fn protocol(&self) -> &str {
        "skill"
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            supports_streaming: false,
            supports_list_changed: false,
            supports_native_retrieval: None,
            max_tool_count: None,
            transport_kind: TransportKind::InProcess,
        }
    }

    async fn discover(&self) -> Result<Vec<Capability>, CapabilityError> {
        let registry = self.activator.registry_arc();
        let guard = registry.read().await;
        Ok(guard
            .skills()
            .iter()
            .map(|def| Capability {
                id: CapabilityId {
                    protocol: "skill".into(),
                    server: String::new(),
                    tool: def.name.clone(),
                },
                name: def.name.clone(),
                description: def.description.clone(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "arguments": { "type": "string", "description": "Optional trailing arguments passed to the skill" }
                    },
                    "required": []
                }),
                parallel_safe: false,
            })
            .collect())
    }

    async fn invoke(
        &self,
        capability_id: &CapabilityId,
        input: serde_json::Value,
        _cancel: CancellationToken,
    ) -> Result<ToolResult, CapabilityError> {
        let arguments = input
            .get("arguments")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        Err(CapabilityError::InvocationFailed(
            capability_id.as_string(),
            format!(
                "skill capabilities are activated via the 'activate_skill' builtin tool, not via CPA invoke(); see Decision Gate 3b.3 in story 9.3b. Skill: {} (arguments: {:?})",
                capability_id.tool, arguments
            ),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_provider_protocol_returns_skill() {
        let activator = Arc::new(SkillActivator::new());
        let provider = SkillsProvider::new(activator);
        assert_eq!(provider.protocol(), "skill");
    }

    #[tokio::test]
    async fn test_discover_empty_registry_returns_empty() {
        let activator = Arc::new(SkillActivator::new());
        let provider = SkillsProvider::new(activator);
        let caps = provider.discover().await.unwrap();
        assert!(caps.is_empty());
    }

    #[tokio::test]
    async fn test_invoke_returns_invoke_error_phase_a() {
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
}
