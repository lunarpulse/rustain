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
    peers: super::driver::A2aPeerBindings,
    delegation: std::sync::OnceLock<Arc<super::driver::A2aDelegationRuntime>>,
}

impl A2aProvider {
    pub fn new(peers: Vec<(A2aPeerSpec, Arc<A2aClientAdapter>)>) -> Self {
        Self {
            peers: peers.into(),
            delegation: std::sync::OnceLock::new(),
        }
    }

    pub(crate) fn peer_bindings(&self) -> super::driver::A2aPeerBindings {
        self.peers.clone()
    }

    /// Inject the delegation runtime (node tree + journal + event sink) after
    /// the composition root has opened them. Until this is set, `invoke()`
    /// returns the Story 17.4b refusal — discovery/inventory still work.
    pub fn set_delegation_runtime(&self, runtime: Arc<super::driver::A2aDelegationRuntime>) {
        let _ = self.delegation.set(runtime);
    }

    pub fn delegation_runtime(&self) -> Option<&Arc<super::driver::A2aDelegationRuntime>> {
        self.delegation.get()
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
        for (peer, client) in self.peers.iter() {
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

/// Collect A2A mention entries from the callable capability registry.
///
/// Story 19.13 backs the `@A2A/` namespace from the same snapshot that
/// `CompositeToolsetAdapter::available_tools` projects into model-visible
/// tools. A cached AgentCard is not enough: a suggestion is shown only while
/// its registration is alive and therefore callable.
///
/// `filter` is a case-insensitive substring match on the raw skill id or
/// description. Registry snapshot order is preserved.
pub fn collect_a2a_autocomplete(
    capabilities: &[crate::domain::models::RegisteredCapability],
    filter: Option<&str>,
) -> Vec<crate::domain::models::autocomplete::A2aAgentInfo> {
    let filter_lower = filter.map(str::to_lowercase);

    capabilities
        .iter()
        .filter(|capability| capability.protocol == "a2a")
        .filter(|capability| {
            filter_lower.as_ref().is_none_or(|filter| {
                capability.id.tool.to_lowercase().contains(filter)
                    || capability.description.to_lowercase().contains(filter)
            })
        })
        .map(
            |capability| crate::domain::models::autocomplete::A2aAgentInfo {
                peer: capability.id.server.clone(),
                name: capability.id.tool.clone(),
                description: capability.description.clone(),
            },
        )
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::a2a::client::A2aClientAdapter;
    use crate::domain::models::{A2aPeerSource, RedactedUrl};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn spec(url: String) -> A2aPeerSpec {
        A2aPeerSpec {
            id: "security-peer".to_owned(),
            url: RedactedUrl::from(url),
            pinned_key: None,
            source: A2aPeerSource::Workspace,
        }
    }

    const MULTI_SKILL_CARD: &str = r#"{
      "name":"Multi Skill Peer",
      "skills":[
        {"id":"scan","name":"Security Scan","description":"Scans a repository","tags":["security"]},
        {"id":"deploy","name":"Ship It","description":"Deploys the service","tags":["ops"]},
        {"id":"bad::id","name":"Separator","description":"reserved separator","tags":[]}
      ]
    }"#;

    /// Prime the cache through a loopback mock server, then drop it: any
    /// on-demand fetch inside `collect_a2a_autocomplete` would fail loudly.
    async fn primed_provider() -> (A2aProvider, A2aPeerSpec) {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/.well-known/agent-card.json"))
            .respond_with(
                ResponseTemplate::new(200).set_body_raw(MULTI_SKILL_CARD, "application/json"),
            )
            .expect(1)
            .mount(&server)
            .await;
        let peer = spec(server.uri());
        let client = Arc::new(A2aClientAdapter::new(&peer, None).expect("client"));
        client.refresh_agent_card(&peer).await.expect("prime cache");
        drop(server);
        (A2aProvider::new(vec![(peer.clone(), client)]), peer)
    }

    #[tokio::test]
    async fn collect_on_a_cold_cache_is_ok_and_empty() {
        let peer = spec("http://127.0.0.1:9".to_owned());
        let client = Arc::new(A2aClientAdapter::new(&peer, None).expect("loopback client"));
        let provider = A2aProvider::new(vec![(peer, client)]);

        let registry = Arc::new(crate::domain::models::CapabilityRegistry::new(None));
        let _handles = registry
            .discover_and_register_all(&provider, "a2a")
            .await
            .expect("empty cache is not an error");
        let infos = collect_a2a_autocomplete(&registry.snapshot_consistent().await, None);
        assert!(infos.is_empty());
    }

    #[tokio::test]
    async fn collect_projects_configured_peer_and_raw_skill_id_skipping_reserved_ids() {
        let (provider, peer) = primed_provider().await;

        let registry = Arc::new(crate::domain::models::CapabilityRegistry::new(None));
        let _handles = registry
            .discover_and_register_all(&provider, "a2a")
            .await
            .expect("cached collect");
        let infos = collect_a2a_autocomplete(&registry.snapshot_consistent().await, None);
        assert_eq!(infos.len(), 2, "the `::` skill id must be excluded");
        let scan = infos
            .iter()
            .find(|info| info.name == "scan")
            .expect("scan skill");
        assert_eq!(
            scan,
            &crate::domain::models::autocomplete::A2aAgentInfo {
                peer: peer.id.clone(),
                name: "scan".to_owned(),
                description: "Scans a repository".to_owned(),
            },
            "name must be the raw skill id, not the `Security Scan` title"
        );
        let deploy = infos
            .iter()
            .find(|info| info.name == "deploy")
            .expect("deploy skill");
        assert_eq!(deploy.peer, peer.id);
    }

    #[tokio::test]
    async fn collect_filters_case_insensitively_on_raw_id_and_description_only() {
        let (provider, _) = primed_provider().await;

        let registry = Arc::new(crate::domain::models::CapabilityRegistry::new(None));
        let _handles = registry
            .discover_and_register_all(&provider, "a2a")
            .await
            .expect("cached collect");
        let capabilities = registry.snapshot_consistent().await;
        let hits = collect_a2a_autocomplete(&capabilities, Some("REPOSITORY"));
        assert_eq!(
            hits.len(),
            1,
            "uppercase filter matches the description case-insensitively"
        );
        assert_eq!(hits[0].name, "scan");

        let title_hits = collect_a2a_autocomplete(&capabilities, Some("Security Scan"));
        assert!(
            title_hits.is_empty(),
            "the human skill title is not a filter key — only the raw id and description are"
        );
    }
}
