use rustain::adapters::tui::color_detect::{ColorCapability, detect_color_capability};

// Note: These tests manipulate environment variables which is inherently
// not thread-safe. We use serial execution via unique test names.
// Each test saves/restores env vars to avoid interference.
// Rust 2024 edition requires unsafe blocks for env var mutation.

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
            // SAFETY: These tests run single-threaded (cargo test -- --test-threads=1 recommended)
            unsafe { std::env::remove_var(name) };
        }
        Self { vars }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (name, val) in &self.vars {
            match val {
                // SAFETY: Restoring original env state
                Some(v) => unsafe { std::env::set_var(name, v) },
                None => unsafe { std::env::remove_var(name) },
            }
        }
    }
}

#[test]
fn test_detect_truecolor() {
    let _guard = EnvGuard::new(&["NO_COLOR", "COLORTERM", "TERM"]);
    // SAFETY: Test env var manipulation, single-threaded test
    unsafe { std::env::set_var("COLORTERM", "truecolor") };
    assert_eq!(detect_color_capability(), ColorCapability::TrueColor);
}

#[test]
fn test_detect_truecolor_24bit() {
    let _guard = EnvGuard::new(&["NO_COLOR", "COLORTERM", "TERM"]);
    unsafe { std::env::set_var("COLORTERM", "24bit") };
    assert_eq!(detect_color_capability(), ColorCapability::TrueColor);
}

#[test]
fn test_detect_256color() {
    let _guard = EnvGuard::new(&["NO_COLOR", "COLORTERM", "TERM"]);
    unsafe { std::env::set_var("TERM", "xterm-256color") };
    assert_eq!(detect_color_capability(), ColorCapability::Color256);
}

#[test]
fn test_detect_color16_default() {
    let _guard = EnvGuard::new(&["NO_COLOR", "COLORTERM", "TERM"]);
    assert_eq!(detect_color_capability(), ColorCapability::Color16);
}

#[test]
fn test_detect_monochrome() {
    let _guard = EnvGuard::new(&["NO_COLOR", "COLORTERM", "TERM"]);
    unsafe { std::env::set_var("NO_COLOR", "1") };
    assert_eq!(detect_color_capability(), ColorCapability::Monochrome);
}

#[test]
fn test_no_color_takes_priority() {
    let _guard = EnvGuard::new(&["NO_COLOR", "COLORTERM", "TERM"]);
    unsafe { std::env::set_var("NO_COLOR", "1") };
    unsafe { std::env::set_var("COLORTERM", "truecolor") };
    // NO_COLOR should win over COLORTERM
    assert_eq!(detect_color_capability(), ColorCapability::Monochrome);
}
