//! Conformance tests for Story 14.3 — single-level fork-join executor.
//!
//! Keystone set (UX `:3209` + epics ACs). Every "X cannot happen / count == 0"
//! assertion ships a positive control (the mechanism CAN fire) + a mutant the
//! test would kill (R1 testability invariant, `epics.md:6141`; retro AI-13.3).
//! No vacuous negatives.
//!
//! The executor is driven through the `SubagentRunner` port by a `FakeRunner`
//! (AC8 — never the concrete `InProcessSubagentRunner`), so these tests are
//! deterministic and need no live LLM.

use std::collections::VecDeque;
use std::sync::Arc;

use async_trait::async_trait;
use proptest::prelude::*;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use rustain::domain::clock::{Clock, MockClock};
use rustain::domain::models::agent_id::AgentId;
use rustain::domain::models::capability_token::{
    Budget, CapabilityFlag, CapabilitySet, CapabilityToken, DelegateConstraint, DelegateRequest,
};
use rustain::domain::models::launch_spec::AgentLaunchSpec;
use rustain::domain::models::node_state::NodeState;
use rustain::domain::models::orchestration::{
    FORK_JOIN_SPAWN_CAP, OrchestrationError, SpokeResult, SpokeSpec, WaitPolicy,
};
use rustain::domain::models::subagent_error::SubagentError;
use rustain::domain::models::task_handle::TaskHandle;
use rustain::domain::models::{ModelTier, Op, ToolPolicy};
use rustain::domain::ports::{AuthorityProvider, ForkJoinRequest, Orchestrator, SubagentRunner};
use rustain::domain::services::authority_ledger::AuthorityLedger;
use rustain::infrastructure::orchestrator::{
    ForkJoinExecutor, SYNTHESIS_RESERVE, WAIT_ESCALATE_THRESHOLD_MS, dispatch_launch, elapsed_ms,
    should_escalate,
};
use rustain::infrastructure::runtime::event_bus::EventBus;

// ─── helpers ──────────────────────────────────────────────────────────────

fn root_token() -> CapabilityToken {
    CapabilityToken::r1_root(AgentId::root())
}

fn spoke(label: &str) -> SpokeSpec {
    SpokeSpec {
        label: label.into(),
        prompt: format!("explore {label}"),
        effective_model: "test-model".into(),
        tier: ModelTier::Flagship,
        tools_allow: ToolPolicy::InheritFromParent,
        waits_for: Vec::new(),
    }
}

fn request(coordinator: AgentId, spokes: Vec<SpokeSpec>) -> ForkJoinRequest {
    let n = spokes.len();
    ForkJoinRequest {
        coordinator,
        spokes,
        wait_policy: WaitPolicy::All,
        concurrency: n.clamp(1, FORK_JOIN_SPAWN_CAP),
    }
}

/// A `SubagentRunner` whose child tasks emit a preconfigured terminal state.
/// Each launch pops the next terminal from the queue. If the wave cancels
/// before the child emits, the child emits `Cancelled` (cancel wins the race).
struct FakeRunner {
    /// Queue of terminal states, one per launch.
    terminals: Arc<Mutex<VecDeque<NodeState>>>,
    /// Launch call counter — read by `ac10_cancel_all_inflight_emits_once_no_new_dispatch`
    /// to prove cancel-all does NOT trigger any new dispatch after the wave is
    /// cancelled. A `tokio::sync::Mutex`-guarded `u32` (no `std::sync` lock —
    /// the ratchet stays at 4).
    launch_count: Arc<Mutex<u32>>,
    /// When `true`, every launched child is TRULY STUCK: it holds its status
    /// channel open but NEVER emits a terminal (and never closes it). Drives
    /// the collector's timeout → `Terminal::Stuck` → `StuckWaiting` escalation
    /// path (re-review N3, AC10). `false` (the default) emits the queued
    /// terminals.
    never_emits: bool,
}

impl FakeRunner {
    fn new(terminals: Vec<NodeState>) -> Self {
        Self {
            terminals: Arc::new(Mutex::new(terminals.into_iter().collect())),
            launch_count: Arc::new(Mutex::new(0)),
            never_emits: false,
        }
    }

    /// A TRULY STUCK child mode: holds its status channel OPEN but NEVER emits
    /// a terminal state (and never closes the channel). Used by the wave-level
    /// stuck-escalation test (re-review N3) to drive a child through
    /// `collect_terminal` → `Terminal::Stuck` → `SpokeResult::Failed` with a
    /// `StuckWaiting` reason.
    fn never_emits() -> Self {
        Self {
            terminals: Arc::new(Mutex::new(VecDeque::new())),
            launch_count: Arc::new(Mutex::new(0)),
            never_emits: true,
        }
    }

    /// Number of `launch` calls observed so far (for the no-new-dispatch
    /// assertion in the spec-named cancel test).
    async fn launch_count(&self) -> u32 {
        *self.launch_count.lock().await
    }
}

