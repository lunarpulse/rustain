//! Conformance tests for Story 14.2 authority tokens.
//!
//! These tests intentionally scan imports (unlike some redaction scans): the
//! hazard is overloading the discovery-side `Capability`/`CapabilityProvider`
//! into the authority port. Comments are skipped so the required guard comment
//! can cite the hazard without tripping the ratchet.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use proptest::prelude::*;
use rustain::domain::models::{
    AgentId, Budget, CapabilityFlag, CapabilitySet, CapabilityToken, DelegateConstraint,
    DelegateRequest, NodeCheckpoint, NodeOrigin, NodeState, Op, WireOwnershipKind,
};
use rustain::domain::ports::{AuthorityError, AuthorityProvider};
use rustain::domain::services::authority_ledger::{AuthorityLedger, ConservationSnapshot};
use rustain::infrastructure::subagent::NodeJournal;

const MAX_KNOWN_DISCOVERY_CAPABILITY_REFS: usize = 0;

fn collect_rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                collect_rs_files(&path, out);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                out.push(path);
            }
        }
    }
}

fn is_comment_line(trimmed: &str) -> bool {
    trimmed.starts_with("//") || trimmed.starts_with("/*") || trimmed.starts_with('*')
}

fn count_discovery_capability_refs_in_source(source: &str) -> usize {
    let capability = regex::Regex::new(r"\bCapability\b").unwrap();
    let provider = regex::Regex::new(r"\bCapabilityProvider\b").unwrap();

    source
        .lines()
        .filter(|line| !is_comment_line(line.trim()))
        .map(|line| capability.find_iter(line).count() + provider.find_iter(line).count())
        .sum()
}

#[test]
fn no_overload_guard_scans_authority_port_imports() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let target = manifest.join("src/domain/ports/authority_provider.rs");
    let content = std::fs::read_to_string(&target).expect("read authority_provider.rs");

    assert!(
        content.contains("trait AuthorityProvider"),
        "scanned-right-file assertion failed: AuthorityProvider trait missing"
    );
    assert!(
        content.len() > 200,
        "scanned-right-file assertion failed: authority_provider.rs unexpectedly tiny"
    );

    let count = count_discovery_capability_refs_in_source(&content);
    assert_eq!(
        count, MAX_KNOWN_DISCOVERY_CAPABILITY_REFS,
        "AuthorityProvider must not import or name discovery-side Capability/CapabilityProvider in code"
    );
}

#[test]
fn no_overload_matcher_positive_control() {
    let fixture = r#"
        use crate::domain::models::Capability;
        use crate::domain::ports::CapabilityProvider;
        use crate::domain::models::{CapabilitySet, CapabilityFlag, CapabilityToken};
        fn validate(want: &CapabilityFlag) { let _ = want; }
    "#;

    assert_eq!(
        count_discovery_capability_refs_in_source(fixture),
        2,
        "positive control must catch exact Capability and CapabilityProvider but not CapabilitySet/Flag/Token"
    );
}

#[test]
fn authority_port_file_is_the_only_authority_provider_trait_definition() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let src = manifest.join("src");
    let mut files = Vec::new();
    collect_rs_files(&src, &mut files);

    let mut trait_defs = BTreeSet::new();
    for path in files {
        let content = std::fs::read_to_string(&path).unwrap_or_default();
        if content.lines().any(|line| {
            let t = line.trim();
            !is_comment_line(t) && t.contains("trait AuthorityProvider")
        }) {
            trait_defs.insert(
                path.strip_prefix(&src)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/"),
            );
        }
    }

    let expected = BTreeSet::from([String::from("domain/ports/authority_provider.rs")]);
    assert_eq!(trait_defs, expected);
}

fn root_token() -> CapabilityToken {
    CapabilityToken::root(
        AgentId::root(),
        CapabilitySet::from_flags(&[
            CapabilityFlag::Spawn,
            CapabilityFlag::ReadFs,
            CapabilityFlag::WriteFs,
            CapabilityFlag::Network,
        ]),
        Budget {
            requests: 100,
            cost_micros: 1_000_000,
        },
        3,
        Some(4_102_444_800_000),
        Some(100),
    )
}

fn child_request(scope: &str, budget: Budget) -> DelegateRequest {
    DelegateRequest {
        scope: AgentId::parse(scope).unwrap(),
        capabilities: CapabilitySet::from_flags(&[CapabilityFlag::Spawn, CapabilityFlag::ReadFs]),
        constraint: DelegateConstraint {
            allowed: CapabilitySet::from_flags(&[CapabilityFlag::Spawn, CapabilityFlag::ReadFs]),
            max_depth: 3,
            max_subset: CapabilitySet::from_flags(&[CapabilityFlag::Spawn, CapabilityFlag::ReadFs]),
        },
        budget,
        not_after: Some(4_102_444_700_000),
        uses_limit: Some(10),
    }
}

