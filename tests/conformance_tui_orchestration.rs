// Story 14.3c wiring contracts, with 21.4 behavioral migrations of moved Wave seams.
// Unchanged legacy guards remain below; migrated paint/effect witnesses call the
// actual production collaborators instead of pinning their source layout.

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use rustain::adapters::tui::{
    app::InputAction, handlers::wave_overlay, presentation::Surface, state::WaveViewState,
};
use rustain::domain::models::orchestration::{
    DrillId, ForkJoinOutcome, SpokeResult, SynthesisView,
};
use rustain::domain::models::{AgentId, FocusState, TabManager};
use rustain::domain::ports::wave_handle::{DrillBody, WaveHandle, WaveSnapshot};

// Retained-data witness only; the native smoke proves real provider request counts.
#[derive(Debug)]
struct PaintWave {
    count: usize,
    snapshots: AtomicUsize,
    drills: AtomicUsize,
    cancels: AtomicUsize,
}

impl PaintWave {
    fn new(count: usize) -> Arc<Self> {
        Arc::new(Self {
            count,
            snapshots: AtomicUsize::new(0),
            drills: AtomicUsize::new(0),
            cancels: AtomicUsize::new(0),
        })
    }
}

impl WaveHandle for PaintWave {
    fn snapshot(&self) -> WaveSnapshot {
        self.snapshots.fetch_add(1, Ordering::SeqCst);
        WaveSnapshot {
            spoke_count: self.count,
            completed: self.count,
            honest_empty: Some(self.count == 0),
            cancelled: false,
            outcome: ForkJoinOutcome {
                spokes: (0..self.count)
                    .map(|slot| {
                        (
                            AgentId::root(),
                            SpokeResult::Completed {
                                summary: format!("PULLED-SPOKE-{slot}"),
                            },
                        )
                    })
                    .collect(),
                synthesis: SynthesisView::default(),
            },
            resolve_count: self.resolve_count(),
            slots: self.slots(),
            rerun_counts: vec![0; self.count],
        }
    }
    fn cancel(&self) {
        self.cancels.fetch_add(1, Ordering::SeqCst);
    }
    fn drill(&self, _: usize) -> Option<DrillBody> {
        self.drills.fetch_add(1, Ordering::SeqCst);
        None
    }
    fn drill_id(&self, _: usize) -> Option<DrillId> {
        None
    }
    fn slots(&self) -> Vec<AgentId> {
        vec![AgentId::root(); self.count]
    }
    fn rerun_count_for_slot(&self, _: usize) -> u8 {
        0
    }
    fn resolve_count(&self) -> usize {
        self.drills.load(Ordering::SeqCst)
    }
}

fn paint_base(
    handle: Option<&dyn WaveHandle>,
    wave: Option<&WaveViewState>,
    frame_open: bool,
) -> String {
    use ratatui::{Terminal, backend::TestBackend};
    let mut terminal = Terminal::new(TestBackend::new(80, 16)).unwrap();
    terminal
        .draw(|frame| {
            let _ = rustain::infrastructure::runtime::event_loop::paint_wave_base(
                frame,
                frame.area(),
                handle,
                wave,
                None,
                frame_open,
            );
        })
        .unwrap();
    terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect()
}

pub(crate) fn render_runtime(
    h: &mut crate::e2e_harness::TestHarness,
    tabs: &mut TabManager,
) -> String {
    rustain::infrastructure::runtime::event_loop::render(
        &mut h.terminal,
        &mut h.state,
        &h.conversation,
        &h.streaming,
        "fixture-model",
        "fixture-provider",
        &rustain::adapters::noop::NoOpSecurity,
        1,
        0,
        Some(tabs),
        &rustain::domain::services::session_index::SessionIndex::new(),
        128_000,
    )
    .unwrap();
    h.screen_text()
}

fn wave_stage(handle: Arc<PaintWave>) -> (crate::e2e_harness::TestHarness, TabManager) {
    let mut h = crate::e2e_harness::TestHarness::new();
    let mut tabs = TabManager::new(tokio_util::sync::CancellationToken::new());
    tabs.active_tab_mut().conversation = std::mem::take(&mut h.conversation);
    h.state.active_tab_id = tabs.active_tab_id();
    h.state.focus = FocusState::Chat;
    h.state.wave_run = Some(handle);
    (h, tabs)
}

fn apply_wave(
    h: &mut crate::e2e_harness::TestHarness,
    tabs: &TabManager,
    key: char,
    expected: InputAction,
) {
    let action = h.type_char(key);
    assert_eq!(action, expected);
    assert!(wave_overlay::apply(
        &mut h.state,
        &tabs.active_tab().conversation.id,
        &action
    ));
}
fn count(haystack: &str, needle: &str) -> usize {
    haystack.matches(needle).count()
}

#[test]
fn wave_input_actions_are_produced_and_consumed() {
    let app = include_str!("../src/adapters/tui/app.rs");
    let event_loop = include_str!("../src/infrastructure/runtime/event_loop.rs");

    for action in [
        "WaveRerunSpoke",
        "WaveDrillSpoke",
        "OpenWaveOverlay",
        "CloseWaveOverlay",
        "OpenDivergeView",
        "CloseDivergeView",
        "DismissWave",
        "SpawnGateConfirm",
    ] {
        assert!(
            count(app, &format!("InputAction::{action}")) >= 1,
            "{action} must be produced by input routing"
        );
        assert!(
            count(event_loop, &format!("InputAction::{action}")) >= 1,
            "{action} must be consumed by the event loop"
        );
    }
}