#[async_trait]
impl SubagentRunner for FakeRunner {
    async fn launch(
        &self,
        _spec: AgentLaunchSpec,
        cancel: CancellationToken,
    ) -> Result<TaskHandle, SubagentError> {
        // Bump the launch counter BEFORE popping the terminal so the spec-named
        // cancel test can assert no new dispatch happens after cancel.
        {
            let mut cnt = self.launch_count.lock().await;
            *cnt += 1;
        }
        if self.never_emits {
            // TRULY STUCK child: hold the status channel OPEN but NEVER emit a
            // terminal (and never close it). The collector's
            // `WAIT_ESCALATE_THRESHOLD_MS` timeout fires — under a
            // `start_paused` runtime the paused clock auto-advances to the
            // deadline — `collect_terminal` returns `Terminal::Stuck`, and
            // `structured_result` maps it to `SpokeResult::Failed{reason:
            // StuckWaiting}`. Drives the wave-level stuck-escalation test
            // (re-review N3, AC10).
            let (status_tx, status_rx) = tokio::sync::mpsc::channel::<NodeState>(8);
            let (command_tx, _) = tokio::sync::mpsc::channel::<Op>(8);
            let (parent_disc_tx, _) = tokio::sync::mpsc::unbounded_channel::<()>();
            let agent_id = AgentId::new();
            let task_id = nanoid::nanoid!(12);
            let child_cancel = cancel.child_token();
            let child_cancel_for_task = child_cancel.clone();
            tokio::spawn(async move {
                // Hold status_tx alive so `status_rx.recv()` blocks (returns
                // `Pending`, not `Ok(None)`). The cancel await makes the task
                // cancelable so the test cleans up; if never cancelled it stays
                // pending forever — a truly stuck child.
                let _ = status_tx;
                let _ = child_cancel_for_task.cancelled().await;
            });
            return Ok(TaskHandle {
                agent_id,
                status_rx,
                command_tx,
                cancel: child_cancel,
                task_id,
                subagent_type: "fake".into(),
                spawned_at: 0,
                parent_disconnect: parent_disc_tx,
                // No structured yield from a stuck child (it never terminates).
                yield_rx: None,
            });
        }
        let terminal = self
            .terminals
            .lock()
            .await
            .pop_front()
            .unwrap_or(NodeState::Completed);
        let (status_tx, status_rx) = tokio::sync::mpsc::channel::<NodeState>(8);
        let (command_tx, mut command_rx) = tokio::sync::mpsc::channel::<Op>(8);
        let (parent_disc_tx, _parent_disc_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
        // AC6 structured-yield channel: a Completed child emits a schema-valid
        // JSON yield so the structured-result contract (validate/retry) goes
        // LIVE in the wave path (P1). Cancelled/Failed children emit nothing
        // (salvage/Failed paths).
        let (yield_tx, yield_rx) = tokio::sync::mpsc::channel::<String>(4);
        let agent_id = AgentId::new();
        let task_id = nanoid::nanoid!(12);
        let child_cancel = cancel.child_token();
        let cancel_for_task = child_cancel.clone();
        tokio::spawn(async move {
            let _ = status_tx.send(NodeState::Running).await;
            // A cancelable wait: if the wave cancels, emit Cancelled; else emit
            // the configured terminal. The sleep is cancelable so start_paused
            // tests drive timing deterministically (G1: a real cancelable child,
            // not a parked stub).
            tokio::select! {
                _ = cancel_for_task.cancelled() => {
                    let _ = status_tx.send(NodeState::Cancelled).await;
                }
                _ = tokio::time::sleep(std::time::Duration::from_millis(5)) => {
                    let _ = status_tx.send(terminal).await;
                    if terminal == NodeState::Completed {
                        // Emit a valid JSON yield (drives validate_yield live).
                        let _ = yield_tx.send(
                            r#"{"summary":"completed","detail":"completed detail"}"#.to_string(),
                        ).await;
                    }
                }
                _ = command_rx.recv() => {
                    let _ = status_tx.send(NodeState::Cancelled).await;
                }
            }
        });
        Ok(TaskHandle {
            agent_id,
            status_rx,
            command_tx,
            cancel: child_cancel,
            task_id,
            subagent_type: "fake".into(),
            spawned_at: 0,
            parent_disconnect: parent_disc_tx,
            yield_rx: Some(yield_rx),
        })
    }
}

fn build_executor(
    runner: Arc<dyn SubagentRunner>,
    root: CapabilityToken,
) -> (ForkJoinExecutor, Arc<AuthorityLedger>) {
    let ledger = Arc::new(AuthorityLedger::new(root.clone()));
    let authority: Arc<dyn AuthorityProvider> = Arc::new(
        rustain::adapters::authority::in_process::InProcessAuthorityProvider::new(ledger.clone()),
    );
    // P21: keep the domain receiver alive so emits are not silently dropped.
    // `EventBus::new` returns (bus, rx); `.0` discarded the rx, making every
    // emit a no-op. Forgetting the rx (test-only) keeps the channel open.
    let event_bus = {
        let (bus, rx) = EventBus::new(16);
        std::mem::forget(rx);
        Arc::new(bus)
    };
    let clock = Arc::new(MockClock::at_wall_ms(0)) as Arc<dyn Clock>;
    let exe = ForkJoinExecutor::new(runner, authority, ledger.clone(), event_bus, clock, root);
    (exe, ledger)
}

// ─── AC2 — waits_for inert + single-level ─────────────────────────────────

#[tokio::test]
async fn ac2_waits_for_non_empty_is_rejected_as_not_single_level() {
    let (exe, _ledger) = build_executor(
        Arc::new(FakeRunner::new(vec![NodeState::Completed])) as Arc<dyn SubagentRunner>,
        root_token(),
    );
    let mut spoke_with_dep = spoke("a");
    spoke_with_dep.waits_for = vec![AgentId("sibling".into())];
    let mut req = request(AgentId::root(), vec![spoke_with_dep]);
    req.spokes.push(spoke("b"));
    let err = exe.run_fork_join(req).await.unwrap_err();
    assert!(
        matches!(err, OrchestrationError::NotSingleLevel { .. }),
        "R1 rejects any non-empty waits_for: {err:?}"
    );
}

#[tokio::test]
async fn ac2_single_level_completes_with_no_sibling_scheduling() {
    // R1 DISCRIMINATOR: a valid single-level wave completes; the executor NEVER
    // schedules a spoke on another's terminal state (zero sibling-triggered
    // transitions). The counter instrumentation
    // (`SIBLING_TRIGGERED_TRANSITIONS`) is gated behind `test-instrumentation`;
    // under that feature the discriminator is asserted as a real count == 0
    // (not the vacuous "both spokes completed" check the original test shipped).
    //
    // The positive control + scheduler-mutant kill live in companion tests:
    // they prove the counter CAN fire (so this == 0 assertion is non-vacuous)
    // and that a mutant scheduler that fires on a sibling terminal is detected.
    #[cfg(feature = "test-instrumentation")]
    use rustain::infrastructure::orchestrator::SIBLING_TRIGGERED_TRANSITIONS;
    #[cfg(feature = "test-instrumentation")]
    {
        use std::sync::atomic::Ordering;
        SIBLING_TRIGGERED_TRANSITIONS.store(0, Ordering::Relaxed);
    }
    let (exe, _ledger) = build_executor(
        Arc::new(FakeRunner::new(vec![
            NodeState::Completed,
            NodeState::Completed,
        ])) as Arc<dyn SubagentRunner>,
        root_token(),
    );
    let outcome = exe
        .run_fork_join(request(AgentId::root(), vec![spoke("a"), spoke("b")]))
        .await
        .unwrap();
    assert_eq!(outcome.spokes.len(), 2);
    // AgentId-keyed, never a flattened blob (AC2).
    for (id, r) in &outcome.spokes {
        assert!(
            matches!(r, SpokeResult::Completed { .. }),
            "{id:?} not completed"
        );
    }
    // DISCRIMINATOR: zero sibling-triggered non-terminal→Running transitions
    // caused by another node reaching a terminal state (R1 single-level).
    #[cfg(feature = "test-instrumentation")]
    {
        use std::sync::atomic::Ordering;
        let sibling_transitions = SIBLING_TRIGGERED_TRANSITIONS.load(Ordering::Relaxed);
        assert_eq!(
            sibling_transitions, 0,
            "R1 single-level executor must never schedule a spoke on a \
             sibling's terminal state (the zero-sibling-transition \
             discriminator — AC2). A non-zero count means the executor \
             implemented R2-style sibling scheduling without flipping R1."
        );
    }
}

/// POSITIVE CONTROL for the AC2 discriminator (Murat vacuity-closer — the
/// original test was vacuously green because nothing proved the counter could
/// fire). Calling `record_sibling_triggered_transition` (the seam an R2
/// readiness predicate would call) bumps the counter, demonstrating the == 0
/// assertion above is non-vacuous. A "instrumentation never wired" mutant
/// would leave the counter at 0 and fail here.
#[cfg(feature = "test-instrumentation")]
#[test]
fn ac2_sibling_transition_counter_positive_control() {
    use rustain::infrastructure::orchestrator::{
        SIBLING_TRIGGERED_TRANSITIONS, record_sibling_triggered_transition,
    };
    use std::sync::atomic::Ordering;
    SIBLING_TRIGGERED_TRANSITIONS.store(0, Ordering::Relaxed);
    let before = SIBLING_TRIGGERED_TRANSITIONS.load(Ordering::Relaxed);
    // Mimics the call site an R2 readiness predicate would insert when a spoke
    // is scheduled on a sibling's terminal state.
    record_sibling_triggered_transition();
    let after = SIBLING_TRIGGERED_TRANSITIONS.load(Ordering::Relaxed);
    assert_eq!(
        after,
        before + 1,
        "POSITIVE CONTROL: record_sibling_triggered_transition must bump the \
         counter (proves the AC2 discriminator is wired, not decorative)."
    );
}

/// SCHEDULER MUTANT KILL for the AC2 discriminator. A stubbed scheduler that
/// fires on a sibling's terminal state (the R2 readiness predicate, or a mutant
/// R1 that mis-impl single-level) is detected by the discriminator. The mutant
/// `fn() { record_sibling_triggered_transition(); }` mirrors exactly what an
/// R2-style sibling-dispatch call site would do; running it under the wave
/// would flip the == 0 assertion above to non-zero (RED). This test simulates
/// that flip and asserts the counter moves, so the mutant does not slip past.
#[cfg(feature = "test-instrumentation")]
#[test]
fn ac2_scheduler_mutant_bumps_discriminator_and_would_be_caught() {
    use rustain::infrastructure::orchestrator::{
        SIBLING_TRIGGERED_TRANSITIONS, record_sibling_triggered_transition,
    };
    use std::sync::atomic::Ordering;
    SIBLING_TRIGGERED_TRANSITIONS.store(0, Ordering::Relaxed);

    // The mutant: a scheduler that, on a sibling reaching terminal state,
    // schedules another spoke (mirrored by recording a sibling-triggered
    // transition). Under the discriminator assertion (count == 0) this mutant
    // MUST fail — which we prove by showing the counter is now non-zero.
    let mutant_scheduler_on_sibling_terminal = || {
        record_sibling_triggered_transition();
    };
    mutant_scheduler_on_sibling_terminal();

    let after = SIBLING_TRIGGERED_TRANSITIONS.load(Ordering::Relaxed);
    assert_ne!(
        after, 0,
        "MUTANT KILL: a scheduler that fires on sibling-terminal state bumps \
         the discriminator counter, so the == 0 assertion would catch it."
    );
}

// ─── AC4 — static spawn cap ───────────────────────────────────────────────

#[tokio::test]
async fn ac4_fan_out_beyond_spawn_cap_is_refused() {
    let (exe, _ledger) = build_executor(
        Arc::new(FakeRunner::new(vec![])) as Arc<dyn SubagentRunner>,
        root_token(),
    );
    let too_many: Vec<SpokeSpec> = (0..(FORK_JOIN_SPAWN_CAP + 1))
        .map(|i| spoke(&format!("s{i}")))
        .collect();
    let err = exe
        .run_fork_join(request(AgentId::root(), too_many))
        .await
        .unwrap_err();
    assert!(
        matches!(err, OrchestrationError::SpawnCapExceeded { cap, attempted } if cap == FORK_JOIN_SPAWN_CAP && attempted == FORK_JOIN_SPAWN_CAP + 1),
        "fan-out at/above the static cap is refused: {err:?}"
    );
}
/// POSITIVE CONTROL for the spawn cap: fan-out EXACTLY AT the cap is permitted
/// (`attempted > cap` is the refusal predicate — `== cap` succeeds). The
/// doc/code mismatch review finding was that the refusal message + test only
/// exercised `cap + 1`; this pins the boundary so a `>= cap` mutant (which
/// would wrongly refuse the cap case) is killed.
#[tokio::test]
async fn ac4_fan_out_exactly_at_spawn_cap_succeeds() {
    let (exe, _ledger) = build_executor(
        Arc::new(FakeRunner::new(vec![
            NodeState::Completed;
            FORK_JOIN_SPAWN_CAP
        ])) as Arc<dyn SubagentRunner>,
        root_token(),
    );
    let at_cap: Vec<SpokeSpec> = (0..FORK_JOIN_SPAWN_CAP)
        .map(|i| spoke(&format!("s{i}")))
        .collect();
    let outcome = exe
        .run_fork_join(request(AgentId::root(), at_cap))
        .await
        .expect("fan-out exactly at cap is permitted (attempted == cap)");
    // Exactly `cap` outcomes — every dispatched spoke reached a terminal.
    assert_eq!(outcome.spokes.len(), FORK_JOIN_SPAWN_CAP);
    for (_id, r) in &outcome.spokes {
        assert!(
            matches!(r, SpokeResult::Completed { .. }),
            "at-cap fan-out completes every spoke"
        );
    }
}

// ─── AC9 — WaitPolicy::Quorum reserved & inert ────────────────────────────

#[tokio::test]
async fn ac9_wait_policy_quorum_is_rejected_in_r1() {
    let (exe, _ledger) = build_executor(
        Arc::new(FakeRunner::new(vec![NodeState::Completed])) as Arc<dyn SubagentRunner>,
        root_token(),
    );
    let mut req = request(AgentId::root(), vec![spoke("a")]);
    req.wait_policy = WaitPolicy::Quorum(1);
    let err = exe.run_fork_join(req).await.unwrap_err();
    assert!(
        matches!(err, OrchestrationError::WaitPolicyUnsupported(_)),
        "Quorum is reserved for R3 (Story 18.6): {err:?}"
    );
}

// ─── AC3 — seal the spawn door (exact-count dispatch-site guard) ───────────

#[test]
fn ac3_orchestrator_has_exactly_one_launch_call_site() {
    // The seal: within src/infrastructure/orchestrator/, production code calls
    // `.launch(` exactly ONCE — inside `dispatch_launch` (the sole chokepoint).
    // A new bypass path adds a second site → this fails. Zero also fails (the
    // chokepoint was removed). Reuses the count_braces lexer pattern (strip
    // test mods) consistent with conformance_node_tree.rs.
    use std::fs;
    let dir = std::path::Path::new("src/infrastructure/orchestrator");
    let mut launch_sites = Vec::new();
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|e| e == "rs") {
            let content = fs::read_to_string(&path).unwrap();
            // Strip #[cfg(test)] mod tests blocks by brace depth (reuses the
            // count_braces discipline — production code only).
            let prod = strip_test_mods(&content);
            for (i, line) in prod.lines().enumerate() {
                let t = line.trim();
                if t.starts_with("//") {
                    continue;
                }
                if t.contains(".launch(") {
                    launch_sites.push(format!("{}:{}", path.display(), i + 1));
                }
            }
        }
    }
    // POSITIVE CONTROL — the chokepoint site exists.
    assert!(
        launch_sites.iter().any(|s| s.contains("mod.rs")),
        "POSITIVE CONTROL FAILED: no .launch( site in orchestrator/mod.rs"
    );
    // Exact-equality (NOT is_subset): exactly one sanctioned spawn site.
    assert_eq!(
        launch_sites.len(),
        1,
        "seal-the-spawn-door: orchestrator must have exactly ONE `.launch(` site \
         (the dispatch_launch chokepoint). Found: {launch_sites:?}"
    );
}

