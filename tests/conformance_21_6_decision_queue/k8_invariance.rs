//! K8 (slice 6-C): composer and Stage invariance across every kind of the slice, the
//! frame's computed extent equals its painted extent, model-originated text is
//! sanitized on paint while the effect and Copy receive the original bytes, and Copy
//! honors `COPY_MAX_BYTES`. Every negative has a positive control.

use std::sync::Arc;

use ratatui::layout::Rect;
use rustain::adapters::tui::app::InputAction;
use rustain::adapters::tui::decision_queue::{
    self, DecisionKey, DecisionKind, PaneOutcome, handle_domain_event,
};
use rustain::adapters::tui::keymap::Host;
use rustain::adapters::tui::notifications::NotificationStore;
use rustain::adapters::tui::presentation::{self, RouteEffect, Surface, geometry, reducer};
use rustain::adapters::tui::state::TuiState;
use rustain::adapters::tui::tool_inspector::source::COPY_MAX_BYTES;
use rustain::adapters::tui::widgets::plan_card::plan_card_height;
use rustain::domain::clock::MockClock;
use rustain::domain::events::DomainKey;
use rustain::domain::models::{
    ChatMessage, ContentBlockType, FocusState, MessageRole, NotificationsConfig, TabManager,
    generate_message_id,
};
use unicode_width::UnicodeWidthStr;

use super::raisers::{ALL_6C, HOSTILE, PLAIN, deciding_key, plan, raise};
use super::support::{
    advance, clocked, facts, key, layout_of, paint, ready, route, row_text, screen, special,
};
use crate::e2e_harness::TestHarness;

const CONTROLS: [char; 5] = ['\u{1b}', '\u{7}', '\u{85}', '\u{9b}', '\u{202e}'];
/// `HOSTILE` after sanitation, spelled out here, not computed: whole CSI/OSC sequences
/// (`ESC[2J`, `ESC]8;;…BEL`), the C1 controls (0x85, 0x9b with its final byte) and the bidi override go.
const CLEANED: &str = "xyzwv";

