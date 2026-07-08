//! Conformance: subagent sandbox inheritance is narrow-only (ADR-10-3).
//!
//! Invariant (ADR-10-3): *a subagent must never escape the parent's sandbox.*
//! The parent's effective policy is the ceiling; the child's policy is at most
//! that ceiling, never above it. This holds across every dimension:
//!   - variant lattice: Permissive ⊐ WorkspaceWrite ⊐ ReadOnly
//!   - network egress:  child ≤ parent  (false cannot widen to true)
//!   - writable roots:  child ⊆ parent
//!   - read-only paths: child ⊇ parent  (more read-only = narrower)
//!
//! Two layers, both required:
//!   1. CONTRACT — the full `validate_narrowing` truth table. The domain
//!      service is the single source of truth the spawn path delegates to.
//!      Living here (not only in the in-module `#[cfg(test)]`) makes this a
//!      cross-cutting ratchet: if someone deletes or weakens the unit tests,
//!      this still fails.
//!   2. WIRING — proof that `InProcessSubagentRunner::launch` actually CALLS
//!      `validate_narrowing` and REJECTS a widening `sandbox_override` BEFORE
//!      spawning the child. This is the privilege-escalation invariant the
//!      Epic 10 retro flagged as "unenforced" (AI-10.2): a correct contract is
//!      worthless if the spawn boundary never invokes it.

use std::path::PathBuf;
use std::sync::Arc;

use arc_swap::ArcSwap;
use rustain::adapters::noop::{NoOpApprovalPersistence, NoOpProvider};
use rustain::adapters::sandbox::NoOpSandbox;
use rustain::adapters::security_adapter::SecurityAdapter;
use rustain::adapters::subagent::InProcessSubagentRunner;
use rustain::adapters::toolset_adapter::ToolSetAdapter;
use rustain::domain::models::{
    AgentLaunchSpec, ModelTier, SandboxPolicy, SubagentError, ToolPolicy,
};
use rustain::domain::ports::SubagentRunner;
use rustain::domain::services::approval_runtime::ApprovalRuntime;
use rustain::domain::services::sandbox_narrowing::validate_narrowing;
use rustain::domain::services::tool_scheduler::ToolScheduler;
use rustain::infrastructure::runtime::event_bus::EventBus;
use rustain::infrastructure::subagent::{NodeTree, SubagentSpool};
use tokio_util::sync::CancellationToken;

// ── policy constructors ─────────────────────────────────────────────────────

fn read_only(network: bool) -> SandboxPolicy {
    SandboxPolicy::ReadOnly { network }
}

fn workspace_write(writable: &[&str], read_only_paths: &[&str], network: bool) -> SandboxPolicy {
    SandboxPolicy::WorkspaceWrite {
        writable_roots: writable.iter().map(PathBuf::from).collect(),
        read_only_paths: read_only_paths.iter().map(PathBuf::from).collect(),
        network,
    }
}

/// Assert a widening was rejected, and (when given) that it was rejected for
/// the expected ADR-10-3 dimension — guarding against a too-coarse rejection
/// that would mask a different bug.
fn assert_widens(result: Result<SandboxPolicy, SubagentError>, expected_dimension: Option<&str>) {
    match result {
        Err(SubagentError::PolicyWidensParent { dimension, .. }) => {
            if let Some(expected) = expected_dimension {
                assert_eq!(
                    dimension, expected,
                    "rejected for the wrong dimension: got {dimension:?}, expected {expected:?}"
                );
            }
        }
        other => panic!("expected PolicyWidensParent, got {other:?}"),
    }
}

// ── LAYER 1: CONTRACT — full ADR-10-3 narrowing truth table ─────────────────

#[test]
fn permissive_parent_accepts_every_child() {
    let parent = SandboxPolicy::Permissive;
    for child in [
        SandboxPolicy::Permissive,
        read_only(true),
        read_only(false),
        workspace_write(&["/ws"], &[], true),
    ] {
        assert!(
            validate_narrowing(&parent, &child).is_ok(),
            "Permissive parent must accept any child, rejected {child:?}"
        );
    }
}

