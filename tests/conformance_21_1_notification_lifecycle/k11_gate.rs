//! K11 — the cutover gate and the class-V views (AC12, AC13; K11a in S1).
//!
//! A scoped static check over the production code of BOTH hosts. It is a ratchet,
//! not behavioral proof (K5/K6/K10 drive the real producers): the deleted
//! mechanisms must not come back, `StatusState::Flash` is gone from every file,
//! and `show_view` is the one production writer of `feedback_blocks`.
//!
//! Gate hygiene (21-11 K7 pattern): `checked == total` (every `.rs` file under
//! `src/` is scanned, none skipped), the one exemption (`views.rs`) must still
//! contain its writer (a stale exemption fails), and in-memory positive controls
//! prove the scanner sees what it bans — including rustfmt's multi-line forms.

use std::path::{Path, PathBuf};

use rustain::adapters::tui::views::{ViewKind, in_render_order, show_view};
use rustain::domain::models::FeedbackLevel;

/// Production part of a source: everything before its trailing test module.
fn production_part(source: &str) -> &str {
    source
        .find("\n#[cfg(test)]\nmod ")
        .map_or(source, |at| &source[..at])
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// Tokens deleted by S1 (AC4) and S2 (AC12/AC13): no production file may mention them.
const DELETED: &[&str] = &[
    // S1
    "status_before_flash",
    "queued_notifications",
    "QueuedNotification",
    "notify_or_queue",
    "auto_switch_to_monitor_on_error",
    "apply_warning_notice",
    "store_background_notice",
    "notification_hold_ms",
    "flash_copy",
    "drain_queued_notifications_for_test",
    // S2
    "StatusState::Flash",
    "active_feedback_id",
    "FeedbackAction",
    "chord_leader_active",
    "InputAction::FeedbackRetry",
    "InputAction::FeedbackCompact",
    "InputAction::FeedbackDismiss",
    "InputAction::FeedbackStartFresh",
    "InputAction::FeedbackBudget",
    "begin_bulk_delete_confirmation",
    "ConfirmationType::DeleteConfirmation",
    "RecoveryChoice",
    "recovery_block",
];

/// The one file allowed to write the view map, and the writer it must keep.
const VIEW_WRITER: (&str, &str) = ("src/adapters/tui/views.rs", "pub fn show_view");

/// A `feedback_blocks` write outside `show_view`, tolerant of rustfmt's line
/// breaks (`state\n.feedback_blocks\n.retain(`).
fn writes_view_map(source: &str) -> Vec<&'static str> {
    let compact: String = source.split_whitespace().collect();
    [
        "feedback_blocks.insert(",
        "feedback_blocks.entry(",
        "feedback_blocks.retain(",
    ]
    .into_iter()
    .filter(|needle| compact.contains(needle))
    .collect()
}

/// Scan `files` (path, source) and return violation lines.
fn scan(files: &[(String, String)]) -> Vec<String> {
    let mut violations = Vec::new();
    for (path, source) in files {
        let production = production_part(source);
        for token in DELETED {
            if production.contains(token) {
                violations.push(format!("{path}: deleted mechanism `{token}` is back"));
            }
        }
        if !path.ends_with(VIEW_WRITER.0) {
            for needle in writes_view_map(production) {
                violations.push(format!("{path}: `{needle}` outside `show_view`"));
            }
        }
    }
    violations
}

fn production_sources() -> (Vec<(String, String)>, usize) {
    let mut paths = Vec::new();
    rust_files(Path::new("src"), &mut paths);
    paths.sort();
    let total = paths.len();
    let files = paths
        .into_iter()
        // `*_tests.rs` files are `#[cfg(test)]` modules: not production code.
        .filter(|p| {
            !p.file_name()
                .is_some_and(|n| n.to_string_lossy().ends_with("_tests.rs"))
        })
        .map(|p| {
            let source = std::fs::read_to_string(&p).unwrap();
            (p.to_string_lossy().replace('\\', "/"), source)
        })
        .collect();
    (files, total)
}

#[test]
fn k11_production_code_of_both_hosts_carries_no_deleted_mechanism_and_one_view_writer() {
    let (files, total) = production_sources();
    // checked == total: the only files not scanned are the `*_tests.rs` test modules.
    let test_modules = total - files.len();
    assert!(
        files.len() > 100 && test_modules < 20,
        "{} scanned of {total}: the scan must cover the whole source tree",
        files.len()
    );
    for must in [
        "src/infrastructure/runtime/event_loop.rs",
        "src/infrastructure/runtime/attach_loop.rs",
        "src/infrastructure/runtime/attach_keys.rs",
        "src/adapters/tui/state.rs",
        "src/adapters/tui/app.rs",
        "src/adapters/tui/notifications.rs",
        "src/adapters/tui/decision_queue/mod.rs",
        "src/domain/models/notice.rs",
        "src/domain/models/tab.rs",
    ] {
        assert_eq!(
            files.iter().filter(|(p, _)| p == must).count(),
            1,
            "{must} must be scanned exactly once"
        );
    }
    let violations = scan(&files);
    assert!(violations.is_empty(), "{violations:#?}");
}

