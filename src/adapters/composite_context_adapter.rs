//! Composite context adapter — composes the local memory context with the
//! peer-origin context (Story 18.4a, FR151).
//!
//! # Why this type exists at all
//!
//! FR151 reads *"a `PeerContextProvider: ContextPort` adapter **composited with
//! local memory providers** — zero core change."* Measured at `57402a4`, the
//! composite half of that sentence named a mechanism that **did not exist**:
//! `build_context` returned exactly one `Arc<dyn ContextPort>` from a three-arm
//! match, the runtime slot is a single `ArcSwap<Arc<dyn ContextPort>>` written
//! by one `store()`, and `CompositeContext*` / `Vec<Arc<dyn ContextPort>>` had
//! **zero hits** in the tree. Selecting the peer provider would therefore have
//! **replaced** the operator's local memory context — a feature that silently
//! deletes another feature.
//!
//! ⛔ The requirement's "zero core change" clause is **false** and is not
//! repeated anywhere in this cut. A new composition-root arm is a core change.
//! It is small and precedented — the toolset dimension has had exactly this
//! shape since Story 9.1, and this type mirrors it rather than inventing a
//! second one — and saying so is the honest line.
//!
//! # What compositing must preserve
//!
//! * **Per-entry `ContextSource`.** A merged bundle whose entries lost their
//!   source would lose the taint bit with it, because taint is derived from the
//!   variant (`ContextSource::is_peer_origin`).
//! * **Determinism (NFR71).** The merge is a concatenation in a fixed member
//!   order, each member's own order preserved. ⛔ No `HashMap`, no interleave by
//!   score, no time-dependent tie-break.
//! * **The budget.** Each member assembles inside the same budget and the merge
//!   truncates the tail rather than letting two members each spend it.

use std::sync::Arc;

use async_trait::async_trait;

use crate::domain::errors::ContextError;
use crate::domain::models::{
    AssembleDiagnostics, ContextBudget, ContextBundle, ContextSource, HealthSummary,
    ProvenancedEntry, estimate_tokens,
};
use crate::domain::ports::ContextPort;

/// Composes a local context provider with a peer-origin one.
pub struct CompositeContextAdapter {
    local: Arc<dyn ContextPort>,
    peer: Arc<dyn ContextPort>,
}

impl std::fmt::Debug for CompositeContextAdapter {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("CompositeContextAdapter").finish()
    }
}

impl CompositeContextAdapter {
    /// `local` is assembled first and therefore wins the budget.
    ///
    /// ⚑ **The order is a security property, not a preference.** A signature is
    /// attribution, never truth (COLLAB invariant 15), so a peer's assertion
    /// must never be able to push the operator's own memory out of a
    /// budget-truncated bundle. Local first makes that structural rather than
    /// hoped-for.
    #[must_use]
    pub fn new(local: Arc<dyn ContextPort>, peer: Arc<dyn ContextPort>) -> Self {
        Self { local, peer }
    }
}

#[async_trait]
impl ContextPort for CompositeContextAdapter {
    async fn assemble(
        &self,
        query: &str,
        budget: ContextBudget,
    ) -> Result<ContextBundle, ContextError> {
        // ⛔ Sequential, not `join!`: the local half's spend decides how much
        // budget the peer half is offered, and a concurrent pair would have to
        // guess. The peer half is an in-memory read, so there is nothing to
        // overlap with.
        let local = self.local.assemble(query, budget).await?;
        let spent = local.diagnostics.total_tokens;
        let remaining = budget.max_tokens.saturating_sub(spent);
        let peer = self
            .peer
            .assemble(query, ContextBudget::new(remaining))
            .await?;
        Ok(merge(local, peer))
    }

    fn health_snapshot(&self) -> HealthSummary {
        // The local provider is the one an operator configured deliberately;
        // the peer provider holds no I/O of its own to be unhealthy about.
        self.local.health_snapshot()
    }
}

