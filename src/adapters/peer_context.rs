//! `PeerContextProvider` — peer-origin context behind the existing
//! `ContextPort` (Story 18.4a, FR151).
//!
//! # An adapter, not a port
//!
//! `ContextPort` has two methods and **both are defaulted** under an explicit
//! SDK-stability mandate, so a new provider plugs in without touching
//! `NoOpContext` or `MemoryContextAdapter`. ⛔ Nothing here widens the port.
//!
//! # ⛔ "Zero core change" is false, and is not repeated here
//!
//! FR151 says the provider is *"composited with local memory providers — zero
//! core change."* Measured at `57402a4`, **there was nothing to composite
//! with**: `build_context` was a `match name` over `default` / `daily` / `noop`
//! returning **one** `Arc<dyn ContextPort>`, the slot is a single `ArcSwap`, and
//! `CompositeContext*` had zero hits in the tree. Selecting this provider
//! without building the compositor would have **replaced** the operator's local
//! memory context — a feature that silently deletes another feature. Story
//! 18.4a therefore adds a `"composite"` arm at the composition root. That *is* a
//! core change: small, precedented by the toolset arm, and stated rather than
//! claimed away.
//!
//! # Every entry is tainted, and the body never arrives
//!
//! COLLAB D8: *"all peer-origin context entries are tainted, regardless of
//! signature validity."* Taint is carried by the [`ContextSource::Peer`] variant
//! itself, so there is no field a peer could set to clear it (17.1b's Vex rule).
//!
//! ⚑ **`content` is the signed summary and nothing else.** Bodies are
//! producer-hosted and authority-gated; this provider holds none, requests none,
//! and has no branch that could inline one. That is the structural ratchet
//! behind test gate 8 — *"if not, we lied about handles"* — and it is also a
//! confidentiality property, because a summary is not gated the way a body is.

use std::sync::Arc;

use async_trait::async_trait;

use crate::adapters::rap::PeerTopicStore;
use crate::domain::errors::ContextError;
use crate::domain::models::{
    AssembleDiagnostics, ContextBudget, ContextBundle, ContextSource, HealthSummary,
    ProvenancedEntry, Relevance, RetrievalMethod, estimate_tokens,
};
use crate::domain::ports::ContextPort;

/// Reads the replicated Topic log and renders it as tainted context entries.
pub struct PeerContextProvider {
    topics: Arc<PeerTopicStore>,
    /// Wall-millisecond clock. Handles carry a `not_after` in the same unit the
    /// peer-frame path uses, so an expired handle drops out of assembly without
    /// anything having to sweep the store.
    now: Arc<dyn Fn() -> i64 + Send + Sync>,
}

impl std::fmt::Debug for PeerContextProvider {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("PeerContextProvider").finish()
    }
}

impl PeerContextProvider {
    #[must_use]
    pub fn new(topics: Arc<PeerTopicStore>) -> Self {
        Self {
            topics,
            now: Arc::new(|| {
                crate::domain::clock::Clock::wall_now_ms(
                    &crate::domain::clock::SystemClock::default(),
                )
            }),
        }
    }

    /// Inject a deterministic clock (hermetic verification).
    #[must_use]
    pub fn with_now(mut self, now: impl Fn() -> i64 + Send + Sync + 'static) -> Self {
        self.now = Arc::new(now);
        self
    }
}

#[async_trait]
impl ContextPort for PeerContextProvider {
    async fn assemble(
        &self,
        _query: &str,
        budget: ContextBudget,
    ) -> Result<ContextBundle, ContextError> {
        // ⛔ The query is deliberately unused: a Topic is a *replicated log*,
        // not a search index, and filtering peer assertions by the operator's
        // wording would make the bundle a function of the prompt rather than of
        // the log. NFR71's determinism is over the handle set.
        let handles = self.topics.live_handles((self.now)()).await;

        let mut entries: Vec<ProvenancedEntry> = Vec::with_capacity(handles.len());
        let mut per_source: Vec<(ContextSource, usize)> = Vec::new();
        let mut total = 0usize;
        let mut truncated = false;

        for handle in handles {
            let source = ContextSource::Peer(handle.issuer.clone());
            // ⚑ **The signed summary, verbatim, and never a fetched body.**
            // There is no cache to consult and no branch to add one to: the
            // store holds handles. A future "inline the body since we already
            // fetched it" convenience would have to add a field here, and
            // `peer_entry_content_is_only_ever_the_signed_summary` turns RED
            // the moment it does.
            let content: Arc<str> = Arc::from(handle.summary.as_str());
            let entry = ProvenancedEntry {
                source: source.clone(),
                content,
                // Handles are not timestamped by the producer; a fabricated
                // local receipt time would read as the producer's. `0` is the
                // shipped "unknown" value.
                timestamp: 0,
                // Peer handles are *retrieved structurally* — they are in the
                // log because a peer put them there, not because a query
                // matched. ⛔ Not `VectorHit`: nothing scored them.
                retrieval_method: RetrievalMethod::Structural,
                relevance: Relevance::Unscored,
            };
            let cost = estimate_tokens(&entry.render_line());
            if total.saturating_add(cost) > budget.max_tokens {
                // Deterministic truncation: the handle order is already the
                // total order the pure core produced, so which entries survive
                // a budget is a function of the set, never of arrival.
                truncated = true;
                break;
            }
            total += cost;
            match per_source.iter_mut().find(|(seen, _)| *seen == source) {
                Some((_, running)) => *running += cost,
                None => per_source.push((source, cost)),
            }
            entries.push(entry);
        }

        Ok(ContextBundle {
            entries,
            diagnostics: AssembleDiagnostics {
                per_source_tokens: per_source,
                total_tokens: total,
                truncated,
                deduped_count: 0,
                ..AssembleDiagnostics::default()
            },
        })
    }

