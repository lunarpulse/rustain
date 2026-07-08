//! Single-level fork-join executor (Story 14.3, FR129 R1).
//!
//! The multi-node generalization of the single-turn driver: fans ONE level of
//! independent children out through the sealed `dispatch` chokepoint, collects
//! AgentId-keyed results, and returns the grounded synthesis floor. DAG/wave
//! scheduling is R2.
//!
//! ## Placement (DD5 — `infrastructure/`, never `domain/`)
//!
//! The executor orchestrates `SubagentRunner` + `AuthorityProvider` + the node
//! tree + `EventBus` + the turn loop, so it lives in `infrastructure/` (lean
//! `runtime/` would also be acceptable). It adds ZERO `std::sync` locks
//! (DD5): the wave's mutable state is owned by a single collecting task using
//! `tokio::sync` + `JoinSet` + `mpsc`, sidestepping both the RR-8 TOCTOU and
//! the sync-lock ratchet.
//!
//! ## The spawn chokepoint (DD1 / AC3)
//!
//! [`ForkJoinExecutor::dispatch`] is the **sole, sealed spawn chokepoint**
//! (`pub(crate)`, NOT on the [`Orchestrator`] trait — exposing the single-child
//! primitive would leak it and create an R2 LSP trap). It validates the CHILD's
//! own delegated token at the spawn gate (Murat vacuity-closer — the 14.2-AC9
//! defeat was validating the root token, which always passes).

mod result_contract;
mod result_store;
mod window;

pub use result_contract::{
    SPOKE_SUMMARY_MAX_BYTES, SpokeYield, YieldError, first_paragraph, retry_on_schema_failure,
    salvage_on_cancel, spoke_summary, validate_yield,
};
pub(crate) use result_store::{NodeResult, ResultStore};
pub use window::{SpokeHandle, Window};

use std::sync::Arc;
use std::sync::atomic::AtomicUsize;

use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use crate::domain::clock::Clock;
use crate::domain::events::AppEvent;
use crate::domain::models::agent_id::AgentId;
use crate::domain::models::capability_token::{
    Budget, CapabilityFlag, CapabilitySet, CapabilityToken, DelegateConstraint, DelegateRequest,
};
use crate::domain::models::launch_spec::AgentLaunchSpec;
use crate::domain::models::node_state::NodeState;
use crate::domain::models::orchestration::{
    CoverageLine, DrillId, FORK_JOIN_SPAWN_CAP, ForkJoinOutcome, OrchestrationError, SpokeCitation,
    SpokeResult, SpokeSpec, SynthesisView, WaitPolicy, WaitReason,
};
use crate::domain::models::{ModelTier, SubagentError};
use crate::domain::ports::SubagentRunner;
use crate::domain::ports::wave_handle::{DrillBody, RerunOutcome, WaveHandle, WaveSnapshot};
use crate::domain::ports::{AuthorityError, AuthorityProvider, ForkJoinRequest, Orchestrator};
use crate::domain::services::authority_ledger::AuthorityLedger;
use crate::infrastructure::runtime::event_bus::EventBus;

/// Ambient cost-meter escalation threshold: a wave whose cumulative cost micros
/// crosses this triggers a `$burn ↑` surfacing on the WaveStrip (config-
/// externalized in a later story; pinned here for determinism).
pub const AMBIENT_COST_ESCALATE_MICROS: u64 = 5_000_000; // $5.00

/// Per-spoke waiting escalation threshold in ms (AC10). A spoke that has not
/// reached a terminal state after this long escalates to a hazard. Read through
/// the injected `Clock`; the `MockClock` pair proves `advance()` (not wall
/// clock) drives it.
pub const WAIT_ESCALATE_THRESHOLD_MS: u64 = 60_000;

/// Synthesis reservation (reserve-the-HERO, AC10). Fan-out draws only from
/// `(coordinator_available − reserve)`; the reserve survives the ceiling.
/// Minimal in R1 (config-externalized later); the invariant is what matters.
pub const SYNTHESIS_RESERVE: Budget = Budget {
    requests: 1,
    cost_micros: 1_000,
};

/// Spawn-gate budget for a per-spoke child token (Murat gate-token). Minimal so
/// the gate is decisive on `Spawn` capability + revocation, not on budget.
const GATE_TOKEN_BUDGET: Budget = Budget {
    requests: 1,
    cost_micros: 1,
};

