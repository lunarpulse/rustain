//! Story 18.2 Cluster B — the surfacing keystones (AC4, AC5, AC6, AC8).
//!
//! Each of these drives a **production entry point**: the real chord map, the
//! real CLI renderer. The forbidden bypasses are called out at each test —
//! they are the shapes that would pass while the feature was dead in
//! production (`DF-CR-14-3a-1`: render fns shipped with zero production call
//! sites and the tasks were checked `[x]`).

use rustain::adapters::tui::app::{InputAction, handle_input, submit_message_for_test};
use rustain::adapters::tui::state::TuiState;
use rustain::domain::events::{DomainEventPayload, DomainInputEvent, DomainKey};
use rustain::domain::models::node_journal::{JournalEntry, JournalRecord};
use rustain::domain::models::visual::PanelType;
use rustain::domain::models::{Direction, PeerId, RejectReason, RoomEvent};
use rustain::domain::services::transparency::{
    STRUCTURAL_REPLAY_CLAIM, TransparencyExport, TransparencyReport, fold_transparency,
    validate_replay_structure,
};
// ── AC5 — the chord is real, not a Noop ─────────────────────────────────────

/// Mutant: leave `ChordAction::Noop("Log panel — Epic 14")` at
/// `state.rs`'s chord map. A rendered, documented binding backed by a `Noop` is
/// what shipped before this story; this keystone is what makes it impossible
/// to ship again.
#[test]
fn ctrl_x_l_dispatches_the_transparency_log_panel() {
    let mut state = TuiState::new(160, 24);
    handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::CtrlX));
    assert!(state.which_key.active);

    let action = handle_input(&mut state, &DomainInputEvent::KeyPress('l'));
    assert_eq!(
        action,
        InputAction::OpenPanel(PanelType::TransparencyLog),
        "Ctrl+X, L must open the real panel"
    );
    assert!(!state.which_key.active);
    // A `Noop` chord inserts a `chord-l` feedback block instead of dispatching.
    assert!(
        !state.feedback_blocks.contains_key("chord-l"),
        "a Noop chord would have left a 'not yet available' block behind"
    );
}

#[test]
fn the_help_screen_advertises_the_panel_by_its_real_name() {
    let categories = rustain::adapters::tui::help_data::help_categories();
    let binding = categories
        .iter()
        .flat_map(|category| category.bindings.iter())
        .find(|binding| binding.key == "Ctrl+X, L")
        .expect("Ctrl+X, L is documented");
    assert!(binding.available, "the binding is real, so it is available");
    assert_eq!(
        binding.description, "Transparency Log panel",
        "the help text must name the spec's surface, not a stale 'Epic 14' label"
    );
}

// ── AC6 — `/team log` reaches the dispatch arm ──────────────────────────────

/// **The mutant that is the whole point.** Remove the `team` entry from the
/// `submit_message` allowlist and `/team log` falls through to
/// `SubmitWithContext`, resolves no command file, and silently never runs — a
/// handler-only test would stay green while the command was dead.
#[test]
fn slash_team_log_is_routed_as_an_execute_command_not_a_missing_custom_command() {
    for input in ["/team log", "/team", "/team log --json --export"] {
        let mut state = TuiState::new(120, 24);
        state.input_buffer = input.to_owned();
        let action = submit_message_for_test(&mut state);
        match action {
            InputAction::ExecuteCommand { name, args } => {
                assert_eq!(name, "team", "input {input:?}");
                let joined = args.clone().unwrap_or_default();
                assert!(
                    input.trim_start_matches("/team").trim().is_empty() || joined.contains("log"),
                    "the sub-verb must survive the parser: {input:?} → {args:?}"
                );
            }
            other => panic!("`{input}` must reach the /team dispatch arm, got {other:?}"),
        }
    }
}

#[test]
fn the_palette_row_names_the_direct_chord() {
    // Graduation convention: a discoverable command that hides its faster path
    // teaches the slow one.
    let registry = rustain::adapters::command_registry::CommandRegistry::new();
    let mut palette = rustain::adapters::palette_registry::PaletteRegistry::new();
    palette.populate_from_command_registry(&registry);
    let entry = palette
        .all_entries()
        .iter()
        .find(|entry| entry.name == "/team")
        .cloned()
        .expect("/team is in the palette");
    assert_eq!(entry.shortcut.as_deref(), Some("Ctrl+X, L"));
    assert!(entry.description.contains("transparency"), "{entry:?}");
}

