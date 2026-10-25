//! Story 19.22 K04: no silent displacement — a send that would take a story
//! away from its current holder is refused until that holder is addressed.
//!
//! One sender journal throughout; every attempt enters through the real
//! attached `/team send` frame and the daemon's pre-spawn validation, and every
//! refusal must leave zero new action records and zero new `message/send`
//! attempts on every host. Runnable once the `scope_change` type widening
//! (Task 2) has landed; the refusal legs go green with Task 3.
use super::attached_harness::{AttachedClient, AttachedDaemon, RecipientHost};
use super::harness::read_room_events;
use rustain::adapters::a2a::admission::A2aAdmissionPolicy;
use rustain::adapters::daemon::protocol::{
    ConnectionTier, DaemonFrame, ProtocolError, RecipientAction, RecipientActionOutcome,
    RecipientActionTarget, TeamSendOutcome,
};
use rustain::domain::models::{RoomEvent, SemanticMessageType, StoryManifest};

async fn action_record_count(daemon: &AttachedDaemon) -> usize {
    read_room_events(&daemon.journal)
        .await
        .iter()
        .filter(|(_, event)| matches!(event, RoomEvent::TeamSendActionRecorded { .. }))
        .count()
}

async fn expect_displacement_refusal(
    writer: &mut AttachedClient,
    request: &str,
    key: &str,
    holder: &str,
) {
    let frame = writer.wait_send_started(request).await;
    let DaemonFrame::RecipientActionResult {
        action: RecipientAction::TeamSend,
        target: RecipientActionTarget::Request { request: actual },
        outcome:
            RecipientActionOutcome::Refused {
                error: ProtocolError::Malformed(text),
            },
    } = frame
    else {
        panic!("expected the request-keyed displaced-holder refusal, got {frame:?}")
    };
    assert_eq!(actual, request);
    assert_eq!(
        text,
        format!(
            "story '{key}' was assigned to '{holder}' by your earlier send; address '{holder}' too, so the change is not silent"
        )
    );
    assert!(
        writer
            .frames_for_request(request)
            .iter()
            .all(|frame| !matches!(
                frame,
                DaemonFrame::TeamSendStarted { .. } | DaemonFrame::TeamSendSettled { .. }
            )),
        "a refused send never starts or settles"
    );
}

#[tokio::test]
async fn v_k04_displaced_holder_is_never_silent() {
    let b = RecipientHost::start(A2aAdmissionPolicy::Allow).await;
    let c = RecipientHost::start(A2aAdmissionPolicy::Allow).await;
    let daemon = AttachedDaemon::start(vec![b.pinned_peer("B"), c.pinned_peer("C")]).await;
    let mut writer =
        AttachedClient::attach(&daemon.path, ConnectionTier::TrustedLocal, false).await;

    // Holder statements: B holds S7-07 and S7-09.
    for (request, manifest) in [("holder-s7-07", "S7-07=B"), ("holder-s7-09", "S7-09=B")] {
        writer
            .send_team(
                request,
                vec!["B".into()],
                "story receipt",
                Some(SemanticMessageType::StoryAssignment),
                None,
                Some(StoryManifest::parse(manifest).unwrap()),
            )
            .await;
        assert!(matches!(
            writer.wait_send_settled(request, 0).await,
            DaemonFrame::TeamSendSettled {
                outcome: TeamSendOutcome::Delivered,
                ..
            }
        ));
    }
    let recorded = action_record_count(&daemon).await;
    assert_eq!(recorded, 2);

    // Releasing S7-07 while addressing only C would displace B silently.
    let before_b = b.method_count("message/send");
    let before_c = c.method_count("message/send");
    writer
        .send_team(
            "silent-release",
            vec!["C".into()],
            "S7-07 released",
            Some(SemanticMessageType::ScopeChange),
            None,
            Some(StoryManifest::parse("S7-07").unwrap()),
        )
        .await;
    expect_displacement_refusal(&mut writer, "silent-release", "S7-07", "B").await;
    assert_eq!(
        action_record_count(&daemon).await,
        recorded,
        "a refusal records no action"
    );
    assert_eq!(
        b.method_count("message/send"),
        before_b,
        "no I/O on a refused send"
    );
    assert_eq!(c.method_count("message/send"), before_c);

    // Addressing B too makes the release ordinary: sent.
    writer
        .send_team(
            "addressed-release",
            vec!["B".into(), "C".into()],
            "S7-07 released",
            Some(SemanticMessageType::ScopeChange),
            None,
            Some(StoryManifest::parse("S7-07").unwrap()),
        )
        .await;
    assert!(matches!(
        writer.wait_send_settled("addressed-release", 0).await,
        DaemonFrame::TeamSendSettled {
            outcome: TeamSendOutcome::Delivered,
            ..
        }
    ));
    assert!(matches!(
        writer.wait_send_settled("addressed-release", 1).await,
        DaemonFrame::TeamSendSettled {
            outcome: TeamSendOutcome::Delivered,
            ..
        }
    ));

    // The latest holder statement is none, so C may now take S7-07 alone.
    writer
        .send_team(
            "reassign-s7-07",
            vec!["C".into()],
            "story receipt",
            Some(SemanticMessageType::StoryAssignment),
            None,
            Some(StoryManifest::parse("S7-07=C").unwrap()),
        )
        .await;
    assert!(matches!(
        writer.wait_send_settled("reassign-s7-07", 0).await,
        DaemonFrame::TeamSendSettled {
            outcome: TeamSendOutcome::Delivered,
            ..
        }
    ));

    // S7-09 is still B's: a C-only story_assignment is refused with no deltas.
    let before_b = b.method_count("message/send");
    let before_c = c.method_count("message/send");
    let before_records = action_record_count(&daemon).await;
    writer
        .send_team(
            "displace-s7-09",
            vec!["C".into()],
            "story receipt",
            Some(SemanticMessageType::StoryAssignment),
            None,
            Some(StoryManifest::parse("S7-09=C").unwrap()),
        )
        .await;
    expect_displacement_refusal(&mut writer, "displace-s7-09", "S7-09", "B").await;
    assert_eq!(action_record_count(&daemon).await, before_records);
    assert_eq!(b.method_count("message/send"), before_b);
    assert_eq!(c.method_count("message/send"), before_c);

    writer.detach().await;
    daemon.stop().await;
    b.stop().await;
    c.stop().await;
}