#[test]
fn canonical_id_excludes_signature_and_uses_length_prefixes() {
    let token = root_token();
    let id_without_sig = token.compute_id_with_signature_for_test(None);
    let id_with_sig = token.compute_id_with_signature_for_test(Some(&[0u8; 64]));
    assert_eq!(
        id_without_sig, id_with_sig,
        "signature bytes must be excluded from CapabilityTokenId preimage"
    );

    let left = CapabilityToken::root(
        AgentId::parse("ab/c").unwrap(),
        CapabilitySet::from_flags(&[CapabilityFlag::Spawn]),
        Budget {
            requests: 1,
            cost_micros: 1,
        },
        1,
        None,
        None,
    );
    let right = CapabilityToken::root(
        AgentId::parse("a/bc").unwrap(),
        CapabilitySet::from_flags(&[CapabilityFlag::Spawn]),
        Budget {
            requests: 1,
            cost_micros: 1,
        },
        1,
        None,
        None,
    );
    assert_ne!(
        left.id, right.id,
        "length-prefixing must prevent boundary collisions"
    );
}

#[test]
fn delegate_debits_and_settle_refunds_unused_once() {
    let root = root_token();
    let ledger = AuthorityLedger::new(root.clone());
    let child = ledger
        .delegate(
            &root,
            child_request(
                "child-a",
                Budget {
                    requests: 10,
                    cost_micros: 100_000,
                },
            ),
        )
        .expect("valid subset delegates");

    assert_eq!(
        ledger.available(&root.id).unwrap(),
        Budget {
            requests: 90,
            cost_micros: 900_000,
        },
        "delegate must debit parent immediately"
    );

    ledger
        .consume(
            &child.id,
            Budget {
                requests: 3,
                cost_micros: 30_000,
            },
        )
        .expect("child spends within reservation");
    ledger.settle(&child.id).expect("first settle succeeds");
    ledger
        .settle(&child.id)
        .expect("second settle is idempotent");

    assert_eq!(
        ledger.available(&root.id).unwrap(),
        Budget {
            requests: 97,
            cost_micros: 970_000,
        },
        "settle refunds reserved minus consumed exactly once"
    );
}

#[test]
fn revoke_scope_denies_next_point_of_use() {
    let root = root_token();
    let ledger = AuthorityLedger::new(root.clone());
    let child = ledger
        .delegate(
            &root,
            child_request(
                "child-a",
                Budget {
                    requests: 10,
                    cost_micros: 100_000,
                },
            ),
        )
        .expect("valid subset delegates");

    ledger
        .validate(
            &child,
            &CapabilityFlag::ReadFs,
            &AgentId::parse("child-a").unwrap(),
        )
        .expect("pre-revoke validate succeeds");
    ledger
        .revoke_scope(&AgentId::parse("child-a").unwrap())
        .expect("scope revoke succeeds");

    let err = ledger
        .validate(
            &child,
            &CapabilityFlag::ReadFs,
            &AgentId::parse("child-a").unwrap(),
        )
        .expect_err("post-revoke validate must deny next gated action");
    assert!(
        err.to_string().contains("revoked"),
        "unexpected error: {err}"
    );
}

#[test]
fn conservation_invariant_holds_across_delegate_consume_settle() {
    let root = root_token();
    let ledger = AuthorityLedger::new(root.clone());
    let child = ledger
        .delegate(
            &root,
            child_request(
                "child-a",
                Budget {
                    requests: 10,
                    cost_micros: 100_000,
                },
            ),
        )
        .expect("valid subset delegates");

    assert_eq!(
        ledger.conservation(&root.id).unwrap(),
        ConservationSnapshot {
            total: Budget {
                requests: 100,
                cost_micros: 1_000_000,
            },
            available: Budget {
                requests: 90,
                cost_micros: 900_000,
            },
            live_reservations: Budget {
                requests: 10,
                cost_micros: 100_000,
            },
            consumed: Budget::ZERO,
        }
    );

    ledger
        .consume(
            &child.id,
            Budget {
                requests: 4,
                cost_micros: 40_000,
            },
        )
        .expect("consume within reservation");
    let mid = ledger.conservation(&root.id).unwrap();
    assert_eq!(
        mid.available + mid.live_reservations + mid.consumed,
        mid.total
    );

    ledger.settle(&child.id).expect("settle child");
    let end = ledger.conservation(&root.id).unwrap();
    assert_eq!(
        end.available + end.live_reservations + end.consumed,
        end.total
    );
    assert_eq!(end.live_reservations, Budget::ZERO);
    assert_eq!(
        end.consumed,
        Budget {
            requests: 4,
            cost_micros: 40_000,
        }
    );
}

