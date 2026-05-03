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
    let forbidden = ["crossterm", "ratatui", "reqwest", "arc_swap"];
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

    let mut violations = Vec::new();

    for (dir, group_name) in &adapter_groups {
        let files = collect_rs_files(Path::new(dir));
        for file in &files {
            for (line_num, import) in get_imports(file) {
                // Check for imports from other adapter groups
                for (_, other_group) in &adapter_groups {
                    if other_group != group_name
                        && import.contains(&format!("crate::adapters::{}", other_group))
                    {
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
/// The shared utility `infrastructure/utils.rs` provides `env_var_trimmed()` — all other
/// code should use that wrapper. Exceptions:
/// - `src/infrastructure/utils.rs` — the shared utility itself
/// - `src/adapters/cli/init.rs` lines 276-277 — test backup/restore (Story 2-5 decision)
#[test]
fn test_no_raw_env_var_outside_utils() {
    let src_dir = Path::new("src");
    let files = collect_rs_files(src_dir);
    assert!(!files.is_empty(), "No .rs files found in src/");

    let allowed_files: &[&str] = &["src/infrastructure/utils.rs"];

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
                if line.contains("// CONFORMANCE_EXCEPTION") {
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
    //                                                       AskUserQuestion + SystemNotice
    //                                                       — flagged in 6-0a review as
    //                                                       action-item, not patched at
    //                                                       story close)
    //   src/infrastructure/runtime/event_loop.rs   16 sites (turn-spawn helpers +
    //                                                       PlanExecutionStarted x2 +
    //                                                       SystemNotice handlers in
    //                                                       plan-mode slash commands)
    //   src/infrastructure/startup.rs               4 sites (bootstrap diagnostics —
    //                                                       pre-event-loop, EventBus
    //                                                       not yet observable; +1 for
    //                                                       S16.8 AC14 mouse capture hint)
    //   src/adapters/toolset_adapter.rs             2 sites (tool execution streaming)
    //   src/infrastructure/signals.rs               1 site  (shutdown signal)
    //   src/adapters/skill_activation.rs            1 site  (activation event)
    // Total: 38
    //
    // To reduce: convert the call site to `event_bus.emit_domain(event)`.
    // Tracked in deferred-work as part of Epic 6 retro AI-6.2 follow-on.
    const MAX_KNOWN_BYPASSES: usize = 38;

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
        "EventBus bypass count grew from {MAX_KNOWN_BYPASSES} to {actual}. \
         Use `app_state.event_bus.emit_domain(event)` instead of `*_tx.send(AppEvent::...)`. \
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
// Policy") requires tokio-aware locks in async code paths.
//
// The SecurityAdapter uses `std::sync::RwLock` for `active_skill_dirs`
// (short critical sections, never across `.await`) — that use is tagged
// and excluded from the ratchet.  This test locks the count at zero:
// any new untagged `std::sync::*Lock` in the scanned directories fails
// the ratchet.
//
// To add a justified exception:
//   1. Tag every line containing the lock type with the comment tag.
//   2. Document the justification in a code comment (see SecurityAdapter
//      AC3 pattern in Story 16-0).
//   3. Raise MAX_KNOWN_STD_SYNC_LOCKS if the usage is truly unavoidable.
#[test]
fn test_no_std_sync_lock_in_async_module() {
    const MAX_KNOWN_STD_SYNC_LOCKS: usize = 0;

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
        "std::sync::RwLock/Mutex count grew from {MAX_KNOWN_STD_SYNC_LOCKS} to {actual}. \
         Use tokio::sync::RwLock/Mutex instead, or add // CONFORMANCE_EXCEPTION_STD_SYNC_LOCK \
         with justification (see Story 16-0 AC3). \
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
