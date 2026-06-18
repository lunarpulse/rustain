//! Startup panic hook test harness (Story 13.7, party-mode NQ2).
//!
//! A separate `[[bin]]` (NOT env-gated dead code in the production binary) so
//! the startup panic hook can be exercised end-to-end via a REAL subprocess
//! panic. Behavior is selected by the `STARTUP_PANIC_MODE` env var:
//!
//! - `startup` (default): install ONLY the startup hook, then panic — verifies
//!   `~/.rustain/panic.log` is written (AC1, AC2, AC3).
//! - `both`: install the startup hook THEN the TUI hook (which sets the
//!   supersession flag), then panic — verifies `panic.log` is NOT written
//!   (AC4 — exactly one hook chain, no double writes).
//!
//! `RUSTAIN_DATA_DIR` (exported by the spawning test) controls where
//! `panic.log` lands. The process is EXPECTED to exit non-zero on the panic.

fn main() {
    let mode = std::env::var("STARTUP_PANIC_MODE").unwrap_or_else(|_| "startup".to_string());

    // AC1 — startup hook FIRST, before anything that could panic.
    rustain::infrastructure::signals::install_startup_panic_hook();

    if mode == "both" {
        // AC4 — the TUI hook installs second and supersedes the startup hook via
        // the AtomicBool flag, so a later panic must NOT write panic.log.
        rustain::infrastructure::signals::install_panic_hook();
    }

    panic!("STARTUP_PANIC_SENTINEL_42");
}
