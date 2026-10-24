use std::time::Duration;

use super::harness::{Host, Journal, Sender};
use super::k07_k08_recipient::local_view;
use rustain::adapters::a2a::admission::A2aAdmissionPolicy;
use rustain::adapters::tui::state::TuiState;
use rustain::domain::models::{
    Authorship, CapabilityId, JournalEntry, JournalRecord, PermissionResolution, ResponseMode,
    RoomEvent, SendAuthorization,
};
use rustain::domain::services::transparency::transparency_row;
use serde_json::json;
use tokio_util::sync::CancellationToken;

const SETTLE: Duration = Duration::from_secs(45);
const ALIAS: &str = "act5-recipient";

async fn received_item(journal: &Journal, body: &str) -> JournalEntry {
    tokio::time::timeout(SETTLE, async {
        loop {
            if let Some(entry) = journal.entries().await.into_iter().find(|entry| {
                matches!(
                    &entry.record,
                    JournalRecord::Room(RoomEvent::RecipientItemReceived { content, .. })
                        if content == body
                )
            }) {
                return entry;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("real send journals the requested recipient item")
}

async fn delegate(sender: &Sender, resolution: PermissionResolution, body: &str) {
    let result = tokio::time::timeout(
        SETTLE,
        sender.egress.provider().invoke_with_permission_resolution(
            &CapabilityId {
                protocol: "a2a".to_owned(),
                server: ALIAS.to_owned(),
                tool: "delegate".to_owned(),
            },
            json!({"message": body}),
            CancellationToken::new(),
            Some(resolution),
        ),
    )
    .await
    .expect("NotifyAndAuto delegation completes")
    .expect("delegation succeeds");
    assert!(!result.is_error, "{}", result.content);
}

async fn assert_carrier(
    sender: &Sender,
    journal: &Journal,
    received: &JournalEntry,
    expected_authorship: Authorship,
    expected_authorization: SendAuthorization,
    tags: &str,
) -> String {
    let JournalRecord::Room(RoomEvent::RecipientItemReceived {
        address,
        task,
        authorship,
        send_authorization,
        ..
    }) = &received.record
    else {
        panic!("expected a real received item")
    };
    assert_eq!(*authorship, expected_authorship);
    assert_eq!(*send_authorization, expected_authorization);
    let entries = sender.journal.entries().await;
    let dispatches: Vec<_> = entries
        .iter()
        .filter_map(|entry| match &entry.record {
            JournalRecord::Room(RoomEvent::RemoteEnvelopeDispatched {
                task: Some(dispatched_task),
                authorship,
                send_authorization,
                ..
            }) if dispatched_task == task => Some((*authorship, *send_authorization)),
            _ => None,
        })
        .collect();
    assert_eq!(
        dispatches,
        vec![(*authorship, *send_authorization)],
        "same task has exactly one real outbound dispatch with the received facts"
    );
    assert_surfaces(journal, received, tags).await;
    address.item().as_str().to_owned()
}

async fn assert_surfaces(journal: &Journal, received: &JournalEntry, tags: &str) {
    let JournalRecord::Room(RoomEvent::RecipientItemReceived { address, .. }) = &received.record
    else {
        panic!("expected a real received item")
    };
    let row = transparency_row(received).expect("received item has a production log row");
    assert!(row.summary.ends_with(tags), "{}", row.summary);
    assert_eq!(
        row.summary.contains("[agent-composed]"),
        tags.contains("[agent-composed]")
    );
    assert_eq!(
        row.summary.contains("[auto-sent]"),
        tags.contains("[auto-sent]")
    );
    let state = local_view(journal.workspace.path(), address.item().as_str()).await;
    let surface = state
        .recipient_surface
        .expect("production /team view opens the real received item");
    let expected = format!("  {} · received{tags}", address.item().as_str());
    assert!(
        surface.content.lines().any(|line| line == expected),
        "View must show exactly the received facts on its item turn: {}",
        surface.content
    );
}

fn strip_received_provenance(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(fields) => {
            if fields.get("event").and_then(serde_json::Value::as_str)
                == Some("recipient_item_received")
            {
                fields.remove("authorship");
                fields.remove("send_authorization");
            }
            for child in fields.values_mut() {
                strip_received_provenance(child);
            }
        }
        serde_json::Value::Array(values) => {
            for child in values {
                strip_received_provenance(child);
            }
        }
        _ => {}
    }
}

/// Story 19.20 K01: production delegation and typed-send front doors carry the
/// same facts as their dispatch records; legacy replay and retract preserve the
/// distinction on journal-derived local log and View surfaces.
#[tokio::test]
async fn act5_k01_policy_delegation_carries_facts_and_log_marks() {
    let host = Host::start(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndAuto).await;
    let mut sender = Sender::start(vec![host.peer(ALIAS)]).await;
    let policy_body = "Story 19.20 Policy carrier";
    delegate(&sender, PermissionResolution::Policy, policy_body).await;
    let policy = received_item(&host.journal, policy_body).await;
    let policy_item = assert_carrier(
        &sender,
        &host.journal,
        &policy,
        Authorship::AgentComposed,
        SendAuthorization::Automatic,
        " [agent-composed] [auto-sent]",
    )
    .await;

    let operator_body = "Story 19.20 Operator carrier";
    delegate(&sender, PermissionResolution::Operator, operator_body).await;
    let operator = received_item(&host.journal, operator_body).await;
    assert_carrier(
        &sender,
        &host.journal,
        &operator,
        Authorship::AgentComposed,
        SendAuthorization::HumanDecided,
        " [agent-composed]",
    )
    .await;

    let typed_body = "Story 19.20 typed carrier control";
    let mut state = TuiState::new(130, 30);
    sender
        .command(&mut state, &format!("/team send {ALIAS} {typed_body}"))
        .await;
    sender.settle(&mut state, 1).await;
    let typed = received_item(&host.journal, typed_body).await;
    assert_carrier(
        &sender,
        &host.journal,
        &typed,
        Authorship::HumanWritten,
        SendAuthorization::HumanDecided,
        "",
    )
    .await;

    // Replay an actual journal prefix as an old build would have written it.
    // No received event or item is synthesized: only the two additive keys on
    // real received lines are removed before the production reader and fold.
    let legacy = Journal::open().await;
    let mut bytes = Vec::new();
    // Keep physical batch lines and their sequence numbers intact.
    let prefix = tokio::fs::read_to_string(host.journal.node.path())
        .await
        .expect("read actual journal prefix for legacy replay");
    for raw_line in prefix.lines() {
        let mut line: serde_json::Value =
            serde_json::from_str(raw_line).expect("parse actual journal line");
        strip_received_provenance(&mut line);
        serde_json::to_writer(&mut bytes, &line).expect("serialize legacy journal line");
        bytes.push(b'\n');
    }
    tokio::fs::write(legacy.node.path(), &bytes)
        .await
        .expect("write legacy replay journal");
    let legacy_policy = received_item(&legacy, policy_body).await;
    let JournalRecord::Room(RoomEvent::RecipientItemReceived {
        authorship,
        send_authorization,
        ..
    }) = &legacy_policy.record
    else {
        unreachable!()
    };
    assert_eq!(*authorship, Authorship::Unknown);
    assert_eq!(*send_authorization, SendAuthorization::Unknown);
    assert_surfaces(&legacy, &legacy_policy, "").await;

    sender
        .raise_retract_card(&mut state, &format!("/team retract {ALIAS} {policy_item}"))
        .await;
    let message = sender
        .confirm_retract(&mut state)
        .await
        .expect("Policy item's retract card confirms through the real sender front door");
    assert!(message.contains("Retracted on"), "{message}");
    let retracted = tokio::time::timeout(SETTLE, async {
        loop {
            if let Some(entry) = host.journal.entries().await.into_iter().find(|entry| {
                matches!(
                    &entry.record,
                    JournalRecord::Room(RoomEvent::RecipientItemRetracted { address, .. })
                        if address.item().as_str() == policy_item
                )
            }) {
                break entry;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("cross-host retract is durable on the recipient");
    let row = transparency_row(&retracted).expect("retract has a production log row");
    assert_eq!(row.kind.label(), "item-retracted");
    let view = local_view(host.journal.workspace.path(), &policy_item).await;
    let surface = view.recipient_surface.expect("retracted item still opens");
    let expected =
        format!("  {policy_item} · received [agent-composed] [auto-sent] · retracted by sender");
    assert!(
        surface.content.lines().any(|line| line == expected),
        "retract must retain both sender-reported tags: {}",
        surface.content
    );
    host.stop().await;
}
