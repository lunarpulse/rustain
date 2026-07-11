//! Conformance tests for Story 14.6 — Multi-Agent Security Hardening.
//!
//! Source of truth:
//! - `_bmad-output/implementation-artifacts/14-6-multi-agent-security-hardening.md`
//! - ADR-14-6-01 (fingerprint canonical-bytes contract)
//!
//! ## Keystone tests
//!
//! AC1 — Inline gate-await: behavioral exactly-once counter + sibling independence
//! AC2 — Provenance-taint: narrow policy, wired and load-bearing
//! AC3 — Fingerprint match/mismatch keystone
//! AC4 — Unforgeable `Self` tier
//! AC5 — Synchronous revocation ordering
//! AC6 — Testability invariants (ApprovalSource #[non_exhaustive], zero .launch(, etc.)

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use rustain::domain::errors::{PermissionError, ToolError};
use rustain::domain::models::invocation_fingerprint::InvocationFingerprint;
use rustain::domain::models::subagent_view::{OwnershipKind, WireOwnershipKind};
use rustain::domain::models::tool_call::{ApprovalSource, ToolCall, ToolCallRequest};
use rustain::domain::models::{
    AgentId, ApprovalOutcome, FileOperation, PathAccessType, PermissionMode, ToolDefinition,
    ToolResult,
};
use rustain::domain::ports::{SecurityPort, ToolSetPort};
use rustain::domain::services::approval_runtime::ApprovalRuntime;
use rustain::domain::services::tool_scheduler::ToolScheduler;
use tokio_util::sync::CancellationToken;

// ═══════════════════════════════════════════════════════════════════════
// AC3 — InvocationFingerprint replay guard
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn ac3_fingerprint_deterministic() {
    let scope = AgentId::parse("agent-a").unwrap();
    let input = serde_json::json!({"command": "ls -la", "path": "/home"});
    let fp1 = InvocationFingerprint::of("Bash", &input, &scope).unwrap();
    let fp2 = InvocationFingerprint::of("Bash", &input, &scope).unwrap();
    assert_eq!(fp1, fp2, "same input must produce same fingerprint");
}

#[test]
fn ac3_fingerprint_scope_binding() {
    let input = serde_json::json!({"command": "rm -rf /"});
    let fp_a =
        InvocationFingerprint::of("Bash", &input, &AgentId::parse("node-a").unwrap()).unwrap();
    let fp_b =
        InvocationFingerprint::of("Bash", &input, &AgentId::parse("node-b").unwrap()).unwrap();
    assert_ne!(
        fp_a, fp_b,
        "approval for node A must not replay on node B (DD1, Murat)"
    );
}