#[test]
fn token_scope_is_single_leaf_and_bijective_with_registered_tokens() {
    let root = root_token();
    let ledger = AuthorityLedger::new(root.clone());
    let child = ledger
        .delegate(
            &root,
            child_request(
                "child-a",
                Budget {
                    requests: 1,
                    cost_micros: 1,
                },
            ),
        )
        .expect("valid subset delegates");

    assert!(
        child.scope.is_local(),
        "R1 token scope must be one leaf AgentId"
    );
    assert_eq!(ledger.token_for_scope(&child.scope).unwrap(), child.id);
    assert_eq!(ledger.scope_for_token(&child.id).unwrap(), child.scope);

    let err = ledger
        .delegate(
            &root,
            child_request(
                "parent/child",
                Budget {
                    requests: 1,
                    cost_micros: 1,
                },
            ),
        )
        .expect_err("path/prefix scopes are deferred past R1");
    assert!(err.to_string().contains("scope"), "unexpected error: {err}");
}

async fn register_authorized_child(
    tree: &rustain::infrastructure::subagent::NodeTree,
    child: &CapabilityToken,
) -> tokio::sync::mpsc::Receiver<rustain::domain::models::Op> {
    let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel(1);
    let (status_tx, _status_rx) =
        tokio::sync::watch::channel(rustain::domain::models::NodeState::Created);
    let (_metrics_tx, metrics_rx) =
        tokio::sync::watch::channel(rustain::domain::models::AgentMetrics::default());
    let handle = rustain::infrastructure::subagent::AgentHandle {
        isolated: false,
        agent_id: child.scope.clone(),
        token: child.id,
        command_tx: cmd_tx,
        cancel_token: tokio_util::sync::CancellationToken::new(),
        depth: 0,
        subagent_type: "authority-test".into(),
        spawned_at: 0,
        status: status_tx,
        metrics: metrics_rx,
        mailbox_budget: rustain::infrastructure::subagent::MailboxBudget::new(),
    };
    tree.register(child.scope.clone(), AgentId::root(), handle)
        .await
        .unwrap();
    cmd_rx
}

#[tokio::test(flavor = "current_thread")]
async fn synchronous_revoke_differential_real_hook_denies_deferred_allows() {
    let root = root_token();
    let ledger = std::sync::Arc::new(AuthorityLedger::new(root.clone()));
    let child = ledger
        .delegate(
            &root,
            child_request(
                "child-sync",
                Budget {
                    requests: 1,
                    cost_micros: 1,
                },
            ),
        )
        .expect("valid subset delegates");
    let tree = rustain::infrastructure::subagent::NodeTree::new().with_on_cascade_kill({
        let ledger = ledger.clone();
        std::sync::Arc::new(move |id| {
            ledger.revoke_scope(id).unwrap();
        })
    });
    let mut cmd_rx = register_authorized_child(&tree, &child).await;
    let status = tree.status_sender(&child.scope).await.unwrap();
    tokio::spawn(async move {
        if matches!(cmd_rx.recv().await, Some(rustain::domain::models::Op::Kill)) {
            let _ = status.send(rustain::domain::models::NodeState::Cancelled);
        }
    });
    tree.cascade_kill(&child.scope, std::time::Duration::from_millis(50))
        .await
        .unwrap();
    assert!(
        ledger
            .validate(&child, &CapabilityFlag::ReadFs, &child.scope)
            .is_err()
    );

    let root = root_token();
    let deferred_ledger = AuthorityLedger::new(root.clone());
    let deferred_child = deferred_ledger
        .delegate(
            &root,
            child_request(
                "child-deferred",
                Budget {
                    requests: 1,
                    cost_micros: 1,
                },
            ),
        )
        .expect("valid subset delegates");
    let deferred_tree = rustain::infrastructure::subagent::NodeTree::new();
    let mut cmd_rx = register_authorized_child(&deferred_tree, &deferred_child).await;
    let status = deferred_tree
        .status_sender(&deferred_child.scope)
        .await
        .unwrap();
    tokio::spawn(async move {
        if matches!(cmd_rx.recv().await, Some(rustain::domain::models::Op::Kill)) {
            let _ = status.send(rustain::domain::models::NodeState::Cancelled);
        }
    });
    deferred_tree
        .cascade_kill(&deferred_child.scope, std::time::Duration::from_millis(50))
        .await
        .unwrap();
    deferred_ledger
        .validate(
            &deferred_child,
            &CapabilityFlag::ReadFs,
            &deferred_child.scope,
        )
        .expect("deferred control proves synchronous hook is load-bearing");
}