#[test]
fn runtime_render_wires_live_wave_strip_and_pending_spawn_gate() {
    let mut h = crate::e2e_harness::TestHarness::new();
    let mut tabs = TabManager::new(tokio_util::sync::CancellationToken::new());
    tabs.active_tab_mut().conversation = h.conversation.clone();
    h.state.active_tab_id = tabs.active_tab_id();
    h.state.focus = FocusState::Chat;
    let absent = render_runtime(&mut h, &mut tabs);
    assert!(!absent.contains("handles") && !absent.contains("spawn 5?"));
    h.state.wave_state = Some(WaveViewState::started(AgentId::root(), 3));
    let first = render_runtime(&mut h, &mut tabs);
    assert!(
        first.contains("3 handles") && first.contains("0/3 done"),
        "{first}"
    );
    assert_ne!(absent, first, "runtime must paint the live strip");

    h.state.wave_state.as_mut().unwrap().completed_count = 2;
    let advanced = render_runtime(&mut h, &mut tabs);
    assert!(advanced.contains("2/3 done"), "{advanced}");
    assert!(!advanced.contains("0/3 done"));
    assert_ne!(first, advanced, "runtime progress must change pixels");

    // The gate is a queue item raised for the active conversation; its frame covers the
    // Stage (and the strip under it) and the real keys adjust the queued count.
    let clock = std::sync::Arc::new(rustain::domain::clock::MockClock::new(
        std::time::Instant::now(),
    ));
    h.state.notifications = rustain::adapters::tui::notifications::NotificationStore::from_config(
        &rustain::domain::models::NotificationsConfig::default(),
        clock.clone(),
    );
    let active = h.conversation.id.clone();
    rustain::adapters::tui::decision_queue::raise_spawn_gate(
        &mut h.state,
        &active,
        rustain::adapters::tui::decision_queue::PendingSpawnGate {
            spec: rustain::adapters::tui::fanout_spec::parse_fanout(Some("5 payload-safe-fixture"))
                .unwrap(),
            requested: 5,
            threshold: 2,
            adjusted: None,
        },
    );
    let gated = render_runtime(&mut h, &mut tabs);
    assert!(
        gated.contains("spawn 5?") && gated.contains("Confirm"),
        "{gated}"
    );
    assert!(!gated.contains("2/3 done"), "gate must replace the strip");
    assert_ne!(advanced, gated, "runtime must paint the pending gate");

    // The first paint started the inert clock: let it pass, then `Left` spawns one fewer.
    clock.advance(rustain::adapters::tui::decision_queue::INERT_AFTER_PAINT);
    let facts = rustain::adapters::tui::notification_log::LogFacts {
        active_conversation_id: Some(active.clone()),
        ..Default::default()
    };
    let left = rustain::adapters::tui::presentation::route(
        &mut h.state,
        &rustain::domain::events::DomainInputEvent::SpecialKey(
            rustain::domain::events::DomainKey::Left,
        ),
        rustain::adapters::tui::keymap::Host::Local,
        &facts,
    );
    assert!(left.is_some(), "the open gate frame owns the key");
    let adjusted = render_runtime(&mut h, &mut tabs);
    assert!(adjusted.contains("spawn 4?"), "{adjusted}");
    assert!(!adjusted.contains("spawn 5?"));
    assert_ne!(gated, adjusted, "runtime must repaint gate adjustment");

    // `n` cancels the spawn: the queued gate leaves and the strip is back.
    let cancelled = rustain::adapters::tui::presentation::route(
        &mut h.state,
        &rustain::domain::events::DomainInputEvent::KeyPress('n'),
        rustain::adapters::tui::keymap::Host::Local,
        &facts,
    );
    assert!(cancelled.is_some());
    assert!(h.state.decisions.is_empty());
    let restored = render_runtime(&mut h, &mut tabs);
    assert!(
        restored.contains("2/3 done") && !restored.contains("spawn 4?"),
        "{restored}"
    );
    h.state.wave_state = None;
    let cleared = render_runtime(&mut h, &mut tabs);
    assert!(
        !cleared.contains("handles") && !cleared.contains("spawn 4?"),
        "{cleared}"
    );
}

#[test]
fn wave_widgets_are_called_from_chat_pane_composition_root() {
    let mut wave = WaveViewState::started(AgentId::root(), 3);
    let first = paint_base(None, Some(&wave), false);
    assert!(
        first.contains("3 handles") && first.contains("0/3 done"),
        "{first}"
    );
    wave.completed_count = 2;
    let advanced = paint_base(None, Some(&wave), false);
    assert!(advanced.contains("2/3 done"), "{advanced}");
    assert_ne!(
        first, advanced,
        "real live-strip progress must change pixels"
    );
    assert!(!paint_base(None, None, false).contains("handles"));

    let handle = PaintWave::new(2);
    // With a decision frame open (the spawn gate is one) the base paints no retained
    // snapshot and pulls none: the frame owns the Stage. The same call with no frame
    // pulls it (the positive control).
    let gated = paint_base(Some(handle.as_ref()), Some(&wave), true);
    assert!(!gated.contains("PULLED-SPOKE") && !gated.contains("2/3 done"));
    assert_eq!(
        handle.snapshots.load(Ordering::SeqCst),
        0,
        "an open frame wins before a retained snapshot is pulled"
    );
    let open = paint_base(Some(handle.as_ref()), Some(&wave), false);
    assert!(open.contains("PULLED-SPOKE"), "{open}");
    assert_eq!(handle.snapshots.load(Ordering::SeqCst), 1);
    assert_ne!(gated, open);

    let (mut h, tabs) = wave_stage(handle.clone());
    h.render_active_tab(&tabs);
    let before = h.screen_text();
    apply_wave(&mut h, &tabs, 'r', InputAction::OpenWaveOverlay);
    h.render_active_tab(&tabs);
    let list = h.screen_text();
    assert!(
        list.contains("PULLED-SPOKE-0") && list.contains("PULLED-SPOKE-1"),
        "{list}"
    );
    assert_ne!(before, list);
    let caller = h.state.modal_stack.top().unwrap().id;
    assert_eq!(
        h.state.modal_stack.top().unwrap().surface,
        Surface::WaveOverlay
    );
    apply_wave(&mut h, &tabs, 'd', InputAction::OpenDivergeView);
    h.render_active_tab(&tabs);
    let divergence = h.screen_text();
    assert!(
        divergence.contains("SPOKE-0") && divergence.contains("SPOKE-1"),
        "{divergence}"
    );
    assert!(divergence.contains("PULLED-SPOKE-0") && divergence.contains("PULLED-SPOKE-1"));
    assert_ne!(
        list, divergence,
        "the real divergence painter must replace the list projection"
    );
    assert_eq!(h.state.modal_stack.top().unwrap().id, caller);
    assert_eq!(handle.drills.load(Ordering::SeqCst), 0);
    assert_eq!(handle.cancels.load(Ordering::SeqCst), 0);
}

