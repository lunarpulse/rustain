//! Story 18.3c — response-mode production wiring and boundary ratchets.
//!
//! Behavioral contracts live beside their decision cores; these tests pin the
//! production call graph so a green unit test cannot coexist with an unwired
//! seam.

fn source(path: &str) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|error| panic!("read {path}: {error}"))
}

/// Serializes the two tests that read the **process-global** workspace-policy
/// load counter against the ones that increment it.
///
/// 🔴 `workspace_policy_load_count()` is a process-wide static, and libtest runs
/// every test in this binary concurrently on one process. Without this gate
/// `ac6_delivery_decisions_never_reload_workspace_policy` reads the loads
/// performed by `ac1_pinned_sender_mode_routes_through_bus_and_unpinned_fails_closed`
/// and fails with `left: 2, right: 1` — measured red 3/3 in the default lane and
/// 2/6 in the a2a lane at `02d6c46`. ⛔ Do not "fix" it by relaxing the
/// assertion to `<= 2` or by deleting the reset: the exact-`1` count IS the
/// ratchet (a delivery-time `load()` reads 2 as well), and a range would let
/// the mutant it exists to catch pass. Serialize instead.
fn policy_load_gate() -> std::sync::MutexGuard<'static, ()> {
    static GATE: std::sync::Mutex<()> = std::sync::Mutex::new(());
    // A panicking test must not cascade into an unrelated poisoned-lock failure.
    GATE.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn write_policy(workspace: &std::path::Path, body: &str) {
    let dir = workspace.join(".rustain");
    std::fs::create_dir_all(&dir).expect("create policy directory");
    std::fs::write(dir.join("a2a-interaction.toml"), body).expect("write interaction policy");
}

fn pinned_peer(alias: &str, key: [u8; 32]) -> rustain::domain::models::A2aPeerSpec {
    use base64::Engine as _;
    use rustain::domain::models::{A2aPeerSource, PinnedKey, PinnedKeyAlgorithm, RedactedUrl};

    rustain::domain::models::A2aPeerSpec::new(
        alias,
        RedactedUrl::new(format!("https://{alias}.example/a2a")),
        A2aPeerSource::Workspace,
    )
    .with_pinned_key(Some(PinnedKey::new(
        PinnedKeyAlgorithm::EdDsa,
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(key),
        None,
    )))
}

fn unpinned_peer(alias: &str) -> rustain::domain::models::A2aPeerSpec {
    use rustain::domain::models::{A2aPeerSource, RedactedUrl};

    rustain::domain::models::A2aPeerSpec::new(
        alias,
        RedactedUrl::new(format!("https://{alias}.example/a2a")),
        A2aPeerSource::Workspace,
    )
}

fn resolved_delivery_policy(
    workspace: &std::path::Path,
    response_mode: &str,
    auto_response: Option<&str>,
    peers: &[rustain::domain::models::A2aPeerSpec],
) -> (
    std::sync::Arc<rustain::domain::models::EffectivePolicy>,
    rustain::domain::ports::EffectiveDeliveryPolicy,
) {
    let auto_response = auto_response
        .map(|response| format!("auto_response = \"{response}\"\n"))
        .unwrap_or_default();
    write_policy(
        workspace,
        &format!(
            "[interaction.defaults]\nresponse_mode = \"notify-and-wait\"\n\
             \n[interaction.overrides.\"trusted\"]\n\
             response_mode = \"{response_mode}\"\n\
             {auto_response}\
             \n[interaction.overrides.\"untrusted\"]\n\
             response_mode = \"notify-and-auto\"\n\
             auto_response = \"untrusted template\"\n"
        ),
    );
    let (effective, _) = rustain::adapters::policy::resolve_workspace_policy(
        workspace,
        peers,
        &rustain::adapters::policy::EmptyConsentProjection,
    )
    .expect("resolve workspace policy");
    let effective = std::sync::Arc::new(effective);
    (
        effective.clone(),
        rustain::domain::ports::EffectiveDeliveryPolicy::new(effective),
    )
}

#[derive(Debug, PartialEq, Eq)]
enum ResponseRoute {
    Parked,
    AutoDispatched(String),
}