/// Positive control: matching resume succeeds — driven through the REAL
/// `ApprovalRuntime::request`/`resolve` (not a bare double-call to
/// `InvocationFingerprint::of` disconnected from production state). The
/// `resolved.fp` compared here is the SERVER-SIDE value threaded back
/// through the real oneshot from a real `PendingRecord` — this kills the
/// `{ let _ = stored; true }` comparator mutant on the "stored" side (the
/// earlier tautology-only version never touched `ApprovalRuntime` at all).
///
/// Mutant (recompute side): the resume-time recompute is fed a mutated
/// input constructed independently of the original request (fresh
/// `serde_json::json!` literal, not cloned/derived from it) — simulating an
/// attacker's tampered resume — and MUST differ from the real stored fp.
///
/// Note (DD1 design intent, stated not assumed): in R1 a true end-to-end
/// CROSS-REQUEST mismatch is structurally unreachable through the
/// scheduler's public surface, because `run_one` always recomputes its
/// local fp from the SAME owned `ToolCallRequest` it used to call
/// `request()` — there is no code path that resumes one request's approval
/// against a DIFFERENT request's input. The re-match is defense-in-depth
/// for future wire-based resume (R2) and for implementation bugs, proven
/// here by tying the "stored" half to the real runtime and the "recomputed"
/// half to the exact comparator `run_one` uses.
#[tokio::test]
async fn ac3_fingerprint_match_mismatch_keystone() {
    let scope = AgentId::parse("agent-x").unwrap();
    let original_input = serde_json::json!({"command": "echo hello"});

    let approval_runtime = ApprovalRuntime::new(
        16,
        Arc::new(rustain::adapters::noop::NoOpApprovalPersistence),
    );
    let source = ApprovalSource::ForegroundTurn {
        conversation_id: scope.as_str().to_string(),
    };

    // Real request() — computes + stores the fp server-side in PendingRecord.
    let (id, rx) = approval_runtime
        .request(
            source.clone(),
            "Bash".to_string(),
            original_input.clone(),
            rustain::domain::models::ToolRisk::Elevated,
            None,
            None,
        )
        .await;
    let id = id.expect("Elevated risk with no session pre-approval must be a slow-path request");

    // Real resolve() — delivers the REAL server-side ResolvedApproval over the oneshot.
    approval_runtime.resolve(&id, ApprovalOutcome::Once).await;
    let resolved = rx.await.expect("resolve must deliver a ResolvedApproval");

    // Positive control: the SAME comparator run_one uses, fed the real stored fp
    // and a recompute from the ORIGINAL (unmutated) input — must match.
    let local_fp_original = InvocationFingerprint::of("Bash", &original_input, &scope).unwrap();
    assert_eq!(
        local_fp_original, resolved.fp,
        "matching resume must succeed against the REAL server-stored fp"
    );

    // Mutant: an independently-constructed mutated input (simulating a
    // tampered resume) must NOT match the real stored fp.
    let mutated_input = serde_json::json!({"command": "echo EVIL"});
    let local_fp_mutated =
        InvocationFingerprint::of("Bash", &mutated_input, &AgentId::parse("agent-x").unwrap())
            .unwrap();
    assert_ne!(
        local_fp_mutated, resolved.fp,
        "mutated input recomputed at resume-time MUST NOT match the real stored fp \
         — comparator mutant `{{ let _ = stored; true }}` would falsely pass this"
    );
}

/// 14.2 boundary-collision pair: length-prefix discipline
#[test]
fn ac3_fingerprint_length_prefix_collision_pair() {
    let scope = AgentId::parse("s").unwrap();
    // ("ab", "c") ≠ ("a", "bc") — length-prefix discipline from CapabilityToken
    let fp1 = InvocationFingerprint::of("ab", &serde_json::json!("c"), &scope).unwrap();
    let fp2 = InvocationFingerprint::of("a", &serde_json::json!("bc"), &scope).unwrap();
    assert_ne!(
        fp1, fp2,
        "length-prefix boundary collision must be rejected"
    );
}

/// Whitespace variation: "rm -rf /" vs "rm  -rf /"
#[test]
fn ac3_fingerprint_whitespace_variation() {
    let scope = AgentId::parse("s").unwrap();
    let fp1 =
        InvocationFingerprint::of("Bash", &serde_json::json!({"cmd": "rm -rf /"}), &scope).unwrap();
    let fp2 = InvocationFingerprint::of("Bash", &serde_json::json!({"cmd": "rm  -rf /"}), &scope)
        .unwrap();
    assert_ne!(
        fp1, fp2,
        "whitespace variations must produce different fingerprints"
    );
}

/// Non-goal (first-class): scope-broad Always* rule matching many invocations
/// is the user's deliberate authorization, not a fingerprint bypass.
#[test]
fn ac3_always_rule_is_not_fingerprint_bypass() {
    // This is a documentation test — the fingerprint asserts identity-of-invocation,
    // never breadth-of-policy.
    let scope = AgentId::parse("s").unwrap();
    let fp1 = InvocationFingerprint::of("Bash", &serde_json::json!({"cmd": "ls"}), &scope).unwrap();
    let fp2 = InvocationFingerprint::of(
        "Bash",
        &serde_json::json!({"cmd": "cat /etc/passwd"}),
        &scope,
    )
    .unwrap();
    // Different invocations → different fingerprints, even if both covered by Always*
    assert_ne!(fp1, fp2);
}

// ═══════════════════════════════════════════════════════════════════════
// AC4 — Unforgeable Self tier
// ═══════════════════════════════════════════════════════════════════════