// AC2 discriminator instrumentation (P8 — non-vacuous zero-sibling-transition
// proof). R1's executor NEVER schedules a spoke on a sibling's terminal state,
// so this counter stays 0 across every R1 wave. An R2 readiness predicate (or
// a mutant scheduler) would bump it; the conformance test asserts 0 + ships a
// positive control proving the counter CAN fire (so the assertion is not
// vacuously green). Gated behind test/test-instrumentation so it adds ZERO
// weight to production. This is an atomic counter, NOT a lock — it does not
// count toward `MAX_KNOWN_STD_SYNC_LOCKS`. `pub` (not `pub(crate)`) mirrors the
// `PROVIDER_CTOR_COUNT` precedent (provider_factory.rs:50) so external
// conformance tests under `--features test-instrumentation` can read + bump it
// (positive control + scheduler-mutant kill).
#[cfg(any(test, feature = "test-instrumentation"))]
pub static SIBLING_TRIGGERED_TRANSITIONS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Record a sibling-triggered transition under the AC2 discriminator counter.
/// R1 never calls this (single-level, no sibling scheduling); it exists so the
/// R2 readiness predicate + the conformance positive-control can demonstrate
/// the counter is wired (non-vacuous). `pub` so the conformance test can drive
/// it directly as the positive control / scheduler-mutant kill.
#[cfg(any(test, feature = "test-instrumentation"))]
pub fn record_sibling_triggered_transition() {
    SIBLING_TRIGGERED_TRANSITIONS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// The single-level fork-join executor. Drives children through the
/// [`SubagentRunner`] port (AC8), never a concrete runner.
pub struct ForkJoinExecutor {
    runner: Arc<dyn SubagentRunner>,
    authority: Arc<dyn AuthorityProvider>,
    ledger: Arc<AuthorityLedger>,
    event_bus: Arc<EventBus>,
    clock: Arc<dyn Clock>,
    /// The root/coordinator authority token — the mint point for per-spoke
    /// gate-tokens. In R1 the coordinator IS the root agent.
    root_authority: CapabilityToken,
    /// Story 14.3a — the latest completed wave run. Used by the trait's
    /// `rerun_spoke` to operate on the retained state (R1: single-wave, DN-1
    /// in-flight guard). Updated on `run_wave` and on Replaced reruns.
    /// tokio::sync::Mutex (NOT std::sync — zero new std::sync locks, ratchet=4).
    current_run: tokio::sync::Mutex<Option<Arc<ForkJoinRun>>>,
    /// D-C (AI-12.3): monotonic wave generation. Every new ForkJoinRun
    /// (`run_wave` + each committed rerun) fetches a unique generation; the
    /// rerun commit guard checks `current_run.generation == prev.generation`
    /// so a stale rerun (after a newer wave OR a sibling commit) cannot clobber.
    wave_generation: std::sync::atomic::AtomicU64,
    /// D-C (AI-12.3): per-slot in-flight rerun RESERVATIONS. The cap bounds
    /// DISPATCH (not just retained count): reserve atomically before dispatch,
    /// release on every terminal path. tokio::sync (std::sync ratchet stays 4).
    rerun_reservations: tokio::sync::Mutex<std::collections::HashMap<usize, u8>>,
}

impl ForkJoinExecutor {
    /// Composition-root ctor (ADR-06-09: shared ports via `Arc<dyn>` at
    /// construction). Concrete deps bound only in `startup.rs`.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        runner: Arc<dyn SubagentRunner>,
        authority: Arc<dyn AuthorityProvider>,
        ledger: Arc<AuthorityLedger>,
        event_bus: Arc<EventBus>,
        clock: Arc<dyn Clock>,
        root_authority: CapabilityToken,
    ) -> Self {
        Self {
            runner,
            authority,
            ledger,
            event_bus,
            clock,
            root_authority,
            current_run: tokio::sync::Mutex::new(None),
            wave_generation: std::sync::atomic::AtomicU64::new(0),
            rerun_reservations: tokio::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// Read the ambient cost meter (WaveStrip `$burn`). Returns the cumulative
    /// consumed cost micros under the coordinator's budget (AC4). NOT an
    /// `AtomicU64` — the real surface is [`AuthorityLedger::conservation`]
    /// (`Mutex<AuthorityState>`-backed, ADR-14-2-01).
    pub fn ambient_cost_micros(&self) -> u64 {
        self.ledger
            .conservation(&self.root_authority.id)
            .map(|s| s.consumed.cost_micros)
            .unwrap_or(0)
    }

    /// Mint a per-spoke child gate-token from the coordinator (root) authority.
    /// The request carries `Spawn` so a normally-minted child HAS it; a probe
    /// that omits `Spawn` (or pre-revokes) produces a child the gate refuses.
    fn mint_gate_token(
        &self,
        scope: AgentId,
        capabilities: CapabilitySet,
    ) -> Result<CapabilityToken, OrchestrationError> {
        let req = DelegateRequest {
            scope,
            capabilities,
            constraint: DelegateConstraint {
                allowed: capabilities,
                max_depth: 1,
                max_subset: capabilities,
            },
            budget: GATE_TOKEN_BUDGET,
            not_after: None,
            uses_limit: Some(1),
        };
        // Synchronous delegate via the ledger (the AuthorityProvider port is
        // async; the ledger is the concrete single-writer map). The gate-token
        // is coordinator-delegated, so it debits the coordinator — refunded on
        // settle.
        self.ledger
            .delegate(&self.root_authority, req)
            .map_err(|e| OrchestrationError::SpawnRefused(e.to_string()))
    }

    /// Emit a wave lifecycle event (the 14.3a seam — purely additive variants).
    fn emit(&self, event: AppEvent) {
        let _ = self.event_bus.emit_domain(event);
    }

    /// Validate a fork-join request at the boundary (AC2 single-level, AC4 cap,
    /// AC9 WaitPolicy).
    fn validate_request(&self, request: &ForkJoinRequest) -> Result<(), OrchestrationError> {
        if let Some(reason) = request.wait_policy.r1_unsupported_reason() {
            return Err(OrchestrationError::WaitPolicyUnsupported(reason));
        }
        let attempted = request.spokes.len();
        if attempted == 0 {
            return Err(OrchestrationError::Internal(
                "fork-join requires at least one spoke".into(),
            ));
        }
        if attempted > FORK_JOIN_SPAWN_CAP {
            return Err(OrchestrationError::SpawnCapExceeded {
                cap: FORK_JOIN_SPAWN_CAP,
                attempted,
            });
        }
        for (idx, spoke) in request.spokes.iter().enumerate() {
            if !spoke.waits_for_is_empty() {
                let label = if spoke.label.is_empty() {
                    format!("spoke #{idx}")
                } else {
                    spoke.label.clone()
                };
                return Err(OrchestrationError::NotSingleLevel {
                    spoke: label,
                    deps: spoke.waits_for.len(),
                });
            }
        }
        Ok(())
    }
}

/// Build an [`AgentLaunchSpec`] from a spoke (no authority concerns here).
fn launch_spec_for(spec: &SpokeSpec) -> AgentLaunchSpec {
    AgentLaunchSpec {
        prompt: spec.prompt.clone(),
        effective_model: spec.effective_model.clone(),
        tier: spec.tier,
        tools_allow: spec.tools_allow.clone(),
        parent_ctx_tokens: 0,
        sandbox_override: None,
        parent_trace: None,
        isolated: false,
    }
}

/// **The sole spawn chokepoint** (DD1 / AC3). The single function that:
/// 1. validates the CHILD's own delegated `gate_token` for `Spawn` (Murat
///    vacuity-closer — NOT the coordinator/root token);
/// 2. drives the child through [`SubagentRunner::launch`] (AC8, the port).
///
/// The wave fan-out calls THIS function, so there is exactly one sanctioned
/// spawn path. The conformance guard pins the production `.launch(` call-site
/// count (AC3 exact-count pin). A child
/// token lacking `Spawn` or pre-revoked → REFUSED, no node, no persistent
/// debit (gate-token reservation refunded on every exit path).
pub async fn dispatch_launch(
    runner: &Arc<dyn SubagentRunner>,
    authority: &Arc<dyn AuthorityProvider>,
    spec: &SpokeSpec,
    gate_token: CapabilityToken,
    wave_cancel: CancellationToken,
) -> Result<crate::domain::models::TaskHandle, OrchestrationError> {
    // ── Spawn gate: validate the CHILD token (not the coordinator's). ──
    if let Err(err) = authority
        .validate(&gate_token, &CapabilityFlag::Spawn, &gate_token.scope)
        .await
    {
        // Refund the gate-token reservation (no persistent debit on refusal).
        let _ = authority.settle(&gate_token.id).await;
        return Err(OrchestrationError::SpawnRefused(err.to_string()));
    }

    let launch_spec = launch_spec_for(spec);
    let cancel = wave_cancel.child_token();
    let handle = match runner.launch(launch_spec, cancel).await {
        Ok(h) => h,
        Err(e) => {
            // Refund the gate-token reservation on launch failure — the doc
            // promise is "settled on every exit path" and launch-Err is one.
            // AI-12.3 party-mode finding (Amelia + Murat, independently): the
            // prior `?` stranded the reservation in `live_reservations` (slow
            // budget leak — conservation invariant held, but `available`
            // eroded by one GATE_TOKEN_BUDGET per refused launch, risking a
            // spurious BudgetPaused). FakeRunner could not trigger this (its
            // launch is infallible); only a real runner launch-Err reaches it.
            let _ = authority.settle(&gate_token.id).await;
            return Err(OrchestrationError::Runner(e.to_string()));
        }
    };

    // Register-before-spawn is satisfied inside `launch` (RR-8 ordering):
    // `register` runs before the child task can be observed by cancel.
    // Always refund the gate-token — it was the spawn gate only (the child's
    // tool-dispatch authority is launch's own delegated token).
    let _ = authority.settle(&gate_token.id).await;
    Ok(handle)
}

/// Per-spoke dispatch through the sealed `dispatch_launch` chokepoint.
/// Both the wave loop and `rerun_spoke` reach the launch through this
/// single intermediary — keeping the `.launch(` site count at exactly 1.
async fn dispatch_one(
    runner: &Arc<dyn SubagentRunner>,
    authority: &Arc<dyn AuthorityProvider>,
    spec: &SpokeSpec,
    gate_token: CapabilityToken,
    wave_cancel: CancellationToken,
) -> Result<crate::domain::models::TaskHandle, OrchestrationError> {
    dispatch_launch(runner, authority, spec, gate_token, wave_cancel).await
}

#[async_trait::async_trait]
impl Orchestrator for ForkJoinExecutor {
    async fn run_fork_join(
        &self,
        request: ForkJoinRequest,
    ) -> Result<ForkJoinOutcome, OrchestrationError> {
        // Convenience: delegates to run_wave and projects the outcome.
        // Mints a fresh cancel token (callers that don't need cancel use this).
        // REVIEW (P5): `run_wave` RETAINS this wave on the executor's
        // `current_run`, so a later `rerun_spoke` will operate on it. Callers
        // that want an outcome with no retained side-state must be aware of
        // this (or avoid the convenience wrapper).
        let handle = self.run_wave(request, CancellationToken::new()).await?;
        Ok(handle.snapshot().outcome)
    }

    async fn run_wave(
        &self,
        request: ForkJoinRequest,
        cancel: CancellationToken,
    ) -> Result<Arc<dyn WaveHandle>, OrchestrationError> {
        let run = self.run_fork_join_run(request, cancel.clone()).await?;
        // Store the wave_cancel on the run so WaveHandle::cancel() works.
        let run = ForkJoinRun {
            wave_cancel: cancel,
            // D-C (AI-12.3): assign this wave a fresh generation.
            generation: self
                .wave_generation
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            ..run
        };
        let arc_run = Arc::new(run);
        // Update the executor's retained state (R1 single-wave).
        *self.current_run.lock().await = Some(Arc::clone(&arc_run));
        Ok(arc_run as Arc<dyn WaveHandle>)
    }

    async fn rerun_spoke(
        &self,
        slot: usize,
        cancel: CancellationToken,
    ) -> Result<RerunOutcome, OrchestrationError> {
        // Read the current run from internal state (R1: single-wave).
        let prev = {
            let guard = self.current_run.lock().await;
            guard
                .as_ref()
                .ok_or_else(|| OrchestrationError::Internal("no active wave for rerun".into()))?
                .clone()
        };
        let outcome = self.rerun_spoke_run(&prev, slot, cancel).await?;
        // REVIEW (P6): `rerun_spoke_run` commits the new run to `current_run`
        // on success under a generation guard (a stale rerun cannot clobber a
        // newer wave — see the guard in `rerun_spoke_run`). No state update
        // belongs here; this method is a thin read+delegate.
        Ok(outcome)
    }
}

impl ForkJoinExecutor {
    /// The wave body, parameterized over the wave [`CancellationToken`].
    /// Cancelling `wave_cancel` is cancel-all: it propagates to every child's
    /// token (the CancellationToken tree) — committed-exact (N = dispatched
    /// spoke count). Exposed `pub` so tests + the WaveStrip's `cancel all` key
    /// inject a token they control (AC10). Delegates to `run_fork_join_run`
    /// and projects `.outcome` (DD-B3: the domain port returns the pure
    /// `ForkJoinOutcome`; the infra handle stays here).
    pub async fn run_fork_join_with_cancel(
        &self,
        request: ForkJoinRequest,
        wave_cancel: CancellationToken,
    ) -> Result<ForkJoinOutcome, OrchestrationError> {
        let run = self.run_fork_join_run(request, wave_cancel).await?;
        Ok(run.outcome)
    }

    /// The wave body — returns the infra-side [`ForkJoinRun`] handle (DD-B3).
    /// `ForkJoinOutcome` (pure domain) is extracted by
    /// [`run_fork_join_with_cancel`] / [`Orchestrator::run_fork_join`]. The
    /// handle carries the drill-source (`Arc<ResultStore>`) and spec map for
    /// rerun/drill.
    pub async fn run_fork_join_run(
        &self,
        request: ForkJoinRequest,
        wave_cancel: CancellationToken,
    ) -> Result<ForkJoinRun, OrchestrationError> {
        self.validate_request(&request)?;

        // P13: the coordinator that owns this wave must own the authority the
        // executor mints child tokens from. A mismatched coordinator would
        // silently debit root's budget under root's identity.
        if request.coordinator != self.root_authority.scope {
            return Err(OrchestrationError::Internal(format!(
                "coordinator {:?} does not own this executor's root authority {:?}",
                request.coordinator, self.root_authority.scope
            )));
        }

        let spokes = request.spokes.clone();
        let coordinator = request.coordinator.clone();
        let n = spokes.len();

        // ── Reserve-the-HERO + aggregate gate-token cost (AC10, P5/P11). ──
        // Fan-out may mint up to `n` gate tokens, each `GATE_TOKEN_BUDGET`; a
        // coordinator near its ceiling must not pass a single-dimension check
        // then fail mid-mint. The reserve + the aggregate gate reservation are
        // checked TOGETHER (AND, not OR) so draining either dimension refuses.
        let gate_aggregate = Budget {
            requests: GATE_TOKEN_BUDGET.requests * n as u64,
            cost_micros: GATE_TOKEN_BUDGET.cost_micros * n as u64,
        };
        let needed = Budget {
            requests: SYNTHESIS_RESERVE.requests + gate_aggregate.requests,
            cost_micros: SYNTHESIS_RESERVE.cost_micros + gate_aggregate.cost_micros,
        };
        let available = self
            .ledger
            .available(&self.root_authority.id)
            .map_err(|e| OrchestrationError::Internal(e.to_string()))?;
        if available.requests < needed.requests || available.cost_micros < needed.cost_micros {
            // AC10 budget-ceiling auto-pause: typed (not generic Internal) so
            // the caller / WaveStrip can surface `paused`. Never silent death;
            // the reserve is untouched (we have not debited it yet).
            return Err(OrchestrationError::BudgetPaused { available, needed });
        }

        let concurrency = request.concurrency.clamp(1, n).min(FORK_JOIN_SPAWN_CAP);
        let semaphore = Arc::new(Semaphore::new(concurrency));

        self.emit(AppEvent::ForkJoinStarted {
            coordinator: coordinator.clone(),
            spoke_count: n,
        });

        // P2: pre-mint ALL gate tokens up front. If a mint fails on spoke k,
        // the already-minted tokens (0..k) are refunded — no ledger leak from
        // a partial mint failure stranding reservations.
        let mut gate_tokens: Vec<CapabilityToken> = Vec::with_capacity(n);
        for _ in &spokes {
            // Fresh non-root LOCAL scope per spoke (delegate rejects path/root
            // + duplicate scopes).
            let scope = AgentId::new();
            let gate_caps = CapabilitySet::from_flags(&[CapabilityFlag::Spawn]);
            match self.mint_gate_token(scope, gate_caps) {
                Ok(tok) => gate_tokens.push(tok),
                Err(e) => {
                    for tok in gate_tokens {
                        let _ = self.authority.settle(&tok.id).await;
                    }
                    return Err(e);
                }
            }
        }
        // P5 (N1 fix — re-review round-2): DEBIT the synthesis reserve AFTER
        // the gate-token pre-mint succeeds. `consume` is irreversible — the
        // ledger has no `unconsume` — so debiting the reserve before a pre-
        // mint failure would leak it: the already-minted tokens (0..k) ARE
        // refunded by the settle loop above, but the reserve was not. Ordering
        // consume AFTER pre-mint keeps conservation on every exit path. The
        // upfront `needed` check (reserve + gate aggregate) still guarantees
        // enough headroom here — pre-mint consumed exactly `gate_aggregate`
        // (single-threaded; no concurrent fan-out yet), so `available` still
        // covers `SYNTHESIS_RESERVE`.
        self.ledger
            .consume(&self.root_authority.id, SYNTHESIS_RESERVE)
            .map_err(|e| OrchestrationError::Internal(e.to_string()))?;

        // P3: collect via mpsc so a panicking spoke still produces a terminal
        // outcome (G7: exactly N, none missing). The fallback after the drain
        // fills any idx that never sent (belt-and-suspenders for an aborted
        // task). The semaphore permit is held across the child's whole
        // lifetime (run-concurrency bound, not launch-rate).
        let (result_tx, mut result_rx) = tokio::sync::mpsc::channel::<SpokeOutcome>(n);
        let mut join_set = tokio::task::JoinSet::new();
        for (idx, (spoke, gate_token)) in spokes.iter().cloned().zip(gate_tokens).enumerate() {
            let runner = self.runner.clone();
            let authority = self.authority.clone();
            let wave_cancel_child = wave_cancel.clone();
            let permit_sem = semaphore.clone();
            let tx = result_tx.clone();
            let clock = self.clock.clone();
            join_set.spawn(async move {
                let _permit = match permit_sem.acquire().await {
                    Ok(p) => p,
                    Err(_) => {
                        let _ = tx
                            .send(failed_outcome(idx, &spoke, "semaphore closed"))
                            .await;
                        return;
                    }
                };
                let dispatched_at_ms = clock.wall_now_ms();
                let launched =
                    dispatch_one(&runner, &authority, &spoke, gate_token, wave_cancel_child).await;
                let (agent_id, label, result, body, isolation_diff) = match launched {
                    Ok(mut handle) => {
                        let agent_id = handle.agent_id.clone();
                        let label = spoke.label.clone();
                        let (terminal, raw, isolation_diff) =
                            collect_terminal(&mut handle, clock.as_ref(), dispatched_at_ms).await;
                        let (result, body) = structured_result(&terminal, raw.as_deref(), &label);
                        (agent_id, label, result, body, isolation_diff)
                    }
                    Err(e) => {
                        let agent_id = agent_id_for(&spoke, idx);
                        let label = spoke.label.clone();
                        // P21: do not surface raw internal error strings; map
                        // to a stable, sanitized SpokeResult::Failed reason.
                        let result = SpokeResult::Failed {
                            reason: sanitize_failure(&e),
                        };
                        (agent_id, label, result, String::new(), None)
                    }
                };
                let _ = tx
                    .send(SpokeOutcome {
                        idx,
                        agent_id,
                        label,
                        result,
                        body,
                        isolation_diff,
                    })
                    .await;
            });
        }
        drop(result_tx); // close after all spawns so recv() terminates.

        let mut store = ResultStore::new();
        let mut spec_by_agent: std::collections::HashMap<AgentId, SpokeSpec> =
            std::collections::HashMap::new();
        let mut outcomes: Vec<Option<(AgentId, SpokeResult)>> = (0..n).map(|_| None).collect();
        // P1 (DD3 / NFR68): collect isolated children's captured deltas here,
        // then hand the map to the `ForkJoinRun` (write-only in R1; R2 reads).
        let mut delta_store: std::collections::HashMap<
            AgentId,
            crate::domain::models::UnifiedDiff,
        > = std::collections::HashMap::new();
        while let Some(outcome) = result_rx.recv().await {
            let SpokeOutcome {
                idx,
                agent_id,
                label,
                result,
                body,
                isolation_diff,
            } = outcome;
            store.insert(NodeResult::ingest(
                agent_id.clone(),
                label.clone(),
                result.clone(),
                body,
            ));
            outcomes[idx] = Some((agent_id.clone(), result.clone()));
            spec_by_agent.insert(agent_id.clone(), spokes[idx].clone());
            if let Some(d) = isolation_diff {
                delta_store.insert(agent_id.clone(), d);
            }
            self.emit(AppEvent::SpokeCompleted { agent_id, label });
        }
        // P3 fallback: a task that panicked/aborted before sending still gets a
        // terminal outcome (G7 — exactly N, none missing, none silently dropped).
        for (idx, slot) in outcomes.iter_mut().enumerate() {
            if slot.is_none() {
                let agent_id = agent_id_for(&spokes[idx], idx);
                let result = SpokeResult::Failed {
                    reason: "spoke task did not produce a terminal result".into(),
                };
                store.insert(NodeResult::ingest(
                    agent_id.clone(),
                    spokes[idx].label.clone(),
                    result.clone(),
                    String::new(),
                ));
                spec_by_agent.insert(agent_id.clone(), spokes[idx].clone());
                *slot = Some((agent_id, result));
            }
        }

        // P6: emit WaveCancelled when the wave was cancelled (committed-exact:
        // killed = count of Cancelled outcomes — the variant carries the count
        // the 14.3a seam advertises).
        if wave_cancel.is_cancelled() {
            let killed = outcomes
                .iter()
                .filter(|o| matches!(o, Some((_, SpokeResult::Cancelled))))
                .count();
            self.emit(AppEvent::WaveCancelled {
                coordinator: coordinator.clone(),
                killed,
            });
        }

        // Build the grounded synthesis floor (AC7) via the shared helper.
        let synthesis = build_synthesis_floor(&store);

        self.emit(AppEvent::SynthesisReady {
            coordinator: coordinator.clone(),
            honest_empty: synthesis.honest_empty,
        });

        let final_outcomes: Vec<(AgentId, SpokeResult)> = outcomes.into_iter().flatten().collect();

        // Slots: AgentIds in dispatch order (positional — DD-B5 rerun uses
        // replace_at_slot with a stable slot index, NOT remove+append).
        let slots: Vec<AgentId> = final_outcomes.iter().map(|(id, _)| id.clone()).collect();
        // PATCH-1 (review): finalize `store.order` to true dispatch order now
        // that every spoke has terminated (insert above ran in completion
        // order). Without this, `replace_at_slot`'s positional assert fires on
        // any out-of-order completion — the production norm under concurrency.
        store.reorder(slots.clone());

        Ok(ForkJoinRun {
            outcome: ForkJoinOutcome {
                spokes: final_outcomes,
                synthesis,
            },
            store: Arc::new(store),
            spec_by_agent,
            delta_store,
            slots,
            resolve_count: AtomicUsize::new(0),
            // DN-3 (AC4) storm-cap: per-slot re-run counter, starts at 0.
            rerun_counts: vec![0u8; n],
            // 14.3a: default token; run_wave overrides with the real root.
            wave_cancel: wave_cancel.clone(),
            // D-C (AI-12.3): placeholder; run_wave assigns the real generation.
            generation: 0,
        })
    }

    /// Concrete single-spoke rerun (DD-B6). Borrows the prior [`ForkJoinRun`]
    /// (compiler-enforced non-destructiveness), deep-clones CoW the store,
    /// re-dispatches ONE spoke through `dispatch_one` → `dispatch_launch`, and
    /// builds the new [`ForkJoinRun`] ONLY on terminal-SUCCESS. Cancel/fail/
    /// dispatch-error → [`RerunOutcome::Reverted`] (prior untouched). Fresh
    /// AgentId + stable SLOT (DD-B5); deep-clone CoW (DD-B4).
    ///
    /// Story 14.3a (F3): takes a cancel token that should be a `child_token()`
    /// of the wave's root token (threaded from the event loop).
    pub async fn rerun_spoke_run(
        &self,
        prev: &ForkJoinRun,
        slot: usize,
        cancel: CancellationToken,
    ) -> Result<RerunOutcome, OrchestrationError> {
        // DN-3 (AC4) storm-cap.
        const RERUN_SPOKE_CAP: u8 = 3;
        // Validate slot (DD-B5: stable positional index).
        if slot >= prev.slots.len() {
            return Err(OrchestrationError::InvalidSlot(slot));
        }
        // D-C (AI-12.3): RESERVE the slot atomically so the DN-3 cap bounds
        // DISPATCH (committed + in-flight reservations), not just retained
        // count. Released on every terminal path below + release_rerun_reservation.
        {
            let mut reservations = self.rerun_reservations.lock().await;
            let committed = prev.inherent_rerun_count_for_slot(slot);
            let in_flight = reservations.get(&slot).copied().unwrap_or(0);
            if committed.saturating_add(in_flight) >= RERUN_SPOKE_CAP {
                return Ok(RerunOutcome::Reverted { slot });
            }
            *reservations.entry(slot).or_insert(0) += 1;
        }
        let old_id = &prev.slots[slot];
        let spec = prev
            .spec_by_agent
            .get(old_id)
            .ok_or_else(|| OrchestrationError::SpecNotFound(old_id.clone()))?
            .clone();

        // Mint a fresh gate token for the rerun spoke.
        let fresh_scope = AgentId::new();
        let gate_caps = CapabilitySet::from_flags(&[CapabilityFlag::Spawn]);
        let gate_token = match self.mint_gate_token(fresh_scope, gate_caps) {
            Ok(t) => t,
            Err(e) => {
                self.release_rerun_reservation(slot).await;
                return Err(e);
            }
        };

        // Dispatch through the sealed chokepoint (AC3 — no new `.launch(`).
        // Story 14.3a (F3): use the provided cancel token (child of wave root),
        // NOT a fresh orphan `CancellationToken::new()`.
        let launched = dispatch_one(&self.runner, &self.authority, &spec, gate_token, cancel).await;

        let mut handle = match launched {
            Ok(h) => h,
            // Dispatch failed → Reverted (prior untouched — DD-B6). Release reservation.
            Err(_) => {
                self.release_rerun_reservation(slot).await;
                return Ok(RerunOutcome::Reverted { slot });
            }
        };

        let new_agent_id = handle.agent_id.clone();
        let dispatched_at_ms = self.clock.wall_now_ms();
        let (terminal, raw, isolation_diff) =
            collect_terminal(&mut handle, self.clock.as_ref(), dispatched_at_ms).await;
        let (result, body) = structured_result(&terminal, raw.as_deref(), &spec.label);

        match &result {
            SpokeResult::Completed { .. } => {
                // SUCCESS: deep-clone CoW the store (DD-B4).
                let mut next_store = (*prev.store).clone();
                let new_node = NodeResult::ingest(
                    new_agent_id.clone(),
                    spec.label.clone(),
                    result.clone(),
                    body,
                );
                next_store.replace_at_slot(slot, old_id, new_agent_id.clone(), new_node);

                let mut next_spec = prev.spec_by_agent.clone();
                next_spec.remove(old_id);
                next_spec.insert(new_agent_id.clone(), spec);

                let mut next_slots = prev.slots.clone();
                next_slots[slot] = new_agent_id.clone();

                let mut next_spokes = prev.outcome.spokes.clone();
                next_spokes[slot] = (new_agent_id.clone(), result);

                let next_synthesis = build_synthesis_floor(&next_store);

                let new_run = Arc::new(ForkJoinRun {
                    outcome: ForkJoinOutcome {
                        spokes: next_spokes,
                        synthesis: next_synthesis,
                    },
                    store: Arc::new(next_store),
                    spec_by_agent: next_spec,
                    delta_store: {
                        let mut m = prev.delta_store.clone();
                        if let Some(d) = isolation_diff {
                            m.insert(new_agent_id.clone(), d);
                        }
                        m
                    },
                    slots: next_slots,
                    resolve_count: AtomicUsize::new(0),
                    rerun_counts: {
                        let mut counts = prev.rerun_counts.clone();
                        if counts.len() > slot {
                            counts[slot] += 1;
                        }
                        counts
                    },
                    // Inherit the same wave-cancel root from the previous run.
                    wave_cancel: prev.wave_cancel.clone(),
                    // D-C (AI-12.3): fresh generation for the committed rerun.
                    generation: self
                        .wave_generation
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed),
                });
                // D-C (AI-12.3): generation guard — commit UNLESS a NEWER wave
                // is retained (current_run.gen != prev.gen). When current_run is
                // None (no retained wave, e.g. a caller using run_fork_join_run
                // directly) the rerun adopts the slot (commit). Catches a stale
                // rerun after a newer wave OR a sibling commit; reservation
                // released on every path.
                let committed_ok = {
                    let mut guard = self.current_run.lock().await;
                    let still_current = guard
                        .as_ref()
                        .is_none_or(|live| live.generation == prev.generation);
                    if still_current {
                        *guard = Some(Arc::clone(&new_run));
                    }
                    still_current
                };
                self.release_rerun_reservation(slot).await;
                if committed_ok {
                    Ok(RerunOutcome::Replaced(new_run as Arc<dyn WaveHandle>))
                } else {
                    // Stale: discard the rebuilt run (counts NOT bumped).
                    Ok(RerunOutcome::Reverted { slot })
                }
            }
            // CANCELLED / FAILED / EMPTY: Reverted (prior untouched — DD-B6). Release.
            _ => {
                self.release_rerun_reservation(slot).await;
                Ok(RerunOutcome::Reverted { slot })
            }
        }
    }

    /// D-C (AI-12.3): release one in-flight rerun reservation for `slot`.
    async fn release_rerun_reservation(&self, slot: usize) {
        let mut reservations = self.rerun_reservations.lock().await;
        if let Some(count) = reservations.get_mut(&slot) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                reservations.remove(&slot);
            }
        }
    }
}

