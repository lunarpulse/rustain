/// Metrics module for height-cache instrumentation.
///
/// Process-global atomic counters for cache hits/misses. Tests that read
/// these counters MUST be `#[serial_test::serial]` to avoid cross-test
/// interference.
#[cfg(any(test, debug_assertions))]
pub mod metrics {
    use std::sync::atomic::{AtomicU64, Ordering};

    pub static HITS: AtomicU64 = AtomicU64::new(0);
    pub static MISSES: AtomicU64 = AtomicU64::new(0);

    pub fn reset() {
        HITS.store(0, Ordering::Relaxed);
        MISSES.store(0, Ordering::Relaxed);
    }

    pub fn snapshot() -> (u64, u64) {
        (HITS.load(Ordering::Relaxed), MISSES.load(Ordering::Relaxed))
    }
}
