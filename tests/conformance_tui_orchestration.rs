// Story 14.3c wiring contracts, with 21.4 behavioral migrations of moved Wave seams.
// Unchanged legacy guards remain below; migrated paint/effect witnesses call the
// actual production collaborators instead of pinning their source layout.

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use rustain::adapters::tui::{
    app::InputAction,
    handlers::wave_overlay,
    presentation::Surface,
    state::{PendingSpawnGate, WaveViewState},
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
    gate: Option<&PendingSpawnGate>,
    handle: Option<&dyn WaveHandle>,
    wave: Option<&WaveViewState>,
    frame_open: bool,
) -> String {
    use ratatui::{Terminal, backend::TestBackend};
    let mut terminal = Terminal::new(TestBackend::new(80, 16)).unwrap();
    terminal
        .draw(|frame| {
            rustain::infrastructure::runtime::event_loop::paint_wave_base(
                frame,
                frame.area(),
                gate,
                handle,
                wave,
                None,
                frame_open,
            )
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

fn render_runtime(
    h: &mut crate::e2e_harness::TestHarness,
    tabs: &mut TabManager,
    core: &rustain::infrastructure::runtime::agent_core::AgentCore,
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
        core,
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
        "SpawnGateCap",
        "SpawnGateAdjustLeft",
        "SpawnGateAdjustRight",
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
    let core = rustain::infrastructure::runtime::agent_core::AgentCore::test_noop();

    let absent = render_runtime(&mut h, &mut tabs, &core);
    assert!(!absent.contains("handles") && !absent.contains("spawn 5?"));
    h.state.wave_state = Some(WaveViewState::started(AgentId::root(), 3));
    let first = render_runtime(&mut h, &mut tabs, &core);
    assert!(
        first.contains("3 handles") && first.contains("0/3 done"),
        "{first}"
    );
    assert_ne!(absent, first, "runtime must paint the live strip");

    h.state.wave_state.as_mut().unwrap().completed_count = 2;
    let advanced = render_runtime(&mut h, &mut tabs, &core);
    assert!(advanced.contains("2/3 done"), "{advanced}");
    assert!(!advanced.contains("0/3 done"));
    assert_ne!(first, advanced, "runtime progress must change pixels");

    h.state.pending_spawn_gate = Some(PendingSpawnGate {
        spec: rustain::adapters::tui::fanout_spec::parse_fanout(Some("5 payload-safe-fixture"))
            .unwrap(),
        requested: 5,
        threshold: 2,
        adjusted: None,
    });
    let gated = render_runtime(&mut h, &mut tabs, &core);
    assert!(
        gated.contains("spawn 5?") && gated.contains("confirm"),
        "{gated}"
    );
    assert!(!gated.contains("2/3 done"), "gate must replace the strip");
    assert_ne!(advanced, gated, "runtime must paint the pending gate");

    h.state.pending_spawn_gate.as_mut().unwrap().adjusted = Some(4);
    let adjusted = render_runtime(&mut h, &mut tabs, &core);
    assert!(adjusted.contains("spawn 4?"), "{adjusted}");
    assert!(!adjusted.contains("spawn 5?"));
    assert_ne!(gated, adjusted, "runtime must repaint gate adjustment");

    h.state.pending_spawn_gate = None;
    let restored = render_runtime(&mut h, &mut tabs, &core);
    assert!(
        restored.contains("2/3 done") && !restored.contains("spawn 4?"),
        "{restored}"
    );
    h.state.wave_state = None;
    let cleared = render_runtime(&mut h, &mut tabs, &core);
    assert!(
        !cleared.contains("handles") && !cleared.contains("spawn 4?"),
        "{cleared}"
    );
}

#[test]
fn wave_widgets_are_called_from_chat_pane_composition_root() {
    let mut wave = WaveViewState::started(AgentId::root(), 3);
    let first = paint_base(None, None, Some(&wave), false);
    assert!(
        first.contains("3 handles") && first.contains("0/3 done"),
        "{first}"
    );
    wave.completed_count = 2;
    let advanced = paint_base(None, None, Some(&wave), false);
    assert!(advanced.contains("2/3 done"), "{advanced}");
    assert_ne!(
        first, advanced,
        "real live-strip progress must change pixels"
    );
    assert!(!paint_base(None, None, None, false).contains("handles"));

    let handle = PaintWave::new(2);
    let mut gate = PendingSpawnGate {
        spec: rustain::adapters::tui::fanout_spec::parse_fanout(Some("5 payload-safe-fixture"))
            .unwrap(),
        requested: 5,
        threshold: 2,
        adjusted: None,
    };
    let gated = paint_base(Some(&gate), Some(handle.as_ref()), Some(&wave), false);
    assert!(
        gated.contains("spawn 5?") && gated.contains("confirm"),
        "{gated}"
    );
    assert!(!gated.contains("PULLED-SPOKE") && !gated.contains("2/3 done"));
    assert_eq!(
        handle.snapshots.load(Ordering::SeqCst),
        0,
        "gate wins before a retained snapshot is pulled"
    );
    gate.adjusted = Some(4);
    let adjusted = paint_base(Some(&gate), Some(handle.as_ref()), Some(&wave), false);
    assert!(adjusted.contains("spawn 4?"), "{adjusted}");
    assert_ne!(gated, adjusted);

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
    let first = paint_base(None, Some(old.as_ref()), Some(&push), false);
    assert!(
        first.contains("PULLED-SPOKE-1") && !first.contains("PULLED-SPOKE-2"),
        "{first}"
    );
    push.completed_count = 17;
    assert_eq!(
        first,
        paint_base(None, Some(old.as_ref()), Some(&push), false),
        "stale push changes cannot affect retained paint"
    );
    let swapped = PaintWave::new(3);
    let changed = paint_base(None, Some(swapped.as_ref()), Some(&push), false);
    assert!(changed.contains("PULLED-SPOKE-2"), "{changed}");
    assert_ne!(
        first, changed,
        "swapping the actual handle must visibly change the result rows"
    );
    let reads = swapped.snapshots.load(Ordering::SeqCst);
    assert!(!paint_base(None, Some(swapped.as_ref()), Some(&push), true).contains("PULLED-SPOKE"));
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
            && fanout_command.contains("pending_spawn_gate"),
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