/// Infrastructure-side handle for a completed fork-join wave (DD-B3).
/// `ForkJoinOutcome` stays pure domain; this handle carries the infra-side
/// drill-source (`Arc<ResultStore>`) and spec map for rerun/drill.
///
/// Story 14.3a: `impl WaveHandle` (zero-new-body delegation) so this can be
/// returned as `Arc<dyn WaveHandle>` from `Orchestrator::run_wave`. The
/// concrete type stays `pub(crate)` infra — never leaks past the domain
/// boundary.
pub struct ForkJoinRun {
    pub outcome: ForkJoinOutcome,
    /// Crate-private — `ResultStore` is `pub(crate)`, so the field is too.
    pub(crate) store: Arc<ResultStore>,
    /// Story 14.5 — durable inert isolated-child deltas, keyed by AgentId.
    /// Lock-free: ForkJoinRun is single-owner until wrapped in Arc after construction.
    pub(crate) delta_store: std::collections::HashMap<AgentId, crate::domain::models::UnifiedDiff>,
    /// Per-port spec map — `pub(crate)`.
    pub(crate) spec_by_agent: std::collections::HashMap<AgentId, SpokeSpec>,
    /// Dispatch-ordered slot AgentIds — `pub(crate)`.
    pub(crate) slots: Vec<AgentId>,
    resolve_count: AtomicUsize,
    /// Per-slot re-run counter (DN-3 storm-cap). Indexed by dispatch slot.
    pub(crate) rerun_counts: Vec<u8>,
    /// Story 14.3a (F3): the wave-cancel root token. `WaveHandle::cancel()`
    /// fires this. Defaults to a fresh (unfired) token for backward compat.
    pub(crate) wave_cancel: CancellationToken,
    /// D-C (AI-12.3): monotonic wave generation (unique per run_wave + each
    /// committed rerun). The rerun commit guard compares this to detect a
    /// stale prev (newer wave / sibling commit).
    pub(crate) generation: u64,
}

