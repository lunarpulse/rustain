use crate::harness::*;
use crate::k02_board_rules::controlled_peer;

use rustain::adapters::a2a::admission::A2aAdmissionPolicy;
use rustain::adapters::tui::state::TuiState;
use rustain::domain::models::ResponseMode;
use std::time::Duration;

fn recipient_item_received(event: &rustain::domain::models::RoomEvent) -> bool {
    matches!(
        event,
        rustain::domain::models::RoomEvent::RecipientItemReceived { .. }
    )
}

/// Exercise the same Info-block renderer the chat pane uses, then paint those
/// lines into a real 60-column Ratatui buffer so clipping cannot hide a lost
/// terminal token.
fn render_info_at_60(message: &str) -> String {
    use ratatui::{backend::TestBackend, prelude::*, widgets::Paragraph};
    let block = rustain::domain::models::FeedbackBlock {
        id: "k04-width".to_owned(),
        level: rustain::domain::models::FeedbackLevel::Info,
        message: message.to_owned(),
        actions: Vec::new(),
    };
    let lines = rustain::adapters::tui::widgets::feedback_block::render_feedback_lines(
        &block,
        60,
        &rustain::adapters::tui::theme::Theme::dark(),
    );
    let height = u16::try_from(lines.len()).expect("small board feedback fits a u16 height");
    let mut terminal = Terminal::new(TestBackend::new(60, height.max(1)))
        .expect("construct a 60-column chat pane backend");
    terminal
        .draw(|frame| frame.render_widget(Paragraph::new(lines), frame.area()))
        .expect("render the Info block into the chat pane buffer");
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

fn assert_survives_60_column_chat(message: &str, tokens: &[&str]) {
    let chat = render_info_at_60(message);
    for token in tokens {
        assert!(
            chat.contains(token),
            "chat pane clipped distinctive token {token:?} from product copy:\n{chat}\nsource:\n{message}"
        );
    }
    for item_id in message.lines().filter_map(|line| {
        line.trim_start()
            .strip_prefix("item ")
            .and_then(|rest| rest.split_once(" ·").map(|(id, _)| id))
    }) {
        assert!(
            chat.contains(item_id),
            "chat pane clipped item id {item_id:?}:\n{chat}\nsource:\n{message}"
        );
    }
}
async fn board(
    base: &Sender,
    peers: Vec<rustain::domain::models::A2aPeerSpec>,
    state: &mut TuiState,
    command: &str,
) -> String {
    let mut reader = Sender::with_journal_and_clock(
        peers,
        base.journal.room.clone(),
        base.journal.clone(),
        base.clock.clone(),
    )
    .await;
    board_in_process(&mut reader, state, command).await
}

async fn board_in_process(sender: &mut Sender, state: &mut TuiState, command: &str) -> String {
    sender.command(state, command).await;
    tokio::time::timeout(Duration::from_secs(45), async {
        loop {
            if let Some(rustain::domain::events::AppEvent::TeamBoardReady { message, .. }) =
                sender.domain_rx.recv().await
            {
                return message;
            }
        }
    })
    .await
    .expect("the production board command publishes its rendered message")
}

async fn issue(sender: &mut Sender, state: &mut TuiState, command: &str, recipients: usize) {
    sender.command(state, command).await;
    sender.settle(state, recipients).await;
}

/// **K04 action-selection leg. Mutant M24(b) (header counts settled recipients),
/// M25 (aggregate replaces row evidence), M26 (`--last` chooses oldest), M27
/// (explicit id ignored), M28 (`--actions` omits one), M31 (wall clock used),
/// M33 (absent-label placeholder) → RED.** Positive controls: three addressed
/// rows with two durably settled and one held in-flight; the unchanged
/// `--last` selector after a second action.
#[tokio::test]
async fn board_selectors_preserve_action_identity_count_and_injected_age() {
    let a = Host::start(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let b = Host::start(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let pending_journal = Journal::open().await;
    let gate = std::sync::Arc::new(GateRoomJournal::new(
        pending_journal.room.clone(),
        recipient_item_received,
    ));
    let c = Host::start_with_journal(
        pending_journal,
        gate.clone(),
        A2aAdmissionPolicy::Allow,
        ResponseMode::NotifyAndWait,
    )
    .await;
    let peers = vec![
        a.peer("first-peer"),
        b.peer("second-peer"),
        c.peer("pending-peer"),
    ];
    let clock = std::sync::Arc::new(rustain::domain::clock::MockClock::at_wall_ms(
        1_800_000_000_000,
    ));
    let mut sender = Sender::start_with_clock(peers.clone(), clock).await;
    let mut state = TuiState::new(100, 40);

    sender
        .command(
            &mut state,
            "/team send --type story_assignment --subject first\u{001b}-label first-peer,second-peer,pending-peer first-body",
        )
        .await;
    gate.wait_started().await;
    sender.settle(&mut state, 2).await;
    let first_task = read_room_events(&a.journal)
        .await
        .into_iter()
        .find_map(|(_, event)| match event {
            rustain::domain::models::RoomEvent::RecipientItemReceived { task, .. } => Some(task),
            _ => None,
        })
        .expect("the real recipient journal records its received item");
    acknowledge_received_item(&a.journal, &first_task).await;
    let actions = board(&sender, peers.clone(), &mut state, "/team board --actions").await;
    let ids: Vec<_> = actions
        .lines()
        .filter_map(|line| {
            let (id, _) = line.split_once("  ")?;
            id.starts_with("sa_").then_some(id.to_owned())
        })
        .collect();
    assert_eq!(
        ids.len(),
        1,
        "the first action id comes from rendered --actions: {actions}"
    );
    let first_id = ids[0].clone();

    let first_board = board_in_process(&mut sender, &mut state, "/team board --last").await;
    assert!(
        first_board.contains("first-label") && !first_board.contains('\u{001b}'),
        "the subject is sanitized at render while remaining one label line: {first_board}"
    );
    assert!(
        first_board.contains("3 addressed"),
        "the action's full addressed set, not just its settlements, drives the header: {first_board}"
    );
    assert!(
        first_board.contains("✓ first-peer"),
        "per-row acknowledgement survives without aggregation: {first_board}"
    );
    assert!(
        first_board.contains("● second-peer"),
        "per-row delivery survives without aggregation: {first_board}"
    );
    assert!(
        first_board.contains("pending-peer") && first_board.contains("sending…"),
        "the in-flight recipient remains an addressed pending row: {first_board}"
    );
    assert!(
        !first_board.contains(" of ") && !first_board.contains("%"),
        "no aggregate substitutes for individual evidence: {first_board}"
    );
    assert!(
        first_board.contains("attempted just now"),
        "the shared injected clock matches the action's send-time timestamp: {first_board}"
    );
    assert_survives_60_column_chat(
        &first_board,
        &[
            "Distribution board",
            "attempted just now",
            "3 addressed",
            "first-label",
            "first-peer",
            "second-peer",
            "pending-peer",
            "sending…",
        ],
    );

    gate.release();
    sender.settle(&mut state, 1).await;

    sender.clock.advance(Duration::from_secs(180));
    let aged = board(&sender, peers.clone(), &mut state, "/team board --last").await;
    assert!(
        aged.contains("attempted 3m ago"),
        "advancing the shared injected clock ages the action: {aged}"
    );
    assert_survives_60_column_chat(&aged, &["attempted 3m ago", "first-peer"]);

    issue(
        &mut sender,
        &mut state,
        "/team send --subject later-label second-peer second-body",
        1,
    )
    .await;
    let actions = board(&sender, peers.clone(), &mut state, "/team board --actions").await;
    let ids: Vec<_> = actions
        .lines()
        .filter_map(|line| {
            let (id, _) = line.split_once("  ")?;
            id.starts_with("sa_").then_some(id.to_owned())
        })
        .collect();
    assert_eq!(
        ids.len(),
        2,
        "both recorded actions render newest first: {actions}"
    );
    assert_ne!(ids[0], first_id, "latest action has its own rendered id");
    assert_eq!(ids[1], first_id);
    assert!(actions.find(&ids[0]).unwrap() < actions.find(&ids[1]).unwrap());
    assert!(
        actions.contains("first-label")
            && actions.contains("later-label")
            && !actions.contains("<no subject>"),
        "labels appear only on the two labeled actions: {actions}"
    );
    assert_survives_60_column_chat(&actions, &[&ids[0], &ids[1], "later-label", "first-label"]);

    let latest = board(&sender, peers.clone(), &mut state, "/team board --last").await;
    assert!(
        latest.contains("second-peer"),
        "latest action includes its addressed peer: {latest}"
    );
    assert!(
        !latest.contains("first-peer"),
        "latest action excludes first-only recipient: {latest}"
    );
    assert!(
        latest.contains("later-label")
            && !latest.contains("first-label")
            && !latest.contains("subject")
            && !latest.contains("<no subject>"),
        "the latest label is present and absent label has no placeholder: {latest}"
    );
    assert_survives_60_column_chat(
        &latest,
        &[
            "Distribution board",
            "later-label",
            "second-peer",
            "delivered",
        ],
    );

    let selected = board(
        &sender,
        peers.clone(),
        &mut state,
        &format!("/team board --action {first_id}"),
    )
    .await;
    assert!(
        selected.contains("first-label")
            && selected.contains("first-peer")
            && selected.contains("second-peer"),
        "explicit first action selects its own rows: {selected}"
    );
    assert!(
        !selected.contains("second-body"),
        "explicit id does not silently select the newest action: {selected}"
    );
    assert_survives_60_column_chat(
        &selected,
        &[
            "first-label",
            "first-peer",
            "second-peer",
            "pending-peer",
            "delivered",
        ],
    );
    issue(
        &mut sender,
        &mut state,
        "/team send second-peer unlabelled-body",
        1,
    )
    .await;
    let actions = board(&sender, peers.clone(), &mut state, "/team board --actions").await;
    let ids: Vec<_> = actions
        .lines()
        .filter_map(|line| {
            let (id, _) = line.split_once("  ")?;
            id.starts_with("sa_").then_some(id.to_owned())
        })
        .collect();
    assert_eq!(
        ids.len(),
        3,
        "the labelled and unlabelled actions are listed: {actions}"
    );
    assert_eq!(ids[2], first_id);
    assert_survives_60_column_chat(&actions, &[&ids[0], &ids[1], &ids[2]]);

    let unlabelled = board(&sender, peers, &mut state, "/team board --last").await;
    assert!(
        unlabelled.contains("second-peer")
            && !unlabelled.contains("first-peer")
            && !unlabelled.contains("first-label")
            && !unlabelled.contains("later-label")
            && !unlabelled.contains("subject")
            && !unlabelled.contains("<no subject>"),
        "the subjectless latest action adds neither label line nor placeholder: {unlabelled}"
    );
    assert_survives_60_column_chat(
        &unlabelled,
        &["Distribution board", "second-peer", "delivered"],
    );

    a.stop().await;
    b.stop().await;
}

/// **K04 no-action/unknown-selector leg. Mutant M29(a) (bare board becomes an
/// action board) and M29(b) (peer target routed to action board) → RED.**
/// Positive control: the production inventory still reads real peer items.
#[tokio::test]
async fn inventory_and_empty_action_selectors_remain_distinct() {
    let host = Host::start(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let peers = vec![host.peer("inventory-peer")];
    let mut sender = Sender::start(peers.clone()).await;
    let mut state = TuiState::new(100, 40);
    issue(
        &mut sender,
        &mut state,
        "/team send inventory-peer inventory-body",
        1,
    )
    .await;

    let inventory = board(&sender, peers.clone(), &mut state, "/team board").await;
    assert!(
        inventory.contains("Peer item inventory"),
        "bare board uses inventory heading: {inventory}"
    );
    assert!(
        inventory.contains("inventory-peer"),
        "positive control: inventory contains real recipient item: {inventory}"
    );
    assert!(
        !inventory.contains("Distribution board"),
        "bare board did not become action view: {inventory}"
    );
    assert_survives_60_column_chat(
        &inventory,
        &["Peer item inventory", "inventory-peer", "item ", "received"],
    );

    let narrowed = board(&sender, peers, &mut state, "/team board inventory-peer").await;
    assert!(
        narrowed.contains("inventory-peer"),
        "peer-id syntax remains inventory: {narrowed}"
    );
    assert!(
        narrowed.contains("item "),
        "peer-target positive control includes the item picker: {narrowed}"
    );

    let empty = Sender::start(Vec::new()).await;
    let mut empty_state = TuiState::new(100, 40);
    let no_action = board(&empty, Vec::new(), &mut empty_state, "/team board --last").await;
    assert!(
        no_action.contains("· no send action recorded in this workspace"),
        "--last empty copy S6: {no_action}"
    );
    assert!(
        no_action.contains("'/team board' reads the peer item inventory."),
        "S6 names the unchanged inventory command as its remedy: {no_action}"
    );
    assert_survives_60_column_chat(
        &no_action,
        &["no send action recorded", "reads the peer item inventory"],
    );
    let unknown = board(
        &empty,
        Vec::new(),
        &mut empty_state,
        "/team board --action absent-id",
    )
    .await;
    assert!(
        unknown.contains("· no send action 'absent-id' is recorded in this workspace"),
        "unknown selector copy S12: {unknown}"
    );
    assert!(
        unknown.contains("'/team board --actions' lists them."),
        "S12 points to the action-list remedy: {unknown}"
    );
    assert_survives_60_column_chat(&unknown, &["absent-id", "no send action", "lists them"]);
    host.stop().await;
}
/// **K04 remembered-view leg. Mutant M32 (a landed retract refreshes the other
/// remembered view) → RED.** Positive control: the production retract POST
/// lands and the returned board text contains its mark.
#[tokio::test]
async fn a_landed_retract_keeps_the_action_board_view() {
    let host = Host::start(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let mut sender = Sender::start(vec![host.peer("retract-peer")]).await;
    let mut state = TuiState::new(100, 40);
    issue(
        &mut sender,
        &mut state,
        "/team send --type story_assignment retract-peer retract-body",
        1,
    )
    .await;
    sender.clock.advance(Duration::from_secs(180));
    sender.command(&mut state, "/team board --last").await;
    let before = tokio::time::timeout(Duration::from_secs(45), async {
        loop {
            if let Some(rustain::domain::events::AppEvent::TeamBoardReady { message, .. }) =
                sender.domain_rx.recv().await
            {
                return message;
            }
        }
    })
    .await
    .expect("action board rendered through the front door");
    let id = before
        .lines()
        .find_map(|line| {
            let marker = "    item ";
            let rest = line.strip_prefix(marker)?;
            rest.split_once(" ·").map(|(item, _)| item.to_owned())
        })
        .expect("action board exposes the recipient's correlated item id");
    assert!(
        !before.contains("[retracted "),
        "positive control: initial action view has no mark: {before}"
    );

    sender
        .raise_retract_card(&mut state, &format!("/team retract retract-peer {id}"))
        .await;
    let (message, refreshed) = sender
        .confirm_retract_with_board(&mut state)
        .await
        .expect("the real confirmation gate accepted the retract");
    assert!(
        message.contains("Retracted"),
        "the real retract landed: {message}"
    );
    let refreshed = refreshed.expect("the remembered action view was re-rendered");
    assert!(
        refreshed.contains("[retracted "),
        "the action view was re-read with the landed mark: {refreshed}"
    );
    assert!(
        refreshed.contains("Distribution board"),
        "refresh retained the action-board view identity: {refreshed}"
    );
    host.stop().await;
}

/// **K04 inventory remembered-view leg. Mutant M32 (an inventory retraction
/// refreshes the action board) → RED.** Positive control: a real retract
/// reaches the host and the remembered inventory gains its tombstone mark.
#[tokio::test]
async fn a_landed_retract_keeps_the_inventory_view() {
    let host = Host::start(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let mut sender = Sender::start(vec![host.peer("inventory-retract")]).await;
    let mut state = TuiState::new(100, 40);
    issue(
        &mut sender,
        &mut state,
        "/team send --type story_assignment inventory-retract inventory-body",
        1,
    )
    .await;
    sender.command(&mut state, "/team board").await;
    let before = tokio::time::timeout(Duration::from_secs(45), async {
        loop {
            if let Some(rustain::domain::events::AppEvent::TeamBoardReady { message, .. }) =
                sender.domain_rx.recv().await
            {
                return message;
            }
        }
    })
    .await
    .expect("inventory board rendered through the front door");
    let id = before
        .lines()
        .find_map(|line| {
            let rest = line.strip_prefix("    item ")?;
            rest.split_once(" ·").map(|(item, _)| item.to_owned())
        })
        .expect("inventory exposes the real recipient item id");
    assert!(
        before.contains("Peer item inventory") && !before.contains("[retracted "),
        "positive control: inventory has no initial tombstone mark: {before}"
    );

    sender
        .raise_retract_card(&mut state, &format!("/team retract inventory-retract {id}"))
        .await;
    let (message, refreshed) = sender
        .confirm_retract_with_board(&mut state)
        .await
        .expect("the real confirmation gate accepted the inventory retract");
    assert!(
        message.contains("Retracted"),
        "real retract landed: {message}"
    );
    let refreshed = refreshed.expect("remembered inventory view was re-rendered");
    assert!(
        refreshed.contains("Peer item inventory")
            && refreshed.contains("item ")
            && refreshed.contains("[retracted "),
        "retract refresh retains inventory and marks its row: {refreshed}"
    );
    assert!(
        !refreshed.contains("Distribution board"),
        "inventory retract must not switch to action board: {refreshed}"
    );
    host.stop().await;
}

/// **K04 inventory-parity leg. Mutant M29 (bare board content changed beyond
/// the S10 heading) → RED.** Positive control: an actionless send remains
/// visible as a real recipient item in both the front-door board and the
/// shipped 19.16 inventory renderer.
#[tokio::test]
async fn bare_board_matches_shipped_inventory_renderer_except_for_s10_heading() {
    let host = Host::start(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let peers = vec![host.peer("parity-peer")];
    let mut sender = Sender::start(peers.clone()).await;
    let mut state = TuiState::new(100, 40);
    issue(
        &mut sender,
        &mut state,
        "/team send parity-peer parity-body",
        1,
    )
    .await;
    let front_door = board(&sender, peers.clone(), &mut state, "/team board").await;
    let renderer = Sender::with_journal_and_clock(
        peers,
        sender.journal.room.clone(),
        sender.journal.clone(),
        sender.clock.clone(),
    )
    .await;
    let view =
        rustain::adapters::a2a::board::collect_board(renderer.runtime(), std::time::Instant::now())
            .await
            .expect("fresh runtime can read the same inventory fixture");
    let shipped = rustain::adapters::a2a::board::render_board(&view);
    let heading = format!(
        "Peer item inventory · {} row{}",
        view.rows.len(),
        if view.rows.len() == 1 { "" } else { "s" }
    );
    let old_heading = format!(
        "Distribution board · {} row{}",
        view.rows.len(),
        if view.rows.len() == 1 { "" } else { "s" }
    );
    assert!(
        front_door.starts_with(&heading) && shipped.starts_with(&heading),
        "both renderings share the same S10 heading shape: {front_door}\n{shipped}"
    );
    assert_eq!(
        front_door.replacen(&heading, &old_heading, 1),
        shipped.replacen(&heading, &old_heading, 1),
        "bare front-door output differs from the 19.16 renderer only in the S10 heading"
    );
    assert!(
        front_door.contains("parity-peer") && front_door.contains("item "),
        "positive control: the same real actionless item is readable: {front_door}"
    );
    host.stop().await;
}
/// **K04 six-recipient/selector leg. Mutants M24(a) (header counts only rendered
/// rows), M24(b) (header counts only settled recipients), M26 (`--last` picks
/// oldest), M27 (explicit id ignored), M28 (`--actions` omits an action), and
/// M33 (absent-label placeholder) → RED.** Positive controls: five correctly
/// marked visible rows plus a held pending send; the newest `--last` view
/// excludes first-action-only recipients while `--action <first-id>` returns
/// the first action's `✗`, `⚠`, and pending evidence.
#[tokio::test]
async fn six_addressed_recipients_keep_the_count_when_one_correlated_row_is_unnameable() {
    let acknowledged = Host::start(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let delivered = Host::start(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let declined = Host::start(A2aAdmissionPolicy::Deny, ResponseMode::NotifyAndWait).await;
    let gate_journal = Journal::open().await;
    let gate = std::sync::Arc::new(GateRoomJournal::new(
        gate_journal.room.clone(),
        recipient_item_received,
    ));
    let pending = Host::start_with_journal(
        gate_journal,
        gate.clone(),
        A2aAdmissionPolicy::Allow,
        ResponseMode::NotifyAndWait,
    )
    .await;
    let future = controlled_peer("future", false, Some("escalated")).await;
    let preflight_host = Host::start(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let mut preflight = preflight_host.peer("preflight");
    preflight.auth = Some("RUSTAIN_19_21_INTENTIONALLY_UNSET_CREDENTIAL".to_owned());
    let peers = vec![
        acknowledged.peer("acknowledged"),
        delivered.peer("delivered"),
        declined.peer("declined"),
        preflight,
        pending.peer("pending"),
        future.spec.clone(),
    ];
    let mut sender = Sender::start(peers.clone()).await;
    let mut state = TuiState::new(100, 40);
    sender
        .command(
            &mut state,
            "/team send --subject six-label acknowledged,delivered,declined,preflight,pending,future six-peer-body",
        )
        .await;
    gate.wait_started().await;
    sender.settle(&mut state, 5).await;
    let task = read_room_events(&acknowledged.journal)
        .await
        .into_iter()
        .find_map(|(_, event)| match event {
            rustain::domain::models::RoomEvent::RecipientItemReceived { task, .. } => Some(task),
            _ => None,
        })
        .expect("the acknowledged recipient journal records its real incoming item");
    acknowledge_received_item(&acknowledged.journal, &task).await;

    let action_list = board(&sender, peers.clone(), &mut state, "/team board --actions").await;
    let ids: Vec<_> = action_list
        .lines()
        .filter_map(|line| {
            let (id, _) = line.split_once("  ")?;
            id.starts_with("sa_").then_some(id.to_owned())
        })
        .collect();
    assert_eq!(
        ids.len(),
        1,
        "the rendered action list identifies the action: {action_list}"
    );
    let first_id = ids[0].clone();
    assert!(action_list.contains("six-label"), "{action_list}");

    issue(
        &mut sender,
        &mut state,
        "/team send --subject later-label delivered later-body",
        1,
    )
    .await;
    let action_list = board(&sender, peers.clone(), &mut state, "/team board --actions").await;
    let ids: Vec<_> = action_list
        .lines()
        .filter_map(|line| {
            let (id, _) = line.split_once("  ")?;
            id.starts_with("sa_").then_some(id.to_owned())
        })
        .collect();
    assert_eq!(
        ids.len(),
        2,
        "both actions appear in the ledger surface: {action_list}"
    );
    assert_ne!(ids[0], first_id);
    assert_eq!(ids[1], first_id);
    assert!(
        action_list.contains("later-label") && action_list.contains("six-label"),
        "newest-first action rows carry their own labels: {action_list}"
    );
    assert_survives_60_column_chat(
        &action_list,
        &[&ids[0], &ids[1], "later-label", "six-label"],
    );

    let latest = board(&sender, peers.clone(), &mut state, "/team board --last").await;
    assert!(
        latest.contains("later-label")
            && latest.contains("delivered")
            && !latest.contains("acknowledged")
            && !latest.contains("declined")
            && !latest.contains("preflight")
            && !latest.contains("pending"),
        "newest action's label and addressed rows exclude first-action-only recipients: {latest}"
    );
    assert_survives_60_column_chat(&latest, &["later-label", "delivered", "Distribution board"]);

    let first = board_in_process(
        &mut sender,
        &mut state,
        &format!("/team board --action {first_id}"),
    )
    .await;
    assert!(first.contains("6 addressed"), "{first}");
    let row_lines: Vec<_> = first
        .lines()
        .filter(|line| {
            line.starts_with("  ✓ acknowledged")
                || line.starts_with("  ● delivered")
                || line.starts_with("  ✗ declined")
                || line.starts_with("  ⚠ preflight")
                || line.starts_with("    pending")
        })
        .collect();
    assert_eq!(row_lines.len(), 5, "unnameable row is withheld: {first}");
    for token in ["✓ acknowledged", "● delivered", "✗ declined", "⚠ preflight"] {
        assert!(first.contains(token), "missing {token}: {first}");
    }
    assert!(
        first.contains("pending") && first.contains("sending…"),
        "{first}"
    );
    assert!(
        !first.contains("future"),
        "unknown peer state is not echoed: {first}"
    );
    assert!(
        first.contains("1 roster peer reported an outcome this build cannot name"),
        "{first}"
    );
    assert!(
        !first.contains(" of ") && !first.contains("%"),
        "per-recipient rows remain, without aggregate substitutes: {first}"
    );
    for count in 0..=6 {
        assert!(
            !first.contains(&format!("{count} acknowledged")),
            "no acknowledged roll-up may replace individual rows: {first}"
        );
    }
    assert!(
        first.contains("six-label")
            && first.contains("✗ declined")
            && first.contains("⚠ preflight")
            && first.contains("pending")
            && first.contains("sending…"),
        "explicit rendered id keeps first action's decline, unreachable and live pending rows: {first}"
    );
    assert_survives_60_column_chat(
        &first,
        &[
            "six-label",
            "✗ declined",
            "⚠ preflight",
            "pending",
            "sending…",
            "no credential",
            "set the env var",
            "its auth field",
        ],
    );
    let reopened_view = board(
        &sender,
        peers.clone(),
        &mut state,
        &format!("/team board --action {first_id}"),
    )
    .await;
    assert!(
        reopened_view.contains("pending") && reopened_view.contains("no outcome recorded"),
        "without this process's live send, pending is S4 rather than sending: {reopened_view}"
    );
    assert!(
        !reopened_view.contains("sending…"),
        "a reopened reader has no live pending send: {reopened_view}"
    );
    assert_survives_60_column_chat(
        &reopened_view,
        &["pending", "no outcome recorded", "six-label"],
    );

    let no_binding = board(
        &sender,
        Vec::new(),
        &mut state,
        &format!("/team board --action {first_id}"),
    )
    .await;
    assert!(
        no_binding.contains("not read — no eligible binding for the recorded peer"),
        "the action says why a now-unconfigured peer was not read: {no_binding}"
    );
    assert_survives_60_column_chat(
        &no_binding,
        &[
            "not read",
            "no eligible binding",
            "recorded",
            "peer",
            "acknowledged",
            "declined",
            "preflight",
            "pending",
        ],
    );

    gate.release();
    sender.settle(&mut state, 1).await;
    acknowledged.stop().await;
    delivered.stop().await;
    declined.stop().await;
    preflight_host.stop().await;
    pending.stop().await;
}
/// A pre-19.21 workspace has sender dispatch/acceptance records but no typed
/// action record. **Mutants M29 and the S6 routing branch → RED.** Positive
/// control: bare `/team board` still finds the correlated legacy item through
/// the real inventory front door.
#[tokio::test]
async fn legacy_dispatch_rows_remain_inventory_only_and_action_selectors_render_s6() {
    let host = Host::start(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let mut sender = Sender::start(vec![host.peer("legacy-peer")]).await;
    let task = "legacy-task-19-21";
    seed_recipient_item(&host.journal, "ri_legacy", task).await;
    let peer = rustain::domain::models::a2a_peer_spec::alias_pseudonym("legacy-peer");
    sender
        .journal
        .room
        .record_event(
            rustain::domain::models::RoomEvent::RemoteEnvelopeDispatched {
                peer: peer.clone(),
                task: Some(task.to_owned()),
                bytes: 1,
                act: rustain::domain::models::DispatchAct::Task,
                content_hash: None,
                endpoint: None,
                action: None,
                authorship: rustain::domain::models::ReplyAuthorship::Unknown,
            },
        )
        .await
        .expect("seed a legacy actionless dispatch through the real journal");
    sender
        .journal
        .room
        .record_event(rustain::domain::models::RoomEvent::RemoteEnvelopeAccepted {
            peer,
            node: rustain::domain::models::AgentId::root(),
            content_hash: rustain::domain::models::ContentHash::from_bytes([0x19; 32]),
            direction: rustain::domain::models::Direction::Outbound,
            task: Some(task.to_owned()),
        })
        .await
        .expect("seed the corresponding legacy accepted row");

    let mut state = TuiState::new(100, 40);
    let last = board_in_process(&mut sender, &mut state, "/team board --last").await;
    assert!(
        last.contains("· no send action recorded in this workspace"),
        "{last}"
    );
    let actions = board_in_process(&mut sender, &mut state, "/team board --actions").await;
    assert!(
        actions.contains("· no send action recorded in this workspace"),
        "{actions}"
    );
    assert!(
        last.contains("'/team board' reads the peer item inventory."),
        "legacy action selectors keep the S6 inventory remedy: {last}"
    );
    assert_survives_60_column_chat(
        &last,
        &["no send action recorded", "reads the peer item inventory"],
    );
    assert_survives_60_column_chat(
        &actions,
        &["no send action recorded", "reads the peer item inventory"],
    );
    let inventory = board_in_process(&mut sender, &mut state, "/team board").await;
    assert!(inventory.contains("Peer item inventory"), "{inventory}");
    assert!(
        inventory.contains("item ri_legacy") && inventory.contains("legacy-peer"),
        "the actionless dispatch still correlates on the inventory rail: {inventory}"
    );
    host.stop().await;
}

/// Render the standalone S2 failed-read copy at the chat pane's 60-column
/// floor; the peer was a real wire recipient before its list endpoint failed.
#[tokio::test]
async fn failed_read_keeps_s2_copy_at_sixty_columns() {
    let down = controlled_peer("down-read", false, Some("received")).await;
    let mut sender = Sender::start(vec![down.spec.clone()]).await;
    let mut state = TuiState::new(100, 40);
    issue(
        &mut sender,
        &mut state,
        "/team send down-read copy-width-body",
        1,
    )
    .await;
    down.server.reset().await;

    let rendered = board_in_process(&mut sender, &mut state, "/team board --last").await;
    assert!(
        rendered.contains("down-read") && rendered.contains("not readable now"),
        "a failed list read renders S2 without changing its landed send outcome: {rendered}"
    );
    assert_survives_60_column_chat(
        &rendered,
        &["down-read", "not readable now", "Distribution board"],
    );
}

/// Hold the actual settlement append, then write the forward-compatible
/// durable outcome through the same journal so the board's live path renders
/// S8 instead of `sending…`.
#[tokio::test]
async fn unrecognized_settlement_keeps_s8_copy_at_sixty_columns() {
    let peer = controlled_peer("unknown-settlement", false, None).await;
    let journal = Journal::open().await;
    let gate = std::sync::Arc::new(GateRoomJournal::new(journal.room.clone(), |event| {
        matches!(
            event,
            rustain::domain::models::RoomEvent::TeamSendRecipientSettled { .. }
        )
    }));
    let sink: std::sync::Arc<dyn rustain::domain::ports::RoomJournal> = gate.clone();
    let mut sender = Sender::with_journal(vec![peer.spec.clone()], sink, journal.clone()).await;
    let mut state = TuiState::new(100, 40);
    sender
        .command(
            &mut state,
            "/team send unknown-settlement held-unknown-body",
        )
        .await;
    gate.wait_started().await;
    let actions = board_in_process(&mut sender, &mut state, "/team board --actions").await;
    let action_id = actions
        .lines()
        .find_map(|line| {
            let (id, _) = line.split_once("  ")?;
            id.starts_with("sa_").then(|| id.to_owned())
        })
        .expect("the production --actions output exposes the generated id");
    let action = rustain::domain::models::SendActionId::from_replay(action_id);
    sender
        .journal
        .room
        .record_event(
            rustain::domain::models::RoomEvent::TeamSendRecipientSettled {
                action,
                index: 0,
                settlement: rustain::domain::models::SendSettlement::Unknown,
            },
        )
        .await
        .expect("append the newer-build settlement to the real workspace journal");

    let rendered = board_in_process(&mut sender, &mut state, "/team board --last").await;
    assert!(
        rendered.contains("unknown-settlement")
            && rendered.contains("recorded outcome not recognised by this build"),
        "S8 keeps the closed outcome set visible: {rendered}"
    );
    assert!(
        !rendered.contains("sending…"),
        "a durable unknown settlement is not live pending: {rendered}"
    );
    assert_survives_60_column_chat(
        &rendered,
        &[
            "unknown-settlement",
            "recorded outcome",
            "not recognised",
            "by this build",
        ],
    );
    gate.release();
    sender.settle(&mut state, 1).await;
}

/// Hold the actual settlement append and write NoUsableAnswer into the real
/// journal; the empty correlated remote list then produces S3 through the
/// production board command.
#[tokio::test]
async fn no_usable_answer_keeps_s3_copy_at_sixty_columns() {
    let peer = controlled_peer("no-answer", false, None).await;
    let journal = Journal::open().await;
    let gate = std::sync::Arc::new(GateRoomJournal::new(journal.room.clone(), |event| {
        matches!(
            event,
            rustain::domain::models::RoomEvent::TeamSendRecipientSettled { .. }
        )
    }));
    let sink: std::sync::Arc<dyn rustain::domain::ports::RoomJournal> = gate.clone();
    let mut sender = Sender::with_journal(vec![peer.spec.clone()], sink, journal.clone()).await;
    let mut state = TuiState::new(100, 40);
    sender
        .command(&mut state, "/team send no-answer held-no-answer-body")
        .await;
    gate.wait_started().await;
    let actions = board_in_process(&mut sender, &mut state, "/team board --actions").await;
    let action_id = actions
        .lines()
        .find_map(|line| {
            let (id, _) = line.split_once("  ")?;
            id.starts_with("sa_").then(|| id.to_owned())
        })
        .expect("the production --actions output exposes the generated id");
    let action = rustain::domain::models::SendActionId::from_replay(action_id);
    sender
        .journal
        .room
        .record_event(
            rustain::domain::models::RoomEvent::TeamSendRecipientSettled {
                action,
                index: 0,
                settlement: rustain::domain::models::SendSettlement::NoUsableAnswer,
            },
        )
        .await
        .expect("append the fixture settlement to the real workspace journal");

    let rendered = board_in_process(&mut sender, &mut state, "/team board --last").await;
    assert!(
        rendered.contains("no-answer")
            && rendered.contains("no usable answer")
            && rendered.contains("no correlated item reported on this read"),
        "S3 states no correlated item and distinguishes the successful empty read: {rendered}"
    );
    assert_survives_60_column_chat(
        &rendered,
        &[
            "no-answer",
            "no usable answer",
            "no correlated item reported",
            "on this read",
        ],
    );
    gate.release();
    sender.settle(&mut state, 1).await;
}