// ── AC6/AC8 — the CLI face, asserted on BYTES ───────────────────────────────

fn hostile_entry(seq: u64) -> JournalEntry {
    hostile_entry_with_direction(seq, Direction::Outbound)
}

fn hostile_entry_with_direction(seq: u64, direction: Direction) -> JournalEntry {
    JournalEntry::new(
        seq,
        JournalRecord::Room(RoomEvent::RemoteEnvelopeRejected {
            peer: PeerId::from_public_key(&[4u8; 32]).unwrap(),
            reason: RejectReason::Policy {
                // A would-be escape sequence, of the shape a remote peer can
                // put in `task.id` on the outbound path.
                detail: "remote agent requested input (task \u{1b}[2K\rWIPED\u{7}, context \
                         \u{9b}2J)"
                    .to_owned(),
            },
            direction,
            task: None,
        }),
        1_700_000_000_000 + seq as i64,
    )
}

fn team_log_stdout(json: bool, filter: Option<&str>, entries: Vec<JournalEntry>) -> Vec<u8> {
    let report = TransparencyReport {
        findings: validate_replay_structure(&entries),
        rows: fold_transparency(&entries),
    };
    let mut out = Vec::new();
    rustain::adapters::cli::team::log::render_team_log(filter, json, &report, None, &mut out)
        .expect("team log renderer succeeds");
    out
}

/// AC8, the sink this story creates. `rustain team log` is a real `println!`
/// to a terminal, unlike every prior consumer (ratatui, which renders escapes
/// as inert glyphs).
///
/// Asserted on **bytes**, deliberately. `tests_tui/` scrapes a pyte screen and
/// pyte *interprets* escape sequences — that assertion would pass no matter
/// what we printed.
#[test]
fn team_log_stdout_carries_no_control_bytes() {
    for json in [false, true] {
        let bytes = team_log_stdout(json, None, vec![hostile_entry(1)]);
        let offenders: Vec<u8> = bytes
            .iter()
            .copied()
            .filter(|byte| (*byte < 0x20 && *byte != b'\n') || *byte == 0x7f)
            .collect();
        assert!(
            offenders.is_empty(),
            "json={json}: stdout must carry no C0/DEL byte, found {offenders:?}"
        );
        let text = String::from_utf8(bytes).expect("stdout is utf-8");
        assert!(
            !text.chars().any(|ch| ('\u{80}'..='\u{9f}').contains(&ch)),
            "json={json}: stdout must carry no C1 character"
        );
        assert!(
            text.contains("WIPED"),
            "json={json}: the text survives; only the control bytes are removed"
        );
    }
}

#[test]
fn both_faces_render_the_same_rows_from_the_same_fold() {
    let entries = vec![hostile_entry(1), hostile_entry(2)];
    let fold = fold_transparency(&entries);

    // CLI `--json` is the export body verbatim…
    let cli = String::from_utf8(team_log_stdout(true, None, entries.clone())).unwrap();
    for row in &fold {
        assert!(cli.contains(&row.summary), "CLI row missing: {row:?}");
        assert!(cli.contains(row.direction.label()));
    }

    // …and the slash command renders the identical rows.
    let slash = rustain::adapters::tui::handlers::team_command::render_rows(&fold, true);
    assert_eq!(
        slash,
        rustain::domain::services::transparency::render_export(&fold),
        "the two faces must not have two renderers"
    );
}

#[test]
fn a_bad_filter_refuses_instead_of_silently_matching_everything() {
    let entries = vec![hostile_entry(1)];
    let report = TransparencyReport {
        findings: validate_replay_structure(&entries),
        rows: fold_transparency(&entries),
    };
    let mut out = Vec::new();
    let error = rustain::adapters::cli::team::log::render_team_log(
        Some("direction=sideways"),
        false,
        &report,
        None,
        &mut out,
    )
    .expect_err("an unknown filter value must refuse");
    assert!(error.to_string().contains("inbound, outbound, unknown"));
    assert!(
        out.is_empty(),
        "nothing may be printed when the filter was rejected"
    );
}

