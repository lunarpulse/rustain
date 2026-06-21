//! Integration test: ADR-10-5 sibling (corrective) — a legacy per-agent tool must
//! accept the payload its own schema advertises.
//!
//! Defect: discovered custom agents (e.g. `code-reviewer`) are registered with a
//! schema that defines and requires ONLY `prompt` (`subagent_provider.rs:152-158`),
//! but `invoke_legacy_agent` forwards the payload to `invoke_task`, which
//! hard-requires an unadvertised `description` field (`:208-213`) and then
//! discards it (`:227`). Any LLM that faithfully follows the advertised schema
//! sends `{"prompt": ...}` and hits `Missing 'description'`.
//!
//! This test invokes the `code-reviewer` capability with EXACTLY the payload its
//! schema advertises (`{"prompt": ...}`) and asserts success. It fails today.

mod common;
use common::stub_subagent::StubSubagentRunner;

use std::sync::Arc;

use rustain::adapters::agent_registry::AgentRegistry;
use rustain::adapters::subagent::SubagentProvider;
use rustain::domain::models::{AgentDef, NodeState};
use rustain::domain::ports::{CapabilityProvider, ProviderInfoPort, SubagentRunner};
use rustain::infrastructure::subagent::{NodeTree, SubagentSpool};
use tokio_util::sync::CancellationToken;

// Minimal stub info port — mirrors conformance_subagent_provider_protocol.rs.
struct StubInfo;
impl ProviderInfoPort for StubInfo {
    fn active_delegate_id(&self) -> String {
        "stub".into()
    }
    fn get_model(
        &self,
        _: &str,
        _: &str,
    ) -> Option<rustain::domain::models::provider::ModelDescriptor> {
        None
    }
    fn get_model_provider(&self, _: &str, _: Option<&str>) -> Option<String> {
        None
    }
    fn list_providers(&self) -> Vec<rustain::domain::models::provider::ProviderDescriptor> {
        Vec::new()
    }
    fn list_models_by_provider(
        &self,
        _: &str,
    ) -> Vec<rustain::domain::models::provider::ModelDescriptor> {
        Vec::new()
    }
    fn get_provider(&self, _: &str) -> Option<Arc<dyn rustain::domain::ports::StreamingProvider>> {
        None
    }
    fn set_active_provider(&self, _: &str) -> Result<(), rustain::domain::errors::ProviderError> {
        Ok(())
    }
    fn now_unix(&self) -> i64 {
        0
    }
    fn today_start_unix_ms(&self) -> i64 {
        0
    }
}

/// 10.7.3-INT-001 · P0 · A legacy per-agent capability invoked with the payload
/// its schema advertises (prompt-only) must succeed — not fail with
/// `Missing 'description'`. Turns green when `invoke_task` stops hard-requiring
/// the vestigial `description` field.
#[tokio::test]
async fn legacy_agent_prompt_only_invoke_succeeds() {
    let runner = Arc::new(StubSubagentRunner::new(
        NodeState::Completed,
        "code review complete",
    )) as Arc<dyn SubagentRunner>;
    let registry = Arc::new(NodeTree::new());
    let agent_registry = Arc::new(tokio::sync::RwLock::new(AgentRegistry::from_agents(vec![
        AgentDef {
            name: "code-reviewer".into(),
            description: "Reviews code for bugs".into(),
            file: std::path::PathBuf::from("/tmp/code-reviewer.md"),
            allowed_tools: Some(vec!["Read".into(), "Grep".into()]),
            exclude_tools: None,
            model: None,
        },
    ])));
    let model_router = Arc::new(StubInfo) as Arc<dyn ProviderInfoPort>;
    let tmp = tempfile::tempdir().unwrap();
    let spool = Arc::new(SubagentSpool::new(tmp.path().join("spool")).await.unwrap());

    let provider = SubagentProvider::new(runner, registry, agent_registry, model_router, spool);

    // Discover the code-reviewer capability, then invoke with its advertised schema.
    let caps = provider.discover().await.unwrap();
    let cap = caps
        .iter()
        .find(|c| c.id.tool == "code-reviewer")
        .expect("code-reviewer capability must be advertised");

    let result = provider
        .invoke(
            &cap.id,
            serde_json::json!({"prompt": "review this"}),
            CancellationToken::new(),
        )
        .await;

    match result {
        Ok(tool_result) => assert!(
            !tool_result.is_error,
            "legacy agent prompt-only invoke must succeed; got error content: {}",
            tool_result.content
        ),
        Err(e) => panic!(
            "legacy agent prompt-only invoke must not fail — the advertised schema omits \
             `description`, so the impl must not require it (subagent_provider.rs:208-213): {e}"
        ),
    }
}
