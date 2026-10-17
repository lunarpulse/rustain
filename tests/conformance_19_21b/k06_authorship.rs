use std::time::Duration;

use rustain::adapters::a2a::admission::A2aAdmissionPolicy;
use rustain::adapters::tui::state::TuiState;
use rustain::domain::models::{JournalRecord, ReplyAuthorship, ResponseMode, RoomEvent};
use rustain::domain::ports::CapabilityProvider;
use rustain::infrastructure::subagent::NodeJournal;
use serde_json::json;
use tokio_util::sync::CancellationToken;

use crate::inherited::{Host, Sender, read_room_events};

const SETTLE: Duration = Duration::from_secs(45);

#[tokio::test]
async fn k06_production_node_journal_replays_legacy_and_future_dispatch_authorship() {
    let workspace = tempfile::tempdir().expect("workspace");
    let journal = NodeJournal::open_workspace(workspace.path())
        .await
        .expect("production journal");
    journal
        .append_room(RoomEvent::RemoteEnvelopeDispatched {
            peer: rustain::domain::models::PeerId::from_public_key(&[61; 32]).expect("peer id"),
            task: Some("rustain-legacy-authorship".to_owned()),
            bytes: 4,
            act: rustain::domain::models::DispatchAct::Task,
            content_hash: None,
            endpoint: None,
            action: None,
            authorship: ReplyAuthorship::Unknown,
        })
        .await
        .expect("write a legacy-shape row through NodeJournal");

    let raw = tokio::fs::read_to_string(journal.path())
        .await
        .expect("read real journal bytes");
    let mut row: serde_json::Value =
        serde_json::from_str(raw.trim()).expect("journal line is JSON");
    assert!(row["record"]["payload"].get("authorship").is_none());

    let legacy_reopened = NodeJournal::open_workspace(workspace.path())
        .await
        .expect("reopen legacy production journal");
    let legacy_entries = legacy_reopened.load().await.expect("legacy row replays");
    let [legacy_entry] = legacy_entries.as_slice() else {
        panic!("legacy row replays as one original dispatch: {legacy_entries:?}");
    };
    let JournalRecord::Room(RoomEvent::RemoteEnvelopeDispatched { authorship, .. }) =
        &legacy_entry.record
    else {
        panic!("legacy row is not a dispatch: {legacy_entry:?}");
    };
    assert_eq!(*authorship, ReplyAuthorship::Unknown);

    row["record"]["payload"]["authorship"] = json!("future-authorship-token");
    let mut future = serde_json::to_vec(&row).expect("encode future token row");
    future.push(b'\n');
    tokio::fs::write(journal.path(), future)
        .await
        .expect("replace row with future-token record");

    let reopened = NodeJournal::open_workspace(workspace.path())
        .await
        .expect("reopen production journal");
    let entries = reopened
        .load()
        .await
        .expect("future token remains readable");
    let [entry] = entries.as_slice() else {
        panic!("future token remains readable as the original dispatch: {entries:?}");
    };
    let JournalRecord::Room(RoomEvent::RemoteEnvelopeDispatched { authorship, .. }) = &entry.record
    else {
        panic!("replayed journal entry is not a dispatch: {entry:?}");
    };
    assert_eq!(*authorship, ReplyAuthorship::Unknown);
}

#[tokio::test]
async fn k06_followup_and_retract_dispatches_keep_human_and_unknown_provenance() {
    let host = Host::start(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let mut sender = Sender::start(vec![host.peer("authorship-peer")]).await;
    let mut state = TuiState::new(100, 40);

    sender
        .command(
            &mut state,
            "/team send --type architecture_update authorship-peer original-body",
        )
        .await;
    sender.settle(&mut state, 1).await;
    let task = read_room_events(&sender.journal)
        .await
        .into_iter()
        .find_map(|(_, event)| match event {
            RoomEvent::RemoteEnvelopeDispatched {
                task: Some(task), ..
            } => Some(task),
            _ => None,
        })
        .expect("initial send dispatch task");

    sender
        .command(
            &mut state,
            &format!("/team followup authorship-peer {task} followup-body"),
        )
        .await;
    sender.settle(&mut state, 1).await;

    let received_item = read_room_events(&host.journal)
        .await
        .into_iter()
        .find_map(|(_, event)| match event {
            RoomEvent::RecipientItemReceived { address, .. } => {
                Some(address.item().as_str().to_owned())
            }
            _ => None,
        })
        .expect("recipient journal contains a real received item");
    sender
        .raise_retract_card(
            &mut state,
            &format!("/team retract authorship-peer {received_item}"),
        )
        .await;
    let retracted = sender
        .confirm_retract(&mut state)
        .await
        .expect("confirmed retract dispatch");
    assert!(retracted.starts_with("Retracted on authorship-peer's host."));

    let authorship: Vec<_> = read_room_events(&sender.journal)
        .await
        .into_iter()
        .filter_map(|(_, event)| match event {
            RoomEvent::RemoteEnvelopeDispatched {
                act, authorship, ..
            } => Some((act, authorship)),
            _ => None,
        })
        .collect();
    assert_eq!(
        authorship.len(),
        3,
        "typed send, follow-up and retract each dispatched"
    );
    assert_eq!(authorship[0].1, ReplyAuthorship::HumanWritten);
    assert_eq!(authorship[1].1, ReplyAuthorship::HumanWritten);
    assert!(matches!(
        authorship[2].0,
        rustain::domain::models::DispatchAct::ItemRetract { .. }
    ));
    assert_eq!(authorship[2].1, ReplyAuthorship::Unknown);
    host.stop().await;
}

#[tokio::test]
async fn k06_model_delegation_dispatch_is_agent_composed() {
    let host = Host::start(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let sender = Sender::start(vec![host.peer("delegation-peer")]).await;
    let capability = rustain::domain::models::CapabilityId {
        protocol: "a2a".to_owned(),
        server: "delegation-peer".to_owned(),
        tool: "delegate".to_owned(),
    };
    let _result = tokio::time::timeout(
        SETTLE,
        sender.egress.provider().invoke(
            &capability,
            json!({"message": "model delegation body"}),
            CancellationToken::new(),
        ),
    )
    .await
    .expect("model delegation invocation completes");
    // NotifyAndWait may require another turn; provenance belongs to the dispatch,
    // independently of that supported capability result. The operator-visible
    // `/team log` projection is asserted before the journal field it renders.
    let entry = sender
        .journal
        .entries()
        .await
        .into_iter()
        .find(|entry| {
            matches!(
                &entry.record,
                JournalRecord::Room(RoomEvent::RemoteEnvelopeDispatched { .. })
            )
        })
        .expect("model delegation wrote its dispatch row");
    let row = rustain::domain::services::transparency::transparency_row(&entry)
        .expect("sender /team log projection includes dispatch");
    assert!(
        row.summary.contains("[agent-composed]"),
        "model delegation /team log row lost its agent-composed tag: {}",
        row.summary
    );
    assert!(!row.summary.contains("[auto-sent]"), "{}", row.summary);
    let JournalRecord::Room(RoomEvent::RemoteEnvelopeDispatched { authorship, .. }) = &entry.record
    else {
        panic!("selected journal entry is not a dispatch: {entry:?}");
    };
    assert_eq!(
        *authorship,
        ReplyAuthorship::AgentComposed,
        "model delegation must retain agent-composed dispatch authorship"
    );
}