#[test]
fn wave_widgets_define_no_new_glyph_constants() {
    for (path, source) in [
        (
            "diverge_view.rs",
            include_str!("../src/adapters/tui/widgets/diverge_view.rs"),
        ),
        (
            "result_row.rs",
            include_str!("../src/adapters/tui/widgets/result_row.rs"),
        ),
        (
            "wave_overlay.rs",
            include_str!("../src/adapters/tui/widgets/wave_overlay.rs"),
        ),
        (
            "exceptional_spawn_gate.rs",
            include_str!("../src/adapters/tui/widgets/exceptional_spawn_gate.rs"),
        ),
    ] {
        assert!(
            !source.contains("GLYPH") && !source.contains("const WARNING"),
            "{path} must reuse orchestration_glyph / visual symbols instead of defining a glyph set"
        );
    }
}

// ─── AC2 boundary test: gate_decision threshold=2, exercises {1,2,3} ─────────

#[test]
fn ac2_gate_decision_boundary_threshold_2() {
    use rustain::adapters::tui::widgets::exceptional_spawn_gate::{GateDecision, gate_decision};

    // Below threshold: silent
    assert_eq!(gate_decision(1, 2), GateDecision::Allow);
    // At threshold: silent (requested <= threshold)
    assert_eq!(gate_decision(2, 2), GateDecision::Allow);
    // Above threshold: gate
    assert_eq!(gate_decision(3, 2), GateDecision::Refuse);

    // Positive control: a per-spawn-modal mutant that prompts below threshold
    // would return Refuse for requested=1 — our test kills it.
}

// ─── AC3 diverge: d/s is zero-dispatch (reuses handles, never dispatches) ────

#[test]
fn ac3_diverge_view_render_does_not_dispatch() {
    // DivergeView is a pure render over existing spoke outcomes.
    // Verify render_diverge_view is a pure function with no side effects
    // by calling it and checking it returns lines without panicking.
    use rustain::adapters::tui::widgets::diverge_view::{DivergeSnapshot, render_diverge_view};
    use rustain::domain::models::orchestration::SpokeResult;

    // 3-spoke snapshot — the diverge view reads this, never dispatches
    let spokes = vec![
        (
            "SPOKE-0".to_string(),
            SpokeResult::Completed {
                summary: "alpha".to_string(),
            },
        ),
        (
            "SPOKE-1".to_string(),
            SpokeResult::Completed {
                summary: "beta".to_string(),
            },
        ),
        (
            "SPOKE-2".to_string(),
            SpokeResult::Completed {
                summary: "alpha".to_string(),
            },
        ),
    ];
    let snap = DivergeSnapshot::new(spokes.clone(), 120);
    let lines = render_diverge_view(&snap);
    assert!(!lines.is_empty(), "diverge view must render lines");

    // d→s→d cycle: re-render with the SAME snapshot — zero cost, no re-fork
    let lines2 = render_diverge_view(&snap);
    assert_eq!(lines.len(), lines2.len(), "d→s→d identity: same output");

    // Empty wave: "No spokes to compare"
    let empty_snap = DivergeSnapshot::new(Vec::new(), 80);
    let empty_lines = render_diverge_view(&empty_snap);
    let text: String = empty_lines.iter().map(|l| l.to_string()).collect();
    assert!(
        text.contains("No spokes to compare"),
        "empty wave must show 'No spokes to compare', got: {text}"
    );
}

// ─── AC3 degrade: ≥120 side-by-side, <120 stacked ───────────────────────────

#[test]
fn ac3_diverge_view_degrade_layout() {
    use rustain::adapters::tui::widgets::diverge_view::{DivergeSnapshot, render_diverge_view};
    use rustain::domain::models::orchestration::SpokeResult;

    let spokes = vec![
        (
            "A".to_string(),
            SpokeResult::Completed {
                summary: "different-alpha".to_string(),
            },
        ),
        (
            "B".to_string(),
            SpokeResult::Completed {
                summary: "different-beta".to_string(),
            },
        ),
    ];

    // ≥120 cols: the snapshot carries width for the render
    let wide = DivergeSnapshot::new(spokes.clone(), 120);
    let wide_lines = render_diverge_view(&wide);

    // <120 cols: stacked
    let narrow = DivergeSnapshot::new(spokes, 80);
    let narrow_lines = render_diverge_view(&narrow);

    // D3 (AI-12.3): a REAL differential, not just "both non-empty". At ≥120 cols
    // the two spokes share ONE side-by-side row; at <120 they stack one-per-line.
    // A mutant that ignores width (always-stacked OR always-side-by-side) fails
    // exactly one of these two assertions. (`to_string()` on a Line flattens its
    // spans — same pattern the empty-wave assertion above uses.)
    let wide_has_both = wide_lines.iter().any(|l| {
        let t = l.to_string();
        t.contains("alpha") && t.contains("beta")
    });
    let narrow_has_both = narrow_lines.iter().any(|l| {
        let t = l.to_string();
        t.contains("alpha") && t.contains("beta")
    });
    assert!(
        wide_has_both,
        "≥120 cols must place both spokes on ONE side-by-side row: {wide_lines:?}"
    );
    assert!(
        !narrow_has_both,
        "<120 cols must stack one spoke per line (no row carries both): {narrow_lines:?}"
    );
}