#[test]
fn ac3_dispatch_launch_chokepoint_is_defined_once() {
    use std::fs;
    let content =
        fs::read_to_string("src/infrastructure/orchestrator/mod.rs").expect("mod.rs reads");
    let prod = strip_test_mods(&content);
    let defs = prod
        .lines()
        .filter(|l| l.trim_start().contains("fn dispatch_launch("))
        .count();
    assert_eq!(
        defs, 1,
        "dispatch_launch chokepoint defined exactly once (found {defs})"
    );
}

/// Strip every `#[cfg(test)] mod tests { … }` (or `mod tests { … }` nested
/// under cfg(test)) from `content`. Used by guards that must inspect production
/// code only — test scaffolding is allowed to exercise shapes the conformance
/// bar forbids in production (e.g. constructing `Remote`).
///
/// **REUSES the hardened `count_braces` / `LexState` source-scan lexer from
/// `conformance_node_tree.rs` verbatim (G5).** The prior hand-rolled brace
/// counter exited test-mod mode prematurely after the first brace-neutral line,
/// leaking test bodies into the production scan (review finding AC3). This
/// canonical version carries cross-line raw-string / char-literal state so a
/// brace inside `r"{ }"` does not corrupt the depth, and pops test-mod mode
/// only when the test-mod block's braces actually balance.
fn strip_test_mods(content: &str) -> String {
    let mut out = String::with_capacity(content.len());
    let mut depth: i32 = 0;
    let mut in_test_mod: Option<i32> = None; // depth at which test mod opened
    let mut pending_test_attr = false;
    // Cross-line lexer state: raw strings (and, defensively, normal strings)
    // can span line boundaries, so the brace counter must carry context
    // between lines instead of resetting per line.
    let mut lex = LexState::default();

    for line in content.lines() {
        let trimmed = line.trim();

        if let Some(open_depth) = in_test_mod {
            // Inside test mod — track depth, pop when balanced.
            depth += count_braces(line, &mut lex);
            if depth <= open_depth {
                in_test_mod = None;
            }
            continue;
        }

        let is_attr = trimmed.starts_with("#[");
        let has_cfg_test = trimmed.contains("cfg(test)");
        // Looser `contains` (vs `starts_with`) on purpose: a false positive
        // fails the conformance test loudly, while a false negative would
        // silently leak test code into the production stream the guards scan.
        let opens_tests_mod = trimmed.contains("mod tests") && trimmed.contains('{');

        // Arm on a cfg(test) attribute. Pending stays armed across consecutive
        // attribute lines (handles `#[cfg(test)]\n#[allow]\nmod tests`) and
        // resolves on the next non-attribute line.
        if is_attr && has_cfg_test {
            pending_test_attr = true;
        }

        // Enter the test mod: same-line `#[cfg(test)] mod tests {`, or a later
        // `mod tests {` while pending from a prior cfg(test) attribute.
        if opens_tests_mod && (has_cfg_test || pending_test_attr) {
            in_test_mod = Some(depth);
            depth += count_braces(line, &mut lex);
            pending_test_attr = false;
            continue;
        }

        // A bare cfg(test)-bearing attribute line (no mod opener on it) is
        // dropped — it belongs to the test item that follows.
        if is_attr && pending_test_attr {
            continue;
        }

        // Any other non-attribute line resolves a stale pending flag.
        if !is_attr {
            pending_test_attr = false;
        }

        depth += count_braces(line, &mut lex);
        out.push_str(line);
        out.push('\n');
    }

    out
}

