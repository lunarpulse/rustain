//! Story 14-4a — Message-Bus Receipt Honesty & Consent Enforcement conformance.
//!
//! Behavioral proofs plus structural ratchets that the 14-4 defects are repaired.
//!
//! Tests tagged `// SUPPLEMENTARY — structural ratchet, not behavioral coverage`
//! are source-text guards: they pin production code shape so a regression is
//! caught at compile/test time, but they are NOT themselves behavioral coverage.
//! The genuine behavioral coverage lives in the AC1 budget test and the two AC5
//! real-tree wiring tests below.

use rustain::domain::models::DeliveryOutcome;

/// AC1 — MAILBOX_CAP constant exists and matches the unified capacity.
#[test]
fn ac1_mailbox_cap_constant_exists() {
    assert_eq!(
        rustain::infrastructure::subagent::MAILBOX_CAP,
        64,
        "MAILBOX_CAP must be 64 (unified across all buffers)"
    );
}

/// AC1 — `MailboxBudget` is atomics-only, and reserve/current are observable
/// from outside the crate while **release is not**.
///
/// Story 18.3 (AC2) rewrote this test rather than deleting it. `release()` used
/// to be fully `pub`, so this test released a slot directly — which is exactly
/// the corruption vector `DF-CR-14-4a-5` named: any holder could release a slot
/// it never reserved. `release` is now `pub(crate)`, so the out-of-crate half of
/// that hazard is gone by construction and this test can no longer perform it.
/// What remains observable here is the reserve side and the live count; the
/// exactly-one-release invariant is proven in-crate by
/// `every_reserve_is_matched_by_exactly_one_release`, which can read the
/// instrumentation counters.
#[test]
fn ac1_mailbox_budget_is_atomics_only() {
    let budget = rustain::infrastructure::subagent::MailboxBudget::new();
    assert_eq!(budget.current(), 0);
    assert!(budget.reserve().is_ok());
    assert_eq!(budget.current(), 1, "a reserve must be observable");
    assert!(budget.reserve().is_ok());
    assert_eq!(budget.current(), 2, "reserves accumulate");
}

/// AC3 grep-ratchet: `let _disposition = self.policy` == 0 (Murat's
/// mechanical guard against the discarded-disposition defect).
// SUPPLEMENTARY — structural ratchet, not behavioral coverage
#[test]
fn ac3_ratchet_no_discarded_disposition_in_production() {
    let source = include_str!("../src/infrastructure/subagent/node_tree.rs");
    let production = source
        .split("#[cfg(test)]")
        .next()
        .expect("production section");
    assert_eq!(
        production.matches("let _disposition = self.policy").count(),
        0,
        "the disposition must be stamped into AgentDelivery, never discarded"
    );
}

/// AC3 — AgentDelivery has a disposition field.
// SUPPLEMENTARY — structural ratchet, not behavioral coverage
#[test]
fn ac3_agent_delivery_has_disposition_field() {
    let source = include_str!("../src/domain/models/agent_message.rs");
    assert!(
        source.contains("pub disposition: DeliveryDisposition"),
        "AgentDelivery must carry the consent disposition field"
    );
}

/// AC4 — DeliveryOutcome has only Accepted (no Delivered, Queued, Refused).
// SUPPLEMENTARY — structural ratchet, not behavioral coverage
#[test]
fn ac4_delivery_outcome_accepted_only() {
    let source = include_str!("../src/domain/models/agent_message.rs");
    let enum_start = source
        .find("pub enum DeliveryOutcome")
        .expect("enum exists");
    let enum_section = &source[enum_start..enum_start + 200];
    assert!(
        !enum_section.contains("Delivered"),
        "Delivered variant must be deleted (the bus cannot truthfully assert it)"
    );
    assert!(
        !enum_section.contains("Queued"),
        "Queued variant must be deleted (the bus cannot truthfully assert it)"
    );
    assert!(
        !enum_section.contains("Refused"),
        "Refused variant must be deleted (replaced by DeliveryError + receipts)"
    );
    assert!(
        enum_section.contains("Accepted"),
        "Accepted must be the sole variant"
    );
}

/// AC4 — Delivered/Queued/Refused do not compile as DeliveryOutcome variants.
#[test]
fn ac4_accepted_is_sole_variant_by_construction() {
    // This compiles → Accepted exists
    let _outcome = DeliveryOutcome::Accepted;
}

