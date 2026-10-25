//! Story 19.25: aggregate-only retrospective surfaces.
use super::harness::{Host, Sender};
use rustain::adapters::a2a::admission::A2aAdmissionPolicy;
use rustain::adapters::tui::handlers::team_command::TeamBoardSelector;
use rustain::adapters::tui::state::TuiState;
use rustain::domain::models::ResponseMode;

#[tokio::test]
async fn r_k02_unanswered_report_names_and_counts_nobody() {
    let host = Host::start(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let mut sender = Sender::start(vec![host.peer("private-member")]).await;
    let mut state = TuiState::new(80, 24);
    sender
        .command(
            &mut state,
            "/team send --type retro_request private-member Sprint retro",
        )
        .await;
    sender.settle(&mut state, 1).await;
    let board = rustain::infrastructure::runtime::transparency_bridge::team_board_message(
        sender.runtime(),
        &TeamBoardSelector::Last,
    )
    .await;
    host.stop().await;
    assert!(
        board.contains("Retrospective report · retro_request"),
        "{board}"
    );
    assert!(board.contains("Nothing has been shared so far."), "{board}");
    assert!(!board.contains("private-member"), "{board}");
    assert!(!board.contains("addressed"), "{board}");
}

#[tokio::test]
async fn r_k02_wait_card_prepares_a_consent_manifest() {
    use super::d_k04_render::StoryRecipient;
    use super::s_harness::request;
    use rustain::domain::models::PeerRowStage;
    let mut recipient =
        StoryRecipient::start_for_type(ResponseMode::NotifyAndWait, "retro_request", None).await;
    let item = request(
        &mut recipient,
        "retro-baseline",
        "Sprint retro",
        "retro_request",
        PeerRowStage::AwaitingOperator,
    )
    .await;
    let control = recipient
        .view
        .peer_row_controls
        .values()
        .find(|control| control.item_address.as_ref() == Some(&item))
        .unwrap();
    let value = serde_json::to_value(control).unwrap();
    recipient.stop().await;
    assert!(
        value["retro"].is_object(),
        "wait card must offer the prepared manifest for explicit Share: {value}"
    );
}

#[tokio::test]
async fn r_k02_preview_is_only_for_waiting_retrospective_requests() {
    use super::d_k04_render::StoryRecipient;
    use super::s_harness::request;
    use rustain::domain::models::PeerRowStage;
    for (mode, token, stage) in [
        (
            ResponseMode::NotifyAndWait,
            "consultation",
            PeerRowStage::AwaitingOperator,
        ),
        (
            ResponseMode::NotifyAndAuto,
            "retro_request",
            PeerRowStage::Settled,
        ),
    ] {
        let mut recipient = StoryRecipient::start_for_type(mode, token, None).await;
        let item = request(
            &mut recipient,
            "no-retro-preview",
            "private body",
            token,
            stage,
        )
        .await;
        assert!(
            recipient
                .view
                .peer_row_controls
                .values()
                .filter(|control| control.item_address.as_ref() == Some(&item))
                .all(|control| control.retro.is_none())
        );
        recipient.stop().await;
    }
}
