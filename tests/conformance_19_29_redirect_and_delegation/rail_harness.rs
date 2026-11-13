//! Shared production stack for the `task` delegation rail (Stories 19.28/19.29).
//!
//! Real `CompositeToolsetAdapter` + real `SubagentProvider` (authority bound
//! exactly as startup binds it) + a runner that records every
//! `AgentLaunchSpec` the production path builds and completes immediately.
//! Included by the `skills` area binary (through this directory's `mod.rs`)
//! and by the `a2a` area binary (through
//! `conformance_19_28_parent_restriction_rail.rs`), so there is ONE copy.

#![allow(dead_code)]

use std::sync::Arc;

use async_trait::async_trait;
use rustain::adapters::composite_toolset_adapter::CompositeToolsetAdapter;
use rustain::domain::errors::ToolError;
use rustain::domain::models::SubagentError;
use rustain::domain::models::capability_registry::{RegisterHandle, RegisteredCapability};
use rustain::domain::models::{
    AgentId, AgentLaunchSpec, CapabilityId, ModelDescriptor, NodeState, Op, ToolDefinition,
    ToolResult, TrustTier,
};
use rustain::domain::ports::{ProviderInfoPort, SubagentRunner, ToolSetPort};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Captures every `AgentLaunchSpec` the production `task` path builds, and
/// completes immediately so `invoke_task`'s terminal-status await returns.
pub(crate) struct RecordingRunner {
    pub(crate) specs: Arc<parking_lot::Mutex<Vec<AgentLaunchSpec>>>,
}

#[async_trait]
impl SubagentRunner for RecordingRunner {
    async fn launch(
        &self,
        spec: AgentLaunchSpec,
        cancel: CancellationToken,
        _parent: Option<&rustain::domain::models::TaskHandle>,
        agent_id: AgentId,
    ) -> Result<rustain::domain::models::TaskHandle, SubagentError> {
        self.specs.lock().push(spec);

        let (status_tx, status_rx) = mpsc::channel(4);
        status_tx
            .send(NodeState::Completed)
            .await
            .expect("queue terminal status");
        let (command_tx, _command_rx) = mpsc::channel::<Op>(4);
        let (parent_disconnect, _parent_disconnect_rx) = mpsc::unbounded_channel();

        Ok(rustain::domain::models::TaskHandle {
            agent_id,
            status_rx,
            command_tx,
            cancel,
            task_id: "rail-probe".to_string(),
            subagent_type: "worker".to_string(),
            spawned_at: 0,
            parent_disconnect,
            yield_rx: None,
            isolation_diff_rx: None,
            effective_workspace: std::path::PathBuf::from("."),
            isolated: false,
            authority: rustain::domain::models::CapabilityTokenId::root(),
            authority_token: None,
            patch_provenance: rustain::domain::models::ProvenanceTag::UserOriginated,
        })
    }
}

pub(crate) struct StubModelRouter;

impl ProviderInfoPort for StubModelRouter {
    fn active_delegate_id(&self) -> String {
        "stub".to_string()
    }
    fn get_model(&self, _provider_id: &str, _model_id: &str) -> Option<ModelDescriptor> {
        None
    }
    fn get_model_provider(&self, _model_id: &str, _prefer: Option<&str>) -> Option<String> {
        None
    }
    fn list_providers(&self) -> Vec<rustain::domain::models::ProviderDescriptor> {
        Vec::new()
    }
    fn list_models_by_provider(&self, _provider_id: &str) -> Vec<ModelDescriptor> {
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

pub(crate) struct NoTools;

#[async_trait]
impl ToolSetPort for NoTools {
    fn available_tools(&self) -> Vec<ToolDefinition> {
        Vec::new()
    }
    async fn execute(
        &self,
        _tool_name: &str,
        _input: serde_json::Value,
        _cancel: CancellationToken,
    ) -> Result<ToolResult, ToolError> {
        Err(ToolError::NotFound("no builtin tools".into()))
    }
}

/// The production stack behind `task`.
pub(crate) struct RailStack {
    pub(crate) cta: Arc<CompositeToolsetAdapter>,
    pub(crate) specs: Arc<parking_lot::Mutex<Vec<AgentLaunchSpec>>>,
    pub(crate) workspace: tempfile::TempDir,
    /// Dropping the handle deregisters `task`, so the stack owns it.
    pub(crate) _task_registration: RegisterHandle,
}

pub(crate) async fn rail_stack() -> RailStack {
    let tmp = tempfile::tempdir().unwrap();
    let specs = Arc::new(parking_lot::Mutex::new(Vec::new()));

    let runner = Arc::new(RecordingRunner {
        specs: specs.clone(),
    }) as Arc<dyn SubagentRunner>;
    let node_tree = Arc::new(rustain::infrastructure::subagent::NodeTree::new());
    let agent_registry = Arc::new(tokio::sync::RwLock::new(
        rustain::adapters::agent_registry::AgentRegistry::new(),
    ));
    let spool = Arc::new(
        rustain::infrastructure::subagent::SubagentSpool::new(tmp.path().join("spool"))
            .await
            .unwrap(),
    );
    let provider = Arc::new(rustain::adapters::subagent::SubagentProvider::new(
        runner,
        node_tree,
        agent_registry,
        Arc::new(StubModelRouter) as Arc<dyn ProviderInfoPort>,
        spool,
    ));

    // `SubagentProvider` fails closed on spawn when authority is unbound, so a
    // rail test must bind it exactly as startup does — otherwise every case
    // "passes" by being refused before the rail is ever read.
    let root = rustain::domain::models::CapabilityToken::r1_root(AgentId::root());
    let ledger = Arc::new(
        rustain::domain::services::authority_ledger::AuthorityLedger::new(
            root.clone(),
            Arc::new(rustain::domain::clock::SystemClock::default()),
        ),
    );
    provider
        .set_authority(
            Arc::new(rustain::adapters::authority::InProcessAuthorityProvider::new(ledger))
                as Arc<dyn rustain::domain::ports::AuthorityProvider>,
            root,
        )
        .await;

    let cta = Arc::new(CompositeToolsetAdapter::new(
        Arc::new(NoTools) as Arc<dyn ToolSetPort>,
        Vec::new(),
        Vec::new(),
        true,
        None,
        None,
        Some(provider),
    ));

    // The composite adapter routes `task` by capability, not by name table.
    let handle = cta
        .capability_registry()
        .register(RegisteredCapability {
            trust: TrustTier::Verified,
            id: CapabilityId {
                protocol: "subagent".into(),
                server: String::new(),
                tool: "task".into(),
            },
            protocol: "subagent".into(),
            provider_id: "subagent".into(),
            name: "task".into(),
            description: "task".into(),
            input_schema: serde_json::json!({"type": "object"}),
            parallel_safe: true,
        })
        .await
        .expect("register task capability");

    RailStack {
        cta,
        specs,
        workspace: tmp,
        _task_registration: handle,
    }
}