// Pull-vs-push honesty at the production base and frame painters, with a swapped-handle control.
#[test]
fn wave_render_pulls_count_from_handle_not_push_counter() {
    let mut push = WaveViewState::started(AgentId::root(), 19);
    push.completed_count = 18;
    let old = PaintWave::new(2);
    let first = paint_base(Some(old.as_ref()), Some(&push), false);
    assert!(
        first.contains("PULLED-SPOKE-1") && !first.contains("PULLED-SPOKE-2"),
        "{first}"
    );
    push.completed_count = 17;
    assert_eq!(
        first,
        paint_base(Some(old.as_ref()), Some(&push), false),
        "stale push changes cannot affect retained paint"
    );
    let swapped = PaintWave::new(3);
    let changed = paint_base(Some(swapped.as_ref()), Some(&push), false);
    assert!(changed.contains("PULLED-SPOKE-2"), "{changed}");
    assert_ne!(
        first, changed,
        "swapping the actual handle must visibly change the result rows"
    );
    let reads = swapped.snapshots.load(Ordering::SeqCst);
    assert!(!paint_base(Some(swapped.as_ref()), Some(&push), true).contains("PULLED-SPOKE"));
    assert_eq!(
        swapped.snapshots.load(Ordering::SeqCst),
        reads,
        "an owned full frame suppresses duplicate base snapshots"
    );

    let (mut h, tabs) = wave_stage(old);
    h.state.wave_state = Some(push);
    apply_wave(&mut h, &tabs, 'r', InputAction::OpenWaveOverlay);
    let caller = h.state.modal_stack.top().unwrap().id;
    h.render_active_tab(&tabs);
    let first_frame = h.screen_text();
    assert!(first_frame.contains("1 of 2"), "{first_frame}");
    h.state.handle_wave_run_ready(swapped);
    h.render_active_tab(&tabs);
    let swapped_frame = h.screen_text();
    assert!(
        swapped_frame.contains("1 of 3") && swapped_frame.contains("PULLED-SPOKE-2"),
        "{swapped_frame}"
    );
    assert_ne!(first_frame, swapped_frame);
    assert_eq!(
        h.state.modal_stack.top().unwrap().id,
        caller,
        "completion updates data, never replaces the frame"
    );
}

// ─── D4 (AI-12.3, DF-CR-14-3c-4): /fanout CONSULTS gate_decision (the wiring) ──
// `ac2_gate_decision_boundary_threshold_2` proves the pure fn at {1,2,3}; it does
// NOT prove `/fanout` routes through it. This guards the wiring: the handler must
// consult `gate_decision`, open the spawn gate on Refuse, and launch on Allow. A
// mutant that drops the gate and launches unconditionally removes `gate_decision(`
// from the arm → RED. Requiring BOTH branches is the built-in positive control.
#[test]
fn fanout_consults_gate_decision_before_launch() {
    let event_loop = include_str!("../src/infrastructure/runtime/event_loop.rs");
    let fanout_arm = event_loop
        .split("cmd_name == \"fanout\"")
        .nth(1)
        .expect("the /fanout ExecuteCommand arm must exist in event_loop.rs")
        .split("cmd_name == \"team\"")
        .next()
        .expect("the /fanout arm must be bounded by the /team branch");
    assert!(
        fanout_arm.contains("transparency_bridge::fanout_command("),
        "/fanout must delegate into the runtime bridge"
    );

    let bridge = include_str!("../src/infrastructure/runtime/transparency_bridge.rs");
    let fanout_command = bridge
        .split("pub(crate) fn fanout_command(")
        .nth(1)
        .expect("the /fanout bridge command must exist")
        .split("pub(crate) async fn team_command(")
        .next()
        .expect("the /fanout bridge command must be bounded by the /team command");

    assert!(
        fanout_command.contains("gate_decision("),
        "/fanout must consult gate_decision before launching the wave"
    );
    assert!(
        fanout_command.contains("GateDecision::Refuse")
            && fanout_command.contains("raise_spawn_gate"),
        "the Refuse path must open the spawn gate, not silently launch"
    );
    assert!(
        fanout_command.contains("GateDecision::Allow")
            && fanout_command.contains("launch_wave_request("),
        "the Allow path must launch the wave"
    );
    // F5 (AI-12.3 post-review party-mode): the launch-on-Refuse equivalent
    // mutant. The positive assertions above prove the Refuse arm opens the
    // gate; this negative assertion proves it cannot also launch.
    let refuse_arm = fanout_command
        .split("GateDecision::Refuse =>")
        .nth(1)
        .expect("the Refuse arm must exist");
    assert!(
        !refuse_arm.contains("launch_wave_request("),
        "F5: the Refuse arm must NOT launch the wave — a launch-on-Refuse mutant \
         reopens the bypass (gate consulted but its result ignored)"
    );
}

// ─── tui-5a (AD-3): dispatch arms with no in-process seam ──────────────────────
// `event_loop::run` has no test seam; the key -> `InputAction` decoders and the
// handlers each have Rust tests (21_9 K3, 21_13 K2/K5/K9, 21_17). These guards pin
// the arm that joins them, the way the /fanout guard above does: the arm exists,
// is bounded by its neighbour, calls the production handler and (for copy) passes
// the production clipboard function, not a closure.
const PRODUCTION_COPIER: &str = "&crate::adapters::tui::clipboard::copy_to_clipboard)";

/// The text of the `InputAction::<head>` arm, bounded by the next `InputAction::`.
fn input_action_arm<'a>(event_loop: &'a str, head: &str) -> &'a str {
    event_loop
        .split(head)
        .nth(1)
        .unwrap_or_else(|| panic!("the {head} arm must exist in event_loop.rs"))
        .split("InputAction::")
        .next()
        .expect("the arm is bounded by the next InputAction arm")
}