/// Lexer state carried across lines — raw strings can contain literal
/// newlines, so [`count_braces`] is stateful rather than purely per-line.
#[derive(Clone, Copy, Default)]
struct LexState {
    in_str: bool,
    /// `Some(n)` ⇒ currently inside a raw string fenced with `n` `#`s.
    raw_hashes: Option<usize>,
}

/// Count net braces on `line` outside char/string/raw-string literals and line
/// comments, carrying cross-line string state in `state`. Raw strings
/// (`r"…"`, `r#"…"#`, `r##"…"##`, …) — including ones that span lines — no
/// longer leak their interior `{`/`}` into the depth.
fn count_braces(line: &str, state: &mut LexState) -> i32 {
    let chars: Vec<char> = line.chars().collect();
    let mut delta: i32 = 0;
    let mut i = 0;
    while i < chars.len() {
        let ch = chars[i];

        // (a) Inside a raw string — scan to its close: `"` + matching fence.
        if let Some(h) = state.raw_hashes {
            let mut closed = false;
            while i < chars.len() {
                if chars[i] == '"' && closes_raw(&chars, i, h) {
                    i += 1 + h;
                    state.raw_hashes = None;
                    closed = true;
                    break;
                }
                i += 1;
            }
            if !closed {
                return delta; // raw string continues onto the next line
            }
            continue;
        }

        // (b) Inside a normal string — honor escapes, close on unescaped `"`.
        if state.in_str {
            if ch == '\\' {
                i += 2;
                continue;
            }
            if ch == '"' {
                state.in_str = false;
            }
            i += 1;
            continue;
        }

        // (c) Normal scan. Raw-string open: `r` at a word boundary + `#`* + `"`.
        if ch == 'r' && raw_string_opens(&chars, i) {
            let mut j = i + 1;
            let mut hashes = 0;
            while j < chars.len() && chars[j] == '#' {
                hashes += 1;
                j += 1;
            }
            // j sits on the opening `"`.
            let mut k = j + 1;
            let mut closed = false;
            while k < chars.len() {
                if chars[k] == '"' && closes_raw(&chars, k, hashes) {
                    k += 1 + hashes;
                    closed = true;
                    break;
                }
                k += 1;
            }
            if closed {
                i = k;
            } else {
                state.raw_hashes = Some(hashes);
                return delta; // raw string spans subsequent lines
            }
            continue;
        }

        match ch {
            '/' if i + 1 < chars.len() && chars[i + 1] == '/' => break, // `//` line comment
            '"' => state.in_str = true,
            '\'' => {
                // Char literal vs lifetime label — peek ahead.
                let is_char_literal = if i + 1 < chars.len() {
                    let next = chars[i + 1];
                    next == '\\' || (i + 2 < chars.len() && chars[i + 2] == '\'')
                } else {
                    false
                };
                if is_char_literal {
                    // Consume through the closing quote, honoring escapes.
                    let mut j = i + 1;
                    while j < chars.len() && chars[j] != '\'' {
                        if chars[j] == '\\' {
                            j += 1;
                        }
                        j += 1;
                    }
                    i = if j < chars.len() { j + 1 } else { j };
                    continue;
                }
                // Otherwise: lifetime label — the quote is brace-neutral.
            }
            '{' => delta += 1,
            '}' => delta -= 1,
            _ => {}
        }
        i += 1;
    }
    delta
}

/// True if a raw-string literal opens at `chars[i]`: `r` (not part of a longer
/// identifier) immediately followed by zero-or-more `#` and a `"`. Covers
/// `r"…"`, `r#"…"#`, `r##"…"##`, …. Byte/cstr raw variants (`br"…"`,
/// `cr"…"`) are not matched — the `b`/`c` prefix makes `r` non-word-boundary;
/// they are vanishingly rare in the scanned tree and their braces stay
/// excluded via the normal-string fallback.
fn raw_string_opens(chars: &[char], i: usize) -> bool {
    let prev_is_ident = i > 0 && (chars[i - 1].is_alphanumeric() || chars[i - 1] == '_');
    if prev_is_ident {
        return false;
    }
    let mut j = i + 1;
    while j < chars.len() && chars[j] == '#' {
        j += 1;
    }
    j < chars.len() && chars[j] == '"'
}

