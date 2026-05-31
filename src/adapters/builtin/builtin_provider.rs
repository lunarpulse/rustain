//! Built-in capability provider — wraps `ToolSetAdapter` behind the
//! `CapabilityProvider` trait (Story 9.3b).
//!
//! `BuiltinProvider` is a thin wrapper that implements `CapabilityProvider`
//! and delegates to the existing `ToolSetAdapter`. The adapter itself is NOT
//! renamed or rewritten — `available_tools()` and `execute()` continue to be
//! the LLM-wire path (Stories 1.5 + 1.6 + 5.x); the trait is an ADDITIONAL
//! surface for the CPA contract.

use async_trait::async_trait;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

use crate::domain::errors::ToolError;
use crate::domain::models::ToolResult;
use crate::domain::models::capability::{Capability, CapabilityError};
use crate::domain::models::capability_id::CapabilityId;
use crate::domain::models::provider_capabilities::{ProviderCapabilities, TransportKind};
use crate::domain::ports::CapabilityProvider;
use crate::domain::ports::ToolSetPort;

pub struct BuiltinProvider {
    inner: Arc<dyn ToolSetPort>,
}

impl BuiltinProvider {
    pub fn new(inner: Arc<dyn ToolSetPort>) -> Self {
        Self { inner }
    }
}

#[async_trait]
impl CapabilityProvider for BuiltinProvider {
    fn protocol(&self) -> &str {
        "builtin"
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
        Ok(self
            .inner
            .available_tools()
            .into_iter()
            .map(|t| Capability {
                id: CapabilityId {
                    protocol: "builtin".into(),
                    server: String::new(),
                    tool: t.name.clone(),
                },
                name: t.name,
                description: t.description,
                input_schema: t.input_schema,
                parallel_safe: t.parallel_safe,
            })
            .collect())
    }

    async fn invoke(
        &self,
        capability_id: &CapabilityId,
        input: serde_json::Value,
        cancel: CancellationToken,
    ) -> Result<ToolResult, CapabilityError> {
        self.inner
            .execute(&capability_id.tool, input, cancel)
            .await
            .map_err(|e| {
                CapabilityError::InvocationFailed(
                    capability_id.as_string(),
                    format!("builtin {capability_id}: {e}"),
                )
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::filesystem::FileSystemStorage;
    use crate::adapters::toolset_adapter::ToolSetAdapter;
    use arc_swap::ArcSwap;
    use std::path::PathBuf;

    fn make_toolset_adapter() -> Arc<ToolSetAdapter> {
        let storage = Arc::new(FileSystemStorage::new(PathBuf::from(".")));
        Arc::new(ToolSetAdapter::new(
            PathBuf::from("."),
            storage,
            Arc::new(ArcSwap::from_pointee(
                Arc::new(crate::adapters::sandbox::NoOpSandbox)
                    as Arc<dyn crate::domain::ports::SandboxManager>,
            )),
            Arc::new(tokio::sync::RwLock::new(
                crate::domain::models::sandbox::SandboxPolicy::Permissive,
            )),
        ))
    }

    #[test]
    fn test_provider_protocol_returns_builtin() {
        let adapter = make_toolset_adapter();
        let provider = BuiltinProvider::new(adapter);
        assert_eq!(provider.protocol(), "builtin");
    }

    #[test]
    fn test_capabilities_in_process() {
        let adapter = make_toolset_adapter();
        let provider = BuiltinProvider::new(adapter);
        let caps = provider.capabilities();
        assert_eq!(caps.transport_kind, TransportKind::InProcess);
        assert!(!caps.supports_streaming);
        assert!(!caps.supports_list_changed);
    }

    #[tokio::test]
    async fn test_discover_projects_all_builtin_tools() {
        let adapter = make_toolset_adapter();
        let provider = BuiltinProvider::new(adapter);
        let caps = provider.discover().await.unwrap();
        let mut names: Vec<String> = caps.into_iter().map(|c| c.name).collect();
        names.sort();
        #[cfg(not(feature = "meta-search"))]
        {
            assert_eq!(
                names,
                vec![
                    "Bash",
                    "Read",
                    "Write",
                    "activate_skill",
                    "exit_plan_mode",
                    "propose_plan",
                    "remember",
                    "remember_fact",
                    "skill_view",
                ]
            );
        }
        #[cfg(feature = "meta-search")]
        {
            assert_eq!(
                names,
                vec![
                    "Bash",
                    "Read",
                    "Write",
                    "activate_skill",
                    "exit_plan_mode",
                    "propose_plan",
                    "remember",
                    "remember_fact",
                    "search_skills",
                    "search_tools",
                    "skill_view",
                ]
            );
        }
    }
}