#[test]
fn stage_item_arms_delegate_to_their_seams() {
    let event_loop = include_str!("../src/infrastructure/runtime/event_loop.rs");

    let cycle = input_action_arm(event_loop, "InputAction::CycleInvocationInFocusedTurn =>");
    assert!(
        cycle.contains("handlers::tool_focus::cycle_item(")
            && cycle.contains("select_item_explicitly("),
        "Tab must cycle the focused turn's items and select the result: {cycle}"
    );

    let inspect = input_action_arm(event_loop, "InputAction::InspectTool =>");
    assert!(
        inspect.contains("block_viewer::inspect_focused(")
            && !inspect.contains("tool_inspector::inspect_focused("),
        "Enter must inspect through the block viewer seam (it opens tools, blocks and thinking): {inspect}"
    );

    let step = input_action_arm(event_loop, "InputAction::StepBlock { forward } =>");
    assert!(
        step.contains("block_viewer::step(")
            && step.contains("open_turn.as_ref(), forward)")
            && !step.contains("!forward"),
        "`[`/`]` must step the block viewer with the unmodified requested direction: {step}"
    );

    let copy_block = input_action_arm(event_loop, "InputAction::CopyBlockSource =>");
    assert!(
        copy_block.contains("block_viewer::copy_source(")
            && !copy_block.contains("tool_inspector::copy::copy_source(")
            && copy_block.contains(PRODUCTION_COPIER),
        "`c` in S-BLOCK must copy the block source through the production sink: {copy_block}"
    );

    let copy_chat = input_action_arm(event_loop, "InputAction::CopyToClipboard(content) =>");
    assert!(
        copy_chat.contains("handlers::clipboard::requested_in(")
            && copy_chat.contains("content")
            && copy_chat.contains(PRODUCTION_COPIER),
        "Chat `c` must copy through the production sink: {copy_chat}"
    );
}

#[test]
fn room_and_peer_command_arms_delegate_to_their_bridges() {
    let event_loop = include_str!("../src/infrastructure/runtime/event_loop.rs");
    let room = event_loop
        .split("cmd_name == \"room\"")
        .nth(1)
        .expect("the /room ExecuteCommand arm must exist in event_loop.rs")
        .split("cmd_name == \"artifacts\"")
        .next()
        .expect("the /room arm must be bounded by the /artifacts branch");
    assert!(
        room.contains("room_bridge::room_command(") && !room.contains("peer_command("),
        "/room must delegate to the room bridge: {room}"
    );
    let peer = event_loop
        .split("cmd_name == \"peer\"")
        .nth(1)
        .expect("the /peer ExecuteCommand arm must exist in event_loop.rs")
        .split("} else if")
        .next()
        .expect("the /peer arm must be bounded by the next branch");
    assert!(
        peer.contains("peer_bridge::peer_command(") && !peer.contains("room_command("),
        "/peer must delegate to the peer bridge: {peer}"
    );
}

/// tui-5b K1: the bookmark, wave-overlay, history, cross-search, scroll and turn-completion
/// arms in `event_loop::run` delegate to their handlers. `run` has no test seam, so each arm
/// is pinned by source text, the way the tui-5a guards above do. Guards the named mutants
/// MB1 (`OpenBookmarkList` closes instead of opening), MB2 (`JumpToBookmark` body emptied),
/// MB3 (`WaveDrillSpoke(_)` dropped from the wave or-pattern), MB4 (`OpenHistory` passes a
/// fresh `SessionIndex`), MB5 (`CrossSearchQueryChanged` scan removed), MB6 (`ScrollFullPageUp`
/// passes `HalfPageUp` or drops the handler), MB7 (the toggle no longer awaits `_pending_save`),
/// MB15 (`WaveRunReady` drops `handle_wave_run_ready`) and MB16 (the turn-completion
/// `session_index.touch` removed).
#[test]
fn bookmark_wave_history_and_scroll_arms_delegate_to_their_handlers() {
    let event_loop = include_str!("../src/infrastructure/runtime/event_loop.rs");

    // ── Bookmarks ──
    let toggle = input_action_arm(event_loop, "InputAction::ToggleBookmark =>");
    let taken = toggle
        .find("if let Some(prev) = _pending_save.take()")
        .expect("the toggle arm must take the in-flight save first");
    let awaited = toggle
        .find("prev.await")
        .expect("the toggle arm must await the taken save handle");
    let handled = toggle
        .find("handlers::bookmark::handle_apply_bookmark_toggle(")
        .expect("the toggle arm must call the bookmark toggle handler");
    assert!(
        taken < awaited && awaited < handled && toggle.contains("&*fs_storage"),
        "the bookmark toggle must take and await `_pending_save` before the handler writes \
         meta.json: {toggle}"
    );
    assert!(
        toggle[handled..].contains(".await"),
        "the bookmark toggle handler itself must be awaited: {toggle}"
    );

    let open_list = input_action_arm(event_loop, "InputAction::OpenBookmarkList =>");
    assert!(
        open_list.contains("handlers::bookmark::handle_apply_open_bookmark_list(")
            && !open_list.contains("bookmark::close("),
        "OpenBookmarkList must open the list, not close it: {open_list}"
    );

    let jump = input_action_arm(event_loop, "InputAction::JumpToBookmark =>");
    assert!(
        jump.contains("handlers::bookmark::handle_apply_jump_bookmark("),
        "JumpToBookmark must jump through the bookmark handler: {jump}"
    );

    let close_list = input_action_arm(event_loop, "InputAction::CloseBookmarkList =>");
    assert!(
        close_list.contains("handlers::bookmark::close(&mut state)"),
        "CloseBookmarkList must close the bookmark list: {close_list}"
    );

    // ── Wave overlay (combined arm + WaveRunReady event) ──
    let wave = event_loop
        .split("action @ (InputAction::OpenWaveOverlay | InputAction::CloseWaveOverlay")
        .nth(1)
        .expect("the combined wave-overlay arm must exist in event_loop.rs")
        .split("InputAction::SpawnGateConfirm =>")
        .next()
        .expect("the combined wave arm must be bounded by the SpawnGateConfirm arm");
    assert!(
        wave.contains("InputAction::WaveDrillSpoke(_)")
            && wave.contains("InputAction::OpenDivergeView")
            && wave.contains("InputAction::CloseDivergeView")
            && wave.contains("InputAction::DismissWave) =>")
            && wave
                .contains("handlers::wave_overlay::apply(&mut state, &conversation.id, &action)"),
        "the wave overlay arm must route all six actions through wave_overlay::apply: {wave}"
    );

    let wave_ready = event_loop
        .split("AppEvent::WaveRunReady(handle) =>")
        .nth(1)
        .expect("the WaveRunReady event arm must exist in event_loop.rs")
        .split("AppEvent::")
        .next()
        .expect("the WaveRunReady arm is bounded by the next AppEvent arm");
    assert!(
        wave_ready.contains("state.handle_wave_run_ready(handle)"),
        "WaveRunReady must install the handle through the state handler: {wave_ready}"
    );

    // ── History and cross-conversation search ──
    let history = input_action_arm(event_loop, "InputAction::OpenHistory =>");
    assert!(
        history.contains("history_bridge::open(&mut state, &session_index)")
            && !history.contains("SessionIndex::new()"),
        "OpenHistory must open over the live session index: {history}"
    );

    let query = input_action_arm(event_loop, "InputAction::CrossSearchQueryChanged =>");
    assert!(
        query.contains("start_cross_search_scan(") && query.contains("&session_index"),
        "a cross-search query change must start a scan over the live session index: {query}"
    );

    let open_result = input_action_arm(event_loop, "InputAction::OpenCrossSearchResult =>");
    assert!(
        open_result.contains("apply_open_cross_search_result("),
        "OpenCrossSearchResult must open the chosen result: {open_result}"
    );

    let close_search = input_action_arm(event_loop, "InputAction::CloseCrossSearch =>");
    assert!(
        close_search.contains("close_cross_search(&mut state)"),
        "CloseCrossSearch must close the cross-search overlay: {close_search}"
    );

    // ── Scroll ──
    let full_up = input_action_arm(event_loop, "InputAction::ScrollFullPageUp =>");
    assert!(
        full_up.contains("handlers::scroll::handle_apply_scroll_intent(")
            && full_up.contains("ScrollDelta::FullPageUp")
            && !full_up.contains("HalfPageUp"),
        "ScrollFullPageUp must scroll a full page up through the scroll handler: {full_up}"
    );

    // ── Turn completion ──
    let completion = event_loop
        .split("// Update sidebar index on turn complete")
        .nth(1)
        .expect("the turn-completion sidebar marker must exist in event_loop.rs")
        .split("Failed to persist session: ")
        .next()
        .expect("the turn-completion block is bounded by the persist error text");
    assert!(
        completion.contains("session_index.touch(")
            && completion.contains("Some(conversation.messages.len())")
            && completion.contains("storage_ref.save_conversation(&conv_clone)"),
        "turn completion must touch the session index and persist the conversation: {completion}"
    );
}