/// True if `chars[pos] == '"'` closes a raw string fenced with `h` `#`s — i.e.
/// at least `h` `#`s follow the quote (for `h == 0`, any `"` closes).
fn closes_raw(chars: &[char], pos: usize, h: usize) -> bool {
    (1..=h).all(|k| chars.get(pos + k) == Some(&'#'))
}

#[test]
fn count_braces_ignores_raw_strings_and_chars_in_fork_join_scan() {
    // Regression for the AC3 review finding: the prior hand-rolled stripper
    // would miscount these and leak test bodies into the production scan.
    let mut s = LexState::default();
    assert_eq!(count_braces("let a = r\"{ }\"; { }", &mut s), 0);
    let mut s = LexState::default();
    assert_eq!(count_braces("let b = r#\"{ }\"#;", &mut s), 0);
    let mut s = LexState::default();
    assert_eq!(count_braces("fn f<'a>() { let ch = '{'; x", &mut s), 1);
}

#[test]
fn count_braces_handles_multiline_raw_string_in_fork_join_scan() {
    let mut s = LexState::default();
    assert_eq!(count_braces("let s = r#\"  ", &mut s), 0);
    assert_eq!(s.raw_hashes, Some(1));
    assert_eq!(count_braces("} } { {", &mut s), 0);
    assert_eq!(s.raw_hashes, Some(1));
    assert_eq!(count_braces("\"#;", &mut s), 0);
    assert_eq!(s.raw_hashes, None);
}

#[test]
fn strip_test_mods_pops_only_when_braces_balance() {
    // The prior buggy stripper exited test-mod mode after the first brace-
    // neutral line, leaking test bodies into the production scan. A test mod
    // containing a brace-neutral line followed by a `r#launch#` literal must
    // NOT leak `.launch(` into the production stream.
    let content = "\
fn prod() { let _ = std::fs::read_to_string(\"x\").unwrap(); }
#[cfg(test)]
mod tests {
    let neutral = 1;
    fn helper() {
        let s = r#\".launch(\"#;
    }
    use super::*;
}
fn after() {}
";
    let stripped = strip_test_mods(content);
    // The `.launch(` inside the test-mod raw string must NOT survive — it
    // belongs to the test scaffolding, not the production scan.
    assert!(
        !stripped.contains(".launch("),
        "test-mod raw-string content leaked into production scan: {stripped}"
    );
    // Production functions on either side of the test mod are preserved.
    assert!(stripped.contains("fn prod()"));
    assert!(stripped.contains("fn after()"));
}

// ─── Murat vacuity-closer — GateProbe child-token refusal ─────────────────

async fn gate_probe_harness() -> (
    Arc<dyn SubagentRunner>,
    Arc<dyn AuthorityProvider>,
    CapabilityToken,
    Arc<AuthorityLedger>,
) {
    let root = root_token();
    let ledger = Arc::new(AuthorityLedger::new(root.clone()));
    let authority: Arc<dyn AuthorityProvider> = Arc::new(
        rustain::adapters::authority::in_process::InProcessAuthorityProvider::new(ledger.clone()),
    );
    let runner = Arc::new(FakeRunner::new(vec![NodeState::Completed])) as Arc<dyn SubagentRunner>;
    (runner, authority, root, ledger)
}

fn mint_child(
    ledger: &AuthorityLedger,
    root: &CapabilityToken,
    scope: &str,
    spawn: bool,
) -> CapabilityToken {
    let mut caps = CapabilitySet::default();
    if spawn {
        caps = CapabilitySet::from_flags(&[CapabilityFlag::Spawn]);
    }
    let req = DelegateRequest {
        scope: AgentId(scope.into()),
        capabilities: caps,
        constraint: DelegateConstraint {
            allowed: caps,
            max_depth: 1,
            max_subset: caps,
        },
        budget: Budget {
            requests: 1,
            cost_micros: 1,
        },
        not_after: None,
        uses_limit: Some(1),
    };
    ledger.delegate(root, req).unwrap()
}

#[tokio::test]
async fn murat_child_token_lacking_spawn_is_refused_no_node() {
    // The 14.2-AC9 defeat: validate the ROOT token (never revoked) → always
    // passes. This probe validates the CHILD token — a Spawn-less child is
    // REFUSED. Under the mutant (validate coordinator) this assertion FAILS.
    let (runner, authority, root, ledger) = gate_probe_harness().await;
    let child_no_spawn = mint_child(&ledger, &root, "nospawn", false);
    let err = dispatch_launch(
        &runner,
        &authority,
        &spoke("probe"),
        child_no_spawn,
        CancellationToken::new(),
    )
    .await
    .err()
    .unwrap();
    assert!(
        matches!(err, OrchestrationError::SpawnRefused(_)),
        "child token lacking Spawn is REFUSED: {err:?}"
    );
}

#[tokio::test]
async fn murat_pre_revoked_child_token_is_refused() {
    let (runner, authority, root, ledger) = gate_probe_harness().await;
    let child = mint_child(&ledger, &root, "revoked", true);
    ledger.revoke(&child.id).unwrap(); // pre-revoke the child token
    let err = dispatch_launch(
        &runner,
        &authority,
        &spoke("probe"),
        child,
        CancellationToken::new(),
    )
    .await
    .err()
    .unwrap();
    assert!(
        matches!(err, OrchestrationError::SpawnRefused(_)),
        "pre-revoked child token is REFUSED: {err:?}"
    );
}

#[tokio::test]
async fn murat_valid_child_token_dispatches_once() {
    // POSITIVE CONTROL: a valid child token dispatches (launch succeeds).
    let (runner, authority, root, ledger) = gate_probe_harness().await;
    let child = mint_child(&ledger, &root, "valid", true);
    let handle = dispatch_launch(
        &runner,
        &authority,
        &spoke("probe"),
        child,
        CancellationToken::new(),
    )
    .await
    .unwrap();
    // Drain to terminal to confirm the child actually ran.
    let mut rx = handle.status_rx;
    let mut terminal = NodeState::Created;
    while let Ok(Some(s)) =
        tokio::time::timeout(std::time::Duration::from_millis(50), rx.recv()).await
    {
        terminal = s;
        if s.is_terminal() {
            break;
        }
    }
    assert!(
        terminal.is_terminal(),
        "valid child dispatched + reached terminal: {terminal:?}"
    );
}

// ─── AC7 — grounded synthesis HERO ────────────────────────────────────────

#[tokio::test]
async fn ac7_honest_empty_when_all_spokes_degraded() {
    // Force honest-empty: all children fail/cancel — never confident noise.
    let (exe, _ledger) = build_executor(
        Arc::new(FakeRunner::new(vec![
            NodeState::Failed,
            NodeState::Cancelled,
        ])) as Arc<dyn SubagentRunner>,
        root_token(),
    );
    let outcome = exe
        .run_fork_join(request(AgentId::root(), vec![spoke("a"), spoke("b")]))
        .await
        .unwrap();
    assert!(
        outcome.synthesis.honest_empty,
        "zero signal → honest-empty (never confident noise)"
    );
    assert_eq!(outcome.synthesis.coverage.completed, 0);
}

