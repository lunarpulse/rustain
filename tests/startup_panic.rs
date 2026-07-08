//! Startup panic hook integration tests (Story 13.7 AC1–AC4).
//!
//! These spawn the `startup-panic-harness` [[bin]] so the startup panic hook is
//! exercised through a REAL subprocess panic — `std::panic::set_hook` is
//! process-global, so hook behavior CANNOT be tested in parallel inside a
//! single process (party-mode NQ2). All tests here are `#[serial]` to keep the
//! shared test process deterministic. The P0-C / P0-D gates read `signals.rs`
//! source at compile time to lock in the structural safety invariants.

use std::path::Path;

use serial_test::serial;
use tempfile::TempDir;

/// Locate the built `startup-panic-harness` `[[bin]]` next to the test exe.
/// Bins under `tests/` don't get `CARGO_BIN_EXE_<name>`, so resolve the
/// artifact by walking up from `current_exe()` (mirrors the `fake-mcp-server`
/// discovery in `integration_mcp_routing.rs`). Built by `cargo test` (all
/// targets) or `cargo build --bin startup-panic-harness`.
fn harness_binary() -> std::path::PathBuf {
    // Bins under tests/ don't get CARGO_BIN_EXE_<name>, so resolve the built
    // artifact next to the test binary (mirrors the fake-mcp-server discovery
    // in integration_mcp_routing.rs). Built by `cargo test` (all targets) or
    // `cargo build --bin startup-panic-harness`.
    let name = if cfg!(target_os = "windows") {
        "startup-panic-harness.exe"
    } else {
        "startup-panic-harness"
    };
    let exe_dir = std::env::current_exe()
        .expect("current exe")
        .parent()
        .expect("parent")
        .to_path_buf();
    for candidate in [
        exe_dir.join(name),
        exe_dir.parent().expect("deps parent").join(name),
    ] {
        if candidate.exists() {
            return candidate;
        }
    }
    panic!(
        "startup-panic-harness binary not found near {} — run `cargo build --bin startup-panic-harness`",
        exe_dir.display()
    );
}

/// Locate + run the harness binary, returning its captured output. `mode`
/// selects `startup` (startup hook only) or `both` (startup + TUI hook → the
/// TUI hook supersedes the startup hook). When `data_dir` is set it is exported
/// as `RUSTAIN_DATA_DIR`, overriding any inherited value so subprocess runs are
/// isolated from sibling tests that mutate the parent env.
fn run_harness(mode: &str, data_dir: Option<&Path>) -> std::process::Output {
    let bin = harness_binary();
    let mut cmd = std::process::Command::new(bin);
    cmd.env("STARTUP_PANIC_MODE", mode);
    if let Some(dir) = data_dir {
        cmd.env("RUSTAIN_DATA_DIR", dir);
    }
    cmd.output()
        .expect("spawn startup-panic-harness — was it built?")
}

/// AC1/AC2 / Task 4.4 — a startup-only panic writes `panic.log` with the
/// sentinel message, the "Startup Crash Report" header, a real backtrace
/// section, and the process exits non-zero.
#[test]
#[serial]
fn startup_panic_writes_panic_log_with_sentinel_header_and_backtrace() {
    let tmp = TempDir::new().unwrap();
    let output = run_harness("startup", Some(tmp.path()));
    assert!(
        !output.status.success(),
        "harness must exit non-zero on panic"
    );
    let panic_log = tmp.path().join("panic.log");
    assert!(panic_log.exists(), "panic.log must be written");
    let content = std::fs::read_to_string(&panic_log).unwrap();
    assert!(
        content.contains("Rustain Startup Crash Report"),
        "missing header sentinel"
    );
    assert!(
        content.contains("STARTUP_PANIC_SENTINEL_42"),
        "missing panic sentinel message"
    );
    assert!(content.contains("Backtrace:"), "missing backtrace header");
    let after = content.split("Backtrace:").nth(1).unwrap_or("");
    assert!(
        after.trim().len() > 10,
        "backtrace section must contain real frames"
    );
}

/// AC3 / Task 4.5 — stderr names the log path AND the reporting URL.
#[test]
#[serial]
fn startup_panic_stderr_names_log_path_and_reporting_url() {
    let tmp = TempDir::new().unwrap();
    let output = run_harness("startup", Some(tmp.path()));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("https://github.com/lunarpulse/rustain/issues"),
        "stderr must contain the reporting URL"
    );
    assert!(
        stderr.contains("panic.log"),
        "stderr must name the panic log path"
    );
    assert!(
        stderr.contains(tmp.path().to_str().unwrap()),
        "stderr must contain the resolved data dir path"
    );
}

/// AC4 / Task 4.6 (negative control) — when BOTH hooks are installed, the TUI
/// hook supersedes the startup hook, so a real panic must NOT write `panic.log`
/// (the TUI hook's `crash-{ts}.log` is written instead). This is the
/// differential: flag ON → file NOT written.
#[test]
#[serial]
fn both_hooks_real_panic_does_not_write_panic_log() {
    let tmp = TempDir::new().unwrap();
    let output = run_harness("both", Some(tmp.path()));
    assert!(
        !output.status.success(),
        "harness must exit non-zero on panic"
    );
    let panic_log = tmp.path().join("panic.log");
    assert!(
        !panic_log.exists(),
        "panic.log must NOT be written once the TUI hook has superseded the startup hook"
    );
}

