//! AgentId-keyed `ResultStore` side-table (DD4 — the highest R2-risk decision).
//!
//! Result bodies live HERE, in a side-table addressed by [`AgentId`], NOT on
//! `NodeHandle` (a payload-free comms handle). The type-wall has two halves:
//!
//! 1. A **crate-private `NodeResult` ctor** ([`NodeResult::ingest`]) — only the
//!    executor mints one, at a child's terminal state, after schema-validation-
//!    at-ingest ("parse, don't validate"). External code cannot construct one.
//! 2. An **opaque [`Window<Handle>`](super::window::Window)** with no body
//!    accessor in prompt-build scope — full payloads never enter the
//!    coordinator's attention window (AC5, the ≈15× token antidote).
//!
//! The public projection is [`SpokeResult`] (Result-shaped — synthesis sees
//! failed/cancelled children too). Drill is lazy fetch-on-open via
//! [`ResultStore::get`], addressed by `AgentId`.

use std::collections::HashMap;

use super::result_contract::validate_yield;
use crate::domain::models::agent_id::AgentId;
use crate::domain::models::orchestration::SpokeResult;

/// The full per-child terminal result. **Crate-private ctor** ([`Self::ingest`]):
/// only the executor mints one at a child's terminal state, after schema
/// validation at ingest. The public projection is [`SpokeResult`].
///
/// `body` is the full payload — it lives in this side-table, never on a
/// `NodeHandle`, never in the prompt window. That is the AC5 type-wall.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct NodeResult {
    pub(crate) agent_id: AgentId,
    pub(crate) label: String,
    pub(crate) outcome: SpokeResult,
    /// Full payload body (meaningful for `Completed`; empty otherwise). The
    /// prompt window sees only compact metadata; drill fetches this on demand.
    pub(crate) body: String,
}
impl NodeResult {
    /// Executor-only ctor. **Schema validation at ingest** (parse, don't
    /// validate — AC6): for a `Completed` outcome the `raw_body` is parsed as a
    /// [`SpokeYield`](super::result_contract::SpokeYield) and the validated
    /// `detail` becomes the drill body; a non-parseable "completed" yield is
    /// downgraded to `Empty` (the honest signal — never a false Completed).
    /// For `Cancelled`, the body is the salvaged partial. Minted exactly once
    /// per spoke at its terminal state (G7).
    pub(crate) fn ingest(
        agent_id: AgentId,
        label: String,
        outcome: SpokeResult,
        raw_body: String,
    ) -> Self {
        let (outcome, body) = match outcome {
            SpokeResult::Completed { .. } => {
                // Parse don't validate: a Completed spoke must yield a schema-
                // valid body. Non-parseable → honest Empty (no false signal).
                match validate_yield(&raw_body) {
                    Ok(y) => (SpokeResult::Completed { summary: y.summary }, y.detail),
                    Err(_) => (SpokeResult::Empty, raw_body),
                }
            }
            // Cancelled/Failed/Empty: keep the raw body for drill (salvage).
            other => (other, raw_body),
        };
        Self {
            agent_id,
            label,
            outcome,
            body,
        }
    }

    /// Compact summary for the window's salience noun-phrase + synthesis
    /// citation. Never the full body.
    pub(crate) fn compact_summary(&self) -> &str {
        match &self.outcome {
            SpokeResult::Completed { summary } => summary.as_str(),
            SpokeResult::Failed { reason } => reason.as_str(),
            SpokeResult::Cancelled => "(cancelled)",
            SpokeResult::Empty => "(empty)",
        }
    }

    /// Project to the public [`SpokeResult`] (Result-shaped).
    pub(crate) fn to_spoke_result(&self) -> SpokeResult {
        self.outcome.clone()
    }
}

