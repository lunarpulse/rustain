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
use rustain::adapters::tui::presentation::{self, RouteEffect, Surface, geometry};
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
        route(&mut self.h.state, event, &super::raisers::facts_6d("c1"))
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
                decision_queue::apply_resolved_action(&mut rig.h.state, action);
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

/// Compare the fitted frame against an independent row budget. Body rows come
/// from the public pre-cell body boundary, not from the painted Buffer; this
/// deliberately checks fitting and placement rather than reimplementing wrapping.
#[test]
fn k8_six_d_fitted_rows_match_the_body_and_rail_budget() {
    use rustain::adapters::tui::widgets::decision_frame::body_lines_for_test;

    for (width, height, full) in [
        (60u16, 24u16, false),
        (80, 24, false),
        (130, 30, false),
        (60, 16, true),
    ] {
        for kind in super::raisers::ALL_6D {
            for tall in [false, true] {
                let at = format!("{kind:?} {width}x{height} full={full} tall={tall}");
                let (mut state, _) = clocked(width, height);
                if full {
                    state.input_buffer = draft();
                    state.focus = FocusState::Chat;
                }
                let text = if tall
                    && matches!(
                        kind,
                        DecisionKind::Consolidation
                            | DecisionKind::Forget
                            | DecisionKind::ArtifactApply
                            | DecisionKind::ArtifactResolve
                            | DecisionKind::Fork
                    ) {
                    "long preview file memory ".repeat(80)
                } else {
                    PLAIN.into()
                };
                if tall && kind == DecisionKind::Rewind {
                    let mut preview = super::raisers::rewind_preview(PLAIN);
                    preview.files_to_revert = (0..25)
                        .map(|n| decision_queue::RevertPreviewItem {
                            display_path: format!("src/file-{n}.rs"),
                            conflict: n % 2 == 0,
                        })
                        .collect();
                    decision_queue::raise_rewind(&mut state, "c1", preview, 0);
                } else {
                    raise(&mut state, kind, &text);
                }
                if !presentation::is_open(&state, Surface::Decision) {
                    let item = item_key(&state, kind);
                    assert!(decision_queue::open(&mut state, &item));
                }
                let layout = layout_of(&state, width, height);
                let rect = geometry::frame_rect(geometry::modal_region(
                    &layout,
                    Rect::new(0, 0, width, height),
                ));
                let inner_width = usize::from(rect.width - 2);
                let item = state.decisions.items().next().unwrap();
                let body: Vec<String> = body_lines_for_test(item, &state, inner_width)
                    .iter()
                    .map(|line| {
                        line.spans
                            .iter()
                            .map(|span| span.content.as_ref())
                            .collect::<String>()
                            .trim_end()
                            .to_owned()
                    })
                    .collect();
                assert!(!body.is_empty(), "{at}");
                let focus = if kind == DecisionKind::Forget {
                    body.iter()
                        .position(|row| row.contains("> "))
                        .expect("focused forget row")
                } else {
                    0
                };
                let mut rail = Vec::new();
                let mut current = String::new();
                for (key, label) in kind.rail() {
                    let pair = format!("{key} {label}  ");
                    if !current.is_empty()
                        && UnicodeWidthStr::width(current.as_str())
                            + UnicodeWidthStr::width(pair.as_str())
                            > inner_width
                    {
                        rail.push(current.trim_end().to_owned());
                        current.clear();
                    }
                    current.push_str(&pair);
                }
                rail.push(current.trim_end().to_owned());
                let interior_height = usize::from(rect.height - 2);
                let crumb = 1 + rail.len() + body.len().min(3) <= interior_height;
                let visible = interior_height.saturating_sub(usize::from(crumb) + rail.len());
                let start = (focus + 1).saturating_sub(visible).min(focus);
                let expected: Vec<_> = body
                    .iter()
                    .skip(start)
                    .take(visible)
                    .chain(rail.iter())
                    .collect();
                let buffer = paint(&mut state, &super::raisers::facts_6d("c1"), width, height);
                let interior: Vec<String> = (rect.y + 1..rect.bottom() - 1)
                    .map(|y| {
                        (rect.x + 1..rect.right() - 1)
                            .map(|x| buffer.cell((x, y)).unwrap().symbol())
                            .collect::<String>()
                            .trim_end()
                            .to_owned()
                    })
                    .collect();
                let offset = usize::from(crumb);
                if crumb {
                    assert!(
                        interior[0].starts_with(&format!("Needs you › {}", kind.title())),
                        "{at}: {interior:#?}"
                    );
                }
                for (index, row) in expected.iter().enumerate() {
                    assert_eq!(
                        &interior[offset + index],
                        *row,
                        "{at}: row {index}, body={}, rail={}\n{interior:#?}",
                        body.len(),
                        rail.len()
                    );
                }
                assert!(
                    interior[offset + expected.len()..]
                        .iter()
                        .all(String::is_empty),
                    "{at}: rows below rail {interior:#?}"
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

/// AC18–AC21: the additional decision bodies preserve the real Stage and composer.
#[test]
fn k8_six_d_open_keys_defer_and_resolve_preserve_stage_and_composer() {
    for kind in super::raisers::ALL_6D {
        let mut rig = rig(80, 24);
        let before = rig.snap();
        raise(&mut rig.h.state, kind, PLAIN);
        if !presentation::is_open(&rig.h.state, Surface::Decision) {
            let item = item_key(&rig.h.state, kind);
            assert!(decision_queue::open(&mut rig.h.state, &item));
        }
        rig.ready();
        for event in [
            key('z'),
            special(DomainKey::PageDown),
            special(DomainKey::PageUp),
        ] {
            assert_eq!(rig.press(&event), Some(RouteEffect::Consumed), "{kind:?}");
        }
        assert_eq!(rig.snap(), before, "{kind:?}: open and keys");
        rig.defer(kind);
        assert_eq!(rig.snap(), before, "{kind:?}: defer");
        let item = item_key(&rig.h.state, kind);
        assert!(decision_queue::open(&mut rig.h.state, &item));
        rig.ready();
        let result = rig.press(&key('n'));
        assert!(
            matches!(result, Some(RouteEffect::Action(_))),
            "{kind:?}: {result:?}"
        );
        assert!(rig.h.state.decisions.take_resolved().is_some(), "{kind:?}");
        assert_eq!(rig.snap(), before, "{kind:?}: resolve");
    }
}

#[test]
fn k8_six_d_frame_extent_and_complete_rail_at_all_sizes() {
    for (width, height, full) in [
        (60, 24, false),
        (80, 24, false),
        (130, 30, false),
        (60, 16, true),
    ] {
        for kind in super::raisers::ALL_6D {
            let (mut state, _) = clocked(width, height);
            if full {
                state.input_buffer = draft();
                state.focus = FocusState::Chat;
            }
            raise(&mut state, kind, PLAIN);
            if !presentation::is_open(&state, Surface::Decision) {
                let item = item_key(&state, kind);
                assert!(decision_queue::open(&mut state, &item));
            }
            let buffer = paint(&mut state, &super::raisers::facts_6d("c1"), width, height);
            let layout = layout_of(&state, width, height);
            let rect = geometry::frame_rect(geometry::modal_region(
                &layout,
                Rect::new(0, 0, width, height),
            ));
            let text = screen(&buffer);
            assert!(
                row_text(&buffer, rect.y).contains(kind.frame_title()),
                "{kind:?} {width}x{height}\n{text}"
            );
            assert_eq!(buffer.cell((rect.x, rect.y)).unwrap().symbol(), "┌");
            assert_eq!(
                buffer.cell((rect.x, rect.bottom() - 1)).unwrap().symbol(),
                "└"
            );
            let inside = flat(&buffer, rect);
            for (key, label) in kind.rail() {
                assert!(
                    inside.contains(&collapse(&format!("{key} {label}"))),
                    "{kind:?} {width}x{height}: {key} {label}\n{text}"
                );
            }
            let item = state.decisions.items().next().unwrap();
            let rows = rustain::adapters::tui::widgets::decision_frame::body_lines_for_test(
                item,
                &state,
                usize::from(rect.width - 2),
            );
            let first = rows
                .first()
                .expect("body row")
                .spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>();
            // A one-row body of `/memory forget` shows the FOCUSED candidate (6D-L1-06), not
            // the header above it.
            let forget_focus = kind == DecisionKind::Forget && inside.contains("[x]");
            assert!(
                !first.trim().is_empty() && (inside.contains(first.trim()) || forget_focus),
                "{kind:?}: first body row missing\n{text}"
            );
            assert_eq!(
                row_text(&buffer, layout.status_bar.y).trim_end(),
                "status-row-sentinel"
            );
            assert!(row_text(&buffer, layout.input_area.y).contains("composer-sentinel"));
        }
    }
}

#[test]
fn k8_six_d_pre_cell_spans_are_clean_copy_and_payload_remain_original() {
    use super::support::record_copies;
    use rustain::adapters::tui::widgets::decision_frame::body_lines_for_test;
    for kind in super::raisers::ALL_6D {
        let (mut state, clock) = clocked(80, 24);
        let copies = record_copies(&mut state);
        raise(&mut state, kind, HOSTILE);
        if !presentation::is_open(&state, Surface::Decision) {
            let item = item_key(&state, kind);
            assert!(decision_queue::open(&mut state, &item));
        }
        let item = state.decisions.items().next().unwrap();
        let rows = body_lines_for_test(item, &state, 120);
        let text = rows
            .iter()
            .flat_map(|line| &line.spans)
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(!text.is_empty(), "{kind:?}: actual body");
        for control in CONTROLS {
            assert!(
                !text.contains(control),
                "{kind:?}: {control:?} survived in {text:?}"
            );
        }
        assert!(
            format!("{:?}", item.subject)
                .contains(&format!("{HOSTILE:?}")[1..format!("{HOSTILE:?}").len() - 1]),
            "{kind:?}: payload lost bytes"
        );
        paint(&mut state, &super::raisers::facts_6d("c1"), 80, 24);
        ready(&mut state, &clock);
        assert_eq!(
            route(&mut state, &key('c'), &super::raisers::facts_6d("c1")),
            Some(RouteEffect::Consumed)
        );
        let copied = copies.lock().unwrap();
        assert_eq!(copied.len(), 1, "{kind:?}");
        assert!(copied[0].contains(HOSTILE), "{kind:?}: {:?}", copied[0]);
        assert!(state.decisions.take_resolved().is_none());
        drop(copied);
        assert!(matches!(
            route(&mut state, &key('n'), &super::raisers::facts_6d("c1")),
            Some(RouteEffect::Action(_))
        ));
        let payload = state.decisions.take_resolved().expect("effect payload");
        let escaped = format!("{HOSTILE:?}");
        assert!(
            format!("{payload:?}").contains(&escaped[1..escaped.len() - 1]),
            "{kind:?}: effect payload lost bytes"
        );
    }
}

#[test]
fn k8_six_d_copy_exact_limit_and_one_byte_over() {
    use super::support::record_copies;
    // Fork and rewind clip their preview rows; the other bodies can reach the
    // exact boundary. Export's sample uses a seven-digit content length, as does
    // its near-limit payload, so its displayed byte-count field has equal width.
    for kind in [
        DecisionKind::Consolidation,
        DecisionKind::Forget,
        DecisionKind::ArtifactApply,
        DecisionKind::ArtifactResolve,
        DecisionKind::PeerAdd,
        DecisionKind::TeamRetract,
        DecisionKind::ExportOverwrite,
    ] {
        let (mut sample, _) = clocked(80, 24);
        let sample_input = "x".repeat(if kind == DecisionKind::ExportOverwrite {
            1_000_000
        } else {
            1
        });
        raise(&mut sample, kind, &sample_input);
        if !presentation::is_open(&sample, Surface::Decision) {
            let item = item_key(&sample, kind);
            assert!(decision_queue::open(&mut sample, &item));
        }
        let Some(PaneOutcome::Copy(text)) = handle_domain_event(
            &mut sample,
            &key('c'),
            &super::raisers::facts_6d("c1"),
            Host::Local,
        ) else {
            panic!("{kind:?}: sample copy");
        };
        let overhead = text.len() - sample_input.len();
        for extra in [0, 1] {
            let (mut state, _) = clocked(80, 24);
            let copies = record_copies(&mut state);
            let input = "x".repeat(COPY_MAX_BYTES - overhead + extra);
            raise(&mut state, kind, &input);
            if !presentation::is_open(&state, Surface::Decision) {
                let item = item_key(&state, kind);
                assert!(decision_queue::open(&mut state, &item));
            }
            assert_eq!(
                route(&mut state, &key('c'), &super::raisers::facts_6d("c1")),
                Some(RouteEffect::Consumed)
            );
            let recorded = copies.lock().unwrap();
            if extra == 0 {
                assert_eq!(recorded.len(), 1, "{kind:?}");
                assert_eq!(recorded[0].len(), COPY_MAX_BYTES, "{kind:?}");
            } else {
                assert!(recorded.is_empty(), "{kind:?}");
                assert!(
                    state.notifications.records().any(|r| r
                        .message
                        .contains("Copy failed: decision text exceeds the 8 MiB copy limit")),
                    "{kind:?}"
                );
            }
            assert!(state.decisions.take_resolved().is_none());
        }
    }
}

#[test]
fn k8_rewind_many_files_copy_at_limit_and_refuse_one_byte_over() {
    use super::support::record_copies;
    use rustain::adapters::tui::decision_queue::{RevertPreviewItem, RewindPreview};
    let preview = |files| RewindPreview {
        target_message_index: 1,
        messages_to_remove: 2,
        files_to_revert: files,
    };
    let (mut sample, _) = clocked(80, 24);
    decision_queue::raise_rewind(
        &mut sample,
        "c1",
        preview(vec![RevertPreviewItem {
            display_path: String::new(),
            conflict: false,
        }]),
        0,
    );
    let Some(PaneOutcome::Copy(text)) = handle_domain_event(
        &mut sample,
        &key('c'),
        &super::raisers::facts_6d("c1"),
        Host::Local,
    ) else {
        panic!("sample copy")
    };
    let base = text.len();
    for extra in [0usize, 1] {
        // Copy is the original paths joined by newlines: `count` paths cost the sum of
        // their bytes plus `count - 1` separators, each path up to 4096 bytes.
        let count = (COPY_MAX_BYTES + extra - base + 1).div_ceil(4097);
        let mut bytes = COPY_MAX_BYTES + extra - base - (count - 1);
        let mut files = Vec::with_capacity(count);
        for _ in 0..count {
            let len = bytes.min(4096);
            bytes -= len;
            files.push(RevertPreviewItem {
                display_path: "x".repeat(len),
                conflict: false,
            });
        }
        assert_eq!(bytes, 0);
        let (mut state, _) = clocked(80, 24);
        let copies = record_copies(&mut state);
        decision_queue::raise_rewind(&mut state, "c1", preview(files), 0);
        assert_eq!(
            route(&mut state, &key('c'), &super::raisers::facts_6d("c1")),
            Some(RouteEffect::Consumed)
        );
        let recorded = copies.lock().unwrap();
        if extra == 0 {
            assert_eq!(recorded.len(), 1);
            assert_eq!(recorded[0].len(), COPY_MAX_BYTES);
        } else {
            assert!(recorded.is_empty());
            assert!(state.notifications.records().any(|r| {
                r.message
                    .contains("Copy failed: decision text exceeds the 8 MiB copy limit")
            }));
        }
        assert!(state.decisions.take_resolved().is_none());
    }
}

/// 6D-L1-07: oversized lists are refused through the real route with the copy notice.
#[test]
fn k8_oversized_artifact_and_rewind_lists_post_copy_refusal() {
    use super::support::record_copies;
    use rustain::adapters::tui::decision_queue::{
        DecisionSubject, RevertPreviewItem, RewindPreview,
    };
    let (mut sample, _) = clocked(80, 24);
    raise(&mut sample, DecisionKind::ArtifactApply, "sample");
    let DecisionSubject::Artifact(card) = &sample.decisions.items().next().unwrap().subject else {
        panic!("artifact")
    };
    let mut card = (**card).clone();
    let files = vec!["x".repeat(4096); COPY_MAX_BYTES / 4096 + 1];
    card.files = files.clone();
    for artifact in [true, false] {
        let (mut state, _) = clocked(80, 24);
        let copies = record_copies(&mut state);
        if artifact {
            decision_queue::raise_artifact(&mut state, card.clone());
        } else {
            decision_queue::raise_rewind(
                &mut state,
                "c1",
                RewindPreview {
                    target_message_index: 1,
                    messages_to_remove: 2,
                    files_to_revert: files
                        .iter()
                        .map(|file| RevertPreviewItem {
                            display_path: file.clone(),
                            conflict: false,
                        })
                        .collect(),
                },
                0,
            );
        }
        assert_eq!(
            route(&mut state, &key('c'), &super::raisers::facts_6d("c1")),
            Some(RouteEffect::Consumed)
        );
        assert!(copies.lock().unwrap().is_empty());
        assert!(state.notifications.records().any(|record| {
            record
                .message
                .contains("Copy failed: decision text exceeds the 8 MiB copy limit")
        }));
    }
}

/// Final-slice producers, including the attached recipient row projection.
fn story21_6_raise_final_kind(state: &mut TuiState, kind: DecisionKind) {
    use rustain::adapters::tui::decision_queue::{Responder, TicketPayload, reply};
    use rustain::domain::models::{
        AgentId, ArtifactId, ContentHash, Conversation, PeerRowControl, PeerRowStage,
        SemanticMessageType, TeamProposal,
    };
    use serde_json::json;

    decision_queue::prune_closed(state, &facts("c1"));
    match kind {
        DecisionKind::TeamProposal => {
            decision_queue::raise_team_proposal(
                state,
                Arc::new(TeamProposal {
                    id: "p1".into(),
                    conversation_id: "c1".into(),
                    recipients: vec!["alice".into()],
                    message_type: SemanticMessageType::ArchitectureUpdate,
                    body: "body\u{1b}[31munsafe".into(),
                }),
                Responder::Local,
            );
        }
        DecisionKind::RecipientDraftApproval => {
            let control = PeerRowControl {
                node_id: "node".into(),
                item_address: None,
                stories: None,
                status: None,
                retro: None,
                selected_for: None,
                message_type: SemanticMessageType::Consultation,
                sender_label: "alice\u{202e}unsafe".into(),
                stage: PeerRowStage::Ready,
                node_resolvable: true,
            };
            let conversation = Conversation {
                id: "c1".into(),
                messages: vec![ChatMessage {
                    id: "row".into(),
                    content: "draft".into(),
                    ..Default::default()
                }],
                peer_row_controls: [("row".into(), control)].into_iter().collect(),
                ..Default::default()
            };
            decision_queue::sync_recipient_drafts(state, &conversation, false);
        }
        DecisionKind::Ticket => {
            let node = AgentId::parse("mcp-task").unwrap();
            let artifact = ArtifactId::from(ContentHash::parse_hex(&"a".repeat(64)).unwrap());
            let payload = TicketPayload::new(
                node.clone(),
                artifact,
                node,
                7,
                0,
                json!({"key":"request","request":{"method":"elicitation/create","params":{
                    "message":"Ticket body\u{1b}[31munsafe",
                    "requestedSchema":{"type":"object","properties":{
                        "name":{"type":"string"}, "count":{"type":"integer"}}}}}}),
            )
            .unwrap();
            decision_queue::raise_ticket(state, payload, Responder::Local);
        }
        DecisionKind::ReplyArrived => {
            reply::observe(state, "principal", "alice\u{202e}unsafe", "ri_1", 1)
        }
        _ => panic!("not a final-slice kind"),
    }
    let key = item_key(state, kind);
    if !presentation::is_open(state, Surface::Decision) {
        assert!(decision_queue::open(state, &key));
    }
}

#[test]
fn story21_6_final_kinds_preserve_stage_composer_and_floor_body_and_rail() {
    use rustain::adapters::tui::widgets::decision_frame::body_lines_for_test;
    for (width, height, full) in [
        (60, 24, false),
        (80, 24, false),
        (130, 30, false),
        (60, 16, true),
    ] {
        for kind in [
            DecisionKind::TeamProposal,
            DecisionKind::RecipientDraftApproval,
            DecisionKind::Ticket,
            DecisionKind::ReplyArrived,
        ] {
            let mut rig = rig(width, height);
            if !full {
                rig.h.state.input_buffer.clear();
                rig.h.state.cursor_position = 0;
            }
            let before = rig.snap();
            story21_6_raise_final_kind(&mut rig.h.state, kind);
            let item = rig.h.state.decisions.items().next().unwrap();
            let body = body_lines_for_test(item, &rig.h.state, usize::from(width - 2));
            for span in body.iter().flat_map(|line| &line.spans) {
                for control in CONTROLS {
                    assert!(
                        !span.content.contains(control),
                        "{kind:?}: unsanitized span"
                    );
                }
            }
            let buffer = paint(&mut rig.h.state, &facts("c1"), width, height);
            let layout = layout_of(&rig.h.state, width, height);
            let rect = geometry::frame_rect(geometry::modal_region(
                &layout,
                Rect::new(0, 0, width, height),
            ));
            assert_eq!(buffer.cell((rect.x, rect.y)).unwrap().symbol(), "┌");
            assert_eq!(
                buffer.cell((rect.x, rect.bottom() - 1)).unwrap().symbol(),
                "└"
            );
            let inside = flat(&buffer, rect);
            let first_body = body
                .iter()
                .map(ToString::to_string)
                .find(|line| !line.trim().is_empty())
                .unwrap();
            let body_row = (rect.y + 1..rect.bottom() - 1)
                .find(|&y| collapse(&row_text(&buffer, y)).contains(&collapse(first_body.trim())))
                .unwrap_or_else(|| panic!("{kind:?} {width}x{height}: body: {inside}"));
            let rail_key = if kind == DecisionKind::TeamProposal && full {
                "Finish or clear your draft".to_owned()
            } else {
                kind.rail()[0].0.clone()
            };
            let rail_row = (rect.y + 1..rect.bottom() - 1)
                .find(|&y| row_text(&buffer, y).contains(&rail_key))
                .unwrap_or_else(|| panic!("{kind:?} {width}x{height}: no rail: {inside}"));
            assert!(
                body_row < rail_row,
                "{kind:?} {width}x{height}: body must precede rail"
            );
            if kind == DecisionKind::TeamProposal && full {
                assert!(inside.contains("Finish or clear your draft to decide."));
            } else {
                for (key, label) in kind.rail() {
                    assert!(
                        inside.contains(&format!("{key} {label}"))
                            || (kind == DecisionKind::TeamProposal
                                && label == "Send"
                                && inside.contains("[y] Send to")),
                        "{kind:?} {width}x{height}: missing {key} {label}: {inside}"
                    );
                }
            }
            let during = rig.snap();
            assert_eq!(during.buffer, before.buffer);
            assert_eq!(during.cursor, before.cursor);
            assert_eq!(during.input_area, before.input_area);
            assert_eq!(during.chat_pane, before.chat_pane);
            assert_eq!(during.view, before.view);
            assert_eq!(during.stage_height, before.stage_height);
            assert_eq!(during.boundaries, before.boundaries);
            rig.defer(kind);
            assert_eq!(rig.snap(), before);
        }
    }
}

#[test]
fn story21_6_ticket_and_reply_copy_exact_limit_and_refuse_one_more() {
    use rustain::adapters::tui::decision_queue::{Responder, TicketPayload, reply};
    use rustain::domain::models::{AgentId, ArtifactId, ContentHash};
    use serde_json::json;

    for kind in [DecisionKind::Ticket, DecisionKind::ReplyArrived] {
        for extra in [0, 1] {
            let (mut state, _) = clocked(80, 24);
            let copies = super::support::record_copies(&mut state);
            let expected = if kind == DecisionKind::Ticket {
                let mut envelope = json!({"key":"request","padding":"","request":{
                    "method":"elicitation/create","params":{"message":"body",
                    "requestedSchema":{"type":"boolean"}}}});
                let base = envelope.to_string().len();
                envelope["padding"] = json!("x".repeat(COPY_MAX_BYTES - base + extra));
                let source = envelope.to_string();
                let node = AgentId::parse("mcp-task").unwrap();
                let artifact = ArtifactId::from(ContentHash::parse_hex(&"a".repeat(64)).unwrap());
                let ticket =
                    TicketPayload::new(node.clone(), artifact, node, 1, 0, envelope).unwrap();
                decision_queue::raise_ticket(&mut state, ticket, Responder::Local);
                source
            } else {
                let base = "From: \nPeer: principal\nItem: ri_1\nType: Reply arrived · 1 replies";
                let alias = "x".repeat(COPY_MAX_BYTES - base.len() + extra);
                let source = format!(
                    "From: {alias}\nPeer: principal\nItem: ri_1\nType: Reply arrived · 1 replies"
                );
                reply::observe(&mut state, "principal", &alias, "ri_1", 1);
                source
            };
            assert_eq!(expected.len(), COPY_MAX_BYTES + extra);
            let item = item_key(&state, kind);
            assert!(decision_queue::open(&mut state, &item));
            assert_eq!(
                route(&mut state, &key('c'), &facts("c1")),
                Some(RouteEffect::Consumed)
            );
            let copied = copies.lock().unwrap();
            if extra == 0 {
                assert_eq!(copied.as_slice(), &[expected]);
            } else {
                assert!(copied.is_empty());
                assert!(state.notifications.records().any(|notice| {
                    notice
                        .message
                        .contains("Copy failed: decision text exceeds the 8 MiB copy limit")
                }));
            }
        }
    }
}

#[test]
fn story21_6_ticket_forms_keep_a_body_row_and_complete_rail_at_the_floor() {
    use rustain::adapters::tui::decision_queue::{Responder, TicketPayload};
    use rustain::adapters::tui::widgets::decision_frame::body_lines_for_test;
    use rustain::domain::models::{AgentId, ArtifactId, ContentHash};
    use serde_json::json;

    for schema in [
        json!({"type":"object","required":["name"],"properties":{
            "name":{"type":"string"},"count":{"type":"integer"}}}),
        json!({"type":"object","properties":{"choice":{"type":"string",
            "enum":["one","two","three","four","five","six","seven","eight"]}}}),
        json!({"type":"array","items":{"type":"string"}}),
    ] {
        for (width, height) in [(60, 16), (60, 24), (80, 24), (130, 30)] {
            let (mut state, clock) = clocked(width, height);
            state.input_buffer = draft();
            state.focus = FocusState::Chat;
            let node = AgentId::parse("mcp-task").unwrap();
            let artifact = ArtifactId::from(ContentHash::parse_hex(&"a".repeat(64)).unwrap());
            let ticket = TicketPayload::new(
                node.clone(),
                artifact,
                node,
                1,
                0,
                json!({"key":"request","request":{"method":"elicitation/create","params":{
                    "message":"Ticket request","requestedSchema":schema.clone()}}}),
            )
            .unwrap();
            decision_queue::raise_ticket(&mut state, ticket, Responder::Local);
            let item = item_key(&state, DecisionKind::Ticket);
            decision_queue::set_ticket_error(&mut state, &item, "invalid\u{1b}[31mvalue".into());
            assert!(decision_queue::open(&mut state, &item));
            let body = body_lines_for_test(
                state.decisions.items().next().unwrap(),
                &state,
                usize::from(width - 2),
            );
            assert!(
                body.iter()
                    .flat_map(|line| &line.spans)
                    .all(|span| !span.content.contains('\u{1b}'))
            );
            let first = body
                .iter()
                .map(ToString::to_string)
                .find(|row| !row.trim().is_empty())
                .unwrap();
            let buffer = paint(&mut state, &facts("c1"), width, height);
            let layout = layout_of(&state, width, height);
            let rect = geometry::frame_rect(geometry::modal_region(
                &layout,
                Rect::new(0, 0, width, height),
            ));
            let interior = flat(&buffer, rect);
            assert!(
                interior.contains(first.trim()),
                "{width}x{height}: {interior}"
            );
            for (key, label) in DecisionKind::Ticket.rail() {
                assert!(
                    interior.contains(&format!("{key} {label}")),
                    "{width}x{height}: {interior}"
                );
            }
            ready(&mut state, &clock);
            let _ = route(&mut state, &key('e'), &facts("c1"));
            let editing = matches!(
                &state.decisions.items().next().unwrap().subject,
                decision_queue::DecisionSubject::Ticket(ticket) if ticket.editing
            );
            if editing {
                let edited = paint(&mut state, &facts("c1"), width, height);
                let text = flat(&edited, rect);
                assert!(
                    text.contains("Ticket request"),
                    "{width}x{height}: edited body: {text}"
                );
                assert!(text.contains("Answer:"), "{width}x{height}: {text}");
                assert!(text.contains("[Enter] Accept"), "{width}x{height}: {text}");
                assert!(text.contains("[Esc] Back"), "{width}x{height}: {text}");
                let _ = route(&mut state, &special(DomainKey::Esc), &facts("c1"));
                assert!(presentation::is_open(&state, Surface::Decision));
            }
            let _ = route(&mut state, &special(DomainKey::Esc), &facts("c1"));
            assert!(!presentation::is_open(&state, Surface::Decision));
            assert!(state.decisions.holds(DecisionKind::Ticket));
            assert_eq!(state.input_buffer, draft());
        }
    }
}