/// Concatenate two bundles, preserving each entry's source and each member's
/// order.
///
/// ⛔ Not a merge in the CRDT sense — nothing reconciles, nothing wins, nothing
/// is dropped. `epics.md`'s Epic-18 shared-context invariant: *"this is git, not
/// a CRDT."* A local row and a peer row asserting contradictory things **both**
/// survive, each attributed, for the model to weigh.
fn merge(local: ContextBundle, peer: ContextBundle) -> ContextBundle {
    let mut entries: Vec<ProvenancedEntry> = local.entries;
    entries.extend(peer.entries);

    let mut per_source: Vec<(ContextSource, usize)> = local.diagnostics.per_source_tokens;
    for (source, cost) in peer.diagnostics.per_source_tokens {
        match per_source.iter_mut().find(|(seen, _)| *seen == source) {
            Some((_, running)) => *running += cost,
            None => per_source.push((source, cost)),
        }
    }

    ContextBundle {
        entries,
        diagnostics: AssembleDiagnostics {
            per_source_tokens: per_source,
            total_tokens: local
                .diagnostics
                .total_tokens
                .saturating_add(peer.diagnostics.total_tokens),
            truncated: local.diagnostics.truncated || peer.diagnostics.truncated,
            deduped_count: local
                .diagnostics
                .deduped_count
                .saturating_add(peer.diagnostics.deduped_count),
            // Message-tier windowing fields belong to whichever member produced
            // them; only the local half ever can.
            active_group_id: local.diagnostics.active_group_id,
            group_count: local.diagnostics.group_count,
            tokens_saved_vs_passthrough: local.diagnostics.tokens_saved_vs_passthrough,
            tokens_saved_pct: local.diagnostics.tokens_saved_pct,
            ..AssembleDiagnostics::default()
        },
    }
}

/// Token cost of one entry as the composite counts it.
#[allow(dead_code)]
fn entry_cost(entry: &ProvenancedEntry) -> usize {
    estimate_tokens(&entry.render_line())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::{PeerId, Relevance, RetrievalMethod};

    struct Fixed(ContextBundle);

    #[async_trait]
    impl ContextPort for Fixed {
        async fn assemble(
            &self,
            _query: &str,
            _budget: ContextBudget,
        ) -> Result<ContextBundle, ContextError> {
            Ok(self.0.clone())
        }
    }

    fn entry(source: ContextSource, content: &str) -> ProvenancedEntry {
        ProvenancedEntry {
            source,
            content: Arc::from(content),
            timestamp: 0,
            retrieval_method: RetrievalMethod::Structural,
            relevance: Relevance::Unscored,
        }
    }

    fn bundle(entries: Vec<ProvenancedEntry>) -> ContextBundle {
        let total = entries.iter().map(|e| entry_cost(e)).sum();
        ContextBundle {
            diagnostics: AssembleDiagnostics {
                per_source_tokens: entries
                    .iter()
                    .map(|e| (e.source.clone(), entry_cost(e)))
                    .collect(),
                total_tokens: total,
                ..AssembleDiagnostics::default()
            },
            entries,
        }
    }

    #[tokio::test]
    async fn compositing_keeps_the_local_row_a_peer_contradicts() {
        let peer_id = PeerId::from_public_key(&[5u8; 32]).expect("valid key");
        let local = Arc::new(Fixed(bundle(vec![entry(
            ContextSource::MemoryMd,
            "the event bus is NATS",
        )]))) as Arc<dyn ContextPort>;
        let peer = Arc::new(Fixed(bundle(vec![entry(
            ContextSource::Peer(peer_id.clone()),
            "the event bus is Kafka",
        )]))) as Arc<dyn ContextPort>;

        let merged = CompositeContextAdapter::new(local, peer)
            .assemble("bus", ContextBudget::new(4096))
            .await
            .expect("composites");

        // ⚑ Both survive, each attributed. Selecting the peer provider must not
        // delete the local one (the P1 finding), and a peer assertion must not
        // evict a colliding local memory row (the dedup-class ruling).
        assert_eq!(merged.entries.len(), 2);
        assert_eq!(merged.entries[0].source, ContextSource::MemoryMd);
        assert_eq!(merged.entries[1].source, ContextSource::Peer(peer_id));
        assert!(merged.has_peer_origin());
        let prefix = merged.to_prefix().expect("both are injectable");
        assert!(prefix.contains("[memory] the event bus is NATS"));
        assert!(prefix.contains("the event bus is Kafka"));
    }

    #[tokio::test]
    async fn a_composite_with_no_peer_entries_is_not_tainted() {
        // Positive/negative control for the taint bridge: the same code path,
        // one peer entry apart.
        let local = Arc::new(Fixed(bundle(vec![entry(
            ContextSource::MemoryMd,
            "local only",
        )]))) as Arc<dyn ContextPort>;
        let peer = Arc::new(Fixed(ContextBundle::empty())) as Arc<dyn ContextPort>;
        let merged = CompositeContextAdapter::new(local, peer)
            .assemble("x", ContextBudget::new(4096))
            .await
            .expect("composites");
        assert!(!merged.has_peer_origin());
    }
}