/// AgentId-keyed side-table of terminal results. Exactly one entry per
/// dispatched spoke at collection time (G7: none missing, none duplicated).
///
/// Interior mutability is intentionally AVOIDED at the struct level: the
/// executor owns a `ResultStore` by value and inserts results as children
/// terminate. This keeps the store off the sync-lock ratchet (DD5 — zero new
/// `std::sync` locks) and off `tokio::sync` (it is never shared across tasks:
/// the single owning task collects all results).
#[derive(Default, Clone)]
pub(crate) struct ResultStore {
    entries: HashMap<AgentId, NodeResult>,
    /// Dispatch order. `insert` appends in **completion order** (results
    /// arrive as children terminate, non-deterministically under concurrency);
    /// the executor calls [`reorder`](Self::reorder) once all spokes are
    /// collected to finalize `order` to true **dispatch order**. This keeps
    /// `ordered()` and the positional `replace_at_slot(slot, …)` correct
    /// regardless of completion order (PATCH-1: the prior append-only `order`
    /// made `replace_at_slot`'s positional assert fire on out-of-order
    /// completion). R2's readiness predicate composes over this; AC2 is
    /// AgentId-keyed, never a flattened blob.
    order: Vec<AgentId>,
}

impl ResultStore {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Insert a terminal result. First-write-wins latch: a spoke reaches
    /// exactly one terminal state (G7), so a duplicate insert for the same
    /// `agent_id` is ignored (the first terminal is authoritative).
    pub(crate) fn insert(&mut self, result: NodeResult) {
        if self.entries.contains_key(&result.agent_id) {
            return;
        }
        self.order.push(result.agent_id.clone());
        self.entries.insert(result.agent_id.clone(), result);
    }

    /// Finalize `order` to true dispatch order. Called by the executor ONCE,
    /// after every spoke has terminated and been inserted (in completion
    /// order). `dispatch_order` is the AgentIds in dispatch-index sequence.
    /// Idempotent + safe: every id must already be present (inserted); a
    /// missing/extra id panics (a wave collects exactly N — G7). After this,
    /// the positional `replace_at_slot(slot, …)` contract holds for rerun
    /// regardless of how spokes completed.
    pub(crate) fn reorder(&mut self, dispatch_order: Vec<AgentId>) {
        debug_assert!(
            dispatch_order.len() == self.entries.len(),
            "reorder: dispatch_order len {} != entries {} (wave must collect exactly N)",
            dispatch_order.len(),
            self.entries.len()
        );
        for id in &dispatch_order {
            assert!(
                self.entries.contains_key(id),
                "reorder: dispatch id {id:?} not present in store (insert missing)"
            );
        }
        self.order = dispatch_order;
    }

    /// Lazy fetch-on-open (drill). Returns the full [`NodeResult`] body for a
    /// handle addressed by `AgentId`. This is the ONLY body accessor — the
    /// [`Window`](super::window::Window) has none (AC5).
    pub(crate) fn get(&self, agent_id: &AgentId) -> Option<&NodeResult> {
        self.entries.get(agent_id)
    }

    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// All results in dispatch order (AgentId-keyed, never flattened — AC2).
    pub(crate) fn ordered(&self) -> Vec<&NodeResult> {
        self.order
            .iter()
            .filter_map(|id| self.entries.get(id))
            .collect()
    }