// Keystone K1 (tui-5c): the leader-mode, login, adapters, usage-panel, permission-mode, /tour
// and notification-store joins in `event_loop::run`, which has no test seam. Each positive
// `contains` is paired with the wrong-seam `!contains`. Mutants (not executed): MC1 `Login`
// arm body emptied, MC2 `OpenAdapters` -> `room_bridge::refresh_mission_tab`, MC3
// `OpenUsagePanel` body emptied, MC4 `announce_permission_mode` removed from the handler,
// MC5 `security.set_mode(mode)` removed, MC6 `/tour` -> `tour::tick(&mut state, false)`,
// MC13 the local store built from `NotificationsConfig::default()`, MC14 a `submit_turn!`
// added to the `CycleMode` arm. `OpenHistory` is already pinned in
// `bookmark_wave_history_and_scroll_arms_delegate_to_their_handlers`; a bare Ctrl+Q decodes
// to `InputAction::Quit` (app.rs `convert_crossterm_event` Ctrl+Q -> `CtrlQ`; the Input row of
// the Ctrl+Q table expects Quit), so no Quit arm is added here.
#[test]
fn leader_mode_and_tour_arms_delegate_to_their_handlers() {
    let event_loop = include_str!("../src/infrastructure/runtime/event_loop.rs");

    let login = input_action_arm(event_loop, "InputAction::Login(action) =>");
    assert!(
        login.contains("handlers::login::on_action(") && login.contains(", action)"),
        "the Login arm must hand its action to the login handler: {login}"
    );

    let adapters = input_action_arm(event_loop, "InputAction::OpenAdapters =>");
    assert!(
        adapters.contains("mission_bridge::open_adapters(")
            && !adapters.contains("refresh_mission_tab("),
        "OpenAdapters must open the adapters panel, not refresh the mission tab: {adapters}"
    );

    let usage = input_action_arm(event_loop, "InputAction::OpenUsagePanel =>");
    assert!(
        usage.contains("handlers::usage_panel::handle_open_usage_panel(")
            && !usage.contains("submit_turn!"),
        "OpenUsagePanel must open the usage panel through its handler: {usage}"
    );

    let cycle = input_action_arm(event_loop, "InputAction::CycleMode =>");
    assert!(
        cycle.contains("AppEvent::SetPermissionMode(next)") && !cycle.contains("submit_turn!"),
        "CycleMode must emit SetPermissionMode and start no turn: {cycle}"
    );

    let set_mode = event_loop
        .split("AppEvent::SetPermissionMode(mode) =>")
        .nth(1)
        .expect("the SetPermissionMode event arm must exist in event_loop.rs")
        .split("AppEvent::RetryMessage")
        .next()
        .expect("the SetPermissionMode arm is bounded by the RetryMessage arm");
    // Story 23.1: the mode is set through the sandbox handler, which refuses
    // YOLO while the sandbox is unavailable.
    assert!(
        set_mode.contains("crate::infrastructure::sandbox::apply_permission_mode(")
            && set_mode.contains("mode, security.as_ref()")
            && set_mode.contains("notice_actions::announce_permission_mode(&mut state, mode)")
            && !set_mode.contains("submit_turn!"),
        "SetPermissionMode must set the mode, announce it and start no turn: {set_mode}"
    );

    let tour = event_loop
        .split("cmd_name == \"tour\"")
        .nth(1)
        .expect("the /tour branch must exist in event_loop.rs")
        .split("} else if")
        .next()
        .expect("the /tour branch is bounded by the next else-if");
    assert!(
        tour.contains("crate::adapters::tui::tour::command(&mut state)")
            && !tour.contains("tour::tick("),
        "/tour must run the tour command, not tick it: {tour}"
    );

    let store = event_loop
        .split("state.notifications = notifications::NotificationStore::from_config(")
        .nth(1)
        .expect("the local notification store construction must exist in event_loop.rs")
        .split(");")
        .next()
        .expect("the store construction ends at its closing call");
    assert!(
        store.trim_start().starts_with("&config.notifications,")
            && !store.contains("NotificationsConfig::default()"),
        "the local store must be built from the loaded [notifications] config: {store}"
    );
}

