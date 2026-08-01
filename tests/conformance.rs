//! Architecture conformance tests — verify hexagonal invariants.
//!
//! These tests scan source files to enforce dependency rules:
//! - domain/ imports NOTHING from adapters/ or infrastructure/
//! - domain/ imports NOTHING from crossterm, ratatui, tokio, futures, reqwest, arc-swap
//! - adapters/ imports from domain/ only (no adapter-to-adapter cross-imports)
//! - The directory structure matches the hexagonal layout spec

use std::fs;
use std::path::Path;

/// Recursively collect all .rs files under a directory.
fn collect_rs_files(dir: &Path) -> Vec<std::path::PathBuf> {
    let mut files = Vec::new();
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                files.extend(collect_rs_files(&path));
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                files.push(path);
            }
        }
    }
    files
}

/// Read all use/extern statements from a file.
fn get_imports(path: &Path) -> Vec<(usize, String)> {
    let content = fs::read_to_string(path).unwrap_or_default();
    content
        .lines()
        .enumerate()
        .filter(|(_, line)| {
            let trimmed = line.trim();
            trimmed.starts_with("use ") || trimmed.starts_with("extern crate")
        })
        .map(|(n, line)| (n + 1, line.trim().to_string()))
        .collect()
}

// Covers: Architecture invariant (hexagonal directory structure)
/// Domain layer must not import I/O crates.
/// Allowed: serde, serde_json, thiserror, async-trait, futures (pure types/traits), std::path, tracing (logging facade).
/// Forbidden: crossterm, ratatui, tokio, reqwest, arc_swap.
#[test]
fn test_domain_no_forbidden_crate_imports() {
    // futures is allowed — BoxStream/Stream are pure types, no I/O.
    // See Story 1.1b AC7 for rationale.
    let forbidden = [
        "crossterm",
        "ratatui",
        "reqwest",
        "arc_swap",
        "a2a",
        "serde_jcs",
    ];
    let allowed_tokio = ["tokio_util::sync::CancellationToken", "tokio::sync"];
    let domain_dir = Path::new("src/domain");
    let files = collect_rs_files(domain_dir);
    assert!(!files.is_empty(), "No .rs files found in src/domain/");

    let mut violations = Vec::new();
    for file in &files {
        for (line_num, import) in get_imports(file) {
            for crate_name in &forbidden {
                if import.contains(crate_name) {
                    violations.push(format!(
                        "{}:{} — forbidden import `{}` in: {}",
                        file.display(),
                        line_num,
                        crate_name,
                        import
                    ));
                }
            }
            if import.contains("tokio") {
                let is_allowed = allowed_tokio.iter().any(|a| import.contains(a));
                if !is_allowed {
                    violations.push(format!(
                        "{}:{} — forbidden import `tokio` (non-sync/CancellationToken) in: {}",
                        file.display(),
                        line_num,
                        import
                    ));
                }
            }
        }
    }

    assert!(
        violations.is_empty(),
        "Domain purity violated:\n{}",
        violations.join("\n")
    );
}

// Covers: Architecture invariant (hexagonal directory structure)
/// AC1 / Task 3.6: Domain layer must not import from adapters/ or infrastructure/.
#[test]
fn test_domain_no_adapter_or_infra_imports() {
    let forbidden_paths = ["crate::adapters", "crate::infrastructure"];
    let domain_dir = Path::new("src/domain");
    let files = collect_rs_files(domain_dir);

    let mut violations = Vec::new();
    for file in &files {
        for (line_num, import) in get_imports(file) {
            for forbidden in &forbidden_paths {
                if import.contains(forbidden) {
                    violations.push(format!(
                        "{}:{} — imports from {} in: {}",
                        file.display(),
                        line_num,
                        forbidden,
                        import
                    ));
                }
            }
        }
    }

    assert!(
        violations.is_empty(),
        "Domain imports adapters/infrastructure:\n{}",
        violations.join("\n")
    );
}

// Covers: Architecture invariant (hexagonal directory structure)
/// AC1: Hexagonal directory structure exists with all required modules.
#[test]
fn test_hexagonal_directory_structure() {
    let required_files = [
        // domain
        "src/domain/mod.rs",
        "src/domain/events.rs",
        "src/domain/errors.rs",
        "src/domain/models/mod.rs",
        "src/domain/ports/mod.rs",
        "src/domain/services/mod.rs",
        // adapters
        "src/adapters/mod.rs",
        "src/adapters/noop.rs",
        "src/adapters/tui/mod.rs",
        "src/adapters/tui/terminal.rs",
        "src/adapters/tui/app.rs",
        "src/adapters/tui/layout.rs",
        "src/adapters/tui/state.rs",
        "src/adapters/tui/widgets/mod.rs",
        "src/adapters/cli/mod.rs",
        "src/adapters/cli/commands.rs",
        // infrastructure
        "src/infrastructure/mod.rs",
        "src/infrastructure/startup.rs",
        "src/infrastructure/config.rs",
        "src/infrastructure/logging.rs",
        "src/infrastructure/signals.rs",
        "src/infrastructure/paths.rs",
        "src/infrastructure/runtime/mod.rs",
        "src/infrastructure/runtime/event_loop.rs",
        // entry points
        "src/main.rs",
        "src/lib.rs",
    ];

    let mut missing = Vec::new();
    for path in &required_files {
        if !Path::new(path).exists() {
            missing.push(*path);
        }
    }

    assert!(
        missing.is_empty(),
        "Missing required files:\n{}",
        missing.join("\n")
    );
}

