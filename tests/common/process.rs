//! Kill-on-drop wrapper for `std::process::Child` in integration tests.
//!
//! A test that spawns a long-lived child (`rustain daemon start --foreground`,
//! a re-exec of the test binary) and asserts afterwards leaks that child when
//! an assertion panics: the harness unwinds past the explicit `kill()`, and in
//! an area binary the orphan outlives the whole run. `ChildGuard` kills and
//! reaps on drop; explicit `kill()` / `try_wait()` / `wait()` keep working
//! through `DerefMut`, and the second `kill()` on an already-dead child is a
//! no-op by design. Tokio children use `Command::kill_on_drop(true)` instead.

use std::ops::{Deref, DerefMut};
use std::process::Child;

pub struct ChildGuard(Option<Child>);

impl ChildGuard {
    pub fn new(child: Child) -> Self {
        Self(Some(child))
    }

    /// Hand the child back for a consuming call such as `wait_with_output`;
    /// the guard no longer kills it.
    pub fn into_inner(mut self) -> Child {
        self.0.take().expect("child already taken")
    }
}

impl Deref for ChildGuard {
    type Target = Child;
    fn deref(&self) -> &Child {
        self.0.as_ref().expect("child already taken")
    }
}

impl DerefMut for ChildGuard {
    fn deref_mut(&mut self) -> &mut Child {
        self.0.as_mut().expect("child already taken")
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}
