// Story 14.3c: TUI ergonomics wiring conformance guards.
// These tests intentionally inspect source wiring: they fail when an
// InputAction variant becomes dead or a shipped widget render becomes orphaned.

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
fn wave_widgets_are_called_from_chat_pane_composition_root() {
    let event_loop = include_str!("../src/infrastructure/runtime/event_loop.rs");
    // P7 (review): scope to the `fn render(` body, then further to the region
    // after `chat_pane::render_with_search` — proves the calls are in the
    // composition root, not just anywhere after the split point in the file.
    let render_fn = event_loop
        .split("fn render(")
        .nth(1)
        .expect("fn render( must exist in event_loop.rs");
    let chat_render = render_fn
        .split("let result = chat_pane::render_with_search(")
        .nth(1)
        .expect("chat pane render call must exist within fn render");

    for call in [
        "render_result_row(",
        "render_wave_overlay(",
        "render_diverge_view(",
        "render_spawn_gate(",
        // 14.3c (human-smoke fix): the IN-PROGRESS wave strip. This entry was
        // MISSING — the widget shipped in 14.3 with green unit tests but zero
        // production wiring, so a running `/fanout` painted nothing. Guarding it
        // here keeps the live strip wired (a mutant that drops the call → RED).
        "render_wave_strip_line(",
    ] {
        assert!(
            chat_render.contains(call),
            "{call} must be reachable from the chat-pane composition root within fn render"
        );
    }
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

// ─── AC11 push-vs-pull structural guard (14.3c AI-12.3, DF-CR-14-3c-6) ───────
// The companion to the value-differential keystone in app.rs
// (`ac11_push_vs_pull_render_reads_swapped_handle_*`). That one proves the
// swapped handle reports the honest count; this one proves the RENDER block
// reads it by PULL — it must never reference the stale push counter
// `wave_state.completed_count`. A mutant that rewires the render to the push
// counter makes this fail the build.

#[test]
fn wave_render_pulls_count_from_handle_not_push_counter() {
    let event_loop = include_str!("../src/infrastructure/runtime/event_loop.rs");
    let render_fn = event_loop
        .split("fn render(")
        .nth(1)
        .expect("fn render( must exist in event_loop.rs");
    // Scope to the COMPLETED wave-render block: from the `state.wave_run` branch
    // to the `WAVE_RUN_RENDER_BLOCK_END` sentinel. The boundary is the sentinel
    // (not `pending_delegation_card`) so the IN-PROGRESS strip branch — which
    // legitimately reads the push counter `wave_state.completed_count` because no
    // handle exists yet — is excluded. This keeps the test's teeth on the
    // completed render (which MUST pull from the handle) without false-failing on
    // the live strip.
    let wave_block = render_fn
        .split("else if let Some(ref handle) = state.wave_run {")
        .nth(1)
        .expect("wave render block must exist within fn render");
    let wave_block = wave_block
        .split("WAVE_RUN_RENDER_BLOCK_END")
        .next()
        .expect("wave render block must be bounded by the WAVE_RUN_RENDER_BLOCK_END sentinel");

    assert!(
        wave_block.contains("handle.snapshot()"),
        "the wave render must PULL state from handle.snapshot()"
    );
    assert!(
        !wave_block.contains("completed_count"),
        "the wave render must NOT read the push counter wave_state.completed_count — \
         counts must be pulled from the swapped handle snapshot (AC11 honesty)"
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

// ─── D5 (AI-12.3, DF-CR-14-3c-5): the diverge toggle (d/s) is zero-dispatch ────
// `ac3_diverge_view_render_does_not_dispatch` only proves the RENDER is a pure fn
// (it cannot dispatch — no orchestrator in scope). The real AC3 claim is that the
// d/s INPUT arms reuse handles and never re-fork. This guards exactly that: the
// OpenDivergeView + CloseDivergeView arms only flip `wave_diverge_open`; they must
// contain no dispatch. Positive control: the SpawnGateConfirm arm DOES launch — so
// the scanner provably detects a dispatch when one exists. A mutant that makes
// opening the diverge view re-launch the wave → RED.
#[test]
fn diverge_toggle_arms_never_dispatch() {
    let event_loop = include_str!("../src/infrastructure/runtime/event_loop.rs");
    let toggle_arms = event_loop
        .split("InputAction::OpenDivergeView =>")
        .nth(1)
        .expect("OpenDivergeView arm must exist");
    // Covers both OpenDivergeView and CloseDivergeView, bounded by the next arm.
    let toggle_arms = toggle_arms
        .split("InputAction::DismissWave =>")
        .next()
        .expect("toggle arms must be bounded by the next InputAction arm");

    for dispatch in [
        "launch_wave_request(",
        "orchestrator",
        "rerun_spoke",
        "run_wave",
    ] {
        assert!(
            !toggle_arms.contains(dispatch),
            "the diverge toggle arms must never dispatch — found `{dispatch}`"
        );
    }

    // Positive control: a real dispatch IS detectable by this scan.
    let confirm_arm = event_loop
        .split("InputAction::SpawnGateConfirm =>")
        .nth(1)
        .expect("SpawnGateConfirm arm must exist");
    let confirm_arm = confirm_arm
        .split("InputAction::SpawnGateCap =>")
        .next()
        .expect("SpawnGateConfirm arm must be bounded");
    assert!(
        confirm_arm.contains("launch_wave_request("),
        "positive control: SpawnGateConfirm must dispatch — else the scan is blind"
    );
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
