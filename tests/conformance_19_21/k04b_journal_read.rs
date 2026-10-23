use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use super::harness::{Journal, Sender, read_room_events};
use super::k02_board_rules::{controlled_peer, list_request_count};
use rustain::adapters::tui::state::TuiState;
use rustain::domain::models::{JournalEntry, RoomEvent};
use rustain::domain::ports::{RoomJournalError, RoomJournalReader};

const S13: &str = "· this host could not read its send journal.";
const WAIT: std::time::Duration = std::time::Duration::from_secs(45);

struct UnreadableReader;

#[async_trait::async_trait]
impl RoomJournalReader for UnreadableReader {
    async fn load_entries(&self) -> Result<Vec<JournalEntry>, RoomJournalError> {
        Err(RoomJournalError::Read("injected read failure".to_owned()))
    }
}

struct FailOnceReader {
    inner: Arc<dyn RoomJournalReader>,
    fail_next: AtomicBool,
}

#[async_trait::async_trait]
impl RoomJournalReader for FailOnceReader {
    async fn load_entries(&self) -> Result<Vec<JournalEntry>, RoomJournalError> {
        if self.fail_next.swap(false, Ordering::SeqCst) {
            Err(RoomJournalError::Read(
                "injected first read failure".to_owned(),
            ))
        } else {
            self.inner.load_entries().await
        }
    }
}

async fn board(sender: &mut Sender, state: &mut TuiState, command: &str) -> String {
    sender.command(state, command).await;
    tokio::time::timeout(WAIT, async {
        loop {
            if let Some(rustain::domain::events::AppEvent::TeamBoardReady { message, .. }) =
                sender.domain_rx.recv().await
            {
                return message;
            }
        }
    })
    .await
    .expect("production team-board command publishes its rendered message")
}

fn rendered_info_at_60(message: &str) -> String {
    use ratatui::{backend::TestBackend, prelude::*, widgets::Paragraph};
    let block = rustain::domain::models::FeedbackBlock {
        id: "k04b-width".to_owned(),
        level: rustain::domain::models::FeedbackLevel::Info,
        message: message.to_owned(),
        actions: Vec::new(),
    };
    let lines = rustain::adapters::tui::widgets::feedback_block::render_feedback_lines(
        &block,
        60,
        &rustain::adapters::tui::theme::Theme::dark(),
    );
    let height = u16::try_from(lines.len()).expect("board block height fits in u16");
    let mut terminal = Terminal::new(TestBackend::new(60, height.max(1)))
        .expect("construct 60-column chat pane backend");
    terminal
        .draw(|frame| frame.render_widget(Paragraph::new(lines), frame.area()))
        .expect("paint board message into the chat pane buffer");
    let buffer = terminal.backend().buffer();
    let mut rendered = String::new();
    for y in 0..height {
        for x in 0..60 {
            rendered.push_str(buffer[(x, y)].symbol());
        }
        rendered.push('\n');
    }
    rendered
}

fn compact_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn flatten_cells(text: &str) -> String {
    text.chars()
        .filter(|character| !character.is_whitespace())
        .collect()
}
fn flatten_field_cells(text: &str) -> String {
    text.chars()
        .filter(|character| !character.is_whitespace() && *character != '│')
        .collect()
}

fn assert_s13(message: &str, selector: &str) {
    assert_eq!(
        message, S13,
        "M56 S13 {selector} must render the unreadable-journal refusal"
    );
    let displayed = rendered_info_at_60(message);
    let compact = compact_whitespace(&displayed);
    let expected = compact_whitespace(S13);
    assert!(
        compact.contains(&expected),
        "S13 must survive the 60-column Info buffer whole (starts with `· `, ends with `journal.`): {displayed:?}"
    );
    let cells = flatten_cells(&displayed);
    assert!(
        cells.starts_with("│ℹ·"),
        "S13 must start with its bullet: {displayed:?}"
    );
    assert!(
        cells.contains("·thishostcouldnotreaditssendjournal.│"),
        "S13 must end with `journal.` before the Info-block border: {displayed:?}"
    );
}