// The real dispatch/effect seam reuses the retained run; provider-count evidence is native.
#[test]
fn diverge_toggle_arms_never_dispatch() {
    for from_list in [false, true] {
        let handle = PaintWave::new(2);
        let (mut h, tabs) = wave_stage(handle.clone());
        h.state.wave_state = Some(WaveViewState::started(AgentId::root(), 2));
        h.state.active_wave_id = Some(7);
        h.state.rerunning_slot = Some(1);
        if from_list {
            apply_wave(&mut h, &tabs, 'r', InputAction::OpenWaveOverlay);
        }
        let retained = h.state.wave_run.as_ref().unwrap().clone();
        let before = format!("{:?}", tabs.active_tab().view_state);
        for _ in 0..3 {
            apply_wave(&mut h, &tabs, 'd', InputAction::OpenDivergeView);
            assert!(h.state.wave_diverge_open);
            assert_eq!(
                h.state.modal_stack.top().unwrap().surface,
                Surface::WaveOverlay
            );
            apply_wave(&mut h, &tabs, 's', InputAction::CloseDivergeView);
            assert!(!h.state.wave_diverge_open);
            assert!(
                h.state.modal_stack.is_empty(),
                "without a drill, legacy s returns to the compact base"
            );
            assert!(Arc::ptr_eq(h.state.wave_run.as_ref().unwrap(), &retained));
            assert!(h.state.wave_state.is_some());
            assert_eq!(h.state.active_wave_id, Some(7));
            assert_eq!(h.state.rerunning_slot, Some(1));
            assert_eq!(format!("{:?}", tabs.active_tab().view_state), before);
        }
        assert_eq!(handle.drills.load(Ordering::SeqCst), 0);
        assert_eq!(handle.cancels.load(Ordering::SeqCst), 0);
        apply_wave(&mut h, &tabs, 'r', InputAction::OpenWaveOverlay);
        let rerun = h.type_char('r');
        assert_eq!(rerun, InputAction::WaveRerunSpoke(0));
        assert!(
            !wave_overlay::apply(&mut h.state, &tabs.active_tab().conversation.id, &rerun),
            "the actual rerun action is outside the view-only effect boundary"
        );
        assert!(wave_overlay::apply(
            &mut h.state,
            &tabs.active_tab().conversation.id,
            &InputAction::WaveDrillSpoke(0)
        ));
        assert_eq!(
            handle.drills.load(Ordering::SeqCst),
            1,
            "independent real handle-operation control"
        );
        handle.cancel();
        assert_eq!(handle.cancels.load(Ordering::SeqCst), 1);
    }
}

// ─── F1 (AI-12.3 post-review): the rerun arm is SUPERVISED (DN-1 parity) ──────
// `launch_wave_request` owns its wave JoinHandle in a supervisor task whose
// `is_panic()` arm converts a panic into a terminal event. The structurally-
// parallel rerun path (WaveRerunSpoke) MUST do the same, or a `rerun_spoke`
// panic is silently swallowed and `rerunning_slot` is never cleared → the AC11
// lamp sticks and the busy-guard fires `RerunRejectedBusy` on every later rerun.
// A full behavioral test needs a mock-terminal loop driver (deferred — `run()`
// reads crossterm events, not an injectable channel); this structural keystone
// is RED the moment the rerun spawn reverts to unsupervised fire-and-forget.
#[test]
fn rerun_arm_is_supervised_panics_clear_the_lamp() {
    let event_loop = include_str!("../src/infrastructure/runtime/event_loop.rs");
    let rerun_arm = event_loop
        .split("InputAction::WaveRerunSpoke(slot) =>")
        .nth(1)
        .expect("WaveRerunSpoke arm must exist");
    let rerun_arm = rerun_arm
        .split("InputAction::OpenWaveOverlay =>")
        .next()
        .expect("rerun arm must be bounded by the next InputAction arm");

    // The rerun JoinHandle is OWNED: an inner spawn produces it and an outer
    // supervisor `await`s it. A fire-and-forget `tokio::spawn(async move { match
    // orchestrator.rerun_spoke(...).await { ... } })` has neither binding.
    assert!(
        rerun_arm.contains("let rerun = tokio::spawn("),
        "F1: the rerun JoinHandle must be bound (supervised), not fire-and-forgotten"
    );
    assert!(
        rerun_arm.contains("match rerun.await"),
        "F1: a supervisor must await the rerun JoinHandle"
    );
    assert!(
        rerun_arm.contains(".is_panic()"),
        "F1: the supervisor needs a panic arm (DN-1 parity with launch_wave_request)"
    );
    // Positive control: the panic arm must clear the lamp (SpokeRerunReverted),
    // so `rerunning_slot` doesn't strand. Scoped to the body between the panic
    // guard and the cancel (`Err(_) =>`) arm.
    let panic_body = rerun_arm
        .split(".is_panic()")
        .nth(1)
        .expect("the panic arm must exist");
    let panic_body = panic_body.split("Err(_) =>").next().unwrap_or(panic_body);
    assert!(
        panic_body.contains("SpokeRerunReverted"),
        "F1: a rerun panic must emit SpokeRerunReverted so rerunning_slot is cleared"
    );
}