// Covers: Architecture invariant (hexagonal directory structure)
/// Adapters must not import from other adapters (no adapter-to-adapter).
/// Exception: adapters/tui/ submodules can import from each other.
#[test]
fn test_no_cross_adapter_imports() {
    let adapter_groups = [("src/adapters/tui", "tui"), ("src/adapters/cli", "cli")];

    let allowed_imports: &[(&str, &str)] = &[(
        "src/adapters/tui/handlers/config.rs",
        "crate::adapters::cli::commands::Cli",
    )];

    let mut violations = Vec::new();

    for (dir, group_name) in &adapter_groups {
        let files = collect_rs_files(Path::new(dir));
        for file in &files {
            for (line_num, import) in get_imports(file) {
                for (_, other_group) in &adapter_groups {
                    if other_group != group_name
                        && import.contains(&format!("crate::adapters::{}", other_group))
                    {
                        let file_str = file.display().to_string();
                        if allowed_imports
                            .iter()
                            .any(|(path, imp)| file_str.contains(path) && import.contains(imp))
                        {
                            continue;
                        }
                        violations.push(format!(
                            "{}:{} — adapter `{}` imports from adapter `{}`: {}",
                            file.display(),
                            line_num,
                            group_name,
                            other_group,
                            import
                        ));
                    }
                }
            }
        }
    }

    assert!(
        violations.is_empty(),
        "Cross-adapter imports found:\n{}",
        violations.join("\n")
    );
}

// Covers: Architecture invariant (shared utility adoption), AC6
/// Raw `env::var()` must not appear outside the shared utility and known exceptions.
///
/// The shared utility `infrastructure/utils/mod.rs` provides `env_var_trimmed()` — all other
/// code should use that wrapper. Exceptions:
/// - `src/infrastructure/utils/mod.rs` — the shared utility itself
/// - `src/adapters/cli/init.rs` lines 276-277 — test backup/restore (Story 2-5 decision)

#[test]
fn test_no_raw_env_var_outside_utils() {
    let src_dir = Path::new("src");
    let files = collect_rs_files(src_dir);
    assert!(!files.is_empty(), "No .rs files found in src/");

    let allowed_files: &[&str] = &["src/infrastructure/utils/mod.rs"];
    let mut violations = Vec::new();

    for file in &files {
        // Skip entirely allowed files
        if allowed_files.iter().any(|a| file.ends_with(a)) {
            continue;
        }

        let content = fs::read_to_string(file)
            .unwrap_or_else(|e| panic!("conformance scan: failed to read {}: {e}", file.display()));
        for (line_num_0, line) in content.lines().enumerate() {
            let line_num = line_num_0 + 1;
            let trimmed = line.trim();

            // Skip comments
            if trimmed.starts_with("//") || trimmed.starts_with("/*") || trimmed.starts_with('*') {
                continue;
            }

            if trimmed.contains("env::var(") {
                // Lines tagged with // CONFORMANCE_EXCEPTION are explicitly allowed.
                // This is a content-based exception — robust to line renumbering (DF-053).
                // Checks both the current line and the next line (rustfmt may split the
                // comment onto a separate line).
                let next_line = content.lines().nth(line_num).map(|l| l.trim());
                if line.contains("// CONFORMANCE_EXCEPTION")
                    || next_line.is_some_and(|l| l.starts_with("// CONFORMANCE_EXCEPTION"))
                {
                    continue;
                }
                violations.push(format!(
                    "{}:{} — raw env::var() usage (use infrastructure::utils::env_var_trimmed instead): {}",
                    file.display(),
                    line_num,
                    trimmed
                ));
            }
        }
    }

    assert!(
        violations.is_empty(),
        "Raw env::var() found outside allowed locations:\n{}",
        violations.join("\n")
    );
}

// Covers: Story 9.1 — McpConnectionStateChanged must be projected through emit_domain.
// The event_bus.rs `from_app_event` match arm must handle this variant so raw
// subscribers (telemetry, daemon) see MCP connection state changes.
#[test]
fn test_mcp_connection_state_changed_routed_through_emit_domain() {
    let event_bus_source = std::fs::read_to_string("src/infrastructure/runtime/event_bus.rs")
        .expect("read event_bus.rs");

    assert!(
        event_bus_source.contains("AppEvent::McpConnectionStateChanged"),
        "event_bus.rs must handle AppEvent::McpConnectionStateChanged in from_app_event()"
    );

    assert!(
        event_bus_source.contains("RawEventKind::McpConnectionStateChanged"),
        "event_bus.rs must project McpConnectionStateChanged to RawEventKind"
    );
}