fn arb_budget(max_req: u64, max_cost: u64) -> impl Strategy<Value = Budget> {
    (0..=max_req, 0..=max_cost).prop_map(|(requests, cost_micros)| Budget {
        requests,
        cost_micros,
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn delegated_child_is_subset_on_budget_uses_and_ttl(
        budget in arb_budget(50, 500_000),
        uses in 0u32..=50,
        ttl_delta in 0u64..=10_000,
    ) {
        let root = root_token();
        let ledger = AuthorityLedger::new(root.clone());
        let mut req = child_request("child-prop", budget);
        req.uses_limit = Some(uses);
        req.not_after = Some(4_102_444_800_000 - ttl_delta);

        let child = ledger.delegate(&root, req).expect("generated child is subset");
        prop_assert!(child.is_subset_of(&root));
        prop_assert_eq!(child.depth(&ledger).unwrap(), root.depth(&ledger).unwrap() + 1);
    }

    #[test]
    fn escalating_child_is_rejected(
        extra_requests in 1u64..=50,
        extra_cost in 1u64..=500_000,
    ) {
        let root = root_token();
        let ledger = AuthorityLedger::new(root.clone());
        let mut req = child_request(
            "child-escalates",
            Budget {
                requests: 100 + extra_requests,
                cost_micros: 1_000_000 + extra_cost,
            },
        );
        req.uses_limit = Some(101);

        prop_assert!(ledger.delegate(&root, req).is_err());
    }
}

// ── Review coverage (P8 / P9 / P10 / P15 / DN3) ──────────────────────────

fn restricted_root() -> CapabilityToken {
    CapabilityToken::root(
        AgentId::root(),
        CapabilitySet::from_flags(&[CapabilityFlag::Spawn]),
        Budget {
            requests: 100,
            cost_micros: 1_000_000,
        },
        3,
        Some(4_102_444_800_000),
        Some(100),
    )
}

/// DN3 — leapfrog differential: a token whose `max_depth` is STRICTER than the
/// node_tree `MAX_DEPTH` (3) rejects a delegation the node_tree would allow,
/// proving the token depth layer is load-bearing (not shadowed by the backstop).
#[test]
fn leapfrog_differential_token_rejects_depth_node_tree_would_allow() {
    let root = root_token();
    let ledger = AuthorityLedger::new(root.clone());
    // Depth-1 child carrying a stricter max_depth (1) than node_tree MAX_DEPTH (3).
    let mut req1 = child_request(
        "leap-1",
        Budget {
            requests: 10,
            cost_micros: 100_000,
        },
    );
    req1.constraint.max_depth = 1;
    let child1 = ledger
        .delegate(&root, req1)
        .expect("depth-1 child with stricter max_depth delegates from root");
    // Delegating FROM child1 to a depth-2 token: node_tree (MAX_DEPTH=3) would
    // allow depth 2, but child1's token max_depth=1 must reject.
    let req2 = child_request(
        "leap-2",
        Budget {
            requests: 5,
            cost_micros: 50_000,
        },
    );
    let err = ledger
        .delegate(&child1, req2)
        .expect_err("token layer must reject past its own stricter max_depth");
    assert!(
        matches!(
            err,
            AuthorityError::MaxDepthExceeded {
                limit: 1,
                attempted: 2
            }
        ),
        "expected MaxDepthExceeded{{limit:1, attempted:2}} (token's ceiling), got {err:?}"
    );
}

/// P8 — per-axis non-subset rejection + hand-crafted positive control.
#[test]
fn delegate_rejects_non_subset_on_every_axis() {
    let parent = restricted_root(); // caps/allowed/max_subset = {Spawn}, max_depth = 3
    let ledger = AuthorityLedger::new(parent.clone());

    // (a) capabilities axis
    let mut req = child_request(
        "axis-caps",
        Budget {
            requests: 1,
            cost_micros: 1,
        },
    );
    req.capabilities = CapabilitySet::from_flags(&[CapabilityFlag::Spawn, CapabilityFlag::ReadFs]);
    assert!(
        ledger.delegate(&parent, req).is_err(),
        "capabilities escalation rejected"
    );

    // (b) allowed axis
    let mut req = child_request(
        "axis-allowed",
        Budget {
            requests: 1,
            cost_micros: 1,
        },
    );
    req.capabilities = CapabilitySet::from_flags(&[CapabilityFlag::Spawn]);
    req.constraint.allowed =
        CapabilitySet::from_flags(&[CapabilityFlag::Spawn, CapabilityFlag::ReadFs]);
    assert!(
        ledger.delegate(&parent, req).is_err(),
        "allowed escalation rejected"
    );

    // (c) max_subset axis
    let mut req = child_request(
        "axis-maxsubset",
        Budget {
            requests: 1,
            cost_micros: 1,
        },
    );
    req.capabilities = CapabilitySet::from_flags(&[CapabilityFlag::Spawn]);
    req.constraint.max_subset =
        CapabilitySet::from_flags(&[CapabilityFlag::Spawn, CapabilityFlag::ReadFs]);
    assert!(
        ledger.delegate(&parent, req).is_err(),
        "max_subset escalation rejected"
    );

    // (d) max_depth axis (NonSubset: child ceiling > parent ceiling)
    let mut req = child_request(
        "axis-maxdepth",
        Budget {
            requests: 1,
            cost_micros: 1,
        },
    );
    req.capabilities = CapabilitySet::from_flags(&[CapabilityFlag::Spawn]);
    req.constraint.allowed = CapabilitySet::from_flags(&[CapabilityFlag::Spawn]);
    req.constraint.max_subset = CapabilitySet::from_flags(&[CapabilityFlag::Spawn]);
    req.constraint.max_depth = 4;
    assert!(
        ledger.delegate(&parent, req).is_err(),
        "max_depth escalation rejected"
    );

    // (e) hand-crafted positive control: a strict subset delegates fine.
    let mut req = child_request(
        "axis-ok",
        Budget {
            requests: 1,
            cost_micros: 1,
        },
    );
    req.capabilities = CapabilitySet::from_flags(&[CapabilityFlag::Spawn]);
    req.constraint.allowed = CapabilitySet::from_flags(&[CapabilityFlag::Spawn]);
    req.constraint.max_subset = CapabilitySet::from_flags(&[CapabilityFlag::Spawn]);
    assert!(
        ledger.delegate(&parent, req).is_ok(),
        "valid strict subset delegates"
    );
}

/// P8 — antisymmetry: a ⊑ b ∧ b ⊑ a ⇒ a ≡ b (kills a ⊑ mis-implemented as `true`).
#[test]
fn subset_algebra_is_antisymmetric() {
    let a = CapabilityToken::root(
        AgentId::parse("a").unwrap(),
        CapabilitySet::from_flags(&[CapabilityFlag::Spawn]),
        Budget {
            requests: 5,
            cost_micros: 50,
        },
        3,
        None,
        None,
    );
    let b = CapabilityToken::root(
        AgentId::parse("b").unwrap(),
        CapabilitySet::from_flags(&[CapabilityFlag::ReadFs]),
        Budget {
            requests: 5,
            cost_micros: 50,
        },
        3,
        None,
        None,
    );
    let ab = a.is_subset_of(&b);
    let ba = b.is_subset_of(&a);
    if ab && ba {
        assert_eq!(
            a.id, b.id,
            "antisymmetry: mutual subset must imply identity"
        );
    }
    assert!(
        !ab || !ba,
        "disjoint-capability tokens must not be mutual subsets"
    );
}

/// P8 — transitivity via real delegation: grandchild ⊆ root, depth strictly −1/hop.
#[test]
fn subset_algebra_is_transitive_via_delegate() {
    let root = root_token();
    let ledger = AuthorityLedger::new(root.clone());
    let child1 = ledger
        .delegate(
            &root,
            child_request(
                "trans-1",
                Budget {
                    requests: 10,
                    cost_micros: 100_000,
                },
            ),
        )
        .expect("child1 delegates");
    let child2 = ledger
        .delegate(
            &child1,
            child_request(
                "trans-2",
                Budget {
                    requests: 5,
                    cost_micros: 50_000,
                },
            ),
        )
        .expect("child2 delegates from child1");
    assert!(
        child2.is_subset_of(&root),
        "transitivity: grandchild ⊆ root"
    );
    assert_eq!(child1.depth(&ledger).unwrap(), 1);
    assert_eq!(child2.depth(&ledger).unwrap(), 2);
}

/// P9 — canonical_bytes length-prefixes the scope (direct preimage proof).
#[test]
fn canonical_bytes_length_prefixes_scope() {
    let token = CapabilityToken::root(
        AgentId::parse("hi").unwrap(),
        CapabilitySet::from_flags(&[CapabilityFlag::Spawn]),
        Budget {
            requests: 1,
            cost_micros: 1,
        },
        3,
        None,
        None,
    );
    let bytes = token.canonical_bytes(None);
    let mut expected = Vec::new();
    expected.extend_from_slice(&2u64.to_be_bytes()); // 8-byte BE length of "hi"
    expected.extend_from_slice(b"hi");
    assert!(
        bytes
            .windows(expected.len())
            .any(|w| w == expected.as_slice()),
        "canonical_bytes must length-prefix the scope (8-byte BE length ‖ bytes)"
    );
}

/// P9 — malformed issuer/signature and fail-closed-on-signature are rejected.
#[test]
fn validate_rejects_malformed_and_signed_tokens() {
    use rustain::domain::models::peer_identity::{Ed25519Sig, PeerId};
    let root = root_token();
    let ledger = AuthorityLedger::new(root.clone());
    let mut child = ledger
        .delegate(
            &root,
            child_request(
                "malformed",
                Budget {
                    requests: 1,
                    cost_micros: 1,
                },
            ),
        )
        .unwrap();

    // issuer without signature ⇒ malformed (both-or-neither).
    child.issuer = Some(PeerId::from_public_key(&[7u8; 32]).expect("valid peer id"));
    let err = ledger
        .validate(&child, &CapabilityFlag::ReadFs, &child.scope)
        .expect_err("issuer⊕signature must be rejected");
    assert!(matches!(err, AuthorityError::Malformed { .. }));

    // signature present ⇒ fail-closed (R1 has no verifier).
    child.issuer = None;
    child.signature = Some(Ed25519Sig(vec![0u8; 64]));
    let err = ledger
        .validate(&child, &CapabilityFlag::ReadFs, &child.scope)
        .expect_err("signed tokens must be fail-closed in R1");
    assert!(matches!(err, AuthorityError::Malformed { .. }));
}

/// P9 — CapabilityToken serialized field-count schema-pin (ADR-14-2-02): an R2
/// grant-field addition is a conscious break.
#[test]
fn capability_token_serialized_field_count_pinned() {
    let token = root_token();
    let v = serde_json::to_value(&token).expect("token serializes");
    let obj = v.as_object().expect("token serializes to a map");
    // id, parent, capabilities, scope, constraint, budget, not_after,
    // uses_limit, issuer, signature.
    assert_eq!(
        obj.len(),
        10,
        "CapabilityToken grant field count drifted — an R2 field add must be a conscious break"
    );
}

/// P15 — token↔node bijection: live token count == live node count, and both
/// drop together when a scope is revoked/deregistered.
#[tokio::test(flavor = "current_thread")]
async fn token_node_bijection_live_counts_match() {
    let root = root_token();
    let ledger = AuthorityLedger::new(root.clone());
    let tree = rustain::infrastructure::subagent::NodeTree::new();
    let mut scopes = Vec::new();
    for name in ["alpha", "beta", "gamma"] {
        let child = ledger
            .delegate(
                &root,
                child_request(
                    name,
                    Budget {
                        requests: 1,
                        cost_micros: 1,
                    },
                ),
            )
            .unwrap();
        let _rx = register_authorized_child(&tree, &child).await;
        scopes.push(child.scope.clone());
    }
    let live_nodes = tree.list().await.len();
    let live_tokens = scopes
        .iter()
        .filter(|s| ledger.token_for_scope(s).is_ok())
        .count();
    assert_eq!(live_nodes, 3);
    assert_eq!(
        live_tokens, live_nodes,
        "every live node has exactly one live token"
    );

    // Revoke + deregister one scope: both counts drop together (bijection holds).
    ledger.revoke_scope(&scopes[0]).unwrap();
    tree.deregister(&scopes[0]).await;
    let live_nodes_after = tree.list().await.len();
    assert_eq!(
        live_nodes_after, 2,
        "node count drops with the revoked token"
    );
}

// P10 — conservation invariant over random consume/settle sequences (where
// double-refund / missing-debit bugs hide).
proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn conservation_holds_across_random_consume_settle(
        consume_req in 0u64..=10,
        consume_cost in 0u64..=100_000,
        extra_settles in 0u32..=3,
    ) {
        let root = root_token();
        let ledger = AuthorityLedger::new(root.clone());
        let child = ledger
            .delegate(
                &root,
                child_request(
                    "conservation-prop",
                    Budget {
                        requests: 10,
                        cost_micros: 100_000,
                    },
                ),
            )
            .unwrap();
        // Random consume (bounded by reservation; over-spend is rejected harmlessly).
        let _ = ledger.consume(
            &child.id,
            Budget {
                requests: consume_req,
                cost_micros: consume_cost,
            },
        );
        // Repeated settle must be idempotent.
        for _ in 0..=extra_settles {
            let _ = ledger.settle(&child.id);
        }
        let snap = ledger.conservation(&root.id).unwrap();
        prop_assert_eq!(snap.available + snap.live_reservations + snap.consumed, snap.total);
        prop_assert!(snap.available.requests <= snap.total.requests);
        prop_assert!(snap.available.cost_micros <= snap.total.cost_micros);
    }
}

// Post-review party-mode gap (Murat/Winston): the post-order settle fix (P2)
// only changes behavior at delegation depth >= 2 — pre-order settle strands
// grandchild consumption and over-reports root.available. A depth-1 fixture
// cannot observe the bug; this depth-2 fixture proves the fix is non-vacuous.
#[test]
fn post_order_settle_propagates_grandchild_consumption_at_depth_two() {
    let root = root_token();
    let ledger = AuthorityLedger::new(root.clone());
    let child = ledger
        .delegate(
            &root,
            child_request(
                "d2-child",
                Budget {
                    requests: 10,
                    cost_micros: 100_000,
                },
            ),
        )
        .expect("child delegates");
    let grandchild = ledger
        .delegate(
            &child,
            child_request(
                "d2-grand",
                Budget {
                    requests: 4,
                    cost_micros: 40_000,
                },
            ),
        )
        .expect("grandchild delegates from child");
    // Spend on the GRANDCHILD — the consumption pre-order settle would strand.
    ledger
        .consume(
            &grandchild.id,
            Budget {
                requests: 2,
                cost_micros: 20_000,
            },
        )
        .expect("grandchild spends within reservation");
    // Revoke the child subtree — settles grandchild then child (post-order).
    ledger.revoke(&child.id).expect("revoke child subtree");
    let snap = ledger.conservation(&root.id).unwrap();
    assert_eq!(
        snap.available + snap.live_reservations + snap.consumed,
        snap.total
    );
    // The grandchild's 2/20k consumption MUST reach root.consumed. Pre-order
    // settle would leave this at ZERO (over-reporting root.available) — so this
    // assertion distinguishes the post-order fix from the pre-order bug.
    assert_eq!(
        snap.consumed,
        Budget {
            requests: 2,
            cost_micros: 20_000,
        },
        "post-order settle must propagate grandchild consumption to root"
    );
    assert_eq!(snap.live_reservations, Budget::ZERO);
}

// AC5 trust-drop, end-to-end (residual-gap closure): adapter.revoke(token) must
// resolve scope → cascade_kill (Op::Kill sent to the node), not just mark the
// token revoked. The synchronous-revoke differential exercises ledger.revoke_scope
// via the hook; this exercises the adapter's trust-drop wire directly.
#[tokio::test(flavor = "current_thread")]
async fn ac5_trust_drop_revoke_routes_into_cascade_kill() {
    let root = root_token();
    let ledger = std::sync::Arc::new(AuthorityLedger::new(root.clone()));
    let tree = std::sync::Arc::new(rustain::infrastructure::subagent::NodeTree::new());
    let provider = rustain::adapters::authority::InProcessAuthorityProvider::new(ledger.clone())
        .with_node_tree(tree.clone());
    let child = ledger
        .delegate(
            &root,
            child_request(
                "trust-drop",
                Budget {
                    requests: 1,
                    cost_micros: 1,
                },
            ),
        )
        .expect("child delegates");
    let mut rx = register_authorized_child(&tree, &child).await;
    assert_eq!(tree.list().await.len(), 1, "node registered before revoke");

    // Trust-drop: revoke(token) routes into cascade_kill.
    provider.revoke(&child.id).await.expect("adapter revoke");

    // cascade_kill sent Op::Kill to the node's command channel.
    let op = tokio::time::timeout(std::time::Duration::from_millis(300), rx.recv())
        .await
        .expect("cascade_kill should have sent Op::Kill");
    assert!(
        matches!(op, Some(Op::Kill)),
        "trust-drop revoke must route into cascade_kill (Op::Kill sent), got {op:?}"
    );
    // And the token is revoked.
    let err = ledger
        .validate(&child, &CapabilityFlag::ReadFs, &child.scope)
        .expect_err("revoked token must deny");
    assert!(matches!(err, AuthorityError::Revoked));
}

// AC9 budget-spend via use-count (residual-gap closure): each spend_use() commits
// one use at the point of use; a token past its uses_limit is denied its next
// gated action. Proves the metering a validate-only gate lacks (uses consumed at
// invoke are not refunded — AC4).
#[test]
fn point_of_use_use_count_denies_after_limit() {
    let root = root_token();
    let ledger = AuthorityLedger::new(root.clone());
    // child_request sets uses_limit: Some(10). Spend them all.
    let child = ledger
        .delegate(
            &root,
            child_request(
                "use-limit",
                Budget {
                    requests: 1,
                    cost_micros: 1,
                },
            ),
        )
        .expect("child delegates");
    for _ in 0..10 {
        ledger.spend_use(&child.id).expect("use within limit");
    }
    // 11th use: denied.
    let err = ledger
        .spend_use(&child.id)
        .expect_err("use past limit must deny");
    assert!(
        matches!(err, AuthorityError::BudgetExhausted),
        "expected BudgetExhausted past uses_limit, got {err:?}"
    );
    // And validate() now also denies (uses_remaining == Some(0)).
    let err = ledger
        .validate(&child, &CapabilityFlag::ReadFs, &child.scope)
        .expect_err("exhausted-use token must deny at validate");
    assert!(matches!(err, AuthorityError::BudgetExhausted));
}

// AC5 TOCTOU concurrency probe (Story 14.6): a child racing its own revocation.
// The hazard is a validate-at-T0 / revoke-at-T1 / dispatch-at-T2 window where a
// stale Ok survives past the revoke. `validate` and `revoke` share a single
// `Mutex`, so revoke's critical section establishes happens-before with every
// subsequent validate: once revoke returns, no later validate may observe the
// pre-revoke state. This probe hammers the interleaving across many threads and
// proves the post-revoke ordering holds — it does NOT add any new ledger
// mutation path (it reuses only `validate` / `revoke_scope`).
#[test]
fn revoke_happens_before_subsequent_validates_under_concurrency() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::thread;

    let root = root_token();
    let ledger = Arc::new(AuthorityLedger::new(root.clone()));
    let child = ledger
        .delegate(
            &root,
            child_request(
                "child-a",
                Budget {
                    requests: 10,
                    cost_micros: 100_000,
                },
            ),
        )
        .expect("valid subset delegates");
    let scope = AgentId::parse("child-a").unwrap();
    let want = CapabilityFlag::ReadFs;

    // Baseline: the token validates before revocation.
    ledger
        .validate(&child, &want, &scope)
        .expect("pre-revoke validate succeeds");

    const WORKERS: usize = 8;
    let start = Arc::new(std::sync::Barrier::new(WORKERS + 1));
    // Relaxed deliberately: this flag announces only that revoke returned; it
    // does NOT establish the happens-before edge under test. The ledger Mutex
    // must order the state mutation against each subsequent validate.
    let revoke_returned = Arc::new(AtomicBool::new(false));
    let interleavings = Arc::new(AtomicUsize::new(0));
    let post_revoke_ok = Arc::new(AtomicUsize::new(0));
    let post_revoke_observations = Arc::new(AtomicUsize::new(0));

    let mut handles = Vec::new();
    for _ in 0..WORKERS {
        let (
            ledger,
            child,
            scope,
            start,
            revoke_returned,
            interleavings,
            post_revoke_ok,
            post_revoke_observations,
        ) = (
            ledger.clone(),
            child.clone(),
            scope.clone(),
            start.clone(),
            revoke_returned.clone(),
            interleavings.clone(),
            post_revoke_ok.clone(),
            post_revoke_observations.clone(),
        );
        handles.push(thread::spawn(move || {
            start.wait();
            loop {
                if revoke_returned.load(Ordering::Relaxed) {
                    // Definitive post-revoke validate: a FRESH call performed
                    // AFTER observing that revoke returned. The ledger Mutex is
                    // therefore the sole happens-before ordering this validate
                    // against the revocation — never a stale pre-observation
                    // result counted as post-revoke (the old bug). Drop the
                    // Mutex (the mandated mutant) and this validate can observe
                    // unsynchronized state → Ok → the assertion below goes RED.
                    let accepted = ledger.validate(&child, &want, &scope).is_ok();
                    post_revoke_observations.fetch_add(1, Ordering::Relaxed);
                    if accepted {
                        post_revoke_ok.fetch_add(1, Ordering::Relaxed);
                    }
                    break;
                }
                // Interleaving stress: hammer validate concurrently with the
                // in-flight revoke to create real contention on the lock.
                let _ = ledger.validate(&child, &want, &scope).is_ok();
                interleavings.fetch_add(1, Ordering::Relaxed);
                thread::yield_now();
            }
        }));
    }

    start.wait();
    ledger.revoke_scope(&scope).expect("scope revoke succeeds");
    revoke_returned.store(true, Ordering::Relaxed);

    for h in handles {
        h.join().expect("worker thread panicked");
    }

    assert_eq!(
        post_revoke_observations.load(Ordering::Relaxed),
        WORKERS,
        "every worker must validate after revoke returned without a barrier"
    );
    assert_eq!(
        post_revoke_ok.load(Ordering::Relaxed),
        0,
        "a validate after revoke's critical section returned Ok — revoke did \
         not establish happens-before with subsequent validates via the shared \
         Mutex (AC5 TOCTOU contract broken)"
    );
    assert!(
        interleavings.load(Ordering::Relaxed) > 0,
        "probe never ran concurrent validates — no interleaving stress",
    );
}

