//! Cross-rail evidence through production RAP/A2A ingress and restart recovery.
use super::harness::{BoundHost, HostSpec, K_P1A, raw_payloads, room_events, setup};
use super::k11_k14_consent_policy::{RapSender, deliver, delivered, open_rap_front_door};
use rustain::adapters::cli::team::log::render_team_log;
use rustain::adapters::cli::team::rows::build_transparency_rows;
use rustain::domain::models::PeerRail;
use rustain::domain::models::{AgentId, NodeState, OrchestrationRoom, OrchestrationRoomId};
use rustain::domain::ports::RoomJournalReader;
use rustain::domain::services::transparency::{
    TransparencyKind, TransparencyReport, render_export,
};

const AUTO: &str =
    "[interaction.defaults]\nresponse_mode = \"notify-and-auto\"\nnotification = \"immediate\"\n";

#[tokio::test(flavor = "multi_thread")]
async fn k01_same_pin_and_task_stay_distinct_and_rap_mints_no_item() {
    setup();
    let mut rap = RapSender::new();
    let pin = rap.pin();
    let mut host = BoundHost::start(HostSpec::b2().roster(rap.roster()).interaction(AUTO)).await;
    let mut operator = host.attach_operator().await;
    let handler = open_rap_front_door(&host).await;
    let delivery = deliver(&handler, &mut rap, "inbox", "one way");
    operator.always_allow().await;
    delivered(delivery).await;
    assert!(raw_payloads(&host.journal, "recipient_item_received").is_empty());
    let entries = host.journal.node.load_entries().await.unwrap();
    let rows = build_transparency_rows(&entries, Some(&format!("peer={pin}"))).unwrap();
    let accepted = rows.iter().find(|r| r.kind.label() == "accepted").unwrap();
    assert_eq!(
        accepted.summary,
        "message delivered one way; no task runs and nothing is sent back"
    );
    assert!(accepted.one_line().contains("via RAP"));
    assert_eq!(accepted.rail, Some(PeerRail::Rap));
    assert_eq!(
        raw_payloads(&host.journal, "remote_envelope_accepted")[0]["rail"],
        "rap"
    );
    let reply = host
        .client(K_P1A)
        .message_send(1, "rap-corr-1", "A2A control", serde_json::json!({}))
        .await;
    assert!(reply.json["error"].is_null(), "{}", reply.body);
    let items = raw_payloads(&host.journal, "recipient_item_received");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["address"]["principal"]["kind"], "a2a_bound");
    assert_eq!(items[0]["address"]["boundPeer"], pin.as_str());
    let entries = host.journal.node.load_entries().await.unwrap();
    let rows = build_transparency_rows(&entries, Some(&format!("peer={pin}"))).unwrap();
    let accepted: Vec<_> = rows
        .iter()
        .filter(|row| row.kind == TransparencyKind::Accepted)
        .collect();
    assert_eq!(accepted.len(), 2);
    assert_eq!(accepted[0].rail, Some(PeerRail::Rap));
    assert_eq!(accepted[1].rail, Some(PeerRail::A2a));
    for row in &accepted {
        assert_eq!(row.peer, pin.as_str());
        assert_eq!(row.task.as_deref(), Some("rap-corr-1"));
    }
    assert!(accepted[1].one_line().contains("via A2A"));
    assert_eq!(accepted[1].summary, "task accepted and executing");
    let item_row = rows
        .iter()
        .find(|row| row.kind == TransparencyKind::RecipientItemReceived)
        .unwrap();
    assert_eq!(item_row.rail, None);
    assert!(!item_row.one_line().contains("via "));
    let export = render_export(&rows);
    let exported: Vec<serde_json::Value> = export
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(
        exported
            .iter()
            .filter(|row| row.get("rail").is_some())
            .count(),
        2
    );
    assert_eq!(
        exported
            .iter()
            .filter_map(|row| row.get("rail"))
            .collect::<Vec<_>>(),
        vec!["rap", "a2a"]
    );
    let report = TransparencyReport {
        rows,
        ..Default::default()
    };
    let mut json = Vec::new();
    render_team_log(None, true, &report, None, &mut json).unwrap();
    let value: serde_json::Value = serde_json::from_slice(&json).unwrap();
    assert_eq!(
        value["rows"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|row| row.get("rail").is_some())
            .count(),
        2
    );
    drop(operator);
}

#[tokio::test(flavor = "multi_thread")]
async fn k02_restart_retires_rap_context_and_next_frame_rematerializes_it() {
    setup();
    let mut rap = RapSender::new();
    let id = AgentId::parse(&format!("{}/inbox", rap.pin())).unwrap();
    let spec = HostSpec::b2().roster(rap.roster()).interaction(AUTO);
    let mut host = BoundHost::start(spec.clone()).await;
    let mut operator = host.attach_operator().await;
    let handler = open_rap_front_door(&host).await;
    let delivery = deliver(&handler, &mut rap, "inbox", "before restart");
    operator.always_allow().await;
    delivered(delivery).await;
    drop(handler);
    drop(operator);
    let host = host.restart(spec.clone()).await;
    assert!(
        host.node_tree.delivery_target(&id).await.is_none(),
        "RAP husk must not be restored"
    );
    let room = OrchestrationRoom::project(
        OrchestrationRoomId::parse("rail-test").unwrap(),
        room_events(&host.journal)
            .await
            .into_iter()
            .map(|(_, event)| event),
    );
    assert_eq!(room.nodes()[&id].state, NodeState::Cancelled);
    let entries = host.journal.node.load_entries().await.unwrap().len();
    let host = host.restart(spec.clone()).await;
    assert_eq!(
        host.journal.node.load_entries().await.unwrap().len(),
        entries,
        "terminal retirement is idempotent"
    );
    let handler = open_rap_front_door(&host).await;
    delivered(deliver(&handler, &mut rap, "inbox", "after restart")).await;
    let target = host.node_tree.delivery_target(&id).await.unwrap();
    assert!(!target.awaiting_resume);
    assert!(!target.state.is_terminal());
    let room = OrchestrationRoom::project(
        OrchestrationRoomId::parse("rail-test").unwrap(),
        room_events(&host.journal)
            .await
            .into_iter()
            .map(|(_, event)| event),
    );
    assert_eq!(
        room.nodes()[&id].state,
        NodeState::Created,
        "new registration resets the retired Room row"
    );
}