/// Drive the production local bus to a real registered peer node, then observe
/// the policy answer at its receiving command channel. The receiver is the
/// consumer boundary: it parks a wait delivery and dispatches an auto template.
async fn drive_bus_consumer(
    policy: std::sync::Arc<dyn rustain::domain::ports::DeliveryPolicy>,
    sender_peer_id: rustain::domain::models::PeerId,
    deliveries: usize,
) -> Vec<ResponseRoute> {
    use rustain::domain::models::{
        AgentId, AgentMessage, AgentMetrics, CapabilityTokenId, CorrelationId, Envelope,
        MessageHeader, MessageKind, NodeState, Op,
    };
    use rustain::domain::ports::AgentMessageBus;
    use rustain::infrastructure::subagent::{
        AgentHandle, LocalMessageBus, MailboxBudget, NodeTree,
    };
    use tokio::sync::{mpsc, watch};

    let node_tree = NodeTree::new();
    let recipient = AgentId::parse("response-consumer").expect("recipient");
    let (command_tx, mut command_rx) = mpsc::channel(deliveries.max(1));
    let (status_tx, _) = watch::channel(NodeState::Created);
    let (_, metrics_rx) = watch::channel(AgentMetrics::default());
    node_tree
        .register_peer(
            recipient.clone(),
            AgentHandle {
                agent_id: recipient.clone(),
                token: CapabilityTokenId::nil(),
                command_tx,
                cancel_token: tokio_util::sync::CancellationToken::new(),
                depth: 0,
                subagent_type: "response-mode-consumer".to_owned(),
                spawned_at: 0,
                status: status_tx,
                metrics: metrics_rx,
                isolated: false,
                mailbox_budget: MailboxBudget::new(),
            },
        )
        .await
        .expect("register consumer");
    let bus = LocalMessageBus::new(node_tree, policy);
    let sender = AgentId::parse("verified-sender").expect("sender");
    let mut routes = Vec::with_capacity(deliveries);

    for sequence in 0..deliveries {
        bus.deliver(
            &recipient,
            Envelope {
                header: MessageHeader {
                    message_type: rustain::domain::models::SemanticMessageType::Unknown,
                    sender: sender.clone(),
                    recipient: recipient.clone(),
                    correlation_id: CorrelationId::new(format!("response-mode-{sequence}")),
                    kind: MessageKind::PeerMessage,
                    sequence: None,
                    verified_peer_id: Some(sender_peer_id.clone()),
                },
                body: AgentMessage::new("peer request"),
            },
        )
        .await
        .expect("bus admits peer delivery");
        let Op::Deliver(delivery) = command_rx.recv().await.expect("consumer receives delivery")
        else {
            panic!("only delivery commands reach the response consumer");
        };
        match delivery.response_policy.mode {
            rustain::domain::models::ResponseMode::NotifyAndWait => {
                routes.push(ResponseRoute::Parked);
            }
            rustain::domain::models::ResponseMode::NotifyAndAuto => {
                routes.push(ResponseRoute::AutoDispatched(
                    delivery
                        .response_policy
                        .auto_response
                        .expect("auto delivery carries its configured template"),
                ));
            }
            mode => panic!("unexpected response mode at consumer: {mode:?}"),
        }
    }
    routes
}

#[test]
fn ac1_production_uses_verified_sender_effective_policy_at_the_real_bus() {
    let composition = source("src/adapters/daemon/mod.rs");
    let policy = source("src/domain/ports/agent_message_bus.rs");
    let delivery = source("src/adapters/rap/peer_delivery.rs");
    assert!(composition.contains("EffectiveDeliveryPolicy::new"));
    assert!(composition.contains("peer_bus_slot_with_policy"));
    assert!(composition.contains("new_with_node_tree_bus_policy_journal_and_urgency"));
    assert!(policy.contains("sender_policy_for(&self.policy, peer_id, message_type)"));
    assert!(policy.contains("relationship_disposition(recipient_ownership)"));
    assert!(delivery.contains("local.header.verified_peer_id = Some(peer_id)"));
    assert!(delivery.contains("ingest_with_policy"));
}