fn draft() -> String {
    (0..12)
        .map(|n| format!("draft line {n}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The literal text each kind's body paints for `PLAIN` (a stable anchor per kind).
fn anchor(kind: DecisionKind) -> &'static str {
    match kind {
        DecisionKind::SkipCascade => "depend on it",
        DecisionKind::SpawnGate => "spawn 5?",
        DecisionKind::PlanApproval => "Plan Approval",
        DecisionKind::PlanDeviation => "Plan deviation",
        DecisionKind::Delegation => "Delegate Task",
        _ => PLAIN,
    }
}

fn item_key(state: &TuiState, kind: DecisionKind) -> DecisionKey {
    let item = state
        .decisions
        .items()
        .find(|item| item.kind == kind)
        .unwrap_or_else(|| panic!("{kind:?} is queued"));
    DecisionKey {
        kind: item.kind,
        source: item.source.clone(),
        conversation_id: item.conversation_id.clone(),
    }
}

/// Everything the Stage and the composer own, as bytes.
#[derive(Debug, PartialEq)]
struct Snap {
    buffer: String,
    cursor: usize,
    input_area: Rect,
    chat_pane: Rect,
    view: String,
    cache: Vec<String>,
    stage_height: usize,
    boundaries: Vec<usize>,
}

struct Rig {
    h: TestHarness,
    tabs: TabManager,
    clock: Arc<MockClock>,
}

/// A real Stage: one conversation `c1` hosting a pending plan card, a full composer
/// in Chat focus, a `MockClock` for the inert window.
fn rig(width: u16, height: u16) -> Rig {
    let mut h = TestHarness::with_size(width, height);
    let clock = Arc::new(MockClock::new(std::time::Instant::now()));
    h.state.notifications =
        NotificationStore::from_config(&NotificationsConfig::default(), clock.clone());
    let mut tabs = TabManager::new(tokio_util::sync::CancellationToken::new());
    let host = host_message("Here is the plan.");
    let mut conversation = std::mem::take(&mut h.conversation);
    conversation.id = "c1".into();
    let mut p = plan("Stage plan");
    p.host_message_id = Some(host.id.clone());
    conversation.messages = vec![host_message("question"), host];
    conversation.plans.insert(p.id.clone(), p);
    tabs.active_tab_mut().conversation = conversation;
    h.state.active_tab_id = tabs.active_tab_id();
    h.state.input_buffer = draft();
    h.state.cursor_position = 9;
    h.state.focus = FocusState::Chat;
    Rig { h, tabs, clock }
}

fn host_message(text: &str) -> ChatMessage {
    ChatMessage {
        id: generate_message_id(),
        role: MessageRole::Assistant,
        content: text.to_string(),
        content_blocks: vec![ContentBlockType::PlanCard],
        tool_calls: vec![],
        created_at: 1_700_000_000,
        token_count: None,
        stop_reason: None,
        synthetic: false,
        images: vec![],
        origin: rustain::domain::models::ChannelKind::Terminal,
        authorship: rustain::domain::models::Authorship::HumanWritten,
        send_authorization: rustain::domain::models::SendAuthorization::Unknown,
        retracted_at_ms: None,
        history_source: Default::default(),
    }
}

impl Rig {
    fn render(&mut self) {
        self.h.render_active_tab(&self.tabs);
        if presentation::is_open(&self.h.state, Surface::Decision) {
            decision_queue::note_painted(&mut self.h.state);
        }
    }

    fn ready(&mut self) {
        self.render();
        advance(&self.clock, decision_queue::INERT_AFTER_PAINT);
    }

    fn snap(&mut self) -> Snap {
        self.render();
        let state = &self.h.state;
        let layout = layout_of(state, state.terminal_width, state.terminal_height);
        let tab = self.tabs.active_tab();
        let cache = state
            .tab_render_states
            .get(&tab.id)
            .map(|render| {
                let mut keys: Vec<String> = render
                    .height_cache
                    .entries
                    .keys()
                    .map(|k| format!("turn:{k:?}"))
                    .chain(
                        render
                            .height_cache
                            .message_entries
                            .iter()
                            .map(|(k, h)| format!("msg:{k:?}={h}")),
                    )
                    .collect();
                keys.sort();
                keys
            })
            .unwrap_or_default();
        Snap {
            buffer: state.input_buffer.clone(),
            cursor: state.cursor_position,
            input_area: layout.input_area,
            chat_pane: layout.chat_pane,
            view: format!("{:?}", tab.view_state),
            cache,
            stage_height: state.total_content_height,
            boundaries: state.message_boundaries.clone(),
        }
    }

    fn press(&mut self, event: &rustain::domain::events::DomainInputEvent) -> Option<RouteEffect> {
        route(&mut self.h.state, event, &facts("c1"))
    }

    /// Esc until the frame closes (a sub-context backs first); the item must remain.
    fn defer(&mut self, kind: DecisionKind) {
        for _ in 0..3 {
            if !presentation::is_open(&self.h.state, Surface::Decision) {
                break;
            }
            let _ = self.press(&special(DomainKey::Esc));
        }
        assert!(
            !presentation::is_open(&self.h.state, Surface::Decision),
            "{kind:?}: Esc defers within three presses"
        );
        assert!(
            self.h.state.decisions.holds(kind),
            "{kind:?}: deferring never drops the item"
        );
    }
}

/// Keys that must never reach the composer, whatever the kind does with them.
fn poke(rig: &mut Rig, kind: DecisionKind) {
    for c in ['z', '1', '!', ' '] {
        let _ = rig.press(&key(c));
    }
    for k in [
        DomainKey::PageDown,
        DomainKey::PageUp,
        DomainKey::Down,
        DomainKey::Up,
        DomainKey::Tab,
        DomainKey::Left,
        DomainKey::Right,
        DomainKey::Home,
        DomainKey::End,
        DomainKey::CtrlA,
        DomainKey::CtrlE,
        DomainKey::CtrlK,
        DomainKey::Backspace,
    ] {
        let _ = rig.press(&special(k));
    }
    if kind == DecisionKind::SkillTrust {
        // `i` asks the host to load the file; the host answers with `open_inspect`.
        let _ = rig.press(&key('i'));
        let item = item_key(&rig.h.state, kind);
        decision_queue::open_inspect(&mut rig.h.state, &item, "line one\nline two".into());
        rig.render();
        for k in [DomainKey::PageDown, DomainKey::Down, DomainKey::PageUp] {
            let _ = rig.press(&special(k));
        }
        // Esc goes back to the undecided trust item, never decides it.
        let _ = rig.press(&special(DomainKey::Esc));
        assert!(rig.h.state.decisions.holds(kind));
    }
}

#[test]
fn no_kind_writes_the_composer() {
    for kind in ALL_6C {
        let mut rig = rig(80, 24);
        let before = rig.snap();
        assert_eq!(
            before.buffer,
            draft(),
            "{kind:?}: the fixture is a full composer"
        );
        // Open.
        let (raised, hold) = raise(&mut rig.h.state, kind, PLAIN);
        assert!(presentation::consistent(&rig.h.state), "{kind:?}");
        assert!(
            rig.h.state.decisions.holds(kind),
            "{kind:?}: queued ({raised:?})"
        );
        let _keep = hold;
        rig.ready();
        assert!(
            presentation::is_open(&rig.h.state, Surface::Decision),
            "{kind:?}: the frame is open over the Stage"
        );
        // Keys.
        poke(&mut rig, kind);
        let during = rig.snap();
        assert_eq!(during.buffer, before.buffer, "{kind:?}: composer text");
        assert_eq!(during.cursor, before.cursor, "{kind:?}: composer caret");
        assert_eq!(
            during.input_area, before.input_area,
            "{kind:?}: composer height"
        );
        assert_eq!(during.chat_pane, before.chat_pane, "{kind:?}: chat rect");
        assert_eq!(during.view, before.view, "{kind:?}: ViewState");
        assert_eq!(
            during.cache, before.cache,
            "{kind:?}: Stage height-cache keys"
        );
        // Defer.
        rig.defer(kind);
        assert_eq!(rig.snap(), before, "{kind:?}: deferred leaves everything");
        // Reopen, then resolve with the kind's deciding key.
        let item = item_key(&rig.h.state, kind);
        assert!(decision_queue::open(&mut rig.h.state, &item), "{kind:?}");
        rig.ready();
        let deciding = if kind == DecisionKind::Question {
            rig.press(&special(DomainKey::Enter))
        } else {
            rig.press(&key(deciding_key(kind)))
        };
        if let Some(RouteEffect::Action(action @ InputAction::Decision(_))) = deciding {
            if kind == DecisionKind::Question {
                decision_queue::apply_resolved_action(
                    &mut rig.h.state,
                    action,
                    reducer::restore_focus_after_modal,
                );
            }
        }
        assert!(
            !rig.h.state.decisions.holds(kind),
            "{kind:?}: the deciding key resolved it"
        );
        assert_eq!(rig.snap(), before, "{kind:?}: resolved leaves everything");
    }
}

#[test]
fn a_press_with_no_frame_is_the_positive_control_for_the_composer_snapshot() {
    // Without a frame the same key DOES reach the composer, so the equalities above
    // are not vacuous.
    let mut rig = rig(80, 24);
    rig.h.state.focus = FocusState::Input;
    let before = rig.snap();
    let _ = rig.press(&key('z'));
    let _ = rustain::adapters::tui::app::handle_input(&mut rig.h.state, &key('z'));
    let after = rig.snap();
    assert_ne!(after.buffer, before.buffer);
}

#[test]
fn the_plan_card_stage_row_height_is_identical_with_and_without_its_decision() {
    let mut rig = rig(80, 24);
    let without = rig.snap();
    let plan = plan("Stage plan");
    for width in [60u16, 80, 130] {
        assert_eq!(
            plan_card_height(&plan, width, true),
            plan_card_height(&plan, width, false),
            "{width}: the Stage row never grows for its decision"
        );
    }
    raise(&mut rig.h.state, DecisionKind::PlanCard, "Stage plan");
    assert!(decision_queue::plan_card_pending(&rig.h.state, "p1"));
    rig.ready();
    let with = rig.snap();
    assert_eq!(with.stage_height, without.stage_height, "Stage rows");
    assert_eq!(with.boundaries, without.boundaries, "message boundaries");
    assert_eq!(with.cache, without.cache, "height-cache keys");
    // Control: the pending marker is read from the queue and clears with the item.
    let item = item_key(&rig.h.state, DecisionKind::PlanCard);
    assert!(decision_queue::open(&mut rig.h.state, &item));
    rig.ready();
    let _ = rig.press(&key('n'));
    assert!(!decision_queue::plan_card_pending(&rig.h.state, "p1"));
    assert_eq!(rig.snap().stage_height, without.stage_height);
    assert!(!decision_queue::plan_card_pending(
        &rig.h.state,
        "never-raised"
    ));
}

/// Rows inside the frame rect, border glyphs stripped, whitespace collapsed.
fn flat(buffer: &ratatui::buffer::Buffer, rect: Rect) -> String {
    (rect.y..rect.bottom())
        .map(|y| row_text(buffer, y))
        .collect::<Vec<_>>()
        .join(" ")
        .replace(['│', '┌', '┐', '└', '┘', '─'], " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn collapse(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[test]
fn height_equals_paint_for_every_kind_at_60_80_130_and_the_full_composer_at_60x16() {
    for (width, height, full) in [
        (60u16, 24u16, false),
        (80, 24, false),
        (130, 30, false),
        (60, 16, true),
    ] {
        for kind in ALL_6C {
            let at = format!("{kind:?} {width}x{height} full={full}");
            let (mut state, _clock) = clocked(width, height);
            if full {
                state.input_buffer = draft();
                state.focus = FocusState::Chat;
            }
            let (_raised, _hold) = raise(&mut state, kind, PLAIN);
            let buffer = paint(&mut state, &facts("c1"), width, height);
            let text = screen(&buffer);
            let layout = layout_of(&state, width, height);
            let rect = geometry::frame_rect(geometry::modal_region(
                &layout,
                Rect::new(0, 0, width, height),
            ));
            // Painted extent == the computed rect: the top border opens it, the bottom
            // border closes it, every row between is a bordered row.
            assert!(
                row_text(&buffer, rect.y).contains(super::raisers::title(kind)),
                "{at}: title on the frame's top border\n{text}"
            );
            assert_eq!(buffer.cell((rect.x, rect.y)).unwrap().symbol(), "┌", "{at}");
            assert_eq!(
                buffer.cell((rect.x, rect.bottom() - 1)).unwrap().symbol(),
                "└",
                "{at}: the bottom border sits at the rect's last row\n{text}"
            );
            for y in rect.y + 1..rect.bottom() - 1 {
                assert_eq!(
                    buffer.cell((rect.x, y)).unwrap().symbol(),
                    "│",
                    "{at}: row {y} is inside the frame\n{text}"
                );
            }
            // Status row and composer are never covered.
            assert_eq!(
                row_text(&buffer, layout.status_bar.y).trim_end(),
                "status-row-sentinel",
                "{at}"
            );
            // At least one body row survives, and the whole rail is painted.
            let inside = flat(&buffer, rect);
            assert!(inside.contains(anchor(kind)), "{at}: body row\n{text}");
            let rail: Vec<(String, String)> = if kind == DecisionKind::Question {
                vec![
                    ("[Enter]".into(), "Send".into()),
                    ("[Ctrl+D]".into(), "Skip question".into()),
                    ("[Esc]".into(), "Back".into()),
                ]
            } else {
                kind.rail()
                    .into_iter()
                    .map(|(key, label)| (key, label.to_owned()))
                    .collect()
            };
            assert!(!rail.is_empty(), "{at}");
            for (key, label) in rail {
                let pair = collapse(&format!("{key} {label}"));
                assert!(inside.contains(&pair), "{at}: rail pair {pair:?}\n{inside}");
            }
            for y in rect.y..rect.bottom() {
                assert!(
                    UnicodeWidthStr::width(row_text(&buffer, y).as_str()) <= usize::from(width),
                    "{at}: row {y} fits"
                );
            }
        }
    }
}

/// The text each kind's body paints for `PLAIN`, row by row, spelled out here.
fn body_of(kind: DecisionKind) -> Vec<&'static str> {
    match kind {
        DecisionKind::Question | DecisionKind::PlanCard => vec!["Plain body text"],
        DecisionKind::SkillTrust => vec![
            "New project skill detected: \"Plain body text\"",
            "Trust and enable this skill for this conversation?",
        ],
        DecisionKind::PlanApproval => vec![
            "Plan Approval — plan.md",
            "Summary: Plain body text",
            "",
            "# Plain body text",
            "",
            "1. first",
            "2. second",
        ],
        DecisionKind::PlanDeviation => vec![
            "Plan deviation — reapproval required",
            "3 tasks → 3 tasks. Skipped: 2",
            "Plain body text",
        ],
        DecisionKind::SkipCascade => vec!["Task 1 skipped — 2 task(s) depend on it."],
        DecisionKind::CancelPlan => vec![
            "Plain body text",
            "2 task(s) remaining, 1 completed (will be kept)",
        ],
        DecisionKind::Delegation => {
            vec!["Delegate Task 1?", "→ Plain body text  (explicit mention)"]
        }
        DecisionKind::SpawnGate => vec!["spawn 5? · ⚠ 5 rows · cap at 2"],
        DecisionKind::AgentKill => vec!["⚠ Kill Plain body text and its subtree?"],
        other => panic!("{other:?} is not a 6-C kind"),
    }
}

/// 6C-L2-07: the interior rows (border stripped) the frame must paint, from an
/// arithmetic model written here: the breadcrumb row (kept while it leaves the body its
/// `min(3, rows)`), the body rows that still fit, the answer field, then the rail rows
/// wrapped pair by pair at the interior width, directly under the body, nothing after.
fn modelled_interior(kind: DecisionKind, height: usize, width: usize) -> (bool, Vec<String>) {
    let body = body_of(kind);
    let mut footer: Vec<String> = Vec::new();
    if kind == DecisionKind::Question {
        footer.push("Answer: ▋".into());
    }
    let pairs: Vec<String> = if kind == DecisionKind::Question {
        vec![
            "[Enter] Send  ".into(),
            "[Ctrl+D] Skip question  ".into(),
            "[Esc] Back  ".into(),
        ]
    } else {
        kind.rail()
            .into_iter()
            .map(|(key, label)| format!("{key} {label}  "))
            .collect()
    };
    let (mut row, mut used) = (String::new(), 0usize);
    for pair in pairs {
        let cells = pair.chars().count();
        if used > 0 && used + cells > width {
            footer.push(row.trim_end().to_owned());
            row.clear();
            used = 0;
        }
        row.push_str(&pair);
        used += cells;
    }
    footer.push(row.trim_end().to_owned());
    let crumb = 1 + footer.len() + body.len().min(3) <= height;
    let shown = body
        .len()
        .min(height.saturating_sub(usize::from(crumb) + footer.len()));
    let mut rows: Vec<String> = body[..shown].iter().map(|row| (*row).to_owned()).collect();
    rows.extend(footer);
    (crumb, rows)
}

#[test]
fn the_painted_content_rows_match_an_independent_row_model_at_60_80_130_and_60x16() {
    for (width, height, full) in [
        (60u16, 24u16, false),
        (80, 24, false),
        (130, 30, false),
        (60, 16, true),
    ] {
        for kind in ALL_6C {
            let at = format!("{kind:?} {width}x{height} full={full}");
            let (mut state, _clock) = clocked(width, height);
            if full {
                state.input_buffer = draft();
                state.focus = FocusState::Chat;
            }
            let (_raised, _hold) = raise(&mut state, kind, PLAIN);
            let buffer = paint(&mut state, &facts("c1"), width, height);
            let layout = layout_of(&state, width, height);
            let rect = geometry::frame_rect(geometry::modal_region(
                &layout,
                Rect::new(0, 0, width, height),
            ));
            let interior: Vec<String> = (rect.y + 1..rect.bottom() - 1)
                .map(|y| {
                    (rect.x + 1..rect.right() - 1)
                        .map(|x| buffer.cell((x, y)).unwrap().symbol())
                        .collect::<String>()
                        .trim_end()
                        .to_owned()
                })
                .collect();
            let (crumb, rows) =
                modelled_interior(kind, interior.len(), usize::from(rect.width) - 2);
            let mut at_row = 0;
            if crumb {
                let first = &interior[0];
                assert!(
                    first.starts_with(&format!("Needs you › {}", kind.title()))
                        && first.ends_with("| 1 pending"),
                    "{at}: breadcrumb row 0 is {first:?}"
                );
                at_row = 1;
            }
            for (offset, expected) in rows.iter().enumerate() {
                assert_eq!(
                    &interior[at_row + offset],
                    expected,
                    "{at}: interior row {} (crumb={crumb})\n{interior:#?}",
                    at_row + offset
                );
            }
            let painted = at_row + rows.len();
            assert!(
                interior[painted..].iter().all(String::is_empty),
                "{at}: nothing is painted under the rail\n{interior:#?}"
            );
            assert!(
                !rows.is_empty() && painted <= interior.len(),
                "{at}: the modelled content fits the interior"
            );
            if full {
                assert_eq!(
                    interior.len(),
                    3,
                    "{at}: the 60x16 floor leaves a five-row frame"
                );
            }
        }
    }
}

/// 6C-L2-02: `c` through the route copies the original bytes via the state's injected
/// copier (never the real clipboard) and posts the one copy notice.
#[test]
fn c_on_every_frame_copies_the_original_bytes_through_the_injected_copier() {
    use super::support::record_copies;
    use rustain::adapters::tui::clipboard::ClipboardResult;
    use rustain::adapters::tui::notifications::NoticeSeverity;
    for kind in ALL_6C {
        let (mut state, clock) = clocked(80, 24);
        let copies = record_copies(&mut state);
        let (_raised, _hold) = raise(&mut state, kind, HOSTILE);
        paint(&mut state, &facts("c1"), 80, 24);
        ready(&mut state, &clock);
        if kind == DecisionKind::Question {
            let _ = route(&mut state, &special(DomainKey::Esc), &facts("c1"));
        }
        assert_eq!(
            route(&mut state, &key('c'), &facts("c1")),
            Some(RouteEffect::Consumed),
            "{kind:?}"
        );
        let copied = copies.lock().unwrap().clone();
        assert_eq!(copied.len(), 1, "{kind:?}: one copy");
        if !matches!(kind, DecisionKind::SkipCascade | DecisionKind::SpawnGate) {
            assert!(
                copied[0].contains(HOSTILE),
                "{kind:?}: original bytes {:?}",
                copied[0]
            );
        }
        let notice = state.notifications.records().last().expect("a copy notice");
        assert_eq!(notice.message, "Copied to clipboard", "{kind:?}");
        assert_eq!(notice.severity, NoticeSeverity::Success);
        assert!(state.decisions.holds(kind), "{kind:?}: Copy never resolves");
    }
    // A failing copier posts the failure notice instead (control for the success text).
    let (mut state, _clock) = clocked(80, 24);
    state.clipboard_copier = Some(Arc::new(|_: &str| ClipboardResult::Failed("boom".into())));
    let (_raised, _hold) = raise(&mut state, DecisionKind::CancelPlan, PLAIN);
    let _ = route(&mut state, &key('c'), &facts("c1"));
    let notice = state
        .notifications
        .records()
        .last()
        .expect("a failure notice");
    assert_eq!(notice.message, "Copy failed: boom");
    assert_eq!(notice.severity, NoticeSeverity::Error);
}

#[test]
fn display_controls_are_never_painted_and_the_effect_and_copy_get_the_original_bytes() {
    let original = format!("{HOSTILE:?}");
    let original = original.trim_matches('"');
    for kind in ALL_6C {
        let (mut state, clock) = clocked(80, 24);
        let (_raised, hold) = raise(&mut state, kind, HOSTILE);
        let buffer = paint(&mut state, &facts("c1"), 80, 24);
        let text = screen(&buffer);
        for control in CONTROLS {
            assert!(
                !text.contains(control),
                "{kind:?}: {control:?} painted\n{text:?}"
            );
        }
        // The frame itself did paint (the sweep is not vacuous).
        assert!(
            text.contains(super::raisers::title(kind)),
            "{kind:?}\n{text}"
        );
        // The complete cleaned string paints (the pre-cell rows are pinned in
        // `decision_frame`'s own tests: ratatui drops controls itself).
        if matches!(
            kind,
            DecisionKind::Question
                | DecisionKind::CancelPlan
                | DecisionKind::PlanDeviation
                | DecisionKind::AgentKill
        ) {
            assert!(text.contains(CLEANED), "{kind:?}: cleaned text\n{text}");
        }
        let carries_text = !matches!(kind, DecisionKind::SkipCascade | DecisionKind::SpawnGate);
        let item = state.decisions.items().next().expect("queued");
        if carries_text {
            // The queued payload (what the effect receives) keeps the original bytes.
            let subject = format!("{:?}", item.subject);
            assert!(
                subject.contains(original),
                "{kind:?}: payload bytes\n{subject}"
            );
        }
        // Copy hands over the original source text.
        ready(&mut state, &clock);
        if kind == DecisionKind::Question {
            let _ = route(&mut state, &special(DomainKey::Esc), &facts("c1"));
        }
        let outcome = handle_domain_event(&mut state, &key('c'), &facts("c1"), Host::Local);
        match outcome {
            Some(PaneOutcome::Copy(copied)) => {
                if carries_text {
                    assert!(
                        copied.contains(HOSTILE),
                        "{kind:?}: copied the original bytes"
                    );
                }
                assert!(
                    state.decisions.holds(kind),
                    "{kind:?}: Copy never resolves the item"
                );
            }
            other => panic!("{kind:?}: `c` is Copy on every decision frame, got {other:?}"),
        }
        drop(hold);
    }
    // Positive control: plain text paints verbatim.
    let (mut state, _clock) = clocked(80, 24);
    let (_r, _hold) = raise(&mut state, DecisionKind::Question, PLAIN);
    let text = screen(&paint(&mut state, &facts("c1"), 80, 24));
    assert!(text.contains(PLAIN), "{text}");
}

#[test]
fn copy_at_the_limit_succeeds_and_one_byte_over_refuses_with_the_failure_notice() {
    for (extra, copies) in [(0usize, true), (1, false)] {
        let (mut state, clock) = clocked(80, 24);
        let (tx, _rx) = tokio::sync::oneshot::channel::<String>();
        // The Question's body source is exactly its question text.
        let question = "a".repeat(COPY_MAX_BYTES + extra);
        decision_queue::raise_question(&mut state, "c1", "tu".into(), question, tx);
        decision_queue::note_painted(&mut state);
        advance(&clock, decision_queue::INERT_AFTER_PAINT);
        let _ = route(&mut state, &special(DomainKey::Esc), &facts("c1"));
        assert!(
            presentation::is_open(&state, Surface::Decision),
            "field left, frame open"
        );
        let outcome = handle_domain_event(&mut state, &key('c'), &facts("c1"), Host::Local);
        if copies {
            match outcome {
                Some(PaneOutcome::Copy(text)) => assert_eq!(text.len(), COPY_MAX_BYTES),
                other => panic!("exactly COPY_MAX_BYTES copies, got {other:?}"),
            }
            assert!(
                !state
                    .notifications
                    .records()
                    .any(|r| r.message.contains("Copy failed")),
                "no failure notice at the limit"
            );
        } else {
            assert!(
                !matches!(outcome, Some(PaneOutcome::Copy(_))),
                "one byte over refuses"
            );
            assert!(
                state.notifications.records().any(|r| r
                    .message
                    .contains("Copy failed: decision text exceeds the 8 MiB copy limit")),
                "the existing failure notice is posted"
            );
        }
        assert!(
            state.decisions.holds(DecisionKind::Question),
            "Copy never resolves"
        );
    }
}

#[test]
fn typing_c_in_the_question_field_types_and_never_copies() {
    let (mut state, clock) = clocked(80, 24);
    let (_raised, _hold) = raise(&mut state, DecisionKind::Question, PLAIN);
    paint(&mut state, &facts("c1"), 80, 24);
    ready(&mut state, &clock);
    let outcome = handle_domain_event(&mut state, &key('c'), &facts("c1"), Host::Local);
    assert!(
        !matches!(outcome, Some(PaneOutcome::Copy(_))),
        "{outcome:?}"
    );
    let typed = state
        .decisions
        .items()
        .find_map(|item| match &item.subject {
            decision_queue::DecisionSubject::Question(payload) => {
                Some(payload.field.as_str().to_owned())
            }
            _ => None,
        })
        .expect("the question is queued");
    assert_eq!(typed, "c", "the typed `c` is in the field");
    assert!(state.decisions.holds(DecisionKind::Question));
}