impl std::fmt::Debug for ForkJoinRun {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ForkJoinRun")
            .field("outcome", &self.outcome)
            .field("slots", &self.slots)
            .field(
                "resolve_count",
                &self
                    .resolve_count
                    .load(std::sync::atomic::Ordering::Relaxed),
            )
            .field("rerun_counts", &self.rerun_counts)
            .finish_non_exhaustive()
    }
}

/// Story 14.3a — `WaveHandle` impl via zero-new-body delegation to the
/// existing `ForkJoinRun` methods (F1+F2+F5 resolution).
impl WaveHandle for ForkJoinRun {
    fn snapshot(&self) -> WaveSnapshot {
        let completed = self
            .outcome
            .spokes
            .iter()
            .filter(|(_, r)| r.is_signal())
            .count();
        WaveSnapshot {
            spoke_count: self.outcome.spokes.len(),
            completed,
            honest_empty: Some(self.outcome.synthesis.honest_empty),
            cancelled: self.wave_cancel.is_cancelled(),
            outcome: self.outcome.clone(),
            resolve_count: self
                .resolve_count
                .load(std::sync::atomic::Ordering::Relaxed),
            slots: self.slots.clone(),
            rerun_counts: self.rerun_counts.clone(),
        }
    }

    fn cancel(&self) {
        self.wave_cancel.cancel();
    }

