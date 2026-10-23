use std::sync::Arc;

use super::harness::{GateRoomJournal, Host, Sender};
use rustain::adapters::a2a::admission::A2aAdmissionPolicy;
use rustain::adapters::tui::handlers::team_command::TeamBoardSelector;
use rustain::adapters::tui::state::TuiState;
use rustain::domain::models::{ResponseMode, RoomEvent};
use rustain::infrastructure::runtime::transparency_bridge::team_board_message;

/// **K03 in-process projection replacement. Mutant M30 → RED** at the row
/// assertions. This is not an OS-process crash test: the spawned send remains
/// held. The native capture suite separately kills and restarts the daemon.
#[tokio::test]
async fn k03_interrupted_send_is_honest_after_sender_restart() {
    let landed = Host::start(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let interrupted_journal = super::harness::Journal::open().await;
    let gate = Arc::new(GateRoomJournal::new(
        interrupted_journal.room.clone(),
        |event| matches!(event, RoomEvent::RecipientItemReceived { .. }),
    ));
    let interrupted = Host::start_with_journal(
        interrupted_journal.clone(),
        gate.clone(),
        A2aAdmissionPolicy::Allow,
        ResponseMode::NotifyAndWait,
    )
    .await;
    let mut sender =
        Sender::start(vec![landed.peer("landed"), interrupted.peer("interrupted")]).await;
    let journal = sender.journal.clone();
    let peers = vec![landed.peer("landed"), interrupted.peer("interrupted")];
    let mut state = TuiState::new(100, 40);
    sender
        .command(&mut state, "/team send landed,interrupted restart-check")
        .await;
    gate.wait_started().await;

    // Reopen the journal in a fresh runtime while the old received-item append
    // remains held. Dropping this fixture does not cancel the spawned send.
    drop(sender);
    let reopened = journal.reopen().await;
    let mut restarted = Sender::with_journal(peers, reopened.room.clone(), reopened).await;
    let mut state = TuiState::new(100, 40);
    restarted.command(&mut state, "/team board --last").await;
    let board = team_board_message(restarted.runtime(), &TeamBoardSelector::Last).await;
    assert!(
        board.contains("interrupted") && board.contains("no outcome recorded"),
        "{board}"
    );
    assert!(!board.contains("sending…"), "{board}");
    assert!(
        board.contains("● landed") && board.contains("delivered"),
        "{board}"
    );

    gate.release();
    landed.stop().await;
    interrupted.stop().await;
}
