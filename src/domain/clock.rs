//! Clock abstraction for testable timing across Epic 16.
//!
//! # Why a trait?
//!
//! Render code must never call `Instant::now()` directly — decoupling spinner snapshots
//! from wall-clock drift makes TUI tests deterministic. The reducer (Story 16.2) and
//! ViewState (Story 16.3) inject `MockClock` for elapsed-threshold and sticky-anchor
//! timing tests.
//!
//! # Frame cadence
//!
//! `FRAME_TICK_MS = 80` (≈ 12.5 fps). This matches the braille-spinner UX precedent
//! used throughout the TUI layer.
//!
//! # Send + Sync requirement
//!
//! `Clock: Send + Sync` is required so downstream tokio tasks can share the same
//! clock instance (e.g. a `MockClock` wired into an async reducer test).
//!
//! # Cross-references
//!
//! - Story 16.2 — reducer elapsed-threshold rules
//! - Story 16.3 — ViewState sticky-anchor timing
//! - Story 16.4 — render spinner snapshots
//! - Story 16.4.5 — heuristic labeler elapsed-aware Tier-1 form
//!
//! See ADR-16-01 §Decision §4 for the canonical clock design.

use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Braille-spinner frame tick in milliseconds.
/// ≈ 12.5 fps — matches existing TUI spinner UX.
pub const FRAME_TICK_MS: u64 = 80;

/// Braille spinner glyphs. 10-frame sequence at ~12.5 fps.
/// Standard TUI convention — see `clock.rs` module doc for cadence rationale.
pub const BRAILLE_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Return the current braille spinner glyph for the given clock.
pub fn current_braille_frame(clock: &dyn Clock) -> &'static str {
    BRAILLE_FRAMES[(clock.frame() % BRAILLE_FRAMES.len() as u64) as usize]
}

/// Abstraction over time sources so tests can be deterministic.
pub trait Clock: Send + Sync {
    /// Current instant.
    fn now(&self) -> Instant;
    /// Current animation frame, derived from elapsed time divided by `FRAME_TICK_MS`.
    fn frame(&self) -> u64;
    /// Current wall-clock time in unix milliseconds.
    ///
    /// This is the **sole crate-wide source** of `SystemTime::now()` post-migration
    /// (P0-12 Round 2 convergence). All render-layer and tool-block elapsed-time
    /// computation reads this method instead of calling `SystemTime::now()` directly.
    fn wall_now_ms(&self) -> i64;
}

/// Wall-clock implementation.
///
/// Note: uses the global [`FRAME_TICK_MS`] constant rather than a per-instance
/// `frame_tick: Duration` field. This simplifies construction (no config argument)
/// while still allowing test overrides via `MockClock`. If downstream stories need
/// per-instance frame cadence, adding `frame_tick: Duration` is a non-breaking field
/// addition with a `Default`-valued `FRAME_TICK_MS`.
#[derive(Debug, Clone)]
pub struct SystemClock {
    start: Instant,
}

impl Default for SystemClock {
    fn default() -> Self {
        Self {
            start: Instant::now(),
        }
    }
}

impl SystemClock {
    fn unix_millis_now() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64
    }
}

impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }

    fn frame(&self) -> u64 {
        (self.start.elapsed().as_millis() / FRAME_TICK_MS as u128) as u64
    }

    fn wall_now_ms(&self) -> i64 {
        Self::unix_millis_now()
    }
}

#[derive(Debug)]
struct MockClockState {
    instant: Instant,
    frame: u64,
    wall_anchor_ms: i64,
    instant_anchor: Instant,
}

/// Test-only clock with interior mutability.
///
/// `std::sync::Mutex` is safe here because `domain/clock.rs` is a synchronous,
/// pure-domain module — no `async fn`, no `.await`. Guard scopes are microscopic
/// (single field read/write) so they can never be held across an await point.
#[derive(Debug)]
pub struct MockClock {
    state: Mutex<MockClockState>,
}

impl MockClock {
    pub fn new(start: Instant) -> Self {
        Self {
            state: Mutex::new(MockClockState {
                instant: start,
                frame: 0,
                wall_anchor_ms: 0,
                instant_anchor: start,
            }),
        }
    }

    /// Create a MockClock pinned to a fixed wall-clock anchor.
    pub fn at_wall_ms(wall_ms: i64) -> Self {
        let instant = Instant::now();
        Self {
            state: Mutex::new(MockClockState {
                instant,
                frame: 0,
                wall_anchor_ms: wall_ms,
                instant_anchor: instant,
            }),
        }
    }

    /// Set the wall-clock anchor for deterministic elapsed-time computation.
    pub fn set_wall_anchor_ms(&self, wall_ms: i64) {
        let mut s = self.state.lock().unwrap();
        s.wall_anchor_ms = wall_ms;
        s.instant_anchor = s.instant;
    }

    pub fn advance(&self, by: Duration) {
        let mut s = self.state.lock().unwrap();
        s.instant += by;
    }

    pub fn tick_frame(&self) {
        let mut s = self.state.lock().unwrap();
        s.frame = s.frame.wrapping_add(1);
    }

    pub fn set_frame(&self, frame: u64) {
        let mut s = self.state.lock().unwrap();
        s.frame = frame;
    }
}

impl Clock for MockClock {
    fn now(&self) -> Instant {
        self.state.lock().unwrap().instant
    }

    fn frame(&self) -> u64 {
        self.state.lock().unwrap().frame
    }

    fn wall_now_ms(&self) -> i64 {
        let s = self.state.lock().unwrap();
        let delta_ms = s.instant.duration_since(s.instant_anchor).as_millis() as i64;
        s.wall_anchor_ms.saturating_add(delta_ms)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn system_clock_now_advances_monotonically() {
        let clock = SystemClock::default();
        let t1 = clock.now();
        thread::sleep(Duration::from_millis(10));
        let t2 = clock.now();
        assert!(t2 > t1, "wall clock did not advance");
    }

    #[test]
    fn system_clock_frame_increases_with_time() {
        let clock = SystemClock::default();
        let f1 = clock.frame();
        thread::sleep(Duration::from_millis(FRAME_TICK_MS + 10));
        let f2 = clock.frame();
        assert!(f2 > f1, "frame counter did not increase after a tick+sleep");
    }

    #[test]
    fn mock_clock_advance_moves_instant() {
        let start = Instant::now();
        let clock = MockClock::new(start);
        clock.advance(Duration::from_secs(42));
        assert_eq!(clock.now().duration_since(start), Duration::from_secs(42));
    }

    #[test]
    fn mock_clock_tick_frame_increments() {
        let clock = MockClock::new(Instant::now());
        assert_eq!(clock.frame(), 0);
        clock.tick_frame();
        assert_eq!(clock.frame(), 1);
        clock.tick_frame();
        assert_eq!(clock.frame(), 2);
    }

    #[test]
    fn mock_clock_send_sync_bound() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<MockClock>();
    }
}