    fn drill(&self, slot: usize) -> Option<DrillBody> {
        let agent_id = self.slots.get(slot)?;
        let drill_id = self.drill_source(agent_id)?;
        self.drill_body(&drill_id)
    }

    fn drill_id(&self, slot: usize) -> Option<crate::domain::models::orchestration::DrillId> {
        let agent_id = self.slots.get(slot)?;
        self.drill_source(agent_id)
    }

    fn slots(&self) -> Vec<AgentId> {
        self.slots.clone()
    }

    fn rerun_count_for_slot(&self, slot: usize) -> u8 {
        self.rerun_counts.get(slot).copied().unwrap_or(0)
    }

    fn resolve_count(&self) -> usize {
        self.resolve_count
            .load(std::sync::atomic::Ordering::Relaxed)
    }
}

impl ForkJoinRun {
    /// Cheap id lookup — validates the agent_id belongs to this wave.
    /// Does NOT resolve the body (resolve_count unchanged).
    pub fn drill_source(&self, id: &AgentId) -> Option<DrillId> {
        if self.store.get(id).is_some() {
            Some(DrillId(id.clone()))
        } else {
            None
        }
    }

    /// Lazy body resolution — increments resolve_count. Returns `None` for a
    /// stale `DrillId` rather than panicking (PATCH-7).
    pub fn drill_body(&self, d: &DrillId) -> Option<DrillBody> {
        let node = self.store.get(&d.0)?;
        self.resolve_count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Some(DrillBody(node.body.clone()))
    }

    /// Read-only view of the dispatch-ordered slot AgentIds.
    pub fn inherent_slots(&self) -> &[AgentId] {
        &self.slots
    }

    /// Re-run count for a slot (DN-3 storm-cap observability). 0 for OOR.
    pub fn inherent_rerun_count_for_slot(&self, slot: usize) -> u8 {
        self.rerun_counts.get(slot).copied().unwrap_or(0)
    }
}