#[test]
fn ac1_resolved_message_types_remove_the_stale_startup_notice() {
    let startup = source("src/adapters/daemon/policy_startup.rs");
    for stale in [
        concat!("MESSAGE_TYPE_", "DEFERRAL_NOTICE"),
        concat!("semantic message type is ", "not carried"),
        concat!("per-sender response mode is ", "enforced"),
    ] {
        assert!(
            !startup.contains(stale),
            "stale startup claim remains: {stale}"
        );
    }
    // Presence half (`A25`): a deleted banner and a never-printing banner both pass
    // the absence loop, so absence alone cannot kill the no-op mutant this ratchet
    // guards — the NFR66-shaped replacement must still be in the guarded source.
    assert!(
        startup.contains("STARTUP_BANNER") && startup.contains("interaction policy resolved"),
        "the resolved-policy startup banner must remain in policy_startup.rs"
    );
}

/// [K1] A pinned sender's workspace override must survive the real local-bus
/// delivery to its consumer: wait parks, while auto dispatches the configured
/// template. This kills a `response_policy_for_peer` mutant that ignores the
/// resolved mode and proves an unpinned identity cannot borrow another sender's
/// override.
#[tokio::test]
async fn ac1_pinned_sender_mode_routes_through_bus_and_unpinned_fails_closed() {
    let trusted = pinned_peer("trusted", [7; 32]);
    let untrusted = unpinned_peer("untrusted");
    let trusted_peer_id = trusted.pinned_identity().expect("trusted pin");
    let untrusted_peer_id = untrusted.resolved_identity();
    let peers = [trusted, untrusted];

    let wait_workspace = tempfile::tempdir().expect("wait workspace");
    let (_, wait_policy) = {
        let _gate = policy_load_gate();
        resolved_delivery_policy(wait_workspace.path(), "notify-and-wait", None, &peers)
    };
    assert_eq!(
        drive_bus_consumer(std::sync::Arc::new(wait_policy), trusted_peer_id.clone(), 1).await,
        vec![ResponseRoute::Parked],
        "the same verified sender parks when its resolved override says wait"
    );

    let auto_workspace = tempfile::tempdir().expect("auto workspace");
    let (effective, auto_policy) = {
        let _gate = policy_load_gate();
        resolved_delivery_policy(
            auto_workspace.path(),
            "notify-and-auto",
            Some("Pinned sender acknowledgement."),
            &peers,
        )
    };
    assert_eq!(
        drive_bus_consumer(std::sync::Arc::new(auto_policy.clone()), trusted_peer_id, 1).await,
        vec![ResponseRoute::AutoDispatched(
            "Pinned sender acknowledgement.".to_owned()
        )],
        "the same verified sender dispatches its resolved auto template"
    );
    assert_eq!(
        drive_bus_consumer(std::sync::Arc::new(auto_policy), untrusted_peer_id, 1).await,
        vec![match effective.automation.value {
            rustain::domain::models::ResponseMode::NotifyAndWait => ResponseRoute::Parked,
            rustain::domain::models::ResponseMode::NotifyAndAuto => {
                panic!("the fixture's effective default must remain wait")
            }
            mode => panic!("unexpected effective default mode: {mode:?}"),
        }],
        "an unpinned sender must receive EffectivePolicy.automation, never another sender's override"
    );
}

#[test]
fn ac2_wait_transition_precedes_stamp_and_projects_auth_required() {
    let server = source("src/adapters/daemon/server.rs");
    let start_impl = server
        .split("impl crate::domain::ports::InboundPeerRuntime for AttachServer")
        .nth(1)
        .expect("InboundPeerRuntime start implementation exists");
    let wait_arm = start_impl
        .split("match task.response_policy.mode")
        .nth(1)
        .expect("start dispatches by response mode")
        .split("crate::domain::models::ResponseMode::NotifyAndWait")
        .nth(1)
        .expect("start wait arm exists")
        .split("crate::domain::models::ResponseMode::NotifyAndAuto")
        .next()
        .expect("start wait arm is bounded");
    let running = wait_arm
        .find("NodeState::Running")
        .expect("running transition");
    let waiting = wait_arm
        .find("NodeState::Waiting")
        .expect("waiting transition");
    let stamp = wait_arm
        .find("WaitReason::AwaitingPeerResponse")
        .expect("wait reason stamp");
    assert!(
        running < waiting && waiting < stamp,
        "the A2A start wait arm must run, park, then stamp the wait reason"
    );

    let projection = source("src/adapters/a2a/exec.rs");
    assert!(projection.contains("NodeState::Waiting => RapTaskState::AuthRequired"));
    let panel = source("src/adapters/tui/widgets/agent_panel.rs");
    assert!(panel.contains("awaiting your decision"));
}