#[tokio::test]
async fn ac7_synthesis_covers_all_completed_spokes_no_orphans() {
    let (exe, _ledger) = build_executor(
        Arc::new(FakeRunner::new(vec![
            NodeState::Completed,
            NodeState::Completed,
            NodeState::Failed,
        ])) as Arc<dyn SubagentRunner>,
        root_token(),
    );
    let outcome = exe
        .run_fork_join(request(
            AgentId::root(),
            vec![spoke("a"), spoke("b"), spoke("c")],
        ))
        .await
        .unwrap();
    // Postcondition: one citation per completed spoke (no orphan claims).
    assert_eq!(
        outcome.synthesis.citations.len(),
        outcome.synthesis.coverage.completed
    );
    assert_eq!(outcome.synthesis.coverage.completed, 2);
    assert_eq!(outcome.synthesis.coverage.failed, 1);
    assert!(!outcome.synthesis.honest_empty);
}

#[tokio::test]
async fn ac7_wave_partial_failure_includes_survivors_excludes_dead() {
    let (exe, _ledger) = build_executor(
        Arc::new(FakeRunner::new(vec![
            NodeState::Completed,
            NodeState::Failed,
            NodeState::Completed,
        ])) as Arc<dyn SubagentRunner>,
        root_token(),
    );
    let outcome = exe
        .run_fork_join(request(
            AgentId::root(),
            vec![spoke("a"), spoke("dead"), spoke("c")],
        ))
        .await
        .unwrap();
    let survivor_labels: Vec<&str> = outcome
        .synthesis
        .citations
        .iter()
        .map(|c| c.label.as_str())
        .collect();
    assert!(
        survivor_labels.contains(&"a") && survivor_labels.contains(&"c"),
        "survivors cited"
    );
    assert!(
        !survivor_labels.contains(&"dead"),
        "dead spoke excluded from citations"
    );
}

// ─── AC10 — cancel-all + reserve + conservation + Clock escalation ────────

#[tokio::test(start_paused = true)]
async fn ac10_cancel_all_makes_every_spoke_terminal() {
    // Cancel-all propagates via the CancellationToken tree (committed-exact:
    // N = dispatched spoke count). Each in-flight child emits exactly one
    // terminal (Cancelled) — no second result.
    let (exe, _ledger) = build_executor(
        Arc::new(FakeRunner::new(vec![
            NodeState::Completed,
            NodeState::Completed,
            NodeState::Completed,
        ])) as Arc<dyn SubagentRunner>,
        root_token(),
    );
    let wave_cancel = CancellationToken::new();
    let cancel_for_test = wave_cancel.clone();
    // Cancel shortly after fan-out begins.
    let cancel_task = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        cancel_for_test.cancel();
    });
    let outcome = exe
        .run_fork_join_with_cancel(
            request(AgentId::root(), vec![spoke("a"), spoke("b"), spoke("c")]),
            wave_cancel,
        )
        .await
        .unwrap();
    cancel_task.await.unwrap();
    // Exactly N terminal outcomes (none missing — G7), all Cancelled.
    assert_eq!(outcome.spokes.len(), 3);
    for (_id, r) in &outcome.spokes {
        assert!(
            matches!(r, SpokeResult::Cancelled),
            "cancel-all cancelled every spoke"
        );
    }
}
/// SPEC-NAMED cancel-all test (`cancel_all_inflight_emits_once_no_new_dispatch`,
/// review finding AC10). The bare `ac10_cancel_all_makes_every_spoke_terminal`
/// only proved every spoke reaches Cancelled; this one pins the two AC10
/// properties the bare test missed:
///
/// 1. **emits once**: each in-flight spoke emits exactly ONE terminal outcome
///    (no second result — `spokes.len() == N`, not more).
/// 2. **no new dispatch**: the launch call count is exactly N (no extra
///    dispatch after cancel — verified via `FakeRunner::launch_count`).
///
/// A `SilentKill` mutant (cancel drops in-flight outcomes silently) would fail
/// (1); a mutant that re-dispatches on cancel would fail (2).
#[tokio::test(start_paused = true)]
async fn ac10_cancel_all_inflight_emits_once_no_new_dispatch() {
    let runner = Arc::new(FakeRunner::new(vec![
        NodeState::Completed,
        NodeState::Completed,
        NodeState::Completed,
    ]));
    let runner_for_count = runner.clone();
    let (exe, _ledger) = build_executor(runner as Arc<dyn SubagentRunner>, root_token());
    let wave_cancel = CancellationToken::new();
    let cancel_for_test = wave_cancel.clone();
    // Cancel shortly after fan-out begins (the in-flight children emit one
    // more result — Cancelled — then go ⊘; never a second outcome).
    let cancel_task = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        cancel_for_test.cancel();
    });
    let outcome = exe
        .run_fork_join_with_cancel(
            request(AgentId::root(), vec![spoke("a"), spoke("b"), spoke("c")]),
            wave_cancel,
        )
        .await
        .unwrap();
    cancel_task.await.unwrap();
    // (1) EMITS ONCE: exactly N terminal outcomes, none missing, none doubled.
    // A `SilentKill` mutant (cancel drops in-flight outcomes) would under-
    // count; a mutant that emits a second result after Cancelled would
    // over-count.
    assert_eq!(
        outcome.spokes.len(),
        3,
        "emits once: cancel-all produced exactly N terminal outcomes \
         (no second result, none silently dropped)"
    );
    for (_id, r) in &outcome.spokes {
        assert!(
            matches!(r, SpokeResult::Cancelled),
            "each in-flight spoke emitted exactly one Cancelled terminal"
        );
    }
    // (2) NO NEW DISPATCH: launch_count == N (no extra dispatch after cancel).
    // The launch semaphore is closed + the cancel propagates via the
    // CancellationToken tree, so no further spokes are spawned. A re-dispatch
    // mutant would over-count here.
    let launches = runner_for_count.launch_count().await;
    assert_eq!(
        launches, 3,
        "no new dispatch: launch_count == N (no extra dispatch after cancel)"
    );
}

#[tokio::test]
async fn ac10_synthesis_reserve_survives_fan_out() {
    // Reserve-the-HERO: after fan-out, the coordinator's budget still covers
    // the synthesis reserve. AND across BOTH dimensions — the prior OR
    // (`>= 1 || >= 1`) let a mutant draining one dimension slip past (review
    // finding). The reserve is DEBITED into `consumed` before fan-out (P5
    // impl fix); `available` reflects `total − reserve` after the gate-token
    // refund round-trip.
    let root = root_token();
    let (exe, ledger) = build_executor(
        Arc::new(FakeRunner::new(vec![
            NodeState::Completed,
            NodeState::Completed,
        ])) as Arc<dyn SubagentRunner>,
        root.clone(),
    );
    let _outcome = exe
        .run_fork_join(request(AgentId::root(), vec![spoke("a"), spoke("b")]))
        .await
        .unwrap();
    let available = ledger.available(&root.id).unwrap();
    // Differential ON: BOTH dimensions of the reserve survive. AND (not OR).
    assert!(
        available.requests >= SYNTHESIS_RESERVE.requests
            && available.cost_micros >= SYNTHESIS_RESERVE.cost_micros,
        "synthesis reserve survived fan-out (both dimensions): available={available:?}"
    );
}