/// AC4 / Task 4.6 (positive control) — the supersession flag guards the hook
/// CLOSURE, not the write function: with both hooks installed (flag = true),
/// calling `record_startup_panic` directly STILL writes `panic.log`. Together
/// with the negative control above this is the full differential. `#[serial]`
/// because `set_hook` is process-global; the default hook + env are restored.
#[test]
#[serial]
fn record_startup_panic_bypasses_supersession_flag() {
    let tmp = TempDir::new().unwrap();
    // SAFETY: #[serial] serializes env mutation; RUSTAIN_DATA_DIR is the
    // documented test seam for data_dir().
    unsafe {
        std::env::set_var("RUSTAIN_DATA_DIR", tmp.path());
    }
    // Install both hooks so the supersession flag is set to true (TUI hook's
    // first statement), matching the negative-control setup.
    rustain::infrastructure::signals::install_startup_panic_hook();
    rustain::infrastructure::signals::install_panic_hook();
    // Direct call must STILL write — the flag guards the closure, not this fn.
    let path = rustain::infrastructure::signals::record_startup_panic(
        "direct-call-marker",
        "marker-backtrace",
    )
    .expect("direct record must succeed even when superseded flag is true");
    let content = std::fs::read_to_string(&path).unwrap();
    assert!(content.contains("direct-call-marker"));
    assert!(content.contains("marker-backtrace"));
    // Cleanup — restore the default panic hook + clear the env so no state
    // leaks into sibling tests in this process.
    let _ = std::panic::take_hook();
    unsafe {
        std::env::remove_var("RUSTAIN_DATA_DIR");
    }
}

/// P0-B / Task 4.8 — graceful degradation: with a non-writable data dir the
/// startup hook cannot write `panic.log`, but stderr STILL carries the reporting
/// URL and the process exits non-zero (no hang, no abort). Uses a portable
/// non-writable path (a dir under a regular file cannot be created) rather than
/// a Unix-only `/dev/null` trick.
#[test]
#[serial]
fn non_writable_data_dir_still_reports_url_and_exits_nonzero() {
    let tmp = TempDir::new().unwrap();
    let blocker = tmp.path().join("blocker");
    std::fs::write(&blocker, b"x").unwrap();
    let bogus = blocker.join("sub"); // create_dir_all fails: parent is a file
    let output = run_harness("startup", Some(&bogus));
    assert!(
        !output.status.success(),
        "harness must exit non-zero even when the log write fails"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("https://github.com/lunarpulse/rustain/issues"),
        "stderr must still carry the reporting URL on write failure"
    );
}

// ── Structural conformance gates (P0-C / P0-D) ──────────────────────────────
//
// Read `signals.rs` at compile time and assert the startup hook keeps its
// safety invariants. Prevents a future refactor from silently dropping
// `catch_unwind` or copy-pasting `restore_terminal_raw` from the TUI hook.

/// Extract a top-level `fn <name>` body from source by brace matching. Robust
/// for the startup-hook functions because every `{`/`}` inside their string
/// literals is balanced (format placeholders like `{info}` / `{}` net to zero).
fn extract_fn_body(src: &str, name: &str) -> String {
    let needle = format!("fn {name}");
    let start = src
        .find(&needle)
        .unwrap_or_else(|| panic!("fn {name} not found in signals.rs"));
    let rel = src[start..].find('{').expect("opening brace");
    let brace_start = start + rel;
    let bytes = src.as_bytes();
    let mut depth: i32 = 0;
    let mut i = brace_start;
    while i < bytes.len() {
        match bytes[i] {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return src[start..=i].to_string();
                }
            }
            _ => {}
        }
        i += 1;
    }
    panic!("unterminated fn {name}");
}

/// P0-C / Task 4.9 — `install_startup_panic_hook` MUST wrap its I/O body in
/// `catch_unwind` (party-mode OQ2 — an OOM-induced double-panic must not abort
/// before the prior hook chains on stderr).
#[test]
fn install_startup_panic_hook_wraps_io_in_catch_unwind() {
    let src = include_str!("../src/infrastructure/signals.rs");
    let body = extract_fn_body(src, "install_startup_panic_hook");
    assert!(
        body.contains("catch_unwind"),
        "install_startup_panic_hook must wrap I/O in catch_unwind"
    );
}

/// P0-D / Task 4.10 — the startup hook functions MUST NOT call
/// `restore_terminal_raw` (no terminal setup has occurred at startup; calling
/// it would couple infrastructure to the TUI adapter for no reason).
#[test]
fn startup_hook_functions_do_not_call_restore_terminal_raw() {
    let src = include_str!("../src/infrastructure/signals.rs");
    let hook = extract_fn_body(src, "install_startup_panic_hook");
    let record = extract_fn_body(src, "record_startup_panic");
    assert!(
        !hook.contains("restore_terminal_raw"),
        "install_startup_panic_hook MUST NOT call restore_terminal_raw"
    );
    assert!(
        !record.contains("restore_terminal_raw"),
        "record_startup_panic MUST NOT call restore_terminal_raw"
    );
}