    /// Positional slot replacement (DD-B5): replaces the AgentId at `slot`
    /// with `new_id` and its result with `new_node`. NOT remove+append — the
    /// slot index is stable (rerun replaces in-place, preserving dispatch
    /// order). Asserts `slot < order.len()` and `order[slot] == *old_id`.
    pub(crate) fn replace_at_slot(
        &mut self,
        slot: usize,
        old_id: &AgentId,
        new_id: AgentId,
        new_node: NodeResult,
    ) {
        assert!(
            slot < self.order.len(),
            "replace_at_slot: slot {slot} out of bounds (order len {})",
            self.order.len()
        );
        assert_eq!(
            &self.order[slot], old_id,
            "replace_at_slot: slot {slot} does not hold {old_id:?}"
        );
        self.entries.remove(old_id);
        self.entries.insert(new_id.clone(), new_node);
        self.order[slot] = new_id;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a NodeResult. For `Completed` the body is a schema-valid JSON
    /// yield (ingest parses it → summary/detail); for other outcomes the raw
    /// body is kept verbatim for drill.
    fn nr(label: &str, outcome: SpokeResult) -> NodeResult {
        let body = match &outcome {
            SpokeResult::Completed { summary } => {
                format!(r#"{{"summary":{summary:?},"detail":"full body of {label}"}}"#)
            }
            _ => format!("full body of {label}"),
        };
        NodeResult::ingest(AgentId::from_validated(label), label.into(), outcome, body)
    }

    #[test]
    fn insert_and_get_preserves_body_off_public_projection() {
        let mut store = ResultStore::new();
        store.insert(nr(
            "a",
            SpokeResult::Completed {
                summary: "sa".into(),
            },
        ));
        let got = store.get(&AgentId::from_validated("a")).unwrap();
        // ingest parsed the JSON yield → detail is the side-table body.
        assert_eq!(got.body, "full body of a"); // body lives in the side-table
        assert_eq!(got.compact_summary(), "sa"); // window sees only compact
        assert!(matches!(
            got.to_spoke_result(),
            SpokeResult::Completed { .. }
        ));
    }

    #[test]
    fn ordered_preserves_dispatch_order_not_map_order() {
        let mut store = ResultStore::new();
        store.insert(nr("zeta", SpokeResult::Cancelled));
        store.insert(nr("alpha", SpokeResult::Failed { reason: "x".into() }));
        let ordered: Vec<&str> = store.ordered().iter().map(|r| r.label.as_str()).collect();
        assert_eq!(ordered, vec!["zeta", "alpha"]); // dispatch order, not lexical
    }

    #[test]
    fn first_insert_wins_latch() {
        let mut store = ResultStore::new();
        store.insert(nr(
            "a",
            SpokeResult::Completed {
                summary: "first".into(),
            },
        ));
        store.insert(nr("a", SpokeResult::Cancelled));
        let got = store.get(&AgentId::from_validated("a")).unwrap();
        assert!(matches!(got.outcome, SpokeResult::Completed { .. }));
    }

    /// PATCH-1 (review) regression: `insert` runs in completion order, but
    /// `reorder` finalizes `order` to dispatch order so the positional
    /// `replace_at_slot(slot, …)` contract holds regardless of completion
    /// order. Without `reorder`, the assert panics on out-of-order completion
    /// (the production norm under concurrency). This is the keystone the
    /// single-threaded `FakeRunner` rerun test could NOT catch.
    #[test]
    fn reorder_then_replace_at_slot_survives_out_of_order_completion() {
        let mut store = ResultStore::new();
        // Insert in COMPLETION order: slot 2, then 0, then 1 (shuffled — the
        // normal case for concurrent dispatch with variable latency).
        store.insert(nr("c", SpokeResult::Cancelled));
        store.insert(nr(
            "a",
            SpokeResult::Completed {
                summary: "sa".into(),
            },
        ));
        store.insert(nr("b", SpokeResult::Failed { reason: "x".into() }));
        // Pre-reorder: `order` is completion order [c, a, b].
        assert_eq!(
            store
                .ordered()
                .iter()
                .map(|r| r.label.as_str())
                .collect::<Vec<_>>(),
            vec!["c", "a", "b"],
            "pre-reorder order is completion order"
        );
        // Finalize to DISPATCH order [a, b, c] (idx 0, 1, 2).
        store.reorder(vec![
            AgentId::from_validated("a"),
            AgentId::from_validated("b"),
            AgentId::from_validated("c"),
        ]);
        assert_eq!(
            store
                .ordered()
                .iter()
                .map(|r| r.label.as_str())
                .collect::<Vec<_>>(),
            vec!["a", "b", "c"],
            "post-reorder order is dispatch order"
        );
        // Positional replace of slot 1 (dispatch idx 1 = "b") must NOT panic
        // and must land at slot 1. Without reorder, order[1] == "a" != "b" →
        // the assert_eq! panics (the original PATCH-1 defect).
        store.replace_at_slot(
            1,
            &AgentId::from_validated("b"),
            AgentId::from_validated("b-rerun"),
            nr(
                "b-rerun",
                SpokeResult::Completed {
                    summary: "sbr".into(),
                },
            ),
        );
        assert_eq!(
            store.ordered()[1].label,
            "b-rerun",
            "slot 1 replaced positionally"
        );
        assert_eq!(store.ordered()[0].label, "a", "slot 0 untouched");
        assert_eq!(store.ordered()[2].label, "c", "slot 2 untouched");
        assert!(
            store.get(&AgentId::from_validated("b")).is_none(),
            "old id removed"
        );
        assert!(
            store.get(&AgentId::from_validated("b-rerun")).is_some(),
            "new id present"
        );
    }
}