#[test]
fn k11_no_stale_exemption_the_one_view_writer_still_exists_exactly_once() {
    let (files, _total) = production_sources();
    let (suffix, writer) = VIEW_WRITER;
    let (path, source) = files
        .iter()
        .find(|(p, _)| p.ends_with(suffix))
        .unwrap_or_else(|| panic!("exempt file {suffix} no longer exists"));
    assert_eq!(
        production_part(source).matches(writer).count(),
        1,
        "stale exemption: {path} must define `{writer}` exactly once"
    );
    // The exemption is real: the exempt file does write the map (through its parameter).
    assert!(production_part(source).contains("blocks.insert("));
    // And no other file defines a second writer.
    let others = files
        .iter()
        .filter(|(p, s)| !p.ends_with(suffix) && production_part(s).contains(writer))
        .count();
    assert_eq!(others, 0);
}

#[test]
fn k11_positive_control_the_scanner_sees_what_it_bans() {
    let bad = vec![
        (
            "src/x/new_file.rs".to_owned(),
            "fn f(s: &mut S) { s.status = StatusState::Flash { message: m, remaining_ms: 1 }; }"
                .to_owned(),
        ),
        (
            "src/y/old_queue.rs".to_owned(),
            "fn g() { notify_or_queue(state, kind); }".to_owned(),
        ),
        (
            "src/z/slot.rs".to_owned(),
            "fn h() { state.active_feedback_id = None; let _ = FeedbackAction::Retry; }".to_owned(),
        ),
        (
            "src/w/single_line.rs".to_owned(),
            "fn i() { state.feedback_blocks.insert(id, block); }".to_owned(),
        ),
        (
            // rustfmt's multi-line form (found in the old budget handler).
            "src/w/multi_line.rs".to_owned(),
            "fn j() {\n    state\n        .feedback_blocks\n        .retain(|id, _| keep(id));\n}"
                .to_owned(),
        ),
        (
            "src/w/entry.rs".to_owned(),
            "fn k() { tab.feedback_blocks.entry(id).or_insert(block); }".to_owned(),
        ),
        (
            "src/v/chord.rs".to_owned(),
            "fn l() { state.chord_leader_active = true; }".to_owned(),
        ),
    ];
    let violations = scan(&bad);
    assert_eq!(violations.len(), 8, "{violations:#?}");
    // The exempt writer file is allowed to write its parameter map…
    let exempt = vec![(
        "src/adapters/tui/views.rs".to_owned(),
        "pub fn show_view(blocks: &mut M) { blocks.insert(id, b); }".to_owned(),
    )];
    assert!(scan(&exempt).is_empty());
    // …but the same text anywhere else is a violation, and trailing test modules
    // are not production.
    let elsewhere = vec![(
        "src/adapters/tui/handlers/other.rs".to_owned(),
        "fn x() { state.feedback_blocks.insert(a, b); }".to_owned(),
    )];
    assert_eq!(scan(&elsewhere).len(), 1);
    let in_tests = vec![(
        "src/z.rs".to_owned(),
        "fn ok() {}\n#[cfg(test)]\nmod tests { fn t() { let _ = StatusState::Flash; s.feedback_blocks.insert(a, b); } }"
            .to_owned(),
    )];
    assert!(scan(&in_tests).is_empty());
}

// ── class-V views keep their order and their commit-on-present ────────────────

#[test]
fn k11_the_view_shown_last_renders_last_in_the_real_chat_pane_and_reports_its_ids_in_order() {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use rustain::adapters::tui::state::TabRenderState;
    use rustain::adapters::tui::theme::Theme;
    use rustain::adapters::tui::widgets::chat_pane;
    use rustain::domain::models::{Conversation, StreamingState};
    use std::collections::BTreeMap;

    let mut blocks = BTreeMap::new();
    // Alphabetical order is the OPPOSITE of arrival order: `z-first` arrived first.
    show_view(
        &mut blocks,
        ViewKind::TeamStatus,
        "z-first",
        FeedbackLevel::Info,
        "FIRST view",
    );
    show_view(
        &mut blocks,
        ViewKind::TeamBoard,
        "a-second",
        FeedbackLevel::Info,
        "SECOND view",
    );
    let order: Vec<&str> = in_render_order(&blocks).map(|b| b.id.as_str()).collect();
    assert_eq!(order, ["z-first", "a-second"]);

    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    let conversation = Conversation::default();
    let theme = Theme::dark();
    let mut visible = Vec::new();
    terminal
        .draw(|frame| {
            let result = chat_pane::render(
                frame,
                frame.area(),
                &conversation,
                &StreamingState::default(),
                0,
                true,
                &theme,
                &mut TabRenderState::default(),
                &blocks,
            );
            visible = result
                .visible_feedback_rows
                .into_iter()
                .map(|(id, _)| id)
                .collect();
        })
        .unwrap();
    let buffer = terminal.backend().buffer().clone();
    let rows: Vec<String> = (0..24)
        .map(|y| {
            (0..80)
                .map(|x| buffer.cell((x, y)).unwrap().symbol().to_owned())
                .collect::<String>()
        })
        .collect();
    let first = rows
        .iter()
        .position(|r| r.contains("FIRST view"))
        .expect("first painted");
    let second = rows
        .iter()
        .position(|r| r.contains("SECOND view"))
        .expect("second painted");
    assert!(first < second, "the newest view renders last: {rows:#?}");
    assert_eq!(
        visible,
        ["z-first", "a-second"],
        "commit-on-present sees them in order"
    );
}