/// **K04b S13 front-door leg. M56-unreadable-claims-no-action and
/// M56-unreadable-renders-empty-list → RED** at the exact S13 assertions for
/// `--last`, `--action`, and `--actions`. Positive control: the readable
/// sender records and renders a real action through the same command path.
#[tokio::test]
async fn k04b_unreadable_journal_selectors_render_s13_without_peer_reads() {
    let peer = controlled_peer("counted-peer", false, Some("received")).await;
    let journal = Journal::open().await;
    let reader: Arc<dyn RoomJournalReader> = Arc::new(UnreadableReader);
    let clock = Arc::new(rustain::domain::clock::MockClock::at_wall_ms(
        1_800_000_000_000,
    ));
    let mut unreadable = Sender::with_reader_and_clock(
        vec![peer.spec.clone()],
        journal.room.clone(),
        journal,
        reader,
        clock,
    )
    .await;
    let mut state = TuiState::new(100, 40);

    for (selector, command) in [
        ("--last", "/team board --last".to_owned()),
        ("--action", "/team board --action sa_unknown1".to_owned()),
        ("--actions", "/team board --actions".to_owned()),
    ] {
        let message = board(&mut unreadable, &mut state, &command).await;
        assert_s13(&message, selector);
        assert!(!message.contains("no send action recorded"));
        assert!(!message.contains("no send action '"));
    }
    assert_eq!(
        list_request_count(&peer.server).await,
        0,
        "an unreadable ledger must refuse before any peer list request"
    );

    let mut readable = Sender::start(vec![peer.spec.clone()]).await;
    let mut readable_state = TuiState::new(100, 40);
    readable
        .command(
            &mut readable_state,
            "/team send counted-peer positive-control",
        )
        .await;
    readable.settle(&mut readable_state, 1).await;
    let control = board(&mut readable, &mut readable_state, "/team board --last").await;
    assert!(control.contains("Distribution board"), "{control}");
    assert!(control.contains("1 addressed"), "{control}");
    assert!(
        !control.contains(S13),
        "readable positive control: {control}"
    );
}

/// **K04b fail-closed mint leg. M58-read-failure-ignored → RED** when the
/// first failed read records an action or touches either peer. Positive
/// control: the same reader recovers for the next send, which records and
/// dispatches to both real command-selected recipients.
#[tokio::test]
async fn k04b_action_id_read_failure_is_not_cached_and_sends_nothing() {
    let a = controlled_peer("a", false, Some("received")).await;
    let b = controlled_peer("b", false, Some("received")).await;
    let journal = Journal::open().await;
    let reader: Arc<dyn RoomJournalReader> = Arc::new(FailOnceReader {
        inner: journal.node.clone(),
        fail_next: AtomicBool::new(true),
    });
    let clock = Arc::new(rustain::domain::clock::MockClock::at_wall_ms(
        1_800_000_000_000,
    ));
    let peers = vec![a.spec.clone(), b.spec.clone()];
    let mut sender =
        Sender::with_reader_and_clock(peers, journal.room.clone(), journal, reader, clock).await;
    let mut state = TuiState::new(100, 40);

    sender
        .command(&mut state, "/team send a,b failed-read-body")
        .await;
    sender.settle(&mut state, 2).await;
    let first_events = read_room_events(&sender.journal).await;
    assert!(
        !first_events
            .iter()
            .any(|(_, event)| matches!(event, RoomEvent::TeamSendActionRecorded { .. })),
        "M58 reader failure must not record an action"
    );
    assert!(
        !first_events
            .iter()
            .any(|(_, event)| matches!(event, RoomEvent::RemoteEnvelopeDispatched { .. })),
        "a failed action-id mint must dispatch nothing"
    );
    let refusal = "not sent — this host could not record the attempt";
    assert!(
        state
            .feedback_blocks
            .values()
            .any(|block| block.message.matches(refusal).count() == 2),
        "each recipient row must render {refusal:?}: {:?}",
        state
            .feedback_blocks
            .values()
            .map(|block| &block.message)
            .collect::<Vec<_>>()
    );
    let post_count = |requests: Vec<wiremock::Request>| {
        requests
            .iter()
            .filter(|request| request.method.as_str() == "POST")
            .count()
    };
    assert_eq!(
        post_count(a.server.received_requests().await.unwrap_or_default()),
        0
    );
    assert_eq!(
        post_count(b.server.received_requests().await.unwrap_or_default()),
        0
    );

    sender
        .command(&mut state, "/team send a,b recovered-body")
        .await;
    sender.settle(&mut state, 2).await;
    let all_events = read_room_events(&sender.journal).await;
    assert_eq!(
        all_events
            .iter()
            .filter(|(_, event)| matches!(event, RoomEvent::TeamSendActionRecorded { .. }))
            .count(),
        1,
        "the recovered read seeds exactly one action record"
    );
    assert_eq!(
        post_count(a.server.received_requests().await.unwrap_or_default()),
        1
    );
    assert_eq!(
        post_count(b.server.received_requests().await.unwrap_or_default()),
        1
    );
}