#[test]
fn the_cli_uses_the_structural_replay_claim() {
    let text =
        String::from_utf8(team_log_stdout(false, None, vec![hostile_entry(1)])).expect("utf-8");
    assert!(text.contains(STRUCTURAL_REPLAY_CLAIM));
    assert!(
        text.contains("credential scheme"),
        "attribution honesty note (DF-18-1-MTLS) must be present: {text}"
    );
}

#[test]
fn cli_export_summary_reports_the_unfiltered_snapshot_count() {
    let entries = vec![
        hostile_entry_with_direction(1, Direction::Inbound),
        hostile_entry_with_direction(2, Direction::Outbound),
    ];
    let report = TransparencyReport {
        findings: validate_replay_structure(&entries),
        rows: fold_transparency(&entries),
    };
    let export = TransparencyExport {
        path: std::path::PathBuf::from(".rustain/transparency.jsonl"),
        rows: report.rows.len(),
        findings: report.findings.clone(),
    };
    let mut out = Vec::new();

    rustain::adapters::cli::team::log::render_team_log(
        Some("direction=inbound"),
        false,
        &report,
        Some(&export),
        &mut out,
    )
    .unwrap();

    let text = String::from_utf8(out).unwrap();
    assert!(text.contains("export: .rustain/transparency.jsonl (2 rows)"));
    assert!(text.contains("inbound"));
    assert!(
        !text.contains("outbound"),
        "the rendered rows are filtered while the export count remains unfiltered"
    );
}

// ── AC4 — the chat notice, through the production handler ───────────────────

/// Mutant: delete the `AppEvent::DomainEvent` arm from `event_loop.rs`. The
/// event falls back into the catch-all and reaches nothing — which is what
/// happened for every room event before this story.
#[test]
fn a_room_event_produces_exactly_one_persistent_transcript_line() {
    let mut state = TuiState::new(120, 24);
    let payload = DomainEventPayload::Room(RoomEvent::RemoteEnvelopeRejected {
        peer: PeerId::from_public_key(&[8u8; 32]).unwrap(),
        reason: RejectReason::Policy {
            detail: "refused by server.admission policy".to_owned(),
        },
        direction: Direction::Inbound,
        task: None,
    });
    assert!(
        rustain::adapters::tui::handlers::transparency::apply_domain_event(&mut state, &payload)
    );
    assert_eq!(state.feedback_blocks.len(), 1);
    let block = state.feedback_blocks.values().next().unwrap();
    assert!(
        block.message.contains("server.admission"),
        "{}",
        block.message
    );
    // Persistent: a FeedbackBlock scrolls with the conversation and never
    // auto-dismisses. An `Info` SystemNotice would have been a transient
    // status-bar flash instead — invisible three seconds later.
    assert!(
        !matches!(
            state.status,
            rustain::domain::models::StatusState::Flash { .. }
        ),
        "the notice must be a transcript block, not a status flash"
    );
}

// ── AC5 — panel open/toggle/narrow, through the real dispatch shape ─────────

fn open_panel(state: &mut TuiState) {
    // Mirrors the event_loop `InputAction::OpenPanel` generic branch.
    state.sidebar_visible = true;
    state.sidebar_panel = Some(PanelType::TransparencyLog);
    state.focus = rustain::domain::models::FocusState::Sidebar {
        panel: PanelType::TransparencyLog,
        selected: 0,
    };
}

#[test]
fn the_panel_renders_an_honest_empty_state_not_a_dead_screen() {
    let mut state = TuiState::new(160, 40);
    open_panel(&mut state);
    let buffer = render_panel(&mut state);
    let text = buffer_text(&buffer);
    assert!(text.contains("Transparency Log"), "{text}");
    assert!(text.contains("no A2A interactions recorded"), "{text}");
    assert!(
        !text.contains("live"),
        "the panel must never present itself as live: {text}"
    );
}

