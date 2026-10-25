//! Story 19.21d K02: the daemon checks deserialized manifests before any work.
use super::attached_harness::{AttachedClient, AttachedDaemon, RecipientHost};
use super::harness::read_room_events;
use rustain::adapters::a2a::admission::A2aAdmissionPolicy;
use rustain::adapters::daemon::protocol::{
    ConnectionTier, DaemonFrame, ProtocolError, RecipientAction, RecipientActionOutcome,
    RecipientActionTarget, TeamSendOutcome,
};
use rustain::domain::models::{RoomEvent, SemanticMessageType, StoryManifest};
use serde_json::json;

#[tokio::test]
async fn d_k02_raw_frame_refusals_are_exact_keyed_and_make_no_posts() {
    let host = RecipientHost::start(A2aAdmissionPolicy::Allow).await;
    let boundary = "é".repeat(128);
    let oversized = format!("{boundary}x");
    let daemon = AttachedDaemon::start(vec![
        host.peer("b"),
        host.peer(&boundary),
        host.peer(&oversized),
    ])
    .await;
    let mut writer =
        AttachedClient::attach(&daemon.path, ConnectionTier::TrustedLocal, false).await;
    let entry = |key: &str, alias: Option<&str>| json!({"key":key,"to":alias});
    let cases = vec![
        (None, vec!["b".into()], vec![entry("S1", None)], "'--stories' needs '--type story_assignment' or '--type scope_change'".to_owned()),
        (Some(SemanticMessageType::DesignUpdate), vec!["b".into()], vec![entry("S1", None)], "'--stories' needs '--type story_assignment' or '--type scope_change'".to_owned()),
        (Some(SemanticMessageType::StoryAssignment), vec!["b".into()], vec![entry("S1", Some("not-addressed"))], "story 'S1' is assigned to 'not-addressed', who is not addressed by this send".to_owned()),
        (Some(SemanticMessageType::StoryAssignment), vec![oversized.clone()], vec![entry("S1", Some(&oversized))], "story 'S1' is assigned to an alias longer than 256 bytes".to_owned()),
        (Some(SemanticMessageType::StoryAssignment), vec!["b".into()], vec![entry("S1", None), entry("S1", Some("b"))], "story key 'S1' is listed twice".to_owned()),
        (Some(SemanticMessageType::StoryAssignment), vec!["b".into()], (0..65).map(|n| entry(&format!("S{n}"), None)).collect(), "a send lists at most 64 stories".to_owned()),
        (Some(SemanticMessageType::StoryAssignment), vec!["b".into()], vec![entry("bad!", None)], "story key 'bad!' is not valid: 1–32 characters from A–Z a–z 0–9 . _ -, starting with a letter or digit".to_owned()),
    ];
    for (index, (kind, recipients, entries, expected)) in cases.into_iter().enumerate() {
        let request = format!("raw-invalid-{index}");
        // Deliberately bypass parse: serde accepts invalid keys and shapes that
        // only the production send boundary can reject.
        let stories: StoryManifest = serde_json::from_value(json!({"stories":entries})).unwrap();
        writer
            .send_team(
                &request,
                recipients,
                "must not leave this host",
                kind,
                None,
                Some(stories),
                None,
            )
            .await;
        let frame = writer.wait_send_started(&request).await;
        let DaemonFrame::RecipientActionResult {
            action: RecipientAction::TeamSend,
            target: RecipientActionTarget::Request { request: actual },
            outcome:
                RecipientActionOutcome::Refused {
                    error: ProtocolError::Malformed(text),
                },
        } = frame
        else {
            panic!("expected request-keyed malformed refusal, got {frame:?}")
        };
        assert_eq!(actual, request);
        assert_eq!(text, expected);
        assert!(
            writer
                .frames_for_request(&request)
                .iter()
                .all(|f| !matches!(
                    f,
                    DaemonFrame::TeamSendStarted { .. } | DaemonFrame::TeamSendSettled { .. }
                ))
        );
        assert!(
            read_room_events(&daemon.journal)
                .await
                .iter()
                .all(|(_, event)| !matches!(
                    event,
                    RoomEvent::TeamSendActionRecorded { .. }
                        | RoomEvent::RemoteEnvelopeDispatched { .. }
                ))
        );
        assert_eq!(host.method_count("message/send"), 0);
    }
    let valid = StoryManifest::parse(&format!("S1={boundary}")).unwrap();
    valid
        .check(
            Some(SemanticMessageType::StoryAssignment),
            std::slice::from_ref(&boundary),
        )
        .unwrap();
    writer
        .send_team(
            "byte-boundary",
            vec![boundary],
            "256-byte alias passes",
            Some(SemanticMessageType::StoryAssignment),
            None,
            Some(valid),
            None,
        )
        .await;
    assert!(matches!(
        writer.wait_send_settled("byte-boundary", 0).await,
        DaemonFrame::TeamSendSettled {
            outcome: TeamSendOutcome::Delivered,
            ..
        }
    ));
    assert_eq!(host.method_count("message/send"), 1);
    assert_eq!(
        read_room_events(&daemon.journal)
            .await
            .iter()
            .filter(|(_, event)| matches!(event, RoomEvent::TeamSendActionRecorded { .. }))
            .count(),
        1
    );
    writer.detach().await;
    daemon.stop().await;
    host.stop().await;
}