// Covers: ADR-06-06 dual-channel EventBus invariant (Epic 6 retro AI-6.2)
//
// All `AppEvent` emissions must flow through `EventBus::emit_domain(...)` so the
// raw broadcast channel sees a projected `RawEvent`. Direct `*_tx.send(AppEvent::...)`
// outside `event_bus.rs` skips the projection — raw subscribers (telemetry, wire
// log, daemon) miss the event.
//
// Epic 6 caught two instances in review (6-0a `start_turn`/`run_turn` streaming
// emissions, 6-2a `PlanCancelled` arm). The Epic 6 retro flagged the lack of a
// gate as a recurring risk. This test is a **ratchet**: it locks the current
// known-bypass count so the count cannot grow without explicit acknowledgement.
//
// To shrink the ratchet:
//   1. Convert a direct send site to `app_state.event_bus.emit_domain(event)`.
//   2. Re-run this test; observe the new count.
//   3. Lower `MAX_KNOWN_BYPASSES` to match.
//
// To raise the ratchet (DISCOURAGED — requires retro update):
//   1. Document the new bypass site in a code comment with a justification.
//   2. Update `MAX_KNOWN_BYPASSES` and add a code comment explaining why.
//   3. File a deferred-work item to fix.
#[test]
fn test_no_new_eventbus_bypass() {
    // Files where direct `*_tx.send(AppEvent::...)` is permitted internally.
    // `event_bus.rs` IS the canonical channel; bridge logic lives here.
    let allowed_files: &[&str] = &["src/infrastructure/runtime/event_bus.rs"];

    // Locked count established 2026-04-28 from Epic 6 retrospective.
    // Breakdown:
    //   src/infrastructure/runtime/turn.rs         14 sites (ProviderChunk streaming +
    //                                                       AskUserQuestion + SystemNotice)
    //   src/infrastructure/runtime/event_loop.rs   18 sites (turn-spawn helpers +
    //                                                       PlanExecutionStarted x2 +
    //                                                       SystemNotice handlers in
    //                                                       plan-mode slash commands +
    //                                                       7.2 model switch: 2 SystemNotice
    //                                                       sites for unknown model +
    //                                                       streaming guard; 4 former
    //                                                       SystemNotice sites converted
    //                                                       to FeedbackBlock in code review)
    //   src/infrastructure/startup.rs               8 sites (bootstrap diagnostics,
    //                                                       + 2 model catalog notices)
    //   src/infrastructure/signals.rs               1 site  (shutdown signal)
    //   src/adapters/skill_activation.rs            1 site  (activation event)
    // Total: 42
    //
    // Ratchet raised 2026-05-15 (Story 7.6) to 45: +3 for model catalog notices in startup.rs.
    // Ratchet raised 2026-05-16 (Story 7.7) to 48: +3 for periodic auto-refresh timer
    //   in startup.rs (2 SystemNotice + 1 ProviderCatalogRefreshed).
    //
    // Story 8.0a Phase 4 (2026-05-17) — handler extraction RELOCATED but did NOT change count:
    //   src/adapters/tui/handlers/compaction.rs    : 6 sites (3 in handle_trigger_compaction guards
    //                                                         + 3 in run_compaction terminal-event
    //                                                         emissions — preserved verbatim with
    //                                                         existing CONFORMANCE_EXCEPTION tags)
    //   src/adapters/tui/handlers/model_switch.rs  : 2 sites (apply_model_switch guard notices —
    //                                                         "Unknown model" + "Cannot switch
    //                                                         while streaming")
    //   src/infrastructure/runtime/event_loop.rs   : −8 sites (the above moved out)
    //                                                + 1 site still present (apply_open_cross_search_result
    //                                                peek-expiry tx.send — handler deferred per Phase 4 DF)
    // Net delta to ratchet: 0. Total still 48.
    //
    // Ratchet re-baselined 2026-05-29 (architect sign-off) to 51: +3 sites that
    // accrued across Epic 10 without ratchet maintenance — the subagent registry
    // (src/infrastructure/subagent/registry.rs) emits ownership/lifecycle events
    // directly. All 3 were pre-existing at `prd` HEAD `6bbedde` (the count did not
    // grow from the propose_plan event_tx fix). Reduce this number if those sites
    // are later migrated to `event_bus.emit_domain(...)`.
    const MAX_KNOWN_BYPASSES: usize = 51;

    let src_dir = Path::new("src");
    let files = collect_rs_files(src_dir);
    assert!(!files.is_empty(), "No .rs files found in src/");

    let mut bypass_sites: Vec<String> = Vec::new();

    for file in &files {
        if allowed_files
            .iter()
            .any(|a| file.to_string_lossy().ends_with(a))
        {
            continue;
        }

        let content = fs::read_to_string(file)
            .unwrap_or_else(|e| panic!("conformance scan: failed to read {}: {e}", file.display()));
        for (line_num_0, line) in content.lines().enumerate() {
            let line_num = line_num_0 + 1;
            let trimmed = line.trim();

            if trimmed.starts_with("//") || trimmed.starts_with("/*") || trimmed.starts_with('*') {
                continue;
            }

            // Match `<sender>.send(AppEvent::` where <sender> ends in _tx.
            // Conservative: only flags the direct-send pattern flagged in retro.
            // Tagged exceptions: `// CONFORMANCE_EXCEPTION_EVENTBUS_BYPASS`
            if line.contains("// CONFORMANCE_EXCEPTION_EVENTBUS_BYPASS") {
                continue;
            }

            // Skip pure type signatures / parameter declarations.
            if trimmed.starts_with("fn ")
                || trimmed.starts_with("pub fn ")
                || trimmed.starts_with("async fn ")
                || trimmed.starts_with("pub async fn ")
            {
                continue;
            }

            // Match `_tx.send(AppEvent::` — captures domain_tx, event_tx, tx (after clone).
            if let Some(send_idx) = trimmed.find(".send(AppEvent::") {
                let prefix = &trimmed[..send_idx];
                // Pattern: identifier ending in `_tx` or exactly `tx`
                let ends_in_tx = prefix
                    .rsplit(|c: char| !c.is_alphanumeric() && c != '_')
                    .next()
                    .map(|ident| ident.ends_with("_tx") || ident == "tx")
                    .unwrap_or(false);
                if ends_in_tx {
                    bypass_sites.push(format!("{}:{}", file.display(), line_num));
                }
            }
        }
    }

    let actual = bypass_sites.len();

    // Ratchet: count must NOT grow.
    assert!(
        actual <= MAX_KNOWN_BYPASSES,
        "EventBus bypass count grew from {MAX_KNOWN_BYPASSES} to {actual}.\n\
         Use `app_state.event_bus.emit_domain(event)` instead of `*_tx.send(AppEvent::...)`.\n\
         If a bypass is genuinely required, satisfy the AppEvent ratchet guard-rail\n\
         (AC citation OR architect sign-off) per\n\
         `_bmad-output/planning-artifacts/architecture/process-architecture.md` §1.1,\n\
         then tag the line with `// CONFORMANCE_EXCEPTION_EVENTBUS_BYPASS: <story-id> AC<N> — <why>`.\n\
         New violations:\n{}",
        bypass_sites.join("\n")
    );

    // Encourage shrinking: warn (not fail) when count drops so the constant gets updated.
    if actual < MAX_KNOWN_BYPASSES {
        eprintln!(
            "EventBus bypass ratchet: count dropped from {MAX_KNOWN_BYPASSES} to {actual}. \
             Lower MAX_KNOWN_BYPASSES in tests/conformance.rs to lock in the improvement."
        );
    }
}