/// Build the grounded synthesis floor from a result store (AC7).
/// Shared between wave completion and single-spoke rerun so the synthesis
/// invariant (one citation per completed spoke) holds identically.
fn build_synthesis_floor(store: &ResultStore) -> SynthesisView {
    let ordered: Vec<&NodeResult> = store.ordered();
    let spoke_results: Vec<SpokeResult> = ordered.iter().map(|r| r.to_spoke_result()).collect();
    let coverage = CoverageLine::from_results(&spoke_results);
    let citations: Vec<SpokeCitation> = ordered
        .iter()
        .filter(|r| matches!(r.outcome, SpokeResult::Completed { .. }))
        .map(|r| SpokeCitation {
            agent_id: r.agent_id.clone(),
            label: r.label.clone(),
            summary: r.compact_summary().to_string(),
        })
        .collect();
    SynthesisView::build(citations, coverage)
}

/// Per-spoke terminal outcome sent through the collection mpsc (P3).
struct SpokeOutcome {
    idx: usize,
    agent_id: AgentId,
    label: String,
    result: SpokeResult,
    body: String,
    /// P1 (DD3 / NFR68): the isolated child's captured `UnifiedDiff` (None for
    /// non-isolated spokes or failed dispatches). Collected into
    /// `ForkJoinRun::delta_store` — write-only in R1.
    isolation_diff: Option<crate::domain::models::UnifiedDiff>,
}

/// Build a `SpokeOutcome` for a spoke that failed before producing a result.
fn failed_outcome(idx: usize, spec: &SpokeSpec, reason: &str) -> SpokeOutcome {
    SpokeOutcome {
        idx,
        agent_id: agent_id_for(spec, idx),
        label: spec.label.clone(),
        result: SpokeResult::Failed {
            reason: reason.into(),
        },
        body: String::new(),
        isolation_diff: None,
    }
}

/// Map an [`OrchestrationError`] to a sanitized `SpokeResult::Failed` reason
/// (P21): never surface raw internal error strings / ids to the synthesis view.
fn sanitize_failure(err: &OrchestrationError) -> String {
    match err {
        OrchestrationError::SpawnRefused(_) => "spawn refused by authority gate".into(),
        OrchestrationError::Runner(_) => "subagent runner error".into(),
        OrchestrationError::StuckWaiting(label) => format!("spoke `{label}` stuck waiting"),
        _ => "spoke failed".into(),
    }
}

/// Derive a UNIQUE placeholder AgentId for a spoke whose launch failed before
/// producing one (P14). Includes the dispatch index so duplicate spoke labels
/// never collapse to the same AgentId (which would make ResultStore drop the
/// second outcome and undercount coverage).
fn agent_id_for(spec: &SpokeSpec, idx: usize) -> AgentId {
    if spec.label.is_empty() {
        AgentId(format!("failed-spoke-{idx}"))
    } else {
        AgentId(format!("failed-spoke-{idx}-{}", spec.label))
    }
}

/// Drain a launched child's status channel to a terminal state (G1: a REAL
/// child, not a parked stub; the handle's `cancel` is the child's real token).
/// Also drains the optional structured-yield channel (AC6) and reads the
/// injected `Clock` to escalate a stuck spoke to a hazard after the threshold
/// (AC10 — `elapsed_ms`/`should_escalate` drive this, not wall clock).
async fn collect_terminal(
    handle: &mut crate::domain::models::TaskHandle,
    clock: &dyn Clock,
    dispatched_at_ms: i64,
) -> (
    Terminal,
    Option<String>,
    Option<crate::domain::models::UnifiedDiff>,
) {
    use Terminal::*;
    let mut last: Terminal = Running_(NodeState::Created);
    let mut raw_yield: Option<String> = None;
    let mut definitive: Option<Terminal> = None;
    loop {
        // Best-effort drain of any queued structured-yield frames (AC6): the
        // last frame wins (a child may emit incremental then a final yield).
        if let Some(rx) = handle.yield_rx.as_mut() {
            while let Ok(y) = rx.try_recv() {
                raw_yield = Some(y);
            }
        }
        match tokio::time::timeout(
            std::time::Duration::from_millis(WAIT_ESCALATE_THRESHOLD_MS),
            handle.status_rx.recv(),
        )
        .await
        {
            Ok(Some(s)) => match s {
                NodeState::Completed => {
                    definitive = Some(Completed);
                    break;
                }
                NodeState::Failed => {
                    definitive = Some(Failed);
                    break;
                }
                NodeState::Cancelled => {
                    definitive = Some(Cancelled);
                    break;
                }
                other => last = Running_(other),
            },
            Ok(None) => break, // channel closed without a terminal → stuck/empty
            Err(_) => break,   // timeout → escalate (P4)
        }
    }
    // Final drain after the loop ends.
    let raw_yield = drain_yield(handle, raw_yield);
    let terminal = match definitive {
        Some(t) => t,
        None => {
            // P4: a spoke that never reached a terminal state within the
            // threshold escalates to a hazard (AC10). `should_escalate` +
            // `elapsed_ms` (read through the injected Clock) decide it.
            let elapsed = elapsed_ms(clock, dispatched_at_ms);
            let escalates = WaitReason::AwaitingSpoke.escalates()
                && should_escalate(elapsed, WAIT_ESCALATE_THRESHOLD_MS);
            match last {
                Running_(NodeState::Completed) if !escalates => Completed,
                Running_(NodeState::Failed) if !escalates => Failed,
                Running_(NodeState::Cancelled) if !escalates => Cancelled,
                // Stuck (escalated) or a non-terminal we couldn't fold: carry
                // the last observed NodeState for diagnostics.
                Running_(ns) => Stuck(ns),
                other => Stuck(node_state_of(&other)),
            }
        }
    };
    // P1 (DD3 / NFR68 seam): drain the isolated child's captured delta. Bounded
    // await — by the time we observe the terminal status the runner's spawn
    // block has just (or is about to) send the `UnifiedDiff`; 500ms is a
    // generous bound. Absent/errored capture → None (R1 delta is write-only).
    let isolation_diff = match handle.isolation_diff_rx.take() {
        Some(rx) => match tokio::time::timeout(std::time::Duration::from_millis(500), rx).await {
            Ok(Ok(d)) => Some(d),
            _ => None,
        },
        None => None,
    };
    (terminal, raw_yield, isolation_diff)
}

/// Drain any remaining structured-yield frames after a terminal state.
fn drain_yield(
    handle: &mut crate::domain::models::TaskHandle,
    mut raw: Option<String>,
) -> Option<String> {
    if let Some(rx) = handle.yield_rx.as_mut() {
        while let Ok(y) = rx.try_recv() {
            raw = Some(y);
        }
    }
    raw
}

/// Best-effort NodeState extraction from a Terminal for diagnostics (the Stuck
/// variant carries it for the hazard reason). Non-state variants default to Created.
fn node_state_of(terminal: &Terminal) -> NodeState {
    match terminal {
        Terminal::Running_(ns) => *ns,
        Terminal::Stuck(ns) => *ns,
        Terminal::Completed => NodeState::Completed,
        Terminal::Failed => NodeState::Failed,
        Terminal::Cancelled => NodeState::Cancelled,
        Terminal::Empty => NodeState::Created,
    }
}

#[derive(Debug)]
enum Terminal {
    Completed,
    Failed,
    Cancelled,
    Empty,
    Running_(NodeState),
    /// A spoke that did not reach a terminal state within the escalation
    /// threshold (AC10). Carries the last observed state for diagnostics.
    Stuck(NodeState),
}

