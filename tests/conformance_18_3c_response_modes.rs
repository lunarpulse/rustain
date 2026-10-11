//! Story 18.3c — production response-mode routing and load-boundary controls.

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
