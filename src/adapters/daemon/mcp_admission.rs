//! Story C-3 (AD-3, AD-11, AD-15) — the admission gate between "the factory
//! starts the daemon's MCP children on first activity" and "shutdown reaps
//! them".
//!
//! One `std::sync::Mutex` guards `closed` + the started toolset. The critical
//! section never awaits (`start` only spawns a task), so a std mutex is the
//! right tool and the closed-check and the start are ONE atomic step: shutdown
//! either finds the started toolset to reap, or closes the gate before the start
//! and nothing is ever started. A split check/start would let a first activity
//! that is still in progress (an inbound A2A/P2P request before the listeners
//! are cancelled) spawn children after shutdown finished reaping.

use std::sync::Arc;

use crate::domain::ports::ToolSetPort;

struct State {
    closed: bool,
    tools: Option<Arc<dyn ToolSetPort>>,
}

impl State {
    fn open() -> Self {
        Self {
            closed: false,
            tools: None,
        }
    }
}

/// Shared by the factory (start) and `DaemonCore::shutdown_mcp` (close + reap).
pub struct McpAdmission {
    state: std::sync::Mutex<State>, // CONFORMANCE_EXCEPTION_STD_SYNC_LOCK: critical section never awaits (check-closed + spawn only), so a tokio mutex would add nothing; the check and the start must be one step. Story C-3 AD-3.
    /// Test-only hook, called between the `closed` check and `start()` while the
    /// lock is held, so a test can park the starter at the exact racy point.
    #[cfg(test)]
    between_check_and_start: std::sync::Mutex<Option<Box<dyn Fn() + Send + Sync>>>, // CONFORMANCE_EXCEPTION_STD_SYNC_LOCK: test-only hook slot, never held across an await. Story C-3 AD-15.
}

impl Default for McpAdmission {
    fn default() -> Self {
        Self {
            state: std::sync::Mutex::new(State::open()), // CONFORMANCE_EXCEPTION_STD_SYNC_LOCK: see the field above.
            #[cfg(test)]
            between_check_and_start: std::sync::Mutex::new(None), // CONFORMANCE_EXCEPTION_STD_SYNC_LOCK: see the field above.
        }
    }
}

impl McpAdmission {
    /// The single function the factory uses to start MCP children. Runs `start`
    /// and records `tools` (so shutdown can reap them) only while the gate is
    /// open; returns whether it ran. When it returns `false` the caller marks the
    /// composite settled without connecting.
    pub fn start_if_open(&self, tools: &Arc<dyn ToolSetPort>, start: impl FnOnce()) -> bool {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.closed {
            return false;
        }
        #[cfg(test)]
        if let Some(hook) = self
            .between_check_and_start
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            hook();
        }
        start();
        state.tools = Some(Arc::clone(tools));
        true
    }

    /// Close the gate and take the started toolset (`None` when nothing was
    /// started). Idempotent: a second call returns `None`.
    pub fn close_and_take(&self) -> Option<Arc<dyn ToolSetPort>> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.closed = true;
        state.tools.take()
    }

    #[cfg(test)]
    pub(crate) fn set_between_check_and_start_hook(&self, hook: Box<dyn Fn() + Send + Sync>) {
        *self
            .between_check_and_start
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(hook);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::noop::NoOpToolSet;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    fn noop_tools() -> Arc<dyn ToolSetPort> {
        Arc::new(NoOpToolSet)
    }

    #[test]
    fn open_gate_runs_start_once_and_records_the_tools() {
        let admission = McpAdmission::default();
        let tools = noop_tools();
        let started = AtomicUsize::new(0);
        assert!(admission.start_if_open(&tools, || {
            started.fetch_add(1, Ordering::SeqCst);
        }));
        assert_eq!(started.load(Ordering::SeqCst), 1);
        let taken = admission.close_and_take().expect("started tools");
        assert!(Arc::ptr_eq(&taken, &tools));
    }

    #[test]
    fn closed_gate_never_starts_and_a_second_close_takes_nothing() {
        let admission = McpAdmission::default();
        assert!(
            admission.close_and_take().is_none(),
            "never built: nothing to reap"
        );
        let started = AtomicBool::new(false);
        assert!(!admission.start_if_open(&noop_tools(), || started.store(true, Ordering::SeqCst)));
        assert!(
            !started.load(Ordering::SeqCst),
            "closed-then-start spawns nothing"
        );
        assert!(admission.close_and_take().is_none());
    }

    /// Rule 4 structural ratchet: the check and the start are ONE step under the
    /// lock. Thread A parks between them; the closer must still be pending while A
    /// is parked, and after the release it must reap what A started.
    #[test]
    fn close_waits_for_a_starter_parked_between_check_and_start() {
        use std::sync::mpsc;
        use std::time::Duration;

        let admission = Arc::new(McpAdmission::default());
        let (entered_tx, entered_rx) = mpsc::channel::<()>();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let release_rx = std::sync::Mutex::new(release_rx); // CONFORMANCE_EXCEPTION_STD_SYNC_LOCK: test-only, receiver shared into a Fn hook.
        admission.set_between_check_and_start_hook(Box::new(move || {
            entered_tx.send(()).expect("entered");
            release_rx
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(10))
                .expect("released");
        }));

        let tools = noop_tools();
        let started = Arc::new(AtomicBool::new(false));
        let starter = {
            let admission = Arc::clone(&admission);
            let tools = Arc::clone(&tools);
            let started = Arc::clone(&started);
            std::thread::spawn(move || {
                admission.start_if_open(&tools, || started.store(true, Ordering::SeqCst))
            })
        };
        entered_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("starter reached the hook");

        let closer_done = Arc::new(AtomicBool::new(false));
        let closer = {
            let admission = Arc::clone(&admission);
            let closer_done = Arc::clone(&closer_done);
            std::thread::spawn(move || {
                let taken = admission.close_and_take();
                closer_done.store(true, Ordering::SeqCst);
                taken
            })
        };
        // The starter holds the lock, parked: the closer cannot have finished.
        // (A bounded negative wait on a condition: it must stay false for the
        // whole window; a split check/start would let it finish at once.)
        let deadline = std::time::Instant::now() + Duration::from_millis(300);
        while std::time::Instant::now() < deadline {
            assert!(
                !closer_done.load(Ordering::SeqCst),
                "close finished while the starter was parked between check and start"
            );
            std::thread::yield_now();
        }
        assert!(
            !started.load(Ordering::SeqCst),
            "start must not have run yet"
        );

        release_tx.send(()).expect("release");
        assert!(starter.join().expect("starter"), "starter ran start()");
        let taken = closer
            .join()
            .expect("closer")
            .expect("closer reaps what the starter started");
        assert!(Arc::ptr_eq(&taken, &tools));
        assert!(started.load(Ordering::SeqCst));
    }
}