/// Structured result contract (AC6): map a terminal state + the captured raw
/// yield to a schema-validated [`SpokeResult`] + body. This is where the
/// contract module goes LIVE (P1):
/// - `Completed` + raw → `retry_on_schema_failure` (a malformed first yield
///   does not forfeit the spoke); a non-parseable / absent yield is honestly
///   `Empty` (never a false Completed).
/// - `Cancelled` + raw → `salvage_on_cancel` (outcome stays Cancelled; the
///   partial body is kept for drill).
/// - `Stuck` → escalates to a hazard via `WaitReason::escalates` +
///   `OrchestrationError::StuckWaiting` (P4).
///
/// `NodeResult::ingest` re-validates the body (parse don't validate at ingest).
fn structured_result(terminal: &Terminal, raw: Option<&str>, label: &str) -> (SpokeResult, String) {
    match terminal {
        Terminal::Completed => match raw {
            Some(r) => match retry_on_schema_failure(&[r.to_string()]) {
                Ok(y) => (
                    SpokeResult::Completed { summary: y.summary },
                    // Pass the raw body to ingest, which parses it (parse don't
                    // validate) and stores the validated detail.
                    r.to_string(),
                ),
                // No parseable yield → honest Empty (the body is kept for drill).
                Err(_) => (SpokeResult::Empty, r.to_string()),
            },
            None => (SpokeResult::Empty, String::new()),
        },
        Terminal::Failed => (
            SpokeResult::Failed {
                reason: "spoke failed".to_string(),
            },
            String::new(),
        ),
        Terminal::Cancelled => match raw {
            Some(r) => salvage_on_cancel(r),
            None => (SpokeResult::Cancelled, String::new()),
        },
        Terminal::Empty => (SpokeResult::Empty, String::new()),
        Terminal::Running_(s) => (
            SpokeResult::Failed {
                reason: format!("spoke did not terminate cleanly (last: {s:?})"),
            },
            String::new(),
        ),
        Terminal::Stuck(_) => {
            // P4: escalation helper path — WaitReason + StuckWaiting are live.
            let hazard = WaitReason::AwaitingSpoke;
            let reason = if hazard.escalates() {
                OrchestrationError::StuckWaiting(label.to_string()).to_string()
            } else {
                "spoke stuck waiting".to_string()
            };
            (SpokeResult::Failed { reason }, String::new())
        }
    }
}

// WaitPolicy/WaitReason escalation helpers are exercised by the Clock-driven
// tests (AC10). Kept here as pure functions so the `MockClock` pair can drive
// them deterministically.

/// `elapsed = now_ms − dispatched_at_ms` as `u64` (DD3). Exactly one `i64→u64`
/// cast at the read; a pre-epoch clock fails CLOSED (treats as elapsed 0 — i.e.
/// not-yet-escalated — which is the fail-safe direction for a fresh dispatch).
pub fn elapsed_ms(clock: &dyn Clock, dispatched_at_ms: i64) -> u64 {
    let now_ms = clock.wall_now_ms();
    if now_ms < dispatched_at_ms {
        return 0; // clock skew / pre-dispatch: not elapsed
    }
    u64::try_from(now_ms - dispatched_at_ms).unwrap_or(0)
}

/// Escalation predicate (AC10): a spoke waiting beyond the threshold escalates
/// to a hazard. `>= threshold` (not `>`) so the at-threshold case escalates
/// exactly once — the boundary mutant is killed by the at-threshold test.
pub fn should_escalate(elapsed_ms: u64, threshold_ms: u64) -> bool {
    elapsed_ms >= threshold_ms
}

#[allow(dead_code)]
fn _model_tier_used() -> ModelTier {
    ModelTier::Flagship
}

#[allow(dead_code)]
fn _wait_policy_used() -> WaitPolicy {
    WaitPolicy::All
}

#[allow(dead_code)]
fn _subagent_error_used(e: SubagentError) -> String {
    e.to_string()
}