/// Pre-fix-decode characterization test (DD2, Murat): the CURRENT `OwnershipKind`
/// no longer derives `Deserialize`, so `"self_"` is unrepresentable on the wire.
#[test]
fn ac4_wire_ownership_kind_has_no_self_variant() {
    // WireOwnershipKind has only Owned and Peer — `"self_"` cannot deserialize.
    let result = serde_json::from_str::<WireOwnershipKind>(r#""self_""#);
    assert!(
        result.is_err(),
        "WireOwnershipKind must NOT accept 'self_' — it has no Self_ variant"
    );
}

/// The sealed root ctor legitimately produces Self_ — but only from within the crate
/// (`self_root()` is `pub(crate)`). From outside (this integration-test binary is a
/// separate crate), `OwnershipKind::Self_` cannot be constructed at all: the tuple
/// variant's `SealedSelf` payload has a private field, so there is no way to produce
/// a `SealedSelf` value here. This test proves the ONLY thing reachable from outside
/// the crate is *matching* on the variant (which `#[non_exhaustive]` + a wildcard arm
/// already requires), never constructing it — see
/// `tests/trybuild/ownership/self_construct_fails.rs` for the compile-time proof.
#[test]
fn ac4_self_variant_matchable_but_not_externally_constructible() {
    // We cannot construct OwnershipKind::Self_ here (SealedSelf's field is
    // private to subagent_view) — only match on it. Build both non-Self_
    // variants and confirm neither matches a Self_ pattern, exercising the
    // match arm's reachability without needing to construct Self_ itself.
    for kind in [OwnershipKind::Owned, OwnershipKind::Peer] {
        let is_self = matches!(kind, OwnershipKind::Self_(_));
        assert!(!is_self, "Owned/Peer must never match the Self_ arm");
    }
}
#[test]
fn ac4_raw_bytes_wire_reject() {
    // Direct deserialization of "self_" into WireOwnershipKind must fail
    let result = serde_json::from_str::<WireOwnershipKind>(r#""self_""#);
    assert!(result.is_err(), "wire type rejects self_");

    // Valid variants still work
    let owned: WireOwnershipKind = serde_json::from_str(r#""owned""#).unwrap();
    assert_eq!(owned, WireOwnershipKind::Owned);
    let peer: WireOwnershipKind = serde_json::from_str(r#""peer""#).unwrap();
    assert_eq!(peer, WireOwnershipKind::Peer);
}

/// Inbound conversion WireOwnershipKind → OwnershipKind is total and never yields Self_.
#[test]
fn ac4_wire_to_domain_never_yields_self() {
    let from_owned: OwnershipKind = WireOwnershipKind::Owned.into();
    let from_peer: OwnershipKind = WireOwnershipKind::Peer.into();
    assert_eq!(from_owned, OwnershipKind::Owned);
    assert_eq!(from_peer, OwnershipKind::Peer);
    // Neither produces Self_ — checked structurally since Self_ is unconstructible here.
    assert!(!matches!(from_owned, OwnershipKind::Self_(_)));
    assert!(!matches!(from_peer, OwnershipKind::Self_(_)));
}

/// Outbound conversion: Self_ serializes as Owned (not transmitted as Self_).
/// The Self_-carrying half of this proof (`self_root().wire() == Owned`) lives
/// as an in-crate unit test in `subagent_view.rs` (`ac4`-tagged) since only
/// in-crate code can mint a `Self_` value; here we confirm the non-Self_ arms
/// of `wire()` are consistent (Owned→Owned, Peer→Peer) as a sanity companion.
#[test]
fn ac4_outbound_non_self_wire_roundtrip() {
    assert_eq!(OwnershipKind::Owned.wire(), WireOwnershipKind::Owned);
    assert_eq!(OwnershipKind::Peer.wire(), WireOwnershipKind::Peer);
}

/// NodeCheckpoint round-trip uses WireOwnershipKind — cannot carry Self_.
#[test]
fn ac4_node_checkpoint_roundtrip_no_self() {
    use rustain::domain::models::agent_node::NodeCheckpoint;
    // Construct a checkpoint with WireOwnershipKind::Peer (the fixture uses Peer)
    // and verify it round-trips through JSON.
    let token_bytes: Vec<u8> = vec![0; 32];
    let json = serde_json::json!({
        "id": "child123",
        "token": token_bytes,
        "parent": "parentabc",
        "ownership": "peer",
        "state": "running",
        "origin": "Subagent",
        "foreground": true,
        "effective_model": "claude-opus-4",
        "tokens_in": 1234,
        "tokens_out": 5678,
        "turns": 7,
        "subagent_type": "code-reviewer",
        "spawned_at": 1700000000000_i64,
        "depth": 2
    });
    let cp: NodeCheckpoint = serde_json::from_value(json.clone()).unwrap();
    assert_eq!(cp.ownership, WireOwnershipKind::Peer);

    // Try to inject "self_" into a checkpoint — it must fail
    let mut evil = json;
    evil["ownership"] = serde_json::json!("self_");
    let result = serde_json::from_value::<NodeCheckpoint>(evil);
    assert!(
        result.is_err(),
        "NodeCheckpoint must reject 'self_' ownership — WireOwnershipKind has no Self_ variant"
    );
}

// Real compile-time guard for this invariant lives in `trybuild_ownership.rs`
// (`ac4_domain_kind_not_deserializable_type_wall`, AI-12.3 post-review closure
// 2026-07-02) — a `compile_fail` proving `serde_json::from_str::<OwnershipKind>`
// does not type-check. The prior version of this test (`let _ =
// OwnershipKind::Owned;`) asserted nothing and was removed as vacuous.

// ═══════════════════════════════════════════════════════════════════════
// AC1 — Inline gate-await: behavioral exactly-once counter + sibling independence
// ═══════════════════════════════════════════════════════════════════════

/// Drive the PRODUCTION `ToolScheduler::schedule` with a gated invocation
/// carrying a non-idempotent, counted pre-gate side effect: gated → suspend
/// (`AwaitingApproval`) → resume → assert the side effect ran **exactly
/// once** (DD3, Murat — behavioral, not structural).
///
/// Mutant this kills: a build that re-invokes `run_one` from the top on
/// resume (instead of suspending in place on the oneshot and resuming past
/// the gate) would drive the counter to 2. This test does NOT reimplement
/// `schedule`/`run_one` — it drives the real production path end-to-end.
#[tokio::test]
#[serial_test::serial(taint_gate_global_state)]
async fn ac1_gated_invocation_exactly_once_counter() {
    let counter = Arc::new(AtomicU32::new(0));
    let security: Arc<dyn SecurityPort> = Arc::new(MockSecurity {
        mode: PermissionMode::Normal,
    });
    let tools: Arc<dyn ToolSetPort> = Arc::new(CountedToolSet {
        counter: counter.clone(),
        delay_ms: 0,
        parallel_safe: true,
    });
    let approval_runtime = ApprovalRuntime::new(
        16,
        Arc::new(rustain::adapters::noop::NoOpApprovalPersistence),
    );
    let scheduler = ToolScheduler::new(security, tools, approval_runtime.clone(), 16);

    // "CountedTool" is Elevated risk (default) + Normal mode → mode_risk_outcome
    // returns None → PermissionDecision::Prompt. The gate fires.
    let batch = vec![ToolCallRequest {
        id: "gated-1".into(),
        tool_name: "CountedTool".into(),
        input: serde_json::json!({}),
    }];
    let source = ApprovalSource::ForegroundTurn {
        conversation_id: "c-ac1-exactly-once".into(),
    };

    // Drive the real scheduler concurrently with the resume so the gate
    // genuinely suspends in place (not a synchronous inline resolve).
    let mut approval_events = approval_runtime.subscribe();
    let sched_task =
        tokio::spawn(scheduler.schedule(source, batch, CancellationToken::new(), None));

    // Wait for the AwaitingApproval request, then approve it exactly once.
    let req_id = loop {
        match approval_events.recv().await.expect("approval event") {
            rustain::domain::services::approval_runtime::ApprovalRuntimeEvent::Requested {
                id,
                ..
            } => break id,
            _ => continue,
        }
    };
    approval_runtime
        .resolve(&req_id, ApprovalOutcome::Once)
        .await;

    let results = sched_task.await.expect("scheduler task panicked");
    assert_eq!(results.len(), 1);
    assert!(
        matches!(results[0], ToolCall::Success { .. }),
        "gated call must execute exactly once after approval, got {:?}",
        results[0]
    );
    assert_eq!(
        counter.load(Ordering::SeqCst),
        1,
        "non-idempotent side effect must run EXACTLY ONCE — a re-invoke-from-top \
         mutant on resume would drive this to 2"
    );
}

/// Positive control (sibling independence, DD3): an **un-gated sibling** in
/// the SAME batch executes exactly once and makes progress WHILE the gated
/// sibling is suspended awaiting approval.
///
/// Mutant this kills: serializing the scheduler (draining the gated call
/// before starting the sibling) would make the sibling's completion
/// observably block on the gate resolving first — this test proves the
/// sibling's side effect lands BEFORE the gate is resolved.
#[tokio::test]
#[serial_test::serial(taint_gate_global_state)]
async fn ac1_sibling_independence_positive_control() {
    let gated_counter = Arc::new(AtomicU32::new(0));
    let sibling_counter = Arc::new(AtomicU32::new(0));
    let security: Arc<dyn SecurityPort> = Arc::new(MockSecurity {
        mode: PermissionMode::Normal,
    });
    let tools: Arc<dyn ToolSetPort> = Arc::new(DualCountedToolSet {
        gated_counter: gated_counter.clone(),
        sibling_counter: sibling_counter.clone(),
    });
    let approval_runtime = ApprovalRuntime::new(
        16,
        Arc::new(rustain::adapters::noop::NoOpApprovalPersistence),
    );
    let scheduler = ToolScheduler::new(security, tools, approval_runtime.clone(), 16);

    // "GatedTool" is Elevated (unknown name → risk_for_builtin default) → Prompt.
    // "SafeSibling" is explicitly listed Safe in risk_for_builtin → auto-allow,
    // no gate — makes progress independently of the gated sibling.
    let batch = vec![
        ToolCallRequest {
            id: "gated".into(),
            tool_name: "GatedTool".into(),
            input: serde_json::json!({}),
        },
        ToolCallRequest {
            id: "sibling".into(),
            tool_name: "remember".into(), // built-in Safe tool name (no gate)
            input: serde_json::json!({}),
        },
    ];
    let source = ApprovalSource::ForegroundTurn {
        conversation_id: "c-ac1-sibling".into(),
    };

    let mut approval_events = approval_runtime.subscribe();
    let sched_task =
        tokio::spawn(scheduler.schedule(source, batch, CancellationToken::new(), None));

    let req_id = loop {
        match approval_events.recv().await.expect("approval event") {
            rustain::domain::services::approval_runtime::ApprovalRuntimeEvent::Requested {
                id,
                ..
            } => break id,
            _ => continue,
        }
    };

    // While the gated sibling is suspended (approval not yet resolved), the
    // un-gated sibling has ALREADY run — the counted side effect is visible.
    // A short bounded wait tolerates scheduling jitter without masking a
    // regression: the un-gated tool has no gate to wait on, so it should
    // complete almost immediately after the batch starts.
    let sibling_progressed = tokio::time::timeout(Duration::from_millis(500), async {
        loop {
            if sibling_counter.load(Ordering::SeqCst) >= 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .is_ok();
    assert!(
        sibling_progressed,
        "un-gated sibling must make progress WHILE the gated sibling is suspended \
         (a serialized/drain-A-before-B mutant would starve it)"
    );
    assert_eq!(
        gated_counter.load(Ordering::SeqCst),
        0,
        "gated sibling must NOT have executed before approval is resolved"
    );

    approval_runtime
        .resolve(&req_id, ApprovalOutcome::Once)
        .await;
    let results = sched_task.await.expect("scheduler task panicked");
    assert_eq!(results.len(), 2);
    assert_eq!(gated_counter.load(Ordering::SeqCst), 1);
    assert_eq!(sibling_counter.load(Ordering::SeqCst), 1);
}

// ═══════════════════════════════════════════════════════════════════════
// AC2 — Provenance-taint: narrow policy, wired + load-bearing (DD4, Murat)
// ═══════════════════════════════════════════════════════════════════════
//
// Requires `--features test-instrumentation` to access `TAINT_GATE_CALLS`
// and `TAINT_GATE_FORCE_DENY`. Three non-vacuous proofs establish that:
// (a) the gate fires exactly once per real scheduler dispatch;
// (b) forcing `Deny` blocks the actual tool side effect; and
// (c) user-originated safe traffic remains silent.

#[cfg(feature = "test-instrumentation")]
#[tokio::test]
#[serial_test::serial(taint_gate_global_state)]
async fn ac2_taint_gate_counter_matches_dispatch_count() {
    use rustain::domain::services::permission_chain::TAINT_GATE_CALLS;
    TAINT_GATE_CALLS.store(0, Ordering::SeqCst);

    let security: Arc<dyn SecurityPort> = Arc::new(MockSecurity {
        mode: PermissionMode::Yolo,
    });
    let tools: Arc<dyn ToolSetPort> = Arc::new(CountedToolSet {
        counter: Arc::new(AtomicU32::new(0)),
        delay_ms: 0,
        parallel_safe: true,
    });
    let approval_runtime = ApprovalRuntime::new(
        16,
        Arc::new(rustain::adapters::noop::NoOpApprovalPersistence),
    );
    let scheduler = ToolScheduler::new(security, tools, approval_runtime, 16);

    // Three real dispatches through the actual ToolScheduler (Yolo mode:
    // no approval gate, isolates the taint-gate call count from AC1's
    // approval-gate mechanics).
    let dispatch_count = 3u64;
    let batch: Vec<ToolCallRequest> = (0..dispatch_count)
        .map(|i| ToolCallRequest {
            id: format!("t{i}"),
            tool_name: "CountedTool".into(),
            input: serde_json::json!({}),
        })
        .collect();
    let source = ApprovalSource::ForegroundTurn {
        conversation_id: "c-ac2-counter".into(),
    };
    let results = scheduler
        .schedule(source, batch, CancellationToken::new(), None)
        .await;
    assert_eq!(results.len(), dispatch_count as usize);

    let count = TAINT_GATE_CALLS.load(Ordering::SeqCst);
    assert_eq!(
        count, dispatch_count,
        "MUTANT #1 KILL: taint_gate must fire exactly once per real dispatch \
         through ToolScheduler — a deleted call site drives count < dispatches"
    );
}

/// Mutant #2 (DD4, Murat): forcing the gate to `Deny` must actually block
/// dispatch. If a `Deny` verdict changed nothing, the seam would be
/// indistinguishable from a dead hook — theater, not load-bearing wiring.
#[cfg(feature = "test-instrumentation")]
#[tokio::test]
#[serial_test::serial(taint_gate_global_state)]
async fn ac2_taint_gate_deny_mutant_blocks_dispatch() {
    use rustain::domain::services::permission_chain::TAINT_GATE_FORCE_DENY;
    // Isolate from other tests sharing the process-global static.
    TAINT_GATE_FORCE_DENY.store(true, Ordering::SeqCst);

    let security: Arc<dyn SecurityPort> = Arc::new(MockSecurity {
        mode: PermissionMode::Yolo,
    });
    let counter = Arc::new(AtomicU32::new(0));
    let tools: Arc<dyn ToolSetPort> = Arc::new(CountedToolSet {
        counter: counter.clone(),
        delay_ms: 0,
        parallel_safe: true,
    });
    let approval_runtime = ApprovalRuntime::new(
        16,
        Arc::new(rustain::adapters::noop::NoOpApprovalPersistence),
    );
    let scheduler = ToolScheduler::new(security, tools, approval_runtime, 16);

    let batch = vec![ToolCallRequest {
        id: "denied-1".into(),
        tool_name: "CountedTool".into(),
        input: serde_json::json!({}),
    }];
    let source = ApprovalSource::ForegroundTurn {
        conversation_id: "c-ac2-deny".into(),
    };
    let results = scheduler
        .schedule(source, batch, CancellationToken::new(), None)
        .await;

    TAINT_GATE_FORCE_DENY.store(false, Ordering::SeqCst);

    assert_eq!(results.len(), 1);
    assert!(
        matches!(results[0], ToolCall::Error { .. }),
        "MUTANT #2 KILL: a forced Deny verdict MUST block dispatch (Error), \
         got {:?} — if Deny changed nothing the seam is theater",
        results[0]
    );
    assert_eq!(
        counter.load(Ordering::SeqCst),
        0,
        "tool must NOT have executed once the taint gate denied it"
    );
}

/// Silent-path proof (c): user-originated traffic remains executable when the
/// test-only forced-deny switch is disabled.
#[cfg(feature = "test-instrumentation")]
#[tokio::test]
#[serial_test::serial(taint_gate_global_state)]
async fn ac2_user_originated_default_remains_silent() {
    use rustain::domain::services::permission_chain::TAINT_GATE_FORCE_DENY;
    TAINT_GATE_FORCE_DENY.store(false, Ordering::SeqCst);

    let security: Arc<dyn SecurityPort> = Arc::new(MockSecurity {
        mode: PermissionMode::Yolo,
    });
    let counter = Arc::new(AtomicU32::new(0));
    let tools: Arc<dyn ToolSetPort> = Arc::new(CountedToolSet {
        counter: counter.clone(),
        delay_ms: 0,
        parallel_safe: true,
    });
    let approval_runtime = ApprovalRuntime::new(
        16,
        Arc::new(rustain::adapters::noop::NoOpApprovalPersistence),
    );
    let scheduler = ToolScheduler::new(security, tools, approval_runtime, 16);

    let batch = vec![ToolCallRequest {
        id: "allowed-1".into(),
        tool_name: "CountedTool".into(),
        input: serde_json::json!({}),
    }];
    let source = ApprovalSource::ForegroundTurn {
        conversation_id: "c-ac2-user-originated".into(),
    };
    let results = scheduler
        .schedule(source, batch, CancellationToken::new(), None)
        .await;

    assert_eq!(results.len(), 1);
    assert!(
        matches!(results[0], ToolCall::Success { .. }),
        "user-originated traffic must remain silent — got {:?}",
        results[0]
    );
    assert_eq!(counter.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn taint_approval_never_bypasses_a_hard_blocklist_denial() {
    let counter = Arc::new(AtomicU32::new(0));
    let tools: Arc<dyn ToolSetPort> = Arc::new(CountedToolSet {
        counter: counter.clone(),
        delay_ms: 0,
        parallel_safe: true,
    });
    let scheduler = ToolScheduler::new(
        Arc::new(DenyingSecurity),
        tools,
        ApprovalRuntime::new(
            16,
            Arc::new(rustain::adapters::noop::NoOpApprovalPersistence),
        ),
        16,
    );
    let results = scheduler
        .schedule_with_provenance(
            ApprovalSource::ForegroundTurn {
                conversation_id: "tainted-hard-deny".into(),
            },
            vec![ToolCallRequest {
                id: "blocked-tainted-command".into(),
                tool_name: "Bash".into(),
                input: serde_json::json!({"command": "forbidden"}),
            }],
            CancellationToken::new(),
            None,
            rustain::domain::models::ProvenanceTag::SelfOriginated,
        )
        .await;
    assert!(matches!(results.as_slice(), [ToolCall::Error { .. }]));
    assert_eq!(
        counter.load(Ordering::SeqCst),
        0,
        "taint approval must not convert a hard denial into executable work"
    );
}

// ═══════════════════════════════════════════════════════════════════════
// AC6 — Testability invariants
// ═══════════════════════════════════════════════════════════════════════

// ApprovalSource keeps #[non_exhaustive] and exposes typed RemotePeer (R2).
//
// Real enforcement lives in `conformance_agent_message_bus.rs`
// (`ac6_approval_source_has_typed_remote_peer_and_stays_non_exhaustive`), which
// source-scans `tool_call.rs` for the `#[non_exhaustive]` attribute, the
// RemotePeer variant, and its RAP `PeerId` field.

// Zero new `.launch(` sites — this story constructs no nodes.
//
// Real enforcement is `ac3_orchestrator_has_exactly_one_launch_call_site`
// (`conformance_fork_join_executor.rs`), which source-scans
// `src/infrastructure/orchestrator/` for `.launch(` call sites and asserts
// exactly one. The prior version of this test here (`ac6_zero_new_launch_sites`)
// had an EMPTY body (always green) and cited a non-existent
// `MAX_KNOWN_LAUNCH_SITES` symbol — removed as vacuous (AI-12.3 post-review
// closure 2026-07-02). Note: the real ratchet's scan is scoped to
// `src/infrastructure/orchestrator/` only, so it does not itself prove
// "zero new `.launch(` sites anywhere in this story's changed files" — this
// story's File List (domain/services, adapters/subagent) touches no code
// outside that scan's exclusion, confirmed by inspection during the
// AI-12.3 pass; no `.launch(` call was added by 14.6.

// ═══════════════════════════════════════════════════════════════════════
// Test helpers
// ═══════════════════════════════════════════════════════════════════════

struct DenyingSecurity;

#[async_trait]
impl SecurityPort for DenyingSecurity {
    fn check_blocklist(&self, _command: &str) -> Result<(), PermissionError> {
        Err(PermissionError::Blocked("test hard denial".into()))
    }

    fn check_workspace_access(
        &self,
        _path: &std::path::Path,
        _op: FileOperation,
    ) -> Result<PathAccessType, PermissionError> {
        Ok(PathAccessType::Workspace)
    }

    fn current_mode(&self) -> PermissionMode {
        PermissionMode::Yolo
    }

    fn set_mode(&self, _mode: PermissionMode) {}
}

struct MockSecurity {
    mode: PermissionMode,
}

#[async_trait]
impl SecurityPort for MockSecurity {
    fn check_blocklist(&self, _command: &str) -> Result<(), PermissionError> {
        Ok(())
    }
    fn check_workspace_access(
        &self,
        _path: &std::path::Path,
        _op: FileOperation,
    ) -> Result<PathAccessType, PermissionError> {
        Ok(PathAccessType::Workspace)
    }
    fn current_mode(&self) -> PermissionMode {
        self.mode
    }
    fn set_mode(&self, _mode: PermissionMode) {}
}

/// A tool set with a counted side effect for the exactly-once keystone.
struct CountedToolSet {
    counter: Arc<AtomicU32>,
    delay_ms: u64,
    parallel_safe: bool,
}

#[async_trait]
impl ToolSetPort for CountedToolSet {
    fn available_tools(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: "CountedTool".to_string(),
            description: "increments counter".to_string(),
            input_schema: serde_json::json!({}),
            parallel_safe: self.parallel_safe,
        }]
    }
    async fn execute(
        &self,
        _tool_name: &str,
        _input: serde_json::Value,
        cancel: CancellationToken,
    ) -> Result<ToolResult, ToolError> {
        // Non-idempotent side effect: increment the counter
        self.counter.fetch_add(1, Ordering::SeqCst);

        if self.delay_ms > 0 {
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(self.delay_ms)) => {},
                _ = cancel.cancelled() => return Err(ToolError::Cancelled),
            }
        }
        Ok(ToolResult {
            tool_use_id: String::new(),
            content: "counted".to_string(),
            is_error: false,
        })
    }
}

/// A tool set with two independently-counted side effects: one on a gated
/// ("GatedTool", Elevated risk by default) tool, one on an ungated built-in
/// Safe tool ("remember") — for the AC1 sibling-independence positive
/// control. Both are listed `parallel_safe: true` so `ToolScheduler::schedule`
/// takes the concurrent `FuturesOrdered` path (never the sequential fallback).
struct DualCountedToolSet {
    gated_counter: Arc<AtomicU32>,
    sibling_counter: Arc<AtomicU32>,
}

#[async_trait]
impl ToolSetPort for DualCountedToolSet {
    fn available_tools(&self) -> Vec<ToolDefinition> {
        vec![
            ToolDefinition {
                name: "GatedTool".to_string(),
                description: "increments gated counter".to_string(),
                input_schema: serde_json::json!({}),
                parallel_safe: true,
            },
            ToolDefinition {
                name: "remember".to_string(),
                description: "increments sibling counter".to_string(),
                input_schema: serde_json::json!({}),
                parallel_safe: true,
            },
        ]
    }
    async fn execute(
        &self,
        tool_name: &str,
        _input: serde_json::Value,
        _cancel: CancellationToken,
    ) -> Result<ToolResult, ToolError> {
        match tool_name {
            "GatedTool" => {
                self.gated_counter.fetch_add(1, Ordering::SeqCst);
            }
            "remember" => {
                self.sibling_counter.fetch_add(1, Ordering::SeqCst);
            }
            _ => unreachable!("DualCountedToolSet only serves GatedTool/remember"),
        }
        Ok(ToolResult {
            tool_use_id: String::new(),
            content: "counted".to_string(),
            is_error: false,
        })
    }
}