/// **K04b field-wrap leg. M56-field-bound-shrunk → RED** if a 200-byte alias
/// or unknown action id is shortened or clipped in the actual 60-column Info
/// buffer. Positive control: the 300-byte alias keeps the documented disclosure
/// marker, demonstrating that only the wider field is preserved whole.
#[tokio::test]
async fn k04b_long_alias_and_unknown_id_wrap_but_disclosure_bound_remains() {
    let alias = "a".repeat(200);
    let long_peer = controlled_peer(&alias, false, Some("received")).await;
    let mut sender = Sender::start(vec![long_peer.spec.clone()]).await;
    let mut state = TuiState::new(100, 40);
    sender
        .command(&mut state, &format!("/team send {alias} body"))
        .await;
    sender.settle(&mut state, 1).await;
    let alias_board = board(&mut sender, &mut state, "/team board --last").await;
    assert!(
        alias_board.contains(&alias),
        "M56 200-byte alias must remain intact below the disclosure bound: {alias_board}"
    );
    assert!(
        !alias_board.contains("…[truncated]"),
        "200-byte alias is below the disclosure bound: {alias_board}"
    );
    let displayed = rendered_info_at_60(&alias_board);
    let flattened = flatten_field_cells(&displayed);
    assert_eq!(
        flattened.matches(&alias).count(),
        1,
        "M56 200-byte alias must survive 60-column wrapping exactly once: {displayed:?}"
    );

    let unknown = "u".repeat(200);
    let refusal = board(
        &mut sender,
        &mut state,
        &format!("/team board --action {unknown}"),
    )
    .await;
    assert!(
        refusal.contains(&unknown),
        "S12 omitted long unknown action id: {refusal}"
    );
    assert!(
        !refusal.contains("…[truncated]"),
        "200-byte action id is below the disclosure bound: {refusal}"
    );
    let displayed = rendered_info_at_60(&refusal);
    let flattened = flatten_field_cells(&displayed);
    assert_eq!(
        flattened.matches(&unknown).count(),
        1,
        "M56 200-character unknown action id must survive 60-column wrapping exactly once: {displayed:?}"
    );

    let disclosure_alias = "d".repeat(320);
    let bounded_peer = controlled_peer(&disclosure_alias, false, Some("received")).await;
    let mut bounded_sender = Sender::start(vec![bounded_peer.spec.clone()]).await;
    let mut bounded_state = TuiState::new(100, 40);
    bounded_sender
        .command(
            &mut bounded_state,
            &format!("/team send {disclosure_alias} body"),
        )
        .await;
    bounded_sender.settle(&mut bounded_state, 1).await;
    let bounded_board = board(
        &mut bounded_sender,
        &mut bounded_state,
        "/team board --last",
    )
    .await;
    assert!(
        bounded_board.contains("…[truncated]"),
        "the 256-byte disclosure bound remains active: {bounded_board}"
    );
    assert!(
        rendered_info_at_60(&bounded_board).contains("…[truncated]"),
        "the documented disclosure marker survives 60-column rendering"
    );
}