#[test]
fn variant_lattice_cannot_be_widened() {
    // WorkspaceWrite parent → Permissive child is a widening (loses confinement).
    assert_widens(
        validate_narrowing(
            &workspace_write(&["/ws"], &[], true),
            &SandboxPolicy::Permissive,
        ),
        Some("variant"),
    );
    // ReadOnly parent → WorkspaceWrite child gains write capability.
    assert_widens(
        validate_narrowing(&read_only(true), &workspace_write(&["/ws"], &[], true)),
        Some("variant"),
    );
    // ReadOnly parent → Permissive child is the maximal escalation.
    assert_widens(
        validate_narrowing(&read_only(true), &SandboxPolicy::Permissive),
        Some("variant"),
    );
}

#[test]
fn variant_lattice_allows_narrowing() {
    // WorkspaceWrite → ReadOnly (drop write) is a valid narrowing.
    assert!(validate_narrowing(&workspace_write(&["/ws"], &[], true), &read_only(true)).is_ok());
    // ReadOnly → ReadOnly is the identity narrowing.
    assert!(validate_narrowing(&read_only(false), &read_only(false)).is_ok());
}

#[test]
fn network_egress_cannot_be_widened() {
    // ReadOnly: parent no-network, child wants network → reject on "network".
    assert_widens(
        validate_narrowing(&read_only(false), &read_only(true)),
        Some("network"),
    );
    // WorkspaceWrite: parent no-network, child wants network → reject on "network".
    assert_widens(
        validate_narrowing(
            &workspace_write(&["/ws"], &[], false),
            &workspace_write(&["/ws"], &[], true),
        ),
        Some("network"),
    );
}

#[test]
fn network_egress_allows_narrowing() {
    // Dropping network access (true → false) is a valid narrowing on each variant.
    assert!(validate_narrowing(&read_only(true), &read_only(false)).is_ok());
    assert!(
        validate_narrowing(
            &workspace_write(&["/ws"], &[], true),
            &workspace_write(&["/ws"], &[], false),
        )
        .is_ok()
    );
}

#[test]
fn writable_roots_must_be_subset_of_parent() {
    let parent = workspace_write(&["/ws"], &[], true);
    // Child requests a writable root the parent never granted → widening.
    let child = workspace_write(&["/ws", "/etc"], &[], true);
    assert_widens(validate_narrowing(&parent, &child), Some("writable_roots"));

    // A strict subset of writable roots is a valid narrowing.
    let parent_two = workspace_write(&["/ws", "/data"], &[], true);
    let narrowed = workspace_write(&["/ws"], &[], true);
    assert!(validate_narrowing(&parent_two, &narrowed).is_ok());
}

#[test]
fn read_only_paths_must_be_superset_of_parent() {
    // Parent pins /secrets read-only; child dropping it would re-expose it → widening.
    let parent = workspace_write(&["/ws"], &["/secrets"], true);
    let child = workspace_write(&["/ws"], &[], true);
    assert_widens(validate_narrowing(&parent, &child), Some("read_only_paths"));

    // Adding MORE read-only paths (superset) is a valid narrowing.
    let stricter = workspace_write(&["/ws"], &["/secrets", "/keys"], true);
    assert!(validate_narrowing(&parent, &stricter).is_ok());
}

// ── LAYER 2: WIRING — launch() enforces the contract before spawn ───────────