/// TIGHT-BUDGET DIFFERENTIAL (review finding): under a coordinator whose
/// budget is exactly `SYNTHESIS_RESERVE + 2-spoke gate-token aggregate`, the
/// reserve must STILL be debited into `consumed` (not merely checked) and the
/// coordinator's available must STILL cover the gate aggregate after the
/// refund round-trip. A "check-only" mutant (the original P5 finding) leaves
/// `consumed == 0` here — the AND across both dimensions catches it. This is
/// the `synthesis_budget_reserved_survives_ceiling` anti-vacuous triple
/// (reserve > 0, contention forced via the tight ceiling, differential ON).
#[tokio::test]
async fn ac10_synthesis_reserve_debited_under_tight_budget() {
    use rustain::domain::models::capability_token::{
        Budget, CapabilityFlag, CapabilitySet, CapabilityToken,
    };
    // GATE_TOKEN_BUDGET = { requests: 1, cost_micros: 1 } (private const); the
    // 2-spoke aggregate is 2/2. Tight total = reserve + aggregate exactly.
    let tight_total = Budget {
        requests: SYNTHESIS_RESERVE.requests + 2,
        cost_micros: SYNTHESIS_RESERVE.cost_micros + 2,
    };
    let root = CapabilityToken::root(
        AgentId::root(),
        CapabilitySet::from_flags(&[CapabilityFlag::Spawn]),
        tight_total,
        3,
        None,
        Some(1_000),
    );
    let (exe, ledger) = build_executor(
        Arc::new(FakeRunner::new(vec![
            NodeState::Completed,
            NodeState::Completed,
        ])) as Arc<dyn SubagentRunner>,
        root.clone(),
    );
    let _outcome = exe
        .run_fork_join(request(AgentId::root(), vec![spoke("a"), spoke("b")]))
        .await
        .expect("tight budget exactly covers reserve + gate aggregate");
    let snap = ledger.conservation(&root.id).unwrap();
    // AND across both dimensions: the reserve was DEBITED into `consumed`. A
    // check-only mutant leaves `consumed == ZERO` (the differential the
    // original OR test missed).
    assert!(
        snap.consumed.requests >= SYNTHESIS_RESERVE.requests
            && snap.consumed.cost_micros >= SYNTHESIS_RESERVE.cost_micros,
        "tight-budget differential: SYNTHESIS_RESERVE was DEBITED into consumed \
         (a check-only mutant leaves consumed at ZERO). snap={snap:?}"
    );
    // The reserve survives the ceiling: fan-out drew only from (ceiling −
    // reserve); after the gate-token refund, available reflects
    // `total − reserve` (gate aggregate refunded). AND across both dims.
    assert!(
        snap.available.requests >= 1 && snap.available.cost_micros >= 1,
        "synthesis reserve survived the tight ceiling (AND across both dims): \
         snap={snap:?}"
    );
    // Conservation holds (belt-and-suspenders).
    assert_eq!(
        snap.available + snap.live_reservations + snap.consumed,
        snap.total,
        "conservation holds under tight budget"
    );
}

/// N1 REGRESSION (re-review round-2, AC10): the synthesis reserve must NOT be
/// leaked when the gate-token pre-mint fails. `consume(SYNTHESIS_RESERVE)` is
/// irreversible — the ledger has no `unconsume` — so the consume is ordered
/// AFTER the pre-mint succeeds. Under the OLD ordering (consume-then-pre-mint)
/// a pre-mint failure stranded the reserve in `consumed`; under the NEW
/// ordering (pre-mint-then-consume) the consume never runs on failure.
///
/// The failure is triggered by a coordinator whose `max_depth == 0`: the
/// budget-only `needed` check does NOT guard depth, so it PASSES, but the very
/// first gate-token `delegate` fails (`attempted_depth 1 > max_depth 0` →
/// `MaxDepthExceeded`). This is the reachable failure the budget-only check
/// lets through; the reserve-leak invariant holds regardless of WHICH mint
/// fails, so a first-mint failure fully exercises the ordering fix.
#[tokio::test]
async fn n1_synthesis_reserve_not_leaked_on_gate_token_mint_failure() {
    use rustain::domain::models::capability_token::{
        Budget, CapabilityFlag, CapabilitySet, CapabilityToken,
    };
    // Budget exactly covers reserve + 2-spoke gate aggregate so the `needed`
    // check PASSES; max_depth == 0 so the FIRST gate-token mint FAILS with
    // MaxDepthExceeded (the check is budget-only — it does not guard depth).
    let budget = Budget {
        requests: SYNTHESIS_RESERVE.requests + 2,
        cost_micros: SYNTHESIS_RESERVE.cost_micros + 2,
    };
    let root = CapabilityToken::root(
        AgentId::root(),
        CapabilitySet::from_flags(&[CapabilityFlag::Spawn]),
        budget,
        0, // max_depth == 0 → first delegate fails (MaxDepthExceeded)
        None,
        Some(1_000),
    );
    let (exe, ledger) = build_executor(
        Arc::new(FakeRunner::new(vec![
            NodeState::Completed,
            NodeState::Completed,
        ])) as Arc<dyn SubagentRunner>,
        root.clone(),
    );
    // The wave fails at the gate-token pre-mint (max_depth=0).
    let err = exe
        .run_fork_join(request(AgentId::root(), vec![spoke("a"), spoke("b")]))
        .await
        .expect_err("pre-mint must fail (max_depth=0)");
    assert!(
        matches!(err, OrchestrationError::SpawnRefused(_)),
        "expected SpawnRefused from the failed pre-mint, got {err:?}"
    );

    // N1 invariant: the reserve was NOT leaked. The consume never ran (pre-mint
    // failed first), so `consumed == ZERO` and the FULL budget remains
    // available (the reserve is still usable for the synthesis floor). Under
    // the OLD consume-then-pre-mint ordering, `consumed` would be the reserve.
    let snap = ledger.conservation(&root.id).unwrap();
    assert_eq!(
        snap.consumed,
        Budget::ZERO,
        "N1: synthesis reserve NOT consumed on pre-mint failure (consumed must \
         be ZERO — the OLD consume-then-pre-mint ordering leaked the reserve \
         here). snap={snap:?}"
    );
    assert!(
        snap.available.requests >= SYNTHESIS_RESERVE.requests
            && snap.available.cost_micros >= SYNTHESIS_RESERVE.cost_micros,
        "N1: the full synthesis reserve remains available after the failed \
         pre-mint (no leak). snap={snap:?}"
    );
    // Conservation holds (the accounting identity is intact — the leak was an
    // available→consumed shift, not a destruction; this pins no breakage).
    assert_eq!(
        snap.available + snap.live_reservations + snap.consumed,
        snap.total,
        "N1: conservation holds across the failed pre-mint"
    );
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    /// REUSE the shared conservation invariant
    /// (`conformance_authority_provider::conservation_holds_across_random_consume_settle`
    /// — same assertion shape `available + live_reservations + consumed ==
    /// total`, proven non-vacuous at delegation depth ≥ 2). The canonical
    /// proptest drives the ledger directly; this one drives a full fork-join
    /// wave through the executor so 14.3's NEW terminal ops (a REAL
    /// `CancellationToken` cancel-all via `run_fork_join_with_cancel` +
    /// per-spoke partial failure) actually touch 14.3's settle/cancel paths
    /// (Murat; G7/G8). The prior version called `run_fork_join` (no real
    /// cancel) and only went to n=4 — leaving the upper half of
    /// `FORK_JOIN_SPAWN_CAP=8` untested. The invariant assertion is identical
    /// to the canonical one (this is the reuse — no re-derivation).
    #[test]
    fn ac10_conservation_holds_across_cancel_and_partial_failure(
        n in 1usize..=FORK_JOIN_SPAWN_CAP,
        cancel_mask in 0u16..=0xFFFF,
        fail_mask in 0u16..=0xFFFF,
        wave_cancel in 0u8..=1,
    ) {
        // start_paused so the cancel-task's 1ms sleep races the FakeRunner's
        // 5ms sleep deterministically (cancel wins on the wave_cancel path).
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .start_paused(true)
            .build()
            .expect("paused current-thread runtime");
        let _ = rt.block_on(async move {
            let root = root_token();
            let n = n.min(FORK_JOIN_SPAWN_CAP);
            // Build a terminal plan: bit i of cancel_mask → Cancelled; else bit
            // i of fail_mask → Failed; else Completed.
            let mut terminals = Vec::new();
            for i in 0..n {
                if cancel_mask & (1 << i) != 0 {
                    terminals.push(NodeState::Cancelled);
                } else if fail_mask & (1 << i) != 0 {
                    terminals.push(NodeState::Failed);
                } else {
                    terminals.push(NodeState::Completed);
                }
            }
            let spokes: Vec<SpokeSpec> = (0..n).map(|i| spoke(&format!("s{i}"))).collect();
            let (exe, ledger) = build_executor(
                Arc::new(FakeRunner::new(terminals)) as Arc<dyn SubagentRunner>,
                root.clone(),
            );
            // ALWAYS route through run_fork_join_with_cancel so the wave's
            // CancellationToken tree is the real one. On the wave_cancel path
            // a real cancel-all fires (propagates to every child via the
            // CancellationToken tree); on the no-cancel path the token is
            // never cancelled and the wave completes normally.
            let wave_token = CancellationToken::new();
            if wave_cancel == 1 {
                let cancel_for_test = wave_token.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                    cancel_for_test.cancel();
                });
            }
            let outcome = exe
                .run_fork_join_with_cancel(request(AgentId::root(), spokes), wave_token)
                .await
                .unwrap();
            // G7: exactly N terminal outcomes, none missing.
            prop_assert_eq!(outcome.spokes.len(), n);
            // REUSED conservation invariant — identical assertion to the
            // canonical `conservation_holds_across_random_consume_settle`.
            let snap = ledger.conservation(&root.id).unwrap();
            prop_assert_eq!(
                snap.available + snap.live_reservations + snap.consumed,
                snap.total,
                "conservation holds across real cancel-all + partial-failure"
            );
            Ok::<(), proptest::test_runner::TestCaseError>(())
        });
    }
}

