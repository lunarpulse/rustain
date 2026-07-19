#![cfg(feature = "a2a")]

use std::sync::Arc;

use async_trait::async_trait;
use rustain::adapters::a2a::client::A2aClientAdapter;
use rustain::adapters::a2a::provider::A2aProvider;
use rustain::adapters::composite_toolset_adapter::CompositeToolsetAdapter;
use rustain::adapters::tui::handlers::a2a_catalog::handle_a2a_catalog_changed;
use rustain::adapters::tui::state::TuiState;
use rustain::domain::errors::ToolError;
use rustain::domain::models::{
    A2aPeerSource, A2aPeerSpec, Capability, CapabilityError, CapabilityId, ProviderCapabilities,
    RedactedUrl, ToolDefinition, ToolResult, TransportKind, TrustTier,
};
use rustain::domain::ports::{CapabilityProvider, ToolSetPort};
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

struct EmptyBuiltin;

#[async_trait]
impl ToolSetPort for EmptyBuiltin {
    fn available_tools(&self) -> Vec<ToolDefinition> {
        Vec::new()
    }

    async fn execute(
        &self,
        tool_name: &str,
        _input: serde_json::Value,
        _cancel: CancellationToken,
    ) -> Result<ToolResult, ToolError> {
        Err(ToolError::NotFound(tool_name.to_owned()))
    }
}

struct CachedA2a;

#[async_trait]
impl CapabilityProvider for CachedA2a {
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
                server: "peer".to_owned(),
                tool: "scan".to_owned(),
            },
            name: "Scan".to_owned(),
            description: "Remote scan".to_owned(),
            input_schema: serde_json::json!({"type":"object"}),
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
        unreachable!("composition registration never invokes providers")
    }
}

#[tokio::test]
async fn a2a_inventory_enters_the_llm_surface_under_namespaced_wire_names() {
    let composite = CompositeToolsetAdapter::new(
        Arc::new(EmptyBuiltin),
        Vec::new(),
        Vec::new(),
        false,
        None,
        None,
        None,
    );
    composite.set_a2a_provider(Arc::new(CachedA2a));
    composite
        .populate_registry()
        .await
        .expect("A2A registry population");

    let snapshot = composite.capability_registry().snapshot();
    assert_eq!(snapshot.len(), 1);
    assert_eq!(snapshot[0].id.to_string(), "a2a::peer::scan");
    assert_eq!(snapshot[0].trust, TrustTier::Unverified);
    let tools = composite.available_tools();
    assert_eq!(
        tools.len(),
        1,
        "17.4b: A2A skills now enter the LLM-facing surface (delegation is an action)"
    );
    assert_eq!(
        tools[0].name, "a2a__peer__scan",
        "R-D: the namespaced wire name reaches the LLM, built from the CapabilityId"
    );
    assert!(
        !tools.iter().any(|tool| tool.name == "Scan"),
        "R-D security boundary: the raw peer-chosen skill name must never reach the LLM"
    );
}

fn peer_spec(url: String) -> A2aPeerSpec {
    A2aPeerSpec {
        id: "wire-peer".to_owned(),
        url: RedactedUrl::from(url),
        pinned_key: None,
        source: A2aPeerSource::Workspace,
    }
}

fn composite() -> Arc<CompositeToolsetAdapter> {
    Arc::new(CompositeToolsetAdapter::new(
        Arc::new(EmptyBuiltin),
        Vec::new(),
        Vec::new(),
        false,
        None,
        None,
        None,
    ))
}

#[tokio::test]
async fn real_fetch_event_handler_and_composition_register_tiered_inventory() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/.well-known/agent-card.json"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            r#"{"name":"Wire Peer","skills":[{"id":"scan","name":"Scan"}]}"#,
            "application/json",
        ))
        .expect(1)
        .mount(&server)
        .await;
    let peer = peer_spec(server.uri());
    let client = Arc::new(A2aClientAdapter::new(&peer, None).unwrap());
    let composite = composite();
    composite.set_a2a_provider(Arc::new(A2aProvider::new(vec![(
        peer.clone(),
        Arc::clone(&client),
    )])));
    let tools: Arc<dyn ToolSetPort> = composite.clone();

    client.refresh_agent_card(&peer).await.unwrap();
    let mut state = TuiState::new(80, 24);
    handle_a2a_catalog_changed(&mut state, &tools, &peer.id, 1).await;

    let snapshot = composite.capability_registry().snapshot();
    assert_eq!(snapshot.len(), 1);
    assert_eq!(snapshot[0].id.to_string(), "a2a::wire-peer::scan");
    assert_eq!(snapshot[0].trust, TrustTier::Unverified);
    assert!(state.needs_redraw);
    let tools = composite.available_tools();
    assert_eq!(tools.len(), 1, "17.4b: peer skill is LLM-exposed");
    assert_eq!(tools[0].name, "a2a__wire-peer__scan");
    assert!(!tools.iter().any(|tool| tool.name == "Scan"));
}

#[tokio::test]
async fn zero_skill_peer_is_a_valid_empty_inventory_control() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/.well-known/agent-card.json"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(r#"{"name":"Empty Peer","skills":[]}"#, "application/json"),
        )
        .mount(&server)
        .await;
    let peer = peer_spec(server.uri());
    let client = Arc::new(A2aClientAdapter::new(&peer, None).unwrap());
    client.refresh_agent_card(&peer).await.unwrap();
    let composite = composite();
    composite.set_a2a_provider(Arc::new(A2aProvider::new(vec![(peer, client)])));
    composite.populate_registry().await.unwrap();

    assert!(composite.capability_registry().snapshot().is_empty());
}
