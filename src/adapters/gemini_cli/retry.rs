//! Cloud Code Assist retry timing, translated from oh-my-pi utils/fetch-retry.ts
//! and ai/src/providers/google-gemini-cli.ts (MAX_RETRIES, BASE_DELAY_MS,
//! RATE_LIMIT_BUDGET_MS, EMPTY_STREAM_BASE_DELAY_MS).
//!
//! MIT License
//!
//! Copyright (c) 2025 Mario Zechner
//! Copyright (c) 2025-2026 Can Bölük
//! Copyright (c) 2026 Stencil Labs, Inc.
//!
//! Permission is hereby granted, free of charge, to any person obtaining a copy
//! of this software and associated documentation files (the "Software"), to deal
//! in the Software without restriction, including without limitation the rights
//! to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
//! copies of the Software, and to permit persons to whom the Software is
//! furnished to do so, subject to the following conditions:
//!
//! The above copyright notice and this permission notice shall be included in all
//! copies or substantial portions of the Software.
//!
//! THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
//! IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
//! FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
//! AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
//! LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
//! OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
//! SOFTWARE.

use std::time::Duration;

/// omp `MAX_RETRIES + 1`.
pub(super) const ATTEMPTS: u32 = 4;
/// omp `RATE_LIMIT_BUDGET_MS`: a hint above this returns the response.
pub(super) const MAX_DELAY_MS: u64 = 300_000;
/// omp `MAX_EMPTY_STREAM_RETRIES`.
pub(super) const EMPTY_RETRIES: u8 = 2;

/// omp `isRetryableStatus`; 401/403/404 are never transport-retried.
pub(super) fn retry_status(status: u16) -> bool {
    status == 408 || status == 429 || status >= 500
}

/// `BASE_DELAY_MS · 2^attempt` for the 0-based attempt that just failed, capped.
pub(super) fn default_delay_ms(attempt: u32) -> u64 {
    1000u64
        .saturating_mul(1u64 << attempt.min(32))
        .min(MAX_DELAY_MS)
}

/// `EMPTY_STREAM_BASE_DELAY_MS · 2^(n-1)` before the `n`-th (1-based) empty retry.
pub(super) fn empty_delay_ms(n: u8) -> u64 {
    500u64 << n.saturating_sub(1).min(16)
}

#[cfg(feature = "test-instrumentation")]
fn scale() -> f64 {
    crate::infrastructure::utils::env_var_trimmed("RUSTAIN_TEST_GEMINI_CLI_RETRY_SCALE")
        .and_then(|v| v.trim().parse::<f64>().ok())
        .filter(|v| v.is_finite() && *v >= 0.0)
        .unwrap_or(1.0)
}
#[cfg(not(feature = "test-instrumentation"))]
fn scale() -> f64 {
    1.0
}

/// A backoff wait, read per use.
pub(super) fn delay(ms: u64) -> Duration {
    Duration::from_secs_f64((ms as f64 * scale() / 1000.0).min(86_400.0))
}

/// A watchdog bound; a zero scale removes waits, never the watchdogs.
pub(super) fn timeout(ms: u64) -> Duration {
    let scale = scale();
    let scale = if scale > 0.0 { scale } else { 1.0 };
    Duration::from_secs_f64((ms as f64 * scale / 1000.0).min(86_400.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delays_follow_omp_backoff() {
        assert_eq!(default_delay_ms(0), 1000);
        assert_eq!(default_delay_ms(1), 2000);
        assert_eq!(default_delay_ms(2), 4000);
        assert_eq!(default_delay_ms(40), MAX_DELAY_MS);
        assert_eq!(empty_delay_ms(1), 500);
        assert_eq!(empty_delay_ms(2), 1000);
        assert!(retry_status(408) && retry_status(429) && retry_status(500) && retry_status(503));
        for status in [400, 401, 403, 404] {
            assert!(!retry_status(status));
        }
    }

    #[cfg(not(feature = "test-instrumentation"))]
    #[test]
    #[serial_test::serial]
    fn retry_scale_knob_is_absent_in_normal_builds() {
        struct Restore(Option<std::ffi::OsString>);
        impl Drop for Restore {
            fn drop(&mut self) {
                // SAFETY: the unkeyed serial guard outlives this restoration.
                unsafe {
                    match self.0.take() {
                        Some(v) => std::env::set_var("RUSTAIN_TEST_GEMINI_CLI_RETRY_SCALE", v),
                        None => std::env::remove_var("RUSTAIN_TEST_GEMINI_CLI_RETRY_SCALE"),
                    }
                }
            }
        }
        let _restore = Restore(std::env::var_os("RUSTAIN_TEST_GEMINI_CLI_RETRY_SCALE"));
        // SAFETY: all environment-mutating fixtures take the unkeyed serial lock.
        unsafe {
            std::env::set_var("RUSTAIN_TEST_GEMINI_CLI_RETRY_SCALE", "0");
        }
        assert_eq!(delay(1000), Duration::from_millis(1000));
        assert_eq!(timeout(60_000), Duration::from_millis(60_000));
    }
}
