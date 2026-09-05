//! Shared outbound A2A composition for standalone and daemon roots.

use std::sync::Arc;

use anyhow::Result;
use tokio::sync::mpsc::UnboundedSender;

use crate::adapters::composite_toolset_adapter::CompositeToolsetAdapter;
use crate::domain::events::AppEvent;
use crate::domain::models::A2aPeerSpec;
use crate::domain::ports::{CapabilityProvider, RoomJournal};
use crate::infrastructure::subagent::NodeTree;

use super::client::A2aClientAdapter;
use super::driver::A2aDelegationRuntime;
use super::provider::A2aProvider;

/// The one outbound A2A runtime shared by a composition root's provider and
/// capability composite. AgentCard refreshes are deliberately un-awaited: a
/// peer that is still offline at boot remains unavailable until a later boot.
#[derive(Clone)]
pub struct A2aEgress {
    provider: Arc<A2aProvider>,
    runtime: Arc<A2aDelegationRuntime>,
}

impl A2aEgress {
    pub fn compose(
        peers: Vec<A2aPeerSpec>,
        node_tree: NodeTree,
        journal: Arc<dyn RoomJournal>,
        event_tx: UnboundedSender<AppEvent>,
    ) -> Result<Self> {
        let mut bindings = Vec::with_capacity(peers.len());
        for spec in peers {
            let client = Arc::new(A2aClientAdapter::new(&spec, None).map_err(|error| {
                anyhow::anyhow!("A2A peer {:?} configuration failed: {error}", spec.id)
            })?);
            bindings.push((spec, client));
        }

        let provider = Arc::new(A2aProvider::new(bindings));
        let runtime = Arc::new(
            A2aDelegationRuntime::new(node_tree, journal, event_tx.clone())
                .with_peer_bindings(provider.peer_bindings()),
        );
        provider.set_delegation_runtime(runtime.clone());

        for (spec, client) in provider.peer_bindings().iter() {
            let spec = spec.clone();
            let client = client.clone();
            let event_tx = event_tx.clone();
            tokio::spawn(async move {
                match client.refresh_agent_card(&spec).await {
                    Ok(()) => {
                        let skill_count = client
                            .cached_card()
                            .await
                            .map(|(card, _)| card.skills.len())
                            .unwrap_or(0);
                        let _ = event_tx.send(AppEvent::A2aCatalogChanged {
                            peer_id: spec.id,
                            skill_count,
                        });
                    }
                    Err(error) => {
                        tracing::warn!(
                            peer_id = %spec.id,
                            %error,
                            "A2A AgentCard refresh failed"
                        );
                    }
                }
            });
        }

        Ok(Self { provider, runtime })
    }

    pub fn install(&self, composite: &CompositeToolsetAdapter) {
        composite.set_a2a_provider(self.provider.clone() as Arc<dyn CapabilityProvider>);
    }

    pub fn provider(&self) -> &Arc<A2aProvider> {
        &self.provider
    }

    pub fn runtime(&self) -> &Arc<A2aDelegationRuntime> {
        &self.runtime
    }
}