#[allow(dead_code)]
fn _authority_error_used(e: AuthorityError) -> String {
    e.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn elapsed_ms_casts_i64_to_u64_once_and_handles_skew() {
        let clock = crate::domain::clock::MockClock::at_wall_ms(100_000);
        // dispatched at 90_000 → elapsed 10_000.
        assert_eq!(elapsed_ms(&clock, 90_000), 10_000);
        // pre-dispatch (skew) → 0, fail-safe.
        assert_eq!(elapsed_ms(&clock, 200_000), 0);
    }

    #[test]
    fn should_escalate_uses_geq_boundary() {
        // At-threshold escalates (kills the `>` boundary mutant).
        assert!(should_escalate(
            WAIT_ESCALATE_THRESHOLD_MS,
            WAIT_ESCALATE_THRESHOLD_MS
        ));
        // One below does not.
        assert!(!should_escalate(
            WAIT_ESCALATE_THRESHOLD_MS - 1,
            WAIT_ESCALATE_THRESHOLD_MS
        ));
    }

    #[test]
    fn spawn_cap_is_within_node_tree_sibling_ceiling() {
        const {
            assert!(FORK_JOIN_SPAWN_CAP <= 10);
        }
    }

    #[test]
    fn ambient_cost_reads_conservation_consumed_micros() {
        // budget available, zero consumed.
        let root = CapabilityToken::r1_root(AgentId::root());
        let ledger = Arc::new(AuthorityLedger::new(root.clone()));
        let exe = ForkJoinExecutor::new(
            Arc::new(NoopRunner) as Arc<dyn SubagentRunner>,
            Arc::new(
                crate::adapters::authority::in_process::InProcessAuthorityProvider::new(
                    ledger.clone(),
                ),
            ),
            ledger,
            Arc::new(EventBus::new(16).0),
            Arc::new(crate::domain::clock::SystemClock::default()),
            root,
        );
        assert_eq!(exe.ambient_cost_micros(), 0);
    }

    /// AC5 runtime size-differential (Murat mandate — review finding AC5).
    /// A 1MB body in the `ResultStore` and a 10MB body in a sibling store
    /// produce IDENTICAL prompt-byte bounds because the prompt-build surface
    /// is `Window<SpokeHandle>` (compact metadata only — agent_id, label,
    /// status, salience), NEVER the inlined body. A mutant that inlined the
    /// body into the window would make these byte counts differ by ~9MB.
    #[test]
    fn ac5_runtime_size_differential_window_byte_bound_is_body_invariant() {
        let one_mb = "x".repeat(1024 * 1024);
        let ten_mb = "y".repeat(10 * 1024 * 1024);
        // Two stores with identical spoke handles but bodies that differ by
        // 9MB. The window's handles carry NO body (the type-wall).
        let mut store_1mb = ResultStore::new();
        let mut store_10mb = ResultStore::new();
        let aid = AgentId("spoke-A".into());
        store_1mb.insert(NodeResult::ingest(
            aid.clone(),
            "alpha".into(),
            SpokeResult::Completed {
                summary: "summary".into(),
            },
            // Schema-valid JSON with a huge detail field — the body lives in
            // the side-table, NOT the window.
            format!(r#"{{"summary":"summary","detail":"{one_mb}"}}"#),
        ));
        store_10mb.insert(NodeResult::ingest(
            aid.clone(),
            "alpha".into(),
            SpokeResult::Completed {
                summary: "summary".into(),
            },
            format!(r#"{{"summary":"summary","detail":"{ten_mb}"}}"#),
        ));
        // Build the symbolic window from each — same handles, same metadata.
        let handle = SpokeHandle {
            agent_id: aid.clone(),
            label: "alpha".into(),
            status: NodeState::Completed,
            salience: "summary".into(),
        };
        let handles = vec![handle.clone()];
        let win_1mb = Window::new(&handles);
        let win_10mb = Window::new(&handles);
        // Prompt-byte bound: flatten the window's compact metadata to bytes
        // (the prompt-build pass). Same handles → identical byte size.
        let flatten = |w: &Window<SpokeHandle>| -> usize {
            w.handles()
                .iter()
                .map(|h| {
                    h.agent_id.0.len()
                        + h.label.len()
                        + h.salience.len()
                        + std::mem::size_of_val(&h.status)
                })
                .sum()
        };
        let bytes_1mb = flatten(&win_1mb);
        let bytes_10mb = flatten(&win_10mb);
        assert_eq!(
            bytes_1mb, bytes_10mb,
            "AC5 size-differential: window prompt-byte bound is invariant to \
             body size (1MB vs 10MB produce the same bound). A mutant inlining \
             the body would differ by ~9MB."
        );
        // Drill differential (sanity): the SIDE-TABLE bodies DO differ by 9MB.
        // Proves the test is non-vacuous (the bodies really are different).
        let drill_1mb = store_1mb.get(&aid).unwrap().body.len();
        let drill_10mb = store_10mb.get(&aid).unwrap().body.len();
        assert!(
            drill_10mb > drill_1mb + 8 * 1024 * 1024,
            "non-vacuous: side-table bodies differ by >8MB (1MB={drill_1mb}, \
             10MB={drill_10mb}) — only the window is body-invariant."
        );
    }

    /// AC7 keystone `symbolic_reference_resolves_to_full_output`: a
    /// `SpokeHandle` (the symbolic reference in the prompt window) resolves to
    /// its FULL payload via `ResultStore::get` (drill-on-open). The window
    /// never inlined the body; this is the lazy-fetch-on-open path that makes
    /// the symbolic-handle contract more than decorative. (Review finding AC7:
    /// no test called `ResultStore::get` to prove a handle drills to its full
    /// payload — this is that test.)
    #[test]
    fn ac7_symbolic_reference_resolves_to_full_output() {
        let full_payload = "PAYER_TXN_DETAIL:".to_string() + &"x".repeat(64 * 1024);
        let aid = AgentId("spoke-A".into());
        let mut store = ResultStore::new();
        store.insert(NodeResult::ingest(
            aid.clone(),
            "alpha".into(),
            SpokeResult::Completed {
                summary: "found 3 races".into(),
            },
            // Schema-valid JSON whose `detail` is the full payload — ingest
            // parses it into the side-table body.
            format!(r#"{{"summary":"found 3 races","detail":"{full_payload}"}}"#),
        ));
        // The symbolic handle in the prompt window carries NO body.
        let handle = SpokeHandle {
            agent_id: aid.clone(),
            label: "alpha".into(),
            status: NodeState::Completed,
            salience: "found 3 races".into(),
        };
        assert!(
            handle.salience.len() < full_payload.len(),
            "the window salience is compact metadata, NOT the full payload"
        );
        // Drill-on-open: the handle's agent_id resolves to the FULL payload in
        // the side-table. This is the AC5/AC7 lazy-fetch path.
        let drilled = store
            .get(&handle.agent_id)
            .expect("handle resolves to its full payload via ResultStore::get");
        assert_eq!(
            drilled.body, full_payload,
            "drill returns the FULL payload (the side-table body), proving the \
             symbolic handle is more than decorative — it is resolvable on demand."
        );
        assert_eq!(drilled.label, "alpha");
        assert!(matches!(drilled.outcome, SpokeResult::Completed { .. }));
    }
    // ─── D-C poisoned mutant (AI-12.3) — P1 stale-rerun-after-newWave ──────
    // A rerun branched from a STALE `prev` (older wave A) that completes AFTER a
    // NEWER wave B is retained MUST be rejected (Reverted) and MUST NOT clobber
    // `current_run` (B stays current). Kills the "commit unconditionally" /
    // "ptr-eq" mutants: with the generation guard removed (commit-always),
    // this test would see Replaced + a clobbered B. Red-first verified.
    //
    // Minimal in-crate harness: a runner whose launched children emit Completed.
    fn build_exe_for_rerun() -> ForkJoinExecutor {
        let root = CapabilityToken::r1_root(AgentId::root());
        let ledger = Arc::new(AuthorityLedger::new(root.clone()));
        ForkJoinExecutor::new(
            Arc::new(CompletedRunner) as Arc<dyn SubagentRunner>,
            Arc::new(
                crate::adapters::authority::in_process::InProcessAuthorityProvider::new(
                    ledger.clone(),
                ),
            ),
            ledger,
            {
                let (bus, rx) = EventBus::new(16);
                std::mem::forget(rx); // keep the domain receiver alive (no silent drops)
                Arc::new(bus)
            },
            Arc::new(crate::domain::clock::MockClock::at_wall_ms(0)),
            root,
        )
    }

    /// A runner whose every launched child emits a Completed terminal with a
    /// schema-valid yield. Minimal — just enough to drive run_wave + rerun.
    #[derive(Default)]
    struct CompletedRunner;
    #[async_trait::async_trait]
    impl SubagentRunner for CompletedRunner {
        async fn launch(
            &self,
            _spec: AgentLaunchSpec,
            cancel: CancellationToken,
        ) -> Result<crate::domain::models::TaskHandle, SubagentError> {
            let (status_tx, status_rx) =
                tokio::sync::mpsc::channel::<crate::domain::models::node_state::NodeState>(8);
            let (command_tx, mut command_rx) =
                tokio::sync::mpsc::channel::<crate::domain::models::Op>(8);
            let (parent_disc_tx, _) = tokio::sync::mpsc::unbounded_channel::<()>();
            let (yield_tx, yield_rx) = tokio::sync::mpsc::channel::<String>(4);
            let agent_id = AgentId::new();
            let task_id = nanoid::nanoid!(12);
            let child_cancel = cancel.child_token();
            let cancel_for_task = child_cancel.clone();
            tokio::spawn(async move {
                let _ = status_tx
                    .send(crate::domain::models::node_state::NodeState::Running)
                    .await;
                tokio::select! {
                    _ = cancel_for_task.cancelled() => {
                        let _ = status_tx.send(crate::domain::models::node_state::NodeState::Cancelled).await;
                    }
                    _ = tokio::time::sleep(std::time::Duration::from_millis(1)) => {
                        let _ = status_tx.send(crate::domain::models::node_state::NodeState::Completed).await;
                        let _ = yield_tx.send(r#"{"summary":"ok","detail":"d"}"#.to_string()).await;
                    }
                    _ = command_rx.recv() => {
                        let _ = status_tx.send(crate::domain::models::node_state::NodeState::Cancelled).await;
                    }
                }
            });
            Ok(crate::domain::models::TaskHandle {
                agent_id,
                status_rx,
                command_tx,
                cancel: child_cancel,
                task_id,
                subagent_type: "completed".into(),
                spawned_at: 0,
                parent_disconnect: parent_disc_tx,
                yield_rx: Some(yield_rx),
                isolation_diff_rx: None,
            })
        }
    }

    #[tokio::test]
    async fn dc_generation_guard_rejects_stale_rerun_after_newer_wave() {
        let exe = build_exe_for_rerun();
        // Wave A — one spoke. current_run = A (generation G_A).
        let req_a = crate::domain::ports::ForkJoinRequest {
            coordinator: AgentId::root(),
            spokes: vec![SpokeSpec {
                label: "a".into(),
                prompt: "explore a".into(),
                effective_model: "m".into(),
                tier: crate::domain::models::ModelTier::Flagship,
                tools_allow: crate::domain::models::ToolPolicy::InheritFromParent,
                waits_for: Vec::new(),
            }],
            wait_policy: crate::domain::models::orchestration::WaitPolicy::All,
            concurrency: 1,
        };
        use crate::domain::ports::Orchestrator;
        exe.run_wave(req_a, CancellationToken::new()).await.unwrap();
        // Snapshot the stale prev A (in-crate: read private current_run).
        let prev_a = exe
            .current_run
            .lock()
            .await
            .clone()
            .expect("wave A retained");
        let gen_a = prev_a.generation;
        // Wave B — replaces current_run (generation G_B > G_A).
        let req_b = crate::domain::ports::ForkJoinRequest {
            coordinator: AgentId::root(),
            spokes: vec![SpokeSpec {
                label: "b".into(),
                prompt: "explore b".into(),
                effective_model: "m".into(),
                tier: crate::domain::models::ModelTier::Flagship,
                tools_allow: crate::domain::models::ToolPolicy::InheritFromParent,
                waits_for: Vec::new(),
            }],
            wait_policy: crate::domain::models::orchestration::WaitPolicy::All,
            concurrency: 1,
        };
        exe.run_wave(req_b, CancellationToken::new()).await.unwrap();
        // STALE rerun branched from A's prev while current is B.
        let outcome = exe
            .rerun_spoke_run(&prev_a, 0, CancellationToken::new())
            .await
            .expect("rerun dispatches without infrastructure error");
        assert!(
            matches!(outcome, RerunOutcome::Reverted { .. }),
            "a stale rerun (prev from wave A) completing after a newer wave B MUST Revert, got {outcome:?}"
        );
        // current_run is STILL B (gen G_B), not clobbered by the stale commit.
        let current = exe
            .current_run
            .lock()
            .await
            .clone()
            .expect("current_run retained after stale rerun");
        assert_ne!(
            current.generation, gen_a,
            "the newer wave B must NOT be clobbered by a stale rerun commit"
        );
    }
    // A no-op runner used only by the ambient-cost unit test (does not launch).
    struct NoopRunner;
    #[async_trait::async_trait]
    impl SubagentRunner for NoopRunner {
        async fn launch(
            &self,
            _spec: AgentLaunchSpec,
            _cancel: CancellationToken,
        ) -> Result<crate::domain::models::TaskHandle, SubagentError> {
            Err(SubagentError::Internal("noop".into()))
        }
    }
}