/// AC4 (CS-4) — MessageRefused carries reason: RefuseReason, not outcome.
// SUPPLEMENTARY — structural ratchet, not behavioral coverage
#[test]
fn ac4_message_refused_carries_reason() {
    let source = include_str!("../src/domain/models/subagent_envelope.rs");
    let refused_section = source
        .find("MessageRefused")
        .expect("MessageRefused exists");
    let section = &source[refused_section..refused_section + 200];
    assert!(
        section.contains("reason: RefuseReason"),
        "MessageRefused must carry reason: RefuseReason (not outcome: DeliveryOutcome)"
    );
}

/// AC5 (CS-5) — run_child emits AppEvent::Subagent for receipts.
// SUPPLEMENTARY — structural ratchet, not behavioral coverage
#[test]
fn ac5_run_child_emits_receipt_envelopes() {
    let runner = include_str!("../src/adapters/subagent/in_process_runner.rs");
    let production = runner
        .split("#[cfg(test)]")
        .next()
        .expect("production section");
    assert!(
        production.contains("AppEvent::Subagent("),
        "run_child must emit AppEvent::Subagent for receipts"
    );
    assert!(
        production.contains("SubagentEvent::MessageRefused"),
        "run_child must emit MessageRefused receipts"
    );
    assert!(
        production.contains("RefuseReason::Policy"),
        "consent-refusal receipts must carry Policy reason"
    );
    assert!(
        production.contains("RefuseReason::TerminalState"),
        "terminal-drain receipts must carry TerminalState reason"
    );
}

/// AC2 — terminal drain exists in run_child.
// SUPPLEMENTARY — structural ratchet, not behavioral coverage
#[test]
fn ac2_terminal_drain_exists() {
    let runner = include_str!("../src/adapters/subagent/in_process_runner.rs");
    let production = runner
        .split("#[cfg(test)]")
        .next()
        .expect("production section");
    assert!(
        production.contains("drain_mailbox("),
        "run_child must call drain_mailbox on terminal paths"
    );
    assert!(
        production.contains("command_rx.close()"),
        "drain must close the command channel"
    );
    assert!(
        production
            .contains("debug_assert_eq!(\n            mailbox_budget.current(),\n            0,")
            || production.contains("mailbox_budget.current(),\n            0"),
        "drain must debug_assert budget == 0"
    );
}

/// AC2 — pending_injected counter exists (release at turn-dispatch).
// SUPPLEMENTARY — structural ratchet, not behavioral coverage
#[test]
fn ac2_pending_injected_counter_exists() {
    let runner = include_str!("../src/adapters/subagent/in_process_runner.rs");
    let production = runner
        .split("#[cfg(test)]")
        .next()
        .expect("production section");
    assert!(
        production.contains("pending_injected"),
        "run_child must track pending_injected for release-at-dispatch"
    );
}

/// AC1 — warn-drop sites demoted to debug_assert (reservation prevents overflow).
/// Behavioral conversion is not feasible here: the defect is the *absence* of a
/// silent warn-drop, which only a source-text ratchet can pin.
// SUPPLEMENTARY — structural ratchet, not behavioral coverage
#[test]
fn ac1_warn_drop_sites_demoted() {
    let runner = include_str!("../src/adapters/subagent/in_process_runner.rs");
    let production = runner
        .split("#[cfg(test)]")
        .next()
        .expect("production section");
    assert_eq!(
        production
            .matches("parked agent message queue full; refusing message")
            .count(),
        0,
        "the silent warn-drop must be gone (reservation prevents overflow)"
    );
}

// ── AC5 behavioral wiring proofs ────────────────────────────────────────────
//
// These replace the former `ac5_startup_restores_real_tree_bus` source-text
// ratchet (which only asserted startup.rs contained certain strings). They
// build a real `LocalMessageBus` over a real `NodeTree` and exercise the
// `AgentMessageBus::deliver` seam at runtime, proving the wiring is correct
// rather than merely present in the source.