#[test]
fn the_panel_shows_a_newer_entries_affordance_instead_of_moving_the_viewport() {
    let mut state = TuiState::new(160, 40);
    open_panel(&mut state);
    let initial_entries = (1..=25).map(hostile_entry).collect::<Vec<_>>();
    let rows = rustain::domain::services::transparency::fold_transparency(&initial_entries);
    state.transparency_panel.apply_read(rows, 100);
    // The operator scrolls — now they are reading.
    state.transparency_panel.scroll_offset = 1;
    let refreshed_entries = (1..=30).map(hostile_entry).collect::<Vec<_>>();
    let more = rustain::domain::services::transparency::fold_transparency(&refreshed_entries);
    state.transparency_panel.apply_read(more, 200);

    assert_eq!(state.transparency_panel.scroll_offset, 1);
    let text = buffer_text(&render_panel(&mut state));
    assert!(
        text.contains("newer"),
        "a panel that silently withholds rows lies about being current: {text}"
    );
}

#[test]
fn the_panel_states_the_scoped_integrity_claim_and_never_overclaims() {
    let mut state = TuiState::new(200, 40);
    open_panel(&mut state);
    let rows = rustain::domain::services::transparency::fold_transparency(&[hostile_entry(1)]);
    state.transparency_panel.apply_read(rows, 100);
    let text = buffer_text(&render_panel(&mut state));
    assert!(text.contains("append-only"), "{text}");
    assert!(
        !text.contains("tamper-evident") || text.contains("not cryptographically tamper-evident"),
        "the panel must not claim cryptographic tamper-evidence: {text}"
    );
}

fn render_panel(state: &mut TuiState) -> ratatui::buffer::Buffer {
    let area = ratatui::layout::Rect::new(0, 0, 100, 20);
    let mut buffer = ratatui::buffer::Buffer::empty(area);
    rustain::adapters::tui::widgets::transparency_panel::render(
        area,
        &mut buffer,
        &mut state.transparency_panel,
        0,
        &state.focus,
        &state.theme,
    );
    buffer
}

fn buffer_text(buffer: &ratatui::buffer::Buffer) -> String {
    buffer
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>()
}
/// Wiring ratchet for the class-B hole (Rule 0 / Rule 4).
///
/// `a_room_event_produces_exactly_one_persistent_transcript_line` proves the
/// handler BEHAVES; it cannot prove the event loop CALLS it. Driving a real
/// room event through the real `event_loop::run` needs an end-to-end
/// bus→TUI harness that does not exist (`DF-CR-14-4a-6`), so the wiring half
/// is proven structurally instead of waved through — the Rule-4 answer to a
/// mutant a behavioural test cannot reach.
///
/// This is NOT a caller-count assertion standing in for behaviour: the
/// behaviour is asserted above, against the same function this pins.
///
/// Mutant: delete the arm. Room events fall back into the `_ =>` catch-all and
/// reach nothing, exactly as they did for every room path before Story 18.2.
#[test]
fn the_domain_event_arm_is_wired_into_the_event_loop() {
    let source = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/infrastructure/runtime/event_loop.rs"
    ))
    .expect("read event_loop.rs");
    assert!(
        source.contains("AppEvent::DomainEvent(payload) => {"),
        "the `AppEvent::DomainEvent` dispatch arm is gone — room events reach nothing"
    );
    assert!(
        source.contains("handlers::transparency::apply_domain_event(&mut state, &payload)"),
        "the arm must call the production handler, not re-implement it"
    );
}

/// Same class, for the `/team` command: without the arm the allowlist entry
/// routes `ExecuteCommand { name: \"team\" }` into the adapter-override path,
/// which answers "unknown adapter 'log'".
#[test]
fn the_team_dispatch_arm_precedes_the_adapter_override_catch_all() {
    let source = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/infrastructure/runtime/event_loop.rs"
    ))
    .expect("read event_loop.rs");
    let team = source
        .find(r#"else if cmd_name == "team""#)
        .expect("the /team dispatch arm exists");
    let override_arm = source
        .find("port_dimension_from_command_name(cmd_name)")
        .expect("the adapter-override catch-all exists");
    assert!(
        team < override_arm,
        "`/team` must be intercepted BEFORE the adapter-override path"
    );
}
