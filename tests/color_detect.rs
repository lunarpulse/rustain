use std::sync::Mutex;

use rustain::adapters::tui::color_detect::{ColorCapability, detect_color_capability};

// Note: These tests manipulate environment variables which is inherently
// not thread-safe. We use a global mutex to serialize all env-dependent tests.
// Each test saves/restores env vars to avoid interference.
// Rust 2024 edition requires unsafe blocks for env var mutation.

/// Global mutex to serialize tests that manipulate environment variables.
/// Cargo runs tests in parallel by default — without this, one test's
/// `set_var("COLORTERM", "truecolor")` can leak into another test's assertion.
static ENV_MUTEX: Mutex<()> = Mutex::new(());

struct EnvGuard {
    vars: Vec<(String, Option<String>)>,
}

impl EnvGuard {
    fn new(var_names: &[&str]) -> Self {
        let vars = var_names
            .iter()
            .map(|name| {
                let val = std::env::var(name).ok();
                (name.to_string(), val)
            })
            .collect();
        // Clear all tracked vars
        for name in var_names {
            // SAFETY: These tests hold ENV_MUTEX, ensuring single-threaded env access
            unsafe { std::env::remove_var(name) };
        }
        Self { vars }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (name, val) in &self.vars {
            match val {
                // SAFETY: Restoring original env state, still under ENV_MUTEX
                Some(v) => unsafe { std::env::set_var(name, v) },
                None => unsafe { std::env::remove_var(name) },
            }
        }
    }
}

// Covers: UX-DR5 (color detection and degradation)
#[test]
fn test_detect_truecolor() {
    let _lock = ENV_MUTEX.lock().unwrap();
    let _guard = EnvGuard::new(&["NO_COLOR", "COLORTERM", "TERM"]);
    // SAFETY: Test env var manipulation, serialized by ENV_MUTEX
    unsafe { std::env::set_var("COLORTERM", "truecolor") };
    assert_eq!(detect_color_capability(), ColorCapability::TrueColor);
}

// Covers: UX-DR5 (color detection and degradation)
#[test]
fn test_detect_truecolor_24bit() {
    let _lock = ENV_MUTEX.lock().unwrap();
    let _guard = EnvGuard::new(&["NO_COLOR", "COLORTERM", "TERM"]);
    unsafe { std::env::set_var("COLORTERM", "24bit") };
    assert_eq!(detect_color_capability(), ColorCapability::TrueColor);
}

// Covers: UX-DR5 (color detection and degradation)
#[test]
fn test_detect_256color() {
    let _lock = ENV_MUTEX.lock().unwrap();
    let _guard = EnvGuard::new(&["NO_COLOR", "COLORTERM", "TERM"]);
    unsafe { std::env::set_var("TERM", "xterm-256color") };
    assert_eq!(detect_color_capability(), ColorCapability::Color256);
}

// Covers: UX-DR5 (color detection and degradation)
#[test]
fn test_detect_color16_default() {
    let _lock = ENV_MUTEX.lock().unwrap();
    let _guard = EnvGuard::new(&["NO_COLOR", "COLORTERM", "TERM"]);
    assert_eq!(detect_color_capability(), ColorCapability::Color16);
}

// Covers: UX-DR5 (color detection and degradation)
#[test]
fn test_detect_monochrome() {
    let _lock = ENV_MUTEX.lock().unwrap();
    let _guard = EnvGuard::new(&["NO_COLOR", "COLORTERM", "TERM"]);
    unsafe { std::env::set_var("NO_COLOR", "1") };
    assert_eq!(detect_color_capability(), ColorCapability::Monochrome);
}

// Covers: UX-DR5 (color detection and degradation)
#[test]
fn test_no_color_takes_priority() {
    let _lock = ENV_MUTEX.lock().unwrap();
    let _guard = EnvGuard::new(&["NO_COLOR", "COLORTERM", "TERM"]);
    unsafe { std::env::set_var("NO_COLOR", "1") };
    unsafe { std::env::set_var("COLORTERM", "truecolor") };
    // NO_COLOR should win over COLORTERM
    assert_eq!(detect_color_capability(), ColorCapability::Monochrome);
}