    fn health_snapshot(&self) -> HealthSummary {
        HealthSummary::unknown()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::rap::TopicGossip;
    use crate::domain::models::{
        AgentEnvelopeHeader, AgentId, ArtifactId, ContentHash, ContextRef, ContextRefProvenance,
        ContextSummary, CorrelationId, MessageKind, PeerId,
    };

    fn peer(seed: u8) -> PeerId {
        PeerId::from_public_key(&[seed; 32]).expect("valid key length")
    }

    fn header(sequence: u64) -> AgentEnvelopeHeader {
        AgentEnvelopeHeader {
            sender: AgentId::parse("sender").expect("valid agent id"),
            recipient: AgentId::parse("recipient").expect("valid agent id"),
            correlation_id: CorrelationId::new("architecture"),
            kind: MessageKind::TopicGossip,
            sequence,
            not_after: 10_000,
            nonce: format!("n{sequence}"),
            content_hash: vec![sequence as u8; 32],
            prev_hash: Vec::new(),
        }
    }

    fn handle(content: u8, issuer: &PeerId, summary: &str) -> ContextRef {
        let hash = ContentHash::from_bytes([content; 32]);
        ContextRef {
            artifact: ArtifactId::from(hash),
            content_hash: hash,
            producer: AgentId::parse("producer").expect("valid agent id"),
            issuer: issuer.clone(),
            summary: ContextSummary::new(summary).expect("bounded"),
            provenance: ContextRefProvenance::Authored,
            not_after: 10_000,
        }
    }

    #[tokio::test]
    async fn peer_entry_content_is_only_ever_the_signed_summary() {
        let store = Arc::new(PeerTopicStore::new());
        let issuer = peer(3);
        store
            .admit(
                &header(1),
                &issuer,
                TopicGossip {
                    refs: vec![handle(1, &issuer, "the event bus is NATS")],
                    heads: Vec::new(),
                },
                0,
            )
            .await
            .expect("admitted");
        let provider = PeerContextProvider::new(Arc::clone(&store)).with_now(|| 0);
        let bundle = provider
            .assemble("anything", ContextBudget::new(4096))
            .await
            .expect("assembles");
        assert_eq!(bundle.entries.len(), 1);
        // ⚑ The structural ratchet (AC6 (b), Rule 4). The mutant — make the
        // assembler write a fetched body into `content` when one is cached —
        // turns this RED. ⛔ Asserting "the body is absent" would have been
        // green from birth; this asserts the content **is** the summary.
        assert_eq!(&*bundle.entries[0].content, "the event bus is NATS");
        assert!(bundle.has_peer_origin());
        assert_eq!(bundle.entries[0].source, ContextSource::Peer(issuer));
    }

    #[tokio::test]
    async fn an_expired_handle_leaves_the_bundle_without_touching_the_log() {
        let store = Arc::new(PeerTopicStore::new());
        let issuer = peer(4);
        store
            .admit(
                &header(1),
                &issuer,
                TopicGossip {
                    refs: vec![handle(2, &issuer, "stale claim")],
                    heads: Vec::new(),
                },
                0,
            )
            .await
            .expect("admitted");
        let live = PeerContextProvider::new(Arc::clone(&store)).with_now(|| 10_000);
        assert_eq!(
            live.assemble("", ContextBudget::new(4096))
                .await
                .expect("assembles")
                .entries
                .len(),
            1
        );
        let expired = PeerContextProvider::new(store).with_now(|| 10_001);
        assert!(
            expired
                .assemble("", ContextBudget::new(4096))
                .await
                .expect("assembles")
                .is_empty()
        );
    }
}