// Compare the control-flow seams, not the indentation or explanatory comments.
fn action_arm_code(event_loop: &str, head: &str) -> String {
    input_action_arm(event_loop, head)
        .lines()
        .flat_map(|line| line.split("//").next().unwrap().split_whitespace())
        .collect::<Vec<_>>()
        .join(" ")
}

#[test]
fn fork_arms_verify_then_await_the_fork_and_settle_to_chat() {
    let event_loop = include_str!("../src/infrastructure/runtime/event_loop.rs");
    let fork = action_arm_code(event_loop, "InputAction::ForkAtMessage =>");
    assert!(fork.starts_with("{ if streaming.is_streaming {"));
    assert!(
        fork.contains(concat!(
            "if let Err(error) = handlers::transcript_target::durable_target(&conversation, fork_message_index) { ",
            "notifications::warning(&mut state, NoticeSource::Session, error.to_string()); ",
            "state.needs_redraw = true; continue; } ",
            "let target = &conversation.messages[fork_message_index]; ",
            "dq::raise_fork(&mut state, &conversation.id, fork_message_index, &target.content, ",
            "dq::message_target_fingerprint(target));",
        )),
        "an invalid focused target must exit before indexing the message or raising Fork"
    );

    let confirm = action_arm_code(event_loop, "InputAction::ForkConfirm =>");
    let after_guard = confirm
        .strip_prefix(concat!(
            "{ let Some(DS::Fork(payload)) = state.decisions.take_resolved() else { continue }; ",
            "let message_index = payload.message_index; ",
            "if let Err(notice) = handlers::transcript_target::verify_target(&conversation, message_index, payload.target_hash) { ",
            "notifications::info(&mut state, NoticeSource::Session, notice); ",
            "state.needs_redraw = true; continue; } ",
        ))
        .expect("a stale resolved target must exit the action before any fork work");
    // `fork_result` exposes the handler's Ok(id) as Some(id); that same id must be loaded.
    let (_, load) = after_guard
        .split_once(concat!(
            "let result = handlers::transcript_target::handle_fork_confirm(&*storage, &mut _pending_save, turn_lifecycle.is_live(&conversation.id), &conversation, message_index).await; ",
            "if let Some(new_id) = super::error_notices::fork_result(&mut state, result) { ",
            "match storage.load_conversation(&new_id).await { Ok(Some(forked_conv)) => { ",
        ))
        .expect("Fork must load the id returned by the awaited handler");
    let (success, _) = load
        .split_once("} Ok(None) => {")
        .expect("a successful load must be handled separately from a missing fork");
    let ordered = [
        "create_tab_with_conversation(forked_conv)",
        "notifications::success(&mut state, NoticeSource::Session, hint_msg)",
        "settle_stage_base(&mut state, FocusState::Chat)",
    ];
    let positions: Vec<_> = ordered.iter().map(|s| success.find(s).unwrap()).collect();
    assert!(positions.windows(2).all(|pair| pair[0] < pair[1]));
    assert!(success.contains("message_index + 1"));
    assert!(!confirm.contains("notifications::info(&mut state, NoticeSource::Session, hint_msg)"));

    let cancel = input_action_arm(event_loop, "InputAction::ForkCancel =>");
    assert!(
        cancel
            .contains("let Some(DS::Fork(_)) = state.decisions.take_resolved() else { continue };")
    );
    assert!(!cancel.contains("handle_fork_confirm"));
}

#[test]
fn mission_window_arms_delegate_to_their_seams() {
    let event_loop = include_str!("../src/infrastructure/runtime/event_loop.rs");
    let task = input_action_arm(event_loop, "InputAction::OpenTaskWindow(task) =>");
    assert!(
        task.contains(
            "if !crate::adapters::tui::task_window::open(&mut state, &conversation, &task)"
        )
    );
    assert!(task.contains("super::error_notices::TASK_UNAVAILABLE"));
    assert!(!task.contains("task_window::open(&mut state, &conversation, &task);"));
    let artifact = input_action_arm(event_loop, "InputAction::OpenArtifactWindow(id) =>");
    assert!(artifact.contains("artifact_bridge::open_window(&app_state, &mut state, &conversation.id, &id, security.current_mode()).await"));
    assert!(!artifact.contains("refresh_mission_tab("));
    let history = action_arm_code(event_loop, "InputAction::OpenHistoryEntry(conv_id) =>");
    let present = history
        .strip_prefix(concat!(
            "{ if session_index.get(&conv_id).is_none() { history_bridge::gone(&mut state); } ",
            "else { if conv_id == conversation.id { history_bridge::finish_open(&mut state); } ",
            "else if tab_manager.find_by_conversation(&conv_id).is_some() { ",
        ))
        .expect("only an indexed selection may reach the active, open-tab, or stored paths");
    let (open_tab, load) = present
        .split_once(concat!(
            "history_bridge::finish_open(&mut state); } else { ",
            "match storage.load_conversation(&conv_id).await { Ok(Some(loaded_conv)) => { ",
        ))
        .expect("an indexed selection not already open must be loaded by that same id");
    assert!(
        !open_tab.contains("storage.load_conversation("),
        "switching to an already open tab must not reload from storage"
    );
    let (loaded, failure) = load
        .split_once("Ok(None) => {")
        .expect("the stored open handles a missing conversation separately");
    let before_commit = loaded
        .strip_suffix(concat!(
            "session_index.set_open(&conv_id, true); session_index.set_active(Some(&conv_id)); ",
            "history_bridge::finish_open(&mut state); } ",
        ))
        .expect("only the successful load activates the selected id and closes History");
    assert!(before_commit.contains("conversation = loaded_conv;"));
    assert!(failure.contains("history_bridge::gone(&mut state);"));
    assert!(!failure.contains("history_bridge::finish_open("));
    assert!(!history.contains("SessionIndex::new()"));
}