#[test]
fn ac3_draft_path_buffers_chunks_and_requires_trusted_local_resolution() {
    let server = source("src/adapters/daemon/server.rs");
    assert!(server.contains("PendingDraftController"));
    assert!(server.contains("if !buffer_response || !buffered_chunk"));
    assert!(server.contains("ResponseMode::NotifyAndDraft"));
    assert!(server.contains("ClientFrame::ResolvePeerDraft"));
    assert!(server.contains("tier != ConnectionTier::TrustedLocal"));
    assert!(server.contains("RoomEvent::PeerDraftResolved"));
    // AC3 keystone (b): the REAL operator dispatch arm — the attached client's
    // decision grammar sends the frame; a rendered-only card is the false
    // green this pins against.
    let attach = source("src/infrastructure/runtime/attach_loop.rs");
    assert!(attach.contains("ClientFrame::ResolvePeerDraft"));
    assert!(attach.contains("PeerDraftAction::Approve"));
    assert!(attach.contains("PeerDraftAction::Edit"));
    assert!(attach.contains("PeerDraftAction::WriteOwn"));
    assert!(attach.contains("pending_peer_response(&conversation)"));
    assert!(attach.contains("peer_draft_edit_node"));
}

#[test]
fn ac4_auto_deadline_marker_and_authority_warning_are_live() {
    let modes = source("src/adapters/daemon/response_modes.rs");
    assert!(modes.contains("AUTO_RESPONSE_VISIBLE_DEADLINE_MS: i64 = 1_000"));
    // The deadline is checked BEFORE any visibility flag is honoured — a row
    // first visible after 1000ms is a miss, so the production call sites
    // passing `true` cannot make the check vacuous.
    assert!(modes.contains("elapsed_ms >= AUTO_RESPONSE_VISIBLE_DEADLINE_MS"));
    let server = source("src/adapters/daemon/server.rs");
    assert!(server.contains("DRAFTING_PLACEHOLDER"));
    assert!(server.contains("MessageAuthorship::AgentComposed"));
    assert!(server.contains("auto_response_surface("));
    let startup = source("src/adapters/daemon/policy_startup.rs");
    assert!(startup.contains("AUTO_AUTHORITY_WARNING"));
    assert!(startup.contains("A2aAdmissionPolicy::Allow"));
    assert!(startup.contains("ResponseMode::NotifyAndAuto"));
}

#[test]
fn ac5_retraction_is_append_only_stateful_and_same_host_only() {
    let room = source("src/domain/models/orchestration_room.rs");
    assert!(room.contains("AutoResponseRetracted"));
    assert!(room.contains("target_seq: u64"));
    let fold = source("src/domain/services/transparency.rs");
    assert!(fold.contains("HashMap::<u64, usize>"));
    assert!(!fold.contains("entries.into_iter().filter_map(transparency_row).collect()"));
    assert!(fold.contains("rows[index].retracted_at_ms.is_none()"));
    let server = source("src/adapters/daemon/server.rs");
    assert!(server.contains("ClientFrame::RetractAutoResponse"));
    assert!(server.contains("retraction is same-host trusted-local only"));
    // AC5's production caller: the focused-row retract action on the attached
    // client, confirmation copy included.
    let attach = source("src/infrastructure/runtime/attach_loop.rs");
    assert!(attach.contains("ClientFrame::RetractAutoResponse"));
    assert!(attach.contains("latest_retractable(&conversation)"));
    assert!(attach.contains("Retracted. Marked in your log — never deleted."));
    assert!(attach.contains("What was already read, was read."));
    let action = server
        .split("async fn retract_auto_response(")
        .nth(1)
        .expect("retraction dispatcher exists")
        .split("/// Returns `true`")
        .next()
        .expect("dispatcher is bounded");
    assert!(
        action
            .find("record_event(plan.event)")
            .expect("journal append")
            < action
                .find("std::mem::replace(&mut conversation.messages[index], plan.message)")
                .expect("row mark"),
        "the journal append must precede the persisted row mark"
    );
}