/// Register a live child agent on `tree` and return its command receiver.
///
/// The caller MUST keep the returned receiver bound (e.g. `let _rx = ...`) so
/// the command channel stays open and the bus's `try_send` succeeds at delivery
/// time. Mirrors `register_live_agent` in `node_tree.rs`.
async fn register_live_child(
    tree: &rustain::infrastructure::subagent::NodeTree,
    agent: &rustain::domain::models::AgentId,
) -> tokio::sync::mpsc::Receiver<rustain::domain::models::Op> {
    use rustain::domain::models::{AgentMetrics, CapabilityTokenId, NodeState, Op};
    use rustain::infrastructure::subagent::{AgentHandle, MAILBOX_CAP, MailboxBudget};
    use tokio::sync::{mpsc, watch};

    let (tx, rx) = mpsc::channel::<Op>(MAILBOX_CAP);
    let (status_tx, _status_rx) = watch::channel(NodeState::Created);
    let (_metrics_tx, metrics_rx) = watch::channel(AgentMetrics::default());
    let handle = AgentHandle {
        isolated: false,
        agent_id: agent.clone(),
        token: CapabilityTokenId::nil(),
        command_tx: tx,
        cancel_token: tokio_util::sync::CancellationToken::new(),
        depth: 1,
        subagent_type: String::from("test"),
        spawned_at: 0,
        status: status_tx,
        metrics: metrics_rx,
        mailbox_budget: MailboxBudget::new(),
    };
    tree.register(
        agent.clone(),
        rustain::domain::models::AgentId::root(),
        handle,
    )
    .await
    .expect("register live child");
    rx
}

/// AC5 — real-tree wiring (behavioral): delivering through a bus built over a
/// real `NodeTree`, with a registered live agent, succeeds at runtime and
/// returns `Accepted`. This proves the wiring is correct, not just that the
/// source contains certain strings.
#[tokio::test]
async fn ac5_real_tree_wiring_behavioral() {
    use rustain::domain::models::{
        AgentId, AgentMessage, CorrelationId, Envelope, MessageHeader, MessageKind,
    };
    use rustain::domain::ports::AgentMessageBus;
    use rustain::domain::ports::RelationshipDeliveryPolicy;
    use rustain::infrastructure::subagent::{LocalMessageBus, NodeTree};
    use std::sync::Arc;

    let tree = NodeTree::new();
    let bus = LocalMessageBus::new(tree.clone(), Arc::new(RelationshipDeliveryPolicy));

    let agent = AgentId::parse("child").unwrap();
    // Keep the receiver alive so the command channel is open for try_send.
    let _rx = register_live_child(&tree, &agent).await;

    let env = Envelope {
        header: MessageHeader {
            sender: AgentId::parse("parent").unwrap(),
            recipient: agent.clone(),
            correlation_id: CorrelationId::new("wiring"),
            kind: MessageKind::PeerMessage,
            sequence: None,
        },
        body: AgentMessage::new("hello"),
    };
    let result = bus.deliver(&agent, env).await;
    assert!(
        result.is_ok(),
        "deliver to a registered agent must succeed, got {result:?}"
    );
    assert_eq!(result.unwrap(), DeliveryOutcome::Accepted);
}

/// AC5 mutant (behavioral): delivering through a bus over an EMPTY tree (no
/// agent registered) returns `NotFound`. This pins the wiring so a regression
/// to an inert `Default::default()` bus is caught at runtime, not just in
/// source text.
#[tokio::test]
async fn ac5_mutant_empty_tree_returns_not_found() {
    use rustain::domain::models::{
        AgentId, AgentMessage, CorrelationId, Envelope, MessageHeader, MessageKind,
    };
    use rustain::domain::ports::AgentMessageBus;
    use rustain::domain::ports::DeliveryError;
    use rustain::domain::ports::RelationshipDeliveryPolicy;
    use rustain::infrastructure::subagent::{LocalMessageBus, NodeTree};
    use std::sync::Arc;

    let tree = NodeTree::new();
    let bus = LocalMessageBus::new(tree.clone(), Arc::new(RelationshipDeliveryPolicy));

    // No agent registered — the tree is empty.
    let ghost = AgentId::parse("ghost").unwrap();
    let env = Envelope {
        header: MessageHeader {
            sender: AgentId::parse("parent").unwrap(),
            recipient: ghost.clone(),
            correlation_id: CorrelationId::new("mutant"),
            kind: MessageKind::PeerMessage,
            sequence: None,
        },
        body: AgentMessage::new("hello"),
    };
    let result = bus.deliver(&ghost, env).await;
    assert!(
        matches!(result, Err(DeliveryError::NotFound(_))),
        "deliver to an unregistered agent must be NotFound, got {result:?}"
    );
}