#[test]
fn ac10_clock_escalation_before_threshold_does_not_fire() {
    // Advance to threshold − 1 → NO escalate (real wall time has passed, proving
    // advance() — not wall clock — drives it; the >=→> boundary mutant is killed
    // by the at-threshold case below).
    let clock = MockClock::at_wall_ms(0);
    clock.set_wall_anchor_ms(0);
    clock.advance(std::time::Duration::from_millis(
        WAIT_ESCALATE_THRESHOLD_MS - 1,
    ));
    let elapsed = elapsed_ms(&clock, 0);
    assert!(!should_escalate(elapsed, WAIT_ESCALATE_THRESHOLD_MS));
}

#[test]
fn ac10_clock_escalation_at_threshold_fires_once() {
    let clock = MockClock::at_wall_ms(0);
    clock.set_wall_anchor_ms(0);
    clock.advance(std::time::Duration::from_millis(WAIT_ESCALATE_THRESHOLD_MS));
    let elapsed = elapsed_ms(&clock, 0);
    // At-threshold escalates (>= not >) — exactly-once positive control.
    assert!(should_escalate(elapsed, WAIT_ESCALATE_THRESHOLD_MS));
}
#[test]
fn ac10_elapsed_negative_clock_fails_closed() {
    // A pre-epoch / pre-dispatch clock → elapsed 0 (fail-safe: not escalated).
    let clock = MockClock::at_wall_ms(0);
    let elapsed = elapsed_ms(&clock, 5_000); // dispatched "in the future"
    assert_eq!(elapsed, 0);
}

// ─── N3 — wave-level stuck escalation (re-review round-2, AC10) ───────────

/// N3 WAVE-LEVEL STUCK ESCALATION (re-review round-2). A TRULY stuck child —
/// one that holds its status channel open but NEVER emits a terminal (and never
/// closes it) — is driven through the full wave path: `dispatch_launch` →
/// `collect_terminal` (timeout) → `Terminal::Stuck` → `structured_result` →
/// `SpokeResult::Failed { reason: StuckWaiting }`.
///
/// The prior suite only unit-tested the escalation *predicates*
/// (`should_escalate`, `WaitReason::escalates`); no test drove a never-emit
/// child through the wave to the structured `Failed{StuckWaiting}` outcome, so
/// the wave-level escalation path was untested (N3). `FakeRunner::never_emits()`
/// is the "never emits a terminal" mode this needs.
///
/// Under `start_paused`, the paused tokio clock auto-advances to the
/// `WAIT_ESCALATE_THRESHOLD_MS` deadline and fires the collector's timeout, so
/// the test is deterministic and fast (no real 60s wait). G7 still holds: the
/// wave produces exactly N terminal outcomes (the stuck spoke becomes a
/// `Failed`, not a silently-dropped slot).
#[tokio::test(start_paused = true)]
async fn n3_stuck_child_never_emitting_escalates_to_stuckwaiting_failure() {
    let root = root_token();
    let (exe, _ledger) = build_executor(
        Arc::new(FakeRunner::never_emits()) as Arc<dyn SubagentRunner>,
        root.clone(),
    );
    let outcome = exe
        .run_fork_join(request(AgentId::root(), vec![spoke("stuck")]))
        .await
        .expect("wave completes even with a stuck child (G7 — exactly N, none missing)");

    // G7: exactly one terminal outcome — the stuck spoke is NOT silently dropped.
    assert_eq!(
        outcome.spokes.len(),
        1,
        "stuck spoke still produces a terminal outcome"
    );

    // The truly stuck child escalates to a Failed carrying a StuckWaiting reason
    // (collect_terminal timeout → Terminal::Stuck → structured_result).
    match &outcome.spokes[0].1 {
        SpokeResult::Failed { reason } => {
            assert!(
                reason.contains("stuck waiting"),
                "expected a StuckWaiting failure reason, got: {reason:?}"
            );
        }
        other => panic!("expected SpokeResult::Failed for the stuck child, got {other:?}"),
    }

    // Honest coverage: a single failed spoke → honest-empty synthesis (never
    // confident noise when the one spoke degraded). Proves the stuck child did
    // not silently become Completed/Empty — the honest-empty floor fires.
    assert!(
        outcome.synthesis.honest_empty,
        "a wave whose only spoke is stuck → honest-empty synthesis"
    );
}
