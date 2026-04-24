# Performance Tests

Performance-regression tests for load-bearing runtime primitives. Each test locks a published P99 or throughput baseline; failing the test indicates a regression worth investigating, not necessarily a blocker.

## Discipline

- **One file per subject.** E.g., `scheduler_overhead.rs` for `ToolScheduler::schedule` overhead vs direct call.
- **Cargo target**: `[[bench]]` or integration-test binary with `#[ignore]` so `cargo test` does not run them by default. Invoke explicitly: `cargo test --test perf_<subject> -- --ignored`.
- **Baseline as a constant**: each test has a `BASELINE_*` constant documenting the target (e.g., `const BASELINE_P99_MICROS: u64 = 100;`). Moving the baseline requires an ADR or story-level decision.
- **Hardware sensitivity**: perf numbers vary across machines. Baselines should target the slowest reasonable CI runner — not a beefy developer laptop. When a baseline fails only on a slow runner, widen the baseline, don't gate CI.

## Planned Inventory

| File | Subject | Baseline target | Source story |
|---|---|---|---|
| `scheduler_overhead.rs` | `ToolScheduler::schedule` vs direct `tools.execute` | P99 < 100 µs per call | Story 6-0b |
| `event_bus_throughput.rs` | `EventBus::emit_domain` single-threaded throughput | > 100k events/sec | Story 6-0a |
| `approval_runtime_fast_path.rs` | `ApprovalRuntime::request` with session-allow fast path | P99 < 10 µs | Story 6-0c |
| `plan_injector_cadence.rs` | `DefaultPlanInjector::pre_turn` overhead | < 1 µs per call | Story 6-0d |

## Running

```bash
# One subject:
cargo test --test scheduler_overhead -- --ignored --nocapture

# All perf tests (ignored so normal test runs skip them):
cargo test --tests perf -- --ignored
```

## Relationship to Stories

Each perf test file is owned by a specific story. Story ACs that specify a performance bound cross-reference the corresponding perf-test file. Perf-tests land alongside the implementing story (not pre-authored as skeletons — premature).

---

*Directory established 2026-04-24 per research §5.14 (dev-workflow additions) and Story 6-0b NFR.*