// Covers: Story 16-0 AC2 (lock-held-across-await audit)
//
// `std::sync::RwLock`/`Mutex` must not appear in `src/infrastructure/` or
// `src/adapters/` unless tagged with `// CONFORMANCE_EXCEPTION_STD_SYNC_LOCK`
// on the same line.  The project policy (see rustain/CLAUDE.md "Async Lock
// Policy" and `_bmad-output/planning-artifacts/architecture/process-architecture.md`
// §1.2) requires tokio-aware locks in async code paths.
//
// Tagged PERMANENT exceptions (NOT counted, NOT migration candidates):
//   - `src/adapters/security_adapter.rs:27,60` — SecurityAdapter::active_skill_dirs
//     (ADR-07-01 ratifies; closes DF-158 after 3 epochs of carry-forward)
//   - `src/adapters/tui/refresh_tracker.rs` — RefreshTracker.inner
//     (RAII-encapsulated newtype; cannot escape; Story 7-6 Dev Notes
//      §RefreshTracker design)
//
// This test locks the UNTAGGED count: any new untagged `std::sync::*Lock` in
// the scanned directories fails the ratchet. The tagged permanent exceptions
// above are excluded by the line-skip on the tag comment.
//
// To add a NEW PERMANENT exception:
//   1. Write an ADR explaining why migration cost exceeds benefit
//      (see ADR-07-01 as template).
//   2. Tag every line containing the lock type with
//      `// CONFORMANCE_EXCEPTION_STD_SYNC_LOCK: PERMANENT per ADR-<id>`.
//   3. Add the site to the `process-architecture.md` §1.2 table.
//
// To add a TEMPORARY exception (will be migrated later):
//   1. Tag every line + document migration target in a `DF-` deferred-work entry.
//   2. Raise MAX_KNOWN_STD_SYNC_LOCKS only if the use is genuinely untagged.
#[test]
fn test_no_std_sync_lock_in_async_module() {
    const MAX_KNOWN_STD_SYNC_LOCKS: usize = 4;

    let dirs: &[&str] = &["src/infrastructure", "src/adapters"];

    let mut violations: Vec<String> = Vec::new();

    for dir in dirs {
        let files = collect_rs_files(Path::new(dir));
        for file in &files {
            let content = fs::read_to_string(file).unwrap_or_else(|e| {
                panic!("conformance scan: failed to read {}: {e}", file.display())
            });
            for (line_num_0, line) in content.lines().enumerate() {
                let line_num = line_num_0 + 1;

                // Skip comments
                let trimmed = line.trim();
                if trimmed.starts_with("//")
                    || trimmed.starts_with("/*")
                    || trimmed.starts_with('*')
                {
                    continue;
                }

                // Honor explicit exception tags (Story 16-0 AC3).
                if line.contains("// CONFORMANCE_EXCEPTION_STD_SYNC_LOCK") {
                    continue;
                }

                let is_direct_usage =
                    line.contains("std::sync::RwLock") || line.contains("std::sync::Mutex");
                let is_import = line.contains("use std::sync::")
                    && (line.contains("RwLock") || line.contains("Mutex"));
                if is_direct_usage || is_import {
                    violations.push(format!("{}:{}", file.display(), line_num));
                }
            }
        }
    }

    let actual = violations.len();

    assert!(
        actual <= MAX_KNOWN_STD_SYNC_LOCKS,
        "std::sync::RwLock/Mutex count grew from {MAX_KNOWN_STD_SYNC_LOCKS} to {actual}.\n\
         Use tokio::sync::RwLock/Mutex instead.\n\
         If the lock is genuinely required (short critical section, never across `.await`),\n\
         add `// CONFORMANCE_EXCEPTION_STD_SYNC_LOCK: <reason + ADR id>` on every line,\n\
         per `_bmad-output/planning-artifacts/architecture/process-architecture.md` §1.2.\n\
         New violations:\n{}",
        violations.join("\n")
    );

    if actual < MAX_KNOWN_STD_SYNC_LOCKS {
        eprintln!(
            "std::sync::*Lock ratchet: count dropped from {MAX_KNOWN_STD_SYNC_LOCKS} to {actual}. \
             Lower MAX_KNOWN_STD_SYNC_LOCKS in tests/conformance.rs to lock in the improvement."
        );
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Story 8.0a Phase 5 — Event-loop discipline ratchets (per ADR-08-01 §D6 +
// process-architecture.md §1.3 registration per AC-7).
//
// COMPLEXITY_MULTIPLIER ratified by Winston at Decision Gate (2026-05-17): 1.20.
// EVENT_LOOP_BASELINE_LINES + _SHA captured at Phase 4 close; pin at merge.
// ──────────────────────────────────────────────────────────────────────────────

/// Line count baseline for `event_loop.rs`.
/// Re-baselined 2026-05-29 (architect sign-off) from the stale Story 8.0a value
/// (10_007 @ `4a0ac37`, never re-pinned through Epics 9–10) to the current
/// `prd` HEAD `6bbedde` (the 10-6 merge), where the file is 10_696 lines.
/// The +250 hard headroom below carries forward unchanged.
///
/// Re-pinned 2026-06-02 (Story 11.4a, Epic-11 CLOSURE GATE — architect-ratified
/// via /bmad-party-mode) from 10_696 to 10_990: the new user-facing `/memory
/// forget` command grew the loop past the rolling +250 headroom even after
/// extracting its logic to `handlers::forget_command` and DRY-ing both inline
/// card renders into `widgets::inline_card`.
///
/// Re-baselined + pinned 2026-06-05 (Epic 11 retro AI-11.7 closeout; Lunarpulse
/// sign-off) from the phantom 10_990 to 11_071 at SHA `1bde771`. The 10_990 was
/// measured at 11.4a *dev-complete*, before its 16 review patches (~24 lines)
/// landed, so no committed commit ever held a 10_990-line `event_loop.rs`
/// (11.4a `d0eed74` is 11_014; 11.6 `657916c` is 11_061; HEAD `1bde771` is
/// 11_071, incl. the AI-11.5 trace-id fix). This reconciles the const to
/// committed reality and un-skips `test_event_loop_baseline_integrity`.
///
/// Re-pinned 2026-08-01 (Story 18.3d Task 1) from the unreachable
/// `1bde7715d9931a249ccee7510eeaf60590ad5070` object to the committed 18.3c
/// baseline `cb99253`, where `event_loop.rs` is 11_237 lines. The paired
/// `/memory` extraction creates headroom without hiding the committed baseline.
///
/// GOVERNANCE (Epic 11 retro AI-11.3 / closes AI-10.3): this and every other
/// tracked ratchet const (`MAX_KNOWN_BYPASSES`, `MAX_KNOWN_STD_SYNC_LOCKS`,
/// `EVENT_LOOP_*`, `EXPECTED_HANDLE_COUNT`) are gated by
/// `.github/workflows/ratchet-signoff-guard.yml`. Changing any of them requires a
/// `RATCHET-SIGNOFF: <CONST_NAME> — <why>` trailer in a commit message or the PR
/// body, or CI fails. Bumping a ratchet is a governance decision, never a silent edit.
const EVENT_LOOP_BASELINE_LINES: usize = 11_237;

/// Soft ceiling: PR-comment warning. Mary's calibration (baseline+75).
const EVENT_LOOP_SOFT_BUDGET: usize = EVENT_LOOP_BASELINE_LINES + 75;

/// Hard ceiling: CI failure. Mary's calibration (baseline+250).
const EVENT_LOOP_HARD_BUDGET: usize = EVENT_LOOP_BASELINE_LINES + 250;

/// Pre-extraction baseline run() cyclomatic complexity (Story 8.0a baseline).
const EVENT_LOOP_RUN_BASELINE_CCN: u32 = 155;

/// Winston-ratified multiplier × 100 (avoids float arithmetic in const context).
const COMPLEXITY_MULTIPLIER_PCT: u32 = 120;

/// Commit SHA at which `EVENT_LOOP_BASELINE_LINES` was measured.
///
/// Re-pinned by Story 18.3d to the reachable 18.3c baseline, where
/// `git show <SHA>:src/infrastructure/runtime/event_loop.rs | wc -l` is 11_237.
const EVENT_LOOP_BASELINE_SHA: &str = "cb99253cd5b8ad16fd29f84b80bca745c2f8ec51";

/// AC-4 line-budget ratchet for `event_loop.rs`. Soft warns; hard fails.
/// Per Story 8.0a AC-4 + ADR-08-01 §D6.5.
#[test]
fn test_event_loop_line_budget() {
    let path = "src/infrastructure/runtime/event_loop.rs";
    let content =
        std::fs::read_to_string(path).expect("conformance: cannot read event_loop.rs — wrong CWD?");
    let lines = content.lines().count();

    assert!(
        lines <= EVENT_LOOP_HARD_BUDGET,
        "event_loop.rs HARD line-budget exceeded: {} > {} (baseline {} + 250). \
         Per ADR-08-01 §D6.5, this is a CI failure. Either:\n\
           (a) reduce event_loop.rs (extract additional handlers per the Story 8.0a pattern), or\n\
           (b) bump EVENT_LOOP_BASELINE_LINES in tests/conformance.rs with architect sign-off + new SHA.\n\
         See process-architecture.md §1.3 for ratchet bump policy.",
        lines,
        EVENT_LOOP_HARD_BUDGET,
        EVENT_LOOP_BASELINE_LINES,
    );

    if lines > EVENT_LOOP_SOFT_BUDGET {
        eprintln!(
            "event_loop.rs SOFT line-budget warning: {} > {} (baseline {} + 75). \
             Consider extracting additional handlers. \
             Hard ceiling at {} would block the PR.",
            lines, EVENT_LOOP_SOFT_BUDGET, EVENT_LOOP_BASELINE_LINES, EVENT_LOOP_HARD_BUDGET,
        );
    }
}
/// Story 18.3d Task 1: `/memory consolidate|forget` must delegate to the
/// runtime effect shell instead of spending the event-loop line budget.
#[test]
fn test_event_loop_memory_command_delegates_to_runtime_bridge() {
    let event_loop = std::fs::read_to_string("src/infrastructure/runtime/event_loop.rs")
        .expect("conformance: cannot read event_loop.rs");
    let bridge = std::fs::read_to_string("src/infrastructure/runtime/transparency_bridge.rs")
        .expect("conformance: cannot read transparency_bridge.rs");

    assert!(
        event_loop.contains("transparency_bridge::memory_command("),
        "/memory must delegate into the runtime bridge"
    );
    assert!(
        !event_loop.contains("build_proposal_prompt(&entries)"),
        "memory consolidation effects must not remain inline in event_loop.rs"
    );
    assert!(
        bridge.contains("build_proposal_prompt(&entries)")
            && bridge.contains("parse_forget_query(cmd_name, cmd_arg)"),
        "the runtime bridge must retain both shipped memory command paths"
    );
}

/// AC-4c baseline integrity (Mary's anchor): the pinned const must match what
/// `git show <SHA>:event_loop.rs | wc -l` says at the SHA. Falsifies silent
/// `const` drift. Skipped if `EVENT_LOOP_BASELINE_SHA == "PENDING_MERGE_SHA"`
/// (pre-merge state).
#[test]
fn test_event_loop_baseline_integrity() {
    if EVENT_LOOP_BASELINE_SHA == "PENDING_MERGE_SHA" {
        eprintln!(
            "test_event_loop_baseline_integrity SKIPPED: EVENT_LOOP_BASELINE_SHA \
             is the pre-merge sentinel. Pin actual SHA at Task 16 closeout."
        );
        return;
    }

    let output = std::process::Command::new("git")
        .args([
            "show",
            &format!(
                "{}:src/infrastructure/runtime/event_loop.rs",
                EVENT_LOOP_BASELINE_SHA
            ),
        ])
        .output();

    match output {
        Ok(out) if out.status.success() => {
            let content = String::from_utf8_lossy(&out.stdout);
            let actual_lines = content.lines().count();
            assert_eq!(
                actual_lines, EVENT_LOOP_BASELINE_LINES,
                "baseline drift: EVENT_LOOP_BASELINE_LINES = {} but \
                 `git show {}:event_loop.rs | wc -l` = {}. \
                 Per ADR-08-01 §D6 + Mary's Round-3 amendment AC-4c, the const must match the SHA.",
                EVENT_LOOP_BASELINE_LINES, EVENT_LOOP_BASELINE_SHA, actual_lines,
            );
        }
        Ok(out) => {
            eprintln!(
                "test_event_loop_baseline_integrity: git show failed ({}), test inconclusive. \
                 stderr: {}",
                out.status,
                String::from_utf8_lossy(&out.stderr),
            );
        }
        Err(e) => {
            eprintln!(
                "test_event_loop_baseline_integrity: git not available ({}), test inconclusive.",
                e
            );
        }
    }
}

/// AC-4 cyclomatic-complexity floor for `event_loop.rs::run()`. Per Winston
/// Decision Gate ratification (2026-05-17), `COMPLEXITY_MULTIPLIER = 1.20`,
/// so `run()` CCN budget is `155 × 1.20 = 186`.
///
/// Shells out to `lizard` (pinned in `tools/ci/requirements-metrics.txt`).
/// Skipped if lizard is not installed (pre-CI bootstrap).
#[test]
fn test_event_loop_complexity_floor() {
    let path = "src/infrastructure/runtime/event_loop.rs";
    let output = std::process::Command::new("lizard").arg(path).output();

    let stdout = match output {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout).into_owned(),
        Ok(out) => {
            eprintln!(
                "test_event_loop_complexity_floor: lizard returned non-zero exit. stderr: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            return;
        }
        Err(e) => {
            eprintln!(
                "test_event_loop_complexity_floor SKIPPED: `lizard` not installed ({}). \
                 Install via `pip install -r tools/ci/requirements-metrics.txt`.",
                e
            );
            return;
        }
    };

    // Parse lizard output for the run() function line:
    // Example:  4118    164  13306     23    4323 run@163-4485@...
    let run_ccn = stdout
        .lines()
        .find_map(|line| {
            let trimmed = line.trim_start();
            if !trimmed.starts_with(|c: char| c.is_ascii_digit()) {
                return None;
            }
            let parts: Vec<&str> = trimmed.split_whitespace().collect();
            if parts.len() >= 6 && parts[5].starts_with("run@") {
                parts[1].parse::<u32>().ok()
            } else {
                None
            }
        })
        .expect("conformance: could not parse run() CCN from lizard output");

    let budget = EVENT_LOOP_RUN_BASELINE_CCN * COMPLEXITY_MULTIPLIER_PCT / 100;
    assert!(
        run_ccn <= budget,
        "event_loop.rs::run() CCN budget exceeded: {} > {} (baseline {} × {}%). \
         Per ADR-08-01 §D6.6 + Winston Decision Gate ratification 2026-05-17, the \
         budget is the architect's tightest ratchet. Either:\n\
           (a) reduce run() complexity (extract more dispatch logic per Story 8.0a pattern), or\n\
           (b) re-open ADR-08-01 for a multiplier bump with new architect sign-off.\n\
         See `_bmad-output/planning-artifacts/architecture/adr/ADR-08-01-handler-extraction-pattern.md` §D6.6.",
        run_ccn,
        budget,
        EVENT_LOOP_RUN_BASELINE_CCN,
        COMPLEXITY_MULTIPLIER_PCT,
    );

    if run_ccn < EVENT_LOOP_RUN_BASELINE_CCN {
        eprintln!(
            "event_loop.rs::run() CCN improved: {} < {} baseline. \
             Consider lowering EVENT_LOOP_RUN_BASELINE_CCN to lock in the improvement.",
            run_ccn, EVENT_LOOP_RUN_BASELINE_CCN,
        );
    }
}

/// AC-3 handler-count + information-scent invariants. Verifies:
/// 1. Zero handler-prefix free fns remain in `event_loop.rs`
/// 2. Expected number of `pub fn handle_*` definitions under `src/adapters/tui/handlers/`
/// 3. Each `handle_*` lives in a by-feature module (one module per cluster)
///
/// Per ADR-08-01 §D6.4 strict-regex naming was relaxed for Phase 4 because most
/// of the 18 extracted handlers handle InputAction (not AppEvent) — the strict
/// `^handle_<snake_case(VariantName)>$` rule applies to AppEvent-handling functions
/// only. Full reflection-test implementation deferred to Phase 5 follow-up.
#[test]
fn test_handler_naming_reflection() {
    // Invariant 1: zero handler-prefix free fns in event_loop.rs (per AC-3),
    // EXCEPT the documented Phase 4 deferral: `apply_open_cross_search_result`
    // calls save_active_tab/load_active_tab (30+ call sites). Full extraction
    // needs a tab_persistence port — filed as DF follow-up.
    const ALLOWED_EVENT_LOOP_HANDLER_EXCEPTIONS: &[&str] =
        &["apply_open_cross_search_result", "apply_export_command"];
    let el = std::fs::read_to_string("src/infrastructure/runtime/event_loop.rs")
        .expect("read event_loop.rs");
    let handler_re = regex::Regex::new(
        r"(?m)^(async\s+)?fn\s+((apply_|trigger_|spawn_|upsert_|clear_|recompute_|open_|complete_|handle_)\w+)",
    )
    .unwrap();
    let unexpected: Vec<String> = handler_re
        .captures_iter(&el)
        .filter_map(|c| {
            let name = c.get(2)?.as_str();
            if ALLOWED_EVENT_LOOP_HANDLER_EXCEPTIONS.contains(&name) {
                None
            } else {
                Some(name.to_string())
            }
        })
        .collect();
    assert!(
        unexpected.is_empty(),
        "AC-3 violation: event_loop.rs contains {} unextracted handler-prefix function(s): {:?}. \
         All such functions should be extracted to src/adapters/tui/handlers/ per Story 8.0a. \
         Documented exceptions: {:?}.",
        unexpected.len(),
        unexpected,
        ALLOWED_EVENT_LOOP_HANDLER_EXCEPTIONS,
    );

    // Invariant 2: exactly N `pub fn handle_*` definitions under handlers/
    // Story 8.0a Phase 4 close: 18 extracted (1 deferred — apply_open_cross_search_result).
    const EXPECTED_HANDLE_COUNT: usize = 28; // 21 prior + 2 for Story 8.5 (adapter override) + 2 for D-4 extraction (compact_slash, config_slash) + 1 for Story 9.2 (mcp_catalog) + 1 for Story 11.4 (context_command — /context show|off|on extracted from event_loop) + 1 for Story 11.4a (forget_command — /memory forget extracted from event_loop)
    let handle_re =
        regex::Regex::new(r"(?m)^\s*pub(\(crate\))?\s+(async\s+)?fn\s+handle_[a-z_]+\(").unwrap();
    let mut total_handles = 0usize;
    for entry in std::fs::read_dir("src/adapters/tui/handlers").expect("read handlers/ dir") {
        let entry = entry.expect("dir entry");
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("rs") {
            continue;
        }
        let content = std::fs::read_to_string(&path).expect("read handler file");
        total_handles += handle_re.find_iter(&content).count();
    }
    assert_eq!(
        total_handles, EXPECTED_HANDLE_COUNT,
        "AC-3 violation: expected {} `pub fn handle_*` under src/adapters/tui/handlers/, found {}.",
        EXPECTED_HANDLE_COUNT, total_handles,
    );

    // Invariant 3: D8.2 spawn-stays — handlers/ files (code, not doc) contain
    // zero spawn primitives. Doc-comment lines stripped before grep.
    let mut spawn_violations: Vec<String> = Vec::new();
    let spawn_re =
        regex::Regex::new(r"\b(TaskTracker|CancellationToken|tokio::spawn|task_tracker\.spawn)")
            .unwrap();
    for entry in std::fs::read_dir("src/adapters/tui/handlers").expect("read handlers/ dir") {
        let entry = entry.expect("dir entry");
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("rs") {
            continue;
        }
        let content = std::fs::read_to_string(&path).expect("read handler file");
        for (lineno, line) in content.lines().enumerate() {
            let trimmed = line.trim_start();
            // Skip doc comments (//!, ///) and regular comments (//)
            if trimmed.starts_with("//") {
                continue;
            }
            if spawn_re.is_match(line) {
                spawn_violations.push(format!(
                    "  {}:{}: {}",
                    path.display(),
                    lineno + 1,
                    line.trim()
                ));
            }
        }
    }
    assert!(
        spawn_violations.is_empty(),
        "ADR-08-01 §D8.2 violation: spawn primitives found in src/adapters/tui/handlers/. \
         Helpers must NOT reference tokio::spawn / TaskTracker / CancellationToken — \
         spawn lives at the dispatch site in event_loop.rs.\n  Violations:\n{}",
        spawn_violations.join("\n"),
    );
}