async fn make_runner_with_parent(
    parent: SandboxPolicy,
) -> (InProcessSubagentRunner, tempfile::TempDir) {
    let tmp = tempfile::tempdir().unwrap();
    let provider = Arc::new(NoOpProvider) as Arc<dyn rustain::domain::ports::StreamingProvider>;
    let storage = Arc::new(rustain::adapters::filesystem::FileSystemStorage::new(
        tmp.path().to_path_buf(),
    )) as Arc<dyn rustain::domain::ports::StoragePort>;
    let security = Arc::new(SecurityAdapter::new(PathBuf::from(".")))
        as Arc<dyn rustain::domain::ports::SecurityPort>;
    let sandbox = Arc::new(ArcSwap::from_pointee(
        Arc::new(NoOpSandbox) as Arc<dyn rustain::domain::ports::SandboxManager>
    ));
    let tools = Arc::new(ToolSetAdapter::new(
        PathBuf::from("."),
        storage.clone(),
        sandbox,
        Arc::new(tokio::sync::RwLock::new(SandboxPolicy::Permissive)),
    )) as Arc<dyn rustain::domain::ports::ToolSetPort>;
    let approval = ApprovalRuntime::new(1024, Arc::new(NoOpApprovalPersistence));
    let scheduler = ToolScheduler::new(security.clone(), tools.clone(), approval.clone(), 1024);
    let (event_bus, _event_rx) = EventBus::new(1024);
    let event_bus = Arc::new(event_bus);
    let registry = Arc::new(NodeTree::new());
    let parent_sandbox = Arc::new(tokio::sync::RwLock::new(parent));
    let spool = Arc::new(SubagentSpool::new(tmp.path().join("spool")).await.unwrap());
    let root_authority =
        rustain::domain::models::CapabilityToken::r1_root(rustain::domain::models::AgentId::root());
    let authority_ledger = Arc::new(
        rustain::domain::services::authority_ledger::AuthorityLedger::new(root_authority.clone()),
    );
    let authority =
        Arc::new(rustain::adapters::authority::InProcessAuthorityProvider::new(authority_ledger))
            as Arc<dyn rustain::domain::ports::AuthorityProvider>;

    let runner = InProcessSubagentRunner::new(
        provider,
        storage,
        security,
        tools,
        approval,
        scheduler,
        event_bus,
        registry,
        parent_sandbox,
        spool,
        authority,
        root_authority,
    );
    (runner, tmp)
}

fn spec_with_override(sandbox_override: Option<SandboxPolicy>) -> AgentLaunchSpec {
    AgentLaunchSpec {
        prompt: String::from("hello"),
        effective_model: String::from("test-model"),
        tier: ModelTier::CheapAgentic,
        tools_allow: ToolPolicy::InheritFromParent,
        parent_ctx_tokens: 0,
        sandbox_override,
        parent_trace: None,
        isolated: false,
    }
}

/// THE gap AI-10.2 closed: a child whose `sandbox_override` widens the parent
/// must be rejected at `launch()` — BEFORE any child task is spawned. If this
/// regresses, a subagent can escalate beyond its parent's sandbox.
#[tokio::test]
async fn launch_rejects_widening_variant_before_spawn() {
    // Parent is read-only; child asks for write access.
    let (runner, _tmp) = make_runner_with_parent(read_only(false)).await;
    let spec = spec_with_override(Some(workspace_write(&["/ws"], &[], true)));

    let result = runner.launch(spec, CancellationToken::new()).await;
    match result {
        Err(SubagentError::PolicyWidensParent { .. }) => {}
        Err(other) => panic!("expected PolicyWidensParent, got error {other:?}"),
        Ok(_) => panic!(
            "launch() must reject a write-widening child of a read-only parent, but it spawned"
        ),
    }
    // Rejection happens before registration → registry stays empty (no orphan).
    assert!(
        runner.registry().list().await.is_empty(),
        "a rejected launch must not leave a registered child"
    );
}

#[tokio::test]
async fn launch_rejects_network_widening_before_spawn() {
    // Parent has network blocked; child tries to re-enable it.
    let (runner, _tmp) = make_runner_with_parent(read_only(false)).await;
    let spec = spec_with_override(Some(read_only(true)));

    let result = runner.launch(spec, CancellationToken::new()).await;
    match result {
        Err(SubagentError::PolicyWidensParent { dimension, .. }) => {
            assert_eq!(
                dimension, "network",
                "expected a network-dimension rejection"
            );
        }
        Err(other) => panic!("expected PolicyWidensParent(network), got error {other:?}"),
        Ok(_) => panic!("launch() must reject re-enabling network, but it spawned"),
    }
    assert!(runner.registry().list().await.is_empty());
}

/// A child that narrows (or matches) the parent must launch successfully — the
/// guard rejects only widening, never legitimate narrowing.
#[tokio::test]
async fn launch_accepts_narrowing_override() {
    // Parent allows network; child drops it (a valid narrowing).
    let (runner, _tmp) = make_runner_with_parent(read_only(true)).await;
    let spec = spec_with_override(Some(read_only(false)));

    let handle = runner
        .launch(spec, CancellationToken::new())
        .await
        .expect("narrowing override must be accepted");
    assert!(!handle.task_id.is_empty());
    handle.cancel.cancel();
}