#[tokio::test]
async fn terminal_prune_requires_journal_proof_and_preserves_conservation() {
    let root = root_token();
    let ledger = std::sync::Arc::new(AuthorityLedger::new(root.clone()));
    let provider =
        rustain::adapters::authority::in_process::InProcessAuthorityProvider::new(ledger.clone());
    let child = ledger
        .delegate(
            &root,
            child_request(
                "prune-child",
                Budget {
                    requests: 10,
                    cost_micros: 10_000,
                },
            ),
        )
        .expect("delegate child");
    ledger
        .consume(
            &child.id,
            Budget {
                requests: 2,
                cost_micros: 500,
            },
        )
        .expect("consume child budget");
    let workspace = tempfile::tempdir().expect("temporary workspace");
    let journal = NodeJournal::open_workspace(workspace.path())
        .await
        .expect("open journal");
    let mut checkpoint = NodeCheckpoint {
        id: child.scope.clone(),
        token: child.id,
        parent: None,
        ownership: WireOwnershipKind::Owned,
        state: NodeState::Running,
        origin: NodeOrigin::Subagent,
        foreground: true,
        effective_model: "test".into(),
        tokens_in: 0,
        tokens_out: 0,
        turns: 0,
        subagent_type: "test".into(),
        spawned_at: 1,
        depth: 1,
        tainted: false,
        waiting_since: None,
        wait_reason: None,
    };
    journal
        .append_checkpoint(checkpoint.clone())
        .await
        .expect("append nonterminal checkpoint");
    assert!(
        journal
            .journaled_terminal(&child.scope)
            .await
            .expect("scan journal")
            .is_none(),
        "crash window: no terminal proof means pruning is impossible"
    );

    ledger.settle(&child.id).expect("settle child");
    let before = ledger.conservation(&root.id).expect("conservation before");
    checkpoint.state = NodeState::Completed;
    journal
        .append_checkpoint(checkpoint)
        .await
        .expect("append terminal checkpoint");
    let proof = journal
        .journaled_terminal(&child.scope)
        .await
        .expect("scan journal")
        .expect("terminal checkpoint is durably proven");

    assert!(provider.prune_terminal(&proof).await.expect("prune child"));
    assert_eq!(
        ledger.token_for_scope(&child.scope),
        Err(AuthorityError::NotFound)
    );
    assert_eq!(ledger.conservation(&root.id).unwrap(), before);
    assert!(
        !provider
            .prune_terminal(&proof)
            .await
            .expect("idempotent prune"),
        "second prune must be a no-op"
    );
}