#[test]
fn ac6_ci_executes_the_story_target_in_default_and_a2a_lanes() {
    let ci = source(".github/workflows/ci.yml");
    let default_lane = ci
        .split("\n  check:\n")
        .nth(1)
        .expect("default check lane exists")
        .split("\n  skills-validation:\n")
        .next()
        .expect("default check lane is bounded");
    assert!(
        default_lane.contains(
            "cargo test --features test-instrumentation --test conformance_18_3c_response_modes"
        ),
        "the default lane must activate the zero-load instrumentation"
    );
    let a2a_lane = ci
        .split("\n  a2a:\n")
        .nth(1)
        .expect("A2A lane exists")
        .split("\n  mcp:\n")
        .next()
        .expect("A2A lane is bounded");
    assert!(
        a2a_lane.contains("--test conformance_18_3c_response_modes"),
        "the A2A lane must execute this story target"
    );
}

#[cfg(feature = "test-instrumentation")]
#[tokio::test]
async fn ac6_delivery_decisions_never_reload_workspace_policy() {
    use std::sync::Arc;

    use rustain::domain::models::{
        AgentId, CorrelationId, MessageHeader, MessageKind, OwnershipKind,
    };
    use rustain::domain::ports::DeliveryPolicy;
    use rustain::infrastructure::subagent::NodeJournal;

    let workspace = tempfile::tempdir().expect("workspace");
    let trusted = pinned_peer("trusted", [19; 32]);
    let trusted_peer_id = trusted.pinned_identity().expect("trusted pin");
    // The whole reset→assert window is synchronous and holds the gate, so no
    // concurrent test in this binary can add a load between them.
    let policy = {
        let _gate = policy_load_gate();
        rustain::adapters::policy::reset_workspace_policy_load_count();
        let (_, policy) = resolved_delivery_policy(
            workspace.path(),
            "notify-and-auto",
            Some("Zero-load acknowledgement."),
            &[trusted],
        );
        assert_eq!(rustain::adapters::policy::workspace_policy_load_count(), 1);

        let header = MessageHeader {
            message_type: rustain::domain::models::SemanticMessageType::Unknown,
            sender: AgentId::parse("peer-agent").expect("sender"),
            recipient: AgentId::parse("recipient").expect("recipient"),
            correlation_id: CorrelationId::new("load-ratchet"),
            kind: MessageKind::PeerMessage,
            sequence: None,
            verified_peer_id: Some(trusted_peer_id.clone()),
        };
        for _ in 0..32 {
            let _ = policy.response_policy(&header);
            let _ = policy.decide(&header, OwnershipKind::Peer);
        }
        assert_eq!(
            rustain::adapters::policy::workspace_policy_load_count(),
            1,
            "delivery-time decisions must use the startup snapshot without calling load()"
        );
        policy
    };

    let journal = NodeJournal::open_workspace(workspace.path())
        .await
        .expect("open journal");
    NodeJournal::reset_load_count();
    let _ = journal.load().await.expect("load journal");
    assert_eq!(
        NodeJournal::load_count(),
        1,
        "the instrumentation must observe an actual NodeJournal::load()"
    );
    NodeJournal::reset_load_count();
    let initial_journal_loads = NodeJournal::load_count();
    let routes = drive_bus_consumer(Arc::new(policy), trusted_peer_id, 32).await;
    assert_eq!(routes.len(), 32, "every delivery must reach the consumer");
    assert_eq!(
        NodeJournal::load_count(),
        initial_journal_loads,
        "the per-message delivery path must never call NodeJournal::load()"
    );
}
